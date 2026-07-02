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

use std::collections::{BTreeSet, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use luchta_cache::resolve_cache_dir;
use luchta_types::PackageName;
use luchta_workspace::{WorkspaceDiscovery, YarnWorkspace};
use miette::Result;
use owo_colors::{OwoColorize, Stream};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::registry::dirty_packages_for_changes;
use super::session::WatchSession;
use super::watcher::{WatchBatch, WatcherHandle};
use crate::build_lock;
use crate::cli::OutputMode;
use crate::run::{CycleOutcome, MemoryPressureConfig, RunCycleParams, TaskSelection};

/// Maximum number of changed file paths to list under `--show-changed-files`
/// before collapsing the remainder into a count.
const MAX_LISTED_CHANGED_FILES: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StructuralPackageSetDiff {
    Changed(BTreeSet<PathBuf>),
    Unchanged,
    KeepPrevious,
}

#[allow(dead_code)]
pub(crate) fn diff_discovered_package_paths(
    workspace_root: &Path,
    current_package_paths: &BTreeSet<PathBuf>,
) -> StructuralPackageSetDiff {
    let workspace = YarnWorkspace::new(workspace_root);
    match workspace.discover() {
        Ok(packages) => {
            let discovered_package_paths = packages
                .into_iter()
                .map(|package| package.path)
                .collect::<BTreeSet<_>>();
            if discovered_package_paths == *current_package_paths {
                StructuralPackageSetDiff::Unchanged
            } else {
                StructuralPackageSetDiff::Changed(discovered_package_paths)
            }
        }
        Err(error) => {
            warn!(
                error = %error,
                workspace_root = %workspace_root.display(),
                "workspace discovery failed"
            );
            StructuralPackageSetDiff::KeepPrevious
        }
    }
}

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
    inner: Mutex<PendingState>,
}

#[derive(Debug, Default)]
struct PendingState {
    paths: HashSet<PathBuf>,
    structural: bool,
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
        let was_empty = pending.paths.is_empty() && !pending.structural;
        pending.paths.extend(batch);
        was_empty && (!pending.paths.is_empty() || pending.structural)
    }

    pub fn mark_structural(&self) -> bool {
        let mut pending = self.inner.lock().expect("pending changes mutex poisoned");
        let was_empty = pending.paths.is_empty() && !pending.structural;
        pending.structural = true;
        was_empty
    }

    pub fn take_structural(&self) -> bool {
        let mut pending = self.inner.lock().expect("pending changes mutex poisoned");
        std::mem::take(&mut pending.structural)
    }

    /// Drain and return whether set was non-empty.
    pub fn drain_non_empty(&self) -> Option<HashSet<PathBuf>> {
        let mut pending = self.inner.lock().expect("pending changes mutex poisoned");
        let drained = std::mem::take(&mut pending.paths);
        if drained.is_empty() {
            None
        } else {
            Some(drained)
        }
    }

    pub fn is_empty(&self) -> bool {
        let pending = self.inner.lock().expect("pending changes mutex poisoned");
        pending.paths.is_empty() && !pending.structural
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
    pub session: Arc<WatchSession>,
    pub watcher_handle: WatcherHandle,
    pub changes_rx: mpsc::Receiver<WatchBatch>,
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
        print_status(&format_cycle_start_line());
        Ok(())
    }

    fn cycle_finished(&self, outcome: CycleOutcome, elapsed: Duration) {
        print_status(&format_cycle_finished_line(outcome, elapsed));
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
                        watcher_handle: &watcher_handle,
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
    let cache_dir = resolve_cache_dir(session.repo_root().as_ref());
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

    let structural_pending = context.pending.take_structural();
    if structural_pending {
        match diff_discovered_package_paths(
            context.session.repo_root().as_ref(),
            &context.session.current_package_paths(),
        ) {
            StructuralPackageSetDiff::KeepPrevious => return Ok(WatchControl::Continue),
            StructuralPackageSetDiff::Unchanged => {}
            StructuralPackageSetDiff::Changed(discovered_package_paths) => {
                if let Err(error) = context
                    .session
                    .rebuild_for_packages(&discovered_package_paths)
                    .await
                {
                    warn!(error = %error, "structural workspace rebuild failed; keeping previous graph");
                    return Ok(WatchControl::Continue);
                }
                let package_nodes = context.session.current_package_nodes();
                if let Err(error) = context
                    .watcher_handle
                    .reconcile_watch_roots(context.session.repo_root().as_ref(), &package_nodes)
                {
                    warn!(error = %error, "watch root reconcile failed after rebuild");
                    return Ok(WatchControl::Continue);
                }
            }
        }
    }

    let changed = match context.pending.drain_non_empty() {
        Some(changed) => changed,
        None if structural_pending => context
            .session
            .current_package_paths()
            .into_iter()
            .collect::<HashSet<_>>(),
        None => return Ok(WatchControl::Continue),
    };
    // Only real changes to a task's declared inputs (verified by size/mtime, then
    // content hash) — or new files matching a task's input globs — dirty a package.
    // Cache outputs, restore staging dirs, and touch-only events are ignored, which
    // is what breaks the watch rebuild loop (#161).
    let affected = dirty_packages_for_changes(&context.session.task_watch_registry(), &changed);
    if affected.is_empty() {
        context.ui.up_to_date();
        return Ok(WatchControl::Continue);
    }

    context
        .ui
        .change_detected(&affected, &changed, context.session.repo_root().as_ref());
    let cycle_selection = context.selection.as_task_selection();
    let cache_dir = resolve_cache_dir(context.session.repo_root().as_ref());
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
    mut changes_rx: mpsc::Receiver<WatchBatch>,
    pending: Arc<PendingChanges>,
    wake: Arc<Notify>,
    active_cycle: Arc<ActiveCycle>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(batch) = changes_rx.recv().await {
            let structural_pending = batch.structural && pending.mark_structural();
            let paths_pending = pending.add(batch.changed_paths);
            if batch.structural {
                active_cycle.cancel_if_active();
                wake.notify_one();
            } else if paths_pending {
                // Cancel the active cycle directly. This ensures the change is NOT lost
                // even if Notify permit semantics would have dropped it.
                // Only fire cancellation if there's an active cycle.
                active_cycle.cancel_if_active();
                // Wake hint — may or may not be consumed; pending.is_empty() is the source of truth.
                wake.notify_one();
            }
            if structural_pending {
                continue;
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
    watcher_handle: &'a WatcherHandle,
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
    session: Arc<WatchSession>,
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
    let mut names = affected.iter().map(ToString::to_string).collect::<Vec<_>>();
    names.sort();
    format!("[watch] change detected: {}", names.join(", "))
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
        .map(|path| format!("[watch]   {path}"))
        .collect::<Vec<_>>();

    if total > MAX_LISTED_CHANGED_FILES {
        let remaining = total - MAX_LISTED_CHANGED_FILES;
        lines.push(format!("[watch]   … and {remaining} more"));
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

fn format_cycle_start_line() -> String {
    "[watch] rebuilding…".to_string()
}

fn format_cycle_finished_line(outcome: CycleOutcome, elapsed: Duration) -> String {
    match outcome {
        CycleOutcome::Success => format!("[watch] done in {}", format_elapsed(elapsed)),
        CycleOutcome::Failed => format!("[watch] build failed in {}", format_elapsed(elapsed)),
        CycleOutcome::Cancelled => "[watch] cancelled (new changes)".to_string(),
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
    use std::collections::BTreeSet;

    use tempfile::tempdir;

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
    fn structural_pending_coalesces_until_taken() {
        let pending = PendingChanges::new();

        assert!(
            pending.mark_structural(),
            "first structural signal should wake loop"
        );
        assert!(
            !pending.mark_structural(),
            "second structural signal should coalesce into same pending rebuild"
        );
        assert!(pending.has_changes());
        assert!(
            pending.take_structural(),
            "pending structural signal should be visible once"
        );
        assert!(
            !pending.take_structural(),
            "latch should clear after consume so next rebuild can re-arm"
        );
        assert!(
            pending.is_empty(),
            "no paths and no structural flag after consume"
        );
    }

    #[test]
    fn structural_pending_keeps_non_structural_paths_pending() {
        let pending = PendingChanges::new();
        pending.add(HashSet::from([PathBuf::from("/repo/pkg-a/src/lib.rs")]));
        pending.mark_structural();

        assert!(pending.take_structural(), "structural latch should be set");
        assert!(
            !pending.is_empty(),
            "draining structural latch alone must not drop ordinary file changes"
        );
        assert_eq!(
            pending.drain_non_empty(),
            Some(HashSet::from([PathBuf::from("/repo/pkg-a/src/lib.rs")]))
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
                "[watch]   pkg-a/src/main.rs".to_string(),
                "[watch]   pkg-b/src/lib.rs".to_string(),
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
        assert_eq!(lines[0], "[watch]   pkg/src/file00.rs");
        assert_eq!(
            lines.last().expect("summary line present"),
            "[watch]   … and 5 more"
        );
    }

    #[test]
    fn diff_discovered_package_paths_reports_changed_and_unchanged() {
        let temp_dir = tempdir().expect("create temp dir");
        let workspace_root = temp_dir.path();
        write_workspace_package_json(workspace_root, &["packages/*"]);
        write_package_json(&workspace_root.join("packages/app/package.json"), "app");

        let current_package_paths = BTreeSet::from([
            workspace_root.to_path_buf(),
            workspace_root.join("packages/app"),
        ]);
        assert_eq!(
            diff_discovered_package_paths(workspace_root, &current_package_paths),
            StructuralPackageSetDiff::Unchanged
        );

        write_package_json(&workspace_root.join("packages/web/package.json"), "web");
        let expected_discovered_paths = BTreeSet::from([
            workspace_root.to_path_buf(),
            workspace_root.join("packages/app"),
            workspace_root.join("packages/web"),
        ]);
        assert_eq!(
            diff_discovered_package_paths(workspace_root, &current_package_paths),
            StructuralPackageSetDiff::Changed(expected_discovered_paths)
        );
    }

    #[test]
    fn diff_discovered_package_paths_keeps_previous_on_discovery_error() {
        let temp_dir = tempdir().expect("create temp dir");
        let workspace_root = temp_dir.path();
        write_workspace_package_json(workspace_root, &["packages/*"]);
        std::fs::create_dir_all(workspace_root.join("packages/app")).expect("create package dir");
        std::fs::write(
            workspace_root.join("packages/app/package.json"),
            "{ invalid json",
        )
        .expect("write malformed package.json");

        let current_package_paths = BTreeSet::from([workspace_root.to_path_buf()]);
        assert_eq!(
            diff_discovered_package_paths(workspace_root, &current_package_paths),
            StructuralPackageSetDiff::KeepPrevious
        );
    }

    fn write_workspace_package_json(workspace_root: &Path, workspaces: &[&str]) {
        let workspaces = workspaces
            .iter()
            .map(|pattern| format!("\"{pattern}\""))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            workspace_root.join("package.json"),
            format!(
                "{{\n  \"name\": \"root\",\n  \"private\": true,\n  \"workspaces\": [{workspaces}]\n}}\n"
            ),
        )
        .expect("write root package.json");
    }

    fn write_package_json(path: &Path, name: &str) {
        std::fs::create_dir_all(path.parent().expect("package parent"))
            .expect("create package dir");
        std::fs::write(path, format!("{{\n  \"name\": \"{name}\"\n}}\n"))
            .expect("write package.json");
    }

    #[test]
    fn changed_files_path_outside_repo_falls_back_to_full_path() {
        let repo_root = PathBuf::from("/repo");
        let changed = HashSet::from([PathBuf::from("/elsewhere/file.rs")]);

        let lines = format_changed_files_lines(&changed, &repo_root);
        assert_eq!(lines, vec!["[watch]   /elsewhere/file.rs".to_string()]);
    }

    #[test]
    fn status_lines_format_expected_messages() {
        let affected = HashSet::from([
            PackageName::new("pkg-b".to_owned()),
            PackageName::new("pkg-a".to_owned()),
        ]);

        assert!(format_watch_started_line().contains("[watch] watch mode started"));
        assert_eq!(
            format_change_detected_line(&affected),
            "[watch] change detected: pkg-a, pkg-b"
        );
        assert_eq!(format_up_to_date_line(), "[watch] up to date");
        assert_eq!(format_cycle_start_line(), "[watch] rebuilding…");
        assert_eq!(
            format_cycle_finished_line(CycleOutcome::Success, Duration::from_millis(125)),
            "[watch] done in 125ms"
        );
        assert_eq!(
            format_cycle_finished_line(CycleOutcome::Failed, Duration::from_millis(1234)),
            "[watch] build failed in 1.234s"
        );
        assert_eq!(
            format_cycle_finished_line(CycleOutcome::Cancelled, Duration::from_secs(5)),
            "[watch] cancelled (new changes)"
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
#[path = "driver_e2e_support.rs"]
mod driver_e2e_support;
#[cfg(test)]
#[path = "driver_e2e_tests.rs"]
mod driver_e2e_tests;
