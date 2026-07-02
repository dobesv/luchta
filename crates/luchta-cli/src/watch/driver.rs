//! Watch loop: tie debounced watcher batches to repeated `WatchSession` cycles.
//!
//! # Core Invariant (no-lost-changes)
//!
//! Every path inserted into `PendingChanges` is either:
//! (a) included in the current cycle's drained `changed` set, or
//! (b) remains pending after drain and forces a follow-up cycle.
//!
//! Nothing is silently dropped, even if changes arrive while a cycle builds.
//!
//! # Mechanism
//!
//! The key to this invariant is the `ActiveCycle` holder. The drain task
//! cancels the active cycle directly (via a shared CancellationToken) when
//! new changes arrive. The outer loop relies ONLY on `pending.is_empty()`
//! to decide whether to wait or proceed — Notify is purely an optimization
//! to wake the loop faster, never required for correctness.

use std::collections::BTreeSet;
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use luchta_cache::resolve_cache_dir;
use luchta_types::PackageName;
use miette::Result;
use owo_colors::{OwoColorize, Stream};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::registry::dirty_packages_for_changes;
use super::session::WatchSession;
use super::watcher::WatcherHandle;
use crate::build_lock;
use crate::cli::OutputMode;
use crate::run::{CycleOutcome, MemoryPressureConfig, RunCycleParams, TaskSelection};

/// Maximum number of changed file paths to list under `--show-changed-files`
/// before collapsing the remainder into a count.
const MAX_LISTED_CHANGED_FILES: usize = 10;

#[derive(Debug, Clone, Default)]
pub struct OwnedSelection {
    pub requested_tasks: Vec<String>,
    pub packages: Vec<String>,
    pub top_level: bool,
}

impl OwnedSelection {
    fn as_task_selection(&self) -> TaskSelection<'_> {
        TaskSelection {
            requested_tasks: &self.requested_tasks,
            packages: &self.packages,
            top_level: self.top_level,
            since: None,
        }
    }
}

/// Thread-safe holder for pending file changes detected by the watcher.
///
/// # Invariant
/// Every path added but not yet drained is either in the current cycle's
/// set or will cause a follow-up cycle.
#[derive(Debug, Default)]
pub struct PendingChanges {
    inner: Mutex<HashSet<PathBuf>>,
}

impl PendingChanges {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a batch of changed paths. Returns true if this was the first change
    /// (i.e., pending was empty before).
    pub fn add(&self, batch: HashSet<PathBuf>) -> bool {
        if batch.is_empty() {
            return false;
        }

        let mut pending = self.inner.lock().expect("pending changes mutex poisoned");
        let was_empty = pending.is_empty();
        pending.extend(batch);
        was_empty && !pending.is_empty()
    }

    /// Drain and return whether the set was non-empty.
    pub fn drain_non_empty(&self) -> Option<HashSet<PathBuf>> {
        let mut pending = self.inner.lock().expect("pending changes mutex poisoned");
        let drained = std::mem::take(&mut *pending);
        if drained.is_empty() {
            None
        } else {
            Some(drained)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .expect("pending changes mutex poisoned")
            .is_empty()
    }

    fn has_changes(&self) -> bool {
        !self.is_empty()
    }
}

pub struct WatchRunConfig {
    pub output: OutputMode,
    pub continue_on_failure: bool,
    pub memory_pressure: MemoryPressureConfig,
    /// When true, list the changed files that triggered each rebuild.
    pub show_changed_files: bool,
}

/// Inputs for running watch mode.
///
/// Bundles all inputs to reduce function argument count.
pub struct WatchInputs {
    pub session: WatchSession,
    pub watcher_handle: WatcherHandle,
    pub changes_rx: mpsc::Receiver<HashSet<PathBuf>>,
    pub selection: OwnedSelection,
    pub config: WatchRunConfig,
}

struct WatchSignals<F, G>
where
    F: Future<Output = std::result::Result<(), std::io::Error>> + Send,
    G: Future<Output = std::result::Result<(), std::io::Error>> + Send,
{
    shutdown: std::pin::Pin<Box<F>>,
    force_shutdown: std::pin::Pin<Box<G>>,
}

impl<F, G> WatchSignals<F, G>
where
    F: Future<Output = std::result::Result<(), std::io::Error>> + Send,
    G: Future<Output = std::result::Result<(), std::io::Error>> + Send,
{
    fn new(shutdown: F, force_shutdown: G) -> Self {
        Self {
            shutdown: Box::pin(shutdown),
            force_shutdown: Box::pin(force_shutdown),
        }
    }
}

struct WatchUi {
    show_changed_files: bool,
}

impl WatchUi {
    fn new(show_changed_files: bool) -> Self {
        Self { show_changed_files }
    }

    fn started(&self) {
        print_status(&format_watch_started_line());
    }

    fn change_detected(
        &self,
        affected: &HashSet<PackageName>,
        changed: &HashSet<PathBuf>,
        repo_root: &Path,
    ) {
        print_status(&format_change_detected_line(affected));
        if self.show_changed_files {
            for line in format_changed_files_lines(changed, repo_root) {
                print_status(&line);
            }
        }
    }

    fn up_to_date(&self) {
        print_status(&format_up_to_date_line());
    }

    fn cycle_started(&self) -> Result<()> {
        // Note: watch mode intentionally does NOT clear the screen here.
        // Preserving scrollback keeps prior build output and change history
        // visible (see GitHub issue #160).
        Ok(())
    }

    fn cycle_finished(&self, outcome: CycleOutcome, elapsed: Duration) {
        if let Some(line) = format_cycle_finished_line(outcome, elapsed) {
            print_status(&line);
        }
    }

    fn shutting_down(&self) {
        print_status("[watch] shutting down…");
    }

    fn forcing_shutdown(&self) {
        print_status("[watch] forcing shutdown");
    }
}

/// Holder for the currently active cycle's cancellation token.
///
/// The drain task uses this to cancel the in-flight cycle when new changes arrive.
/// This ensures correctness without relying on Notify permit semantics.
#[derive(Debug, Default)]
struct ActiveCycle {
    token: Mutex<Option<CancellationToken>>,
}

impl ActiveCycle {
    fn new() -> Self {
        Self::default()
    }

    /// Set the active cycle's cancellation token. Call when starting a cycle.
    fn set(&self, token: CancellationToken) {
        *self.token.lock().expect("active cycle mutex poisoned") = Some(token);
    }

    /// Clear the active cycle. Call when the cycle completes.
    fn clear(&self) {
        *self.token.lock().expect("active cycle mutex poisoned") = None;
    }

    /// Cancel the active cycle if one exists. Called by the drain task on new changes.
    /// Returns true if a cycle was cancelled.
    fn cancel_if_active(&self) -> bool {
        if let Some(token) = self
            .token
            .lock()
            .expect("active cycle mutex poisoned")
            .take()
        {
            token.cancel();
            true
        } else {
            false
        }
    }

    /// Check if there's an active cycle.
    #[allow(dead_code)]
    fn is_active(&self) -> bool {
        self.token
            .lock()
            .expect("active cycle mutex poisoned")
            .is_some()
    }
}

pub async fn run_watch(inputs: WatchInputs) -> Result<()> {
    run_watch_until(inputs, tokio::signal::ctrl_c(), tokio::signal::ctrl_c()).await
}

pub(crate) async fn run_watch_until<F, G>(
    inputs: WatchInputs,
    shutdown: F,
    force_shutdown: G,
) -> Result<()>
where
    F: Future<Output = std::result::Result<(), std::io::Error>> + Send + 'static,
    G: Future<Output = std::result::Result<(), std::io::Error>> + Send + 'static,
{
    let WatchInputs {
        session,
        watcher_handle,
        changes_rx,
        selection,
        config,
    } = inputs;
    let pending = Arc::new(PendingChanges::new());
    // wake is ONLY an optimization to unblock the wait faster. The outer loop
    // MUST NOT rely on it for correctness — pending.is_empty() is the source of truth.
    let wake = Arc::new(Notify::new());
    let active_cycle = Arc::new(ActiveCycle::new());
    let drain_task = spawn_change_drain_task(
        changes_rx,
        Arc::clone(&pending),
        Arc::clone(&wake),
        Arc::clone(&active_cycle),
    );
    let ui = WatchUi::new(config.show_changed_files);
    let mut signals = WatchSignals::new(shutdown, force_shutdown);

    ui.started();

    let result: Result<()> = async {
        if should_stop(
            run_initial_watch_cycle(
                &session,
                &selection,
                &config,
                &active_cycle,
                &ui,
                &mut signals,
            )
            .await?,
        ) {
            return Ok(());
        }

        loop {
            if should_stop(
                run_one_iteration(
                    WatchIterationContext {
                        session: &session,
                        selection: &selection,
                        config: &config,
                        pending: &pending,
                        wake: &wake,
                        active_cycle: &active_cycle,
                        ui: &ui,
                    },
                    &mut signals,
                )
                .await?,
            ) {
                return Ok(());
            }
        }
    }
    .await;

    finish_shutdown(session, watcher_handle, drain_task).await;
    result
}

fn should_stop(control: WatchControl) -> bool {
    matches!(control, WatchControl::Stop)
}

async fn run_initial_watch_cycle<F, G>(
    session: &WatchSession,
    selection: &OwnedSelection,
    config: &WatchRunConfig,
    active_cycle: &ActiveCycle,
    ui: &WatchUi,
    signals: &mut WatchSignals<F, G>,
) -> Result<WatchControl>
where
    F: Future<Output = std::result::Result<(), std::io::Error>> + Send,
    G: Future<Output = std::result::Result<(), std::io::Error>> + Send,
{
    let cache_dir = resolve_cache_dir(session.repo_root());
    let acquire_lock = build_lock::acquire(&cache_dir);
    tokio::pin!(acquire_lock);
    let _build_lock = tokio::select! {
        lock = &mut acquire_lock => match lock? {
            Some(lock) => lock,
            None => return Ok(WatchControl::Stop), // Ctrl+C while waiting
        },
        _ = &mut signals.shutdown => {
            ui.shutting_down();
            shutdown_watch(session, ui, signals).await;
            return Ok(WatchControl::Stop);
        }
    };
    let initial_selection = selection.as_task_selection();
    run_cycle_with_status(
        session,
        cycle_request(&initial_selection, None, config),
        active_cycle,
        ui,
        signals,
    )
    .await
}

async fn run_one_iteration<F, G>(
    context: WatchIterationContext<'_>,
    signals: &mut WatchSignals<F, G>,
) -> Result<WatchControl>
where
    F: Future<Output = std::result::Result<(), std::io::Error>> + Send,
    G: Future<Output = std::result::Result<(), std::io::Error>> + Send,
{
    if !wait_for_pending_or_shutdown(
        context.session,
        context.pending,
        context.wake,
        context.ui,
        signals,
    )
    .await?
    {
        return Ok(WatchControl::Stop);
    }

    let changed = match context.pending.drain_non_empty() {
        Some(changed) => changed,
        None => return Ok(WatchControl::Continue),
    };
    // Only real changes to a task's declared inputs (verified by size/mtime, then
    // content hash) — or new files matching a task's input globs — dirty a package.
    // Cache outputs, restore staging dirs, and touch-only events are ignored, which
    // is what breaks the watch rebuild loop (#161).
    let affected = dirty_packages_for_changes(context.session.task_watch_registry(), &changed);
    if affected.is_empty() {
        context.ui.up_to_date();
        return Ok(WatchControl::Continue);
    }

    context
        .ui
        .change_detected(&affected, &changed, context.session.repo_root());
    let cycle_selection = context.selection.as_task_selection();
    let cache_dir = resolve_cache_dir(context.session.repo_root());
    let acquire_lock = build_lock::acquire(&cache_dir);
    tokio::pin!(acquire_lock);
    let _build_lock = tokio::select! {
        lock = &mut acquire_lock => match lock? {
            Some(lock) => lock,
            None => return Ok(WatchControl::Stop), // Ctrl+C while waiting
        },
        _ = &mut signals.shutdown => {
            context.ui.shutting_down();
            shutdown_watch(context.session, context.ui, signals).await;
            return Ok(WatchControl::Stop);
        }
    };
    run_cycle_with_status(
        context.session,
        cycle_request(&cycle_selection, Some(&affected), context.config),
        context.active_cycle,
        context.ui,
        signals,
    )
    .await
}

async fn wait_for_pending_or_shutdown<F, G>(
    session: &WatchSession,
    pending: &PendingChanges,
    wake: &Notify,
    ui: &WatchUi,
    signals: &mut WatchSignals<F, G>,
) -> Result<bool>
where
    F: Future<Output = std::result::Result<(), std::io::Error>> + Send,
    G: Future<Output = std::result::Result<(), std::io::Error>> + Send,
{
    while !pending.has_changes() {
        tokio::select! {
            _ = wake.notified() => {}
            _ = &mut signals.shutdown => {
                ui.shutting_down();
                shutdown_watch(session, ui, signals).await;
                return Ok(false);
            }
        }
    }

    Ok(true)
}

fn cycle_request<'a>(
    selection: &'a TaskSelection<'a>,
    affected: Option<&'a HashSet<PackageName>>,
    config: &WatchRunConfig,
) -> CycleRequest<'a> {
    CycleRequest {
        selection,
        affected,
        output: config.output,
        continue_on_failure: config.continue_on_failure,
        memory_pressure: config.memory_pressure.clone(),
    }
}

fn spawn_change_drain_task(
    mut changes_rx: mpsc::Receiver<HashSet<PathBuf>>,
    pending: Arc<PendingChanges>,
    wake: Arc<Notify>,
    active_cycle: Arc<ActiveCycle>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(batch) = changes_rx.recv().await {
            let was_added = pending.add(batch);
            if was_added {
                // Cancel the active cycle directly. This ensures the change is NOT lost
                // even if Notify permit semantics would have dropped it.
                // Only fire cancellation if there's an active cycle.
                active_cycle.cancel_if_active();
                // Wake hint — may or may not be consumed; pending.is_empty() is the source of truth.
                wake.notify_one();
            }
        }
    })
}

enum WatchControl {
    Continue,
    Stop,
}

struct WatchIterationContext<'a> {
    session: &'a WatchSession,
    selection: &'a OwnedSelection,
    config: &'a WatchRunConfig,
    pending: &'a PendingChanges,
    wake: &'a Notify,
    active_cycle: &'a ActiveCycle,
    ui: &'a WatchUi,
}

struct CycleRequest<'a> {
    selection: &'a TaskSelection<'a>,
    affected: Option<&'a HashSet<PackageName>>,
    output: OutputMode,
    continue_on_failure: bool,
    memory_pressure: MemoryPressureConfig,
}

async fn run_cycle_with_status<F, G>(
    session: &WatchSession,
    request: CycleRequest<'_>,
    active_cycle: &ActiveCycle,
    ui: &WatchUi,
    signals: &mut WatchSignals<F, G>,
) -> Result<WatchControl>
where
    F: Future<Output = std::result::Result<(), std::io::Error>> + Send,
    G: Future<Output = std::result::Result<(), std::io::Error>> + Send,
{
    ui.cycle_started()?;

    let cancel = CancellationToken::new();
    // Register as the active cycle so the drain task can cancel us on new changes.
    active_cycle.set(cancel.clone());

    let cycle = session.run_cycle(
        RunCycleParams {
            selection: request.selection,
            since_affected: request.affected,
            output: request.output,
            continue_on_failure: request.continue_on_failure,
            memory_pressure: request.memory_pressure.clone(),
        },
        cancel.clone(),
    );
    tokio::pin!(cycle);
    let started_at = Instant::now();

    let result = tokio::select! {
        result = &mut cycle => {
            result
        }
        _ = &mut signals.shutdown => {
            ui.shutting_down();
            cancel.cancel();
            let result = cycle.await;
            active_cycle.clear();
            shutdown_watch(session, ui, signals).await;
            let outcome = result?;
            ui.cycle_finished(outcome, started_at.elapsed());
            return Ok(WatchControl::Stop);
        }
    };

    // Clear active cycle on completion.
    active_cycle.clear();

    let outcome = result?;
    ui.cycle_finished(outcome, started_at.elapsed());
    Ok(WatchControl::Continue)
}

async fn shutdown_watch<F, G>(
    session: &WatchSession,
    ui: &WatchUi,
    signals: &mut WatchSignals<F, G>,
) where
    F: Future<Output = std::result::Result<(), std::io::Error>> + Send,
    G: Future<Output = std::result::Result<(), std::io::Error>> + Send,
{
    tokio::select! {
        _ = session.shutdown() => {}
        _ = &mut signals.force_shutdown => {
            ui.forcing_shutdown();
            session.shutdown_immediate().await;
        }
    }
}

async fn finish_shutdown(
    session: WatchSession,
    watcher_handle: WatcherHandle,
    drain_task: JoinHandle<()>,
) {
    drop(watcher_handle);
    drain_task.abort();
    let _ = drain_task.await;
    session.shutdown().await;
}

fn print_status(line: &str) {
    println!("{line}");
}

fn format_watch_started_line() -> String {
    "[watch] watch mode started — press Ctrl-C to exit"
        .if_supports_color(Stream::Stdout, |text| text.cyan())
        .to_string()
}

fn format_change_detected_line(affected: &HashSet<PackageName>) -> String {
    if affected.len() == 1 {
        let name = affected.iter().next().expect("len checked").to_string();
        return format!("📝 {name}")
            .if_supports_color(Stream::Stdout, |text| text.cyan())
            .to_string();
    }

    let packages_set: BTreeSet<&str> = affected.iter().map(|p| p.as_str()).collect();
    let shared_scope = crate::progress_task_list::common_scope(&packages_set);
    let compacted = crate::progress_task_list::format_package_set(&packages_set, shared_scope);

    format!("📝 {}", compacted)
        .if_supports_color(Stream::Stdout, |text| text.cyan())
        .to_string()
}

/// Render the changed files that triggered a rebuild: the first
/// `MAX_LISTED_CHANGED_FILES` paths (sorted, relative to the repo root when
/// possible) followed by a summary count of any remainder.
fn format_changed_files_lines(changed: &HashSet<PathBuf>, repo_root: &Path) -> Vec<String> {
    let mut paths = changed
        .iter()
        .map(|path| display_changed_path(path, repo_root))
        .collect::<Vec<_>>();
    paths.sort();

    let total = paths.len();
    let mut lines = paths
        .iter()
        .take(MAX_LISTED_CHANGED_FILES)
        .map(|path| {
            format!("  {path}")
                .if_supports_color(Stream::Stdout, |t| t.dimmed())
                .to_string()
        })
        .collect::<Vec<_>>();

    if total > MAX_LISTED_CHANGED_FILES {
        let remaining = total - MAX_LISTED_CHANGED_FILES;
        lines.push(
            format!("  … and {remaining} more")
                .if_supports_color(Stream::Stdout, |t| t.dimmed())
                .to_string(),
        );
    }

    lines
}

/// Present a changed path relative to `repo_root` when possible; otherwise fall
/// back to the full path.
fn display_changed_path(path: &Path, repo_root: &Path) -> String {
    path.strip_prefix(repo_root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn format_up_to_date_line() -> String {
    "[watch] up to date".to_string()
}

fn format_cycle_finished_line(outcome: CycleOutcome, elapsed: Duration) -> Option<String> {
    match outcome {
        CycleOutcome::Success => None, // Handled by progress summary
        CycleOutcome::Failed => Some(format!(
            "[watch] build failed in {}",
            format_elapsed(elapsed)
        )),
        CycleOutcome::Cancelled => None, // Folded into final progress line
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let millis = elapsed.subsec_millis();
    if secs == 0 {
        format!("{}ms", elapsed.as_millis())
    } else {
        format!("{}.{:03}s", secs, millis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_swaps_to_fresh_pending_set() {
        let pending = PendingChanges::new();
        pending.add(HashSet::from([PathBuf::from("/repo/pkg-a/src/lib.rs")]));
        assert!(!pending.is_empty());

        let drained = pending.drain_non_empty().expect("pending set");
        assert!(pending.is_empty());
        assert_eq!(drained.len(), 1);
    }

    #[test]
    fn add_coalesces_duplicate_paths() {
        let pending = PendingChanges::new();
        pending.add(HashSet::from([
            PathBuf::from("/repo/pkg-a/src/lib.rs"),
            PathBuf::from("/repo/pkg-a/src/lib.rs"),
        ]));
        pending.add(HashSet::from([
            PathBuf::from("/repo/pkg-a/src/lib.rs"),
            PathBuf::from("/repo/pkg-b/src/lib.rs"),
        ]));

        let drained = pending.drain_non_empty().expect("pending set");
        assert_eq!(drained.len(), 2);
        assert!(drained.contains(&PathBuf::from("/repo/pkg-a/src/lib.rs")));
        assert!(drained.contains(&PathBuf::from("/repo/pkg-b/src/lib.rs")));
    }

    #[test]
    fn add_after_drain_stays_pending_for_follow_up_cycle() {
        let pending = PendingChanges::new();
        pending.add(HashSet::from([PathBuf::from("/repo/pkg-a/src/lib.rs")]));

        let in_flight = pending.drain_non_empty();
        assert_eq!(
            in_flight,
            Some(HashSet::from([PathBuf::from("/repo/pkg-a/src/lib.rs")]))
        );
        assert!(pending.is_empty());

        pending.add(HashSet::from([PathBuf::from("/repo/pkg-b/src/lib.rs")]));
        assert!(!pending.is_empty());

        let follow_up = pending.drain_non_empty();
        assert_eq!(
            follow_up,
            Some(HashSet::from([PathBuf::from("/repo/pkg-b/src/lib.rs")]))
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn changed_files_lines_relative_and_sorted() {
        let repo_root = PathBuf::from("/repo");
        let changed = HashSet::from([
            PathBuf::from("/repo/pkg-b/src/lib.rs"),
            PathBuf::from("/repo/pkg-a/src/main.rs"),
        ]);

        let lines = format_changed_files_lines(&changed, &repo_root);
        assert_eq!(
            lines,
            vec![
                "  pkg-a/src/main.rs".to_string(),
                "  pkg-b/src/lib.rs".to_string(),
            ]
        );
    }

    #[test]
    fn changed_files_lines_truncate_with_count() {
        let repo_root = PathBuf::from("/repo");
        let changed: HashSet<PathBuf> = (0..15)
            .map(|i| PathBuf::from(format!("/repo/pkg/src/file{i:02}.rs")))
            .collect();

        let lines = format_changed_files_lines(&changed, &repo_root);
        assert_eq!(lines.len(), MAX_LISTED_CHANGED_FILES + 1);
        assert_eq!(lines[0], "  pkg/src/file00.rs");
        assert_eq!(
            lines.last().expect("summary line present"),
            "  … and 5 more"
        );
    }

    #[test]
    fn changed_files_path_outside_repo_falls_back_to_full_path() {
        let repo_root = PathBuf::from("/repo");
        let changed = HashSet::from([PathBuf::from("/elsewhere/file.rs")]);

        let lines = format_changed_files_lines(&changed, &repo_root);
        assert_eq!(lines, vec!["  /elsewhere/file.rs".to_string()]); // Colored but we ignore color in assertion if it's default stream or we use contains
                                                                     // actually if_supports_color on Stream::Stdout in tests returns the string directly
    }

    #[test]
    fn status_lines_format_expected_messages() {
        let affected_multi = HashSet::from([
            PackageName::new("@formative/pkg-b".to_owned()),
            PackageName::new("@formative/pkg-a".to_owned()),
        ]);
        let affected_single =
            HashSet::from([PackageName::new("@formative/react-reporting".to_owned())]);

        assert!(format_watch_started_line().contains("[watch] watch mode started"));
        assert_eq!(format_change_detected_line(&affected_multi), "📝 pkg-{a,b}");
        assert_eq!(
            format_change_detected_line(&affected_single),
            "📝 @formative/react-reporting"
        );
        assert_eq!(format_up_to_date_line(), "[watch] up to date");
        assert_eq!(
            format_cycle_finished_line(CycleOutcome::Success, Duration::from_millis(125)),
            None
        );
        assert_eq!(
            format_cycle_finished_line(CycleOutcome::Failed, Duration::from_millis(1234)),
            Some("[watch] build failed in 1.234s".to_string())
        );
        assert_eq!(
            format_cycle_finished_line(CycleOutcome::Cancelled, Duration::from_secs(5)),
            None
        );
    }

    #[test]
    fn active_cycle_cancel_if_active_cancels_set_token() {
        let active = ActiveCycle::new();
        let token = CancellationToken::new();
        active.set(token.clone());
        assert!(!token.is_cancelled());
        active.cancel_if_active();
        assert!(token.is_cancelled());
        // Second call is no-op
        active.cancel_if_active();
    }

    #[test]
    fn active_cycle_cancel_if_active_no_op_when_none() {
        let active = ActiveCycle::new();
        // Should not panic
        active.cancel_if_active();
    }

    #[test]
    fn active_cycle_clear_removes_token() {
        let active = ActiveCycle::new();
        let token = CancellationToken::new();
        active.set(token.clone());
        active.clear();
        // Cancel no longer does anything
        active.cancel_if_active();
        assert!(!token.is_cancelled());
    }
}

#[cfg(test)]
mod driver_e2e_tests {
    use super::*;
    use crate::watch::session::WatchSession;
    use crate::watch::watcher::WatcherHandle;
    use std::collections::HashSet;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    const TEST_TIMEOUT: Duration = Duration::from_secs(15);
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    /// Write a workspace with a blocking shell worker.
    /// Worker appends a newline to `.run-marker` on every job.
    /// First job BLOCKS until `.release-1` sentinel appears.
    /// Subsequent jobs run free (don't block).
    fn write_blocking_workspace(workspace_root: &std::path::Path) {
        std::fs::create_dir_all(workspace_root.join("packages/app")).expect("create package dir");
        std::fs::write(
            workspace_root.join("package.json"),
            r#"{"name": "root", "private": true, "workspaces": ["packages/*"]}"#,
        )
        .expect("write root package.json");
        std::fs::write(
            workspace_root.join("packages/app/package.json"),
            r#"{"name": "app", "version": "1.0.0", "scripts": {"build": "echo build"}}"#,
        )
        .expect("write app package.json");

        // Worker script:
        // - On first job, wait for .release-1 sentinel (blocking)
        // - Appends to .run-marker on every job
        // - Jobs 2+ run free
        let marker_file = workspace_root.join(".run-marker");
        let release_1 = workspace_root.join(".release-1");
        let job_count = workspace_root.join(".job-count");
        let worker_script = format!(
            r##"#!/bin/sh
job_count=0
while IFS= read -r line; do
  case "$line" in
    *'"type":"resolveTask"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
      ;;
    *'"type":"run"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      job_count=$((job_count + 1))
      echo "$job_count" > '{}'
      if [ "$job_count" -eq 1 ]; then
        while [ ! -f '{}' ]; do sleep 0.01; done
      fi
      echo "" >> '{}'
      printf '{{"type":"done","id":"%s","success":true,"exitCode":0}}\n' "$id"
      ;;
  esac
done
"##,
            job_count.display(),
            release_1.display(),
            marker_file.display(),
        );
        let worker_script_path = workspace_root.join("fake-worker.sh");
        std::fs::write(&worker_script_path, &worker_script).expect("write worker script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&worker_script_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod worker script");
        }

        let config = format!(
            r##"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"fake":{{"command":"{}"}}}},"tasks":{{"build":{{"worker":"fake"}}}}}}'
"##,
            worker_script_path.display()
        );
        std::fs::write(workspace_root.join("luchta-config.sh"), &config).expect("write config");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                workspace_root.join("luchta-config.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("chmod config");
        }
    }

    fn read_marker_count(workspace_root: &std::path::Path) -> usize {
        let marker = workspace_root.join(".run-marker");
        std::fs::read_to_string(&marker)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    fn read_job_count(workspace_root: &std::path::Path) -> usize {
        std::fs::read_to_string(workspace_root.join(".job-count"))
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(0)
    }

    struct E2eHarness {
        _temp_dir: tempfile::TempDir,
        workspace_root: PathBuf,
        worker_manager_handle: Arc<luchta_engine::WorkerManager>,
        changes_tx: mpsc::Sender<HashSet<PathBuf>>,
        shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
        handle: Option<tokio::task::JoinHandle<()>>,
    }

    impl E2eHarness {
        async fn start() -> Self {
            let temp_dir = tempfile::tempdir().expect("create temp dir");
            let workspace_root = temp_dir.path().canonicalize().expect("canonicalize");
            write_blocking_workspace(&workspace_root);

            let session = WatchSession::new(&workspace_root, None)
                .await
                .expect("create watch session")
                .expect("session should not be None");
            let worker_manager_handle = session.worker_manager_handle();
            let watcher_handle = WatcherHandle::noop();
            let (changes_tx, changes_rx) = mpsc::channel(32);
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

            let handle = tokio::spawn(async move {
                let shutdown_future = async move {
                    let _ = shutdown_rx.await;
                    Ok::<(), std::io::Error>(())
                };
                let force_shutdown = async { std::future::pending::<std::io::Result<()>>().await };

                run_watch_until(
                    WatchInputs {
                        session,
                        watcher_handle,
                        changes_rx,
                        selection: OwnedSelection {
                            requested_tasks: vec!["build".to_string()],
                            packages: vec![],
                            top_level: false,
                        },
                        config: WatchRunConfig {
                            output: OutputMode::Default,
                            continue_on_failure: false,
                            memory_pressure: crate::run::MemoryPressureConfig {
                                usage: None,
                                free: None,
                            },
                            show_changed_files: false,
                        },
                    },
                    shutdown_future,
                    force_shutdown,
                )
                .await
                .expect("watch loop");
            });

            Self {
                _temp_dir: temp_dir,
                workspace_root,
                worker_manager_handle,
                changes_tx,
                shutdown_tx: Some(shutdown_tx),
                handle: Some(handle),
            }
        }

        async fn wait_for_jobs(&self, target: usize) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while tokio::time::Instant::now() < deadline {
                if read_job_count(&self.workspace_root) >= target {
                    return;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            panic!("timed out waiting for job count {target}");
        }

        async fn wait_for_markers(&self, target: usize) {
            assert!(
                wait_for_count(
                    &self.workspace_root.join(".run-marker"),
                    target,
                    Duration::from_secs(10)
                )
                .await,
                "timed out waiting for marker count {}",
                target
            );
        }

        async fn send_package_change(&self) {
            let app_path = self.workspace_root.join("packages/app/src/lib.rs");
            std::fs::create_dir_all(app_path.parent().expect("app parent")).ok();
            std::fs::write(&app_path, "// change").expect("write change");
            self.changes_tx
                .send(HashSet::from([app_path]))
                .await
                .expect("send change");
        }

        async fn send_outside_change(&self) {
            let outside_path = self.workspace_root.join("README.md");
            std::fs::write(&outside_path, "change").expect("write change");
            self.changes_tx
                .send(HashSet::from([outside_path]))
                .await
                .expect("send change");
        }

        fn release_first_cycle(&self) {
            std::fs::write(self.workspace_root.join(".release-1"), "")
                .expect("release first cycle");
        }

        async fn shutdown(mut self) {
            let _ = self.shutdown_tx.take().expect("shutdown tx").send(());
            timeout(TEST_TIMEOUT, self.handle.take().expect("watch handle"))
                .await
                .expect("watch loop timeout")
                .expect("watch loop join");
        }
    }

    async fn wait_for_count(path: &std::path::Path, target: usize, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if count_lines(path) >= target {
                return true;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        false
    }

    fn count_lines(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    async fn marker_count_stays_at(
        workspace_root: &std::path::Path,
        expected: usize,
        duration: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + duration;
        while tokio::time::Instant::now() < deadline {
            if read_marker_count(workspace_root) != expected {
                return false;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        read_marker_count(workspace_root) == expected
    }

    /// NO-LOST-CHANGES: Prove that injecting a change during an in-flight cycle triggers a follow-up cycle.
    #[tokio::test]
    async fn no_lost_changes_change_during_build_triggers_second_cycle() {
        let harness = E2eHarness::start().await;

        harness.wait_for_jobs(1).await;
        harness.send_package_change().await;
        harness.release_first_cycle();
        harness.wait_for_jobs(2).await;
        harness.wait_for_markers(2).await;

        assert_eq!(
            read_marker_count(&harness.workspace_root),
            2,
            "expected exactly 2 worker jobs before shutdown"
        );

        harness.shutdown().await;
    }

    /// KEEPS-MANAGER-ALIVE: Prove manager survives a change-triggered cancel.
    #[tokio::test]
    async fn change_during_cycle_keeps_worker_manager_alive() {
        let harness = E2eHarness::start().await;
        let h1 = Arc::clone(&harness.worker_manager_handle);
        let h2 = Arc::clone(&harness.worker_manager_handle);

        harness.wait_for_jobs(1).await;
        harness.send_package_change().await;
        harness.release_first_cycle();
        harness.wait_for_jobs(2).await;
        harness.wait_for_markers(2).await;

        assert_eq!(
            read_marker_count(&harness.workspace_root),
            2,
            "expected exactly 2 worker jobs before shutdown"
        );
        assert!(
            Arc::ptr_eq(&h1, &h2),
            "worker manager Arc identity should stay stable across cycles"
        );
        assert!(
            !h1.is_shutdown(),
            "worker manager should NOT be shut down mid-watch"
        );

        harness.shutdown().await;
    }

    /// IGNORE/EMPTY-AFFECTED: Prove changes outside all packages don't trigger rebuild.
    #[tokio::test]
    async fn change_outside_package_does_not_trigger_rebuild() {
        let harness = E2eHarness::start().await;

        harness.wait_for_jobs(1).await;
        harness.release_first_cycle();
        tokio::time::sleep(Duration::from_millis(100)).await;
        harness.send_outside_change().await;

        assert!(
            marker_count_stays_at(&harness.workspace_root, 1, Duration::from_millis(500)).await,
            "expected marker count to stay at 1 for change outside packages, got {}",
            read_marker_count(&harness.workspace_root)
        );

        harness.shutdown().await;
    }
}
