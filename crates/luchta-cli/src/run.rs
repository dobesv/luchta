//! Run command implementation.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
};

use luchta_engine::{
    CompletionSignal, ExecutionRequest, TaskGraph, TaskNode, Walker, WeightedExecutor,
    WorkerManager,
};
use luchta_types::{TaskDefinition, TaskId, TaskName, WorkerDefinition};
use luchta_workspace::{PackageNode, WorkspaceDiscovery, YarnWorkspace};
use miette::{bail, Context, IntoDiagnostic, Result};
use owo_colors::OwoColorize;
use petgraph::Direction;
use serde::Deserialize;

/// Script entry from package.json scripts map.
#[derive(Debug, Clone, Deserialize)]
struct PackageJson {
    scripts: Option<HashMap<String, String>>,
}

/// Resolve workspace root: use explicit path, otherwise walk up from cwd
/// looking for `package.json` with `workspaces` field.
pub fn resolve_workspace_root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if !path.join("package.json").exists() {
            bail!(
                "workspace root {} does not contain package.json",
                path.display()
            );
        }
        return Ok(path);
    }

    let cwd = std::env::current_dir()
        .into_diagnostic()
        .wrap_err("failed to get current directory")?;

    find_workspace_root(&cwd).ok_or_else(|| {
        miette::miette!(
            "no package.json with 'workspaces' field found in {} or any parent directory",
            cwd.display()
        )
    })
}

/// Walk up from `start` looking for package.json with workspaces.
fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if has_workspaces_field(&current) {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Returns true when `dir`'s `package.json` declares a `workspaces` field.
fn has_workspaces_field(dir: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(dir.join("package.json")) else {
        return false;
    };
    let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    pkg.get("workspaces").is_some()
}

/// Run the requested tasks.
pub async fn run_tasks(workspace_root: &PathBuf, requested_tasks: &[String]) -> Result<()> {
    // Load config
    let config = crate::config::load_config(workspace_root)
        .await
        .wrap_err_with(|| {
            format!(
                "failed to load config from workspace root {}",
                workspace_root.display()
            )
        })?;

    // Discover packages
    let workspace = YarnWorkspace::new(workspace_root);
    let packages = workspace
        .discover()
        .map_err(|e| miette::miette!("workspace discovery failed: {}", e))?;

    if packages.is_empty() {
        println!("{}", "No packages found in workspace".yellow());
        return Ok(());
    }

    // Build package graph
    let package_graph = luchta_workspace::PackageGraph::build(packages.clone())
        .map_err(|e| miette::miette!("failed to build package graph: {}", e))?;

    // Clone workers before config.tasks is moved.
    let workers: HashMap<String, WorkerDefinition> = config.workers.clone();

    // Build task graph - convert HashMap<String, TaskDefinition> to HashMap<TaskName, TaskDefinition>
    let pipeline: HashMap<TaskName, _> = config
        .tasks
        .into_iter()
        .map(|(name, def)| (TaskName::from(name), def))
        .collect();

    let task_graph = TaskGraph::build(&package_graph, &pipeline)
        .map_err(|e| miette::miette!("failed to build task graph: {}", e))?;

    // Determine which tasks to run (requested + their dependency closure)
    let tasks_to_run = resolve_tasks_to_run(&task_graph, requested_tasks)?;

    // Compute prefix width from tasks to run
    let prefix_width = compute_prefix_width(&task_graph, &tasks_to_run);

    // Build command map, create worker manager and executor
    let worker_manager =
        Arc::new(WorkerManager::new(workers.clone()).with_prefix_width(prefix_width));
    let max_weight = config.concurrency.max_weight;
    let executor = Arc::new(
        WeightedExecutor::new(max_weight)
            .with_worker_manager(Arc::clone(&worker_manager))
            .with_prefix_width(prefix_width),
    );

    // Build command map for each task node
    let commands = build_command_map(&task_graph, &packages, &pipeline, workspace_root, &workers)?;

    // Register execution requests
    for request in commands.values() {
        executor.register(request.clone());
    }

    // Create walker
    let (walker, mut receiver) = Walker::new(&task_graph);

    // Track failures
    let any_failed = Arc::new(AtomicBool::new(false));

    // Process ready tasks
    while let Some((task_node, done_tx)) = receiver.recv().await {
        dispatch_ready_task(
            task_node,
            done_tx,
            &tasks_to_run,
            &commands,
            &executor,
            &any_failed,
        );
    }

    // Wait for walker to complete
    walker
        .wait()
        .await
        .into_diagnostic()
        .wrap_err("walker task panicked")?;

    // Check final status
    let result = if any_failed.load(Ordering::SeqCst) {
        Err(miette::miette!("one or more tasks failed"))
    } else {
        Ok(())
    };

    // Gracefully shut down workers
    worker_manager.shutdown().await;

    result
}

/// Handles a single ready task: skip non-targets/no-ops, short-circuit after a
/// prior failure, or spawn the command for execution.
fn dispatch_ready_task(
    task_node: TaskNode,
    done_tx: CompletionSignal,
    tasks_to_run: &HashSet<TaskId>,
    commands: &HashMap<TaskId, ExecutionRequest>,
    executor: &Arc<WeightedExecutor>,
    any_failed: &Arc<AtomicBool>,
) {
    let task_id = task_node.id.clone();

    // Skip if not in our target set (send success to unblock dependents).
    if !tasks_to_run.contains(&task_id) {
        let _ = done_tx.send(true);
        return;
    }

    // No command - this is a no-op, mark as success.
    let Some(request) = commands.get(&task_id) else {
        println!(
            "{} {} (no command, skipping)",
            "○".dimmed(),
            task_id.to_string().dimmed()
        );
        let _ = done_tx.send(true);
        return;
    };

    // Short-circuit once any prior task has failed.
    if any_failed.load(Ordering::SeqCst) {
        println!(
            "{} {} (skipped due to prior failure)",
            "⊘".yellow(),
            task_id.to_string().yellow()
        );
        let _ = done_tx.send(false);
        return;
    }

    println!("{} {}", "●".green(), task_id.to_string().green());
    let job = TaskJob {
        task_id,
        request: request.clone(),
        executor: Arc::clone(executor),
        any_failed: Arc::clone(any_failed),
    };
    spawn_task_execution(job, done_tx);
}

/// Everything needed to execute one task on the executor.
struct TaskJob {
    task_id: TaskId,
    request: ExecutionRequest,
    executor: Arc<WeightedExecutor>,
    any_failed: Arc<AtomicBool>,
}

/// Spawns a task on the executor, reporting status and recording any failure.
fn spawn_task_execution(job: TaskJob, done_tx: CompletionSignal) {
    let TaskJob {
        task_id,
        request,
        executor,
        any_failed,
    } = job;

    tokio::spawn(async move {
        match executor.run(&request).await {
            Ok(status) if status.success() => {
                println!("{} {}", "✓".green(), task_id.to_string().green());
                let _ = done_tx.send(true);
            }
            Ok(status) => {
                eprintln!(
                    "{} {} (exit code: {:?})",
                    "✗".red(),
                    task_id.to_string().red(),
                    status.code()
                );
                any_failed.store(true, Ordering::SeqCst);
                let _ = done_tx.send(false);
            }
            Err(e) => {
                eprintln!("{} {} error: {}", "✗".red(), task_id.to_string().red(), e);
                any_failed.store(true, Ordering::SeqCst);
                let _ = done_tx.send(false);
            }
        }
    });
}

/// Resolve which tasks to run: requested tasks + all their dependencies.
fn resolve_tasks_to_run(
    task_graph: &TaskGraph,
    requested_tasks: &[String],
) -> Result<HashSet<TaskId>> {
    let mut tasks_to_run = HashSet::new();
    let mut to_process = seed_requested_tasks(task_graph, requested_tasks)?;

    // Walk dependencies
    while let Some(task_id) = to_process.pop() {
        if !tasks_to_run.insert(task_id.clone()) {
            continue;
        }
        push_dependencies(task_graph, &task_id, &mut to_process);
    }

    Ok(tasks_to_run)
}

/// Collects the task IDs of every node whose task name was requested.
///
/// Errors if a requested name matches no node in the pipeline.
fn seed_requested_tasks(task_graph: &TaskGraph, requested_tasks: &[String]) -> Result<Vec<TaskId>> {
    let mut seed = Vec::new();
    for task_name in requested_tasks {
        let matching: Vec<TaskId> = task_graph
            .as_graph()
            .node_weights()
            .filter(|node| node.id.task.as_str() == task_name)
            .map(|node| node.id.clone())
            .collect();

        if matching.is_empty() {
            bail!("task '{}' not found in pipeline", task_name);
        }
        seed.extend(matching);
    }
    Ok(seed)
}

/// Pushes the direct dependency task IDs of `task_id` onto `to_process`.
fn push_dependencies(task_graph: &TaskGraph, task_id: &TaskId, to_process: &mut Vec<TaskId>) {
    let Some(&node_idx) = task_graph.indices_by_id.get(task_id) else {
        return;
    };
    for neighbor in task_graph
        .graph
        .neighbors_directed(node_idx, Direction::Outgoing)
    {
        to_process.push(task_graph.graph[neighbor].id.clone());
    }
}

/// Compute aligned prefix width from the tasks being run.
fn compute_prefix_width(task_graph: &TaskGraph, tasks_to_run: &HashSet<TaskId>) -> usize {
    task_graph
        .as_graph()
        .node_weights()
        .filter(|node| tasks_to_run.contains(&node.id))
        .map(|node| node.id.to_string().len())
        .max()
        .unwrap_or(0)
}

/// Build command map for each task in the graph.
fn build_command_map(
    task_graph: &TaskGraph,
    packages: &[PackageNode],
    pipeline: &HashMap<TaskName, TaskDefinition>,
    workspace_root: &Path,
    workers: &HashMap<String, WorkerDefinition>,
) -> Result<HashMap<TaskId, ExecutionRequest>> {
    let mut commands = HashMap::new();

    for node in task_graph.as_graph().node_weights() {
        let task_id = &node.id;

        // Find the TaskDefinition for this task name
        let task_def = pipeline.get(&task_id.task);

        // Find the package node for this task's package
        let package = packages.iter().find(|p| p.name == task_id.package);

        // Resolve command
        let command = resolve_command(task_def, &task_id.task, package)?;

        if let Some(cmd) = command {
            let cwd = package
                .map(|p| p.path.clone())
                .unwrap_or_else(|| workspace_root.to_path_buf());

            let worker = task_def.and_then(|def| def.worker.clone());
            if let Some(ref name) = worker {
                if !workers.contains_key(name) {
                    bail!("task {} references undefined worker '{}'", task_id, name);
                }
            }

            let request = ExecutionRequest {
                task: node.clone(),
                command: cmd,
                cwd: Some(cwd),
                env: HashMap::new(),
                worker,
            };
            commands.insert(task_id.clone(), request);
        }
    }

    Ok(commands)
}

/// Resolve command for a task: explicit command in config, or package.json scripts.
fn resolve_command(
    task_def: Option<&TaskDefinition>,
    task_name: &TaskName,
    package: Option<&PackageNode>,
) -> Result<Option<String>> {
    // Priority 1: explicit command in TaskDefinition.
    if let Some(cmd) = task_def.and_then(|def| def.command.clone()) {
        return Ok(Some(cmd));
    }

    // Priority 2: look up in package.json scripts.
    let Some(pkg) = package else {
        return Ok(None);
    };
    script_from_package_json(&pkg.path, task_name)
}

/// Looks up `task_name` in the `scripts` map of `<package_dir>/package.json`.
///
/// A missing `package.json` is a legitimate no-op (`Ok(None)`); an unreadable or
/// malformed manifest is an error.
fn script_from_package_json(package_dir: &Path, task_name: &TaskName) -> Result<Option<String>> {
    let pkg_json_path = package_dir.join("package.json");

    let contents = match fs::read_to_string(&pkg_json_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).into_diagnostic().wrap_err_with(|| {
                format!("Failed to read package.json at {}", pkg_json_path.display())
            });
        }
    };

    let pkg_json: PackageJson = serde_json::from_str(&contents).map_err(|e| {
        miette::miette!(
            "Failed to parse package.json at {}: {}",
            pkg_json_path.display(),
            e
        )
    })?;

    let command = pkg_json
        .scripts
        .as_ref()
        .and_then(|scripts| scripts.get(task_name.as_str()))
        .cloned();
    Ok(command)
}
