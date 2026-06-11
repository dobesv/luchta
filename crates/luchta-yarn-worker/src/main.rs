use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use luchta_engine::{LogStream, WorkerRequest, WorkerResponse};
use thiserror::Error;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

type SharedWriter = Arc<Mutex<tokio::io::Stdout>>;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("worker error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), WorkerError> {
    let writer = Arc::new(Mutex::new(stdout()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut requests = BufReader::new(stdin()).lines();
    let mut jobs = JoinSet::new();

    loop {
        match requests.next_line().await {
            Ok(Some(line)) => {
                let request = serde_json::from_str(&line)?;
                spawn_request(request, &writer, &shutdown, &mut jobs);
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

fn spawn_request(
    request: WorkerRequest,
    writer: &SharedWriter,
    shutdown: &Arc<AtomicBool>,
    jobs: &mut JoinSet<()>,
) {
    let writer = Arc::clone(writer);
    let shutdown = Arc::clone(shutdown);
    jobs.spawn(async move {
        if let Err(error) = handle_request(request, writer, shutdown).await {
            if !error.is_pipe_shutdown() {
                eprintln!("job failed: {error}");
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

async fn handle_request(
    request: WorkerRequest,
    writer: SharedWriter,
    shutdown: Arc<AtomicBool>,
) -> Result<(), WorkerError> {
    let id = request.id.clone();
    let exit_code = match run_one_job(&request, &writer).await {
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
    write_response(&writer, &WorkerResponse::done(id, exit_code)).await
}

async fn run_one_job(
    request: &WorkerRequest,
    writer: &SharedWriter,
) -> Result<std::process::ExitStatus, WorkerError> {
    let mut child = spawn_child(request)?;
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

fn spawn_child(request: &WorkerRequest) -> Result<tokio::process::Child, WorkerError> {
    let mut command = Command::new("sh");
    command.arg("-c").arg(build_shell_command(
        request.workspace.as_deref(),
        &request.command,
    ));
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.envs(&request.env);

    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }

    command.spawn().map_err(WorkerError::from)
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn build_shell_command(workspace: Option<&str>, command: &str) -> String {
    match workspace {
        None => command.to_owned(),
        Some("") => format!("yarn {command}"),
        Some(workspace) => format!("yarn workspace {} {command}", shell_single_quote(workspace)),
    }
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
enum WorkerError {
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
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    use luchta_engine::WorkerRequest;
    use tokio::sync::Mutex;

    use super::{
        build_shell_command, handle_request, is_pipe_shutdown_error, shell_single_quote,
        WorkerError,
    };

    #[test]
    fn build_shell_command_keeps_raw_command_when_workspace_missing() {
        assert_eq!(build_shell_command(None, "echo hello"), "echo hello");
    }

    #[test]
    fn build_shell_command_prefixes_root_workspace_with_yarn() {
        assert_eq!(
            build_shell_command(Some(""), "install --mode=skip-build"),
            "yarn install --mode=skip-build"
        );
    }

    #[test]
    fn build_shell_command_prefixes_named_workspace_with_yarn_workspace() {
        assert_eq!(
            build_shell_command(Some("a"), "build --flag"),
            "yarn workspace 'a' build --flag"
        );
    }

    #[test]
    fn shell_single_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_single_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn build_shell_command_quotes_workspace_names_with_spaces() {
        assert_eq!(
            build_shell_command(Some("my pkg"), "build"),
            "yarn workspace 'my pkg' build"
        );
    }

    #[test]
    fn build_shell_command_quotes_workspace_names_with_single_quotes() {
        assert_eq!(
            build_shell_command(Some("a'b"), "build"),
            r"yarn workspace 'a'\''b' build"
        );
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

    #[tokio::test]
    async fn handle_request_returns_ok_and_does_not_mark_shutdown_on_success() {
        let writer = Arc::new(Mutex::new(tokio::io::stdout()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let request = WorkerRequest::new("pkg#task", "echo hello");

        let result = handle_request(request, writer, Arc::clone(&shutdown)).await;

        assert!(result.is_ok());
        // Successful job execution does not set shutdown flag.
        // Shutdown is only marked when a pipe error (BrokenPipe/UnexpectedEof/ConnectionReset)
        // occurs during output streaming — see broken_pipe_errors_are_treated_as_pipe_shutdown.
        assert!(!shutdown.load(Ordering::SeqCst));
    }
}
