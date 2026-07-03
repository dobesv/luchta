use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use thiserror::Error;
use tokio::io::{stderr, stdout, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::{WorkerMessage, WorkerResponse};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>;
type SharedChildStdin = Arc<Mutex<ChildStdin>>;
type ResponseResult = Result<WorkerResponse, String>;
/// In-flight requests keyed by correlation id. Each entry holds the
/// `oneshot::Sender` half whose receiver the calling `send` future awaits, so a
/// response delivered before the caller parks can never be lost (the oneshot
/// buffers the single value). The reader task removes and fires the sender when
/// the matching response arrives.
type ResponseWaiters = Arc<Mutex<HashMap<String, oneshot::Sender<ResponseResult>>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateArgvSplit {
    pub stage_args: Vec<String>,
    pub delegate_command: Vec<String>,
}

pub fn split_delegate_argv<I, S>(args: I) -> DelegateArgvSplit
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut stage_args = Vec::new();
    let mut delegate_command = Vec::new();
    let mut seen_split = false;

    for arg in args {
        let arg = arg.into();
        if seen_split {
            delegate_command.push(arg);
        } else if arg == "--" {
            seen_split = true;
        } else {
            stage_args.push(arg);
        }
    }

    DelegateArgvSplit {
        stage_args,
        delegate_command,
    }
}

pub fn split_current_process_argv() -> DelegateArgvSplit {
    split_delegate_argv(std::env::args())
}

pub struct DelegateHandle {
    delegate_command: Vec<String>,
    state: Mutex<Option<DelegateState>>,
    stdout_writer: SharedWriter,
    stderr_writer: SharedWriter,
    stderr_prefix: Option<String>,
}

struct DelegateState {
    child: Child,
    stdin: SharedChildStdin,
    waiters: ResponseWaiters,
    stdout_task: tokio::task::JoinHandle<Result<(), ProxyError>>,
    stderr_task: tokio::task::JoinHandle<Result<(), ProxyError>>,
}

pub struct RawDelegate {
    state: Arc<Mutex<Option<RawDelegateState>>>,
    stdin_tx: StdMutex<Option<mpsc::UnboundedSender<String>>>,
    stdout_rx: Option<mpsc::Receiver<String>>,
}

struct RawDelegateState {
    child: Child,
    stdin_task: JoinHandle<Result<(), ProxyError>>,
    stdout_task: JoinHandle<Result<(), ProxyError>>,
    stderr_task: JoinHandle<Result<(), ProxyError>>,
}

impl DelegateHandle {
    pub fn new(delegate_command: Vec<String>) -> Self {
        Self::with_stderr_prefix(delegate_command, None)
    }

    pub fn with_stderr_prefix(
        delegate_command: Vec<String>,
        stderr_prefix: Option<String>,
    ) -> Self {
        let stdout_writer: SharedWriter = Arc::new(Mutex::new(Box::new(stdout())));
        let stderr_writer: SharedWriter = Arc::new(Mutex::new(Box::new(stderr())));
        Self::with_writers(
            delegate_command,
            stdout_writer,
            stderr_writer,
            stderr_prefix,
        )
    }

    pub fn with_writers(
        delegate_command: Vec<String>,
        stdout_writer: SharedWriter,
        stderr_writer: SharedWriter,
        stderr_prefix: Option<String>,
    ) -> Self {
        Self {
            delegate_command,
            state: Mutex::new(None),
            stdout_writer,
            stderr_writer,
            stderr_prefix,
        }
    }

    /// Forwards `message` to the delegate and waits for its correlated response.
    ///
    /// No response timeout is applied: a `Run` can legitimately stream `Log`
    /// lines for a long time before its terminal `Done`, so bounding it here
    /// would kill long-running builds. Delegate *death* is still handled — when
    /// the delegate's stdout closes, every in-flight waiter is failed with
    /// [`ProxyError::DelegateClosed`] (no hang). For paths where the delegate
    /// must respond promptly (e.g. graph-build `resolve`), use
    /// [`DelegateHandle::send_with_timeout`].
    pub async fn send(&self, message: WorkerMessage) -> Result<WorkerResponse, ProxyError> {
        self.send_inner(message, None).await
    }

    /// Like [`send`](DelegateHandle::send) but fails with
    /// [`ProxyError::ResponseTimeout`] if no response arrives within `timeout`.
    ///
    /// Use this for `resolve` forwards (graph-build must not hang on a delegate
    /// that is alive but silent/deadlocked).
    pub async fn send_with_timeout(
        &self,
        message: WorkerMessage,
        timeout: Duration,
    ) -> Result<WorkerResponse, ProxyError> {
        self.send_inner(message, Some(timeout)).await
    }

    async fn send_inner(
        &self,
        message: WorkerMessage,
        response_timeout: Option<Duration>,
    ) -> Result<WorkerResponse, ProxyError> {
        let state = self.ensure_spawned().await?;
        let (response_tx, response_rx) = oneshot::channel::<ResponseResult>();
        let id = message.id().to_owned();

        {
            let mut waiters = state.waiters.lock().await;
            if waiters.insert(id.clone(), response_tx).is_some() {
                return Err(ProxyError::DuplicateInflightId(id));
            }
        }

        let line = serde_json::to_string(&message)?;
        let send_result = async {
            let mut stdin = state.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
            Ok::<(), std::io::Error>(())
        }
        .await;

        if let Err(error) = send_result {
            // Drop our registration so the reader doesn't try to deliver later.
            state.waiters.lock().await.remove(&id);
            return Err(ProxyError::Io(error));
        }

        // The oneshot buffers the single value, so a response delivered by the
        // reader before we park here is still received (no lost-wakeup race).
        let received = match response_timeout {
            Some(limit) => match timeout(limit, response_rx).await {
                Ok(received) => received,
                Err(_) => {
                    // Abandon the waiter so a late reader delivery is dropped.
                    state.waiters.lock().await.remove(&id);
                    return Err(ProxyError::ResponseTimeout(id));
                }
            },
            None => response_rx.await,
        };

        match received {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(message)) => Err(ProxyError::DelegateClosed(message)),
            // Sender dropped without sending (reader task gone/aborted).
            Err(_) => Err(ProxyError::DelegateClosed(
                "delegate response channel closed".to_owned(),
            )),
        }
    }

    async fn ensure_spawned(&self) -> Result<SpawnedDelegate, ProxyError> {
        let mut state_guard = self.state.lock().await;
        if let Some(state) = state_guard.as_mut() {
            return Ok(SpawnedDelegate {
                stdin: Arc::clone(&state.stdin),
                waiters: Arc::clone(&state.waiters),
            });
        }

        let mut child = spawn_delegate_child(&self.delegate_command)?;
        let stdin = Arc::new(Mutex::new(
            child.stdin.take().ok_or(ProxyError::MissingPipe("stdin"))?,
        ));
        let stdout = child
            .stdout
            .take()
            .ok_or(ProxyError::MissingPipe("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(ProxyError::MissingPipe("stderr"))?;
        let waiters: ResponseWaiters = Arc::new(Mutex::new(HashMap::new()));
        let stdout_task = tokio::spawn(read_delegate_stdout(
            BufReader::new(stdout).lines(),
            Arc::clone(&waiters),
            Arc::clone(&self.stdout_writer),
        ));
        let stderr_task = tokio::spawn(forward_delegate_stderr(
            BufReader::new(stderr).lines(),
            Arc::clone(&self.stderr_writer),
            self.stderr_prefix.clone(),
        ));

        *state_guard = Some(DelegateState {
            child,
            stdin: Arc::clone(&stdin),
            waiters: Arc::clone(&waiters),
            stdout_task,
            stderr_task,
        });

        Ok(SpawnedDelegate { stdin, waiters })
    }

    pub async fn shutdown(&self) -> Result<(), ProxyError> {
        let state = self.state.lock().await.take();
        if let Some(state) = state {
            shutdown_delegate(state).await?;
        }
        Ok(())
    }
}

impl RawDelegate {
    pub fn spawn(command: Vec<String>) -> Result<Self, ProxyError> {
        Self::spawn_with_stderr(command, default_shared_stderr_writer())
    }

    pub fn spawn_with_stderr(
        command: Vec<String>,
        stderr_writer: SharedWriter,
    ) -> Result<Self, ProxyError> {
        let mut child = spawn_delegate_child(&command)?;
        let stdin = child.stdin.take().ok_or(ProxyError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(ProxyError::MissingPipe("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(ProxyError::MissingPipe("stderr"))?;

        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
        let (stdout_tx, stdout_rx) = mpsc::channel(64);
        let stdin_task = tokio::spawn(drain_raw_stdin(stdin, stdin_rx));
        let stdout_task = tokio::spawn(read_raw_stdout(stdout, stdout_tx));
        let stderr_task = tokio::spawn(forward_raw_stderr(stderr, stderr_writer));

        Ok(Self {
            state: Arc::new(Mutex::new(Some(RawDelegateState {
                child,
                stdin_task,
                stdout_task,
                stderr_task,
            }))),
            stdin_tx: StdMutex::new(Some(stdin_tx)),
            stdout_rx: Some(stdout_rx),
        })
    }

    pub fn send_line(&self, line: String) -> Result<(), ProxyError> {
        let sender = self
            .stdin_tx
            .lock()
            .expect("raw delegate stdin mutex poisoned")
            .as_ref()
            .cloned()
            .ok_or_else(|| ProxyError::DelegateClosed("delegate stdin closed".to_owned()))?;
        sender
            .send(line)
            .map_err(|_| ProxyError::DelegateClosed("delegate stdin closed".to_owned()))
    }

    pub fn take_stdout(&mut self) -> Option<mpsc::Receiver<String>> {
        self.stdout_rx.take()
    }

    pub async fn close_stdin(&self) {
        self.stdin_tx
            .lock()
            .expect("raw delegate stdin mutex poisoned")
            .take();
    }

    pub async fn shutdown(self) -> Result<(), ProxyError> {
        self.close_stdin().await;
        let state = self.state.lock().await.take();
        if let Some(state) = state {
            shutdown_raw_delegate(state).await?;
        }
        Ok(())
    }
}

struct SpawnedDelegate {
    stdin: SharedChildStdin,
    waiters: ResponseWaiters,
}

fn spawn_delegate_child(delegate_command: &[String]) -> Result<Child, ProxyError> {
    let program = delegate_command
        .first()
        .cloned()
        .ok_or(ProxyError::MissingDelegateCommand)?;
    let mut command = Command::new(program);
    command.args(delegate_command.iter().skip(1));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        command.process_group(0);
    }

    Ok(command.spawn()?)
}

async fn read_delegate_stdout(
    mut lines: tokio::io::Lines<BufReader<ChildStdout>>,
    waiters: ResponseWaiters,
    writer: SharedWriter,
) -> Result<(), ProxyError> {
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                fail_all_waiters(&waiters, "delegate stdout closed".to_owned()).await;
                return Ok(());
            }
            Err(error) => {
                fail_all_waiters(&waiters, format!("delegate stdout read failed: {error}")).await;
                return Err(error.into());
            }
        };

        let response: WorkerResponse = match serde_json::from_str(&line) {
            Ok(response) => response,
            Err(error) => {
                fail_all_waiters(
                    &waiters,
                    format!("delegate stdout contained invalid JSON: {error}"),
                )
                .await;
                return Err(error.into());
            }
        };

        if let Err(error) = write_response(&writer, &response).await {
            fail_all_waiters(&waiters, format!("proxy stdout write failed: {error}")).await;
            return Err(error);
        }
        let waiter = { waiters.lock().await.remove(response.id()) };
        if let Some(waiter) = waiter {
            // Receiver may have gone away (caller cancelled); ignore that.
            let _ = waiter.send(Ok(response));
        }
    }
}

async fn drain_raw_stdin(
    mut stdin: ChildStdin,
    mut lines: mpsc::UnboundedReceiver<String>,
) -> Result<(), ProxyError> {
    while let Some(line) = lines.recv().await {
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
    }
    Ok(())
}

async fn read_raw_stdout(
    stdout: ChildStdout,
    stdout_tx: mpsc::Sender<String>,
) -> Result<(), ProxyError> {
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        if stdout_tx.send(line).await.is_err() {
            break;
        }
    }
    Ok(())
}

async fn forward_delegate_stderr(
    mut lines: tokio::io::Lines<BufReader<ChildStderr>>,
    writer: SharedWriter,
    prefix: Option<String>,
) -> Result<(), ProxyError> {
    while let Some(line) = lines.next_line().await? {
        let rendered = match &prefix {
            Some(prefix) => format!("{prefix}{line}"),
            None => line,
        };
        let mut writer = writer.lock().await;
        writer.write_all(rendered.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}

async fn forward_raw_stderr(
    stderr_pipe: ChildStderr,
    writer: SharedWriter,
) -> Result<(), ProxyError> {
    forward_delegate_stderr(BufReader::new(stderr_pipe).lines(), writer, None).await
}

fn default_shared_stderr_writer() -> SharedWriter {
    Arc::new(Mutex::new(Box::new(stderr())))
}

async fn write_response(
    writer: &SharedWriter,
    response: &WorkerResponse,
) -> Result<(), ProxyError> {
    let line = serde_json::to_string(response)?;
    let mut writer = writer.lock().await;
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn shutdown_delegate(mut state: DelegateState) -> Result<(), ProxyError> {
    drop(state.stdin);

    let wait_result = timeout(SHUTDOWN_TIMEOUT, state.child.wait()).await;
    match wait_result {
        Ok(status_result) => {
            status_result?;
        }
        Err(_) => {
            terminate_child(&mut state.child).await?;
            match timeout(SHUTDOWN_TIMEOUT, state.child.wait()).await {
                Ok(status_result) => {
                    status_result?;
                }
                Err(_) => {
                    kill_child(&mut state.child).await?;
                    state.child.wait().await?;
                }
            }
        }
    }

    state.stdout_task.await??;
    state.stderr_task.await??;
    Ok(())
}

async fn shutdown_raw_delegate(mut state: RawDelegateState) -> Result<(), ProxyError> {
    let wait_result = timeout(SHUTDOWN_TIMEOUT, state.child.wait()).await;
    match wait_result {
        Ok(status_result) => {
            status_result?;
        }
        Err(_) => {
            terminate_child(&mut state.child).await?;
            match timeout(SHUTDOWN_TIMEOUT, state.child.wait()).await {
                Ok(status_result) => {
                    status_result?;
                }
                Err(_) => {
                    kill_child(&mut state.child).await?;
                    state.child.wait().await?;
                }
            }
        }
    }

    state.stdin_task.await??;
    state.stdout_task.await??;
    state.stderr_task.await??;
    Ok(())
}

#[cfg(unix)]
async fn terminate_child(child: &mut Child) -> Result<(), ProxyError> {
    let id = child.id().ok_or(ProxyError::MissingChildId)? as i32;
    nix_killpg(id, libc::SIGTERM)?;
    Ok(())
}

#[cfg(unix)]
async fn kill_child(child: &mut Child) -> Result<(), ProxyError> {
    let id = child.id().ok_or(ProxyError::MissingChildId)? as i32;
    nix_killpg(id, libc::SIGKILL)?;
    Ok(())
}

#[cfg(unix)]
fn nix_killpg(pgid: i32, signal: i32) -> Result<(), ProxyError> {
    let result = unsafe { libc::kill(-pgid, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(ProxyError::Io(std::io::Error::last_os_error()))
    }
}

// Windows has no process-group signalling equivalent to SIGTERM/SIGKILL, so both
// the graceful and forceful paths fall back to `Child::start_kill`, which issues
// a `TerminateProcess`. The caller already awaits `child.wait()` afterwards to
// reap the process.
#[cfg(windows)]
async fn terminate_child(child: &mut Child) -> Result<(), ProxyError> {
    child.start_kill()?;
    Ok(())
}

#[cfg(windows)]
async fn kill_child(child: &mut Child) -> Result<(), ProxyError> {
    child.start_kill()?;
    Ok(())
}

async fn fail_all_waiters(waiters: &ResponseWaiters, message: String) {
    let waiters = {
        let mut guard = waiters.lock().await;
        guard.drain().map(|(_, waiter)| waiter).collect::<Vec<_>>()
    };
    for waiter in waiters {
        // Receiver may already be gone; ignore the resulting send error.
        let _ = waiter.send(Err(message.clone()));
    }
}

impl Drop for DelegateHandle {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.try_lock() {
            if let Some(mut state) = state.take() {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        let _ = shutdown_delegate(state).await;
                    });
                } else {
                    let _ = state.child.start_kill();
                    state.stdout_task.abort();
                    state.stderr_task.abort();
                }
            }
        }
    }
}

impl Drop for RawDelegate {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.try_lock() {
            if let Some(mut state) = state.take() {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    self.stdin_tx
                        .lock()
                        .expect("raw delegate stdin mutex poisoned")
                        .take();
                    runtime.spawn(async move {
                        let _ = shutdown_raw_delegate(state).await;
                    });
                } else {
                    let _ = state.child.start_kill();
                    state.stdin_task.abort();
                    state.stdout_task.abort();
                    state.stderr_task.abort();
                }
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("join error: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("missing delegate command")]
    MissingDelegateCommand,
    #[error("missing {0} pipe")]
    MissingPipe(&'static str),
    #[error("duplicate in-flight delegate id: {0}")]
    DuplicateInflightId(String),
    #[error("delegate closed before response: {0}")]
    DelegateClosed(String),
    #[error("delegate did not respond before timeout for id: {0}")]
    ResponseTimeout(String),
    #[cfg(unix)]
    #[error("delegate child missing pid")]
    MissingChildId,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Value;
    use tokio::io::{duplex, AsyncBufReadExt, BufReader, DuplexStream};

    use crate::{ResolveResult, ResolveTask, WorkerRequest};

    use super::*;

    fn writer_pair() -> (SharedWriter, DuplexStream) {
        let (writer_stream, reader) = duplex(16 * 1024);
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer_stream)));
        (writer, reader)
    }

    async fn read_json_lines(reader: DuplexStream) -> Vec<Value> {
        let mut lines = BufReader::new(reader).lines();
        let mut values = Vec::new();
        while let Some(line) = lines.next_line().await.expect("read line") {
            values.push(serde_json::from_str(&line).expect("json line"));
        }
        values
    }

    fn loopback_delegate_command() -> Vec<String> {
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            r#"while IFS= read -r line; do
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    case $line in
        *'"type":"run"'*)
            printf '{"type":"done","id":"%s","exitCode":0}\n' "$id"
            ;;
        *)
            printf '{"type":"resolved","id":"%s","result":{"decision":"accept"}}\n' "$id"
            ;;
    esac
done
"#
            .to_owned(),
        ]
    }

    #[test]
    fn split_delegate_argv_uses_first_separator_only() {
        let split = split_delegate_argv([
            "wrapper",
            "--flag",
            "value",
            "--",
            "python3",
            "-c",
            "print('two words')",
            "arg with spaces",
            "--",
            "tail",
        ]);
        assert_eq!(
            split,
            DelegateArgvSplit {
                stage_args: vec!["wrapper", "--flag", "value"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                delegate_command: vec![
                    "python3",
                    "-c",
                    "print('two words')",
                    "arg with spaces",
                    "--",
                    "tail",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            }
        );
    }

    #[tokio::test]
    async fn forwarded_run_and_resolve_emit_delegate_stdout() {
        let (stdout_writer, stdout_reader) = writer_pair();
        let (stderr_writer, stderr_reader) = writer_pair();
        let handle = DelegateHandle::with_writers(
            loopback_delegate_command(),
            stdout_writer,
            stderr_writer,
            Some("delegate: ".to_owned()),
        );

        let done = handle
            .send(WorkerMessage::Run(WorkerRequest::new("job-1", "build")))
            .await
            .expect("run forwarded");
        assert_eq!(done, WorkerResponse::done("job-1", 0));

        let resolved = handle
            .send(WorkerMessage::ResolveTask(ResolveTask {
                id: "pkg#build".to_owned(),
                name: "build".to_owned(),
                command: String::new(),
                package: "pkg".to_owned(),
                cwd: None,
                scripts: Vec::new(),
                mode: crate::ResolveMode::Run,
            }))
            .await
            .expect("resolve forwarded");
        assert_eq!(
            resolved,
            WorkerResponse::resolved("pkg#build", ResolveResult::accept())
        );

        handle.shutdown().await.expect("shutdown ok");
        // Drop the handle so its retained stdout/stderr writer halves close and
        // the reader sees EOF (otherwise read_json_lines blocks forever).
        drop(handle);
        let stdout_values = read_json_lines(stdout_reader).await;
        let stderr_values = read_json_lines(stderr_reader).await;

        assert_eq!(stdout_values.len(), 2);
        assert_eq!(stdout_values[0]["type"], "done");
        assert_eq!(stdout_values[1]["type"], "resolved");
        assert!(stderr_values.is_empty());
    }

    #[tokio::test]
    async fn concurrent_requests_route_by_id() {
        let (stdout_writer, stdout_reader) = writer_pair();
        let (stderr_writer, _stderr_reader) = writer_pair();
        let handle = Arc::new(DelegateHandle::with_writers(
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                r#"while IFS= read -r line; do
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    case $id in
        first)
            delay=0.2
            ;;
        *)
            delay=0.05
            ;;
    esac
    (
        sleep "$delay"
        printf '{"type":"done","id":"%s","exitCode":0}\n' "$id"
    ) &
done
wait
"#
                .to_owned(),
            ],
            stdout_writer,
            stderr_writer,
            None,
        ));

        let first_handle = Arc::clone(&handle);
        let second_handle = Arc::clone(&handle);
        let first = tokio::spawn(async move {
            first_handle
                .send(WorkerMessage::Run(WorkerRequest::new("first", "build")))
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = tokio::spawn(async move {
            second_handle
                .send(WorkerMessage::Run(WorkerRequest::new("second", "test")))
                .await
        });

        let first_response = first.await.expect("join first").expect("first response");
        let second_response = second.await.expect("join second").expect("second response");
        assert_eq!(first_response.id(), "first");
        assert_eq!(second_response.id(), "second");

        handle.shutdown().await.expect("shutdown ok");
        // Release the last handle Arc so the retained writer halves close and the
        // reader sees EOF (the spawned tasks already dropped their clones).
        drop(handle);
        let stdout_values = read_json_lines(stdout_reader).await;
        assert_eq!(stdout_values.len(), 2);
        assert_eq!(stdout_values[0]["id"], "second");
        assert_eq!(stdout_values[1]["id"], "first");
    }

    #[tokio::test]
    async fn delegate_stdout_close_surfaces_clean_error() {
        let handle = DelegateHandle::new(vec![
            "python3".to_owned(),
            "-c".to_owned(),
            "import sys; sys.exit(1)".to_owned(),
        ]);

        let error = tokio::time::timeout(
            Duration::from_secs(5),
            handle.send(WorkerMessage::Run(WorkerRequest::new("job-1", "build"))),
        )
        .await
        .expect("must not hang")
        .expect_err("delegate should fail");

        match error {
            ProxyError::DelegateClosed(message) => {
                assert!(message.contains("delegate stdout closed"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn send_with_timeout_surfaces_clean_error_when_delegate_stays_silent() {
        // Delegate stays ALIVE (reads stdin forever) but never writes a
        // response. Without a timeout this would hang; send_with_timeout must
        // surface ResponseTimeout instead.
        let handle = DelegateHandle::new(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "while IFS= read -r _line; do :; done".to_owned(),
        ]);

        let error = tokio::time::timeout(
            Duration::from_secs(5),
            handle.send_with_timeout(
                WorkerMessage::Run(WorkerRequest::new("job-silent", "build")),
                Duration::from_millis(150),
            ),
        )
        .await
        .expect("must not hang")
        .expect_err("silent delegate should time out");

        match error {
            ProxyError::ResponseTimeout(id) => assert_eq!(id, "job-silent"),
            other => panic!("unexpected error: {other}"),
        }

        handle.shutdown().await.expect("shutdown ok");
    }

    #[tokio::test]
    async fn malformed_delegate_stdout_fails_inflight_waiter_instead_of_hanging() {
        let handle = DelegateHandle::new(vec![
            "python3".to_owned(),
            "-c".to_owned(),
            "import sys\nsys.stdout.write(\"not json\\n\")\nsys.stdout.flush()\nwhile sys.stdin.readline():\n    pass\n"
                .to_owned(),
        ]);

        let error = tokio::time::timeout(
            Duration::from_secs(5),
            handle.send(WorkerMessage::Run(WorkerRequest::new(
                "job-bad-json",
                "build",
            ))),
        )
        .await
        .expect("must not hang")
        .expect_err("malformed delegate stdout should fail send");

        match error {
            ProxyError::DelegateClosed(message) => {
                assert!(
                    message.contains("invalid JSON"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    fn raw_delegate_command(command: &[&str]) -> Vec<String> {
        command.iter().map(|part| (*part).to_owned()).collect()
    }

    fn spawn_cat_raw_delegate() -> (RawDelegate, mpsc::Receiver<String>) {
        spawn_raw_delegate(raw_delegate_command(&["cat"]))
    }

    fn spawn_raw_delegate(command: Vec<String>) -> (RawDelegate, mpsc::Receiver<String>) {
        let mut handle = RawDelegate::spawn(command).expect("spawn raw delegate");
        let stdout = handle.take_stdout().expect("stdout receiver available");
        (handle, stdout)
    }

    async fn recv_raw_line(stdout: &mut mpsc::Receiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(2), stdout.recv())
            .await
            .expect("recv should not time out")
            .expect("stdout should yield line")
    }

    async fn assert_raw_line_round_trip(line: &str) {
        let (handle, mut stdout) = spawn_cat_raw_delegate();
        handle.send_line(line.to_owned()).expect("send line");
        assert_eq!(recv_raw_line(&mut stdout).await, line);
        handle.close_stdin().await;
        handle.shutdown().await.expect("shutdown ok");
    }

    #[tokio::test]
    async fn raw_delegate_echoes_lines() {
        assert_raw_line_round_trip("hello").await;
    }

    #[tokio::test]
    async fn raw_delegate_exits_on_stdin_close() {
        let (handle, _stdout) =
            spawn_raw_delegate(raw_delegate_command(&["sh", "-c", "cat >/dev/null"]));
        handle.close_stdin().await;
        tokio::time::timeout(Duration::from_secs(2), handle.shutdown())
            .await
            .expect("shutdown should finish quickly")
            .expect("shutdown ok");
    }

    #[tokio::test]
    async fn raw_delegate_shutdown_kills_stuck_child() {
        let (handle, _stdout) = spawn_raw_delegate(raw_delegate_command(&[
            "sh",
            "-c",
            "trap \"\" TERM; sleep 60",
        ]));
        tokio::time::timeout(Duration::from_secs(12), handle.shutdown())
            .await
            .expect("shutdown should complete via kill path")
            .expect("shutdown ok");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn send_line_from_async_context_does_not_panic() {
        assert_raw_line_round_trip("async-context").await;
    }

    #[test]
    fn dropping_delegate_handle_without_runtime_does_not_panic() {
        let join = std::thread::spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let handle = runtime.block_on(async {
                let handle = DelegateHandle::new(vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "sleep 30".to_owned(),
                ]);
                handle
                    .send_with_timeout(
                        WorkerMessage::Run(WorkerRequest::new("job-drop", "build")),
                        Duration::from_millis(50),
                    )
                    .await
                    .expect_err("delegate should stay silent");
                handle
            });
            drop(runtime);
            drop(handle);
        });

        assert!(join.join().is_ok(), "drop should not panic without runtime");
    }

    #[test]
    fn dropping_raw_delegate_without_runtime_does_not_panic() {
        let join = std::thread::spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let handle = runtime.block_on(async {
                RawDelegate::spawn(vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "sleep 30".to_owned(),
                ])
                .expect("spawn raw delegate")
            });
            drop(runtime);
            drop(handle);
        });

        assert!(join.join().is_ok(), "drop should not panic without runtime");
    }
}
