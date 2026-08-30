use std::path::Path;
use std::process;
use std::sync::Arc;

use globset::{GlobSet, GlobSetBuilder};
use luchta_worker::{
    run_concurrent_middleware, split_current_process_argv, version_requested,
    write_worker_response, DelegateHandle, ProxyError, ResolveResult, SharedWriter, WorkerMessage,
    WorkerResponse,
};
use tokio::io::stdout;
use tokio::sync::Mutex;
use walkdir::WalkDir;

/// Bound on how long the delegate may take to answer a forwarded `resolve`.
/// `resolve` runs during graph build and must be fast; a delegate that is alive
/// but never responds would otherwise hang the whole build. A timeout fails the
/// worker.
const RESOLVE_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

struct DispatchContext {
    patterns: Vec<String>,
    globs: GlobSet,
    delegate: Arc<DelegateHandle>,
    stdout_writer: SharedWriter,
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
    let delegate = Arc::new(DelegateHandle::with_writers(
        argv.delegate_command,
        Arc::clone(&stdout_writer),
        Arc::new(Mutex::new(Box::new(tokio::io::stderr()))),
        Some("delegate stderr: ".to_owned()),
    ));

    let mut exit_code = 0;
    let context = Arc::new(DispatchContext {
        patterns,
        globs,
        delegate: Arc::clone(&delegate),
        stdout_writer,
    });
    if let Err(error) = run_concurrent_middleware(move |message| {
        let context = Arc::clone(&context);
        async move { dispatch_message(context, message).await }
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
    context: Arc<DispatchContext>,
    message: WorkerMessage,
) -> Result<(), String> {
    match message {
        WorkerMessage::ResolveTask(resolve) => {
            let request_id = resolve.id.clone();
            let evaluation_context = Arc::clone(&context);
            let (resolve, matches) = tokio::task::spawn_blocking(move || {
                let matches = resolve_matches_any_pattern(&resolve, &evaluation_context.globs);
                (resolve, matches)
            })
            .await
            .map_err(|error| format!("file-exists filter task failed: {error}"))?;

            match matches {
                Ok(true) => {
                    if let Err(error) = context
                        .delegate
                        .send_with_timeout(
                            WorkerMessage::ResolveTask(resolve),
                            RESOLVE_FORWARD_TIMEOUT,
                        )
                        .await
                    {
                        return Err(context
                            .delegate
                            .failure_message("delegate failed before resolve decision", error)
                            .await);
                    }
                    Ok(())
                }
                Ok(false) => {
                    let response = WorkerResponse::resolved(request_id, ResolveResult::prune(None));
                    write_worker_response(&context.stdout_writer, &response)
                        .await
                        .map_err(|error| format!("failed to write prune response: {error}"))
                }
                Err(error) => Err(format!(
                    "failed to evaluate file-exists patterns: patterns={:?}, error={error}",
                    context.patterns
                )),
            }
        }
        WorkerMessage::Run(request) => {
            match context.delegate.send(WorkerMessage::Run(request)).await {
                Ok(_) => Ok(()),
                Err(error) => Err(context
                    .delegate
                    .failure_message("delegate failed", error)
                    .await),
            }
        }
    }
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(luchta_glob::build_path_glob(pattern)?);
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

#[cfg(test)]
mod tests {
    use super::build_globset;

    #[test]
    fn single_star_does_not_cross_directory_separator() {
        let globs = build_globset(&["config/*.json".to_string()]).expect("build globset");

        assert!(globs.is_match("config/babel.json"));
        assert!(!globs.is_match("config/nested/babel.json"));
    }
}
