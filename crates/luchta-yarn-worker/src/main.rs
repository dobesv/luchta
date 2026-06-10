use std::process::Stdio;
use std::sync::Arc;

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
    let mut requests = BufReader::new(stdin()).lines();
    let mut jobs = JoinSet::new();

    while let Some(line) = requests.next_line().await? {
        let request = serde_json::from_str(&line)?;
        spawn_request(request, &writer, &mut jobs);
    }

    drain_jobs(&mut jobs).await;
    Ok(())
}

fn spawn_request(request: WorkerRequest, writer: &SharedWriter, jobs: &mut JoinSet<()>) {
    let writer = Arc::clone(writer);
    jobs.spawn(async move {
        if let Err(error) = handle_request(request, writer).await {
            eprintln!("job failed: {error}");
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

async fn handle_request(request: WorkerRequest, writer: SharedWriter) -> Result<(), WorkerError> {
    let id = request.id.clone();
    let exit_code = match run_one_job(&request, &writer).await {
        Ok(status) => status.code().unwrap_or(1),
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
    command.arg("-c").arg(&request.command);
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
