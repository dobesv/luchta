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
//! - the run delegate's response writer suppresses only the matching resolve id
//!   while the wrapper awaits its terminal `Resolved` response, so unrelated
//!   concurrent run output continues streaming
//!
//! Precedence:
//! - if resolve worker returns `Modify` and run delegate returns `Accept`,
//!   resolve worker's modification intentionally wins per issue #253 acceptance
//!   criteria
//! - if both modify, or delegate prunes/rejects, delegate decision wins

use std::process;
use std::sync::Arc;
use std::time::Duration;

mod response_filter;

use luchta_worker::{
    run_concurrent_middleware, split_current_process_argv, version_requested,
    write_worker_response, DelegateHandle, ProxyError, ResolveDecision, ResolveResult, ResolveTask,
    SharedWriter, TaskModification, WorkerMessage, WorkerRequest, WorkerResponse,
};
use tokio::io::stdout;
use tokio::sync::Mutex;

use response_filter::{shared_response_writer, ResponseFilter};

const RESOLVE_FORWARD_TIMEOUT: Duration = Duration::from_secs(30);

struct App {
    stdout_writer: SharedWriter,
    resolve_delegate: DelegateHandle,
    run_delegate: DelegateHandle,
    run_response_filter: ResponseFilter,
}

#[derive(Clone, Copy)]
enum DelegateOutput<'a> {
    Forward,
    Suppress(&'a ResponseFilter),
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

    let app = Arc::new(build_app(resolve_cmd, delegate_cmd));
    let mut exit_code = 0;
    let dispatch_app = Arc::clone(&app);
    if let Err(error) = run_concurrent_middleware(move |message| {
        let app = Arc::clone(&dispatch_app);
        async move { dispatch_message(&app, message).await }
    })
    .await
    {
        eprintln!("{error}");
        exit_code = 1;
    }

    if let Err(error) = app.resolve_delegate.shutdown().await {
        eprintln!(
            "failed to shut down resolve delegate: command={:?}, error={error}",
            app.resolve_delegate.delegate_command()
        );
        exit_code = 1;
    }
    if let Err(error) = app.run_delegate.shutdown().await {
        eprintln!(
            "failed to shut down run delegate: command={:?}, error={error}",
            app.run_delegate.delegate_command()
        );
        exit_code = 1;
    }

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
    let stderr_writer: SharedWriter = Arc::new(Mutex::new(Box::new(tokio::io::stderr())));
    let resolve_sink: SharedWriter = Arc::new(Mutex::new(Box::new(tokio::io::sink())));
    let run_response_filter = ResponseFilter::default();
    // Delegate and synthetic responses must share this outer lock. The proxy
    // holds it across each JSON body, newline, and flush, making the complete
    // JSONL record the serialization boundary for both output paths.
    let stdout_writer = shared_response_writer(stdout(), run_response_filter.clone());

    let resolve_delegate = DelegateHandle::with_writers(
        resolve_cmd,
        resolve_sink,
        Arc::clone(&stderr_writer),
        Some("resolve stderr: ".to_owned()),
    );
    let run_delegate = DelegateHandle::with_writers(
        delegate_cmd,
        Arc::clone(&stdout_writer),
        stderr_writer,
        Some("delegate stderr: ".to_owned()),
    );

    App {
        stdout_writer,
        resolve_delegate,
        run_delegate,
        run_response_filter,
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
    let result = match resolve_via_delegate(
        &app.resolve_delegate,
        resolve.clone(),
        DelegateOutput::Forward,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return Err(app
                .resolve_delegate
                .failure_message("delegate failed before resolve decision", error)
                .await);
        }
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
    match resolve_via_delegate(
        &app.run_delegate,
        resolve,
        DelegateOutput::Suppress(&app.run_response_filter),
    )
    .await
    {
        Ok(result) => emit_resolve_result(app, resolve_id, result).await,
        Err(error) => Err(app
            .run_delegate
            .failure_message("delegate failed before resolve decision", error)
            .await),
    }
}

async fn handle_modify(
    app: &App,
    resolve_id: String,
    original_resolve: ResolveTask,
    modification: TaskModification,
) -> Result<(), String> {
    let modified_resolve = apply_modification(&original_resolve, &modification);
    let delegate_result = match resolve_via_delegate(
        &app.run_delegate,
        modified_resolve,
        DelegateOutput::Suppress(&app.run_response_filter),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return Err(app
                .run_delegate
                .failure_message("delegate failed before resolve decision", error)
                .await);
        }
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
        return Err(app
            .run_delegate
            .failure_message("delegate failed", error)
            .await);
    }

    Ok(())
}

async fn resolve_via_delegate(
    delegate: &DelegateHandle,
    resolve: ResolveTask,
    output: DelegateOutput<'_>,
) -> Result<ResolveResult, ProxyError> {
    if let DelegateOutput::Suppress(filter) = output {
        filter.suppress(resolve.id.clone());
    }
    let message = WorkerMessage::ResolveTask(resolve);
    // Keep the id suppressed if the delegate fails or times out. The request
    // error terminates this worker, and a late internal response must not leak
    // onto stdout while delegate shutdown is in progress.
    let response = delegate
        .send_with_timeout(message, RESOLVE_FORWARD_TIMEOUT)
        .await?;

    match response {
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
    write_worker_response(&app.stdout_writer, &response)
        .await
        .map_err(|error| format!("failed to write resolve response: {error}"))
}
