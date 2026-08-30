use std::process;
use std::sync::Arc;

use luchta_worker::{
    run_concurrent_middleware, split_current_process_argv, version_requested,
    write_worker_response, DelegateHandle, ResolveResult, SharedWriter, WorkerMessage,
    WorkerResponse,
};
use tokio::io::stdout;
use tokio::sync::Mutex;

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
    let split = split_current_process_argv();
    if version_requested(
        &split.stage_args,
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ) {
        return 0;
    }
    if split.delegate_command.is_empty() {
        eprintln!("usage: luchta-lazy-worker -- <delegate command> [args...]");
        return 2;
    }

    let stdout_writer: SharedWriter = Arc::new(Mutex::new(Box::new(stdout())));
    let delegate = Arc::new(DelegateHandle::with_writers(
        split.delegate_command,
        Arc::clone(&stdout_writer),
        Arc::new(Mutex::new(Box::new(tokio::io::stderr()))),
        None,
    ));

    let mut exit_code = 0;
    let dispatch_delegate = Arc::clone(&delegate);
    let dispatch_writer = Arc::clone(&stdout_writer);
    if let Err(error) = run_concurrent_middleware(move |message| {
        let delegate = Arc::clone(&dispatch_delegate);
        let stdout_writer = Arc::clone(&dispatch_writer);
        async move { dispatch_message(&delegate, &stdout_writer, message).await }
    })
    .await
    {
        eprintln!("{error}");
        exit_code = 1;
    }

    if let Err(error) = delegate.shutdown().await {
        eprintln!(
            "failed to shut down delegate: command={:?}, error={}",
            delegate.delegate_command(),
            error
        );
        exit_code = 1;
    }

    exit_code
}

async fn dispatch_message(
    delegate: &DelegateHandle,
    stdout_writer: &SharedWriter,
    message: WorkerMessage,
) -> Result<(), String> {
    match message {
        WorkerMessage::ResolveTask(resolve) => {
            let response = WorkerResponse::resolved(resolve.id, ResolveResult::accept());
            write_worker_response(stdout_writer, &response)
                .await
                .map_err(|error| format!("failed to write resolve response: {error}"))
        }
        WorkerMessage::Run(request) => match delegate.send(WorkerMessage::Run(request)).await {
            Ok(_) => Ok(()),
            Err(error) => Err(delegate.failure_message("delegate failed", error).await),
        },
    }
}
