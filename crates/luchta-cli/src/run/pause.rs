use std::{future::Future, pin::Pin, sync::Arc};

use miette::Result;

use super::{dispatch_ready_task, shutdown_signal, DispatchContext, ShutdownSignal};
use crate::{
    cli::OutputMode,
    memory_pressure::{MemoryMonitor, MemoryPressure, PressureReason, PressureState},
};
use luchta_engine::ReadyTaskMessage;

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
    Shutdown,
}

/// The real implementation (ProdPressureEnv) preserves the exact behavior
/// of original pause loop: 250ms TTL, no timeout escape hatch, intentional
/// pause-forever comment.
pub(super) trait PressureEnv {
    /// Check current memory pressure. Updates pressure_state for Task 5 visibility.
    fn check(&mut self) -> MemoryPressure;

    /// Await next tick event: re-check timer, progress interval, or shutdown.
    ///
    /// In production, this is `tokio::select!` over three futures.
    async fn next_tick(&mut self) -> PauseTick;

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
    Shutdown,
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
pub(super) async fn await_pressure_clearance<E: PressureEnv>(env: &mut E) -> PressureClearance {
    if !env.check().paused {
        return PressureClearance::Dispatch;
    }

    // **Intentional pause-forever behavior**: If pressure never clears,
    // this loop runs forever. No timeout escape hatch.
    loop {
        match env.next_tick().await {
            PauseTick::ReCheck => {
                if !env.check().paused {
                    return PressureClearance::Dispatch;
                }
            }
            PauseTick::ProgressDue => env.render_progress(),
            PauseTick::Shutdown => return PressureClearance::Shutdown,
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

impl<'a> PressureEnv for ProdPressureEnv<'a> {
    fn check(&mut self) -> MemoryPressure {
        let pressure = self.monitor.check();
        self.pressure_state.update(&pressure);
        pressure
    }

    async fn next_tick(&mut self) -> PauseTick {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                PauseTick::ReCheck
            }
            _ = self.progress_interval.tick() => {
                PauseTick::ProgressDue
            }
            _ = self.shutdown_signal.as_mut() => {
                PauseTick::Shutdown
            }
        }
    }

    fn render_progress(&self) {
        render_status_line(self.progress_reporter, self.pressure_state);
    }
}

/// Emits the periodic status line (with any memory-pressure warning suffix),
/// but only in default output mode while tasks are running. Shared by the
/// outer dispatch loop and the in-pause progress tick so the formatting stays
/// in one place.
fn render_status_line(
    reporter: &crate::progress::ProgressReporter,
    pressure_state: &PressureState,
) {
    if reporter.mode == OutputMode::Default && reporter.running_count() > 0 {
        let rss = crate::rss::format_rss(crate::rss::process_tree_rss_bytes());
        let reasons_guard = pressure_state.reasons();
        let reasons: &[PressureReason] = &reasons_guard;
        eprintln!("{}", reporter.render_progress(&rss, reasons));
    }
}

pub(super) async fn dispatch_loop(
    receiver: &mut tokio::sync::mpsc::Receiver<ReadyTaskMessage>,
    ctx: &DispatchContext<'_>,
    monitor: &mut MemoryMonitor,
    pressure_state: &Arc<PressureState>,
) -> Result<()> {
    // A SINGLE shutdown future for the whole loop. Both the outer select arm
    // and the inner pause loop (via ProdPressureEnv) poll this same future, so
    // a signal delivered while transitioning into the pause loop is never lost.
    let mut signal: ShutdownFuture = shutdown_signal();
    let mut progress_interval = tokio::time::interval(super::progress_interval_duration());
    progress_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    progress_interval.tick().await;

    loop {
        tokio::select! {
            signal_result = signal.as_mut() => {
                let shutdown = signal_result?;
                ctx.interrupted.store(true, std::sync::atomic::Ordering::SeqCst);
                eprintln!(
                    "Interrupted by {}: {} tasks running after {}s; RSS: {}",
                    shutdown.name(),
                    ctx.reporter.running_count(),
                    ctx.reporter.start.elapsed().as_secs(),
                    crate::rss::format_rss(crate::rss::process_tree_rss_bytes()),
                );
                break Err(miette::miette!("interrupted"));
            }
            message = receiver.recv() => {
                let Some((task_node, done_tx)) = message else {
                    break Ok(());
                };

                let mut env = ProdPressureEnv {
                    monitor,
                    pressure_state,
                    progress_interval: &mut progress_interval,
                    progress_reporter: ctx.reporter,
                    shutdown_signal: &mut signal,
                };
                match await_pressure_clearance(&mut env).await {
                    PressureClearance::Dispatch => dispatch_ready_task(task_node, done_tx, ctx),
                    PressureClearance::Shutdown => return interrupted_during_pause(ctx),
                }
            }
            _ = progress_interval.tick() => render_status_line(ctx.reporter, pressure_state),
        }
    }
}

fn interrupted_during_pause(ctx: &DispatchContext<'_>) -> Result<()> {
    ctx.interrupted
        .store(true, std::sync::atomic::Ordering::SeqCst);
    eprintln!(
        "Interrupted: {} tasks running after {}s; RSS: {}",
        ctx.reporter.running_count(),
        ctx.reporter.start.elapsed().as_secs(),
        crate::rss::format_rss(crate::rss::process_tree_rss_bytes()),
    );
    Err(miette::miette!("interrupted"))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::memory_pressure::{MemoryPressure, MemorySample};

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
    }

    impl FakePressureEnv {
        fn new(check_results: Vec<MemoryPressure>, tick_events: Vec<PauseTick>) -> Self {
            Self {
                check_results: check_results.into(),
                tick_events: tick_events.into(),
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

        async fn next_tick(&mut self) -> PauseTick {
            self.tick_events
                .pop_front()
                .expect("FakePressureEnv: next_tick() called but no events remaining")
        }

        fn render_progress(&self) {
            self.render_calls.fetch_add(1, Ordering::SeqCst);
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
        let mut env = FakePressureEnv::new(check_results, tick_events);
        let clearance = await_pressure_clearance(&mut env).await;
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
        let out = drive(vec![paused_pressure()], vec![PauseTick::Shutdown]).await;

        assert_eq!(out.clearance, PressureClearance::Shutdown);
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
}
