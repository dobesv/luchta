//! Run command implementation.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
};

use luchta_engine::{
    is_root_task, CompletionSignal, ExecutionRequest, TaskExecutor, TaskGraph, TaskNode, Walker,
    WeightedExecutor, WorkerManager,
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
}

pub fn resolve_workspace_root(workspace_root: Option<PathBuf>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().into_diagnostic()?;
    Ok(workspace_root.unwrap_or(cwd))
}

pub async fn prepare_workspace(workspace_root: &Path) -> Result<PreparedWorkspace> {
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

    let task_graph = TaskGraph::build(&package_graph, &pipeline)
        .map_err(|error| miette::miette!("failed to build task graph: {}", error))?;

    Ok(PreparedWorkspace {
        packages,
        package_graph,
        pipeline,
        task_graph,
        workers: config.workers,
        max_weight: config.concurrency.max_weight,
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
    } = prepare_workspace(workspace_root).await?;

    if packages.is_empty() {
        println!("{}", "No packages found in workspace".yellow());
        return Ok(());
    }

    let tasks_to_run = collect_requested_subgraph(&task_graph, requested_tasks)?;
    let prefix_width = compute_prefix_width(&task_graph, &tasks_to_run);

    let worker_manager = Arc::new(WorkerManager::new(workers.clone()));
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

    while let Some((task_node, done_tx)) = receiver.recv().await {
        dispatch_ready_task(
            task_node,
            done_tx,
            &tasks_to_run,
            &commands,
            &invalid,
            &executor,
            &any_failed,
        );
    }

    walker
        .wait()
        .await
        .into_diagnostic()
        .wrap_err("walker task panicked")?;

    worker_manager.shutdown().await;

    if any_failed.load(Ordering::SeqCst) {
        bail!("one or more tasks failed");
    }

    Ok(())
}

fn collect_requested_subgraph(
    task_graph: &TaskGraph,
    requested_tasks: &[String],
) -> Result<HashSet<TaskId>> {
    let mut requested_ids = HashSet::new();
    let available_nodes: Vec<&TaskNode> = task_graph.nodes().collect();

    for requested in requested_tasks {
        let mut matched = false;
        for node in &available_nodes {
            if node.id.task.as_str() == requested {
                requested_ids.insert(node.id.clone());
                matched = true;
            }
        }

        if !matched {
            bail!("task '{}' not found in task graph", requested);
        }
    }

    let mut to_visit: Vec<TaskId> = requested_ids.iter().cloned().collect();
    let mut included = requested_ids;

    while let Some(task_id) = to_visit.pop() {
        for dependency in task_graph.dependencies_of(&task_id) {
            if included.insert(dependency.id.clone()) {
                to_visit.push(dependency.id.clone());
            }
        }
    }

    Ok(included)
}

fn dispatch_ready_task(
    task_node: TaskNode,
    done_tx: CompletionSignal,
    tasks_to_run: &HashSet<TaskId>,
    commands: &HashMap<TaskId, ExecutionRequest>,
    invalid: &HashMap<TaskId, String>,
    executor: &Arc<WeightedExecutor>,
    any_failed: &Arc<AtomicBool>,
) {
    let task_id = task_node.id.clone();

    if !tasks_to_run.contains(&task_id) {
        let _ = done_tx.send(true);
        return;
    }

    // A misconfigured task (e.g. command without worker) only fails when it is
    // actually selected to run — it must not abort unrelated tasks.
    if let Some(message) = invalid.get(&task_id) {
        any_failed.store(true, Ordering::SeqCst);
        eprintln!("{} {}", "✖".red(), message.red());
        let _ = done_tx.send(false);
        return;
    }

    let Some(request) = commands.get(&task_id).cloned() else {
        println!(
            "{} {} (no command, skipping)",
            "○".dimmed(),
            task_id.to_string().dimmed()
        );
        let _ = done_tx.send(true);
        return;
    };

    if any_failed.load(Ordering::SeqCst) {
        println!(
            "{} {} (skipped due to previous failure)",
            "○".dimmed(),
            task_id
        );
        let _ = done_tx.send(false);
        return;
    }

    let executor = Arc::clone(executor);
    let any_failed = Arc::clone(any_failed);

    tokio::spawn(async move {
        match executor.execute(&request.task).await {
            Ok(status) if status.success() => {
                let _ = done_tx.send(true);
            }
            Ok(status) => {
                any_failed.store(true, Ordering::SeqCst);
                eprintln!("task '{}' failed with status {:?}", task_id, status.code());
                let _ = done_tx.send(false);
            }
            Err(error) => {
                any_failed.store(true, Ordering::SeqCst);
                eprintln!("{:?}", error);
                let _ = done_tx.send(false);
            }
        }
    });
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
            let command = task_def
                .and_then(|def| def.command.clone())
                .map(|command| command.trim().to_owned())
                .filter(|command| !command.is_empty())
                .unwrap_or_else(|| task_id.task.as_str().to_string());
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
    match task_def {
        Some(def) if def.command.is_some() => NonWorkerCommand::CommandWithoutWorker,
        _ => NonWorkerCommand::NoOp,
    }
}
