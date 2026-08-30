use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;

use luchta_worker::{
    run_concurrent_middleware, split_current_process_argv, version_requested,
    write_worker_response, DelegateHandle, ProxyError, ResolveResult, ResolveTask, SharedWriter,
    WorkerMessage, WorkerResponse,
};
use tokio::io::stdout;
use tokio::process::Command;
use tokio::sync::Mutex;

/// Bound on how long the delegate may take to answer a forwarded `resolve`.
/// `resolve` runs during graph build and must be fast; a delegate that is alive
/// but never responds would otherwise hang the build. A timeout fails the worker.
const RESOLVE_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Bound on how long the predicate command may run during `resolve`. A predicate
/// that hangs (e.g. waiting on stdin or deadlocked) would otherwise hang the
/// whole graph-build phase. A timeout is treated as no match and prunes the task;
/// predicate evaluation errors fail the worker.
const PREDICATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
        "usage: luchta-command-filter <predicate-token> [<predicate-token>...] -- <delegate command...>";

    // stage_args includes argv[0] (the wrapper binary name); drop it so the
    // predicate is the user-supplied tokens before `--`.
    let predicate = Arc::new(argv.stage_args.into_iter().skip(1).collect::<Vec<_>>());

    if predicate.is_empty() {
        eprintln!("missing predicate command; {usage}");
        return 1;
    }
    if argv.delegate_command.is_empty() {
        eprintln!("missing delegate command; {usage}");
        return 1;
    }

    let stdout_writer: SharedWriter = Arc::new(Mutex::new(Box::new(stdout())));
    let delegate = Arc::new(DelegateHandle::with_writers(
        argv.delegate_command,
        Arc::clone(&stdout_writer),
        Arc::new(Mutex::new(Box::new(tokio::io::stderr()))),
        Some("delegate stderr: ".to_owned()),
    ));

    let mut exit_code = 0;
    let dispatch_predicate = Arc::clone(&predicate);
    let dispatch_delegate = Arc::clone(&delegate);
    let dispatch_writer = Arc::clone(&stdout_writer);
    if let Err(error) = run_concurrent_middleware(move |message| {
        let predicate = Arc::clone(&dispatch_predicate);
        let delegate = Arc::clone(&dispatch_delegate);
        let stdout_writer = Arc::clone(&dispatch_writer);
        async move { dispatch_message(predicate, &delegate, &stdout_writer, message).await }
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
    predicate: Arc<Vec<String>>,
    delegate: &DelegateHandle,
    stdout_writer: &SharedWriter,
    message: WorkerMessage,
) -> Result<(), String> {
    match message {
        WorkerMessage::ResolveTask(resolve) => {
            let request_id = resolve.id.clone();
            match predicate_passes(&predicate, &resolve).await {
                Ok(true) => {
                    if let Err(error) = delegate
                        .send_with_timeout(
                            WorkerMessage::ResolveTask(resolve),
                            RESOLVE_FORWARD_TIMEOUT,
                        )
                        .await
                    {
                        return Err(delegate
                            .failure_message("delegate failed before resolve decision", error)
                            .await);
                    }
                    Ok(())
                }
                Ok(false) => {
                    let response = WorkerResponse::resolved(request_id, ResolveResult::prune(None));
                    write_worker_response(stdout_writer, &response)
                        .await
                        .map_err(|error| format!("failed to write prune response: {error}"))
                }
                Err(error) => Err(format!(
                    "failed to run predicate command: predicate={predicate:?}, error={error}"
                )),
            }
        }
        WorkerMessage::Run(request) => match delegate.send(WorkerMessage::Run(request)).await {
            Ok(_) => Ok(()),
            Err(error) => Err(delegate.failure_message("delegate failed", error).await),
        },
    }
}

async fn predicate_passes(predicate: &[String], resolve: &ResolveTask) -> Result<bool, ProxyError> {
    let base_dir = resolve_base_dir(resolve)?;
    let mut command = Command::new(&predicate[0]);
    command.args(&predicate[1..]);
    command.current_dir(base_dir);
    command.kill_on_drop(true);
    // ResolveTask has cwd but no env payload. Predicate therefore inherits wrapper env
    // only, evaluated relative to resolved cwd or current_dir fallback.
    command.stdout(process::Stdio::null());
    command.stderr(process::Stdio::null());

    // Bound the predicate so a hung/deadlocked command cannot stall graph build.
    // `kill_on_drop(true)` reaps the child when `child` is dropped on timeout.
    let mut child = command.spawn()?;
    match tokio::time::timeout(PREDICATE_TIMEOUT, child.wait()).await {
        Ok(status) => Ok(status?.success()),
        Err(_) => {
            eprintln!(
                "predicate command timed out after {}s; pruning task",
                PREDICATE_TIMEOUT.as_secs()
            );
            // Best-effort kill; kill_on_drop also covers it when `child` drops.
            let _ = child.kill().await;
            Ok(false)
        }
    }
}

fn resolve_base_dir(resolve: &ResolveTask) -> Result<PathBuf, ProxyError> {
    match &resolve.cwd {
        Some(cwd) => Ok(Path::new(cwd).to_path_buf()),
        None => Ok(std::env::current_dir()?),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use luchta_worker::{ResolveMode, ResolveTask};

    use super::{predicate_passes, resolve_base_dir};

    fn resolve_task(cwd: Option<&str>) -> ResolveTask {
        ResolveTask {
            id: "job-test".to_owned(),
            name: "build".to_owned(),
            command: "echo hi".to_owned(),
            package: "@repo/app".to_owned(),
            cwd: cwd.map(str::to_owned),
            scripts: vec!["build".to_owned()],
            inputs: Vec::new(),
            mode: ResolveMode::Run,
        }
    }

    #[test]
    fn resolve_base_dir_uses_cwd_when_present() {
        let base_dir = resolve_base_dir(&resolve_task(Some("packages/app"))).expect("base dir");
        assert_eq!(base_dir, PathBuf::from("packages/app"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn predicate_passes_on_zero_exit() {
        let passed = predicate_passes(
            &["sh".to_owned(), "-c".to_owned(), "exit 0".to_owned()],
            &resolve_task(None),
        )
        .await
        .expect("predicate status");
        assert!(passed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn predicate_fails_on_nonzero_exit() {
        let passed = predicate_passes(
            &["sh".to_owned(), "-c".to_owned(), "exit 7".to_owned()],
            &resolve_task(None),
        )
        .await
        .expect("predicate status");
        assert!(!passed);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn predicate_fails_on_signal_exit() {
        let passed = predicate_passes(
            &["sh".to_owned(), "-c".to_owned(), "kill -TERM $$".to_owned()],
            &resolve_task(None),
        )
        .await
        .expect("predicate status");
        assert!(!passed);
    }
}
