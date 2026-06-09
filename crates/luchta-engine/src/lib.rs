pub mod executor;
pub mod task_graph;
pub mod walker;

use luchta_types::TaskId;
use luchta_workspace::WorkspaceError;
use thiserror::Error;

pub use executor::{ExecutorError, TaskExecutor, WeightedExecutor};
pub use task_graph::{TaskGraph, TaskNode};
pub use walker::{CompletionSignal, ReadyTaskMessage, Walker};

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("executor error: {0}")]
    Executor(#[from] ExecutorError),
    #[error("workspace error: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("task graph cycle detected at {task}")]
    TaskGraphCycle { task: TaskId },
}
