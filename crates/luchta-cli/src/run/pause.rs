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

/// The real implementation (ProdPressureEnv) preserves the exact behavior of
/// the original pause loop: a 250ms pressure re-check that resumes dispatch
/// automatically once pressure clears, with no timeout that force-resumes
/// while pressure persists.
pub(super) trait PressureEnv {
    /// Check current memory pressure. Updates pressure_state for Task 5 visibility.
    fn check(&mut self) -> MemoryPressure;

    /// Emit a one-shot notice that new task dispatch has paused on memory
    /// pressure. Fires once per pause episode, in every output mode, so the
    /// escape hatch stays discoverable even under `--output summary` where
    /// periodic status lines are suppressed.
    fn notify_paused(&self);

    /// Emit a one-shot notice that the pressure cleared and dispatch resumes.
    fn notify_resumed(&self);

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
/// dispatch task), or `PressureClearance::Shutdown` if interrupted. Emits
/// one-shot pause and resume notices around each pressure pause episode.
///
/// Dispatch resumes automatically: the loop re-checks pressure every 250ms
/// (via `PauseTick::ReCheck`) and returns `Dispatch` as soon as pressure
/// clears. What it deliberately does NOT have is a timeout that force-resumes
/// while pressure persists — if pressure never clears, this function does not
/// return, and the user must interrupt with Ctrl-C/SIGTERM (or rerun with
/// `--no-mem-pressure`). Holding until the OS says memory is available is BY
/// DESIGN; resuming into a thrashing machine would defeat the backpressure.
pub(super) async fn await_pressure_clearance<E: PressureEnv>(
    env: &mut E,
) -> Result<PressureClearance> {
    if !env.check().paused {
        return Ok(PressureClearance::Dispatch);
    }

    env.notify_paused();

    // Re-check pressure every 250ms and resume the moment it clears. There is
    // no timeout that force-resumes while pressure persists, so if it never
    // clears the loop waits until a shutdown signal arrives.
    loop {
        match env.next_tick().await? {
            PauseTick::ReCheck => {
                if !env.check().paused {
                    env.notify_resumed();
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

/// Builds the one-shot pause notice. Names the reason (when the OS gave one)
/// and the `--no-mem-pressure` escape hatch, so a run gated under `--output
/// summary` — where periodic status lines are suppressed — still explains why
/// it stopped and how to override. Pure so the wording is unit-testable
/// without a live reporter.
fn pause_notice_line(detail: Option<crate::memory_pressure::PressureDetail>) -> String {
    let reason = match detail {
        Some(d) => format!("memory pressure ({d})"),
        None => "memory pressure".to_string(),
    };
    format!("New task dispatch paused: {reason}. Ctrl-C to stop; rerun with --no-mem-pressure to bypass.")
}

impl<'a> PressureEnv for ProdPressureEnv<'a> {
    fn check(&mut self) -> MemoryPressure {
        let pressure = self.monitor.check();
        self.pressure_state.update(&pressure);
        pressure
    }

    fn notify_paused(&self) {
        let line = pause_notice_line(self.pressure_state.snapshot().detail);
        self.progress_reporter.output().stderr_line(
            &line
                .as_str()
                .if_supports_color(owo_colors::Stream::Stderr, |t| t.yellow())
                .to_string(),
        );
    }

    fn notify_resumed(&self) {
        self.progress_reporter
            .output()
            .stderr_line("Memory pressure cleared; resuming task dispatch.");
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
    let rss = crate::rss::format_rss(reporter.tree_rss());
    let output = reporter.output();
    let line = reporter.render_progress_for_width(crate::progress::ProgressRenderContext {
        rss_formatted: &rss,
        pressure: pressure.detail,
        stream: owo_colors::Stream::Stderr,
        max_width: output.terminal_width(),
    });
    output.progress_line(&line);
}

fn should_render(paused: bool, running_count: usize, mode: OutputMode) -> bool {
    mode != OutputMode::Summary && (paused || running_count > 0)
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
                break report_interrupted(ctx, Some(shutdown));
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
                        return report_interrupted(ctx, Some(shutdown));
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
                    return report_interrupted(ctx, Some(shutdown));
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

fn report_interrupted(ctx: &DispatchContext<'_>, shutdown: Option<ShutdownSignal>) -> Result<()> {
    ctx.interrupted
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let rss = ctx.reporter.tree_rss();
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
    use crate::memory_pressure::{MemoryPressure, PressureDetail};

    /// Fake implementation of PressureEnv for deterministic testing.
    ///
    /// Memory pressure checks come from queue, next_tick events
    /// come from queue, and render_progress() calls are counted. No real time
    /// or tokio intervals involved.
    struct FakePressureEnv {
        check_results: VecDeque<MemoryPressure>,
        tick_events: VecDeque<PauseTick>,
        render_calls: AtomicUsize,
        check_calls: AtomicUsize,
        notify_paused_calls: AtomicUsize,
        notify_resumed_calls: AtomicUsize,
    }

    impl FakePressureEnv {
        fn new(check_results: Vec<MemoryPressure>, tick_events: Vec<PauseTick>) -> Self {
            Self {
                check_results: check_results.into(),
                tick_events: tick_events.into(),
                render_calls: AtomicUsize::new(0),
                check_calls: AtomicUsize::new(0),
                notify_paused_calls: AtomicUsize::new(0),
                notify_resumed_calls: AtomicUsize::new(0),
            }
        }

        fn render_count(&self) -> usize {
            self.render_calls.load(Ordering::SeqCst)
        }

        fn check_count(&self) -> usize {
            self.check_calls.load(Ordering::SeqCst)
        }

        fn notify_paused_count(&self) -> usize {
            self.notify_paused_calls.load(Ordering::SeqCst)
        }

        fn notify_resumed_count(&self) -> usize {
            self.notify_resumed_calls.load(Ordering::SeqCst)
        }
    }

    impl PressureEnv for FakePressureEnv {
        fn check(&mut self) -> MemoryPressure {
            self.check_calls.fetch_add(1, Ordering::SeqCst);
            self.check_results
                .pop_front()
                .expect("FakePressureEnv: check() called but no results remaining")
        }

        fn notify_paused(&self) {
            self.notify_paused_calls.fetch_add(1, Ordering::SeqCst);
        }

        fn notify_resumed(&self) {
            self.notify_resumed_calls.fetch_add(1, Ordering::SeqCst);
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
    }

    /// Builds a scripted verdict. `paused == true` carries a detail, matching
    /// the `detail.is_some() == paused` invariant; `false` carries none.
    /// Centralises the literal so the individual tests stay free of duplicated
    /// struct construction.
    fn pressure_sample(paused: bool) -> MemoryPressure {
        MemoryPressure {
            paused,
            detail: paused.then_some(PressureDetail::Stalled(42.0)),
        }
    }

    /// Scripted "paused" verdict (the OS reports pressure).
    fn paused_pressure() -> MemoryPressure {
        pressure_sample(true)
    }

    /// Scripted "cleared" verdict (no pressure).
    fn clear_pressure() -> MemoryPressure {
        pressure_sample(false)
    }

    /// Outcome of driving the pause loop against a scripted `FakePressureEnv`.
    struct PauseOutcome {
        clearance: PressureClearance,
        checks: usize,
        renders: usize,
        paused_notices: usize,
        resumed_notices: usize,
        remaining_ticks: usize,
    }

    /// Drives `await_pressure_clearance` with a scripted fake and reports the
    /// observable outcome. Keeps the individual tests to their assertions.
    async fn drive(
        check_results: Vec<MemoryPressure>,
        tick_events: Vec<PauseTick>,
    ) -> PauseOutcome {
        let mut env = FakePressureEnv::new(check_results, tick_events);
        let clearance = await_pressure_clearance(&mut env)
            .await
            .expect("pause clearance should succeed");
        PauseOutcome {
            clearance,
            checks: env.check_count(),
            renders: env.render_count(),
            paused_notices: env.notify_paused_count(),
            resumed_notices: env.notify_resumed_count(),
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
        assert_eq!(out.paused_notices, 1);
        assert_eq!(out.resumed_notices, 1);
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
        assert_eq!(out.paused_notices, 1);
        assert_eq!(out.resumed_notices, 0);
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
        assert_eq!(out.paused_notices, 0);
        assert_eq!(out.resumed_notices, 0);
        assert_eq!(out.remaining_ticks, 2);
    }

    #[tokio::test]
    async fn pause_loop_notifies_pause_and_resume_without_progress_tick() {
        let out = drive(
            vec![paused_pressure(), clear_pressure()],
            vec![PauseTick::ReCheck],
        )
        .await;

        assert_eq!(out.clearance, PressureClearance::Dispatch);
        assert_eq!(out.checks, 2);
        assert_eq!(out.renders, 0);
        assert_eq!(out.paused_notices, 1);
        assert_eq!(out.resumed_notices, 1);
    }

    #[test]
    fn render_status_line_shows_os_pressure_reason() {
        let reporter = crate::progress::ProgressReporter::new(
            OutputMode::Default,
            std::collections::HashMap::new(),
            0,
        );
        let pressure_state = PressureState::new();
        pressure_state.update(&MemoryPressure {
            paused: true,
            detail: Some(crate::memory_pressure::PressureDetail::Stalled(23.0)),
        });

        let line = reporter.render_progress(
            "32 MB",
            pressure_state.snapshot().detail,
            owo_colors::Stream::Stderr,
        );

        assert!(line.contains("🐏 32 MB"));
        assert!(line.contains("memory pressure (stalled 23%)"));
    }

    #[test]
    fn pause_notice_names_reason_and_escape_hatch() {
        let line = pause_notice_line(Some(PressureDetail::Stalled(60.0)));

        // The whole point of #343: a gated run must say why it paused and how
        // to override, even under `--output summary`.
        assert!(line.contains("memory pressure (stalled 60%)"), "{line}");
        assert!(line.contains("--no-mem-pressure"), "{line}");
        assert!(line.contains("New task dispatch paused"), "{line}");
    }

    #[test]
    fn pause_notice_without_detail_still_names_escape_hatch() {
        let line = pause_notice_line(None);

        assert!(line.contains("memory pressure"), "{line}");
        assert!(!line.contains("("), "no empty reason parens: {line}");
        assert!(line.contains("--no-mem-pressure"), "{line}");
    }

    #[test]
    fn should_render_status_line_in_plain_mode_for_pause_or_running_matrix() {
        assert!(should_render(true, 0, OutputMode::Plain));
        assert!(should_render(true, 2, OutputMode::Plain));
        assert!(!should_render(false, 0, OutputMode::Plain));
        assert!(should_render(false, 2, OutputMode::Plain));
    }

    #[test]
    fn should_never_render_status_line_in_summary_mode() {
        assert!(!should_render(true, 0, OutputMode::Summary));
        assert!(!should_render(true, 2, OutputMode::Summary));
        assert!(!should_render(false, 2, OutputMode::Summary));
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
