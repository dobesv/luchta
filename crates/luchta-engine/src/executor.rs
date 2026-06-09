use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    process::ExitStatus,
    sync::{Arc, Mutex},
};

use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::Semaphore,
};

use crate::task_graph::TaskNode;

#[derive(Debug, Clone)]
pub struct ExecutionRequest {
    pub task: TaskNode,
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
}

impl ExecutionRequest {
    pub fn new(task: TaskNode, command: impl Into<String>) -> Self {
        Self {
            task,
            command: command.into(),
            cwd: None,
            env: HashMap::new(),
        }
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
}

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("task {task} has weight {weight}, which exceeds executor max weight {max_weight}")]
    WeightExceedsMax {
        task: String,
        weight: u32,
        max_weight: u32,
    },
    #[error("failed to spawn task {task}: {source}")]
    Spawn {
        task: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to wait for task {task}: {source}")]
    Wait {
        task: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read stdout for task {task}: {source}")]
    StdoutRead {
        task: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read stderr for task {task}: {source}")]
    StderrRead {
        task: String,
        #[source]
        source: std::io::Error,
    },
    #[error("task {task} missing command for execute() seam; use WeightedExecutor::run with ExecutionRequest")]
    MissingCommand { task: String },
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
    commands: Arc<Mutex<HashMap<crate::TaskId, ExecutionRequest>>>,
}

impl WeightedExecutor {
    pub fn new(max_weight: u32) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_weight as usize)),
            max_weight,
            commands: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn semaphore(&self) -> &Arc<Semaphore> {
        &self.semaphore
    }

    pub fn max_weight(&self) -> u32 {
        self.max_weight
    }

    pub fn register(&self, request: ExecutionRequest) {
        self.commands
            .lock()
            .expect("executor commands poisoned")
            .insert(request.task.id.clone(), request);
    }

    pub async fn run(&self, request: &ExecutionRequest) -> Result<ExitStatus, ExecutorError> {
        self.validate_weight(&request.task)?;

        let permit = self
            .semaphore
            .clone()
            .acquire_many_owned(request.task.weight)
            .await
            .expect("executor semaphore closed");

        let task_name = request.task.id.to_string();
        let prefix = format!("[{}]", task_name);

        let mut command = Command::new("sh");
        command.arg("-c").arg(&request.command);
        if let Some(cwd) = &request.cwd {
            command.current_dir(cwd);
        }
        command.envs(request.env.iter());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());

        let mut child = command.spawn().map_err(|source| ExecutorError::Spawn {
            task: task_name.clone(),
            source,
        })?;

        let stdout = child.stdout.take().expect("child stdout piped");
        let stderr = child.stderr.take().expect("child stderr piped");

        let stdout_task_name = task_name.clone();
        let stdout_prefix = prefix.clone();
        let stdout_handle = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) =
                lines
                    .next_line()
                    .await
                    .map_err(|source| ExecutorError::StdoutRead {
                        task: stdout_task_name.clone(),
                        source,
                    })?
            {
                println!("{} {}", stdout_prefix, line);
            }
            Ok::<(), ExecutorError>(())
        });

        let stderr_task_name = task_name.clone();
        let stderr_prefix = prefix.clone();
        let stderr_handle = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(line) =
                lines
                    .next_line()
                    .await
                    .map_err(|source| ExecutorError::StderrRead {
                        task: stderr_task_name.clone(),
                        source,
                    })?
            {
                eprintln!("{} {}", stderr_prefix, line);
            }
            Ok::<(), ExecutorError>(())
        });

        let status = child.wait().await.map_err(|source| ExecutorError::Wait {
            task: task_name.clone(),
            source,
        })?;

        stdout_handle.await.expect("stdout task panicked")?;
        stderr_handle.await.expect("stderr task panicked")?;
        drop(permit);

        Ok(status)
    }

    fn validate_weight(&self, task: &TaskNode) -> Result<(), ExecutorError> {
        if task.weight > self.max_weight {
            eprintln!(
                "warning: task {} weight {} exceeds executor max weight {}",
                task.id, task.weight, self.max_weight
            );
            return Err(ExecutorError::WeightExceedsMax {
                task: task.id.to_string(),
                weight: task.weight,
                max_weight: self.max_weight,
            });
        }

        Ok(())
    }
}

impl TaskExecutor for WeightedExecutor {
    async fn execute(&self, task: &TaskNode) -> Result<ExitStatus, ExecutorError> {
        let request = {
            self.commands
                .lock()
                .expect("executor commands poisoned")
                .get(&task.id)
                .cloned()
        };

        match request {
            Some(request) => self.run(&request).await,
            None => Err(ExecutorError::MissingCommand {
                task: task.id.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicU32, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };

    use luchta_types::{PackageName, TaskId, TaskName};
    use tokio::sync::Barrier;

    use super::*;

    fn task_node(package: &str, task: &str, weight: u32) -> TaskNode {
        TaskNode {
            id: TaskId::new(PackageName::from(package), TaskName::from(task)),
            weight,
        }
    }

    fn success_status() -> ExitStatus {
        std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .status()
            .expect("create success exit status")
    }

    #[derive(Clone)]
    struct MockExecutor {
        calls: Arc<Mutex<Vec<TaskId>>>,
    }

    impl TaskExecutor for MockExecutor {
        async fn execute(&self, task: &TaskNode) -> Result<ExitStatus, ExecutorError> {
            self.calls
                .lock()
                .expect("calls poisoned")
                .push(task.id.clone());
            Ok(success_status())
        }
    }

    #[tokio::test]
    async fn task_executor_trait_is_mockable() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let executor = MockExecutor {
            calls: Arc::clone(&calls),
        };
        let task = task_node("pkg", "build", 1);

        let status = executor
            .execute(&task)
            .await
            .expect("mock execute succeeds");

        assert!(status.success());
        assert_eq!(calls.lock().expect("calls poisoned").as_slice(), &[task.id]);
    }

    #[tokio::test]
    async fn run_enforces_total_weight_and_rejects_oversized_tasks() {
        let executor = Arc::new(WeightedExecutor::new(4));
        let active_weight = Arc::new(AtomicU32::new(0));
        let max_observed = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(Barrier::new(2));

        let run_weighted_task = |task: TaskNode| {
            let executor = Arc::clone(&executor);
            let active_weight = Arc::clone(&active_weight);
            let max_observed = Arc::clone(&max_observed);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                let _permit = executor
                    .semaphore()
                    .clone()
                    .acquire_many_owned(task.weight)
                    .await
                    .expect("acquire permits");

                let current = active_weight.fetch_add(task.weight, Ordering::SeqCst) + task.weight;
                max_observed.fetch_max(current, Ordering::SeqCst);
                assert!(
                    current <= executor.max_weight(),
                    "task {} pushed active weight above limit: {current}",
                    task.id
                );

                barrier.wait().await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                active_weight.fetch_sub(task.weight, Ordering::SeqCst);
            })
        };

        let handle_a = run_weighted_task(task_node("pkg", "a", 2));
        let handle_b = run_weighted_task(task_node("pkg", "b", 2));

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(executor.semaphore().available_permits(), 0);

        handle_a.await.expect("task a joined");
        handle_b.await.expect("task b joined");

        assert_eq!(max_observed.load(Ordering::SeqCst), 4);
        assert_eq!(active_weight.load(Ordering::SeqCst), 0);
        assert_eq!(executor.semaphore().available_permits(), 4);

        let oversized = task_node("pkg", "too-big", 5);
        let err = executor
            .run(&ExecutionRequest::new(oversized.clone(), "echo nope"))
            .await
            .expect_err("oversized task rejected");
        assert!(matches!(
            err,
            ExecutorError::WeightExceedsMax {
                task,
                weight: 5,
                max_weight: 4
            } if task == oversized.id.to_string()
        ));
    }

    #[tokio::test]
    async fn run_spawns_real_command() {
        let executor = WeightedExecutor::new(2);
        let request = ExecutionRequest::new(task_node("pkg", "echo", 1), "echo hello");

        let status = executor.run(&request).await.expect("real command succeeds");

        assert!(status.success());
        assert_eq!(executor.semaphore().available_permits(), 2);
    }
}
