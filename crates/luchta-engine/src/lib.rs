pub mod executor;
pub mod task_graph;
pub mod walker;
pub mod worker;

use luchta_types::{TaskId, TaskName};
use luchta_workspace::WorkspaceError;
use thiserror::Error;

pub use executor::{ExecutionRequest, ExecutorError, TaskExecutor, WeightedExecutor};
pub use task_graph::{TaskGraph, TaskNode};
pub use walker::{CompletionSignal, ReadyTaskMessage, Walker};
pub use worker::manager::{WorkerError, WorkerManager};
pub use worker::protocol::{LogStream, WorkerRequest, WorkerResponse};

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("executor error: {0}")]
    Executor(#[from] ExecutorError),
    #[error("workspace error: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("task graph dependency references unknown pipeline task `{task}` from {from}")]
    UnknownDependencyTask { from: TaskId, task: TaskName },
    #[error("task graph dependency references unknown task node `{target}` from {from}")]
    UnknownDependencyTarget { from: TaskId, target: TaskId },
    #[error("task graph cycle detected at {task}")]
    TaskGraphCycle { task: TaskId },
}
