use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use thiserror::Error;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::{LogStream, ResolveResult, ResolveTask, WorkerMessage, WorkerRequest, WorkerResponse};

pub trait Worker: Send + Sync + 'static {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult;
    fn build_command(&self, req: &WorkerRequest) -> String;

    fn done_response(&self, req: &WorkerRequest, exit_code: i32) -> WorkerResponse {
        WorkerResponse::done(req.id.clone(), exit_code)
    }
}

type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>;

pub async fn run_worker<W: Worker>(worker: W) -> Result<(), WorkerError> {
    let worker = Arc::new(worker);
    let writer: SharedWriter = Arc::new(Mutex::new(Box::new(stdout())));
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut requests = BufReader::new(stdin()).lines();
    let mut jobs = JoinSet::new();

    loop {
        match requests.next_line().await {
            Ok(Some(line)) => {
                let message = serde_json::from_str(&line)?;
                spawn_request(message, Arc::clone(&worker), &writer, &shutdown, &mut jobs);
            }
            Ok(None) => break,
            Err(error) if is_pipe_shutdown_error(&error) => {
                shutdown.store(true, Ordering::SeqCst);
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }

    drain_jobs(&mut jobs).await;
    Ok(())
}

pub async fn run_worker_main<W: Worker>(worker: W) {
    if let Err(error) = run_worker(worker).await {
        eprintln!("worker error: {error}");
        std::process::exit(1);
    }
}

pub fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn spawn_request<W: Worker>(
    message: WorkerMessage,
    worker: Arc<W>,
    writer: &SharedWriter,
    shutdown: &Arc<AtomicBool>,
    jobs: &mut JoinSet<()>,
) {
    match message {
        WorkerMessage::Run(request) => spawn_run(request, worker, writer, shutdown, jobs),
        WorkerMessage::ResolveTask(resolve) => spawn_resolve(resolve, worker, writer, jobs),
    }
}

fn spawn_run<W: Worker>(
    request: WorkerRequest,
    worker: Arc<W>,
    writer: &SharedWriter,
    shutdown: &Arc<AtomicBool>,
    jobs: &mut JoinSet<()>,
) {
    let writer = Arc::clone(writer);
    let shutdown = Arc::clone(shutdown);
    jobs.spawn(async move {
        if let Err(error) = handle_request(request, worker, writer, shutdown).await {
            if !error.is_pipe_shutdown() {
                eprintln!("job failed: {error}");
            }
        }
    });
}

fn spawn_resolve<W: Worker>(
    resolve: ResolveTask,
    worker: Arc<W>,
    writer: &SharedWriter,
    jobs: &mut JoinSet<()>,
) {
    let writer = Arc::clone(writer);
    jobs.spawn(async move {
        let id = resolve.id.clone();
        let result = worker.resolve_task(&resolve);
        if let Err(error) = write_response(&writer, &WorkerResponse::resolved(id, result)).await {
            if !error.is_pipe_shutdown() {
                eprintln!("resolve failed: {error}");
            }
        }
    });
}

async fn drain_jobs(jobs: &mut JoinSet<()>) {
    while let Some(result) = jobs.join_next().await {
        if let Err(error) = result {
            eprintln!("job task join error: {error}");
        }
    }
}

async fn handle_request<W: Worker>(
    request: WorkerRequest,
    worker: Arc<W>,
    writer: SharedWriter,
    shutdown: Arc<AtomicBool>,
) -> Result<(), WorkerError> {
    let id = request.id.clone();
    let exit_code = match run_one_job(&request, worker.as_ref(), &writer).await {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) if error.is_pipe_shutdown() => {
            shutdown.store(true, Ordering::SeqCst);
            return Ok(());
        }
        Err(error) => {
            eprintln!("job {id} failed: {error}");
            1
        }
    };
    write_response(&writer, &worker.done_response(&request, exit_code)).await
}

async fn run_one_job<W: Worker>(
    request: &WorkerRequest,
    worker: &W,
    writer: &SharedWriter,
) -> Result<std::process::ExitStatus, WorkerError> {
    let mut child = spawn_child(request, worker)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(WorkerError::MissingPipe("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(WorkerError::MissingPipe("stderr"))?;

    let stdout_task = tokio::spawn(stream_child_output(
        request.id.clone(),
        LogStream::Stdout,
        stdout,
        Arc::clone(writer),
    ));
    let stderr_task = tokio::spawn(stream_child_output(
        request.id.clone(),
        LogStream::Stderr,
        stderr,
        Arc::clone(writer),
    ));

    let status = child.wait().await?;
    stdout_task.await??;
    stderr_task.await??;
    Ok(status)
}

fn spawn_child<W: Worker>(
    request: &WorkerRequest,
    worker: &W,
) -> Result<tokio::process::Child, WorkerError> {
    let mut command = Command::new("sh");
    command.arg("-c").arg(worker.build_command(request));
    // Detach the job from the worker's own stdin. The worker reads its JSONL
    // request protocol from fd 0; if a job child inherited that fd, a process in
    // its tree (notably Node/libuv, which flips inherited stdin to O_NONBLOCK on
    // the shared open file description when it activates `process.stdin`) could
    // mark the worker's control pipe non-blocking. The worker's next protocol
    // read would then fail with EAGAIN ("Resource temporarily unavailable",
    // os error 11) on an otherwise-fine pipe, killing the resident worker. Jobs
    // never need the protocol stdin, so give them `/dev/null` instead.
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.envs(&request.env);

    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }

    command.spawn().map_err(WorkerError::from)
}

async fn stream_child_output<R>(
    id: String,
    stream: LogStream,
    reader: R,
    writer: SharedWriter,
) -> Result<(), WorkerError>
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        write_response(&writer, &WorkerResponse::log(id.clone(), stream, line)).await?;
    }

    Ok(())
}

async fn write_response(
    writer: &SharedWriter,
    response: &WorkerResponse,
) -> Result<(), WorkerError> {
    let line = serde_json::to_string(response)?;
    let mut writer = writer.lock().await;
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

impl WorkerError {
    fn is_pipe_shutdown(&self) -> bool {
        match self {
            Self::Io(error) => is_pipe_shutdown_error(error),
            _ => false,
        }
    }
}

fn is_pipe_shutdown_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
    )
}

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("join error: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("missing {0} pipe")]
    MissingPipe(&'static str),
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use tokio::io::duplex;
    use tokio::io::DuplexStream;

    use super::*;

    #[derive(Clone)]
    struct TestWorker {
        command: String,
        resolve_result: ResolveResult,
        build_calls: Arc<AtomicUsize>,
    }

    impl TestWorker {
        fn new(command: impl Into<String>) -> Self {
            Self {
                command: command.into(),
                resolve_result: ResolveResult::accept(),
                build_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Worker for TestWorker {
        fn resolve_task(&self, _req: &ResolveTask) -> ResolveResult {
            self.resolve_result.clone()
        }

        fn build_command(&self, _req: &WorkerRequest) -> String {
            self.build_calls.fetch_add(1, Ordering::SeqCst);
            self.command.clone()
        }
    }

    fn writer_pair() -> (SharedWriter, DuplexStream) {
        let (writer_stream, reader) = duplex(16 * 1024);
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer_stream)));
        (writer, reader)
    }

    async fn read_responses(reader: DuplexStream) -> Vec<WorkerResponse> {
        let mut lines = BufReader::new(reader).lines();
        let mut responses = Vec::new();
        while let Some(line) = lines.next_line().await.expect("read line") {
            responses.push(serde_json::from_str(&line).expect("decode response"));
        }
        responses
    }

    #[tokio::test]
    async fn build_command_is_invoked_and_executed() {
        let worker = TestWorker::new("printf 'alpha\\n' && printf 'beta\\n' >&2");
        let request = WorkerRequest::new("job-1", "ignored");
        let (writer, reader) = writer_pair();

        let status = run_one_job(&request, &worker, &writer)
            .await
            .expect("job runs");
        drop(writer);
        let responses = read_responses(reader).await;

        assert!(status.success());
        assert_eq!(worker.build_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            responses,
            vec![
                WorkerResponse::log("job-1", LogStream::Stdout, "alpha"),
                WorkerResponse::log("job-1", LogStream::Stderr, "beta"),
            ]
        );
    }

    #[tokio::test]
    async fn handle_request_emits_terminal_done_on_success() {
        let worker = Arc::new(TestWorker::new("printf 'hello\\n'"));
        let shutdown = Arc::new(AtomicBool::new(false));
        let request = WorkerRequest::new("pkg#task", "ignored");
        let (writer, reader) = writer_pair();

        handle_request(request, worker, Arc::clone(&writer), Arc::clone(&shutdown))
            .await
            .expect("handle request succeeds");
        drop(writer);
        let responses = read_responses(reader).await;

        assert_eq!(
            responses,
            vec![
                WorkerResponse::log("pkg#task", LogStream::Stdout, "hello"),
                WorkerResponse::done("pkg#task", 0),
            ]
        );
        assert!(!shutdown.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn handle_request_emits_terminal_done_on_failure() {
        let worker = Arc::new(TestWorker::new("echo should not run"));
        let shutdown = Arc::new(AtomicBool::new(false));
        let request = WorkerRequest::new("pkg#task", "ignored")
            .with_cwd("/definitely/missing/luchta-worker-test");
        let (writer, reader) = writer_pair();

        handle_request(request, worker, Arc::clone(&writer), Arc::clone(&shutdown))
            .await
            .expect("handle request succeeds after spawn failure");
        drop(writer);
        let responses = read_responses(reader).await;

        assert_eq!(responses, vec![WorkerResponse::done("pkg#task", 1)]);
        assert!(!shutdown.load(Ordering::SeqCst));
    }

    /// Regression: a job child must NOT inherit the worker's protocol stdin
    /// (fd 0). If it did, a process in the job tree could flip the worker's
    /// control pipe to O_NONBLOCK (Node/libuv does this on the shared open file
    /// description), making the worker's next protocol read fail with EAGAIN
    /// ("Resource temporarily unavailable", os error 11) and killing the worker.
    /// `spawn_child` gives jobs `/dev/null` on stdin, so reading the child's
    /// stdin yields immediate EOF (empty) rather than blocking on or mutating an
    /// inherited pipe.
    #[cfg(unix)]
    #[tokio::test]
    async fn job_child_stdin_is_detached_from_worker_protocol_stdin() {
        // The child reads its own stdin to completion and echoes how many bytes
        // it saw. With `/dev/null` as stdin this is always 0 and returns at once.
        // If stdin were an inherited pipe with no data, `cat` would block forever
        // and this test would hang — so a prompt, "count: 0" result proves the
        // detach.
        let worker = TestWorker::new("printf 'count: %s\\n' \"$(cat | wc -c)\"");
        let request = WorkerRequest::new("job-1", "ignored");
        let (writer, reader) = writer_pair();

        let status = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_one_job(&request, &worker, &writer),
        )
        .await
        .expect("job must not hang on inherited stdin")
        .expect("job runs");
        drop(writer);
        let responses = read_responses(reader).await;

        assert!(status.success());
        assert_eq!(
            responses,
            vec![WorkerResponse::log("job-1", LogStream::Stdout, "count: 0")]
        );
    }

    #[test]
    fn shell_single_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_single_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn broken_pipe_errors_are_treated_as_pipe_shutdown() {
        assert!(is_pipe_shutdown_error(&std::io::Error::from(
            std::io::ErrorKind::BrokenPipe,
        )));
        assert!(
            WorkerError::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
                .is_pipe_shutdown()
        );
        assert!(!is_pipe_shutdown_error(&std::io::Error::other("boom")));
    }
}
