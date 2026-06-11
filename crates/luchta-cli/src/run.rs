//! Run command implementation.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
};

use luchta_engine::{
    is_root_task, CompletionSignal, ExecutionRequest, PackageResolveInfo, PrunedTask,
    ReadyTaskMessage, ResolveMode, TaskExecutor, TaskGraph, TaskNode, Walker, WeightedExecutor,
    WorkerManager,
};
use luchta_types::{TaskDefinition, TaskId, TaskName, WorkerDefinition};
use luchta_workspace::{PackageGraph, PackageNode, WorkspaceDiscovery, YarnWorkspace};
use miette::{bail, Context, IntoDiagnostic, Result};
use owo_colors::OwoColorize;

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

pub async fn run_tasks(workspace_root: &Path, requested_tasks: &[String]) -> Result<()> {
    let PreparedWorkspace {
        packages,
        package_graph: _,
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

    report_pruned_tasks(&pruned);

    let tasks_to_run = collect_requested_subgraph(&task_graph, requested_tasks, &pruned)?;
    let prefix_width = compute_prefix_width(&task_graph, &tasks_to_run);

    let executor = Arc::new(
        WeightedExecutor::new(max_weight)
            .with_worker_manager(Arc::clone(&worker_manager))
            .with_prefix_width(prefix_width),
    );

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
    };
    let run_result = dispatch_loop(&mut receiver, &ctx).await;

    finalize_run(&worker_manager, walker, receiver, run_result.is_err()).await?;

    run_result?;

    if any_failed.load(Ordering::SeqCst) {
        bail!("one or more tasks failed");
    }

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

    loop {
        tokio::select! {
            signal_result = &mut signal => {
                signal_result?;
                // Mark interrupted BEFORE returning so in-flight task runners
                // suppress their crash output once shutdown kills the workers.
                ctx.interrupted.store(true, Ordering::SeqCst);
                break Err(miette::miette!("interrupted"));
            }
            message = receiver.recv() => {
                let Some((task_node, done_tx)) = message else {
                    break Ok(());
                };
                dispatch_ready_task(task_node, done_tx, ctx);
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

/// Group the selected tasks into ordered execution "waves" using longest-path
/// layering over the subgraph induced by `tasks_to_run`.
///
/// A task lands in wave `N` where `N` is one greater than the deepest wave of
/// any of its dependencies that are also in `tasks_to_run`. Tasks with no
/// in-subgraph dependencies are in wave 0. This mirrors how the walker releases
/// work: a task only becomes ready once all of its dependencies have completed.
/// Within each returned wave, task ids are sorted for stable, readable output.
fn compute_execution_waves(
    task_graph: &TaskGraph,
    tasks_to_run: &HashSet<TaskId>,
) -> Vec<Vec<TaskId>> {
    let mut depth_of: HashMap<TaskId, usize> = HashMap::new();

    // Resolve each task's wave by recursing through its dependencies. Memoize so
    // repeated dependencies are only computed once. The graph is acyclic
    // (validated during TaskGraph::build), so recursion terminates.
    fn resolve_depth(
        task_id: &TaskId,
        task_graph: &TaskGraph,
        tasks_to_run: &HashSet<TaskId>,
        depth_of: &mut HashMap<TaskId, usize>,
    ) -> usize {
        if let Some(&depth) = depth_of.get(task_id) {
            return depth;
        }

        let mut depth = 0;
        for dependency in task_graph.dependencies_of(task_id) {
            if !tasks_to_run.contains(&dependency.id) {
                continue;
            }
            let dependency_depth =
                resolve_depth(&dependency.id, task_graph, tasks_to_run, depth_of) + 1;
            depth = depth.max(dependency_depth);
        }

        depth_of.insert(task_id.clone(), depth);
        depth
    }

    let mut max_depth = 0;
    for task_id in tasks_to_run {
        let depth = resolve_depth(task_id, task_graph, tasks_to_run, &mut depth_of);
        max_depth = max_depth.max(depth);
    }

    let mut waves: Vec<Vec<TaskId>> = vec![Vec::new(); max_depth + 1];
    for (task_id, depth) in depth_of {
        waves[depth].push(task_id);
    }

    for wave in &mut waves {
        wave.sort_by_key(|task_id| task_id.to_string());
    }

    waves
}

fn shutdown_signal() -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
    Box::pin(async {
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .into_diagnostic()
                    .wrap_err("failed to install SIGTERM handler")?;

            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    result.into_diagnostic().wrap_err("failed to install Ctrl-C handler")?;
                }
                _ = sigterm.recv() => {}
            }
        }

        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c()
                .await
                .into_diagnostic()
                .wrap_err("failed to install Ctrl-C handler")?;
        }

        Ok(())
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

fn dispatch_ready_task(task_node: TaskNode, done_tx: CompletionSignal, ctx: &DispatchContext<'_>) {
    let task_id = task_node.id.clone();

    if !ctx.tasks_to_run.contains(&task_id) {
        let _ = done_tx.send(true);
        return;
    }

    // A misconfigured task (e.g. command without worker) only fails when it is
    // actually selected to run — it must not abort unrelated tasks.
    if let Some(message) = ctx.invalid.get(&task_id) {
        ctx.any_failed.store(true, Ordering::SeqCst);
        eprintln!("{} {}", "✖".red(), message.red());
        let _ = done_tx.send(false);
        return;
    }

    let Some(request) = ctx.commands.get(&task_id).cloned() else {
        println!(
            "{} {} (no command, skipping)",
            "○".dimmed(),
            task_id.to_string().dimmed()
        );
        let _ = done_tx.send(true);
        return;
    };

    if ctx.any_failed.load(Ordering::SeqCst) {
        println!(
            "{} {} (skipped due to previous failure)",
            "○".dimmed(),
            task_id
        );
        let _ = done_tx.send(false);
        return;
    }

    spawn_task_runner(task_id, request, done_tx, ctx);
}

/// Spawns the async runner that executes `request` and reports completion back
/// through `done_tx`. Records failures in `any_failed`; errors/non-zero exits
/// are reported unless the run was interrupted (in which case killed jobs are
/// expected and their noise is suppressed).
fn spawn_task_runner(
    task_id: TaskId,
    request: ExecutionRequest,
    done_tx: CompletionSignal,
    ctx: &DispatchContext<'_>,
) {
    let executor = Arc::clone(ctx.executor);
    let any_failed = Arc::clone(ctx.any_failed);
    let interrupted = Arc::clone(ctx.interrupted);

    tokio::spawn(async move {
        let succeeded = match executor.execute(&request.task).await {
            Ok(status) if status.success() => true,
            Ok(status) => {
                report_task_failure(
                    &task_id,
                    &format!("failed with status {:?}", status.code()),
                    &any_failed,
                    &interrupted,
                );
                false
            }
            Err(error) => {
                report_task_failure(
                    &task_id,
                    &format!("failed: {error}"),
                    &any_failed,
                    &interrupted,
                );
                false
            }
        };
        let _ = done_tx.send(succeeded);
    });
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
            env: HashMap::new(),
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
