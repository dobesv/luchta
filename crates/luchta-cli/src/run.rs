//! Run command implementation.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
};

use luchta_engine::{ExecutionRequest, TaskGraph, Walker, WeightedExecutor};
use luchta_types::{TaskDefinition, TaskId, TaskName};
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
        let pkg_json_path = current.join("package.json");
        if let Ok(contents) = fs::read_to_string(&pkg_json_path) {
            if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&contents) {
                if pkg.get("workspaces").is_some() {
                    return Some(current);
                }
            }
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Run the requested tasks.
pub async fn run_tasks(workspace_root: &PathBuf, requested_tasks: &[String]) -> Result<()> {
    // Load config
    let config_path = workspace_root.join("luchta.toml");
    let config = crate::config::load_config(&config_path)
        .wrap_err_with(|| format!("failed to load config from {}", config_path.display()))?;

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

    // Build task graph - convert HashMap<String, TaskDefinition> to HashMap<TaskName, TaskDefinition>
    let pipeline: HashMap<TaskName, _> = config
        .pipeline
        .into_iter()
        .map(|(name, def)| (TaskName::from(name), def))
        .collect();

    let task_graph = TaskGraph::build(&package_graph, &pipeline)
        .map_err(|e| miette::miette!("failed to build task graph: {}", e))?;

    // Determine which tasks to run (requested + their dependency closure)
    let tasks_to_run = resolve_tasks_to_run(&task_graph, requested_tasks)?;

    // Create executor
    let max_weight = config.concurrency.max_weight;
    let executor = Arc::new(WeightedExecutor::new(max_weight));

    // Build command map for each task node
    let commands = build_command_map(&task_graph, &packages, &pipeline, workspace_root)?;

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
        let task_id = task_node.id.clone();

        // Skip if not in our target set
        if !tasks_to_run.contains(&task_id) {
            // Send success to unblock dependents
            let _ = done_tx.send(true);
            continue;
        }

        // Check if we have a command
        let Some(request) = commands.get(&task_id) else {
            // No command - this is a no-op, mark as success
            println!(
                "{} {} (no command, skipping)",
                "○".dimmed(),
                task_id.to_string().dimmed()
            );
            let _ = done_tx.send(true);
            continue;
        };

        // Check if any prior task failed - if so, skip
        if any_failed.load(Ordering::SeqCst) {
            println!(
                "{} {} (skipped due to prior failure)",
                "⊘".yellow(),
                task_id.to_string().yellow()
            );
            let _ = done_tx.send(false);
            continue;
        }

        println!("{} {}", "●".green(), task_id.to_string().green());

        // Execute
        let executor_clone = Arc::clone(&executor);
        let any_failed_clone = Arc::clone(&any_failed);
        let request_clone = request.clone();

        tokio::spawn(async move {
            let result = executor_clone.run(&request_clone).await;
            match result {
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
                    any_failed_clone.store(true, Ordering::SeqCst);
                    let _ = done_tx.send(false);
                }
                Err(e) => {
                    eprintln!("{} {} error: {}", "✗".red(), task_id.to_string().red(), e);
                    any_failed_clone.store(true, Ordering::SeqCst);
                    let _ = done_tx.send(false);
                }
            }
        });
    }

    // Wait for walker to complete
    walker
        .wait()
        .await
        .into_diagnostic()
        .wrap_err("walker task panicked")?;

    // Check final status
    if any_failed.load(Ordering::SeqCst) {
        bail!("one or more tasks failed")
    }

    Ok(())
}

/// Resolve which tasks to run: requested tasks + all their dependencies.
fn resolve_tasks_to_run(
    task_graph: &TaskGraph,
    requested_tasks: &[String],
) -> Result<HashSet<TaskId>> {
    let mut tasks_to_run = HashSet::new();
    let mut to_process: Vec<TaskId> = Vec::new();

    // Find initial task IDs matching requested names
    for task_name in requested_tasks {
        let found_any = task_graph
            .as_graph()
            .node_weights()
            .any(|node| node.id.task.as_str() == task_name);

        if !found_any {
            bail!("task '{}' not found in pipeline", task_name);
        }

        // Add all task IDs matching this name
        for node in task_graph.as_graph().node_weights() {
            if node.id.task.as_str() == task_name {
                to_process.push(node.id.clone());
            }
        }
    }

    // Walk dependencies
    while let Some(task_id) = to_process.pop() {
        if tasks_to_run.contains(&task_id) {
            continue;
        }
        tasks_to_run.insert(task_id.clone());

        // Add dependencies
        if let Some(node_idx) = task_graph.indices_by_id.get(&task_id) {
            let node_idx = *node_idx;
            for neighbor in task_graph
                .graph
                .neighbors_directed(node_idx, Direction::Outgoing)
            {
                let dep_node = &task_graph.graph[neighbor];
                to_process.push(dep_node.id.clone());
            }
        }
    }

    Ok(tasks_to_run)
}

/// Build command map for each task in the graph.
fn build_command_map(
    task_graph: &TaskGraph,
    packages: &[PackageNode],
    pipeline: &HashMap<TaskName, TaskDefinition>,
    workspace_root: &Path,
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
            let request = ExecutionRequest {
                task: node.clone(),
                command: cmd,
                cwd: Some(cwd),
                env: HashMap::new(),
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
    // Priority 1: explicit command in TaskDefinition
    if let Some(def) = task_def {
        if let Some(ref cmd) = def.command {
            return Ok(Some(cmd.clone()));
        }
    }

    // Priority 2: look up in package.json scripts
    if let Some(pkg) = package {
        let pkg_json_path = pkg.path.join("package.json");

        match fs::read_to_string(&pkg_json_path) {
            Ok(contents) => {
                // File read successfully - parse JSON
                match serde_json::from_str::<PackageJson>(&contents) {
                    Ok(pkg_json) => {
                        // Parsed successfully - look for script
                        if let Some(ref scripts) = pkg_json.scripts {
                            if let Some(cmd) = scripts.get(task_name.as_str()) {
                                return Ok(Some(cmd.clone()));
                            }
                        }
                        // No matching script - fall through to Ok(None)
                    }
                    Err(e) => {
                        // Malformed JSON - this is an error, not a no-op
                        bail!(
                            "Failed to parse package.json at {}: {}",
                            pkg_json_path.display(),
                            e
                        );
                    }
                }
            }
            Err(e) => {
                // File read failed - distinguish NotFound from other errors
                if e.kind() != std::io::ErrorKind::NotFound {
                    // Permission error, I/O error, etc. - this is an error
                    return Err(e).into_diagnostic().wrap_err_with(|| {
                        format!("Failed to read package.json at {}", pkg_json_path.display())
                    });
                }
                // NotFound is fine - package legitimately may have no package.json
                // Fall through to Ok(None)
            }
        }
    }

    // No command found - this is a no-op task
    Ok(None)
}
