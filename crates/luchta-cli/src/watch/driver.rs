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
use luchta_workspace::{PackageGraph, WorkspaceDiscovery, YarnWorkspace};
use miette::Result;
use owo_colors::{OwoColorize, Stream};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::lockfile_watch::LockfileWatchState;
use super::registry::dirty_packages_for_changes;
use super::session::WatchSession;
use super::watcher::{WatchBatch, WatcherHandle};
use crate::build_lock;
use crate::cli::OutputMode;
use crate::run::{CycleOutcome, RunCycleParams, TaskSelection};

/// Maximum number of changed file paths to list under `--show-changed-files`
/// before collapsing the remainder into a count.
const MAX_LISTED_CHANGED_FILES: usize = 10;
const RECOVERY_RETRY_MIN: Duration = Duration::from_millis(250);
const RECOVERY_RETRY_MAX: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct RecoveryRetry {
    consecutive_failures: u32,
    retry_at: Option<Instant>,
}

impl RecoveryRetry {
    fn record_failure(&mut self) -> Duration {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let exponent = self.consecutive_failures.saturating_sub(1).min(5);
        let delay = RECOVERY_RETRY_MIN
            .saturating_mul(1_u32 << exponent)
            .min(RECOVERY_RETRY_MAX);
        self.retry_at = Some(Instant::now() + delay);
        delay
    }

    fn remaining_delay(&self) -> Duration {
        self.retry_at
            .map(|retry_at| retry_at.saturating_duration_since(Instant::now()))
            .unwrap_or_default()
    }

    fn reset(&mut self) {
        self.consecutive_failures = 0;
        self.retry_at = None;
    }
}

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
            watch_warning(&format!(
                "workspace discovery failed for '{}': {error}",
                workspace_root.display()
            ));
            StructuralPackageSetDiff::KeepPrevious
        }
    }
}

async fn diff_discovered_package_paths_async(
    watcher_handle: &WatcherHandle,
    workspace_root: &Path,
    current_package_paths: &BTreeSet<PathBuf>,
) -> StructuralPackageSetDiff {
    let workspace_root = workspace_root.to_path_buf();
    let current_package_paths = current_package_paths.clone();
    match watcher_handle
        .run_tracked_blocking(move || {
            diff_discovered_package_paths(&workspace_root, &current_package_paths)
        })
        .await
    {
        Ok(diff) => diff,
        Err(error) => {
            watch_warning(&format!("workspace discovery task failed: {error}"));
            StructuralPackageSetDiff::KeepPrevious
        }
    }
}

async fn rebuild_and_reconcile_watch_state(
    context: &WatchIterationContext<'_>,
    package_paths: &BTreeSet<PathBuf>,
) -> RecoveryOutcome {
    if let Err(error) = context.session.rebuild_for_packages(package_paths).await {
        watch_warning(&format!(
            "structural workspace rebuild failed; keeping previous graph: {error}"
        ));
        return RecoveryOutcome::Retry {
            requires_rescan: false,
        };
    }
    let package_nodes = context.session.current_package_nodes();
    if let Err(error) = context
        .watcher_handle
        .reconcile_watch_roots(context.session.repo_root().as_ref(), &package_nodes)
        .await
    {
        if error.is_terminal_recovery_error() {
            return RecoveryOutcome::Fatal(error.to_string());
        }
        watch_warning(&format!(
            "watch root reconcile failed after rebuild; scheduling another reconciliation: {error}"
        ));
        return RecoveryOutcome::Retry {
            requires_rescan: true,
        };
    }
    RecoveryOutcome::Complete
}

async fn recover_structural_watch_state(context: WatchIterationContext<'_>) -> RecoveryOutcome {
    match diff_discovered_package_paths_async(
        context.watcher_handle,
        context.session.repo_root().as_ref(),
        &context.session.current_package_paths(),
    )
    .await
    {
        StructuralPackageSetDiff::KeepPrevious => RecoveryOutcome::Retry {
            requires_rescan: false,
        },
        StructuralPackageSetDiff::Unchanged => {
            rebuild_and_reconcile_watch_state(&context, &context.session.current_package_paths())
                .await
        }
        StructuralPackageSetDiff::Changed(discovered_package_paths) => {
            rebuild_and_reconcile_watch_state(&context, &discovered_package_paths).await
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
    rescan: bool,
    watcher_failure: Option<String>,
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
        pending.paths.extend(batch);
        was_empty && !pending.is_empty()
    }

    pub fn mark_structural(&self) -> bool {
        let mut pending = self.inner.lock().expect("pending changes mutex poisoned");
        let was_empty = pending.is_empty();
        pending.structural = true;
        was_empty
    }

    pub fn take_structural(&self) -> bool {
        let mut pending = self.inner.lock().expect("pending changes mutex poisoned");
        std::mem::take(&mut pending.structural)
    }

    pub fn mark_rescan(&self) -> bool {
        let mut pending = self.inner.lock().expect("pending changes mutex poisoned");
        let was_empty = pending.is_empty();
        pending.rescan = true;
        was_empty
    }

    pub fn take_rescan(&self) -> bool {
        let mut pending = self.inner.lock().expect("pending changes mutex poisoned");
        std::mem::take(&mut pending.rescan)
    }

    fn mark_watcher_failed(&self, message: String) {
        let mut pending = self.inner.lock().expect("pending changes mutex poisoned");
        pending.watcher_failure.get_or_insert(message);
    }

    fn take_watcher_failure(&self) -> Option<String> {
        let mut pending = self.inner.lock().expect("pending changes mutex poisoned");
        pending.watcher_failure.take()
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
        pending.is_empty()
    }

    fn has_changes(&self) -> bool {
        !self.is_empty()
    }
}

impl PendingState {
    fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.structural && !self.rescan && self.watcher_failure.is_none()
    }
}

pub struct WatchRunConfig {
    pub output: OutputMode,
    pub continue_on_failure: bool,
    pub no_cache: bool,
    pub memory_pressure_enabled: bool,
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
    let workspace_root = session.repo_root();
    let package_nodes = session.current_package_nodes();
    let package_graph = session.current_package_graph();
    let mut initial_lockfile_state = LockfileWatchState::new(workspace_root.as_ref());
    initial_lockfile_state.rebuild_baseline(
        &package_nodes,
        Some(package_graph.as_ref()),
        workspace_root.as_ref(),
    );
    let lockfile_state = Arc::new(Mutex::new(initial_lockfile_state));
    let drain_task = spawn_change_drain_task(
        changes_rx,
        Arc::clone(&pending),
        Arc::clone(&wake),
        Arc::clone(&active_cycle),
    );
    let ui = WatchUi::new(config.show_changed_files);
    let mut signals = WatchSignals::new(shutdown, force_shutdown);
    let context = WatchIterationContext {
        session: &session,
        watcher_handle: &watcher_handle,
        selection: &selection,
        config: &config,
        pending: &pending,
        wake: &wake,
        active_cycle: &active_cycle,
        lockfile_state: &lockfile_state,
        ui: &ui,
    };

    ui.started();
    let result = drive_watch_loop(context, &mut signals).await;

    finish_shutdown(session, watcher_handle, drain_task).await;
    result
}

async fn drive_watch_loop<F, G>(
    context: WatchIterationContext<'_>,
    signals: &mut WatchSignals<F, G>,
) -> Result<()>
where
    F: Future<Output = std::result::Result<(), std::io::Error>> + Send,
    G: Future<Output = std::result::Result<(), std::io::Error>> + Send,
{
    if should_stop(run_initial_watch_cycle(context, signals).await?) {
        return Ok(());
    }

    let mut recovery_retry = RecoveryRetry::default();
    loop {
        if should_stop(run_one_iteration(context, &mut recovery_retry, signals).await?) {
            return Ok(());
        }
    }
}

fn should_stop(control: WatchControl) -> bool {
    matches!(control, WatchControl::Stop)
}

fn begin_cycle_if_caught_up(
    active_cycle: &ActiveCycle,
    pending: &PendingChanges,
) -> Option<CancellationToken> {
    let cancel = CancellationToken::new();
    active_cycle.set(cancel.clone());
    if pending.has_changes() {
        active_cycle.clear();
        None
    } else {
        Some(cancel)
    }
}

fn requeue_processed_changes(
    pending: &PendingChanges,
    changed: &HashSet<PathBuf>,
    structural: bool,
    rescan: bool,
) {
    pending.add(changed.clone());
    if structural {
        pending.mark_structural();
    }
    if rescan {
        pending.mark_rescan();
    }
}

async fn run_initial_watch_cycle<F, G>(
    context: WatchIterationContext<'_>,
    signals: &mut WatchSignals<F, G>,
) -> Result<WatchControl>
where
    F: Future<Output = std::result::Result<(), std::io::Error>> + Send,
    G: Future<Output = std::result::Result<(), std::io::Error>> + Send,
{
    let cache_dir = resolve_cache_dir(context.session.repo_root().as_ref());
    let Some(lock) =
        wait_for_watch_operation(context, signals, build_lock::acquire(&cache_dir)).await?
    else {
        return Ok(WatchControl::Stop);
    };
    let Some(_build_lock) = lock? else {
        return Ok(WatchControl::Stop);
    };
    let initial_selection = context.selection.as_task_selection();
    let Some(cancel) = begin_cycle_if_caught_up(context.active_cycle, context.pending) else {
        // The initial full selection has not run yet, so retry it as a full rescan.
        context.pending.mark_rescan();
        return Ok(WatchControl::Continue);
    };
    run_cycle_with_status(
        context.session,
        cycle_request(&initial_selection, None, context.config),
        context.active_cycle,
        cancel,
        context.ui,
        signals,
    )
    .await
}

async fn wait_for_watch_operation<F, G, O, T>(
    context: WatchIterationContext<'_>,
    signals: &mut WatchSignals<F, G>,
    operation: O,
) -> Result<Option<T>>
where
    F: Future<Output = std::result::Result<(), std::io::Error>> + Send,
    G: Future<Output = std::result::Result<(), std::io::Error>> + Send,
    O: Future<Output = T>,
{
    check_watcher_failure(context.pending)?;
    tokio::pin!(operation);
    loop {
        tokio::select! {
            output = &mut operation => {
                check_watcher_failure(context.pending)?;
                return Ok(Some(output));
            }
            _ = context.wake.notified() => {
                check_watcher_failure(context.pending)?;
            }
            _ = &mut signals.shutdown => {
                context.ui.shutting_down();
                shutdown_watch(context.session, context.ui, signals).await;
                return Ok(None);
            }
        }
    }
}

fn check_watcher_failure(pending: &PendingChanges) -> Result<()> {
    if let Some(message) = pending.take_watcher_failure() {
        return Err(miette::miette!("filesystem watcher stopped: {message}"));
    }
    Ok(())
}

async fn run_one_iteration<F, G>(
    context: WatchIterationContext<'_>,
    recovery_retry: &mut RecoveryRetry,
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

    check_watcher_failure(context.pending)?;

    let rescan_pending = context.pending.take_rescan();
    let structural_pending = context.pending.take_structural();
    if structural_pending || rescan_pending {
        let retry_delay = recovery_retry.remaining_delay();
        if !retry_delay.is_zero()
            && wait_for_watch_operation(context, signals, tokio::time::sleep(retry_delay))
                .await?
                .is_none()
        {
            return Ok(WatchControl::Stop);
        }
    }
    if structural_pending || rescan_pending {
        let Some(recovery) =
            wait_for_watch_operation(context, signals, recover_structural_watch_state(context))
                .await?
        else {
            return Ok(WatchControl::Stop);
        };
        match recovery {
            RecoveryOutcome::Complete => recovery_retry.reset(),
            RecoveryOutcome::Retry { requires_rescan } => {
                // Reconciliation may have partially changed backend coverage. Retry the
                // structure update and force a full scan once coverage is restored.
                context.pending.mark_structural();
                if rescan_pending || requires_rescan {
                    context.pending.mark_rescan();
                }
                let delay = recovery_retry.record_failure();
                watch_warning(&format!(
                    "workspace watch recovery will retry in {} ms",
                    delay.as_millis()
                ));
                return Ok(WatchControl::Continue);
            }
            RecoveryOutcome::Fatal(message) => {
                return Err(miette::miette!(
                    "filesystem watcher recovery stopped: {message}"
                ));
            }
        }
    }

    let changed = match context.pending.drain_non_empty() {
        Some(changed) => changed,
        None if structural_pending || rescan_pending => context
            .session
            .current_package_paths()
            .into_iter()
            .collect::<HashSet<_>>(),
        None => return Ok(WatchControl::Continue),
    };

    if rescan_pending {
        watch_warning(
            "the filesystem backend reported dropped events; rescanning by running the full selection",
        );
        let cycle_selection = context.selection.as_task_selection();
        let cache_dir = resolve_cache_dir(context.session.repo_root().as_ref());
        let Some(lock) =
            wait_for_watch_operation(context, signals, build_lock::acquire(&cache_dir)).await?
        else {
            return Ok(WatchControl::Stop);
        };
        let Some(_build_lock) = lock? else {
            return Ok(WatchControl::Stop);
        };
        let Some(cancel) = begin_cycle_if_caught_up(context.active_cycle, context.pending) else {
            requeue_processed_changes(
                context.pending,
                &changed,
                structural_pending,
                rescan_pending,
            );
            return Ok(WatchControl::Continue);
        };
        return run_cycle_with_status(
            context.session,
            cycle_request(&cycle_selection, None, context.config),
            context.active_cycle,
            cancel,
            context.ui,
            signals,
        )
        .await;
    }
    // Only real changes to a task's declared inputs (verified by size/mtime, then
    // content hash) — or new files matching a task's input globs — dirty a package.
    // Cache outputs, restore staging dirs, and touch-only events are ignored, which
    // is what breaks the watch rebuild loop (#161).
    let mut affected = dirty_packages_for_changes(&context.session.task_watch_registry(), &changed);
    let workspace_root = context.session.repo_root();
    let lockfile_path = {
        let state = context
            .lockfile_state
            .lock()
            .expect("lockfile watch state mutex poisoned");
        state.lockfile_path().to_path_buf()
    };
    let lockfile_changed = changed.iter().any(|path| path == &lockfile_path);
    if lockfile_changed {
        let packages = context.session.current_package_nodes();
        let package_graph = context.session.current_package_graph();
        let mut lockfile_state = context
            .lockfile_state
            .lock()
            .expect("lockfile watch state mutex poisoned");
        affected.extend(lockfile_state.affected_packages(
            &packages,
            Some(package_graph.as_ref()),
            workspace_root.as_ref(),
        ));
        lockfile_state.rebuild_baseline(
            &packages,
            Some(package_graph.as_ref()),
            workspace_root.as_ref(),
        );
    }
    if affected.is_empty() {
        context.ui.up_to_date();
        return Ok(WatchControl::Continue);
    }
    affected =
        expand_affected_with_dependents(context.session.current_package_graph().as_ref(), affected);

    context
        .ui
        .change_detected(&affected, &changed, context.session.repo_root().as_ref());
    let cycle_selection = context.selection.as_task_selection();
    let cache_dir = resolve_cache_dir(context.session.repo_root().as_ref());
    let Some(lock) =
        wait_for_watch_operation(context, signals, build_lock::acquire(&cache_dir)).await?
    else {
        return Ok(WatchControl::Stop);
    };
    let Some(_build_lock) = lock? else {
        return Ok(WatchControl::Stop);
    };
    let Some(cancel) = begin_cycle_if_caught_up(context.active_cycle, context.pending) else {
        requeue_processed_changes(
            context.pending,
            &changed,
            structural_pending,
            rescan_pending,
        );
        return Ok(WatchControl::Continue);
    };
    run_cycle_with_status(
        context.session,
        cycle_request(&cycle_selection, Some(&affected), context.config),
        context.active_cycle,
        cancel,
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

fn expand_affected_with_dependents(
    package_graph: &PackageGraph,
    affected: HashSet<PackageName>,
) -> HashSet<PackageName> {
    match package_graph.transitive_dependents_of(affected.iter().cloned()) {
        Ok(expanded) => expanded,
        Err(error) => {
            watch_warning(&format!(
                "failed to expand affected packages with dependents: {error}"
            ));
            affected
        }
    }
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
        no_cache: config.no_cache,
        memory_pressure_enabled: config.memory_pressure_enabled,
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
            apply_watch_batch(batch, &pending, &wake, &active_cycle);
        }
        pending.mark_watcher_failed("event channel closed unexpectedly".to_string());
        active_cycle.cancel_if_active();
        wake.notify_one();
    })
}

fn apply_watch_batch(
    mut batch: WatchBatch,
    pending: &PendingChanges,
    wake: &Notify,
    active_cycle: &ActiveCycle,
) {
    for warning in batch.warnings.drain(..) {
        watch_warning(&warning);
    }
    if let Some(failure) = batch.failure.take() {
        pending.mark_watcher_failed(failure);
        active_cycle.cancel_if_active();
        wake.notify_one();
        return;
    }
    if batch.structural {
        pending.mark_structural();
    }
    if batch.rescan {
        pending.mark_rescan();
    }
    let paths_pending = pending.add(batch.changed_paths);
    let should_wake = batch.structural || batch.rescan || paths_pending;
    if should_wake {
        // Cancel the active cycle directly. This ensures the change is NOT lost
        // even if Notify permit semantics would have dropped it. Notify remains
        // only a wake hint; pending state is the source of truth.
        active_cycle.cancel_if_active();
        wake.notify_one();
    }
}

enum WatchControl {
    Continue,
    Stop,
}

enum RecoveryOutcome {
    Complete,
    Retry { requires_rescan: bool },
    Fatal(String),
}

#[derive(Clone, Copy)]
struct WatchIterationContext<'a> {
    session: &'a WatchSession,
    watcher_handle: &'a WatcherHandle,
    selection: &'a OwnedSelection,
    config: &'a WatchRunConfig,
    pending: &'a PendingChanges,
    wake: &'a Notify,
    active_cycle: &'a ActiveCycle,
    lockfile_state: &'a Arc<Mutex<LockfileWatchState>>,
    ui: &'a WatchUi,
}

struct CycleRequest<'a> {
    selection: &'a TaskSelection<'a>,
    affected: Option<&'a HashSet<PackageName>>,
    output: OutputMode,
    continue_on_failure: bool,
    no_cache: bool,
    memory_pressure_enabled: bool,
}

async fn run_cycle_with_status<F, G>(
    session: &WatchSession,
    request: CycleRequest<'_>,
    active_cycle: &ActiveCycle,
    cancel: CancellationToken,
    ui: &WatchUi,
    signals: &mut WatchSignals<F, G>,
) -> Result<WatchControl>
where
    F: Future<Output = std::result::Result<(), std::io::Error>> + Send,
    G: Future<Output = std::result::Result<(), std::io::Error>> + Send,
{
    ui.cycle_started()?;

    let cycle = session.run_cycle(
        RunCycleParams {
            selection: request.selection,
            since_affected: request.affected,
            output: request.output,
            continue_on_failure: request.continue_on_failure,
            no_cache: request.no_cache,
            memory_pressure_enabled: request.memory_pressure_enabled,
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
    drain_task.abort();
    let _ = drain_task.await;
    watcher_handle.shutdown().await;
    session.shutdown().await;
}

fn print_status(line: &str) {
    println!("{line}");
}

fn watch_warning(message: &str) {
    eprintln!("[watch] warning: {message}");
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
    use std::collections::BTreeSet;

    use tempfile::tempdir;

    #[test]
    fn recovery_retry_uses_capped_exponential_backoff_and_resets() {
        let mut retry = RecoveryRetry::default();

        let delays = (0..7).map(|_| retry.record_failure()).collect::<Vec<_>>();
        assert_eq!(
            delays,
            vec![
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                RECOVERY_RETRY_MAX,
                RECOVERY_RETRY_MAX,
            ]
        );

        retry.reset();
        assert_eq!(retry.consecutive_failures, 0);
        assert_eq!(retry.retry_at, None);
        assert_eq!(retry.record_failure(), RECOVERY_RETRY_MIN);
    }

    #[test]
    fn cycle_handoff_registers_cancellation_before_checking_pending_state() {
        let pending = PendingChanges::new();
        let active_cycle = ActiveCycle::new();

        let cancel = begin_cycle_if_caught_up(&active_cycle, &pending)
            .expect("caught-up watcher can begin cycle");
        assert_eq!(
            (active_cycle.cancel_if_active(), cancel.is_cancelled()),
            (true, true)
        );

        pending.mark_rescan();
        assert_eq!(
            (
                begin_cycle_if_caught_up(&active_cycle, &pending).is_none(),
                active_cycle.is_active()
            ),
            (true, false)
        );
    }

    #[test]
    fn deferred_cycle_requeues_every_consumed_signal() {
        let pending = PendingChanges::new();
        let changed = HashSet::from([PathBuf::from("/repo/pkg/src/lib.rs")]);

        requeue_processed_changes(&pending, &changed, true, true);

        assert_eq!(pending.drain_non_empty(), Some(changed));
        assert_eq!(
            (pending.take_structural(), pending.take_rescan()),
            (true, true)
        );
    }

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
    fn rescan_pending_coalesces_and_is_independent_of_paths() {
        let pending = PendingChanges::new();

        assert_eq!(
            (
                pending.mark_rescan(),
                pending.mark_rescan(),
                pending.has_changes()
            ),
            (true, false, true),
            "only the first rescan should wake the loop"
        );
        assert_eq!(
            (
                pending.take_rescan(),
                pending.take_rescan(),
                pending.is_empty()
            ),
            (true, false, true),
            "taking the rescan should clear the coalesced latch"
        );
    }

    #[tokio::test]
    async fn closed_watcher_channel_wakes_loop_with_failure() {
        let (changes_tx, changes_rx) = mpsc::channel(1);
        let pending = Arc::new(PendingChanges::new());
        let wake = Arc::new(Notify::new());
        let active_cycle = Arc::new(ActiveCycle::new());
        let drain_task = spawn_change_drain_task(
            changes_rx,
            Arc::clone(&pending),
            Arc::clone(&wake),
            active_cycle,
        );

        drop(changes_tx);
        wake.notified().await;

        assert_eq!(
            pending.take_watcher_failure().as_deref(),
            Some("event channel closed unexpectedly")
        );
        drain_task.await.expect("drain task exits cleanly");
    }

    #[tokio::test]
    async fn backend_failure_survives_the_following_channel_close() {
        let (changes_tx, changes_rx) = mpsc::channel(1);
        let pending = Arc::new(PendingChanges::new());
        let wake = Arc::new(Notify::new());
        let active_cycle = Arc::new(ActiveCycle::new());
        let drain_task = spawn_change_drain_task(
            changes_rx,
            Arc::clone(&pending),
            Arc::clone(&wake),
            active_cycle,
        );

        changes_tx
            .send(WatchBatch {
                failure: Some("backend invalidated its watch".to_string()),
                ..WatchBatch::default()
            })
            .await
            .expect("send failure batch");
        drop(changes_tx);
        drain_task.await.expect("drain task exits cleanly");

        assert_eq!(
            pending.take_watcher_failure().as_deref(),
            Some("backend invalidated its watch")
        );
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
    fn expand_affected_with_dependents_includes_downstream_packages() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let repo_root = temp_dir.path();
        let api_dir = repo_root.join("packages/api");
        let app_dir = repo_root.join("packages/app");
        std::fs::create_dir_all(&api_dir).expect("create api dir");
        std::fs::create_dir_all(&app_dir).expect("create app dir");
        std::fs::write(
            api_dir.join("package.json"),
            r#"{"name":"api","version":"1.0.0"}"#,
        )
        .expect("write api package.json");
        std::fs::write(
            app_dir.join("package.json"),
            r#"{"name":"app","version":"1.0.0","dependencies":{"api":"1.0.0"}}"#,
        )
        .expect("write app package.json");
        let packages = vec![
            luchta_workspace::PackageNode::new(PackageName::new("api".to_owned()), &api_dir),
            luchta_workspace::PackageNode::new(PackageName::new("app".to_owned()), &app_dir),
        ];
        let package_graph = PackageGraph::build(packages).expect("build package graph");

        let expanded = expand_affected_with_dependents(
            &package_graph,
            HashSet::from([PackageName::new("api".to_owned())]),
        );

        assert_eq!(
            expanded,
            HashSet::from([
                PackageName::new("api".to_owned()),
                PackageName::new("app".to_owned()),
            ])
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

#[cfg(test)]
#[path = "driver_rescan_e2e_tests.rs"]
mod driver_rescan_e2e_tests;
