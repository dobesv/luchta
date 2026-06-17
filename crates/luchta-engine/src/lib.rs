pub mod executor;
pub mod input_expansion;
pub mod task_graph;
pub mod walker;
pub mod worker;

use luchta_types::TaskId;
use luchta_workspace::WorkspaceError;
use thiserror::Error;

pub use executor::{
    ExecutionLogSink, ExecutionRequest, ExecutorError, TaskExecutor, TaskRunOutcome,
    WeightedExecutor,
};
pub use input_expansion::{expand_input_patterns, InputExpansionError};
pub use luchta_worker::{
    CapturedLogLine, LogStream, ResolveDecision, ResolveMode, ResolveResult, ResolveTask,
    TaskModification, WorkerMessage, WorkerRequest, WorkerResponse,
};
pub use task_graph::{
    is_root_task, root_package_name, root_task_id, DeadDependencyReason, DependencyValidationError,
    PackageResolveInfo, PruneOutcome, PrunedTask, ResolveError, TaskGraph, TaskNode, TaskResolver,
    TaskValidationDiagnostic, TaskValidationReason, ROOT_PACKAGE_NAME,
};
pub use walker::{CompletionSignal, ReadyTaskMessage, Walker};
pub use worker::manager::{WorkerError, WorkerManager};

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("executor error: {0}")]
    Executor(#[from] ExecutorError),
    #[error("workspace error: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("task graph cycle detected at {task}")]
    TaskGraphCycle { task: TaskId },
    #[error("task resolution error: {0}")]
    Resolve(#[from] task_graph::ResolveError),
}
