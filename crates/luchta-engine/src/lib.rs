pub mod executor;
pub mod task_graph;
pub mod walker;

use thiserror::Error;

pub use executor::{ExecutorError, TaskExecutor, WeightedExecutor};
pub use task_graph::{TaskGraph, TaskNode};
pub use walker::{CompletionSignal, ReadyTaskMessage, Walker};

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("executor error: {0}")]
    Executor(#[from] ExecutorError),
}
