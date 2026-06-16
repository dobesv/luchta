//! Run setup helpers: building the memory monitor, the execution resources
//! (executor, cache, command map), and resolving the final run outcome.
//!
//! Extracted from `run.rs` to keep that module cohesive.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use luchta_cache::Cache;
use luchta_engine::{ExecutionRequest, TaskGraph, WeightedExecutor, WorkerManager};
use luchta_types::{EnvSpec, TaskId, WorkerDefinition};
use luchta_workspace::PackageNode;
use miette::{bail, Context, IntoDiagnostic, Result};

use super::{
    dispatch::{build_command_map, CommandMap},
    resolve_cache_dir,
};
use crate::progress::ProgressReporter;

/// Resolved memory-pressure thresholds passed to the dispatch loop.
///
/// `None` for either field means "use the default" (50% of total system memory
/// for usage, 1/16 of total for free), resolved by the `MemoryMonitor`.
pub struct MemoryPressureConfig {
    pub usage: Option<crate::memory_pressure::ThresholdSpec>,
    pub free: Option<crate::memory_pressure::ThresholdSpec>,
}

/// Builds the memory monitor and the shared pressure state from the resolved
/// threshold config. The monitor drives pause decisions; the `PressureState` is
/// shared so the status line can render the current warning suffix.
pub(super) fn build_memory_pressure(
    config: MemoryPressureConfig,
) -> (
    crate::memory_pressure::MemoryMonitor,
    Arc<crate::memory_pressure::PressureState>,
) {
    let monitor = crate::memory_pressure::MemoryMonitor::with_specs_for_current_process(
        config.usage,
        config.free,
    );
    (
        monitor,
        Arc::new(crate::memory_pressure::PressureState::new()),
    )
}

/// Resolves the dispatch loop's result into the run's final outcome: propagate
/// interruption, fail if any task failed, otherwise print the success summary.
pub(super) fn report_run_outcome(
    run_result: Result<()>,
    any_failed: &AtomicBool,
    reporter: &ProgressReporter,
) -> Result<()> {
    run_result?;

    if any_failed.load(Ordering::SeqCst) {
        bail!("one or more tasks failed");
    }

    println!("{}", reporter.render_summary());
    Ok(())
}

/// Inputs for [`build_execution_resources`].
pub(super) struct BuildResourcesInputs<'a> {
    pub(super) task_graph: &'a TaskGraph,
    pub(super) packages: &'a [PackageNode],
    pub(super) workspace_root: &'a Path,
    pub(super) workers: &'a HashMap<String, WorkerDefinition>,
    pub(super) env: &'a BTreeMap<String, EnvSpec>,
    pub(super) worker_manager: &'a Arc<WorkerManager>,
    pub(super) max_weight: u32,
    pub(super) prefix_width: usize,
}

/// Execution resources shared across the dispatch loop and task runners.
pub(super) struct ExecutionResources {
    pub(super) executor: Arc<WeightedExecutor>,
    pub(super) cache: Arc<Cache>,
    pub(super) output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    pub(super) commands: HashMap<TaskId, ExecutionRequest>,
    pub(super) invalid: HashMap<TaskId, String>,
    pub(super) task_envs: HashMap<TaskId, BTreeMap<String, EnvSpec>>,
}

/// Builds the executor (with all task commands registered), the build cache,
/// the output-hash map, and the command map for a run.
pub(super) fn build_execution_resources(
    inputs: BuildResourcesInputs<'_>,
) -> Result<ExecutionResources> {
    let executor = Arc::new(
        WeightedExecutor::new(inputs.max_weight)
            .with_worker_manager(Arc::clone(inputs.worker_manager))
            .with_prefix_width(inputs.prefix_width),
    );
    let cache = Arc::new(
        Cache::open(&resolve_cache_dir(inputs.workspace_root))
            .into_diagnostic()
            .wrap_err("open cache")?,
    );
    let output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>> = Arc::new(Mutex::new(HashMap::new()));

    let CommandMap {
        commands,
        invalid,
        task_envs,
    } = build_command_map(
        inputs.task_graph,
        inputs.packages,
        inputs.workspace_root,
        inputs.env,
        inputs.workers,
    );

    for request in commands.values() {
        executor.register(request.clone());
    }

    Ok(ExecutionResources {
        executor,
        cache,
        output_hashes,
        commands,
        invalid,
        task_envs,
    })
}
