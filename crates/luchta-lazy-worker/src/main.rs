use std::process;
use std::sync::Arc;

use luchta_worker::{
    split_current_process_argv, DelegateHandle, LogStream, ProxyError, ResolveResult,
    WorkerMessage, WorkerResponse,
};
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>;

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
    if split.delegate_command.is_empty() {
        eprintln!("usage: luchta-lazy-worker -- <delegate command> [args...]");
        return 2;
    }

    let stdout_writer: SharedWriter = Arc::new(Mutex::new(Box::new(stdout())));
    let delegate = DelegateHandle::with_writers(
        split.delegate_command,
        Arc::clone(&stdout_writer),
        Arc::new(Mutex::new(Box::new(tokio::io::stderr()))),
        None,
    );

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

        match message {
            WorkerMessage::ResolveTask(resolve) => {
                let response = WorkerResponse::resolved(resolve.id, ResolveResult::accept());
                if let Err(error) = write_response(&stdout_writer, &response).await {
                    eprintln!("failed to write resolve response: {error}");
                    exit_code = 1;
                    break;
                }
            }
            WorkerMessage::Run(request) => {
                let request_id = request.id.clone();
                match delegate.send(WorkerMessage::Run(request)).await {
                    Ok(_) => {}
                    Err(error) => {
                        let line = format!("delegate failed before done: {error}");
                        if let Err(write_error) = write_response(
                            &stdout_writer,
                            &WorkerResponse::log(&request_id, LogStream::Stderr, line),
                        )
                        .await
                        {
                            eprintln!("failed to write delegate error log: {write_error}");
                            exit_code = 1;
                            break;
                        }
                        if let Err(write_error) =
                            write_response(&stdout_writer, &WorkerResponse::done(&request_id, 1))
                                .await
                        {
                            eprintln!("failed to write delegate error done: {write_error}");
                            exit_code = 1;
                            break;
                        }
                    }
                }
            }
        }
    }

    if let Err(error) = delegate.shutdown().await {
        eprintln!("failed to shut down delegate: {error}");
        exit_code = 1;
    }

    exit_code
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
