use std::{future::Future, process::ExitStatus, sync::Arc};

use thiserror::Error;
use tokio::sync::Semaphore;

use crate::task_graph::TaskNode;

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("executor stub: execution not implemented for {task}")]
    NotImplemented { task: String },
}

/// Spawns and awaits a single task, returning its process exit status.
///
/// Desugared from `async fn` to `-> impl Future + Send` so implementors are
/// usable across `tokio` tasks (the future is required to be `Send`).
pub trait TaskExecutor {
    fn execute(
        &self,
        task: &TaskNode,
    ) -> impl Future<Output = Result<ExitStatus, ExecutorError>> + Send;
}

#[derive(Debug, Clone)]
pub struct WeightedExecutor {
    semaphore: Arc<Semaphore>,
    max_weight: u32,
}

impl WeightedExecutor {
    pub fn new(max_weight: u32) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_weight as usize)),
            max_weight,
        }
    }

    pub fn semaphore(&self) -> &Arc<Semaphore> {
        &self.semaphore
    }

    pub fn max_weight(&self) -> u32 {
        self.max_weight
    }
}

impl TaskExecutor for WeightedExecutor {
    async fn execute(&self, task: &TaskNode) -> Result<ExitStatus, ExecutorError> {
        let _ = &self.semaphore;
        let _ = self.max_weight;

        Err(ExecutorError::NotImplemented {
            task: task.id.to_string(),
        })
    }
}
