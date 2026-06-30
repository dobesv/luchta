//! Watch session: long-lived holder of WorkerManager for repeated build cycles.

use std::path::Path;

use luchta_workspace::PackageGraph;
use miette::Result;
use tokio_util::sync::CancellationToken;

use crate::run::{run_cycle, CycleOutcome, RunContext, RunCycleParams};

/// Long-lived session for watch mode.
///
/// Owns a prepared `RunContext` (graphs, config, workers) and can execute
/// multiple build cycles without respawning workers between cycles.
pub struct WatchSession {
    run: RunContext,
}

impl WatchSession {
    /// Create a new watch session.
    ///
    /// Prepares the workspace (packages, graphs, workers) without `--since`
    /// resolution. Returns `None` for empty workspace (message printed).
    pub async fn new(
        workspace_root: &Path,
        max_weight_override: Option<u32>,
    ) -> Result<Option<Self>> {
        let run =
            match crate::run::prepare_session_context(workspace_root, max_weight_override).await? {
                Some(r) => r,
                None => return Ok(None),
            };
        Ok(Some(Self { run }))
    }

    /// Get a clone of the WorkerManager Arc for identity comparison.
    #[cfg(test)]
    pub fn worker_manager_handle(&self) -> std::sync::Arc<luchta_engine::WorkerManager> {
        std::sync::Arc::clone(&self.run.worker_manager)
    }

    /// Repo root used for absolute-path -> package mapping.
    pub(crate) fn repo_root(&self) -> &Path {
        &self.run.workspace_root
    }

    /// Package graph reused across watch cycles.
    pub(crate) fn package_graph(&self) -> &PackageGraph {
        &self.run.package_graph
    }

    /// Test-only hook for checking whether the manager was shut down.
    #[cfg(test)]
    pub fn worker_manager_is_shutdown(&self) -> bool {
        self.run.worker_manager.is_shutdown()
    }

    /// Execute one build cycle.
    ///
    /// Delegates to `run_cycle` without shutting down workers.
    /// The `cancel_token` can be used to abort mid-cycle (non-terminal;
    /// workers stay alive for the next cycle).
    pub async fn run_cycle(
        &self,
        params: RunCycleParams<'_>,
        cancel: CancellationToken,
    ) -> Result<CycleOutcome> {
        let (outcome, _was_interrupted) = run_cycle(&self.run, params, cancel).await?;
        Ok(outcome)
    }

    /// Gracefully shut down the worker manager.
    pub async fn shutdown(&self) {
        self.run.worker_manager.shutdown().await;
    }

    /// Immediately shut down (for interrupt path).
    pub async fn shutdown_immediate(&self) {
        self.run.worker_manager.shutdown_immediate().await;
    }
}
