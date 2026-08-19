//! Passive task-readiness polling.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

use luchta_cache::{decide, resolve_cache_dir, Cache, Decision, ListingCache, TaskRunRecord};
use luchta_engine::ResolveMode;
use luchta_types::TaskId;
use luchta_workspace::PackageGraph;
use miette::{Context, IntoDiagnostic, Result};

use crate::{
    build_lock,
    cache_ctx::{load_lockfile_state, LockfileState},
    live_cache_state::{build_live_task_state, LiveCacheContext},
    run::{
        analyze_tasks, collect_requested_subgraph, prepare_workspace, CollectSubgraphRequest,
        PreparedWorkspace, TaskAnalysis, TaskSelection,
    },
};

const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) struct AwaitOptions<'a> {
    pub(crate) tasks: &'a [String],
    pub(crate) packages: &'a [String],
    pub(crate) top_level: bool,
}

struct AwaitContext {
    prepared: PreparedWorkspace,
    package_graph: Arc<PackageGraph>,
    cache: Cache,
    task_envs: HashMap<TaskId, BTreeMap<String, luchta_types::EnvSpec>>,
    lockfile_state: LockfileState,
    ordered_tasks: Vec<TaskId>,
}

/// Wait until another build process has made the complete selected subgraph
/// current. This command never dispatches work and never consults the shared
/// cache.
pub(crate) async fn execute_await(workspace_root: &Path, options: &AwaitOptions<'_>) -> Result<()> {
    let prepared = prepare_workspace(workspace_root, ResolveMode::Run, None).await?;
    prepared.worker_manager.shutdown().await;

    let selection = TaskSelection {
        requested_tasks: options.tasks,
        packages: options.packages,
        top_level: options.top_level,
        since: None,
    };
    let selected_tasks = collect_requested_subgraph(CollectSubgraphRequest {
        task_graph: &prepared.task_graph,
        selection: &selection,
        pruned: &prepared.pruned,
        since_affected: None,
        expand_dependencies: true,
    })?;
    let ordered_tasks = selected_topological_order(&prepared, &selected_tasks)?;
    let TaskAnalysis { invalid, task_envs } = analyze_tasks(&prepared, workspace_root);
    fail_for_invalid_tasks(&ordered_tasks, &invalid)?;

    let cache_dir = resolve_cache_dir(workspace_root);
    let cache =
        Cache::open(&cache_dir).map_err(|error| miette::miette!("cache open failed: {error}"))?;
    let package_graph = Arc::new(prepared.package_graph.clone());
    let context = AwaitContext {
        prepared,
        package_graph,
        cache,
        task_envs,
        lockfile_state: load_lockfile_state(workspace_root),
        ordered_tasks,
    };

    poll_until_ready_or_cancel(workspace_root, &cache_dir, &context).await
}

async fn poll_until_ready_or_cancel(
    workspace_root: &Path,
    cache_dir: &Path,
    context: &AwaitContext,
) -> Result<()> {
    #[cfg(unix)]
    let mut cancellation =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .into_diagnostic()
            .wrap_err("failed to install Ctrl-C handler")?;
    let polling = poll_until_ready(workspace_root, cache_dir, context);
    tokio::pin!(polling);

    #[cfg(unix)]
    tokio::select! {
        biased;
        _ = cancellation.recv() => Ok(()),
        result = &mut polling => result,
    }

    #[cfg(not(unix))]
    {
        let cancellation = tokio::signal::ctrl_c();
        tokio::pin!(cancellation);
        tokio::select! {
            biased;
            result = &mut cancellation => {
            result
                .into_diagnostic()
                .wrap_err("failed to install Ctrl-C handler")?;
            Ok(())
            },
            result = &mut polling => result,
        }
    }
}

fn selected_topological_order(
    prepared: &PreparedWorkspace,
    selected_tasks: &HashSet<TaskId>,
) -> Result<Vec<TaskId>> {
    prepared
        .task_graph
        .topological_order()
        .map_err(|error| miette::miette!("failed to order selected tasks: {error}"))
        .map(|ordered| {
            ordered
                .into_iter()
                .filter(|task| selected_tasks.contains(&task.id))
                .map(|task| task.id.clone())
                .collect()
        })
}

fn fail_for_invalid_tasks(
    ordered_tasks: &[TaskId],
    invalid: &HashMap<TaskId, String>,
) -> Result<()> {
    let messages: Vec<&str> = ordered_tasks
        .iter()
        .filter_map(|task_id| invalid.get(task_id).map(String::as_str))
        .collect();
    if messages.is_empty() {
        Ok(())
    } else {
        Err(miette::miette!("{}", messages.join("\n")))
    }
}

async fn poll_until_ready(
    workspace_root: &Path,
    cache_dir: &Path,
    context: &AwaitContext,
) -> Result<()> {
    let mut announced_wait = false;

    loop {
        let Some(build_lock) = build_lock::acquire_quiet(cache_dir).await? else {
            return Ok(());
        };
        let ready = selected_subgraph_is_ready(workspace_root, context)?;
        drop(build_lock);

        if ready {
            println!("All awaited tasks are current.");
            return Ok(());
        }
        if !announced_wait {
            println!("Waiting for tasks to become current ...");
            announced_wait = true;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn selected_subgraph_is_ready(workspace_root: &Path, context: &AwaitContext) -> Result<bool> {
    let listing_cache = Arc::new(ListingCache::default());
    let live_context = LiveCacheContext {
        prepared: &context.prepared,
        package_graph: &context.package_graph,
        cache: &context.cache,
        task_envs: &context.task_envs,
        lockfile_state: &context.lockfile_state,
        workspace_root,
        listing_cache,
    };
    let mut ready_tasks = HashSet::with_capacity(context.ordered_tasks.len());

    for task_id in &context.ordered_tasks {
        let dependencies_ready = context
            .prepared
            .task_graph
            .dependencies_of(task_id)
            .into_iter()
            .all(|dependency| ready_tasks.contains(&dependency.id));
        if !dependencies_ready {
            return Ok(false);
        }

        let Some(task_definition) = context.prepared.task_graph.task_definition(task_id) else {
            return Err(miette::miette!(
                "cannot inspect readiness for task '{task_id}': task definition is missing"
            ));
        };
        let ready = if !task_definition.counts_in_progress() {
            true
        } else if task_definition.cache_enabled() {
            cacheable_task_is_ready(task_id, context, &live_context)?
        } else {
            non_cacheable_task_is_ready(task_id, context)
        };
        if !ready {
            return Ok(false);
        }
        ready_tasks.insert(task_id.clone());
    }

    Ok(true)
}

fn cacheable_task_is_ready(
    task_id: &TaskId,
    context: &AwaitContext,
    live_context: &LiveCacheContext<'_>,
) -> Result<bool> {
    let prior = context.cache.read(&task_id.to_string());
    let Some(state) = build_live_task_state(task_id, live_context)
        .wrap_err_with(|| format!("failed to inspect readiness for task '{task_id}'"))?
    else {
        return Err(miette::miette!(
            "cannot inspect readiness for task '{task_id}': package context is missing"
        ));
    };
    let current = state.current_state();
    Ok(decide(prior.as_ref(), &current).action == Decision::Skip)
}

fn non_cacheable_task_is_ready(task_id: &TaskId, context: &AwaitContext) -> bool {
    let Some(prior) = context.cache.read(&task_id.to_string()) else {
        return false;
    };
    if !prior.succeeded {
        return false;
    }

    current_successful_dependency_outputs(task_id, context)
        .is_some_and(|dependency_outputs| prior.dep_outputs == dependency_outputs)
}

fn current_successful_dependency_outputs(
    task_id: &TaskId,
    context: &AwaitContext,
) -> Option<BTreeMap<String, [u8; 32]>> {
    context
        .prepared
        .task_graph
        .dependencies_of(task_id)
        .into_iter()
        .filter(|dependency| {
            context
                .prepared
                .task_graph
                .task_definition(&dependency.id)
                .is_some_and(luchta_types::TaskDefinition::counts_in_progress)
        })
        .try_fold(BTreeMap::new(), |mut outputs, dependency| {
            let record: TaskRunRecord = context.cache.read(&dependency.id.to_string())?;
            if !record.succeeded {
                return None;
            }
            outputs.insert(dependency.id.to_string(), record.outputs_hash);
            Some(outputs)
        })
}
