//! Why command implementation.
//!
//! Explains why a task would run or skip. For each matched pkg×task, prints three
//! distinct facts:
//! 1. If PRUNED, the live prune reason.
//! 2. The persisted run_reason from the prior record ("last ran").
//! 3. A LIVE decide() result ("what would happen now").

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use luchta_cache::{
    decide, files_diff, resolve_cache_dir, Cache, CurrentState, Decision, FileEntry,
    FileStateResolver, ListingCache, DECIDE_FILES_DIFF_LIMIT,
};
use luchta_engine::ResolveMode;
use luchta_types::{EnvSpec, PackageName, TaskDefinition, TaskId};
use luchta_workspace::PackageNode;
use miette::Result;

use crate::cache_ctx::{
    build_current_state, gather_pkg_dep_pairs_filtered, load_lockfile_state, PackageDirResolver,
};
use crate::cache_nonce::resolve_cache_nonce;
use crate::env_merge::merge_env;
use crate::format::package_and_task_display;
use crate::run::{
    build_globset, collect_matched_package_names, collect_requested_subgraph, prepare_workspace,
    CollectSubgraphRequest, PreparedWorkspace, TaskSelection,
};
use luchta_engine::{PrunedTask, TaskGraph};

pub(crate) struct WhyOptions<'a> {
    pub tasks: &'a [String],
    pub packages: &'a [String],
    pub top_level: bool,
    pub show_inputs: bool,
    pub show_outputs: bool,
}

/// Context struct for print_why_for_task, grouping related parameters.
struct WhyContext<'a> {
    prepared: &'a PreparedWorkspace,
    cache: &'a Cache,
    prune_reasons: &'a HashMap<TaskId, String>,
    task_envs: &'a HashMap<TaskId, BTreeMap<String, EnvSpec>>,
    lockfile_state: &'a crate::cache_ctx::LockfileState,
    workspace_root: &'a Path,
    show_inputs: bool,
    show_outputs: bool,
}

/// Execute the `luchta why` command.
pub async fn execute_why(workspace_root: &Path, options: &WhyOptions<'_>) -> Result<()> {
    let prepared = prepare_workspace(workspace_root, ResolveMode::Run, None).await?;
    prepared.worker_manager.shutdown().await;

    let selection = TaskSelection {
        requested_tasks: options.tasks,
        packages: options.packages,
        top_level: options.top_level,
        since: None,
    };

    // Build a map from TaskId -> prune reason from prepared.pruned (both Pruned and Rejected)
    let prune_reasons: HashMap<TaskId, String> = prepared
        .pruned
        .iter()
        .map(|p| (p.task_id.clone(), p.outcome.describe()))
        .collect();

    // Identify all task IDs we'd consider for the "now" decision
    let all_task_ids: HashSet<TaskId> = prepared.task_graph.nodes().map(|n| n.id.clone()).collect();

    // Determine which task IDs match the selection
    let selected_ids = select_task_ids(
        &prepared.task_graph,
        &selection,
        &prepared.pruned,
        options.top_level,
    )?;

    // Open cache
    let cache_dir = resolve_cache_dir(workspace_root);
    let cache = Cache::open(&cache_dir).map_err(|e| miette::miette!("cache open failed: {}", e))?;

    // Build lockfile state once
    let lockfile_state = load_lockfile_state(workspace_root);

    // Sort for deterministic output
    let mut sorted_ids: Vec<TaskId> = selected_ids.into_iter().collect();
    sorted_ids.sort_by_key(|id| id.to_string());

    // Build env map: task_id -> merged env (global + worker + task-specific)
    let task_envs = build_task_envs(&prepared, &all_task_ids);

    let ctx = WhyContext {
        prepared: &prepared,
        cache: &cache,
        prune_reasons: &prune_reasons,
        task_envs: &task_envs,
        lockfile_state: &lockfile_state,
        workspace_root,
        show_inputs: options.show_inputs,
        show_outputs: options.show_outputs,
    };

    // For each selected task, print the three facts
    for task_id in &sorted_ids {
        print_why_for_task(task_id, &ctx);
    }

    Ok(())
}

fn select_task_ids(
    task_graph: &TaskGraph,
    selection: &TaskSelection<'_>,
    pruned: &[PrunedTask],
    _top_level: bool,
) -> Result<HashSet<TaskId>> {
    // Default scope: all tasks
    if is_selection_empty(selection) {
        let mut ids: HashSet<TaskId> = task_graph.nodes().map(|node| node.id.clone()).collect();
        // Include pruned tasks so their prune reasons are shown
        ids.extend(pruned.iter().map(|p| p.task_id.clone()));
        return Ok(ids);
    }

    // Top-level only
    if is_top_level_only(selection) {
        let mut ids: HashSet<TaskId> = task_graph
            .nodes()
            .filter(|node| node.id.is_root())
            .map(|node| node.id.clone())
            .collect();
        // Include pruned root tasks
        ids.extend(
            pruned
                .iter()
                .filter(|p| p.task_id.is_root())
                .map(|p| p.task_id.clone()),
        );
        return Ok(ids);
    }

    // Packages without tasks - delegate to select_by_packages (now handles pruned)
    if is_packages_only(selection) {
        let ids = select_by_packages(task_graph, selection, pruned)?;
        return Ok(ids);
    }

    let mut ids = collect_requested_subgraph(CollectSubgraphRequest {
        task_graph,
        selection,
        pruned,
        since_affected: None,
        expand_dependencies: false,
    })?;
    ids.extend(matching_pruned_ids(task_graph, selection, pruned)?);
    Ok(ids)
}

fn matching_pruned_ids(
    task_graph: &TaskGraph,
    selection: &TaskSelection<'_>,
    pruned: &[PrunedTask],
) -> Result<HashSet<TaskId>> {
    let package_globs = build_globset(selection.packages)?;
    let task_globs = build_globset(selection.requested_tasks)?;
    let matched_packages =
        matched_package_names_for_selection(task_graph, selection, pruned, &package_globs);

    Ok(pruned
        .iter()
        .filter(|pruned_task| {
            matches_pruned_selection(pruned_task, selection, &matched_packages, &task_globs)
        })
        .map(|pruned_task| pruned_task.task_id.clone())
        .collect())
}

fn matched_package_names_for_selection(
    task_graph: &TaskGraph,
    selection: &TaskSelection<'_>,
    pruned: &[PrunedTask],
    package_globs: &globset::GlobSet,
) -> HashSet<PackageName> {
    let available_nodes: Vec<_> = task_graph.nodes().collect();
    let matched_package_names =
        collect_matched_package_names(&available_nodes, selection.packages, package_globs);
    let pruned_package_names = pruned
        .iter()
        .filter(|pruned_task| !pruned_task.task_id.is_root())
        .filter(|pruned_task| package_globs.is_match(pruned_task.task_id.package.as_str()))
        .map(|pruned_task| pruned_task.task_id.package.clone());

    matched_package_names
        .into_iter()
        .chain(pruned_package_names)
        .collect()
}

fn matches_pruned_selection(
    pruned_task: &PrunedTask,
    selection: &TaskSelection<'_>,
    matched_packages: &HashSet<PackageName>,
    task_globs: &globset::GlobSet,
) -> bool {
    let matches_package =
        selection.packages.is_empty() || matched_packages.contains(&pruned_task.task_id.package);
    let matches_task = selection.requested_tasks.is_empty()
        || task_globs.is_match(pruned_task.task_id.task.as_str());
    matches_package && matches_task
}

fn is_selection_empty(selection: &TaskSelection<'_>) -> bool {
    selection.requested_tasks.is_empty() && selection.packages.is_empty() && !selection.top_level
}

fn is_top_level_only(selection: &TaskSelection<'_>) -> bool {
    selection.top_level && selection.requested_tasks.is_empty() && selection.packages.is_empty()
}

fn is_packages_only(selection: &TaskSelection<'_>) -> bool {
    selection.requested_tasks.is_empty() && !selection.packages.is_empty() && !selection.top_level
}

fn select_by_packages(
    task_graph: &TaskGraph,
    selection: &TaskSelection<'_>,
    pruned: &[PrunedTask],
) -> Result<HashSet<TaskId>> {
    let package_globs = build_globset(selection.packages)?;
    let available_nodes: Vec<_> = task_graph.nodes().collect();
    let matched_package_names =
        collect_matched_package_names(&available_nodes, selection.packages, &package_globs);

    // Also match package names from pruned tasks
    let pruned_package_names: HashSet<PackageName> = pruned
        .iter()
        .filter(|p| !p.task_id.is_root())
        .filter(|p| package_globs.is_match(p.task_id.package.as_str()))
        .map(|p| p.task_id.package.clone())
        .collect();
    let all_matched: HashSet<PackageName> = matched_package_names
        .union(&pruned_package_names)
        .cloned()
        .collect();

    if all_matched.is_empty() {
        return Err(miette::miette!(
            "No packages matched: [{}]. -p matches package names, not paths.",
            selection.packages.join(", ")
        ));
    }

    let mut result: HashSet<TaskId> = available_nodes
        .into_iter()
        .filter(|node| !node.id.is_root())
        .filter(|node| all_matched.contains(&node.id.package))
        .map(|node| node.id.clone())
        .collect();

    // Include pruned tasks
    result.extend(
        pruned
            .iter()
            .filter(|p| !p.task_id.is_root())
            .filter(|p| all_matched.contains(&p.task_id.package))
            .map(|p| p.task_id.clone()),
    );

    Ok(result)
}

fn print_why_for_task(task_id: &TaskId, ctx: &WhyContext<'_>) {
    let (package_display, task_display) = package_and_task_display(task_id);
    let task_id_str = task_id.to_string();

    // Header line: package#task
    println!("{}#{}", package_display, task_display);

    // (1) PRUNED check
    if let Some(reason) = ctx.prune_reasons.get(task_id) {
        println!("  pruned: {}", reason);
        return;
    }

    // (2) Check for invalid task (no worker / unknown worker) - must check BEFORE decide()
    // because a task with command but no worker should show as invalid, not as cache hit/miss.
    if let Some(reason) = get_invalid_task_reason(task_id, ctx.prepared) {
        println!("  would run: {}", reason);
        return;
    }

    // (3) "last ran" (persisted history)
    let prior = ctx.cache.read(&task_id_str);
    print_last_ran(&prior);

    // (4) "now" (live would-it-run)
    if let Some(task_def) = ctx.prepared.task_graph.task_definition(task_id) {
        print_live_decision(task_id, ctx, &prior, task_def.clone());
    } else {
        println!("  would run: no task definition found");
    }
}

/// Returns the reason if a task is invalid (no worker or unknown worker).
/// Mirrors the validation logic in dispatch.rs build_command_map.
fn get_invalid_task_reason(task_id: &TaskId, prepared: &PreparedWorkspace) -> Option<String> {
    let task_def = prepared.task_graph.task_definition(task_id)?;
    let worker = task_def.worker.as_deref();

    match worker {
        Some(worker_name) => {
            if !prepared.workers.contains_key(worker_name) {
                Some(format!(
                    "task '{}' references unknown worker '{}'",
                    task_id, worker_name
                ))
            } else {
                None
            }
        }
        None => {
            // Task has no worker - check if it has a command
            if task_def
                .command
                .as_deref()
                .map(|c| !c.trim().is_empty())
                .unwrap_or(false)
            {
                Some(format!(
                    "task '{}' defines a command but no worker; specify a worker to execute it",
                    task_id
                ))
            } else {
                // No worker, no command - not invalid (e.g., synthetic/connector tasks)
                None
            }
        }
    }
}

fn print_last_ran(prior: &Option<luchta_cache::TaskRunRecord>) {
    match prior {
        Some(record) => {
            if let Some(reason) = &record.run_reason {
                println!("  last ran: {}", reason.summary());
            } else {
                println!("  last ran: not recorded");
            }
        }
        None => println!("  last ran: not recorded"),
    }
}

fn print_live_decision(
    task_id: &TaskId,
    ctx: &WhyContext<'_>,
    prior: &Option<luchta_cache::TaskRunRecord>,
    task_def: TaskDefinition,
) {
    let nonce = resolve_task_nonce(&task_def, ctx.prepared);

    let Some((package_path, package_name)) =
        get_package_context(task_id, &ctx.prepared.packages, ctx.workspace_root)
    else {
        println!("  would run: no package context");
        return;
    };

    let resolver = PackageDirResolver::new(
        package_path.clone(),
        ctx.workspace_root.to_path_buf(),
        package_name.clone(),
        ctx.prepared.package_graph.clone(),
        Arc::new(ListingCache::default()),
    );

    let dep_outputs = build_dep_outputs_from_cache(task_id, ctx.prepared, ctx.cache);

    let package = PackageNode::new(package_name.clone(), package_path.clone());
    let pkg_dep_pairs = gather_pkg_dep_pairs_filtered(
        &package,
        Some(&ctx.prepared.package_graph),
        ctx.workspace_root,
        ctx.lockfile_state,
        &task_def.dependencies,
    )
    .unwrap_or_default();

    let empty_env = BTreeMap::new();
    let merged_env = ctx.task_envs.get(task_id).unwrap_or(&empty_env);
    let current = build_current_state(
        &task_def,
        merged_env,
        dep_outputs,
        &pkg_dep_pairs,
        &resolver,
        nonce.as_deref(),
    );

    let decision = decide(prior.as_ref(), &current);
    print_decision(&decision);

    if ctx.show_inputs || ctx.show_outputs {
        print_file_diffs(ctx, prior, &current, &resolver);
    }
}

fn print_decision(decision: &luchta_cache::DecisionResult) {
    match decision.action {
        Decision::Run => println!("  would run: {}", decision.reason.summary()),
        Decision::Skip => println!("  up to date (local cache hit)"),
        Decision::SharedHit => println!("  up to date (shared cache hit)"),
    }
}

fn print_file_diffs(
    ctx: &WhyContext<'_>,
    prior: &Option<luchta_cache::TaskRunRecord>,
    current: &CurrentState<'_>,
    resolver: &PackageDirResolver,
) {
    if let Some(ref record) = prior {
        if ctx.show_inputs {
            print_file_diff(
                "inputs",
                &FileDiffContext {
                    prior_entries: &record.inputs,
                    current,
                    resolver,
                    is_inputs: true,
                },
            );
        }
        if ctx.show_outputs {
            print_file_diff(
                "outputs",
                &FileDiffContext {
                    prior_entries: &record.outputs,
                    current,
                    resolver,
                    is_inputs: false,
                },
            );
        }
    }
}

/// Build dep_outputs map from cached dependency records.
///
/// For each dependency task of the target task, reads the cached record and extracts
/// its outputs_hash. Dependencies without cached records are omitted (they would
/// re-run anyway). Mirrors dependency_output_hashes in dispatch.rs but sources from
/// cache records instead of live output_hashes.
fn build_dep_outputs_from_cache(
    task_id: &TaskId,
    prepared: &PreparedWorkspace,
    cache: &Cache,
) -> BTreeMap<String, [u8; 32]> {
    let deps = prepared.task_graph.dependencies_of(task_id);

    deps.into_iter()
        .filter_map(|dep| {
            let dep_id_str = dep.id.to_string();
            let record = cache.read(&dep_id_str)?;
            Some((dep_id_str, record.outputs_hash))
        })
        .collect()
}

fn resolve_task_nonce(task_def: &TaskDefinition, prepared: &PreparedWorkspace) -> Option<String> {
    let env_nonce = std::env::var("LUCHTA_CACHE_NONCE").ok();
    let global_nonce = prepared.global_cache_nonce.as_deref();
    // Worker nonce: sparse lookup — missing worker or dangling ref yields None
    let worker_nonce = task_def
        .worker
        .as_deref()
        .and_then(|w| prepared.workers.get(w))
        .and_then(|wd| wd.cache.as_ref())
        .and_then(|c| c.cache_nonce.as_deref());
    // Task nonce
    let task_nonce = task_def
        .cache
        .as_ref()
        .and_then(|c| c.cache_nonce.as_deref());

    resolve_cache_nonce(env_nonce.as_deref(), global_nonce, worker_nonce, task_nonce)
}

fn get_package_context(
    task_id: &TaskId,
    packages: &[PackageNode],
    workspace_root: &Path,
) -> Option<(std::path::PathBuf, PackageName)> {
    if task_id.is_root() {
        // Root task: use workspace root as package path (matches dispatch.rs behavior)
        return Some((workspace_root.to_path_buf(), task_id.package.clone()));
    }

    // Find package by name
    packages
        .iter()
        .find(|p| p.name == task_id.package)
        .map(|p| (p.path.clone(), p.name.clone()))
}

fn build_task_envs(
    prepared: &PreparedWorkspace,
    all_task_ids: &HashSet<TaskId>,
) -> HashMap<TaskId, BTreeMap<String, EnvSpec>> {
    let mut task_envs = HashMap::new();

    for task_id in all_task_ids {
        if let Some(task_def) = prepared.task_graph.task_definition(task_id) {
            // Get worker env if task has a worker
            let worker_env = task_def
                .worker
                .as_deref()
                .and_then(|w| prepared.workers.get(w).map(|wd| &wd.env));
            let merged = merge_env(&prepared.env, worker_env, &task_def.env);
            task_envs.insert(task_id.clone(), merged);
        }
    }

    task_envs
}

/// Arguments for file diff output.
struct FileDiffContext<'a> {
    prior_entries: &'a [FileEntry],
    current: &'a CurrentState<'a>,
    resolver: &'a PackageDirResolver,
    is_inputs: bool,
}

/// Print file differences between prior and current state.
fn print_file_diff(label: &str, ctx: &FileDiffContext<'_>) {
    let current_entries = resolve_current_entries(ctx.current, ctx.resolver, ctx.is_inputs);

    let (changed, truncated, count) =
        files_diff(ctx.prior_entries, &current_entries, DECIDE_FILES_DIFF_LIMIT);

    if changed.is_empty() {
        return;
    }

    println!("  changed {}:", label);
    for delta in &changed {
        print_file_delta(delta);
    }

    if truncated {
        println!("    ... and {} more", count as usize - changed.len());
    }
}

fn resolve_current_entries(
    current: &CurrentState<'_>,
    resolver: &PackageDirResolver,
    is_inputs: bool,
) -> Vec<FileEntry> {
    if is_inputs {
        resolver
            .resolve_inputs(current.declared_input_patterns, &[])
            .unwrap_or_default()
    } else {
        resolver
            .resolve_outputs(current.declared_output_patterns, &[])
            .unwrap_or_default()
    }
}

fn print_file_delta(delta: &luchta_cache::FileDelta) {
    if delta.prior_absent && !delta.current_absent {
        println!("    + {} (added)", delta.path);
    } else if !delta.prior_absent && delta.current_absent {
        println!("    - {} (removed)", delta.path);
    } else {
        println!("    ~ {}", delta.path);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder_test_module_exists() {
        // Real tests require workspace setup with cache fixtures
    }
}
