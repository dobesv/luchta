pub mod executor;
pub mod task_graph;
pub mod walker;
pub mod worker;

use luchta_types::TaskId;
use luchta_workspace::WorkspaceError;
use thiserror::Error;

pub use executor::{ExecutionRequest, ExecutorError, TaskExecutor, WeightedExecutor};
pub use task_graph::{
    is_root_task, root_package_name, root_task_id, DeadDependencyReason, DependencyValidationError,
    TaskGraph, TaskNode, TaskValidationDiagnostic, TaskValidationReason, ROOT_PACKAGE_NAME,
};
pub use walker::{CompletionSignal, ReadyTaskMessage, Walker};
pub use worker::manager::{WorkerError, WorkerManager};
pub use worker::protocol::{LogStream, WorkerRequest, WorkerResponse};

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("executor error: {0}")]
    Executor(#[from] ExecutorError),
    #[error("workspace error: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("task graph cycle detected at {task}")]
    TaskGraphCycle { task: TaskId },
}
