use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
};

use miette::{IntoDiagnostic, Result};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

use super::{
    dispatch_decision_result, dispatch_ready_task, dispatch_ready_task_async, shutdown_signal,
    DecisionTaskResult, DispatchContext, ShutdownSignal,
};
use crate::{
    cli::OutputMode,
    memory_pressure::{MemoryMonitor, MemoryPressure, PressureState},
};
use luchta_engine::ReadyTaskMessage;
use owo_colors::OwoColorize;

/// Events that can occur during a pause-loop tick.
///
/// Used by `PressureEnv::next_tick` to indicate which event fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PauseTick {
    /// Re-check timer elapsed — caller should check pressure again.
    ReCheck,
    /// Progress interval ticked — caller should render progress.
    ProgressDue,
    /// Shutdown signal arrived — caller should return interrupted.
    Shutdown(ShutdownSignal),
}

/// The real implementation (ProdPressureEnv) drives the pause loop from real
/// time and signals: a 250ms re-check tick, the progress interval, and the
/// shared shutdown future.
pub(super) trait PressureEnv {
    /// Check current memory pressure. Updates pressure_state for Task 5 visibility.
    fn check(&mut self) -> MemoryPressure;

    /// Number of tasks currently executing.
    ///
    /// The pause loop never holds a task back while this is zero: with nothing
    /// in flight there is no build work left that could release memory, so
    /// waiting can only stall forever.
    fn running_count(&self) -> usize;

    /// Await next tick event: re-check timer, progress interval, or shutdown.
    ///
    /// In production, this is `tokio::select!` over three futures.
    async fn next_tick(&mut self) -> Result<PauseTick>;

    /// Render progress line using current pressure state.
    fn render_progress(&self);
}

/// Result of pressure-clearance await decision.
///
/// Returned by `await_pressure_clearance` to tell dispatcher what action
/// to take after pause logic completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PressureClearance {
    /// Pressure cleared — caller should dispatch held task now.
    Dispatch,
    /// Shutdown signal fired during pause — caller should return interrupted.
    Shutdown(ShutdownSignal),
}

/// Drives pause loop using injected PressureEnv.
///
/// Generic over `PressureEnv` so tests can inject deterministic fakes.
///
/// Returns `PressureClearance::Dispatch` when pressure clears (caller should
/// dispatch task), or `PressureClearance::Shutdown` if interrupted.
///
/// **Intentional pause-forever behavior**: If pressure never clears, this
/// function will not return. User must interrupt with Ctrl-C/SIGTERM.
/// This is BY DESIGN — we do NOT add timeout or auto-resume escape hatch.
///
/// macOS is the one exception, via [`DISPATCH_WHEN_IDLE`]: there the loop
/// never holds the last task back, because with nothing in flight no amount of
/// waiting can free memory, and macOS pressure originating outside the build
/// would otherwise deadlock the run until the user hits Ctrl-C.
pub(super) async fn await_pressure_clearance<E: PressureEnv>(
    env: &mut E,
) -> Result<PressureClearance> {
    await_pressure_clearance_with(env, DISPATCH_WHEN_IDLE).await
}

/// Whether the pause loop lets a held task through once nothing is in flight.
///
/// Enabled on macOS only. Linux and Windows keep the original pause-forever
/// behavior, where dispatch waits on the byte thresholds alone.
const DISPATCH_WHEN_IDLE: bool = cfg!(target_os = "macos");

/// [`await_pressure_clearance`] with the idle-dispatch policy made explicit,
/// so tests can exercise both policies on any host.
async fn await_pressure_clearance_with<E: PressureEnv>(
    env: &mut E,
    dispatch_when_idle: bool,
) -> Result<PressureClearance> {
    if may_dispatch(env, dispatch_when_idle) {
        return Ok(PressureClearance::Dispatch);
    }

    loop {
        match env.next_tick().await? {
            PauseTick::ReCheck => {
                if may_dispatch(env, dispatch_when_idle) {
                    return Ok(PressureClearance::Dispatch);
                }
            }
            PauseTick::ProgressDue => env.render_progress(),
            PauseTick::Shutdown(shutdown) => {
                return Ok(PressureClearance::Shutdown(shutdown));
            }
        }
    }
}

/// Whether the held task may be dispatched now.
///
/// True when memory pressure has cleared. With `dispatch_when_idle` it is also
/// true when nothing is in flight: at least one task must then be allowed to
/// run, or the build can never make the progress that would release memory.
///
/// Always samples pressure so the status line keeps updating while paused.
fn may_dispatch<E: PressureEnv>(env: &mut E, dispatch_when_idle: bool) -> bool {
    !env.check().paused || (dispatch_when_idle && env.running_count() == 0)
}

/// The shutdown future type produced by [`shutdown_signal`].
type ShutdownFuture = Pin<Box<dyn Future<Output = Result<ShutdownSignal>> + Send>>;

/// Production implementation of PressureEnv using real time and signals.
pub(super) struct ProdPressureEnv<'a> {
    monitor: &'a mut MemoryMonitor,
    pressure_state: &'a Arc<PressureState>,
    progress_interval: &'a mut tokio::time::Interval,
    progress_reporter: &'a crate::progress::ProgressReporter,
    /// Borrow of the dispatch loop's SINGLE shutdown future, so a signal that
    /// arrives between receiving a ready task and entering the pause loop is
    /// not lost. Creating a fresh `shutdown_signal()` here would re-register a
    /// listener and drop any signal delivered in that window (the first Ctrl-C
    /// could be missed).
    shutdown_signal: &'a mut ShutdownFuture,
}

impl<'a> PressureEnv for ProdPressureEnv<'a> {
    fn check(&mut self) -> MemoryPressure {
        let pressure = self.monitor.check();
        self.pressure_state.update(&pressure);
        pressure
    }

    fn running_count(&self) -> usize {
        self.progress_reporter.running_count()
    }

    async fn next_tick(&mut self) -> Result<PauseTick> {
        Ok(tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                PauseTick::ReCheck
            }
            _ = self.progress_interval.tick() => {
                PauseTick::ProgressDue
            }
            shutdown = self.shutdown_signal.as_mut() => {
                PauseTick::Shutdown(shutdown?)
            }
        })
    }

    fn render_progress(&self) {
        render_status_line(self.progress_reporter, self.pressure_state, true);
    }
}

/// Emits periodic status line (with any memory-pressure warning suffix).
/// Shared by outer dispatch loop and in-pause progress tick so formatting stays
/// in one place.
fn render_status_line(
    reporter: &crate::progress::ProgressReporter,
    pressure_state: &PressureState,
    paused: bool,
) {
    if !should_render(paused, reporter.running_count(), reporter.mode) {
        return;
    }

    let pressure = pressure_state.snapshot();
    let rss = crate::rss::format_rss(pressure.sample.map(|sample| sample.tree_rss));
    let output = reporter.output();
    let line = reporter.render_progress_for_width(crate::progress::ProgressRenderContext {
        rss_formatted: &rss,
        warnings: &pressure.reasons,
        pressure: &pressure,
        stream: owo_colors::Stream::Stderr,
        max_width: output.terminal_width(),
    });
    output.progress_line(&line);
}

fn should_render(paused: bool, running_count: usize, mode: OutputMode) -> bool {
    mode == OutputMode::Default && (paused || running_count > 0)
}

pub(super) async fn dispatch_loop(
    receiver: &mut tokio::sync::mpsc::Receiver<ReadyTaskMessage>,
    ctx: &DispatchContext<'_>,
    monitor: &mut MemoryMonitor,
    pressure_state: &Arc<PressureState>,
) -> Result<()> {
    let decision_semaphore = Arc::new(Semaphore::new(decision_parallelism()));
    let (decision_result_tx, mut decision_result_rx) =
        mpsc::channel::<DecisionTaskResult>(decision_parallelism().saturating_mul(4).max(1));
    // A SINGLE shutdown future for the whole loop. Both the outer select arm
    // and the inner pause loop (via ProdPressureEnv) poll this same future, so
    // a signal delivered while transitioning into the pause loop is never lost.
    let mut signal: ShutdownFuture = shutdown_signal();
    let mut progress_interval = tokio::time::interval(super::progress_interval_duration(
        ctx.reporter.uses_live_status(),
    ));
    progress_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    progress_interval.tick().await;

    loop {
        tokio::select! {
            signal_result = signal.as_mut() => {
                let shutdown = signal_result?;
                break report_interrupted(ctx, pressure_state, Some(shutdown));
            }
            message = receiver.recv() => {
                let Some((task_node, done_tx)) = message else {
                    break Ok(());
                };

                // Shared-cache / non-cache-enabled decisions are computed
                // synchronously and returned here (kept serialized); local-cache
                // decisions are offloaded and arrive via `decision_result_rx`.
                if let Some(result) = dispatch_ready_task_async(
                    task_node,
                    done_tx,
                    ctx,
                    Arc::clone(&decision_semaphore),
                    decision_result_tx.clone(),
                ) {
                    if let std::ops::ControlFlow::Break(shutdown) = handle_decision_result(
                        result,
                        ctx,
                        monitor,
                        pressure_state,
                        &mut progress_interval,
                        &mut signal,
                    )
                    .await?
                    {
                        return report_interrupted(ctx, pressure_state, Some(shutdown));
                    }
                }
            }
            result = decision_result_rx.recv() => {
                let Some(result) = result else {
                    break Ok(());
                };

                if let std::ops::ControlFlow::Break(shutdown) = handle_decision_result(
                    result,
                    ctx,
                    monitor,
                    pressure_state,
                    &mut progress_interval,
                    &mut signal,
                )
                .await?
                {
                    return report_interrupted(ctx, pressure_state, Some(shutdown));
                }
            }
            _ = progress_interval.tick() => render_status_line(ctx.reporter, pressure_state, false),
        }
    }
}

/// Finalizes a completed cache decision on the (single) dispatch-loop task.
///
/// A cache Skip/SharedHit is finalized inside `dispatch_decision_result`. A
/// task that must Run is returned as a `ReadyTask` and dispatched here behind
/// the memory-pressure gate (only actual execution consumes weight; cache
/// skips are never pressure-gated). Returns `ControlFlow::Break` when the
/// pressure wait resolved to shutdown so the caller can unwind cleanly.
async fn handle_decision_result(
    result: DecisionTaskResult,
    ctx: &DispatchContext<'_>,
    monitor: &mut MemoryMonitor,
    pressure_state: &Arc<PressureState>,
    progress_interval: &mut tokio::time::Interval,
    signal: &mut ShutdownFuture,
) -> Result<std::ops::ControlFlow<ShutdownSignal>> {
    let Some(ready) = dispatch_decision_result(result, ctx)? else {
        return Ok(std::ops::ControlFlow::Continue(()));
    };

    let mut env = ProdPressureEnv {
        monitor,
        pressure_state,
        progress_interval,
        progress_reporter: ctx.reporter,
        shutdown_signal: signal,
    };
    match await_pressure_clearance(&mut env).await? {
        PressureClearance::Dispatch => {
            dispatch_ready_task(ready, ctx);
            Ok(std::ops::ControlFlow::Continue(()))
        }
        PressureClearance::Shutdown(shutdown) => Ok(std::ops::ControlFlow::Break(shutdown)),
    }
}

#[cfg(test)]
pub(super) fn decision_parallelism_for_test() -> usize {
    decision_parallelism()
}

fn decision_parallelism() -> usize {
    static PARALLELISM: OnceLock<usize> = OnceLock::new();
    *PARALLELISM.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1)
    })
}

pub(super) async fn run_decision_task<T, F>(semaphore: Arc<Semaphore>, blocking: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let _permit: OwnedSemaphorePermit = semaphore.acquire_owned().await.into_diagnostic()?;
    tokio::task::spawn_blocking(blocking)
        .await
        .into_diagnostic()
}

fn report_interrupted(
    ctx: &DispatchContext<'_>,
    pressure_state: &PressureState,
    shutdown: Option<ShutdownSignal>,
) -> Result<()> {
    ctx.interrupted
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let pressure = pressure_state.snapshot();
    let rss = pressure
        .sample
        .map(|sample| sample.tree_rss)
        .or_else(crate::rss::process_tree_rss_bytes);
    let source = shutdown
        .map(|signal| format!(" by {}", signal.name()))
        .unwrap_or_default();
    let message = format!(
        "Interrupted{source}: {} tasks running after {}s; RSS: {}",
        ctx.reporter.running_count(),
        ctx.reporter.start.elapsed().as_secs(),
        crate::rss::format_rss(rss),
    );
    ctx.reporter.output().stderr_line(
        &message
            .as_str()
            .if_supports_color(owo_colors::Stream::Stderr, |t| t.red())
            .to_string(),
    );
    Err(miette::miette!("interrupted"))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::memory_pressure::{MemoryPressure, MemorySample, PressureReason};

    /// Fake implementation of PressureEnv for deterministic testing.
    ///
    /// Memory pressure checks come from queue, next_tick events
    /// come from queue, and render_progress() calls are counted. No real time
    /// or tokio intervals involved.
    struct FakePressureEnv {
        check_results: VecDeque<MemoryPressure>,
        tick_events: VecDeque<PauseTick>,
        /// Scripted `running_count()` answers; the final entry repeats once the
        /// script is exhausted so tests only script the transitions they care
        /// about.
        running_counts: std::sync::Mutex<VecDeque<usize>>,
        render_calls: AtomicUsize,
        check_calls: AtomicUsize,
    }

    impl FakePressureEnv {
        fn with_running_counts(
            check_results: Vec<MemoryPressure>,
            tick_events: Vec<PauseTick>,
            running_counts: Vec<usize>,
        ) -> Self {
            Self {
                check_results: check_results.into(),
                tick_events: tick_events.into(),
                running_counts: std::sync::Mutex::new(running_counts.into()),
                render_calls: AtomicUsize::new(0),
                check_calls: AtomicUsize::new(0),
            }
        }

        fn render_count(&self) -> usize {
            self.render_calls.load(Ordering::SeqCst)
        }

        fn check_count(&self) -> usize {
            self.check_calls.load(Ordering::SeqCst)
        }
    }

    impl PressureEnv for FakePressureEnv {
        fn check(&mut self) -> MemoryPressure {
            self.check_calls.fetch_add(1, Ordering::SeqCst);
            self.check_results
                .pop_front()
                .expect("FakePressureEnv: check() called but no results remaining")
        }

        async fn next_tick(&mut self) -> Result<PauseTick> {
            Ok(self
                .tick_events
                .pop_front()
                .expect("FakePressureEnv: next_tick() called but no events remaining"))
        }

        fn render_progress(&self) {
            self.render_calls.fetch_add(1, Ordering::SeqCst);
        }

        fn running_count(&self) -> usize {
            let mut counts = self.running_counts.lock().expect("running counts poisoned");
            if counts.len() > 1 {
                counts.pop_front().expect("non-empty")
            } else {
                *counts
                    .front()
                    .expect("FakePressureEnv: running_counts is empty")
            }
        }
    }

    /// Builds a scripted pressure sample. `paused == true` yields a usage-high
    /// sample; `false` yields a fully-cleared one. Centralises the literal so
    /// the individual tests stay free of duplicated struct construction.
    fn pressure_sample(paused: bool) -> MemoryPressure {
        let (tree_rss, system_available, reasons) = if paused {
            (1_000_000, 1_000_000, vec![PressureReason::UsageHigh])
        } else {
            (0, u64::MAX, vec![])
        };
        MemoryPressure {
            sample: MemorySample {
                tree_rss,
                system_available,
                kernel_pressure: None,
            },
            reasons,
            paused,
        }
    }

    /// Scripted "paused" pressure sample (usage over threshold).
    fn paused_pressure() -> MemoryPressure {
        pressure_sample(true)
    }

    /// Scripted "cleared" pressure sample (no pressure).
    fn clear_pressure() -> MemoryPressure {
        pressure_sample(false)
    }

    /// Outcome of driving the pause loop against a scripted `FakePressureEnv`.
    struct PauseOutcome {
        clearance: PressureClearance,
        checks: usize,
        renders: usize,
        remaining_ticks: usize,
    }

    /// Drives `await_pressure_clearance` with a scripted fake and reports the
    /// observable outcome. Keeps the individual tests to their assertions.
    async fn drive(
        check_results: Vec<MemoryPressure>,
        tick_events: Vec<PauseTick>,
    ) -> PauseOutcome {
        drive_with_policy(check_results, tick_events, vec![1], DISPATCH_WHEN_IDLE).await
    }

    /// Like [`drive`], but scripts what `running_count()` reports on each call
    /// and fixes the idle-dispatch policy, so tests can express "the last
    /// in-flight task finished while paused" under either platform's rule.
    async fn drive_with_policy(
        check_results: Vec<MemoryPressure>,
        tick_events: Vec<PauseTick>,
        running_counts: Vec<usize>,
        dispatch_when_idle: bool,
    ) -> PauseOutcome {
        let mut env =
            FakePressureEnv::with_running_counts(check_results, tick_events, running_counts);
        let clearance = await_pressure_clearance_with(&mut env, dispatch_when_idle)
            .await
            .expect("pause clearance should succeed");
        PauseOutcome {
            clearance,
            checks: env.check_count(),
            renders: env.render_count(),
            remaining_ticks: env.tick_events.len(),
        }
    }

    /// While paused the loop keeps re-checking (here twice) and rendering
    /// progress on each `ProgressDue`, then dispatches once a check clears —
    /// covering both the "waits for clearance" and "renders while waiting"
    /// behaviours in one scenario.
    #[tokio::test]
    async fn pause_loop_renders_progress_then_dispatches_when_pressure_clears() {
        let out = drive(
            vec![paused_pressure(), paused_pressure(), clear_pressure()],
            vec![
                PauseTick::ProgressDue,
                PauseTick::ReCheck,
                PauseTick::ProgressDue,
                PauseTick::ReCheck,
            ],
        )
        .await;

        assert_eq!(out.clearance, PressureClearance::Dispatch);
        assert_eq!(out.checks, 3);
        assert_eq!(out.renders, 2);
    }

    #[tokio::test]
    async fn pause_loop_returns_shutdown_when_interrupted() {
        let out = drive(
            vec![paused_pressure()],
            vec![PauseTick::Shutdown(ShutdownSignal::CtrlC)],
        )
        .await;

        assert_eq!(
            out.clearance,
            PressureClearance::Shutdown(ShutdownSignal::CtrlC)
        );
        assert_eq!(out.checks, 1);
    }

    #[tokio::test]
    async fn pause_loop_fast_path_when_not_paused() {
        let out = drive(
            vec![clear_pressure()],
            vec![PauseTick::ProgressDue, PauseTick::ReCheck],
        )
        .await;

        assert_eq!(out.clearance, PressureClearance::Dispatch);
        assert_eq!(out.checks, 1);
        assert_eq!(out.renders, 0);
        assert_eq!(out.remaining_ticks, 2);
    }

    #[test]
    fn render_status_line_uses_decision_tree_rss() {
        let reporter = crate::progress::ProgressReporter::new(
            OutputMode::Default,
            std::collections::HashMap::new(),
            0,
        );
        let pressure_state = PressureState::new(30 * 1024 * 1024, 0);
        pressure_state.update(&MemoryPressure {
            sample: MemorySample {
                tree_rss: 32 * 1024 * 1024,
                system_available: u64::MAX,
                kernel_pressure: None,
            },
            reasons: vec![crate::memory_pressure::PressureReason::UsageHigh],
            paused: true,
        });

        let pressure = pressure_state.snapshot();
        let line = reporter.render_progress(
            &crate::rss::format_rss(pressure.sample.map(|sample| sample.tree_rss)),
            &pressure.reasons,
            &pressure,
            owo_colors::Stream::Stderr,
        );

        assert!(line.contains("🐏 32 MB"));
        assert!(line.contains("mem usage high (32 MB / 30 MB)"));
    }

    #[test]
    fn should_render_status_line_in_default_mode_for_pause_or_running_matrix() {
        assert!(should_render(true, 0, OutputMode::Default));
        assert!(should_render(true, 2, OutputMode::Default));
        assert!(!should_render(false, 0, OutputMode::Default));
        assert!(should_render(false, 2, OutputMode::Default));
    }

    #[test]
    fn live_and_redirected_progress_use_distinct_default_cadences() {
        assert_eq!(
            super::super::progress_interval_duration_from_value(true, None),
            std::time::Duration::from_millis(100)
        );
        assert_eq!(
            super::super::progress_interval_duration_from_value(false, None),
            std::time::Duration::from_secs(5)
        );
        assert_eq!(
            super::super::progress_interval_duration_from_value(true, Some("250")),
            std::time::Duration::from_millis(250)
        );
    }

    /// On macOS, pausing with nothing in flight can never resolve: no build
    /// work is left to release memory, so the loop would wait forever.
    /// Dispatch instead.
    #[tokio::test]
    async fn pause_loop_dispatches_under_pressure_when_nothing_is_in_flight() {
        let out = drive_with_policy(
            vec![paused_pressure()],
            vec![PauseTick::ReCheck, PauseTick::ProgressDue],
            vec![0],
            true,
        )
        .await;

        assert_eq!(out.clearance, PressureClearance::Dispatch);
        assert_eq!(out.checks, 1);
        assert_eq!(out.renders, 0);
        assert_eq!(out.remaining_ticks, 2);
    }

    /// Pressure raised by processes outside the build never clears on its own.
    /// Once the last in-flight task drains, the pause loop must let the held
    /// task through rather than deadlock the run.
    #[tokio::test]
    async fn pause_loop_dispatches_when_last_in_flight_task_drains_during_pause() {
        let out = drive_with_policy(
            vec![paused_pressure(), paused_pressure()],
            vec![PauseTick::ReCheck],
            vec![1, 0],
            true,
        )
        .await;

        assert_eq!(out.clearance, PressureClearance::Dispatch);
        assert_eq!(out.checks, 2);
    }

    /// Off macOS the original contract holds: pressure with nothing in flight
    /// keeps the task held until pressure clears or the user interrupts.
    #[tokio::test]
    async fn pause_loop_keeps_holding_when_idle_dispatch_is_disabled() {
        let out = drive_with_policy(
            vec![paused_pressure(), paused_pressure()],
            vec![
                PauseTick::ReCheck,
                PauseTick::Shutdown(ShutdownSignal::CtrlC),
            ],
            vec![0],
            false,
        )
        .await;

        assert_eq!(
            out.clearance,
            PressureClearance::Shutdown(ShutdownSignal::CtrlC)
        );
        assert_eq!(out.checks, 2);
    }

    #[test]
    fn idle_dispatch_is_a_macos_only_policy() {
        assert_eq!(DISPATCH_WHEN_IDLE, cfg!(target_os = "macos"));
    }

    #[tokio::test]
    async fn pause_loop_renders_while_paused_with_zero_running_tasks() {
        let out = drive(
            vec![paused_pressure(), clear_pressure()],
            vec![PauseTick::ProgressDue, PauseTick::ReCheck],
        )
        .await;

        assert_eq!(out.clearance, PressureClearance::Dispatch);
        assert_eq!(out.renders, 1);
        assert_eq!(out.checks, 2);
    }
}
