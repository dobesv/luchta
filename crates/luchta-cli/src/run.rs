//! Run command implementation.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use luchta_cache::shared::SharedCache;
use luchta_cache::{
    combined_outputs_hash, decide, resolve_cache_dir, resolve_inputs_with_semantics,
    resolve_outputs, Cache, CurrentState, Decision, DecisionResult, ListingCache, RunReason,
    TaskRunRecord,
};
use luchta_engine::{
    expand_input_patterns, is_root_task, CompletionSignal, ExecutionLogSink, ExecutionRequest,
    LogStream, PackageResolveInfo, PruneOutcome, PrunedTask, ReadyTaskMessage, ResolveMode,
    TaskGraph, TaskNode, TaskRunOutcome, Walker, WeightedExecutor, WorkerManager,
};
use luchta_types::{EnvSpec, PackageName, TaskDefinition, TaskId, TaskName, WorkerDefinition};
use luchta_workspace::{PackageGraph, PackageNode, WorkspaceDiscovery, YarnWorkspace};
use miette::{bail, Context, IntoDiagnostic, Result};
use owo_colors::{OwoColorize, Stream};

use crate::build_lock;
use crate::cache_ctx::{
    build_current_state, load_lockfile_state, LockfileState, PackageDirResolver,
};
use crate::cli::OutputMode;
use crate::progress::ProgressReporter;
use crate::watch::registry::TaskWatchRegistry;
use tokio_util::sync::CancellationToken;

mod dispatch;
mod pause;
use dispatch::{build_command_map, CommandMap};
use pause::dispatch_loop;

mod setup;
pub use setup::MemoryPressureConfig;
use setup::{
    build_execution_resources, build_memory_pressure, report_run_outcome, BuildResourcesInputs,
    ExecutionResources,
};

// Re-exported so `run/pause.rs` can reach it via `super::dispatch_ready_task`.
use dispatch::dispatch_ready_task;

/// User's task selection from CLI arguments.
///
/// Encapsulates the requested tasks, package filters, and top-level flag into a
/// single struct to reduce function argument counts and improve API ergonomics.
#[derive(Debug)]
pub(crate) struct TaskSelection<'a> {
    pub requested_tasks: &'a [String],
    pub packages: &'a [String],
    pub top_level: bool,
    pub since: Option<&'a str>,
}

/// Internal criteria for matching task nodes.
///
/// Pre-built from `TaskSelection` to avoid repeatedly passing the same arguments
/// to `collect_matching_task_ids` and `package_matches`.
pub(crate) struct SelectionCriteria<'a> {
    task_globs: &'a GlobSet,
    package_globs: &'a GlobSet,
    match_all_non_root_packages: bool,
    top_level: bool,
    since_affected: Option<&'a HashSet<PackageName>>,
}

#[derive(Debug)]
pub struct PreparedWorkspace {
    pub packages: Vec<PackageNode>,
    pub package_graph: PackageGraph,
    pub pipeline: HashMap<TaskName, TaskDefinition>,
    pub env: BTreeMap<String, EnvSpec>,
    pub task_graph: TaskGraph,
    pub workers: HashMap<String, WorkerDefinition>,
    pub max_weight: u32,
    /// Tasks excluded from the graph during the worker-mediated resolution
    /// phase (with their reasons), for CLI reporting.
    pub pruned: Vec<PrunedTask>,
    /// The set of pruned task ids, for validation tolerance in `check`.
    pub pruned_ids: HashSet<TaskId>,
    /// The resident worker manager used for resolution; reused for execution so
    /// resolve and run share the same worker processes.
    pub worker_manager: Arc<WorkerManager>,
    /// Global cache nonce from LuchtaConfig.cache.
    pub global_cache_nonce: Option<String>,
}

enum SinceSelection {
    /// No affected packages — caller should no-op with exit 0.
    NoOp,
    /// Continue with optional affected-package filter (`None` when `--since` absent).
    Proceed(Option<HashSet<PackageName>>),
}

pub fn resolve_workspace_root(workspace_root: Option<PathBuf>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().into_diagnostic()?;
    Ok(workspace_root.unwrap_or(cwd))
}

/// Discovers the workspace, loads config, and builds the task graph after
/// running the worker-mediated resolution phase. `mode` controls how a worker
/// `Reject` is treated (run: warn+prune; check: error). The worker manager
/// created here is returned so callers reuse the same resident worker processes
/// for execution.
pub async fn prepare_workspace(
    workspace_root: &Path,
    mode: ResolveMode,
    max_weight_override: Option<u32>,
) -> Result<PreparedWorkspace> {
    let workspace = YarnWorkspace::new(workspace_root);
    let packages = workspace
        .discover()
        .map_err(|error| miette::miette!("workspace discovery failed: {}", error))?;
    let root_package = packages
        .iter()
        .find(|package| package.path == workspace_root)
        .map(|package| package.name.clone());
    let package_graph = PackageGraph::build(packages.clone())
        .map_err(|error| miette::miette!("failed to build package graph: {}", error))?;
    let package_graph = if let Some(root_package) = root_package {
        package_graph.with_root_package(root_package)
    } else {
        package_graph
    };

    let config = crate::config::load_config(workspace_root)
        .await
        .wrap_err_with(|| format!("Failed to load config at {}", workspace_root.display()))?;
    let pipeline = config
        .tasks
        .into_iter()
        .map(|(name, definition)| (TaskName::from(name), definition))
        .collect::<HashMap<_, _>>();

    let worker_manager = Arc::new(WorkerManager::new(config.workers.clone()));
    let resolve_info = PackageResolveInfo::map_from_packages_with_root(&packages, workspace_root);
    let (task_graph, pruned) = TaskGraph::build_resolved(
        &package_graph,
        &pipeline,
        &resolve_info,
        &config.workers,
        worker_manager.as_ref(),
        mode,
    )
    .await
    .map_err(|error| miette::miette!("failed to build task graph: {}", error))?;
    let pruned_ids = pruned.iter().map(|entry| entry.task_id.clone()).collect();

    Ok(PreparedWorkspace {
        packages,
        package_graph,
        pipeline,
        env: config.env,
        task_graph,
        workers: config.workers,
        max_weight: max_weight_override.unwrap_or(config.concurrency.max_weight),
        pruned,
        pruned_ids,
        worker_manager,
        global_cache_nonce: config.cache.and_then(|c| c.cache_nonce),
    })
}

pub(crate) struct RunContext {
    pub(crate) package_nodes: Vec<PackageNode>,
    pub(crate) package_graph: PackageGraph,
    pub(crate) env: BTreeMap<String, EnvSpec>,
    pub(crate) task_graph: TaskGraph,
    pub(crate) workers: HashMap<String, WorkerDefinition>,
    pub(crate) max_weight: u32,
    pub(crate) pruned: Vec<PrunedTask>,
    pub(crate) worker_manager: Arc<WorkerManager>,
    pub(crate) since_affected: Option<HashSet<PackageName>>,
    /// Global cache nonce from LuchtaConfig.cache.
    pub(crate) global_cache_nonce: Option<String>,
    /// Workspace root path.
    pub(crate) workspace_root: PathBuf,
    pub(crate) task_watch_registry: TaskWatchRegistry,
}

/// Outcome of a single execution cycle.
///
/// Returned by `run_cycle` so callers can determine whether the cycle
/// completed successfully, failed, or was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CycleOutcome {
    /// All tasks completed successfully.
    Success,
    /// One or more tasks failed.
    Failed,
    /// Cycle was cancelled before completion.
    Cancelled,
}

/// Parameters for a single run cycle.
pub(crate) struct RunCycleParams<'a> {
    pub selection: &'a TaskSelection<'a>,
    pub since_affected: Option<&'a HashSet<PackageName>>,
    pub output: OutputMode,
    pub continue_on_failure: bool,
    pub memory_pressure: MemoryPressureConfig,
}

pub struct RunTasksRequest<'a> {
    pub workspace_root: &'a Path,
    pub selection: &'a TaskSelection<'a>,
    pub output: OutputMode,
    pub continue_on_failure: bool,
    pub memory_pressure: MemoryPressureConfig,
    pub max_weight_override: Option<u32>,
}

pub async fn run_tasks(request: RunTasksRequest<'_>) -> Result<()> {
    let RunTasksRequest {
        workspace_root,
        selection,
        output,
        continue_on_failure,
        memory_pressure,
        max_weight_override,
    } = request;

    let cache_dir = resolve_cache_dir(workspace_root);
    let _build_lock = match build_lock::acquire(&cache_dir).await? {
        Some(lock) => lock,
        None => return Ok(()),
    };

    // Prepare the run context (handles empty packages / NoOp since early returns)
    let Some(run) = prepare_run_context(workspace_root, selection, max_weight_override).await?
    else {
        return Ok(());
    };

    // Execute one cycle using the shared run_cycle function
    let cancel_token = CancellationToken::new();
    let (outcome, was_interrupted) = run_cycle(
        &run,
        RunCycleParams {
            selection,
            since_affected: run.since_affected.as_ref(),
            output,
            continue_on_failure,
            memory_pressure,
        },
        cancel_token,
    )
    .await?;

    // Shut down workers (the ONE place for one-shot `luchta run`)
    if was_interrupted {
        run.worker_manager.shutdown_immediate().await;
    } else {
        run.worker_manager.shutdown().await;
    }

    // Return error if failed
    if outcome == CycleOutcome::Failed {
        return Err(miette::Report::new(crate::outcome::TasksFailed));
    }

    Ok(())
}

async fn prepare_run_context(
    workspace_root: &Path,
    selection: &TaskSelection<'_>,
    max_weight_override: Option<u32>,
) -> Result<Option<RunContext>> {
    // Build session context (without since resolution)
    let mut run = match prepare_session_context(workspace_root, max_weight_override).await? {
        Some(r) => r,
        None => return Ok(None),
    };

    // Layer since resolution on top
    let since_affected =
        match resolve_since_selection(selection, workspace_root, &run.package_graph)? {
            SinceSelection::NoOp => {
                // No-op: shut down workers (session-owned) to avoid leak
                run.worker_manager.shutdown().await;
                return Ok(None);
            }
            SinceSelection::Proceed(set) => set,
        };

    // Set since_affected for one-shot run
    run.since_affected = since_affected;

    Ok(Some(run))
}

/// Prepare a session context without `--since` resolution.
///
/// Builds package graph, task graph, workers; handles empty-packages early return.
/// Used by `WatchSession::new` and layered on by `prepare_run_context` for one-shot `luchta run`.
pub(crate) async fn prepare_session_context(
    workspace_root: &Path,
    max_weight_override: Option<u32>,
) -> Result<Option<RunContext>> {
    let PreparedWorkspace {
        packages,
        package_graph,
        pipeline: _,
        env,
        task_graph,
        workers,
        max_weight,
        pruned,
        pruned_ids: _,
        worker_manager,
        global_cache_nonce,
    } = prepare_workspace(workspace_root, ResolveMode::Run, max_weight_override).await?;

    if packages.is_empty() {
        println!(
            "{}",
            "No packages found in workspace"
                .if_supports_color(Stream::Stdout, |text| text.yellow())
        );
        // Resolution may have spawned resident workers; shut them down on this
        // early exit so we do not leak worker processes.
        worker_manager.shutdown().await;
        return Ok(None);
    }

    Ok(Some(RunContext {
        package_nodes: packages,
        package_graph,
        env,
        task_graph,
        workers,
        max_weight,
        pruned,
        worker_manager,
        since_affected: None,
        global_cache_nonce,
        workspace_root: workspace_root.to_owned(),
        task_watch_registry: crate::watch::registry::empty_task_watch_registry(),
    }))
}

#[derive(Clone)]
struct DecisionContext {
    task_envs: Arc<HashMap<TaskId, BTreeMap<String, EnvSpec>>>,
    workspace_root: PathBuf,
    package_graph: Arc<PackageGraph>,
    packages: Arc<Vec<PackageNode>>,
    task_graph: Arc<TaskGraph>,
    cache: Arc<Cache>,
    output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    lockfile: Arc<LockfileState>,
    /// Shared cache for cross-worktree cache hits. `None` if shared cache disabled.
    shared_cache: Option<Arc<SharedCache>>,
    /// Run-scoped directory listing cache shared across all task resolvers.
    listing_cache: Arc<ListingCache>,
    /// Workers map for nonce resolution.
    workers: Arc<HashMap<String, WorkerDefinition>>,
    /// Global cache nonce from LuchtaConfig.cache.
    global_cache_nonce: Option<String>,
    /// Environment nonce from LUCHTA_CACHE_NONCE (read once at startup).
    env_cache_nonce: Option<String>,
    /// Progress reporter, used to replay captured logs on shared-cache hits.
    reporter: Arc<ProgressReporter>,
    task_watch_registry: TaskWatchRegistry,
}

impl DecisionContext {
    fn resolve_task_nonce(&self, task_def: &TaskDefinition) -> Option<String> {
        let env_nonce = self.env_cache_nonce.as_deref();
        let global_nonce = self.global_cache_nonce.as_deref();
        let worker_nonce = task_def
            .worker
            .as_deref()
            .and_then(|w| self.workers.get(w))
            .and_then(|wd| wd.cache.as_ref())
            .and_then(|c| c.cache_nonce.as_deref());
        let task_nonce = task_def
            .cache
            .as_ref()
            .and_then(|c| c.cache_nonce.as_deref());

        crate::cache_nonce::resolve_cache_nonce(env_nonce, global_nonce, worker_nonce, task_nonce)
    }
}

/// Shared, read-only context the dispatch loop hands to each ready task.
struct DispatchContext<'a> {
    tasks_to_run: &'a HashSet<TaskId>,
    commands: &'a HashMap<TaskId, ExecutionRequest>,
    invalid: &'a HashMap<TaskId, String>,
    executor: &'a Arc<WeightedExecutor>,
    any_failed: &'a Arc<AtomicBool>,
    interrupted: &'a Arc<AtomicBool>,
    continue_on_failure: bool,
    worker_manager: &'a Arc<WorkerManager>,
    workspace_root: &'a Path,
    packages: &'a [PackageNode],
    task_graph: &'a TaskGraph,
    cache: &'a Arc<Cache>,
    output_hashes: &'a Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    reporter: &'a Arc<ProgressReporter>,
    /// Shared cache for cross-worktree cache hits. `None` if shared cache disabled.
    shared_cache: Option<Arc<SharedCache>>,
    decision_ctx: DecisionContext,
}

struct TaskRunContext {
    executor: Arc<WeightedExecutor>,
    any_failed: Arc<AtomicBool>,
    interrupted: Arc<AtomicBool>,
    cache: Arc<Cache>,
    output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    cache_write: Option<CacheWriteContext>,
    output_hash_record: Option<OutputHashRecordContext>,
    /// Shared cache for cross-worktree cache hits. `None` if shared cache disabled.
    shared_cache: Option<Arc<SharedCache>>,
}

#[derive(Clone)]
struct CacheDecisionContext {
    action: Decision,
    run_reason: RunReason,
}

struct CacheWriteContext {
    task_id: TaskId,
    task_def: TaskDefinition,
    package_path: PathBuf,
    dep_outputs: BTreeMap<String, [u8; 32]>,
    task_spec_hash: [u8; 32],
    env_hash: [u8; 32],
    pkg_dep_hash: [u8; 32],
    start_unix_ms: u64,
    repo_root: PathBuf,
    source_pkg: PackageName,
    package_graph: PackageGraph,
    /// Resolved cache nonce string for this task.
    cache_nonce: Option<String>,
    decision: CacheDecisionContext,
    /// Registry of watched task inputs, updated when this task's record is built.
    task_watch_registry: TaskWatchRegistry,
}

struct CacheStateContext {
    cache_package: CachePackageContextOwned,
    dep_outputs: BTreeMap<String, [u8; 32]>,
    pkg_dep_pairs: Vec<(String, String)>,
    resolver: PackageDirResolver,
}

struct CachePackageContextOwned {
    package_path: PathBuf,
    package_name: PackageName,
}

struct CacheCurrentStateInput<'a> {
    task_def: &'a TaskDefinition,
    merged_env: &'a BTreeMap<String, EnvSpec>,
    nonce: Option<&'a str>,
    cache_context: &'a CacheStateContext,
}

struct SharedCacheSkipInput<'a> {
    task_id: &'a TaskId,
    task_def: &'a TaskDefinition,
    current: &'a CurrentState<'a>,
    decision: &'a DecisionResult,
}

#[derive(Clone)]
struct BuildRunRecordArgs<'a> {
    outcome: Option<&'a TaskRunOutcome>,
    succeeded: bool,
    end_unix_ms: u64,
    run_reason: Option<RunReason>,
}

struct OutputHashRecordContext {
    task_id: TaskId,
    package_path: PathBuf,
    /// Effective output patterns to hash. Initialized to the task's declared
    /// outputs and overridden by worker-detected outputs (when emitted) via
    /// [`Self::with_effective_patterns`] after the task runs.
    output_patterns: Vec<String>,
}

impl OutputHashRecordContext {
    /// Returns the context with its output patterns overridden by the worker's
    /// detected outputs when present, mirroring `effective_output_patterns`
    /// used by the cache-write path so uncached-dependency coupling hashes the
    /// same outputs a cached task would.
    fn with_effective_patterns(mut self, outcome: Option<&TaskRunOutcome>) -> Self {
        if let Some(detected) = outcome.and_then(|o| o.detected_outputs.clone()) {
            self.output_patterns = detected;
        }
        self
    }
}

enum CacheInputState {
    Ready(Box<CacheWriteContext>),
    Disabled,
}

/// Interval between periodic progress lines.
///
/// Defaults to 5 seconds. Overridable via the `LUCHTA_PROGRESS_INTERVAL_MS`
/// environment variable (milliseconds), primarily so tests can exercise the
/// periodic-progress path quickly. A missing, empty, unparseable, or zero value
/// falls back to the 5-second default.
fn progress_interval_duration() -> Duration {
    const DEFAULT_MS: u64 = 5_000;
    let ms = std::env::var("LUCHTA_PROGRESS_INTERVAL_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(DEFAULT_MS);
    Duration::from_millis(ms)
}

/// Per-cycle finalization: wait on walker, NO worker shutdown.
///
/// Used by `run_cycle` to wait for the dispatch loop to complete without
/// shutting down the worker manager. The caller is responsible for shutdown.
async fn finalize_cycle(
    walker: Walker,
    receiver: Option<tokio::sync::mpsc::Receiver<ReadyTaskMessage>>,
    interrupted: bool,
) -> Result<()> {
    if interrupted || receiver.is_none() {
        // Dropping the receiver makes the walker's channel sends fail, so it
        // stops enqueueing new work and can wind down.
        drop(receiver);
    }

    walker
        .wait()
        .await
        .into_diagnostic()
        .wrap_err("walker task panicked")
}

/// Print the tasks in the order they would run, grouped into parallel "waves",
/// without executing anything. This is a diagnostic view into the task
/// dependency graph: every displayed task in a wave only depends on displayed
/// tasks in earlier waves, so all tasks within a wave could run concurrently.
pub async fn dry_run_tasks(workspace_root: &Path, selection: &TaskSelection<'_>) -> Result<()> {
    let PreparedWorkspace {
        packages: package_nodes,
        package_graph,
        pipeline: _,
        env,
        task_graph,
        workers,
        max_weight: _,
        pruned,
        pruned_ids: _,
        worker_manager,
        global_cache_nonce: _,
    } = prepare_workspace(workspace_root, ResolveMode::Run, None).await?;

    // Resolution may have spawned resident workers; shut them down once the
    // graph is built since dry-run executes nothing.
    worker_manager.shutdown().await;

    if package_nodes.is_empty() {
        println!(
            "{}",
            "No packages found in workspace"
                .if_supports_color(Stream::Stdout, |text| text.yellow())
        );
        return Ok(());
    }

    let since_affected = match resolve_since_selection(selection, workspace_root, &package_graph)? {
        SinceSelection::NoOp => return Ok(()),
        SinceSelection::Proceed(set) => set,
    };

    let tasks_to_run = collect_requested_subgraph(CollectSubgraphRequest {
        task_graph: &task_graph,
        selection,
        pruned: &pruned,
        since_affected: since_affected.as_ref(),
        expand_dependencies: true,
    })?;
    let CommandMap {
        commands,
        invalid,
        task_envs: _,
    } = build_command_map(
        &task_graph,
        &package_nodes,
        workspace_root,
        &env,
        &workers,
        Some(&package_graph),
    );

    let displayed_waves = compute_displayed_dry_run_waves(
        &task_graph,
        &tasks_to_run,
        &commands,
        &invalid,
        selection,
        Stream::Stdout,
    );
    let displayed_task_total: usize = displayed_waves.iter().map(Vec::len).sum();

    println!(
        "{} {} task(s) across {} wave(s) (tasks within a wave can run in parallel):",
        "dry-run:".if_supports_color(Stream::Stdout, |text| text.bold()),
        displayed_task_total,
        displayed_waves.len()
    );

    for (index, wave) in displayed_waves.iter().enumerate() {
        let wave_label = format!("Wave {}:", index + 1);
        println!(
            "\n{}",
            wave_label.if_supports_color(Stream::Stdout, |text| text.cyan().bold().to_string())
        );
        for (task_id, action) in wave {
            println!(
                "  {} {}",
                task_id
                    .to_string()
                    .if_supports_color(Stream::Stdout, |text| text.bold()),
                action
            );
        }
    }

    Ok(())
}

/// Build the dry-run wave plan as it should be *displayed*: each wave keeps only
/// the tasks worth showing (paired with their description), and waves left empty
/// by filtering are dropped so wave numbering stays gap-free. The underlying
/// execution topology in `compute_execution_waves` is unchanged — this only
/// shapes presentation.
fn compute_displayed_dry_run_waves(
    task_graph: &TaskGraph,
    tasks_to_run: &HashSet<TaskId>,
    commands: &HashMap<TaskId, ExecutionRequest>,
    invalid: &HashMap<TaskId, String>,
    selection: &TaskSelection<'_>,
    stream: Stream,
) -> Vec<Vec<(TaskId, String)>> {
    compute_execution_waves(task_graph, tasks_to_run)
        .into_iter()
        .map(|wave| {
            wave.into_iter()
                .filter_map(|task_id| {
                    describe_planned_action(&task_id, commands, invalid, selection, stream)
                        .map(|action| (task_id, action))
                })
                .collect()
        })
        .filter(|wave: &Vec<(TaskId, String)>| !wave.is_empty())
        .collect()
}

/// Describe what a task would do when executed, for dry-run output. Returns
/// `None` for uninteresting connector-only tasks that would be skipped (no
/// command, no config error) so they are hidden from the plan — the visibility
/// rule and the rendered text are derived from a single lookup (issue #133).
fn describe_planned_action(
    task_id: &TaskId,
    commands: &HashMap<TaskId, ExecutionRequest>,
    invalid: &HashMap<TaskId, String>,
    selection: &TaskSelection<'_>,
    stream: Stream,
) -> Option<String> {
    if let Some(message) = invalid.get(task_id) {
        return Some(format!(
            "{} ({})",
            "config error".if_supports_color(stream, |text| text.red()),
            message
        ));
    }

    if let Some(request) = commands.get(task_id) {
        let worker = request
            .worker
            .as_deref()
            .map(|name| format!("worker '{name}'"))
            .unwrap_or_else(|| "no worker".to_string());
        return Some(format!(
            "{} via {}",
            request
                .command
                .if_supports_color(stream, |text| text.dimmed()),
            worker.if_supports_color(stream, |text| text.dimmed())
        ));
    }

    // A top-level (`-T`) request explicitly targets workspace-root tasks; keep
    // such a root ordering task in the plan with a clear label rather than
    // dropping it as noise or printing a bare id.
    if selection.top_level && task_id.is_root() {
        return Some(
            "(ordering task)"
                .if_supports_color(stream, |text| text.dimmed())
                .to_string(),
        );
    }

    None
}

fn resolve_since_selection(
    selection: &TaskSelection<'_>,
    workspace_root: &Path,
    package_graph: &PackageGraph,
) -> Result<SinceSelection> {
    let Some(since_ref) = selection.since else {
        return Ok(SinceSelection::Proceed(None));
    };

    let repo_root = crate::since::discover_repo_root(workspace_root)?;
    let affected =
        crate::since::affected_packages(workspace_root, &repo_root, since_ref, package_graph)?;
    // An empty affected set means no package changed since the ref. Normally
    // that is a no-op, but top-level (`-T`) requests target workspace-root
    // tasks which bypass the since filter entirely — those must still run.
    // In that case proceed with the (empty) affected set so `package_matches`
    // selects the root tasks and excludes every non-root task.
    if affected.is_empty() && !selection.top_level {
        println!(
            "{}",
            format!("No packages changed since {since_ref}; nothing to run.")
                .if_supports_color(Stream::Stdout, |text| text.yellow())
        );
        return Ok(SinceSelection::NoOp);
    }

    Ok(SinceSelection::Proceed(Some(affected)))
}
fn compute_longest_path_waves(
    task_graph: &TaskGraph,
    included_tasks: &HashSet<TaskId>,
) -> (HashMap<TaskId, usize>, usize) {
    // Resolve each task's wave by recursing through its dependencies. Memoize so
    // repeated dependencies are only computed once. The graph is acyclic
    // (validated during TaskGraph::build), so recursion terminates.
    fn resolve_depth(
        task_id: &TaskId,
        task_graph: &TaskGraph,
        included_tasks: &HashSet<TaskId>,
        wave_of: &mut HashMap<TaskId, usize>,
    ) -> usize {
        if let Some(&depth) = wave_of.get(task_id) {
            return depth;
        }

        let mut depth = 0;
        for dependency in task_graph.dependencies_of(task_id) {
            if !included_tasks.contains(&dependency.id) {
                continue;
            }
            let dependency_depth =
                resolve_depth(&dependency.id, task_graph, included_tasks, wave_of) + 1;
            depth = depth.max(dependency_depth);
        }

        wave_of.insert(task_id.clone(), depth);
        depth
    }

    if included_tasks.is_empty() {
        return (HashMap::new(), 0);
    }

    let mut wave_of: HashMap<TaskId, usize> = HashMap::with_capacity(included_tasks.len());
    let mut max_depth = 0;
    for task_id in included_tasks {
        let depth = resolve_depth(task_id, task_graph, included_tasks, &mut wave_of);
        max_depth = max_depth.max(depth);
    }

    (wave_of, max_depth + 1)
}

/// Compute runtime progress wave indices from full selected topology, then drop
/// uncounted tasks from returned stats map.
///
/// Longest-path depth is resolved over whole selected subgraph so ordering-only
/// connectors preserve real stage boundaries. Returned `wave_of` only includes
/// tasks that count toward runtime stats/progress via
/// `TaskDefinition::counts_in_progress()`. `total_waves` keeps full-topology wave
/// count, including waves that become empty after filtering, so runtime `🌊 X / Y`
/// stays aligned with dry-run numbering and can still reach `Y / Y` at completion.
fn compute_wave_indices(
    task_graph: &TaskGraph,
    tasks_to_run: &HashSet<TaskId>,
) -> (HashMap<TaskId, usize>, usize) {
    let (full_wave_of, total_waves) = compute_longest_path_waves(task_graph, tasks_to_run);
    let counted_wave_of = full_wave_of
        .into_iter()
        .filter(|(task_id, _)| {
            task_graph
                .task_definition(task_id)
                .is_some_and(TaskDefinition::counts_in_progress)
        })
        .collect();

    (counted_wave_of, total_waves)
}

/// Group selected tasks into ordered execution "waves" using longest-path
/// layering over full subgraph induced by `tasks_to_run`.
///
/// Dry-run lists selected tasks, not counted runtime stats, so ordering-only
/// connectors stay visible. Within each returned wave, task ids are sorted for
/// stable, readable output.
fn compute_execution_waves(
    task_graph: &TaskGraph,
    tasks_to_run: &HashSet<TaskId>,
) -> Vec<Vec<TaskId>> {
    let (wave_of, total_waves) = compute_longest_path_waves(task_graph, tasks_to_run);
    if total_waves == 0 {
        return Vec::new();
    }

    let mut waves: Vec<Vec<TaskId>> = vec![Vec::new(); total_waves];
    for (task_id, wave_index) in wave_of {
        waves[wave_index].push(task_id);
    }

    for wave in &mut waves {
        wave.sort_by_key(|task_id| task_id.to_string());
    }

    waves
}

#[derive(Debug, Clone, Copy)]
enum ShutdownSignal {
    CtrlC,
    #[cfg(unix)]
    SigTerm,
}

impl ShutdownSignal {
    fn name(self) -> &'static str {
        match self {
            Self::CtrlC => "SIGINT (Ctrl-C)",
            #[cfg(unix)]
            Self::SigTerm => "SIGTERM",
        }
    }
}

fn shutdown_signal() -> Pin<Box<dyn Future<Output = Result<ShutdownSignal>> + Send>> {
    Box::pin(async {
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .into_diagnostic()
                    .wrap_err("failed to install SIGTERM handler")?;

            let signal = tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    result.into_diagnostic().wrap_err("failed to install Ctrl-C handler")?;
                    ShutdownSignal::CtrlC
                }
                _ = sigterm.recv() => ShutdownSignal::SigTerm,
            };

            Ok(signal)
        }

        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c()
                .await
                .into_diagnostic()
                .wrap_err("failed to install Ctrl-C handler")?;
            Ok(ShutdownSignal::CtrlC)
        }
    })
}

pub(crate) fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            Glob::new(pattern).map_err(|error| {
                miette::miette!("invalid glob pattern '{}': {}", pattern, error)
            })?,
        );
    }
    builder
        .build()
        .into_diagnostic()
        .wrap_err("failed to build glob set")
}

pub(crate) struct CollectSubgraphRequest<'a> {
    pub task_graph: &'a TaskGraph,
    pub selection: &'a TaskSelection<'a>,
    pub pruned: &'a [PrunedTask],
    pub since_affected: Option<&'a HashSet<PackageName>>,
    pub expand_dependencies: bool,
}

pub(crate) fn collect_requested_subgraph(
    request: CollectSubgraphRequest<'_>,
) -> Result<HashSet<TaskId>> {
    let CollectSubgraphRequest {
        task_graph,
        selection,
        pruned,
        since_affected,
        expand_dependencies,
    } = request;
    let package_globs = build_globset(selection.packages)?;
    let task_globs = build_globset(selection.requested_tasks)?;
    let available_nodes: Vec<&TaskNode> = task_graph.nodes().collect();
    let matched_package_names =
        collect_matched_package_names(&available_nodes, selection.packages, &package_globs);

    if !selection.packages.is_empty() && matched_package_names.is_empty() {
        bail!(
            "No packages matched: [{}]. -p matches package names, not paths.",
            selection.packages.join(", ")
        );
    }

    let criteria = SelectionCriteria {
        task_globs: &task_globs,
        package_globs: &package_globs,
        match_all_non_root_packages: selection.packages.is_empty(),
        top_level: selection.top_level,
        since_affected,
    };

    let requested_ids = collect_matching_task_ids(&available_nodes, &criteria);

    let requested_task_names: HashSet<&str> = requested_ids
        .iter()
        .map(|task_id| task_id.task.as_str())
        .collect();
    let selection_prunes: Vec<PrunedTask> = available_nodes
        .iter()
        .filter(|node| !requested_ids.contains(&node.id))
        .filter(|node| package_matches(&node.id, &criteria))
        .filter(|node| {
            selection
                .requested_tasks
                .iter()
                .any(|requested| requested == node.id.task.as_str())
                || requested_task_names.contains(node.id.task.as_str())
        })
        .map(|node| PrunedTask {
            task_id: node.id.clone(),
            outcome: PruneOutcome::Pruned {
                reason: Some("not in requested subgraph".to_string()),
            },
        })
        .collect();
    let mut pruned_with_selection = Vec::with_capacity(pruned.len() + selection_prunes.len());
    pruned_with_selection.extend_from_slice(pruned);
    pruned_with_selection.extend(selection_prunes);

    validate_literal_task_requests(&requested_ids, selection, &pruned_with_selection, &criteria)?;

    if requested_ids.is_empty() && single_literal_task_request(selection.requested_tasks).is_none()
    {
        bail!(
            "No tasks matched filter: packages=[{}] tasks=[{}]",
            selection.packages.join(", "),
            selection.requested_tasks.join(", "),
        );
    }

    if expand_dependencies {
        Ok(expand_with_dependencies(task_graph, requested_ids))
    } else {
        Ok(requested_ids)
    }
}

/// Adds every node whose task name matches `task_globs` and whose package scope
/// matches selection matrix, returning matched goal ids.
pub(crate) fn collect_matching_task_ids(
    available_nodes: &[&TaskNode],
    criteria: &SelectionCriteria<'_>,
) -> HashSet<TaskId> {
    available_nodes
        .iter()
        .filter(|node| criteria.task_globs.is_match(node.id.task.as_str()))
        .filter(|node| package_matches(&node.id, criteria))
        .map(|node| node.id.clone())
        .collect()
}

pub(crate) fn collect_matched_package_names(
    available_nodes: &[&TaskNode],
    packages: &[String],
    package_globs: &GlobSet,
) -> HashSet<PackageName> {
    if packages.is_empty() {
        return HashSet::new();
    }

    available_nodes
        .iter()
        .map(|node| &node.id)
        .filter(|task_id| !task_id.is_root())
        .map(|task_id| task_id.package.clone())
        .filter(|package_name| package_globs.is_match(package_name.as_str()))
        .collect()
}

pub(crate) fn package_matches(task_id: &TaskId, criteria: &SelectionCriteria<'_>) -> bool {
    let is_root = task_id.is_root();

    let matches_non_root_package = if criteria.match_all_non_root_packages {
        !is_root
    } else {
        !is_root && criteria.package_globs.is_match(task_id.package.as_str())
    };

    let base_match = match (criteria.top_level, criteria.match_all_non_root_packages) {
        (true, false) => matches_non_root_package || is_root,
        (true, true) => is_root,
        (false, false) => matches_non_root_package,
        (false, true) => !is_root,
    };

    // The `--since` filter is an additional intersection that applies only to
    // non-root tasks: a non-root task is kept only if its package is in the
    // affected set. Root/top-level tasks bypass this check entirely so that an
    // aggregate root task still runs when the affected set is non-empty.
    if is_root {
        return base_match;
    }
    let passes_since = criteria
        .since_affected
        .map_or(true, |set| set.contains(&task_id.package));
    base_match && passes_since
}

pub(crate) fn is_literal_pattern(s: &str) -> bool {
    !s.contains(['*', '?', '[', ']', '{', '}', '!', '\\'])
}

fn validate_literal_task_requests(
    requested_ids: &HashSet<TaskId>,
    selection: &TaskSelection<'_>,
    pruned: &[PrunedTask],
    criteria: &SelectionCriteria<'_>,
) -> Result<()> {
    for requested in selection
        .requested_tasks
        .iter()
        .map(String::as_str)
        .filter(|requested| is_literal_pattern(requested))
    {
        let matched = requested_ids
            .iter()
            .any(|task_id| task_id.task.as_str() == requested);
        if !matched {
            report_unmatched_request(requested, pruned, criteria)?;
        }
    }
    Ok(())
}

fn single_literal_task_request(requested_tasks: &[String]) -> Option<&str> {
    match requested_tasks {
        [requested] if is_literal_pattern(requested) => Some(requested.as_str()),
        _ => None,
    }
}

/// Handles literal task request that matched no graph node. A task that survives
/// nowhere may have been pruned away during resolution (a normal, expected
/// outcome) — reported informationally — rather than never existing, which is
/// an error.
///
/// The pruned-away check honours the active package/scope filter: a pruned task
/// in a package excluded by `-p` (or at the wrong root scope) must not suppress
/// the "not found" error for the selected packages.
fn report_unmatched_request(
    requested: &str,
    pruned: &[PrunedTask],
    criteria: &SelectionCriteria<'_>,
) -> Result<()> {
    let pruned_away = pruned.iter().any(|entry| {
        entry.task_id.task.as_str() == requested && package_matches(&entry.task_id, criteria)
    });
    if pruned_away {
        let note_label =
            "note:".if_supports_color(Stream::Stdout, |text| text.yellow().bold().to_string());
        println!(
            "{} task '{}' was pruned from every package during resolution; nothing to run",
            note_label, requested
        );
        return Ok(());
    }
    bail!("task '{}' not found in task graph", requested);
}

/// Expands the seed task ids to include all of their transitive dependencies.
fn expand_with_dependencies(task_graph: &TaskGraph, seed: HashSet<TaskId>) -> HashSet<TaskId> {
    let mut to_visit: Vec<TaskId> = seed.iter().cloned().collect();
    let mut included = seed;

    while let Some(task_id) = to_visit.pop() {
        for dependency in task_graph.dependencies_of(&task_id) {
            if included.insert(dependency.id.clone()) {
                to_visit.push(dependency.id.clone());
            }
        }
    }

    included
}

fn compute_prefix_width(task_graph: &TaskGraph, tasks_to_run: &HashSet<TaskId>) -> usize {
    task_graph
        .nodes()
        .filter(|node| tasks_to_run.contains(&node.id))
        .map(|node| node.id.to_string().len())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use luchta_types::{DependsOn, EnvSpec};
    use std::sync::Mutex;

    /// Process-wide lock to serialize env-mutating tests.
    /// Prevents races when multiple tests use set_var/remove_var concurrently.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Guard that restores an environment variable to its prior value on drop.
    /// Captures the current value on construction (if any) and restores it
    /// (or removes if it was absent) when dropped, even on panic.
    struct EnvVarGuard {
        name: &'static str,
        prior: Option<String>,
    }

    impl EnvVarGuard {
        /// Set an env var and return a guard that will restore the prior value.
        fn set(name: &'static str, value: &str) -> Self {
            let prior = std::env::var(name).ok();
            std::env::set_var(name, value);
            Self { name, prior }
        }

        /// Remove an env var and return a guard that will restore the prior value.
        fn remove(name: &'static str) -> Self {
            let prior = std::env::var(name).ok();
            std::env::remove_var(name);
            Self { name, prior }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(ref value) = self.prior {
                std::env::set_var(self.name, value);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }

    #[tokio::test]
    async fn prepare_workspace_sets_root_package_from_workspace_root_package_json() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        std::fs::create_dir_all(temp_dir.path().join("packages/app")).expect("create package dir");
        std::fs::write(
            temp_dir.path().join("package.json"),
            r#"{
                "name": "root",
                "private": true,
                "workspaces": ["packages/*"]
            }"#,
        )
        .expect("write root package.json");
        std::fs::write(
            temp_dir.path().join("packages/app/package.json"),
            r#"{
                "name": "@repo/app"
            }"#,
        )
        .expect("write app package.json");
        std::fs::write(
            temp_dir.path().join("luchta-config.js"),
            "#!/usr/bin/env node\nconsole.log('{}');\n",
        )
        .expect("write config");

        let prepared = prepare_workspace(temp_dir.path(), ResolveMode::Run, None)
            .await
            .expect("prepare workspace");

        assert_eq!(
            prepared.package_graph.root_package(),
            Some(&PackageName::from("root"))
        );
    }

    fn make_task_def(env: std::collections::BTreeMap<String, EnvSpec>) -> TaskDefinition {
        TaskDefinition {
            env,
            ..TaskDefinition::default()
        }
    }

    fn build_test_task_graph(
        depends_on: impl Fn(TaskName) -> Vec<luchta_types::DependsOn>,
    ) -> TaskGraph {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        for (package, dependencies) in [
            ("@repo/a", vec!["@repo/b"]),
            ("@repo/b", vec!["@repo/c"]),
            ("@repo/c", Vec::new()),
        ] {
            let package_dir = temp_dir
                .path()
                .join("packages")
                .join(package.trim_start_matches("@repo/"));
            std::fs::create_dir_all(&package_dir).expect("create package dir");
            std::fs::write(
                package_dir.join("package.json"),
                serde_json::json!({
                    "name": package,
                    "version": "1.0.0",
                    "dependencies": dependencies
                        .into_iter()
                        .map(|name| (name.to_string(), serde_json::Value::String("1.0.0".to_string())))
                        .collect::<serde_json::Map<_, _>>(),
                })
                .to_string(),
            )
            .expect("write package manifest");
        }

        let package_graph = PackageGraph::build(vec![
            PackageNode::new(
                PackageName::from("@repo/a"),
                temp_dir.path().join("packages/a"),
            ),
            PackageNode::new(
                PackageName::from("@repo/b"),
                temp_dir.path().join("packages/b"),
            ),
            PackageNode::new(
                PackageName::from("@repo/c"),
                temp_dir.path().join("packages/c"),
            ),
        ])
        .expect("build package graph");
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: depends_on(TaskName::from("build")),
                worker: Some("test-worker".to_string()),
                ..TaskDefinition::default()
            },
        )]);

        TaskGraph::build(&package_graph, &pipeline).expect("build task graph")
    }

    fn assert_wave_lookup(wave_of: &HashMap<TaskId, usize>, expected: &[(&str, &str, usize)]) {
        assert_eq!(wave_of.len(), expected.len());
        for &(package, task, wave) in expected {
            assert_eq!(wave_of[&TaskId::new(package, task)], wave);
        }
    }

    #[test]
    fn compute_wave_indices_covers_every_selected_task() {
        let task_graph = build_test_task_graph(|task_name| {
            vec![luchta_types::DependsOn::DirectUpstream(task_name)]
        });
        let tasks_to_run = HashSet::from([
            TaskId::new("@repo/a", "build"),
            TaskId::new("@repo/b", "build"),
            TaskId::new("@repo/c", "build"),
        ]);

        let (wave_of, total_waves) = compute_wave_indices(&task_graph, &tasks_to_run);

        assert_eq!(total_waves, 3);
        assert_wave_lookup(
            &wave_of,
            &[
                ("@repo/c", "build", 0),
                ("@repo/b", "build", 1),
                ("@repo/a", "build", 2),
            ],
        );
    }

    #[test]
    fn compute_wave_indices_excludes_no_worker_tasks_and_empty_waves() {
        let (task_graph, tasks_to_run) = no_worker_connector_plan();

        let (wave_of, total_waves) = compute_wave_indices(&task_graph, &tasks_to_run);

        assert_eq!(total_waves, 4);
        assert_wave_lookup(
            &wave_of,
            &[("@repo/app", "build", 1), ("@repo/app", "test", 3)],
        );
    }

    #[test]
    fn compute_execution_waves_keeps_no_worker_connectors_in_dry_run_plan() {
        let (task_graph, tasks_to_run) = no_worker_connector_plan();

        let waves = compute_execution_waves(&task_graph, &tasks_to_run);

        assert_eq!(waves.len(), 4);
        assert_eq!(waves[0], vec![TaskId::new("@repo/app", "noop-root")]);
        assert_eq!(waves[1], vec![TaskId::new("@repo/app", "build")]);
        assert_eq!(waves[2], vec![TaskId::new("@repo/app", "noop-mid")]);
        assert_eq!(waves[3], vec![TaskId::new("@repo/app", "test")]);
    }

    #[test]
    fn describe_planned_action_hides_non_root_no_command_task() {
        let task_id = TaskId::new("@repo/app", "noop");
        let commands: HashMap<TaskId, ExecutionRequest> = HashMap::new();
        let invalid: HashMap<TaskId, String> = HashMap::new();
        let selection = TaskSelection {
            requested_tasks: &[],
            packages: &[],
            top_level: false,
            since: None,
        };

        assert_eq!(
            describe_planned_action(&task_id, &commands, &invalid, &selection, Stream::Stdout),
            None,
            "a no-command, non-root task must be hidden from the dry-run plan"
        );
    }

    #[test]
    fn describe_planned_action_labels_top_level_root_ordering_task() {
        // `-T` explicitly targets workspace-root tasks; a root ordering task
        // with no command must stay visible with a clear label, not a bare id.
        let task_id = TaskId::new(luchta_types::ROOT_PACKAGE_NAME, "build");
        assert!(task_id.is_root(), "root package name denotes a root task");
        let commands: HashMap<TaskId, ExecutionRequest> = HashMap::new();
        let invalid: HashMap<TaskId, String> = HashMap::new();
        let selection = TaskSelection {
            requested_tasks: &[],
            packages: &[],
            top_level: true,
            since: None,
        };

        let action =
            describe_planned_action(&task_id, &commands, &invalid, &selection, Stream::Stdout)
                .expect("top-level root ordering task should be shown");
        assert!(
            action.contains("(ordering task)"),
            "top-level root ordering task should be labeled, got: {action}"
        );
    }

    #[test]
    fn describe_planned_action_keeps_config_error_visible() {
        let task_id = TaskId::new("@repo/app", "check");
        let commands: HashMap<TaskId, ExecutionRequest> = HashMap::new();
        let mut invalid: HashMap<TaskId, String> = HashMap::new();
        invalid.insert(
            task_id.clone(),
            "defines a command but no worker".to_string(),
        );
        let selection = TaskSelection {
            requested_tasks: &[],
            packages: &[],
            top_level: false,
            since: None,
        };

        let action =
            describe_planned_action(&task_id, &commands, &invalid, &selection, Stream::Stdout)
                .expect("config-error task should stay visible");
        assert!(
            action.contains("config error"),
            "config-error task should be labeled as such, got: {action}"
        );
    }

    #[test]
    fn compute_wave_indices_counts_command_without_worker_config_error() {
        let package = "@repo/app";
        let task_graph = package_task_graph(
            vec![(package, "packages/app", Vec::new())],
            vec![
                task_entry("build", Vec::new()),
                (
                    TaskName::from("misconfigured"),
                    TaskDefinition {
                        depends_on: vec![DependsOn::SamePackage(TaskName::from("build"))],
                        command: Some("echo nope".to_string()),
                        ..TaskDefinition::default()
                    },
                ),
            ],
        );
        let tasks_to_run = HashSet::from([
            TaskId::new(package, "build"),
            TaskId::new(package, "misconfigured"),
        ]);

        let (wave_of, total_waves) = compute_wave_indices(&task_graph, &tasks_to_run);

        assert_eq!(total_waves, 2);
        assert_wave_lookup(
            &wave_of,
            &[(package, "build", 0), (package, "misconfigured", 1)],
        );
    }

    #[test]
    fn compute_wave_indices_allows_all_uncounted_selection_with_zero_counters() {
        let package = "@repo/app";
        let task_graph = package_task_graph(
            vec![(package, "packages/app", Vec::new())],
            vec![
                no_worker_task_entry("noop-root", Vec::new()),
                no_worker_task_entry(
                    "noop-leaf",
                    vec![DependsOn::SamePackage(TaskName::from("noop-root"))],
                ),
            ],
        );
        let tasks_to_run = HashSet::from([
            TaskId::new(package, "noop-root"),
            TaskId::new(package, "noop-leaf"),
        ]);

        let (wave_of, total_waves) = compute_wave_indices(&task_graph, &tasks_to_run);

        assert!(wave_of.is_empty());
        assert_eq!(total_waves, 2);
    }

    fn matching_scope_task_graph() -> TaskGraph {
        package_task_graph(
            vec![("@repo/pkg", "packages/pkg", Vec::new())],
            vec![
                task_entry("build", Vec::new()),
                task_entry("#build", Vec::new()),
            ],
        )
    }

    fn package_task_graph(
        packages: Vec<(&str, &str, Vec<&str>)>,
        tasks: Vec<(TaskName, TaskDefinition)>,
    ) -> TaskGraph {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let mut package_nodes = Vec::new();

        for (package_name, relative_dir, dependencies) in packages {
            let package_dir = temp_dir.path().join(relative_dir);
            std::fs::create_dir_all(&package_dir).expect("create package dir");
            std::fs::write(
                package_dir.join("package.json"),
                serde_json::json!({
                    "name": package_name,
                    "version": "1.0.0",
                    "dependencies": dependencies
                        .into_iter()
                        .map(|name| {
                            (
                                name.to_string(),
                                serde_json::Value::String("1.0.0".to_string()),
                            )
                        })
                        .collect::<serde_json::Map<_, _>>(),
                })
                .to_string(),
            )
            .expect("write package manifest");
            package_nodes.push(PackageNode::new(
                PackageName::from(package_name),
                package_dir,
            ));
        }

        let package_graph = PackageGraph::build(package_nodes).expect("build package graph");
        let pipeline = HashMap::from_iter(tasks);

        TaskGraph::build(&package_graph, &pipeline).expect("build task graph")
    }

    fn task_entry(name: &str, depends_on: Vec<DependsOn>) -> (TaskName, TaskDefinition) {
        test_task_entry(name, depends_on, true)
    }

    fn no_worker_task_entry(name: &str, depends_on: Vec<DependsOn>) -> (TaskName, TaskDefinition) {
        test_task_entry(name, depends_on, false)
    }

    fn test_task_entry(
        name: &str,
        depends_on: Vec<DependsOn>,
        has_worker: bool,
    ) -> (TaskName, TaskDefinition) {
        (
            TaskName::from(name),
            TaskDefinition {
                depends_on,
                worker: has_worker.then(|| "test-worker".to_string()),
                ..TaskDefinition::default()
            },
        )
    }

    fn no_worker_connector_plan() -> (TaskGraph, HashSet<TaskId>) {
        let package = "@repo/app";
        let task_graph = package_task_graph(
            vec![(package, "packages/app", Vec::new())],
            vec![
                no_worker_task_entry("noop-root", Vec::new()),
                task_entry(
                    "build",
                    vec![DependsOn::SamePackage(TaskName::from("noop-root"))],
                ),
                no_worker_task_entry(
                    "noop-mid",
                    vec![DependsOn::SamePackage(TaskName::from("build"))],
                ),
                task_entry(
                    "test",
                    vec![DependsOn::SamePackage(TaskName::from("noop-mid"))],
                ),
            ],
        );
        let tasks_to_run = HashSet::from([
            TaskId::new(package, "noop-root"),
            TaskId::new(package, "build"),
            TaskId::new(package, "noop-mid"),
            TaskId::new(package, "test"),
        ]);

        (task_graph, tasks_to_run)
    }

    fn package_selection_task_graph() -> TaskGraph {
        package_task_graph(
            vec![
                ("@repo/app", "packages/app", vec!["@repo/api"]),
                ("@repo/api", "packages/api", Vec::new()),
                ("pkg-foo", "packages/pkg-foo", Vec::new()),
                ("pkg-bar", "packages/pkg-bar", Vec::new()),
            ],
            vec![
                task_entry(
                    "build",
                    vec![DependsOn::DirectUpstream(TaskName::from("build"))],
                ),
                task_entry("build-lib", Vec::new()),
                task_entry(
                    "test",
                    vec![DependsOn::SamePackage(TaskName::from("build"))],
                ),
                task_entry(
                    "test:e2e",
                    vec![DependsOn::SamePackage(TaskName::from("build"))],
                ),
                task_entry("#build", Vec::new()),
            ],
        )
    }

    fn goal_model_prereq_task_graph() -> TaskGraph {
        package_task_graph(
            vec![
                ("@repo/app", "packages/app", vec!["@repo/api"]),
                ("@repo/api", "packages/api", Vec::new()),
            ],
            vec![
                task_entry(
                    "build",
                    vec![DependsOn::Specific(TaskId::new("@repo/api", "codegen"))],
                ),
                task_entry("codegen", Vec::new()),
            ],
        )
    }

    #[test]
    fn collect_matching_task_ids_excludes_root_tasks_by_default() {
        let task_graph = matching_scope_task_graph();
        let available_nodes: Vec<&TaskNode> = task_graph.nodes().collect();
        let task_globs = build_globset(&["build".to_string()]).expect("build task globs");
        let package_globs = build_globset(&[]).expect("build package globs");

        let criteria = SelectionCriteria {
            task_globs: &task_globs,
            package_globs: &package_globs,
            match_all_non_root_packages: true,
            top_level: false,
            since_affected: None,
        };

        let requested_ids = collect_matching_task_ids(&available_nodes, &criteria);

        assert_eq!(
            requested_ids,
            HashSet::from([TaskId::new("@repo/pkg", "build")])
        );
    }

    #[test]
    fn collect_matching_task_ids_selects_only_root_tasks_for_top_level() {
        let task_graph = matching_scope_task_graph();
        let available_nodes: Vec<&TaskNode> = task_graph.nodes().collect();
        let task_globs = build_globset(&["build".to_string()]).expect("build task globs");
        let package_globs = build_globset(&[]).expect("build package globs");

        let criteria = SelectionCriteria {
            task_globs: &task_globs,
            package_globs: &package_globs,
            match_all_non_root_packages: true,
            top_level: true,
            since_affected: None,
        };

        let requested_ids = collect_matching_task_ids(&available_nodes, &criteria);

        assert_eq!(
            requested_ids,
            HashSet::from([TaskId::new("//root", "build")])
        );
    }

    #[path = "run_selection_matrix_tests.rs"]
    mod run_selection_matrix_tests;

    #[test]
    fn build_globset_reports_invalid_pattern() {
        let error = build_globset(&["[".to_string()]).expect_err("invalid glob must fail");
        let message = error.to_string();

        assert!(message.contains("invalid glob pattern '['"));
        assert!(message.contains("["));
    }

    fn collect_requested_for_test(
        task_graph: &TaskGraph,
        selection: TaskSelection<'_>,
        expand_dependencies: bool,
    ) -> miette::Result<HashSet<TaskId>> {
        collect_requested_subgraph(CollectSubgraphRequest {
            task_graph,
            selection: &selection,
            pruned: &[],
            since_affected: None,
            expand_dependencies,
        })
    }

    #[test]
    fn collect_requested_subgraph_expands_prereqs_in_unmatched_package() {
        let task_graph = goal_model_prereq_task_graph();

        let requested = collect_requested_for_test(
            &task_graph,
            TaskSelection {
                requested_tasks: &["build".to_string()],
                packages: &["@repo/app".to_string()],
                top_level: false,
                since: None,
            },
            true,
        )
        .expect("collect requested");

        assert!(requested.contains(&TaskId::new("@repo/app", "build")));
        assert!(requested.contains(&TaskId::new("@repo/api", "codegen")));
    }

    #[test]
    fn collect_requested_subgraph_without_dependency_expansion_returns_only_direct_matches() {
        let task_graph = package_task_graph(
            vec![
                ("@repo/a", "packages/a", vec!["@repo/b"]),
                ("@repo/b", "packages/b", Vec::new()),
            ],
            vec![
                task_entry(
                    "build",
                    vec![luchta_types::DependsOn::DirectUpstream(TaskName::from(
                        "codegen",
                    ))],
                ),
                task_entry("bundle", Vec::new()),
                task_entry("codegen", Vec::new()),
            ],
        );

        let requested = collect_requested_for_test(
            &task_graph,
            TaskSelection {
                requested_tasks: &["b*".to_string()],
                packages: &[],
                top_level: false,
                since: None,
            },
            false,
        )
        .expect("collect requested without deps");

        assert_eq!(
            requested,
            HashSet::from([
                TaskId::new("@repo/a", "build"),
                TaskId::new("@repo/a", "bundle"),
                TaskId::new("@repo/b", "build"),
                TaskId::new("@repo/b", "bundle"),
            ])
        );
        assert!(
            !requested.contains(&TaskId::new("@repo/b", "codegen")),
            "no-expansion selection must not include transitive deps"
        );
    }

    #[test]
    fn collect_requested_subgraph_package_glob_does_not_select_root_without_top_level() {
        let task_graph = package_selection_task_graph();

        let error = collect_requested_for_test(
            &task_graph,
            TaskSelection {
                requested_tasks: &["build".to_string()],
                packages: &["*root*".to_string()],
                top_level: false,
                since: None,
            },
            true,
        )
        .expect_err("root sentinel must not satisfy package filter");

        assert!(error.to_string().contains("No packages matched"));
    }

    #[test]
    fn report_unmatched_request_ignores_pruned_task_at_wrong_scope() {
        // A package task `build` was pruned, but the request is top-level
        // (`-T build`). The pruned package task must NOT be treated as a match
        // for the top-level request — it should report "not found" instead.
        let pruned = vec![PrunedTask {
            task_id: TaskId::new("@repo/pkg", "build"),
            outcome: luchta_engine::PruneOutcome::Pruned { reason: None },
        }];
        let empty_globs = build_globset(&[]).expect("build empty globs");

        let top_level_criteria = SelectionCriteria {
            task_globs: &empty_globs,
            package_globs: &empty_globs,
            match_all_non_root_packages: true,
            top_level: true,
            since_affected: None,
        };
        let result = report_unmatched_request("build", &pruned, &top_level_criteria);
        assert!(
            result.is_err(),
            "top-level request must not match a pruned package task"
        );

        // The same pruned package task IS a valid explanation for a default
        // (non-top-level) request of the same name.
        let default_criteria = SelectionCriteria {
            task_globs: &empty_globs,
            package_globs: &empty_globs,
            match_all_non_root_packages: true,
            top_level: false,
            since_affected: None,
        };
        report_unmatched_request("build", &pruned, &default_criteria)
            .expect("default request should treat the pruned package task as a match");
    }

    #[test]
    fn report_unmatched_request_ignores_pruned_task_outside_package_filter() {
        // `build` was pruned in `@repo/other`, but the request is filtered to
        // `@repo/app` (`-p @repo/app build`). The pruned task in the unrelated
        // package must NOT suppress the "not found" error for the selection.
        let pruned = vec![PrunedTask {
            task_id: TaskId::new("@repo/other", "build"),
            outcome: luchta_engine::PruneOutcome::Pruned { reason: None },
        }];
        let task_globs = build_globset(&["build".to_string()]).expect("build task globs");
        let package_globs = build_globset(&["@repo/app".to_string()]).expect("build pkg globs");

        let criteria = SelectionCriteria {
            task_globs: &task_globs,
            package_globs: &package_globs,
            match_all_non_root_packages: false,
            top_level: false,
            since_affected: None,
        };
        let result = report_unmatched_request("build", &pruned, &criteria);
        assert!(
            result.is_err(),
            "a pruned task outside the package filter must not suppress the not-found error"
        );

        // A pruned task INSIDE the package filter does explain the absence.
        let pruned_in_scope = vec![PrunedTask {
            task_id: TaskId::new("@repo/app", "build"),
            outcome: luchta_engine::PruneOutcome::Pruned { reason: None },
        }];
        report_unmatched_request("build", &pruned_in_scope, &criteria)
            .expect("a pruned task within the package filter should be treated as a match");
    }

    // =========================================================================
    // package_matches / since_affected tests
    // =========================================================================

    #[test]
    fn package_matches_since_filter_cases() {
        struct Case {
            name: &'static str,
            task_id: TaskId,
            top_level: bool,
            match_all_non_root_packages: bool,
            since_affected: Option<HashSet<PackageName>>,
            expected: bool,
        }

        let task_globs = build_globset(&["build".to_string()]).expect("task globs");
        let package_globs = build_globset(&[]).expect("package globs");

        let cases = [
            Case {
                name: "non-root task in affected set should match",
                task_id: TaskId::new("@repo/app", "build"),
                top_level: false,
                match_all_non_root_packages: true,
                since_affected: Some([PackageName::from("@repo/app")].into_iter().collect()),
                expected: true,
            },
            Case {
                name: "non-root task not in affected set should be excluded",
                task_id: TaskId::new("@repo/other", "build"),
                top_level: false,
                match_all_non_root_packages: true,
                since_affected: Some([PackageName::from("@repo/app")].into_iter().collect()),
                expected: false,
            },
            Case {
                name: "root task should bypass since filter under -T",
                task_id: TaskId::new("//root", "build"),
                top_level: true,
                match_all_non_root_packages: true,
                since_affected: Some([PackageName::from("@repo/app")].into_iter().collect()),
                expected: true,
            },
            Case {
                name: "non-root task should match when since_affected is None",
                task_id: TaskId::new("@repo/app", "build"),
                top_level: false,
                match_all_non_root_packages: true,
                since_affected: None,
                expected: true,
            },
        ];

        for case in cases {
            let criteria = SelectionCriteria {
                task_globs: &task_globs,
                package_globs: &package_globs,
                match_all_non_root_packages: case.match_all_non_root_packages,
                top_level: case.top_level,
                since_affected: case.since_affected.as_ref(),
            };

            assert_eq!(
                package_matches(&case.task_id, &criteria),
                case.expected,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn resolve_task_env_explicit_value_is_used() {
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "FOO".to_string(),
            EnvSpec {
                value: Some("bar".to_string()),
                default: None,
                input: true,
            },
        );
        let task_def = make_task_def(env);

        let resolved = dispatch::resolve_task_env(&task_def.env);
        assert_eq!(resolved.get("FOO"), Some(&"bar".to_string()));
    }

    #[test]
    fn resolve_task_env_inherits_from_process_env_when_value_is_none() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::set("LUCHTA_TEST_INHERIT_VAR", "inherited_value");

        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "LUCHTA_TEST_INHERIT_VAR".to_string(),
            EnvSpec {
                value: None,
                default: None,
                input: true,
            },
        );
        let task_def = make_task_def(env);

        let resolved = dispatch::resolve_task_env(&task_def.env);
        assert_eq!(
            resolved.get("LUCHTA_TEST_INHERIT_VAR"),
            Some(&"inherited_value".to_string())
        );
    }

    #[test]
    fn resolve_task_env_omits_missing_process_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::remove("LUCHTA_TEST_UNDEF_VAR");

        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "LUCHTA_TEST_UNDEF_VAR".to_string(),
            EnvSpec {
                value: None,
                default: None,
                input: true,
            },
        );
        let task_def = make_task_def(env);

        let resolved = dispatch::resolve_task_env(&task_def.env);
        assert!(!resolved.contains_key("LUCHTA_TEST_UNDEF_VAR"));
    }

    #[test]
    fn resolve_task_env_input_false_vars_are_still_present() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::set("LUCHTA_TEST_INPUT_FALSE_VAR", "still_present");

        let mut env = std::collections::BTreeMap::new();
        // input: false should still be included in resolved env
        env.insert(
            "LUCHTA_TEST_INPUT_FALSE_VAR".to_string(),
            EnvSpec {
                value: None,
                default: None,
                input: false,
            },
        );
        let task_def = make_task_def(env);

        let resolved = dispatch::resolve_task_env(&task_def.env);
        assert_eq!(
            resolved.get("LUCHTA_TEST_INPUT_FALSE_VAR"),
            Some(&"still_present".to_string())
        );
    }

    #[test]
    fn resolve_task_env_explicit_value_overrides_process_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::set("LUCHTA_TEST_OVERRIDE_VAR", "process_value");

        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "LUCHTA_TEST_OVERRIDE_VAR".to_string(),
            EnvSpec {
                value: Some("explicit_value".to_string()),
                default: None,
                input: true,
            },
        );
        let task_def = make_task_def(env);

        let resolved = dispatch::resolve_task_env(&task_def.env);
        assert_eq!(
            resolved.get("LUCHTA_TEST_OVERRIDE_VAR"),
            Some(&"explicit_value".to_string())
        );
    }

    /// Integration test for memory-pressure pause behavior.
    ///
    /// Verifies that:
    /// - When paused, the monitor returns paused=true
    /// - Check() is called multiple times during the pause loop
    ///
    /// When the override clears, the task is dispatched.
    #[test]
    fn memory_pressure_test_override_allows_forced_pause() {
        use crate::memory_pressure::{MemoryMonitor, MemoryPressure, PressureReason};
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        // Create a monitor with test override that returns paused twice.
        let mut monitor = MemoryMonitor::for_current_process(u64::MAX, 0);
        let pause_count = Arc::new(AtomicU32::new(0));
        let pause_count_clone = Arc::clone(&pause_count);

        monitor.set_test_override(Some(Arc::new(move || {
            let count = pause_count_clone.fetch_add(1, Ordering::SeqCst);
            if count < 2 {
                MemoryPressure {
                    sample: crate::memory_pressure::MemorySample {
                        tree_rss: 1_000_000,
                        system_available: 1_000_000,
                    },
                    reasons: vec![PressureReason::UsageHigh],
                    paused: true,
                }
            } else {
                MemoryPressure {
                    sample: crate::memory_pressure::MemorySample {
                        tree_rss: 0,
                        system_available: u64::MAX,
                    },
                    reasons: vec![],
                    paused: false,
                }
            }
        })));

        // First check returns paused.
        let pressure = monitor.check();
        assert!(pressure.paused);
        assert!(pressure.reasons.contains(&PressureReason::UsageHigh));

        // Second check returns paused.
        let pressure = monitor.check();
        assert!(pressure.paused);

        // Third check clears.
        let pressure = monitor.check();
        assert!(!pressure.paused);

        // Verify call count.
        assert_eq!(pause_count.load(Ordering::SeqCst), 3);
    }
}

fn compute_cycle_outcome(
    was_cancelled: bool,
    was_interrupted: bool,
    any_failed: &AtomicBool,
) -> CycleOutcome {
    if was_cancelled {
        CycleOutcome::Cancelled
    } else if was_interrupted || any_failed.load(Ordering::SeqCst) {
        CycleOutcome::Failed
    } else {
        CycleOutcome::Success
    }
}

#[allow(clippy::manual_async_fn)]
/// Executes one cycle of task dispatch.
///
/// This function builds fresh per-cycle resources (Walker, executor, output_hashes)
/// and runs the dispatch loop to completion. It does NOT shut down the worker
/// manager — the caller is responsible for that.
///
/// The `since_affected` parameter filters tasks to affected packages:
/// - `None` = run all matching tasks (full build)
/// - `Some(&set)` = run only tasks for packages in the set
///
/// The `cancel_token` is active: dispatch races against cancellation so watch-mode
/// rebuilds can stop in-flight work without shutting down resident workers.
pub(crate) fn run_cycle<'a>(
    run: &'a RunContext,
    params: RunCycleParams<'a>,
    cancel_token: CancellationToken,
) -> impl std::future::Future<Output = Result<(CycleOutcome, bool)>> + Send + 'a {
    async move {
        let RunCycleParams {
            selection,
            since_affected,
            output,
            continue_on_failure,
            memory_pressure,
        } = params;
        let (mut memory_monitor, pressure_state) = build_memory_pressure(memory_pressure);

        let Some((tasks_to_run, reporter, resources)) =
            prepare_cycle_resources(run, selection, since_affected, output)?
        else {
            // Nothing to run: success.
            return Ok((CycleOutcome::Success, false));
        };
        let (walker, receiver) = Walker::new(&run.task_graph);
        let any_failed = Arc::new(AtomicBool::new(false));
        let interrupted = Arc::new(AtomicBool::new(false));
        let lockfile_state = load_lockfile_state(&run.workspace_root);

        let ctx = build_dispatch_context(BuildDispatchContext {
            run,
            tasks_to_run: &tasks_to_run,
            resources: &resources,
            reporter: &reporter,
            any_failed: &any_failed,
            interrupted: &interrupted,
            lockfile_state: &lockfile_state,
            continue_on_failure,
        });

        let (run_result, was_cancelled, receiver_option) = run_dispatch_with_cancel(
            DispatchWithCancel {
                ctx: &ctx,
                receiver,
                interrupted: &interrupted,
                memory_monitor: &mut memory_monitor,
                pressure_state: &pressure_state,
            },
            cancel_token,
        )
        .await;

        finalize_and_report(FinalizeCycle {
            run,
            walker,
            receiver_option,
            interrupted: &interrupted,
            was_cancelled,
            run_result,
            any_failed: &any_failed,
            reporter: &reporter,
            pressure_state: &pressure_state,
        })
        .await
    }
}

/// Borrowed inputs for constructing a per-cycle [`DispatchContext`].
struct BuildDispatchContext<'a> {
    run: &'a RunContext,
    tasks_to_run: &'a HashSet<TaskId>,
    resources: &'a ExecutionResources,
    reporter: &'a Arc<ProgressReporter>,
    any_failed: &'a Arc<AtomicBool>,
    interrupted: &'a Arc<AtomicBool>,
    lockfile_state: &'a LockfileState,
    continue_on_failure: bool,
}

/// Assembles the read-only [`DispatchContext`] handed to each ready task from the
/// run context and the per-cycle execution resources.
fn build_dispatch_context<'a>(inputs: BuildDispatchContext<'a>) -> DispatchContext<'a> {
    let BuildDispatchContext {
        run,
        tasks_to_run,
        resources,
        reporter,
        any_failed,
        interrupted,
        lockfile_state,
        continue_on_failure,
    } = inputs;
    DispatchContext {
        tasks_to_run,
        commands: &resources.commands,
        invalid: &resources.invalid,
        executor: &resources.executor,
        any_failed,
        interrupted,
        continue_on_failure,
        worker_manager: &run.worker_manager,
        workspace_root: &run.workspace_root,
        packages: &run.package_nodes,
        task_graph: &run.task_graph,
        cache: &resources.cache,
        output_hashes: &resources.output_hashes,
        reporter,
        shared_cache: resources.shared_cache.clone(),
        decision_ctx: DecisionContext {
            task_envs: Arc::new(resources.task_envs.clone()),
            workspace_root: run.workspace_root.clone(),
            package_graph: Arc::new(run.package_graph.clone()),
            packages: Arc::new(run.package_nodes.clone()),
            task_graph: Arc::new(run.task_graph.clone()),
            cache: Arc::clone(&resources.cache),
            output_hashes: Arc::clone(&resources.output_hashes),
            lockfile: Arc::new(lockfile_state.clone()),
            shared_cache: resources.shared_cache.clone(),
            listing_cache: Arc::clone(&resources.listing_cache),
            workers: Arc::new(run.workers.clone()),
            global_cache_nonce: run.global_cache_nonce.clone(),
            env_cache_nonce: std::env::var("LUCHTA_CACHE_NONCE").ok(),
            reporter: Arc::clone(reporter),
            task_watch_registry: Arc::clone(&run.task_watch_registry),
        },
    }
}

/// Collects the requested subgraph and builds the per-cycle execution resources
/// (executor, cache, commands, etc.) plus the progress reporter. Returns `None`
/// when there is nothing to run (the cycle should report success immediately).
#[allow(clippy::type_complexity)]
fn prepare_cycle_resources(
    run: &RunContext,
    selection: &TaskSelection<'_>,
    since_affected: Option<&HashSet<PackageName>>,
    output: OutputMode,
) -> Result<Option<(HashSet<TaskId>, Arc<ProgressReporter>, ExecutionResources)>> {
    let tasks_to_run = collect_requested_subgraph(CollectSubgraphRequest {
        task_graph: &run.task_graph,
        selection,
        pruned: &run.pruned,
        since_affected,
        expand_dependencies: true,
    })?;

    if tasks_to_run.is_empty() {
        return Ok(None);
    }

    let (wave_of, total_waves) = compute_wave_indices(&run.task_graph, &tasks_to_run);
    let reporter = Arc::new(ProgressReporter::new(output, wave_of, total_waves));

    let resources = build_execution_resources(BuildResourcesInputs {
        task_graph: &run.task_graph,
        packages: &run.package_nodes,
        workspace_root: &run.workspace_root,
        workers: &run.workers,
        env: &run.env,
        worker_manager: &run.worker_manager,
        max_weight: run.max_weight,
        prefix_width: compute_prefix_width(&run.task_graph, &tasks_to_run),
        package_graph: Some(&run.package_graph),
    })?;

    Ok(Some((tasks_to_run, reporter, resources)))
}

/// Borrowed inputs for the cancellable dispatch phase of a cycle.
struct DispatchWithCancel<'a> {
    ctx: &'a DispatchContext<'a>,
    receiver: tokio::sync::mpsc::Receiver<ReadyTaskMessage>,
    interrupted: &'a Arc<AtomicBool>,
    memory_monitor: &'a mut crate::memory_pressure::MemoryMonitor,
    pressure_state: &'a Arc<crate::memory_pressure::PressureState>,
}

/// Runs the dispatch loop, racing it against the cancellation token. On cancel,
/// drops the receiver so the walker stops enqueueing work and can drain. Returns
/// the dispatch result, whether it was cancelled, and the (possibly dropped)
/// receiver so the caller can finalize the walker.
async fn run_dispatch_with_cancel(
    inputs: DispatchWithCancel<'_>,
    cancel_token: CancellationToken,
) -> (
    Result<()>,
    bool,
    Option<tokio::sync::mpsc::Receiver<ReadyTaskMessage>>,
) {
    let DispatchWithCancel {
        ctx,
        receiver,
        interrupted,
        memory_monitor,
        pressure_state,
    } = inputs;

    let mut was_cancelled = false;
    let mut receiver_option = if interrupted.load(Ordering::SeqCst) {
        // Already interrupted, no need to run dispatch loop.
        None
    } else {
        Some(receiver)
    };

    let run_result = if let Some(ref mut rx) = receiver_option {
        tokio::select! {
            result = dispatch_loop(rx, ctx, memory_monitor, pressure_state) => result,
            _ = cancel_token.cancelled() => {
                was_cancelled = true;
                // Drop the receiver to stop walker from sending more work.
                drop(receiver_option.take());
                Ok(())
            }
        }
    } else {
        Ok(())
    };

    (run_result, was_cancelled, receiver_option)
}

/// Borrowed inputs for finalizing a cycle: draining the walker, shutting workers
/// down on interrupt, and reporting the outcome.
struct FinalizeCycle<'a> {
    run: &'a RunContext,
    walker: Walker,
    receiver_option: Option<tokio::sync::mpsc::Receiver<ReadyTaskMessage>>,
    interrupted: &'a Arc<AtomicBool>,
    was_cancelled: bool,
    run_result: Result<()>,
    any_failed: &'a Arc<AtomicBool>,
    reporter: &'a Arc<ProgressReporter>,
    pressure_state: &'a Arc<crate::memory_pressure::PressureState>,
}

/// Finalizes a cycle: kills workers immediately on interrupt (so the walker can
/// drain), waits on the walker, computes the outcome, and reports it.
async fn finalize_and_report(inputs: FinalizeCycle<'_>) -> Result<(CycleOutcome, bool)> {
    let FinalizeCycle {
        run,
        walker,
        receiver_option,
        interrupted,
        was_cancelled,
        run_result,
        any_failed,
        reporter,
        pressure_state,
    } = inputs;

    let was_interrupted = interrupted.load(Ordering::SeqCst);

    // On interrupt, must kill workers immediately so the walker can drain. The
    // caller also calls shutdown for cleanup, but the immediate kill is needed
    // here to unblock walker.wait().
    if was_interrupted {
        run.worker_manager.shutdown_immediate().await;
    }

    // For cancellation (watch mode) the receiver was already dropped, so the
    // walker can drain.
    finalize_cycle(walker, receiver_option, was_interrupted || was_cancelled).await?;

    let outcome = compute_cycle_outcome(was_cancelled, was_interrupted, any_failed);

    // `outcome` already encodes task failure for the caller. `report_run_outcome`
    // returns `Err(TasksFailed)` when `any_failed` is set; we must NOT let that
    // short-circuit here, or callers never get to shut down workers (one-shot
    // `luchta run`) or keep watching after a failed build (`luchta watch`).
    // A non-`any_failed` error is a genuine failure (e.g. walker panic) and is
    // still propagated.
    if let Err(err) = report_run_outcome(
        run_result,
        any_failed,
        reporter,
        pressure_state,
        outcome == CycleOutcome::Cancelled,
    ) {
        if !any_failed.load(Ordering::SeqCst) {
            return Err(err);
        }
    }

    Ok((outcome, was_interrupted))
}

#[cfg(test)]
#[path = "run_watch_tests.rs"]
mod watch_tests;
