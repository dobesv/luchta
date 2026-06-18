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
    resolve_outputs, Cache, Decision, TaskRunRecord, SCHEMA_VERSION_V1,
};
use luchta_engine::{
    expand_input_patterns, is_root_task, CompletionSignal, ExecutionLogSink, ExecutionRequest,
    LogStream, PackageResolveInfo, PrunedTask, ReadyTaskMessage, ResolveMode, TaskGraph, TaskNode,
    TaskRunOutcome, Walker, WeightedExecutor, WorkerManager,
};
use luchta_types::{EnvSpec, PackageName, TaskDefinition, TaskId, TaskName, WorkerDefinition};
use luchta_workspace::{PackageGraph, PackageNode, WorkspaceDiscovery, YarnWorkspace};
use miette::{bail, Context, IntoDiagnostic, Result};
use owo_colors::OwoColorize;

use crate::cache_ctx::{
    build_current_state, gather_pkg_dep_pairs, load_lockfile_state, LockfileState,
    PackageDirResolver,
};
use crate::cli::OutputMode;
use crate::progress::ProgressReporter;

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
pub struct TaskSelection<'a> {
    pub requested_tasks: &'a [String],
    pub packages: &'a [String],
    pub top_level: bool,
}

/// Internal criteria for matching task nodes.
///
/// Pre-built from `TaskSelection` to avoid repeatedly passing the same arguments
/// to `collect_matching_task_ids` and `package_matches`.
struct SelectionCriteria<'a> {
    task_globs: &'a GlobSet,
    package_globs: &'a GlobSet,
    match_all_non_root_packages: bool,
    top_level: bool,
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
        max_weight: config.concurrency.max_weight,
        pruned,
        pruned_ids,
        worker_manager,
    })
}

pub async fn run_tasks(
    workspace_root: &Path,
    selection: &TaskSelection<'_>,
    output: OutputMode,
    memory_pressure: MemoryPressureConfig,
) -> Result<()> {
    // Construct the memory monitor + shared pressure state once before the
    // dispatch loop (used by the loop and by the status-line warning suffix).
    let (mut memory_monitor, pressure_state) = build_memory_pressure(memory_pressure);

    let PreparedWorkspace {
        packages: package_nodes,
        package_graph,
        pipeline: _,
        env,
        task_graph,
        workers,
        max_weight,
        pruned,
        pruned_ids: _,
        worker_manager,
    } = prepare_workspace(workspace_root, ResolveMode::Run).await?;

    if package_nodes.is_empty() {
        println!("{}", "No packages found in workspace".yellow());
        return Ok(());
    }

    let tasks_to_run = collect_requested_subgraph(&task_graph, selection, &pruned)?;

    // Build the progress reporter with wave indices for all tasks to run.
    let (wave_of, total_waves) = compute_wave_indices(&task_graph, &tasks_to_run);
    let reporter = Arc::new(ProgressReporter::new(output, wave_of, total_waves));

    let ExecutionResources {
        executor,
        cache,
        output_hashes,
        commands,
        invalid,
        task_envs,
        shared_cache,
    } = build_execution_resources(BuildResourcesInputs {
        task_graph: &task_graph,
        packages: &package_nodes,
        workspace_root,
        workers: &workers,
        env: &env,
        worker_manager: &worker_manager,
        max_weight,
        prefix_width: compute_prefix_width(&task_graph, &tasks_to_run),
        package_graph: Some(&package_graph),
    })?;

    run_dispatch_loop(RunDispatch {
        task_graph: &task_graph,
        tasks_to_run: &tasks_to_run,
        package_nodes: &package_nodes,
        package_graph: &package_graph,
        workspace_root,
        worker_manager: &worker_manager,
        reporter: &reporter,
        commands: &commands,
        invalid: &invalid,
        task_envs: &task_envs,
        executor: &executor,
        cache: &cache,
        output_hashes: &output_hashes,
        memory_monitor: &mut memory_monitor,
        pressure_state: &pressure_state,
        shared_cache,
    })
    .await
}

/// Bundles the borrowed state needed to drive the dispatch loop, so
/// `run_tasks` stays small and the many shared references travel as one unit.
struct RunDispatch<'a> {
    task_graph: &'a TaskGraph,
    tasks_to_run: &'a HashSet<TaskId>,
    package_nodes: &'a [PackageNode],
    package_graph: &'a PackageGraph,
    workspace_root: &'a Path,
    worker_manager: &'a Arc<WorkerManager>,
    reporter: &'a Arc<ProgressReporter>,
    commands: &'a HashMap<TaskId, ExecutionRequest>,
    invalid: &'a HashMap<TaskId, String>,
    task_envs: &'a HashMap<TaskId, BTreeMap<String, EnvSpec>>,
    executor: &'a Arc<WeightedExecutor>,
    cache: &'a Arc<Cache>,
    output_hashes: &'a Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    memory_monitor: &'a mut crate::memory_pressure::MemoryMonitor,
    pressure_state: &'a Arc<crate::memory_pressure::PressureState>,
    shared_cache: Option<Arc<SharedCache>>,
}

/// Constructs the dispatch context, runs the dispatch loop to completion, and
/// finalizes the run (worker shutdown + outcome reporting).
async fn run_dispatch_loop(d: RunDispatch<'_>) -> Result<()> {
    let (walker, mut receiver) = Walker::new(d.task_graph);
    let any_failed = Arc::new(AtomicBool::new(false));
    // Set once a shutdown signal arrives. Spawned task runners consult this so
    // that jobs killed by the interrupt don't each print a crash/failure error
    // (which would flood the terminal with one line per in-flight task).
    let interrupted = Arc::new(AtomicBool::new(false));
    let lockfile_state = load_lockfile_state(d.workspace_root);

    let ctx = DispatchContext {
        tasks_to_run: d.tasks_to_run,
        commands: d.commands,
        invalid: d.invalid,
        task_envs: d.task_envs,
        executor: d.executor,
        any_failed: &any_failed,
        interrupted: &interrupted,
        workspace_root: d.workspace_root,
        package_graph: d.package_graph,
        packages: d.package_nodes,
        task_graph: d.task_graph,
        cache: d.cache,
        output_hashes: d.output_hashes,
        reporter: d.reporter,
        lockfile: &lockfile_state,
        shared_cache: d.shared_cache.clone(),
    };
    let run_result = dispatch_loop(&mut receiver, &ctx, d.memory_monitor, d.pressure_state).await;

    finalize_run(d.worker_manager, walker, receiver, run_result.is_err()).await?;

    report_run_outcome(run_result, &any_failed, d.reporter, d.pressure_state)
}

/// Shared, read-only context the dispatch loop hands to each ready task.
struct DispatchContext<'a> {
    tasks_to_run: &'a HashSet<TaskId>,
    commands: &'a HashMap<TaskId, ExecutionRequest>,
    invalid: &'a HashMap<TaskId, String>,
    task_envs: &'a HashMap<TaskId, BTreeMap<String, EnvSpec>>,
    executor: &'a Arc<WeightedExecutor>,
    any_failed: &'a Arc<AtomicBool>,
    interrupted: &'a Arc<AtomicBool>,
    workspace_root: &'a Path,
    package_graph: &'a PackageGraph,
    packages: &'a [PackageNode],
    task_graph: &'a TaskGraph,
    cache: &'a Arc<Cache>,
    output_hashes: &'a Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    reporter: &'a Arc<ProgressReporter>,
    lockfile: &'a LockfileState,
    /// Shared cache for cross-worktree cache hits. `None` if shared cache disabled.
    shared_cache: Option<Arc<SharedCache>>,
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

/// Tears down workers and drains the walker after the dispatch loop ends.
///
/// On interruption the dispatch loop stops draining the walker's ready channel
/// while a task is still executing. The walker cannot finish until that
/// in-flight task reports completion, and the task cannot complete until its
/// worker subprocess is stopped. So on interrupt we shut workers down FIRST
/// (immediately, killing their process groups), which makes the executor
/// resolve the pending job as a failure and lets the walker drain. On the
/// normal path the walker has already finished, so workers are shut down
/// gracefully afterwards.
async fn finalize_run(
    worker_manager: &Arc<WorkerManager>,
    walker: Walker,
    receiver: tokio::sync::mpsc::Receiver<ReadyTaskMessage>,
    interrupted: bool,
) -> Result<()> {
    if interrupted {
        // Dropping the receiver makes the walker's channel sends fail, so it
        // stops enqueueing new work and can wind down once the in-flight job is
        // resolved by the worker shutdown.
        drop(receiver);
        worker_manager.shutdown_immediate().await;
    }

    let walker_result = walker
        .wait()
        .await
        .into_diagnostic()
        .wrap_err("walker task panicked");

    if !interrupted {
        worker_manager.shutdown().await;
    }

    walker_result
}

/// Emit an informational line for each task the resolution phase pruned, with
/// the worker-supplied reason (e.g. "script `build` not found in package …").
/// Pruning is normal, expected behavior — not an error — so this is a notice.
pub fn report_pruned_tasks(pruned: &[PrunedTask]) {
    if pruned.is_empty() {
        return;
    }

    let mut entries: Vec<&PrunedTask> = pruned.iter().collect();
    entries.sort_by_key(|entry| entry.task_id.to_string());

    println!(
        "{} {} task(s) pruned during resolution:",
        "note:".bold().yellow(),
        entries.len()
    );
    for entry in entries {
        println!(
            "  {} {}",
            entry.task_id.to_string().bold(),
            format!("({})", entry.outcome.describe()).dimmed()
        );
    }
}

/// Print the tasks in the order they would run, grouped into parallel "waves",
/// without executing anything. This is a diagnostic view into the task
/// dependency graph: every task in a wave only depends on tasks in earlier
/// waves, so all tasks within a wave could run concurrently.
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
    } = prepare_workspace(workspace_root, ResolveMode::Run).await?;

    // Resolution may have spawned resident workers; shut them down once the
    // graph is built since dry-run executes nothing.
    worker_manager.shutdown().await;

    if package_nodes.is_empty() {
        println!("{}", "No packages found in workspace".yellow());
        return Ok(());
    }

    report_pruned_tasks(&pruned);

    let tasks_to_run = collect_requested_subgraph(&task_graph, selection, &pruned)?;
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

    let waves = compute_execution_waves(&task_graph, &tasks_to_run);

    println!(
        "{} {} task(s) across {} wave(s) (tasks within a wave can run in parallel):",
        "dry-run:".bold(),
        tasks_to_run.len(),
        waves.len()
    );

    for (index, wave) in waves.iter().enumerate() {
        println!("\n{}", format!("Wave {}:", index + 1).bold().cyan());
        for task_id in wave {
            let action = describe_planned_action(task_id, &commands, &invalid);
            println!("  {} {}", task_id.to_string().bold(), action);
        }
    }

    Ok(())
}

/// Describe what a task would do when executed, for the dry-run output.
fn describe_planned_action(
    task_id: &TaskId,
    commands: &HashMap<TaskId, ExecutionRequest>,
    invalid: &HashMap<TaskId, String>,
) -> String {
    if let Some(message) = invalid.get(task_id) {
        return format!("{} ({})", "config error".red(), message);
    }

    match commands.get(task_id) {
        Some(request) => {
            let worker = request
                .worker
                .as_deref()
                .map(|name| format!("worker '{name}'"))
                .unwrap_or_else(|| "no worker".to_string());
            format!("{} via {}", request.command.dimmed(), worker.dimmed())
        }
        None => "(no command, would be skipped)".dimmed().to_string(),
    }
}

/// Compute longest-path wave indices over subgraph induced by `tasks_to_run`.
///
/// A task lands in wave `N` where `N` is one greater than deepest wave of any
/// of its dependencies that are also in `tasks_to_run`. Tasks with no
/// in-subgraph dependencies are in wave 0. Returns `(wave_of, total_waves)`,
/// where `total_waves` is `max_wave + 1`, or `0` when `tasks_to_run` is empty.
fn compute_wave_indices(
    task_graph: &TaskGraph,
    tasks_to_run: &HashSet<TaskId>,
) -> (HashMap<TaskId, usize>, usize) {
    // Resolve each task's wave by recursing through its dependencies. Memoize so
    // repeated dependencies are only computed once. The graph is acyclic
    // (validated during TaskGraph::build), so recursion terminates.
    fn resolve_depth(
        task_id: &TaskId,
        task_graph: &TaskGraph,
        tasks_to_run: &HashSet<TaskId>,
        wave_of: &mut HashMap<TaskId, usize>,
    ) -> usize {
        if let Some(&depth) = wave_of.get(task_id) {
            return depth;
        }

        let mut depth = 0;
        for dependency in task_graph.dependencies_of(task_id) {
            if !tasks_to_run.contains(&dependency.id) {
                continue;
            }
            let dependency_depth =
                resolve_depth(&dependency.id, task_graph, tasks_to_run, wave_of) + 1;
            depth = depth.max(dependency_depth);
        }

        wave_of.insert(task_id.clone(), depth);
        depth
    }

    if tasks_to_run.is_empty() {
        return (HashMap::new(), 0);
    }

    let mut wave_of: HashMap<TaskId, usize> = HashMap::with_capacity(tasks_to_run.len());
    let mut max_depth = 0;
    for task_id in tasks_to_run {
        let depth = resolve_depth(task_id, task_graph, tasks_to_run, &mut wave_of);
        max_depth = max_depth.max(depth);
    }

    (wave_of, max_depth + 1)
}

/// Group the selected tasks into ordered execution "waves" using longest-path
/// layering over the subgraph induced by `tasks_to_run`.
///
/// This mirrors how the walker releases work: a task only becomes ready once
/// all of its dependencies have completed. Within each returned wave, task ids
/// are sorted for stable, readable output.
fn compute_execution_waves(
    task_graph: &TaskGraph,
    tasks_to_run: &HashSet<TaskId>,
) -> Vec<Vec<TaskId>> {
    let (wave_of, total_waves) = compute_wave_indices(task_graph, tasks_to_run);
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
    SigTerm,
}

impl ShutdownSignal {
    fn name(self) -> &'static str {
        match self {
            Self::CtrlC => "SIGINT (Ctrl-C)",
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

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
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

fn collect_requested_subgraph(
    task_graph: &TaskGraph,
    selection: &TaskSelection<'_>,
    pruned: &[PrunedTask],
) -> Result<HashSet<TaskId>> {
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
    };

    let requested_ids = collect_matching_task_ids(&available_nodes, &criteria);

    validate_literal_task_requests(&requested_ids, selection, pruned, &criteria)?;

    if requested_ids.is_empty() && single_literal_task_request(selection.requested_tasks).is_none()
    {
        bail!(
            "No tasks matched filter: packages=[{}] tasks=[{}]",
            selection.packages.join(", "),
            selection.requested_tasks.join(", "),
        );
    }

    Ok(expand_with_dependencies(task_graph, requested_ids))
}

/// Adds every node whose task name matches `task_globs` and whose package scope
/// matches selection matrix, returning matched goal ids.
fn collect_matching_task_ids(
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

fn collect_matched_package_names(
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

fn package_matches(task_id: &TaskId, criteria: &SelectionCriteria<'_>) -> bool {
    let is_root = task_id.is_root();
    let matches_non_root_package = if criteria.match_all_non_root_packages {
        !is_root
    } else {
        !is_root && criteria.package_globs.is_match(task_id.package.as_str())
    };

    match (criteria.top_level, criteria.match_all_non_root_packages) {
        (true, false) => matches_non_root_package || is_root,
        (true, true) => is_root,
        (false, false) => matches_non_root_package,
        (false, true) => !is_root,
    }
}

fn is_literal_pattern(s: &str) -> bool {
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
        println!(
            "{} task '{}' was pruned from every package during resolution; nothing to run",
            "note:".bold().yellow(),
            requested
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

/// Outcome of resolving the command for a non-worker task.
enum NonWorkerCommand {
    /// No worker and no command: a pure no-op node (ordering only).
    NoOp,
    /// A command is declared without a worker. This is a configuration error,
    /// but it must only fail the task itself *if it is actually executed* — it
    /// must not abort the whole run during graph construction.
    CommandWithoutWorker,
}

fn resolve_non_worker_command(task_def: Option<&TaskDefinition>) -> NonWorkerCommand {
    // A blank/whitespace-only command is treated as absent — matching the
    // worker path's `resolve_script_name` normalization and `check`'s
    // `has_non_blank_command` — so it is a no-op node, not a config error.
    let has_command = task_def
        .and_then(|def| def.command.as_deref())
        .map(str::trim)
        .is_some_and(|command| !command.is_empty());
    if has_command {
        NonWorkerCommand::CommandWithoutWorker
    } else {
        NonWorkerCommand::NoOp
    }
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

        let prepared = prepare_workspace(temp_dir.path(), ResolveMode::Run)
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
                ..TaskDefinition::default()
            },
        )]);

        TaskGraph::build(&package_graph, &pipeline).expect("build task graph")
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

        assert_eq!(wave_of.len(), tasks_to_run.len());
        for task_id in &tasks_to_run {
            assert!(wave_of.contains_key(task_id), "missing {task_id}");
        }
        assert_eq!(total_waves, 3);
        assert_eq!(wave_of[&TaskId::new("@repo/c", "build")], 0);
        assert_eq!(wave_of[&TaskId::new("@repo/b", "build")], 1);
        assert_eq!(wave_of[&TaskId::new("@repo/a", "build")], 2);
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
        (
            TaskName::from(name),
            TaskDefinition {
                depends_on,
                ..TaskDefinition::default()
            },
        )
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

    #[test]
    fn collect_requested_subgraph_expands_prereqs_in_unmatched_package() {
        let task_graph = goal_model_prereq_task_graph();

        let selection = TaskSelection {
            requested_tasks: &["build".to_string()],
            packages: &["@repo/app".to_string()],
            top_level: false,
        };
        let requested =
            collect_requested_subgraph(&task_graph, &selection, &[]).expect("collect requested");

        assert!(requested.contains(&TaskId::new("@repo/app", "build")));
        assert!(requested.contains(&TaskId::new("@repo/api", "codegen")));
    }

    #[test]
    fn collect_requested_subgraph_package_glob_does_not_select_root_without_top_level() {
        let task_graph = package_selection_task_graph();

        let selection = TaskSelection {
            requested_tasks: &["build".to_string()],
            packages: &["*root*".to_string()],
            top_level: false,
        };
        let error = collect_requested_subgraph(&task_graph, &selection, &[])
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
