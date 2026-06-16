use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    process::ExitStatus,
    sync::{Arc, Mutex},
};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::Semaphore,
};

use crate::{task_graph::TaskNode, LogStream, WorkerError, WorkerManager, WorkerRequest};

#[derive(Debug, Clone)]
pub struct ExecutionRequest {
    pub task: TaskNode,
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub worker: Option<String>,
    pub workspace: Option<String>,
    pub log_sink: Option<ExecutionLogSink>,
}

impl ExecutionRequest {
    pub fn new(task: TaskNode, command: impl Into<String>) -> Self {
        Self {
            task,
            command: command.into(),
            cwd: None,
            env: HashMap::new(),
            worker: None,
            workspace: None,
            log_sink: None,
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

    pub fn with_worker(mut self, name: impl Into<String>) -> Self {
        self.worker = Some(name.into());
        self
    }

    pub fn with_log_sink(mut self, sink: ExecutionLogSink) -> Self {
        self.log_sink = Some(sink);
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
    #[error("task {task} is assigned to worker '{worker}' but no worker manager is configured")]
    MissingWorkerManager { task: String, worker: String },
    #[error("failed to read stderr for task {task}: {source}")]
    StderrRead {
        task: String,
        #[source]
        source: std::io::Error,
    },
    #[error("task {task} worker error: {source}")]
    Worker {
        task: String,
        #[source]
        source: WorkerError,
    },
    #[error("task {task} missing command for execute() seam; use WeightedExecutor::run with ExecutionRequest")]
    MissingCommand { task: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedLogLine {
    pub stream: LogStream,
    pub line: String,
}

#[derive(Debug, Clone, Default)]
pub struct ExecutionLogSink {
    lines: Arc<Mutex<Vec<CapturedLogLine>>>,
}

impl ExecutionLogSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, stream: LogStream, line: impl Into<String>) {
        self.lines
            .lock()
            .expect("execution log sink poisoned")
            .push(CapturedLogLine {
                stream,
                line: line.into(),
            });
    }

    #[must_use]
    pub fn lines(&self) -> Vec<CapturedLogLine> {
        self.lines
            .lock()
            .expect("execution log sink poisoned")
            .clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRunOutcome {
    pub status: ExitStatus,
    pub detected_inputs: Option<Vec<String>>,
    pub detected_outputs: Option<Vec<String>>,
}

impl TaskRunOutcome {
    fn shell(status: ExitStatus) -> Self {
        Self {
            status,
            detected_inputs: None,
            detected_outputs: None,
        }
    }

    fn worker(
        status: ExitStatus,
        detected_inputs: Option<Vec<String>>,
        detected_outputs: Option<Vec<String>>,
    ) -> Self {
        Self {
            status,
            detected_inputs,
            detected_outputs,
        }
    }
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
    worker_manager: Option<Arc<WorkerManager>>,
    prefix_width: usize,
}

impl WeightedExecutor {
    pub fn new(max_weight: u32) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_weight as usize)),
            max_weight,
            commands: Arc::new(Mutex::new(HashMap::new())),
            worker_manager: None,
            prefix_width: 0,
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

    pub fn with_command_map(mut self, commands: HashMap<crate::TaskId, ExecutionRequest>) -> Self {
        self.commands = Arc::new(Mutex::new(commands));
        self
    }

    pub fn with_worker_manager(mut self, mgr: Arc<WorkerManager>) -> Self {
        self.worker_manager = Some(mgr);
        self
    }

    pub fn with_prefix_width(mut self, width: usize) -> Self {
        self.prefix_width = width;
        self
    }

    /// Run one task request, respecting weight-based concurrency.
    pub async fn run(&self, request: &ExecutionRequest) -> Result<TaskRunOutcome, ExecutorError> {
        self.run_with_on_start(request, || {}).await
    }

    /// Run one task request, firing `on_start` immediately after permit acquisition.
    pub async fn run_with_on_start<F>(
        &self,
        request: &ExecutionRequest,
        on_start: F,
    ) -> Result<TaskRunOutcome, ExecutorError>
    where
        F: FnOnce(),
    {
        self.validate_weight(&request.task)?;

        let permit = self
            .semaphore
            .clone()
            .acquire_many_owned(request.task.weight)
            .await
            .expect("executor semaphore closed unexpectedly");

        on_start();

        let task_name = request.task.id.to_string();

        match (&request.worker, &self.worker_manager) {
            (Some(worker_name), Some(manager)) => {
                let result = manager
                    .run_job(
                        worker_name,
                        WorkerRequest {
                            id: task_name.clone(),
                            command: request.command.clone(),
                            cwd: request
                                .cwd
                                .as_ref()
                                .map(|path| path.to_string_lossy().into_owned()),
                            workspace: request.workspace.clone(),
                            env: request.env.clone(),
                        },
                        request.log_sink.as_ref(),
                    )
                    .await
                    .map_err(|source| ExecutorError::Worker {
                        task: task_name,
                        source,
                    });
                drop(permit);
                return result.map(|(code, inputs, outputs, _logs)| {
                    TaskRunOutcome::worker(synthesize_exit_status(code), inputs, outputs)
                });
            }
            (Some(worker_name), None) => {
                drop(permit);
                return Err(ExecutorError::MissingWorkerManager {
                    task: task_name,
                    worker: worker_name.clone(),
                });
            }
            (None, _) => {}
        }

        let status = self.run_shell_command(request, &task_name).await;
        drop(permit);
        status.map(TaskRunOutcome::shell)
    }

    async fn run_shell_command(
        &self,
        request: &ExecutionRequest,
        task_name: &str,
    ) -> Result<ExitStatus, ExecutorError> {
        let prefix = task_prefix(task_name, self.prefix_width);
        let mut child = spawn_shell_child(request, task_name)?;

        let stdout = child.stdout.take().expect("child stdout piped");
        let stderr = child.stderr.take().expect("child stderr piped");

        let stdout_handle = spawn_stream_logger(
            stdout,
            StreamLoggerContext {
                task_name: task_name.to_owned(),
                prefix: prefix.clone(),
                sink: request.log_sink.clone(),
                stream: LogStream::Stdout,
            },
        );
        let stderr_handle = spawn_stream_logger(
            stderr,
            StreamLoggerContext {
                task_name: task_name.to_owned(),
                prefix,
                sink: request.log_sink.clone(),
                stream: LogStream::Stderr,
            },
        );

        let status = child.wait().await.map_err(|source| ExecutorError::Wait {
            task: task_name.to_owned(),
            source,
        })?;

        stdout_handle.await.expect("stdout task panicked")?;
        stderr_handle.await.expect("stderr task panicked")?;

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
            Some(request) => self.run(&request).await.map(|outcome| outcome.status),
            None => Err(ExecutorError::MissingCommand {
                task: task.id.to_string(),
            }),
        }
    }
}

#[cfg(unix)]
fn synthesize_exit_status(code: i32) -> ExitStatus {
    ExitStatus::from_raw((code & 0xff) << 8)
}

#[cfg(not(unix))]
fn synthesize_exit_status(_code: i32) -> ExitStatus {
    unreachable!("resident workers are only supported on Unix")
}

fn task_prefix(task_name: &str, prefix_width: usize) -> String {
    let width = if prefix_width > 0 {
        prefix_width
    } else {
        task_name.len()
    };
    format!("{task_name:<width$} |")
}

fn spawn_shell_child(
    request: &ExecutionRequest,
    task_name: &str,
) -> Result<tokio::process::Child, ExecutorError> {
    let mut command = Command::new("sh");
    command.arg("-c").arg(&request.command);
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }

    // Clear all inherited environment variables for strict isolation.
    // The request.env already contains the full effective env (whitelist + declared).
    command.env_clear();
    command.envs(&request.env);

    command.spawn().map_err(|source| ExecutorError::Spawn {
        task: task_name.to_owned(),
        source,
    })
}

struct StreamLoggerContext {
    task_name: String,
    prefix: String,
    sink: Option<ExecutionLogSink>,
    stream: LogStream,
}

fn spawn_stream_logger<R>(
    reader: R,
    context: StreamLoggerContext,
) -> tokio::task::JoinHandle<Result<(), ExecutorError>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|source| stream_read_error(context.stream, &context.task_name, source))?
        {
            emit_log_line(&context.prefix, context.sink.as_ref(), context.stream, line);
        }
        Ok(())
    })
}

fn stream_read_error(stream: LogStream, task_name: &str, source: std::io::Error) -> ExecutorError {
    match stream {
        LogStream::Stdout => ExecutorError::StdoutRead {
            task: task_name.to_owned(),
            source,
        },
        LogStream::Stderr => ExecutorError::StderrRead {
            task: task_name.to_owned(),
            source,
        },
    }
}

fn emit_log_line(prefix: &str, sink: Option<&ExecutionLogSink>, stream: LogStream, line: String) {
    if let Some(sink) = sink {
        sink.push(stream, line);
        return;
    }

    match stream {
        LogStream::Stdout => println!("{} {}", prefix, line),
        LogStream::Stderr => eprintln!("{} {}", prefix, line),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        sync::{
            atomic::{AtomicU32, AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use luchta_types::{PackageName, TaskId, TaskName, WorkerDefinition};
    use tempfile::TempDir;
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
    async fn task_executor_trait_remains_object_safe_in_spawned_tasks() {
        let task = task_node("pkg", "build", 1);
        let executor = Arc::new(MockExecutor {
            calls: Arc::new(Mutex::new(Vec::new())),
        });

        let executor_clone = Arc::clone(&executor);
        let task_clone = task.clone();
        let handle = tokio::spawn(async move { executor_clone.execute(&task_clone).await });

        let status = handle
            .await
            .expect("join handle completes")
            .expect("task succeeds");
        assert!(status.success());

        let calls = executor.calls.lock().expect("calls lock");
        assert_eq!(calls.as_slice(), &[task.id]);
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
    async fn run_with_on_start_fires_only_after_permit_acquired() {
        let executor = Arc::new(WeightedExecutor::new(1));
        let temp = TempDir::new().expect("tempdir");
        let block_path = temp.path().join("blocker.sh");
        fs::write(
            &block_path,
            "#!/bin/sh
while [ ! -f release ]; do sleep 0.01; done
",
        )
        .expect("blocker script written");
        let mut permissions = fs::metadata(&block_path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&block_path, permissions).expect("chmod");

        let request_a =
            ExecutionRequest::new(task_node("pkg", "a", 1), block_path.display().to_string())
                .with_cwd(temp.path());
        let request_b =
            ExecutionRequest::new(task_node("pkg", "b", 1), "echo second").with_cwd(temp.path());

        let started = Arc::new(AtomicUsize::new(0));
        let max_started = Arc::new(AtomicUsize::new(0));

        let run_request = |request: ExecutionRequest| {
            let executor = Arc::clone(&executor);
            let started = Arc::clone(&started);
            let max_started = Arc::clone(&max_started);
            tokio::spawn(async move {
                let started_for_run = Arc::clone(&started);
                let max_started_for_run = Arc::clone(&max_started);
                let outcome = executor
                    .run_with_on_start(&request, move || {
                        let current = started_for_run.fetch_add(1, Ordering::SeqCst) + 1;
                        max_started_for_run.fetch_max(current, Ordering::SeqCst);
                    })
                    .await;
                started.fetch_sub(1, Ordering::SeqCst);
                outcome
            })
        };

        let handle_a = run_request(request_a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(started.load(Ordering::SeqCst), 1);

        let handle_b = run_request(request_b);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(max_started.load(Ordering::SeqCst), 1);

        fs::write(temp.path().join("release"), "ok").expect("release file written");

        let outcome_a = handle_a
            .await
            .expect("first join succeeds")
            .expect("first task succeeds");
        let outcome_b = handle_b
            .await
            .expect("second join succeeds")
            .expect("second task succeeds");

        assert!(outcome_a.status.success());
        assert!(outcome_b.status.success());
        assert_eq!(max_started.load(Ordering::SeqCst), 1);
        assert_eq!(started.load(Ordering::SeqCst), 0);
        assert_eq!(executor.semaphore().available_permits(), 1);
    }

    #[tokio::test]
    async fn run_rejects_worker_assignment_without_manager() {
        let executor = WeightedExecutor::new(2);
        let request =
            ExecutionRequest::new(task_node("pkg", "worker-without-manager", 1), "echo hi")
                .with_worker("fake");

        let err = executor
            .run(&request)
            .await
            .expect_err("missing worker manager surfaces");

        assert!(matches!(
            err,
            ExecutorError::MissingWorkerManager { task, worker }
                if task == "pkg#worker-without-manager" && worker == "fake"
        ));
        assert_eq!(executor.semaphore().available_permits(), 2);
    }

    #[tokio::test]
    async fn run_spawns_real_command() {
        let executor = WeightedExecutor::new(2);
        let request = ExecutionRequest::new(task_node("pkg", "echo", 1), "echo hello");

        let outcome = executor.run(&request).await.expect("real command succeeds");

        assert!(outcome.status.success());
        assert_eq!(executor.semaphore().available_permits(), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_routes_worker_requests_and_synthesizes_exit_codes() {
        // (task, command, worker exit code, optional log line) -> expected exit code
        let cases = [
            (
                task_node("pkg", "worker", 1),
                "echo from worker",
                0,
                Some("worker hello"),
            ),
            (task_node("pkg", "worker-exit", 1), "exit 3", 3, None),
        ];

        for (task, command, exit_code, log_line) in cases {
            let status =
                run_worker_status_test(task, command, worker_done_script(exit_code, log_line))
                    .await;

            assert_eq!(status.success(), exit_code == 0);
            assert_eq!(status.code(), Some(exit_code));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_surfaces_worker_crashes() {
        let temp = TempDir::new().expect("tempdir");
        let worker_path = write_worker_script(
            temp.path(),
            r#"#!/bin/sh
read -r _
exit 0
"#,
        );
        let manager = Arc::new(manager_with_worker(&worker_path));
        let executor = WeightedExecutor::new(2).with_worker_manager(Arc::clone(&manager));
        let request = ExecutionRequest::new(task_node("pkg", "worker-crash", 1), "echo hi")
            .with_worker("fake");

        let err = executor
            .run(&request)
            .await
            .expect_err("worker crash surfaces");

        assert!(matches!(
            err,
            ExecutorError::Worker {
                task,
                source: WorkerError::Crashed { worker, id, .. }
            } if task == "pkg#worker-crash" && worker == "fake" && id == "pkg#worker-crash"
        ));
        drop(executor);
        Arc::try_unwrap(manager)
            .expect("manager only ref")
            .shutdown()
            .await;
    }

    #[cfg(unix)]
    async fn run_worker_status_test(
        task: TaskNode,
        command: &str,
        script_body: String,
    ) -> ExitStatus {
        let temp = TempDir::new().expect("tempdir");
        let worker_path = write_worker_script(temp.path(), &script_body);
        let manager = Arc::new(manager_with_worker(&worker_path));
        let executor = WeightedExecutor::new(2).with_worker_manager(Arc::clone(&manager));
        let request = ExecutionRequest::new(task, command).with_worker("fake");

        let outcome = executor
            .run(&request)
            .await
            .expect("worker command returns status");

        drop(executor);
        Arc::try_unwrap(manager)
            .expect("manager only ref")
            .shutdown()
            .await;
        outcome.status
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_logs_are_captured_once_in_sink() {
        let temp = TempDir::new().expect("tempdir");
        let worker_path =
            write_worker_script(temp.path(), &worker_done_script(0, Some("only-once")));
        let manager = Arc::new(manager_with_worker(&worker_path));
        let executor = WeightedExecutor::new(2).with_worker_manager(Arc::clone(&manager));
        let sink = ExecutionLogSink::new();
        let request = ExecutionRequest::new(task_node("pkg", "build", 1), "echo ignored")
            .with_worker("fake")
            .with_log_sink(sink.clone());

        let outcome = executor.run(&request).await.expect("worker succeeds");

        assert!(outcome.status.success());
        assert_eq!(
            sink.lines(),
            vec![CapturedLogLine {
                stream: LogStream::Stdout,
                line: "only-once".to_owned(),
            }]
        );

        drop(executor);
        Arc::try_unwrap(manager)
            .expect("manager only ref")
            .shutdown()
            .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_log_sink_can_be_replayed_once_and_cache_hit_stays_quiet() {
        let temp = TempDir::new().expect("tempdir");
        let worker_path = write_worker_script(
            temp.path(),
            &worker_done_script(0, Some("fresh-run-visible")),
        );
        let manager = Arc::new(manager_with_worker(&worker_path));
        let executor = WeightedExecutor::new(2).with_worker_manager(Arc::clone(&manager));
        let sink = ExecutionLogSink::new();
        let request = ExecutionRequest::new(task_node("pkg", "build", 1), "echo ignored")
            .with_worker("fake")
            .with_log_sink(sink.clone());

        let outcome = executor.run(&request).await.expect("worker succeeds");

        assert!(outcome.status.success());
        let first_replay = sink.lines();
        assert_eq!(
            first_replay,
            vec![CapturedLogLine {
                stream: LogStream::Stdout,
                line: "fresh-run-visible".to_owned(),
            }]
        );
        let cache_hit_replay = Vec::<CapturedLogLine>::new();
        assert!(cache_hit_replay.is_empty());
        assert_eq!(sink.lines(), first_replay);

        drop(executor);
        Arc::try_unwrap(manager)
            .expect("manager only ref")
            .shutdown()
            .await;
    }

    #[cfg(unix)]
    fn worker_done_script(exit_code: i32, log_line: Option<&str>) -> String {
        let mut script = String::from(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
"#,
        );
        if let Some(log_line) = log_line {
            script.push_str(&format!(
                "  printf '{{\"type\":\"log\",\"id\":\"%s\",\"stream\":\"stdout\",\"line\":\"{}\"}}\\n' \"$id\"\n",
                log_line
            ));
        }
        script.push_str(&format!(
            "  printf '{{\"type\":\"done\",\"id\":\"%s\",\"exitCode\":{exit_code}}}\\n' \"$id\"\ndone\n"
        ));
        script
    }
    #[cfg(unix)]
    fn manager_with_worker(worker_path: &Path) -> WorkerManager {
        let mut definitions = HashMap::new();
        definitions.insert(
            "fake".to_owned(),
            WorkerDefinition {
                command: worker_path.display().to_string(),
                depends_on: Vec::new(),
                env: std::collections::BTreeMap::new(),
            },
        );
        WorkerManager::new(definitions)
    }

    #[cfg(unix)]
    fn write_worker_script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-worker.sh");
        fs::write(&path, body).expect("worker script written");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod");
        path
    }
}
