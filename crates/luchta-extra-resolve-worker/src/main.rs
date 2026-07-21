//! Middleware worker that combines resolve-time and run-time delegates.
//!
//! Contract:
//! - resolve worker handles `ResolveTask` first during graph build
//! - run delegate handles `Run` requests directly
//! - when resolve worker accepts or modifies, wrapper forwards resolve request to
//!   run delegate for fallback/final decision
//!
//! Rationale:
//! - resolve worker stdout is always sunk because wrapper must merge and emit one
//!   final resolve response itself
//! - run delegate normally auto-forwards to real stdout for streaming run-phase
//!   logs/done messages
//! - during resolve-phase forwards, wrapper temporarily switches run delegate
//!   stdout to sink to prevent duplicate protocol output while still awaiting its
//!   terminal `Resolved` response
//!
//! Precedence:
//! - if resolve worker returns `Modify` and run delegate returns `Accept`,
//!   resolve worker's modification intentionally wins per issue #253 acceptance
//!   criteria
//! - if both modify, or delegate prunes/rejects, delegate decision wins

use std::pin::Pin;
use std::process;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use luchta_worker::{
    split_current_process_argv, version_requested, DelegateHandle, ProxyError, ResolveDecision,
    ResolveResult, ResolveTask, TaskModification, WorkerMessage, WorkerRequest, WorkerResponse,
};
use tokio::io::{
    sink, stdin, stdout, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Sink, Stdout,
};
use tokio::sync::Mutex;

type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>;

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
const FORWARD_REAL: u8 = 0;
const FORWARD_SINK: u8 = 1;

struct App {
    stdout_writer: SharedWriter,
    resolve_delegate: DelegateHandle,
    run_delegate: DelegateHandle,
    run_forward_mode: Arc<AtomicU8>,
}

struct SwitchableStdoutWriter {
    mode: Arc<AtomicU8>,
    real: Stdout,
    sink: Sink,
}

impl SwitchableStdoutWriter {
    fn new(mode: Arc<AtomicU8>) -> Self {
        Self {
            mode,
            real: stdout(),
            sink: sink(),
        }
    }

    fn active(self: Pin<&mut Self>) -> ActiveWriter<'_> {
        let this = self.get_mut();
        if this.mode.load(Ordering::SeqCst) == FORWARD_SINK {
            ActiveWriter::Sink(Pin::new(&mut this.sink))
        } else {
            ActiveWriter::Stdout(Pin::new(&mut this.real))
        }
    }
}

enum ActiveWriter<'a> {
    Sink(Pin<&'a mut Sink>),
    Stdout(Pin<&'a mut Stdout>),
}

impl ActiveWriter<'_> {
    fn with<R>(self, f: impl FnOnce(Pin<&mut (dyn AsyncWrite + Send)>) -> R) -> R {
        match self {
            Self::Sink(writer) => f(writer),
            Self::Stdout(writer) => f(writer),
        }
    }
}

impl AsyncWrite for SwitchableStdoutWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.active().with(|writer| writer.poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.active().with(|writer| writer.poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.active().with(|writer| writer.poll_shutdown(cx))
    }
}

fn main() {
    let exit_code = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(async_main()),
        Err(error) => {
            eprintln!("failed to build tokio runtime: {error}");
            1
        }
    };

    if exit_code != 0 {
        process::exit(exit_code);
    }
}

async fn async_main() -> i32 {
    let (resolve_cmd, delegate_cmd) = match parse_args() {
        Ok(commands) => commands,
        Err(exit_code) => return exit_code,
    };

    let app = build_app(resolve_cmd, delegate_cmd);
    let mut exit_code = 0;
    let mut lines = BufReader::new(stdin()).lines();

    loop {
        let Some(line) = (match lines.next_line().await {
            Ok(line) => line,
            Err(error) => {
                eprintln!("failed to read worker stdin: {error}");
                exit_code = 1;
                break;
            }
        }) else {
            break;
        };

        let message = match serde_json::from_str::<WorkerMessage>(&line) {
            Ok(message) => message,
            Err(error) => {
                eprintln!("failed to parse worker message: {error}");
                exit_code = 1;
                break;
            }
        };

        if let Err(message) = dispatch_message(&app, message).await {
            eprintln!("{message}");
            exit_code = 1;
            break;
        }
    }

    let _ = app.resolve_delegate.shutdown().await;
    let _ = app.run_delegate.shutdown().await;

    exit_code
}

fn parse_args() -> Result<(Vec<String>, Vec<String>), i32> {
    let argv = split_current_process_argv();
    if version_requested(
        &argv.stage_args,
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ) {
        return Err(0);
    }

    let usage = "usage: luchta-extra-resolve-worker <resolve-command...> -- <delegate-command...>";
    let resolve_cmd: Vec<String> = argv.stage_args.into_iter().skip(1).collect();
    let delegate_cmd = argv.delegate_command;
    if resolve_cmd.is_empty() || delegate_cmd.is_empty() {
        eprintln!("{usage}");
        return Err(1);
    }

    Ok((resolve_cmd, delegate_cmd))
}

fn build_app(resolve_cmd: Vec<String>, delegate_cmd: Vec<String>) -> App {
    let stdout_writer: SharedWriter = Arc::new(Mutex::new(Box::new(stdout())));
    let stderr_writer: SharedWriter = Arc::new(Mutex::new(Box::new(tokio::io::stderr())));
    let resolve_sink: SharedWriter = Arc::new(Mutex::new(Box::new(tokio::io::sink())));
    let run_forward_mode = Arc::new(AtomicU8::new(FORWARD_REAL));
    let run_forward_writer: SharedWriter = Arc::new(Mutex::new(Box::new(
        SwitchableStdoutWriter::new(Arc::clone(&run_forward_mode)),
    )));

    let resolve_delegate = DelegateHandle::with_writers(
        resolve_cmd,
        resolve_sink,
        Arc::clone(&stderr_writer),
        Some("resolve stderr: ".to_owned()),
    );
    let run_delegate = DelegateHandle::with_writers(
        delegate_cmd,
        run_forward_writer,
        stderr_writer,
        Some("delegate stderr: ".to_owned()),
    );

    App {
        stdout_writer,
        resolve_delegate,
        run_delegate,
        run_forward_mode,
    }
}

async fn dispatch_message(app: &App, message: WorkerMessage) -> Result<(), String> {
    match message {
        WorkerMessage::ResolveTask(resolve) => handle_resolve_task(app, resolve).await,
        WorkerMessage::Run(request) => handle_run_request(app, request).await,
    }
}

async fn handle_resolve_task(app: &App, resolve: ResolveTask) -> Result<(), String> {
    let resolve_id = resolve.id.clone();
    let result = match resolve_via_delegate(&app.resolve_delegate, resolve.clone()).await {
        Ok(result) => result,
        Err(_) => return emit_prune(app, resolve_id).await,
    };

    match result.decision {
        ResolveDecision::Prune { reason } => {
            emit_resolve_result(app, resolve_id, ResolveResult::prune(reason)).await
        }
        ResolveDecision::Reject { message } => {
            emit_resolve_result(app, resolve_id, ResolveResult::reject(message)).await
        }
        ResolveDecision::Accept => handle_accept(app, resolve_id, resolve).await,
        ResolveDecision::Modify(modification) => {
            handle_modify(app, resolve_id, resolve, modification).await
        }
    }
}

async fn handle_accept(app: &App, resolve_id: String, resolve: ResolveTask) -> Result<(), String> {
    match resolve_via_run_delegate(app, resolve).await {
        Ok(result) => emit_resolve_result(app, resolve_id, result).await,
        Err(_) => emit_prune(app, resolve_id).await,
    }
}

async fn handle_modify(
    app: &App,
    resolve_id: String,
    original_resolve: ResolveTask,
    modification: TaskModification,
) -> Result<(), String> {
    let modified_resolve = apply_modification(&original_resolve, &modification);
    let delegate_result = match resolve_via_run_delegate(app, modified_resolve).await {
        Ok(result) => result,
        Err(_) => return emit_prune(app, resolve_id).await,
    };

    match delegate_result.decision {
        // Intentional precedence per issue #253 acceptance criteria.
        ResolveDecision::Accept => {
            emit_resolve_result(app, resolve_id, ResolveResult::modify(modification)).await
        }
        _ => emit_resolve_result(app, resolve_id, delegate_result).await,
    }
}

async fn handle_run_request(app: &App, request: WorkerRequest) -> Result<(), String> {
    if let Err(error) = app.run_delegate.send(WorkerMessage::Run(request)).await {
        let exit = match app.run_delegate.exit_status().await {
            Some(status) => status.to_string(),
            None => "<unknown>".to_owned(),
        };
        return Err(format!(
            "delegate failed: command={:?}, exit={}, error={}",
            app.run_delegate.delegate_command(),
            exit,
            error
        ));
    }

    Ok(())
}

async fn resolve_via_delegate(
    delegate: &DelegateHandle,
    resolve: ResolveTask,
) -> Result<ResolveResult, ProxyError> {
    match delegate
        .send_with_timeout(WorkerMessage::ResolveTask(resolve), RESOLVE_TIMEOUT)
        .await?
    {
        WorkerResponse::Resolved { result, .. } => Ok(result),
        _ => Err(ProxyError::DelegateClosed(
            "delegate returned non-resolved response for resolve task".to_owned(),
        )),
    }
}

async fn resolve_via_run_delegate(
    app: &App,
    resolve: ResolveTask,
) -> Result<ResolveResult, ProxyError> {
    app.run_forward_mode.store(FORWARD_SINK, Ordering::SeqCst);
    let result = app
        .run_delegate
        .send_with_timeout(WorkerMessage::ResolveTask(resolve), RESOLVE_TIMEOUT)
        .await;
    app.run_forward_mode.store(FORWARD_REAL, Ordering::SeqCst);

    match result? {
        WorkerResponse::Resolved { result, .. } => Ok(result),
        _ => Err(ProxyError::DelegateClosed(
            "delegate returned non-resolved response for resolve task".to_owned(),
        )),
    }
}

fn apply_modification(resolve: &ResolveTask, modification: &TaskModification) -> ResolveTask {
    ResolveTask {
        command: modification
            .command
            .clone()
            .unwrap_or_else(|| resolve.command.clone()),
        inputs: modification
            .inputs
            .clone()
            .unwrap_or_else(|| resolve.inputs.clone()),
        ..resolve.clone()
    }
}

async fn emit_resolve_result(
    app: &App,
    resolve_id: String,
    result: ResolveResult,
) -> Result<(), String> {
    let response = WorkerResponse::resolved(resolve_id, result);
    write_response(&app.stdout_writer, &response)
        .await
        .map_err(|error| format!("failed to write resolve response: {error}"))
}

async fn emit_prune(app: &App, resolve_id: String) -> Result<(), String> {
    emit_resolve_result(app, resolve_id, ResolveResult::prune(None)).await
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
