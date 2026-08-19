use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use thiserror::Error;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::process_items_in_parallel;
use crate::{
    is_valid_report_filename, LogStream, ResolveResult, ResolveTask, WorkerMessage, WorkerRequest,
    WorkerResponse,
};
use crate::{ItemProgress, ParallelProgress, TaskProgress};

pub trait Worker: Send + Sync + 'static {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult;
    fn build_command(&self, req: &WorkerRequest) -> String;

    fn run_in_process(
        &self,
        _req: &WorkerRequest,
        _ctx: &JobContext,
    ) -> impl std::future::Future<Output = InProcessOutcome> + Send {
        async { InProcessOutcome::NotHandled }
    }

    fn done_response(&self, req: &WorkerRequest, exit_code: i32) -> WorkerResponse {
        WorkerResponse::done(req.id.clone(), exit_code)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InProcessOutcome {
    NotHandled,
    Done {
        exit_code: i32,
        outputs: Option<Vec<String>>,
    },
}

type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>;

#[derive(Clone)]
pub struct JobContext {
    id: String,
    writer: SharedWriter,
    progress_enabled: bool,
}

impl JobContext {
    /// Construct a job context for a worker request.
    pub fn new(id: String, writer: SharedWriter) -> Self {
        Self::with_progress(id, writer, false)
    }

    /// Construct a context with an explicit progress capability, primarily for
    /// in-process worker composition and tests. Protocol runtimes derive this
    /// value from [`WorkerRequest::progress`](crate::WorkerRequest::progress).
    pub fn with_progress(id: String, writer: SharedWriter, progress_enabled: bool) -> Self {
        Self {
            id,
            writer,
            progress_enabled,
        }
    }

    fn negotiated(id: String, writer: SharedWriter, progress_enabled: bool) -> Self {
        Self::with_progress(id, writer, progress_enabled)
    }

    pub async fn emit_stdout(&self, line: impl Into<String>) -> Result<(), WorkerError> {
        write_response(
            &self.writer,
            &WorkerResponse::log(self.id.clone(), LogStream::Stdout, line.into()),
        )
        .await
    }

    /// Emit many stdout lines with a single writer lock and flush.
    ///
    /// Prefer this over calling [`Self::emit_stdout`] in a loop: each single
    /// emit serializes, locks, writes, and flushes the shared pipe, which is
    /// a syscall per line.
    pub async fn emit_stdout_lines<I>(&self, lines: I) -> Result<(), WorkerError>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let mut buffer = Vec::new();
        for line in lines {
            let response = WorkerResponse::log(self.id.clone(), LogStream::Stdout, line.into());
            serde_json::to_writer(&mut buffer, &response)?;
            buffer.push(b'\n');
        }
        if buffer.is_empty() {
            return Ok(());
        }
        let mut writer = self.writer.lock().await;
        writer.write_all(&buffer).await?;
        writer.flush().await?;
        Ok(())
    }

    pub async fn emit_stderr(&self, line: impl Into<String>) -> Result<(), WorkerError> {
        write_response(
            &self.writer,
            &WorkerResponse::log(self.id.clone(), LogStream::Stderr, line.into()),
        )
        .await
    }

    pub async fn emit_report(
        &self,
        filename: impl Into<String>,
        mime_type: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<(), WorkerError> {
        let filename = filename.into();
        if !is_valid_report_filename(&filename) {
            self.emit_stderr(format!("ignored invalid report filename: {filename}"))
                .await?;
            return Ok(());
        }

        write_response(
            &self.writer,
            &WorkerResponse::report(self.id.clone(), filename, mime_type.into(), content.into()),
        )
        .await
    }

    /// Emit an absolute progress snapshot when the engine negotiated support.
    pub async fn emit_progress(&self, progress: TaskProgress) -> Result<(), WorkerError> {
        if !self.progress_enabled {
            return Ok(());
        }
        write_response(
            &self.writer,
            &WorkerResponse::progress(self.id.clone(), progress),
        )
        .await
    }

    /// Observe shared item counters while `work` runs.
    ///
    /// An initial snapshot and a final snapshot are always emitted when
    /// negotiated. Changed intermediate snapshots are emitted at most once per
    /// 250 milliseconds.
    pub async fn run_with_progress<F, T>(
        &self,
        progress: &ItemProgress,
        work: F,
    ) -> Result<T, WorkerError>
    where
        F: std::future::Future<Output = T>,
    {
        if !self.progress_enabled {
            return Ok(work.await);
        }

        let mut last = progress.snapshot();
        self.emit_progress(last).await?;
        tokio::pin!(work);
        let start = tokio::time::Instant::now() + Duration::from_millis(250);
        let mut updates = tokio::time::interval_at(start, Duration::from_millis(250));
        updates.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let output = loop {
            tokio::select! {
                output = &mut work => break output,
                _ = updates.tick() => {
                    let snapshot = progress.snapshot();
                    if snapshot != last {
                        self.emit_progress(snapshot).await?;
                        last = snapshot;
                    }
                }
            }
        };

        self.emit_progress(progress.snapshot()).await?;
        Ok(output)
    }

    /// Process owned items on blocking threads while reporting coherent item
    /// progress. The blocking task is not spawned until after the initial
    /// pending snapshot has been emitted.
    pub async fn process_items_with_progress<I, T, F>(
        &self,
        items: Vec<I>,
        context: ParallelProgress,
        process: F,
    ) -> Result<Vec<T>, String>
    where
        I: Send + Sync + 'static,
        T: Send + 'static,
        F: Fn(&I, crate::ItemProgressGuard) -> T + Send + Sync + 'static,
    {
        let progress = ItemProgress::new(items.len());
        let processing_progress = progress.clone();
        let processing = async move {
            tokio::task::spawn_blocking(move || {
                process_items_in_parallel(&items, context.panic_message, |item| {
                    let progress_item = processing_progress.start_item();
                    process(item, progress_item)
                })
            })
            .await
        };

        let joined = self
            .run_with_progress(&progress, processing)
            .await
            .map_err(|error| format!("failed to emit {} progress: {error}", context.worker_name))?;
        joined.map_err(|error| format!("{} parallel task failed: {error}", context.worker_name))?
    }
}

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
        // Resolve delegates do blocking filesystem work (config discovery,
        // directory walks). Run them off the event loop so they don't stall
        // protocol reads and log writes, and so concurrent resolves don't
        // serialize behind each other on the current-thread runtime.
        let result = tokio::task::spawn_blocking(move || worker.resolve_task(&resolve))
            .await
            .unwrap_or_else(|join_error| {
                ResolveResult::reject(format!("resolve delegate panicked: {join_error}"))
            });
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
    let context = JobContext::negotiated(id.clone(), Arc::clone(&writer), request.progress);
    let done_response = match worker.run_in_process(&request, &context).await {
        InProcessOutcome::Done { exit_code, outputs } => {
            WorkerResponse::done_with_outputs(id.clone(), exit_code, outputs)
        }
        InProcessOutcome::NotHandled => {
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
            worker.done_response(&request, exit_code)
        }
    };

    write_response(&writer, &done_response).await
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
    // Clear all inherited environment variables for strict isolation.
    // The request.env contains the full effective env (whitelist + declared).
    command.env_clear();
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
        in_process: Option<InProcessBehavior>,
    }

    #[derive(Clone)]
    enum InProcessBehavior {
        EmitAndFinish,
        InvalidReportOnly,
    }

    impl TestWorker {
        fn new(command: impl Into<String>) -> Self {
            Self {
                command: command.into(),
                resolve_result: ResolveResult::accept(),
                build_calls: Arc::new(AtomicUsize::new(0)),
                in_process: None,
            }
        }

        fn with_in_process(mut self, behavior: InProcessBehavior) -> Self {
            self.in_process = Some(behavior);
            self
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

        fn run_in_process(
            &self,
            _req: &WorkerRequest,
            ctx: &JobContext,
        ) -> impl std::future::Future<Output = InProcessOutcome> + Send {
            let behavior = self.in_process.clone();
            let ctx = ctx.clone();
            async move {
                match behavior {
                    Some(InProcessBehavior::EmitAndFinish) => {
                        ctx.emit_stdout("in-process stdout")
                            .await
                            .expect("emit stdout succeeds");
                        ctx.emit_report("report.txt", "text/plain", "report-body")
                            .await
                            .expect("emit report succeeds");
                        InProcessOutcome::Done {
                            exit_code: 0,
                            outputs: None,
                        }
                    }
                    Some(InProcessBehavior::InvalidReportOnly) => {
                        ctx.emit_report("../evil", "text/plain", "report-body")
                            .await
                            .expect("invalid report is ignored");
                        InProcessOutcome::Done {
                            exit_code: 0,
                            outputs: None,
                        }
                    }
                    None => InProcessOutcome::NotHandled,
                }
            }
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
    async fn emit_stdout_lines_writes_one_log_response_per_line() {
        let (writer, reader) = writer_pair();
        let ctx = JobContext::new("job-1".to_owned(), Arc::clone(&writer));

        ctx.emit_stdout_lines(["first".to_owned(), "second".to_owned()])
            .await
            .expect("batch emit succeeds");
        ctx.emit_stdout_lines(Vec::<String>::new())
            .await
            .expect("empty batch is a no-op");
        drop(ctx);
        drop(writer);
        let responses = read_responses(reader).await;

        assert_eq!(
            responses,
            vec![
                WorkerResponse::log("job-1", LogStream::Stdout, "first"),
                WorkerResponse::log("job-1", LogStream::Stdout, "second"),
            ]
        );
    }

    #[tokio::test]
    async fn progress_is_silent_without_negotiation() {
        let (writer, reader) = writer_pair();
        let ctx = JobContext::new("job-1".to_owned(), Arc::clone(&writer));
        ctx.emit_progress(TaskProgress {
            pending: 4,
            ..TaskProgress::default()
        })
        .await
        .expect("disabled progress is a no-op");
        drop(ctx);
        drop(writer);

        assert!(read_responses(reader).await.is_empty());
    }

    #[tokio::test]
    async fn tracked_progress_emits_initial_and_final_absolute_snapshots() {
        let (writer, reader) = writer_pair();
        let ctx = JobContext::with_progress("job-1".to_owned(), Arc::clone(&writer), true);
        let progress = ItemProgress::new(2);

        ctx.run_with_progress(&progress, async {
            drop(progress.start_item());
            progress.start_item().skip();
        })
        .await
        .expect("tracked work succeeds");
        drop(ctx);
        drop(writer);

        assert_eq!(
            read_responses(reader).await,
            vec![
                WorkerResponse::progress(
                    "job-1",
                    TaskProgress {
                        pending: 2,
                        ..TaskProgress::default()
                    }
                ),
                WorkerResponse::progress(
                    "job-1",
                    TaskProgress {
                        completed: 2,
                        skipped: 1,
                        ..TaskProgress::default()
                    }
                ),
            ]
        );
    }

    #[tokio::test]
    async fn tracked_progress_emits_running_snapshot_at_rate_limited_boundary() {
        let (writer, reader) = writer_pair();
        let ctx = JobContext::with_progress("job-1".to_owned(), Arc::clone(&writer), true);
        let progress = ItemProgress::new(1);
        let release = Arc::new(tokio::sync::Notify::new());
        let controller = {
            let release = Arc::clone(&release);
            async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                release.notify_one();
            }
        };
        let work = async {
            let _item = progress.start_item();
            release.notified().await;
        };

        let (result, ()) = tokio::join!(ctx.run_with_progress(&progress, work), controller);
        result.expect("tracked work succeeds");
        drop(ctx);
        drop(writer);

        assert_eq!(
            read_responses(reader).await,
            vec![
                WorkerResponse::progress(
                    "job-1",
                    TaskProgress {
                        pending: 1,
                        ..TaskProgress::default()
                    }
                ),
                WorkerResponse::progress(
                    "job-1",
                    TaskProgress {
                        running: 1,
                        ..TaskProgress::default()
                    }
                ),
                WorkerResponse::progress(
                    "job-1",
                    TaskProgress {
                        completed: 1,
                        ..TaskProgress::default()
                    }
                ),
            ]
        );
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
    async fn handle_request_emits_in_process_log_report_then_done() {
        let worker = Arc::new(
            TestWorker::new("echo should not run")
                .with_in_process(InProcessBehavior::EmitAndFinish),
        );
        let shutdown = Arc::new(AtomicBool::new(false));
        let request = WorkerRequest::new("pkg#task", "ignored");
        let (writer, reader) = writer_pair();

        handle_request(
            request,
            worker.clone(),
            Arc::clone(&writer),
            Arc::clone(&shutdown),
        )
        .await
        .expect("handle request succeeds");
        drop(writer);
        let responses = read_responses(reader).await;

        assert_eq!(
            responses,
            vec![
                WorkerResponse::log("pkg#task", LogStream::Stdout, "in-process stdout"),
                WorkerResponse::report("pkg#task", "report.txt", "text/plain", "report-body"),
                WorkerResponse::done("pkg#task", 0),
            ]
        );
        assert_eq!(worker.build_calls.load(Ordering::SeqCst), 0);
        assert!(!shutdown.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn emit_report_rejects_invalid_filename() {
        let worker = Arc::new(
            TestWorker::new("echo should not run")
                .with_in_process(InProcessBehavior::InvalidReportOnly),
        );
        let shutdown = Arc::new(AtomicBool::new(false));
        let request = WorkerRequest::new("pkg#task", "ignored");
        let (writer, reader) = writer_pair();

        handle_request(
            request,
            worker.clone(),
            Arc::clone(&writer),
            Arc::clone(&shutdown),
        )
        .await
        .expect("handle request succeeds");
        drop(writer);
        let responses = read_responses(reader).await;

        assert_eq!(
            responses,
            vec![
                WorkerResponse::log(
                    "pkg#task",
                    LogStream::Stderr,
                    "ignored invalid report filename: ../evil",
                ),
                WorkerResponse::done("pkg#task", 0),
            ]
        );
        assert_eq!(worker.build_calls.load(Ordering::SeqCst), 0);
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
