use std::path::Path;
use std::process;
use std::sync::Arc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use luchta_worker::{
    split_current_process_argv, version_requested, DelegateHandle, ProxyError, ResolveResult,
    WorkerMessage, WorkerResponse,
};
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use walkdir::WalkDir;

type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>;

/// Bound on how long the delegate may take to answer a forwarded `resolve`.
/// `resolve` runs during graph build and must be fast; a delegate that is alive
/// but never responds would otherwise hang the whole build. On timeout we treat
/// the task as pruned (clean error, no hang).
const RESOLVE_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
    let argv = split_current_process_argv();
    if version_requested(
        &argv.stage_args,
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ) {
        return 0;
    }
    let usage =
        "usage: luchta-file-exists-filter <pattern> [<pattern>...] -- <delegate command...>";

    // stage_args includes argv[0] (the wrapper binary name); drop it so the
    // patterns are exactly the user-supplied globs before `--`.
    let patterns = argv.stage_args.into_iter().skip(1).collect::<Vec<_>>();

    if patterns.is_empty() {
        eprintln!("missing pattern(s); {usage}");
        return 1;
    }
    if argv.delegate_command.is_empty() {
        eprintln!("missing delegate command; {usage}");
        return 1;
    }

    let globs = match build_globset(&patterns) {
        Ok(globs) => globs,
        Err(error) => {
            eprintln!("failed to compile file-exists patterns: {error}");
            return 1;
        }
    };

    let stdout_writer: SharedWriter = Arc::new(Mutex::new(Box::new(stdout())));
    let delegate = DelegateHandle::with_writers(
        argv.delegate_command,
        Arc::clone(&stdout_writer),
        Arc::new(Mutex::new(Box::new(tokio::io::stderr()))),
        Some("delegate stderr: ".to_owned()),
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
                let request_id = resolve.id.clone();
                match resolve_matches_any_pattern(&resolve, &globs) {
                    Ok(true) => {
                        if let Err(error) = delegate
                            .send_with_timeout(
                                WorkerMessage::ResolveTask(resolve),
                                RESOLVE_FORWARD_TIMEOUT,
                            )
                            .await
                        {
                            eprintln!("delegate failed before resolve decision: {error}");
                            let response =
                                WorkerResponse::resolved(request_id, ResolveResult::prune(None));
                            if let Err(write_error) =
                                write_response(&stdout_writer, &response).await
                            {
                                eprintln!("failed to write resolve fallback: {write_error}");
                                exit_code = 1;
                                break;
                            }
                        }
                    }
                    Ok(false) => {
                        let response =
                            WorkerResponse::resolved(request_id, ResolveResult::prune(None));
                        if let Err(error) = write_response(&stdout_writer, &response).await {
                            eprintln!("failed to write prune response: {error}");
                            exit_code = 1;
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("failed to evaluate file-exists patterns: {error}");
                        let response =
                            WorkerResponse::resolved(request_id, ResolveResult::prune(None));
                        if let Err(write_error) = write_response(&stdout_writer, &response).await {
                            eprintln!("failed to write resolve fallback: {write_error}");
                            exit_code = 1;
                            break;
                        }
                    }
                }
            }
            WorkerMessage::Run(request) => {
                if let Err(error) = delegate.send(WorkerMessage::Run(request)).await {
                    eprintln!("delegate failed: {error}");
                    exit_code = 1;
                    break;
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

fn build_globset(patterns: &[String]) -> Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern)?);
    }
    builder.build()
}

fn resolve_matches_any_pattern(
    resolve: &luchta_worker::ResolveTask,
    globs: &GlobSet,
) -> Result<bool, ProxyError> {
    let base_dir = match &resolve.cwd {
        Some(cwd) => Path::new(cwd).to_path_buf(),
        None => std::env::current_dir()?,
    };

    if !base_dir.exists() {
        return Ok(false);
    }

    for entry in WalkDir::new(&base_dir).into_iter() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!("failed to walk {}: {error}", base_dir.display());
                continue;
            }
        };

        let relative = match entry.path().strip_prefix(&base_dir) {
            Ok(relative) => relative,
            Err(_) => continue,
        };

        if relative.as_os_str().is_empty() {
            continue;
        }

        if globs.is_match(relative) {
            return Ok(true);
        }
    }

    Ok(false)
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
