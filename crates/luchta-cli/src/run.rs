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

use luchta_cache::{
    combined_outputs_hash, decide, resolve_cache_dir, resolve_inputs, resolve_outputs, Cache,
    Decision, RunArtifacts, TaskRunRecord, SCHEMA_VERSION_V1,
};
use luchta_engine::{
    is_root_task, CompletionSignal, ExecutionLogSink, ExecutionRequest, LogStream,
    PackageResolveInfo, PrunedTask, ReadyTaskMessage, ResolveMode, TaskGraph, TaskNode,
    TaskRunOutcome, Walker, WeightedExecutor, WorkerManager,
};
use luchta_types::{PackageName, TaskDefinition, TaskId, TaskName, WorkerDefinition};
use luchta_workspace::{PackageGraph, PackageNode, WorkspaceDiscovery, YarnWorkspace};
use miette::{bail, Context, IntoDiagnostic, Result};
use owo_colors::OwoColorize;

use crate::cache_ctx::{build_current_state, gather_pkg_dep_pairs, PackageDirResolver};
use crate::cli::OutputMode;
use crate::progress::ProgressReporter;

#[derive(Debug)]
pub struct PreparedWorkspace {
    pub packages: Vec<PackageNode>,
    pub package_graph: PackageGraph,
    pub pipeline: HashMap<TaskName, TaskDefinition>,
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
    let package_graph = PackageGraph::build(packages.clone())
        .map_err(|error| miette::miette!("failed to build package graph: {}", error))?;

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
    requested_tasks: &[String],
    output: OutputMode,
) -> Result<()> {
    let PreparedWorkspace {
        packages,
        package_graph,
        pipeline: _,
        task_graph,
        workers,
        max_weight,
        pruned,
        pruned_ids: _,
        worker_manager,
    } = prepare_workspace(workspace_root, ResolveMode::Run).await?;

    if packages.is_empty() {
        println!("{}", "No packages found in workspace".yellow());
        return Ok(());
    }

    let tasks_to_run = collect_requested_subgraph(&task_graph, requested_tasks, &pruned)?;
    let prefix_width = compute_prefix_width(&task_graph, &tasks_to_run);

    // Build the progress reporter with wave indices for all tasks to run.
    let (wave_of, total_waves) = compute_wave_indices(&task_graph, &tasks_to_run);
    let reporter = Arc::new(ProgressReporter::new(output, wave_of, total_waves));

    let executor = Arc::new(
        WeightedExecutor::new(max_weight)
            .with_worker_manager(Arc::clone(&worker_manager))
            .with_prefix_width(prefix_width),
    );
    let cache = Arc::new(
        Cache::open(&resolve_cache_dir(workspace_root))
            .into_diagnostic()
            .wrap_err("open cache")?,
    );
    let output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>> = Arc::new(Mutex::new(HashMap::new()));

    let CommandMap { commands, invalid } =
        build_command_map(&task_graph, &packages, workspace_root, &workers);

    for request in commands.values() {
        executor.register(request.clone());
    }

    let (walker, mut receiver) = Walker::new(&task_graph);
    let any_failed = Arc::new(AtomicBool::new(false));
    // Set once a shutdown signal arrives. Spawned task runners consult this so
    // that jobs killed by the interrupt don't each print a crash/failure error
    // (which would flood the terminal with one line per in-flight task).
    let interrupted = Arc::new(AtomicBool::new(false));

    let ctx = DispatchContext {
        tasks_to_run: &tasks_to_run,
        commands: &commands,
        invalid: &invalid,
        executor: &executor,
        any_failed: &any_failed,
        interrupted: &interrupted,
        workspace_root,
        package_graph: &package_graph,
        packages: &packages,
        task_graph: &task_graph,
        cache: &cache,
        output_hashes: &output_hashes,
        reporter: &reporter,
    };
    let run_result = dispatch_loop(&mut receiver, &ctx).await;

    finalize_run(&worker_manager, walker, receiver, run_result.is_err()).await?;

    run_result?;

    if any_failed.load(Ordering::SeqCst) {
        bail!("one or more tasks failed");
    }

    // Print final summary on success.
    println!("{}", reporter.render_summary());

    Ok(())
}

/// Shared, read-only context the dispatch loop hands to each ready task.
struct DispatchContext<'a> {
    tasks_to_run: &'a HashSet<TaskId>,
    commands: &'a HashMap<TaskId, ExecutionRequest>,
    invalid: &'a HashMap<TaskId, String>,
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
}

struct TaskRunContext {
    executor: Arc<WeightedExecutor>,
    any_failed: Arc<AtomicBool>,
    interrupted: Arc<AtomicBool>,
    cache: Arc<Cache>,
    output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    cache_write: Option<CacheWriteContext>,
    output_hash_record: Option<OutputHashRecordContext>,
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

/// Drives the walker's ready-task channel until it drains (normal completion)
/// or a shutdown signal (Ctrl-C / SIGTERM) arrives. Returns `Ok(())` on normal
/// completion and `Err(..)` when interrupted.
async fn dispatch_loop(
    receiver: &mut tokio::sync::mpsc::Receiver<ReadyTaskMessage>,
    ctx: &DispatchContext<'_>,
) -> Result<()> {
    let signal = shutdown_signal();
    tokio::pin!(signal);
    let mut progress_interval = tokio::time::interval(progress_interval_duration());
    progress_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    progress_interval.tick().await;

    loop {
        tokio::select! {
            signal_result = &mut signal => {
                let shutdown = signal_result?;
                // Mark interrupted BEFORE returning so in-flight task runners
                // suppress their crash output once shutdown kills the workers.
                ctx.interrupted.store(true, Ordering::SeqCst);
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
                dispatch_ready_task(task_node, done_tx, ctx);
            }
            _ = progress_interval.tick() => {
                if ctx.reporter.mode == OutputMode::Default && ctx.reporter.running_count() > 0 {
                    let rss = crate::rss::format_rss(crate::rss::process_tree_rss_bytes());
                    eprintln!("{}", ctx.reporter.render_progress(&rss));
                }
            }
        }
    }
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
pub async fn dry_run_tasks(workspace_root: &Path, requested_tasks: &[String]) -> Result<()> {
    let PreparedWorkspace {
        packages,
        package_graph: _,
        pipeline: _,
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

    if packages.is_empty() {
        println!("{}", "No packages found in workspace".yellow());
        return Ok(());
    }

    report_pruned_tasks(&pruned);

    let tasks_to_run = collect_requested_subgraph(&task_graph, requested_tasks, &pruned)?;
    let CommandMap { commands, invalid } =
        build_command_map(&task_graph, &packages, workspace_root, &workers);

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

fn collect_requested_subgraph(
    task_graph: &TaskGraph,
    requested_tasks: &[String],
    pruned: &[PrunedTask],
) -> Result<HashSet<TaskId>> {
    let mut requested_ids = HashSet::new();
    let available_nodes: Vec<&TaskNode> = task_graph.nodes().collect();

    for requested in requested_tasks {
        let matched = collect_matching_task_ids(&available_nodes, requested, &mut requested_ids);
        if !matched {
            report_unmatched_request(requested, pruned)?;
        }
    }

    Ok(expand_with_dependencies(task_graph, requested_ids))
}

/// Adds every node whose task name equals `requested` to `requested_ids`,
/// returning whether at least one matched.
fn collect_matching_task_ids(
    available_nodes: &[&TaskNode],
    requested: &str,
    requested_ids: &mut HashSet<TaskId>,
) -> bool {
    let mut matched = false;
    for node in available_nodes {
        if node.id.task.as_str() == requested {
            requested_ids.insert(node.id.clone());
            matched = true;
        }
    }
    matched
}

/// Handles a requested task that matched no graph node. A task that survives
/// nowhere may have been pruned away during resolution (a normal, expected
/// outcome) — reported informationally — rather than never existing, which is
/// an error.
fn report_unmatched_request(requested: &str, pruned: &[PrunedTask]) -> Result<()> {
    let pruned_away = pruned
        .iter()
        .any(|entry| entry.task_id.task.as_str() == requested);
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

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn package_node_for<'a>(
    packages: &'a [PackageNode],
    workspace_root: &Path,
    id: &TaskId,
) -> Option<&'a PackageNode> {
    if is_root_task(id) {
        packages
            .iter()
            .find(|package| package.path == workspace_root)
    } else {
        packages.iter().find(|package| package.name == id.package)
    }
}

struct CachePackageContext<'a> {
    package: Option<&'a PackageNode>,
    package_path: PathBuf,
    package_name: PackageName,
}

fn cache_package_context_for<'a>(
    packages: &'a [PackageNode],
    workspace_root: &Path,
    id: &TaskId,
) -> Option<CachePackageContext<'a>> {
    if is_root_task(id) {
        Some(CachePackageContext {
            package: package_node_for(packages, workspace_root, id),
            package_path: workspace_root.to_path_buf(),
            package_name: id.package.clone(),
        })
    } else {
        package_node_for(packages, workspace_root, id).map(|package| CachePackageContext {
            package: Some(package),
            package_path: package.path.clone(),
            package_name: package.name.clone(),
        })
    }
}

fn dependency_output_hashes(
    task_id: &TaskId,
    task_graph: &TaskGraph,
    output_hashes: &Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
) -> BTreeMap<String, [u8; 32]> {
    let map = output_hashes.lock().expect("output_hashes poisoned");
    task_graph
        .dependencies_of(task_id)
        .into_iter()
        .filter_map(|d| map.get(&d.id).copied().map(|h| (d.id.to_string(), h)))
        .collect()
}

fn record_output_hash(
    output_hashes: &Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    task_id: &TaskId,
    hash: [u8; 32],
) {
    output_hashes
        .lock()
        .expect("output_hashes poisoned")
        .insert(task_id.clone(), hash);
}

fn effective_output_patterns(
    task_def: &TaskDefinition,
    outcome: Option<&TaskRunOutcome>,
) -> (Vec<String>, bool) {
    match outcome.and_then(|o| o.detected_outputs.clone()) {
        Some(p) => (p, true),
        None => (task_def.outputs.clone(), false),
    }
}

fn effective_input_patterns(
    task_def: &TaskDefinition,
    outcome: Option<&TaskRunOutcome>,
) -> (Vec<String>, bool) {
    match outcome.and_then(|o| o.detected_inputs.clone()) {
        Some(patterns) => (patterns, true),
        None => (task_def.inputs.clone(), false),
    }
}

fn split_captured_logs(sink: &ExecutionLogSink) -> (Vec<u8>, Vec<u8>) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    for line in sink.lines() {
        let buf = match line.stream {
            LogStream::Stdout => &mut out,
            LogStream::Stderr => &mut err,
        };
        buf.extend_from_slice(line.line.as_bytes());
        buf.push(b'\n');
    }
    (out, err)
}

fn print_captured_logs(sink: &ExecutionLogSink) {
    for line in sink.lines() {
        match line.stream {
            LogStream::Stdout => println!("{}", line.line),
            LogStream::Stderr => eprintln!("{}", line.line),
        }
    }
}

fn dispatch_ready_task(task_node: TaskNode, done_tx: CompletionSignal, ctx: &DispatchContext<'_>) {
    let task_id = task_node.id.clone();

    if !ctx.tasks_to_run.contains(&task_id) {
        // Task not in requested subgraph — not counted.
        ctx.reporter.task_finished_other(&task_id);
        let _ = done_tx.send(true);
        return;
    }

    // A misconfigured task (e.g. command without worker) only fails when it is
    // actually selected to run — it must not abort unrelated tasks.
    if let Some(message) = ctx.invalid.get(&task_id) {
        ctx.any_failed.store(true, Ordering::SeqCst);
        eprintln!("{} {}", "✖".red(), message.red());
        // Invalid/config-error — NOT counted (failure path handles it).
        ctx.reporter.task_finished_other(&task_id);
        let _ = done_tx.send(false);
        return;
    }

    let Some(request) = ctx.commands.get(&task_id).cloned() else {
        // No command — treat ordering-only node as completed, not skipped.
        ctx.reporter.task_ran(&task_id);
        let _ = done_tx.send(true);
        return;
    };

    if ctx.any_failed.load(Ordering::SeqCst) {
        // Skipped due to previous failure — not counted.
        ctx.reporter.task_finished_other(&task_id);
        let _ = done_tx.send(false);
        return;
    }

    let cache_enabled = ctx
        .task_graph
        .task_definition(&task_id)
        .is_some_and(TaskDefinition::cache_enabled);
    if cache_enabled {
        if let Some(decision) = try_cache_skip(&task_id, ctx) {
            if matches!(decision, Decision::Skip) {
                // Cache hit — this IS the "skipped" count.
                ctx.reporter.task_skipped_cache_hit(&task_id);
                let _ = done_tx.send(true);
                return;
            }
        }
    }

    spawn_task_runner(task_id, request, done_tx, cache_enabled, ctx);
}

fn build_task_run_context(
    task_id: &TaskId,
    cache_enabled: bool,
    ctx: &DispatchContext<'_>,
) -> TaskRunContext {
    let output_hash_record =
        build_output_hash_record_context(task_id, ctx.task_graph, ctx.packages, ctx.workspace_root);
    let cache_write = if cache_enabled {
        match build_cache_write_context(task_id, ctx) {
            CacheInputState::Ready(cache_ctx) => Some(*cache_ctx),
            CacheInputState::Disabled => None,
        }
    } else {
        None
    };

    TaskRunContext {
        executor: Arc::clone(ctx.executor),
        any_failed: Arc::clone(ctx.any_failed),
        interrupted: Arc::clone(ctx.interrupted),
        cache: Arc::clone(ctx.cache),
        output_hashes: Arc::clone(ctx.output_hashes),
        cache_write,
        output_hash_record,
    }
}

fn record_resolved_output_hash(
    output_hashes: &Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    output_hash_record: &OutputHashRecordContext,
) {
    match resolve_outputs(&output_hash_record.package_path, &output_hash_record.output_patterns) {
        Ok(outputs) => {
            let outputs_hash = combined_outputs_hash(&outputs);
            record_output_hash(output_hashes, &output_hash_record.task_id, outputs_hash);
        }
        Err(error) => eprintln!(
            "warning: skipping dependency output hash record for task '{}': failed to resolve cache outputs: {error}",
            output_hash_record.task_id
        ),
    }
}

fn build_output_hash_record_context(
    task_id: &TaskId,
    task_graph: &TaskGraph,
    packages: &[PackageNode],
    workspace_root: &Path,
) -> Option<OutputHashRecordContext> {
    let task_def = task_graph.task_definition(task_id)?;
    let cache_package = cache_package_context_for(packages, workspace_root, task_id)?;
    Some(OutputHashRecordContext {
        task_id: task_id.clone(),
        package_path: cache_package.package_path,
        output_patterns: task_def.outputs.clone(),
    })
}
fn build_cache_write_context(task_id: &TaskId, ctx: &DispatchContext<'_>) -> CacheInputState {
    let Some(task_def) = ctx.task_graph.task_definition(task_id).cloned() else {
        return CacheInputState::Disabled;
    };
    let Some(cache_package) = cache_package_context_for(ctx.packages, ctx.workspace_root, task_id)
    else {
        return CacheInputState::Disabled;
    };
    let dep_outputs = dependency_output_hashes(task_id, ctx.task_graph, ctx.output_hashes);
    let synthetic_package;
    let package = if let Some(package) = cache_package.package {
        package
    } else {
        synthetic_package = PackageNode::new(
            cache_package.package_name.clone(),
            cache_package.package_path.clone(),
        );
        &synthetic_package
    };
    let pkg_dep_pairs = match gather_pkg_dep_pairs(
        ctx.workspace_root,
        package,
        cache_package.package.map(|_| ctx.package_graph),
    ) {
        Ok(pkg_dep_pairs) => pkg_dep_pairs,
        Err(error) => {
            eprintln!(
                "warning: skipping cache write for task '{task_id}': failed to gather package dependencies: {error}"
            );
            return CacheInputState::Disabled;
        }
    };
    let resolver = PackageDirResolver::new(cache_package.package_path.clone());

    let current = build_current_state(&task_def, dep_outputs.clone(), &pkg_dep_pairs, &resolver);
    let task_spec_hash = current.task_spec_hash;
    let env_hash = current.env_hash;
    let pkg_dep_hash = current.pkg_dep_hash;

    CacheInputState::Ready(Box::new(CacheWriteContext {
        task_id: task_id.clone(),
        task_def,
        package_path: cache_package.package_path,
        dep_outputs,
        task_spec_hash,
        env_hash,
        pkg_dep_hash,
        start_unix_ms: now_unix_ms(),
    }))
}

fn build_run_record(
    cache_ctx: &CacheWriteContext,
    outcome: Option<&TaskRunOutcome>,
    succeeded: bool,
    end_unix_ms: u64,
) -> Option<TaskRunRecord> {
    let (output_patterns, detected_output_patterns) =
        effective_output_patterns(&cache_ctx.task_def, outcome);
    let (input_patterns, detected_input_patterns) =
        effective_input_patterns(&cache_ctx.task_def, outcome);
    let inputs = match resolve_inputs(&cache_ctx.package_path, &input_patterns) {
        Ok(inputs) => inputs,
        Err(error) => {
            eprintln!(
                "warning: skipping cache write for task '{}': failed to resolve cache inputs: {error}",
                cache_ctx.task_id
            );
            return None;
        }
    };
    let outputs = match resolve_outputs(&cache_ctx.package_path, &output_patterns) {
        Ok(outputs) => outputs,
        Err(error) => {
            eprintln!(
                "warning: skipping cache write for task '{}': failed to resolve cache outputs: {error}",
                cache_ctx.task_id
            );
            return None;
        }
    };
    let outputs_hash = combined_outputs_hash(&outputs);
    let exit_status = outcome
        .map(|result| result.status.code().unwrap_or(1))
        .unwrap_or(1);

    Some(TaskRunRecord {
        schema_version: SCHEMA_VERSION_V1,
        task_spec_hash: cache_ctx.task_spec_hash,
        input_patterns,
        inputs,
        output_patterns,
        outputs,
        detected_input_patterns,
        detected_output_patterns,
        outputs_hash,
        env_hash: cache_ctx.env_hash,
        pkg_dep_hash: cache_ctx.pkg_dep_hash,
        dep_outputs: cache_ctx.dep_outputs.clone(),
        exit_status,
        succeeded,
        start_unix_ms: cache_ctx.start_unix_ms,
        end_unix_ms,
    })
}

async fn write_run_record(
    cache: Arc<Cache>,
    cache_ctx: CacheWriteContext,
    output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    log_sink: Option<&ExecutionLogSink>,
    outcome: Option<&TaskRunOutcome>,
    succeeded: bool,
    end_unix_ms: u64,
) {
    let Some(record) = build_run_record(&cache_ctx, outcome, succeeded, end_unix_ms) else {
        return;
    };
    record_output_hash(&output_hashes, &cache_ctx.task_id, record.outputs_hash);
    let (stdout, stderr) = log_sink.map(split_captured_logs).unwrap_or_default();
    let cache_key = cache_ctx.task_id.to_string();
    match tokio::task::spawn_blocking(move || {
        cache.write(
            &cache_key,
            RunArtifacts {
                record: &record,
                stdout: &stdout,
                stderr: &stderr,
            },
        )
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!(
            "warning: failed to write cache record for task '{}': {error}",
            cache_ctx.task_id
        ),
        Err(error) => eprintln!(
            "warning: cache write task panicked for task '{}': {error}",
            cache_ctx.task_id
        ),
    }
}

fn report_task_outcome(
    task_id: &TaskId,
    outcome: &Result<TaskRunOutcome, luchta_engine::ExecutorError>,
    any_failed: &Arc<AtomicBool>,
    interrupted: &Arc<AtomicBool>,
) {
    match outcome {
        Ok(result) if result.status.success() => {}
        Ok(result) => report_task_failure(
            task_id,
            &format!("failed with status {:?}", result.status.code()),
            any_failed,
            interrupted,
        ),
        Err(error) => report_task_failure(
            task_id,
            &format!("failed: {error}"),
            any_failed,
            interrupted,
        ),
    }
}

fn try_cache_skip(task_id: &TaskId, ctx: &DispatchContext<'_>) -> Option<Decision> {
    let task_def = ctx.task_graph.task_definition(task_id)?;
    let cache_package = cache_package_context_for(ctx.packages, ctx.workspace_root, task_id)?;
    let resolver = PackageDirResolver::new(cache_package.package_path.clone());
    let dep_outputs = dependency_output_hashes(task_id, ctx.task_graph, ctx.output_hashes);
    let synthetic_package;
    let package = if let Some(package) = cache_package.package {
        package
    } else {
        synthetic_package = PackageNode::new(
            cache_package.package_name.clone(),
            cache_package.package_path.clone(),
        );
        &synthetic_package
    };
    let pkg_dep_pairs = match gather_pkg_dep_pairs(
        ctx.workspace_root,
        package,
        cache_package.package.map(|_| ctx.package_graph),
    ) {
        Ok(pkg_dep_pairs) => pkg_dep_pairs,
        Err(error) => {
            eprintln!(
                "warning: skipping cache read for task '{task_id}': failed to gather package dependencies: {error}; task will run"
            );
            return Some(Decision::Run);
        }
    };
    let current = build_current_state(task_def, dep_outputs, &pkg_dep_pairs, &resolver);
    let prior = ctx.cache.read(&task_id.to_string());
    let decision = decide(prior.as_ref(), &current);
    if matches!(decision, Decision::Skip) {
        if let Some(p) = prior {
            record_output_hash(ctx.output_hashes, task_id, p.outputs_hash);
        }
    }
    Some(decision)
}

/// Spawns the async runner that executes `request` and reports completion back
/// through `done_tx`. Records failures in `any_failed`; errors/non-zero exits
/// are reported unless the run was interrupted (in which case killed jobs are
/// expected and their noise is suppressed).
fn spawn_task_runner(
    task_id: TaskId,
    mut request: ExecutionRequest,
    done_tx: CompletionSignal,
    cache_enabled: bool,
    ctx: &DispatchContext<'_>,
) {
    let TaskRunContext {
        executor,
        any_failed,
        interrupted,
        cache,
        output_hashes,
        cache_write,
        output_hash_record,
    } = build_task_run_context(&task_id, cache_enabled, ctx);

    let log_sink = ExecutionLogSink::new();
    request.log_sink = Some(log_sink.clone());

    // Clone reporter Arc to move into the spawned future.
    let reporter = Arc::clone(ctx.reporter);

    let started_task_id = task_id.clone();

    tokio::spawn(async move {
        let outcome_res = executor
            .run_with_on_start(&request, {
                let reporter = Arc::clone(&reporter);
                move || reporter.task_started(&started_task_id)
            })
            .await;
        let end_unix_ms = now_unix_ms();
        let succeeded = matches!(&outcome_res, Ok(result) if result.status.success());
        // Override the declared output patterns with worker-detected outputs
        // (when emitted) so uncached-dependency coupling matches the cache-write
        // path's `effective_output_patterns` precedence.
        let output_hash_record = output_hash_record
            .map(|record| record.with_effective_patterns(outcome_res.as_ref().ok()));

        persist_cache_state(CachePersistInputs {
            cache,
            cache_write,
            output_hashes: &output_hashes,
            output_hash_record: output_hash_record.as_ref(),
            log_sink: Some(&log_sink),
            outcome: outcome_res.as_ref().ok(),
            succeeded,
            end_unix_ms,
        })
        .await;

        let interrupted_run = interrupted.load(Ordering::SeqCst);
        let failed = !succeeded;
        if failed && !interrupted_run {
            print_captured_logs(&log_sink);
        }

        report_task_outcome(&task_id, &outcome_res, &any_failed, &interrupted);

        // Report task completion to the progress reporter.
        if succeeded {
            reporter.task_ran(&task_id);
        } else {
            // Failed tasks are NOT counted in done/skipped.
            reporter.task_finished_other(&task_id);
        }

        let _ = done_tx.send(succeeded);
    });
}

/// Inputs for persisting a finished task's cache state.
struct CachePersistInputs<'a> {
    cache: Arc<Cache>,
    cache_write: Option<CacheWriteContext>,
    output_hashes: &'a Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    output_hash_record: Option<&'a OutputHashRecordContext>,
    log_sink: Option<&'a ExecutionLogSink>,
    outcome: Option<&'a TaskRunOutcome>,
    succeeded: bool,
    end_unix_ms: u64,
}

/// Records the run record (cached tasks) or just the resolved output hash
/// (uncached tasks) so downstream dependency coupling stays correct.
async fn persist_cache_state(inputs: CachePersistInputs<'_>) {
    let CachePersistInputs {
        cache,
        cache_write,
        output_hashes,
        output_hash_record,
        log_sink,
        outcome,
        succeeded,
        end_unix_ms,
    } = inputs;

    if let Some(cache_ctx) = cache_write {
        write_run_record(
            cache,
            cache_ctx,
            Arc::clone(output_hashes),
            log_sink,
            outcome,
            succeeded,
            end_unix_ms,
        )
        .await;
        return;
    }

    if succeeded {
        if let Some(record) = output_hash_record {
            record_resolved_output_hash(output_hashes, record);
        }
    }
}

/// Marks the run as failed and prints a concise message, unless the run is
/// being interrupted (where killed jobs are expected and must stay quiet).
fn report_task_failure(
    task_id: &TaskId,
    detail: &str,
    any_failed: &Arc<AtomicBool>,
    interrupted: &Arc<AtomicBool>,
) {
    any_failed.store(true, Ordering::SeqCst);
    if !interrupted.load(Ordering::SeqCst) {
        eprintln!("task '{task_id}' {detail}");
    }
}

fn compute_prefix_width(task_graph: &TaskGraph, tasks_to_run: &HashSet<TaskId>) -> usize {
    task_graph
        .nodes()
        .filter(|node| tasks_to_run.contains(&node.id))
        .map(|node| node.id.to_string().len())
        .max()
        .unwrap_or(0)
}

/// Result of building the per-task execution plan.
///
/// `invalid` holds tasks that are misconfigured (e.g. a command without a
/// worker, or a worker that is not defined). Such tasks do not abort graph
/// construction; they only fail if they are actually selected for execution.
struct CommandMap {
    commands: HashMap<TaskId, ExecutionRequest>,
    invalid: HashMap<TaskId, String>,
}

fn build_command_map(
    task_graph: &TaskGraph,
    packages: &[PackageNode],
    workspace_root: &Path,
    workers: &HashMap<String, WorkerDefinition>,
) -> CommandMap {
    let package_by_name: HashMap<_, _> = packages.iter().map(|pkg| (&pkg.name, pkg)).collect();
    let mut commands = HashMap::new();
    let mut invalid = HashMap::new();

    for node in task_graph.nodes() {
        let task_id = &node.id;
        let task_def = task_graph.task_definition(task_id);
        let package = package_by_name.get(&task_id.package).copied();
        let cwd = if is_root_task(task_id) {
            workspace_root.to_path_buf()
        } else {
            package
                .map(|pkg| pkg.path.clone())
                .unwrap_or_else(|| workspace_root.to_path_buf())
        };

        let worker = task_def.and_then(|def| def.worker.clone());

        let (command, workspace) = if let Some(worker_name) = &worker {
            if !workers.contains_key(worker_name) {
                invalid.insert(
                    task_id.clone(),
                    format!("task '{task_id}' references unknown worker '{worker_name}'"),
                );
                continue;
            }
            let command = luchta_types::resolve_script_name(
                task_def.and_then(|def| def.command.as_deref()),
                task_id.task.as_str(),
            )
            .to_owned();
            let workspace = package
                .filter(|pkg| pkg.path != workspace_root)
                .map(|pkg| pkg.name.to_string())
                .unwrap_or_default();
            (command, Some(workspace))
        } else {
            match resolve_non_worker_command(task_def) {
                NonWorkerCommand::NoOp => continue,
                NonWorkerCommand::CommandWithoutWorker => {
                    invalid.insert(
                        task_id.clone(),
                        format!(
                            "task '{task_id}' defines a command but no worker; specify a worker to execute it"
                        ),
                    );
                    continue;
                }
            }
        };

        let request = ExecutionRequest {
            task: node.clone(),
            command,
            cwd: Some(cwd),
            env: resolve_task_env(task_def),
            log_sink: None,
            worker,
            workspace,
        };
        commands.insert(task_id.clone(), request);
    }

    CommandMap { commands, invalid }
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

/// Resolves environment variables from a task definition for execution.
///
/// For each `(name, EnvSpec)` in `task_def.env`:
/// - If `value` is `Some`, use that value.
/// - Otherwise, inherit from the luchta process environment via `std::env::var(name)`.
///   If the variable is unset in the process environment, omit it (do not insert).
/// - The `input` flag only affects cache hashing, NOT this resolution —
///   `input: false` vars are still present in the resulting map.
fn resolve_task_env(task_def: Option<&TaskDefinition>) -> HashMap<String, String> {
    let Some(task_def) = task_def else {
        return HashMap::new();
    };

    task_def
        .env
        .iter()
        .filter_map(|(name, spec)| {
            let value = match &spec.value {
                Some(v) => Some(v.clone()),
                None => std::env::var(name).ok(),
            };
            value.map(|v| (name.clone(), v))
        })
        .collect()
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
    use luchta_types::EnvSpec;
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

    #[test]
    fn resolve_task_env_explicit_value_is_used() {
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "FOO".to_string(),
            EnvSpec {
                value: Some("bar".to_string()),
                input: true,
            },
        );
        let task_def = make_task_def(env);

        let resolved = resolve_task_env(Some(&task_def));
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
                input: true,
            },
        );
        let task_def = make_task_def(env);

        let resolved = resolve_task_env(Some(&task_def));
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
                input: true,
            },
        );
        let task_def = make_task_def(env);

        let resolved = resolve_task_env(Some(&task_def));
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
                input: false,
            },
        );
        let task_def = make_task_def(env);

        let resolved = resolve_task_env(Some(&task_def));
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
                input: true,
            },
        );
        let task_def = make_task_def(env);

        let resolved = resolve_task_env(Some(&task_def));
        assert_eq!(
            resolved.get("LUCHTA_TEST_OVERRIDE_VAR"),
            Some(&"explicit_value".to_string())
        );
    }
}
