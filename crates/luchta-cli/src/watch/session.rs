//! Watch session: long-lived holder of WorkerManager for repeated build cycles.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use miette::Result;
use tokio_util::sync::CancellationToken;

use crate::run::{run_cycle, CycleOutcome, RunContext, RunCycleParams};
use crate::watch::registry::{retain_task_watch_registry_task_ids, TaskWatchRegistry};
use luchta_engine::{PackageResolveInfo, ResolveMode, TaskGraph, WorkerManager};
use luchta_types::TaskName;
use luchta_workspace::{PackageGraph, PackageNode, WorkspaceDiscovery, YarnWorkspace};

/// Long-lived session for watch mode.
///
/// Owns prepared `RunContext` (graphs, config, workers) and can execute
/// multiple build cycles without respawning workers between cycles.
pub struct WatchSession {
    run: ArcSwap<RunContext>,
    max_weight_override: Option<u32>,
    #[cfg(test)]
    rebuild_generation: AtomicU64,
}

impl WatchSession {
    /// Create a new watch session.
    ///
    /// Prepares workspace (packages, graphs, workers) without `--since`
    /// resolution. Returns `None` for empty workspace (message printed).
    pub async fn new(
        workspace_root: &Path,
        max_weight_override: Option<u32>,
    ) -> Result<Option<Self>> {
        let mut run =
            match crate::run::prepare_session_context(workspace_root, max_weight_override).await? {
                Some(r) => r,
                None => return Ok(None),
            };
        run.owns_worker_manager = false;
        Ok(Some(Self {
            run: ArcSwap::from_pointee(run),
            max_weight_override,
            #[cfg(test)]
            rebuild_generation: AtomicU64::new(0),
        }))
    }

    fn run_context(&self) -> Arc<RunContext> {
        self.run.load_full()
    }

    /// Rebuild graph-backed run context for discovered packages and atomically swap it in.
    /// Reuses existing `WorkerManager` so watch workers stay alive across structural changes.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn rebuild_for_packages(
        &self,
        discovered_package_paths: &BTreeSet<PathBuf>,
    ) -> Result<()> {
        let current = self.run_context();
        let config = crate::config::load_config(&current.workspace_root).await?;
        let packages =
            discover_packages_for_paths(&current.workspace_root, discovered_package_paths)?;
        let PreparedRunContext {
            package_nodes,
            package_graph,
            env,
            task_graph,
            workers,
            max_weight,
            pruned,
            worker_manager,
            global_cache_nonce,
            workspace_root,
        } = build_reused_worker_run_context(ReusedContextParams {
            workspace_root: &current.workspace_root,
            packages,
            max_weight_override: self.max_weight_override,
            worker_manager: Arc::clone(&current.worker_manager),
            config,
        })
        .await?;
        let task_watch_registry = Arc::clone(&current.task_watch_registry);
        let live_task_ids = task_graph
            .nodes()
            .map(|node| node.id.clone())
            .collect::<HashSet<_>>();
        retain_task_watch_registry_task_ids(&task_watch_registry, &live_task_ids);
        self.run.store(Arc::new(RunContext {
            package_nodes,
            package_graph,
            env,
            task_graph,
            workers,
            max_weight,
            pruned,
            worker_manager,
            owns_worker_manager: false,
            since_affected: None,
            global_cache_nonce,
            workspace_root,
            task_watch_registry,
        }));
        #[cfg(test)]
        self.rebuild_generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Get clone of WorkerManager Arc for identity comparison.
    #[cfg(test)]
    pub fn worker_manager_handle(&self) -> Arc<WorkerManager> {
        let run = self.run_context();
        Arc::clone(&run.worker_manager)
    }

    #[cfg(test)]
    pub(crate) fn run_context_for_test(&self) -> Arc<RunContext> {
        self.run_context()
    }

    #[cfg(test)]
    pub(crate) fn rebuild_generation(&self) -> u64 {
        self.rebuild_generation.load(Ordering::Relaxed)
    }

    /// Repo root used for absolute-path -> package mapping.
    pub(crate) fn repo_root(&self) -> Arc<PathBuf> {
        Arc::new(self.run_context().workspace_root.clone())
    }

    pub(crate) fn task_watch_registry(&self) -> TaskWatchRegistry {
        Arc::clone(&self.run_context().task_watch_registry)
    }

    pub(crate) fn current_package_paths(&self) -> BTreeSet<PathBuf> {
        self.run_context()
            .package_nodes
            .iter()
            .map(|package| package.path.clone())
            .collect()
    }

    pub(crate) fn current_package_nodes(&self) -> Vec<PackageNode> {
        self.run_context().package_nodes.clone()
    }

    pub(crate) fn current_package_graph(&self) -> Arc<PackageGraph> {
        Arc::new(self.run_context().package_graph.clone())
    }

    /// Test-only hook for checking whether manager was shut down.
    #[cfg(test)]
    pub fn worker_manager_is_shutdown(&self) -> bool {
        self.run_context().worker_manager.is_shutdown()
    }

    /// Execute one build cycle.
    ///
    /// Delegates to `run_cycle` without shutting down workers.
    /// `cancel_token` can be used to abort mid-cycle (non-terminal;
    /// workers stay alive for next cycle).
    pub async fn run_cycle(
        &self,
        params: RunCycleParams<'_>,
        cancel: CancellationToken,
    ) -> Result<CycleOutcome> {
        let run = self.run_context();
        let (outcome, _was_interrupted) = run_cycle(&run, params, cancel).await?;
        Ok(outcome)
    }

    /// Gracefully shut down worker manager.
    pub async fn shutdown(&self) {
        self.run_context().worker_manager.shutdown().await;
    }

    /// Immediately shut down (for interrupt path).
    pub async fn shutdown_immediate(&self) {
        self.run_context().worker_manager.shutdown_immediate().await;
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct PreparedRunContext {
    package_nodes: Vec<PackageNode>,
    package_graph: luchta_workspace::PackageGraph,
    env: std::collections::BTreeMap<String, luchta_types::EnvSpec>,
    task_graph: luchta_engine::TaskGraph,
    workers: std::collections::HashMap<String, luchta_types::WorkerDefinition>,
    max_weight: u32,
    pruned: Vec<luchta_engine::PrunedTask>,
    worker_manager: Arc<WorkerManager>,
    global_cache_nonce: Option<String>,
    workspace_root: PathBuf,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ReusedContextParams<'a> {
    workspace_root: &'a Path,
    packages: Vec<PackageNode>,
    max_weight_override: Option<u32>,
    worker_manager: Arc<WorkerManager>,
    config: crate::config::LuchtaConfig,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn build_reused_worker_run_context(
    params: ReusedContextParams<'_>,
) -> Result<PreparedRunContext> {
    let ReusedContextParams {
        workspace_root,
        packages,
        max_weight_override,
        worker_manager,
        config,
    } = params;
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
    let pipeline = config
        .tasks
        .into_iter()
        .map(|(name, definition)| (TaskName::from(name), definition))
        .collect::<std::collections::HashMap<_, _>>();
    let resolve_info = PackageResolveInfo::map_from_packages_with_root(&packages, workspace_root);
    let (task_graph, pruned) = TaskGraph::build_resolved(
        &package_graph,
        &pipeline,
        &resolve_info,
        &config.workers,
        worker_manager.as_ref(),
        ResolveMode::Run,
    )
    .await
    .map_err(|error| miette::miette!("failed to build task graph: {}", error))?;

    Ok(PreparedRunContext {
        package_nodes: packages,
        package_graph,
        env: config.env,
        task_graph,
        workers: config.workers,
        max_weight: max_weight_override.unwrap_or(config.concurrency.max_weight),
        pruned,
        worker_manager,
        global_cache_nonce: config.cache.and_then(|cache| cache.cache_nonce),
        workspace_root: workspace_root.to_owned(),
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn discover_packages_for_paths(
    workspace_root: &Path,
    discovered_package_paths: &BTreeSet<PathBuf>,
) -> Result<Vec<PackageNode>> {
    let discovered = YarnWorkspace::new(workspace_root)
        .discover()
        .map_err(|error| miette::miette!("workspace discovery failed: {}", error))?;
    Ok(discovered
        .into_iter()
        .filter(|package| discovered_package_paths.contains(&package.path))
        .collect())
}
