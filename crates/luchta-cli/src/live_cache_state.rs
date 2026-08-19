//! Shared construction of the live local cache state used by read-only commands.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

use luchta_cache::{Cache, CurrentState, ListingCache};
use luchta_types::{EnvSpec, PackageName, TaskDefinition, TaskId};
use luchta_workspace::{PackageGraph, PackageNode};
use miette::Result;

use crate::{
    cache_ctx::{
        build_current_state, gather_pkg_dep_pairs_filtered, LockfileState, PackageDirResolver,
    },
    cache_nonce::resolve_cache_nonce,
    run::PreparedWorkspace,
};

/// Inputs that are stable for a single read-only cache inspection.
pub(crate) struct LiveCacheContext<'a> {
    pub(crate) prepared: &'a PreparedWorkspace,
    pub(crate) package_graph: &'a Arc<PackageGraph>,
    pub(crate) cache: &'a Cache,
    pub(crate) task_envs: &'a HashMap<TaskId, BTreeMap<String, EnvSpec>>,
    pub(crate) lockfile_state: &'a LockfileState,
    pub(crate) workspace_root: &'a Path,
    pub(crate) listing_cache: Arc<ListingCache>,
}

/// Owned components of a task's current cache state.
///
/// Keeping these components together lets callers create a short-lived
/// [`CurrentState`] without duplicating the hashing and package-resolution
/// rules used by task execution.
pub(crate) struct LiveTaskState {
    task_definition: TaskDefinition,
    merged_env: BTreeMap<String, EnvSpec>,
    dependency_outputs: BTreeMap<String, [u8; 32]>,
    package_dependencies: Vec<(String, String)>,
    resolver: PackageDirResolver,
    nonce: Option<String>,
}

impl LiveTaskState {
    pub(crate) fn current_state(&self) -> CurrentState<'_> {
        build_current_state(
            &self.task_definition,
            &self.merged_env,
            self.dependency_outputs.clone(),
            &self.package_dependencies,
            &self.resolver,
            self.nonce.as_deref(),
        )
    }

    pub(crate) fn resolver(&self) -> &PackageDirResolver {
        &self.resolver
    }
}

/// Build the same live task state used for a normal local cache decision.
///
/// `Ok(None)` means the graph has no definition or package context for the
/// task. Package-dependency resolution errors are returned because execution
/// disables caching in that case; treating an empty dependency set as current
/// would incorrectly report a local hit.
pub(crate) fn build_live_task_state(
    task_id: &TaskId,
    context: &LiveCacheContext<'_>,
) -> Result<Option<LiveTaskState>> {
    let Some(task_definition) = context
        .prepared
        .task_graph
        .task_definition(task_id)
        .cloned()
    else {
        return Ok(None);
    };
    let Some(package_context) = package_context(task_id, context) else {
        return Ok(None);
    };

    let resolver = PackageDirResolver::new(
        package_context.path.clone(),
        context.workspace_root.to_path_buf(),
        package_context.name.clone(),
        Arc::clone(context.package_graph),
        Arc::clone(&context.listing_cache),
    );
    let dependency_outputs =
        dependency_outputs_from_cache(task_id, &context.prepared.task_graph, context.cache);
    let synthetic_package;
    let package = match package_context.node {
        Some(package) => package,
        None => {
            synthetic_package =
                PackageNode::new(package_context.name.clone(), package_context.path.clone());
            &synthetic_package
        }
    };
    let package_dependencies = gather_pkg_dep_pairs_filtered(
        package,
        package_context
            .node
            .is_some()
            .then_some(context.package_graph.as_ref()),
        context.workspace_root,
        context.lockfile_state,
        &task_definition.dependencies,
    )?;
    let merged_env = context.task_envs.get(task_id).cloned().unwrap_or_default();
    let nonce = resolve_task_nonce(&task_definition, context.prepared);

    Ok(Some(LiveTaskState {
        task_definition,
        merged_env,
        dependency_outputs,
        package_dependencies,
        resolver,
        nonce,
    }))
}

/// Read successful dependency output hashes using execution's counted-task
/// semantics. Ordering-only connectors can retain stale records from older
/// configurations, but those records never contribute to a live task state.
pub(crate) fn dependency_outputs_from_cache(
    task_id: &TaskId,
    task_graph: &luchta_engine::TaskGraph,
    cache: &Cache,
) -> BTreeMap<String, [u8; 32]> {
    task_graph
        .dependencies_of(task_id)
        .into_iter()
        .filter(|dependency| {
            task_graph
                .task_definition(&dependency.id)
                .is_some_and(TaskDefinition::counts_in_progress)
        })
        .filter_map(|dependency| {
            let dependency_id = dependency.id.to_string();
            let record = cache.read(&dependency_id)?;
            Some((dependency_id, record.outputs_hash))
        })
        .collect()
}

struct PackageContext<'a> {
    node: Option<&'a PackageNode>,
    path: PathBuf,
    name: PackageName,
}

fn package_context<'a>(
    task_id: &TaskId,
    context: &'a LiveCacheContext<'_>,
) -> Option<PackageContext<'a>> {
    if task_id.is_root() {
        let node = context
            .prepared
            .packages
            .iter()
            .find(|package| package.path == context.workspace_root);
        return Some(PackageContext {
            node,
            path: context.workspace_root.to_path_buf(),
            name: task_id.package.clone(),
        });
    }

    context
        .prepared
        .packages
        .iter()
        .find(|package| package.name == task_id.package)
        .map(|package| PackageContext {
            node: Some(package),
            path: package.path.clone(),
            name: package.name.clone(),
        })
}

fn resolve_task_nonce(
    task_definition: &TaskDefinition,
    prepared: &PreparedWorkspace,
) -> Option<String> {
    let env_nonce = std::env::var("LUCHTA_CACHE_NONCE").ok();
    let global_nonce = prepared.global_cache_nonce.as_deref();
    let worker_nonce = task_definition
        .worker
        .as_deref()
        .and_then(|worker| prepared.workers.get(worker))
        .and_then(|worker| worker.cache.as_ref())
        .and_then(|cache| cache.cache_nonce.as_deref());
    let task_nonce = task_definition
        .cache
        .as_ref()
        .and_then(|cache| cache.cache_nonce.as_deref());

    resolve_cache_nonce(env_nonce.as_deref(), global_nonce, worker_nonce, task_nonce)
}
