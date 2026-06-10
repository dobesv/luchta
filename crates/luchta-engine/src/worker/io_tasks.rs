use std::{
    io,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::{
    io::AsyncWriteExt,
    process::{Child, ChildStdin, ChildStdout},
    sync::{mpsc, Mutex, Notify},
    task::JoinHandle,
    time::timeout,
};
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, LinesCodec, LinesCodecError};

use super::{
    handle::{JobMap, WriterContext, WriterRuntime},
    protocol::{LogStream, WorkerRequest, WorkerResponse},
};

const MAX_LINE_LENGTH: usize = 1 << 20;

pub(crate) struct ReaderContext {
    pub(crate) jobs: JobMap,
    pub(crate) is_shutdown: Arc<std::sync::atomic::AtomicBool>,
}

enum ReaderStep {
    Continue(String),
    Stop,
}

enum WriterAction {
    Write(String),
    DropJob(String),
}

pub(crate) fn spawn_reader_task(context: ReaderContext, stdout: ChildStdout) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_LINE_LENGTH));

        while let Some(result) = reader.next().await {
            let line = match handle_reader_frame(&context, result).await {
                ReaderStep::Continue(line) => line,
                ReaderStep::Stop => return,
            };

            if route_worker_response(&context.jobs, &line).await.is_err() {
                crash_reader_jobs(&context).await;
                return;
            }
        }

        crash_reader_jobs(&context).await;
    })
}

pub(crate) fn spawn_writer_task(context: WriterContext) -> JoinHandle<()> {
    tokio::spawn(async move {
        let WriterContext {
            worker,
            mut stdin,
            mut writer_rx,
            jobs,
            is_shutdown,
        } = context;

        let mut runtime = WriterRuntime {
            worker: &worker,
            stdin: &mut stdin,
            jobs: &jobs,
            is_shutdown: &is_shutdown,
        };

        while let Some(request) = writer_rx.recv().await {
            let action = prepare_writer_action(request);
            if execute_writer_action(&mut runtime, action).await {
                return;
            }
        }
    })
}

pub(crate) struct ReaperContext {
    pub(crate) child: Arc<Mutex<Option<Child>>>,
    pub(crate) exit_notify: Arc<Notify>,
    pub(crate) exited: Arc<AtomicBool>,
    pub(crate) jobs: JobMap,
    pub(crate) is_shutdown: Arc<std::sync::atomic::AtomicBool>,
}

pub(crate) fn spawn_reaper_task(context: ReaperContext) -> JoinHandle<()> {
    tokio::spawn(async move {
        let did_exit = reap_child(Arc::clone(&context.child)).await;
        if did_exit {
            context.exited.store(true, Ordering::SeqCst);
        }
        context.exit_notify.notify_waiters();

        if did_exit
            && !context
                .is_shutdown
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            crash_all_jobs(&context.jobs).await;
        }
    })
}

async fn handle_reader_frame(
    context: &ReaderContext,
    result: Result<String, LinesCodecError>,
) -> ReaderStep {
    match result {
        Ok(line) => ReaderStep::Continue(line),
        Err(_error) => {
            crash_reader_jobs(context).await;
            ReaderStep::Stop
        }
    }
}

async fn route_worker_response(jobs: &JobMap, line: &str) -> Result<(), ()> {
    let response = serde_json::from_str::<WorkerResponse>(line).map_err(|_| ())?;
    let id = response.id().to_owned();
    let sender = {
        let jobs = jobs.lock().await;
        jobs.get(&id).cloned()
    };

    if let Some(sender) = sender {
        // Shared stdout reader must keep draining worker output even if one job stops consuming.
        if let Err(error) = sender.try_send(response) {
            match error {
                mpsc::error::TrySendError::Full(response) => eprintln!(
                    "worker response queue full for job '{}' ; dropping {:?}",
                    response.id(),
                    response
                ),
                mpsc::error::TrySendError::Closed(_) => {}
            }
        }
    }

    Ok(())
}

async fn crash_reader_jobs(context: &ReaderContext) {
    if !context
        .is_shutdown
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        crash_all_jobs(&context.jobs).await;
    }
}

fn prepare_writer_action(request: WorkerRequest) -> WriterAction {
    match serde_json::to_string(&request) {
        Ok(line) => WriterAction::Write(line),
        Err(_error) => WriterAction::DropJob(request.id),
    }
}

async fn execute_writer_action(runtime: &mut WriterRuntime<'_>, action: WriterAction) -> bool {
    match action {
        WriterAction::Write(line) => write_worker_request(runtime, &line).await,
        WriterAction::DropJob(id) => {
            drop_job_sender(runtime.jobs, &id).await;
            false
        }
    }
}

async fn write_worker_request(runtime: &mut WriterRuntime<'_>, line: &str) -> bool {
    if write_request_line(runtime.stdin, line).await.is_ok() {
        return false;
    }

    crash_writer_jobs(runtime).await;
    true
}

async fn crash_writer_jobs(runtime: &WriterRuntime<'_>) {
    if !runtime
        .is_shutdown
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        let _ = runtime.worker;
        crash_all_jobs(runtime.jobs).await;
    }
}

async fn write_request_line(stdin: &mut ChildStdin, line: &str) -> io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

pub(crate) async fn collect_worker_handles(
    workers: &Arc<Mutex<std::collections::HashMap<String, Arc<super::handle::WorkerHandle>>>>,
) -> Vec<Arc<super::handle::WorkerHandle>> {
    let mut workers = workers.lock().await;
    workers.drain().map(|(_, handle)| handle).collect()
}

pub(crate) fn clear_writer_sender(writer_tx: &Mutex<Option<mpsc::Sender<WorkerRequest>>>) {
    if let Ok(mut writer_tx) = writer_tx.try_lock() {
        writer_tx.take();
    }
}

pub(crate) fn abort_task_handles(tasks: &Mutex<Vec<JoinHandle<()>>>) {
    if let Ok(mut tasks) = tasks.try_lock() {
        for task in tasks.drain(..) {
            task.abort();
        }
    }
}

pub(crate) async fn wait_for_exit_signal(
    exit_notify: &Arc<Notify>,
    exited: &Arc<AtomicBool>,
    shutdown_timeout: Duration,
) -> Result<(), tokio::time::error::Elapsed> {
    if exited.load(Ordering::SeqCst) {
        return Ok(());
    }

    timeout(shutdown_timeout, exit_notify.notified()).await
}

pub(crate) async fn wait_for_reaper_completion(reaper_task: &Mutex<Option<JoinHandle<()>>>) {
    let task = reaper_task.lock().await.take();
    if let Some(task) = task {
        let _ = task.await;
    }
}

async fn reap_child(child: Arc<Mutex<Option<Child>>>) -> bool {
    let mut child_guard = child.lock().await;
    let Some(process) = child_guard.as_mut() else {
        return false;
    };

    let _ = process.wait().await;
    child_guard.take().is_some()
}

pub(crate) async fn crash_all_jobs(jobs: &JobMap) {
    let mut jobs = jobs.lock().await;
    jobs.clear();
}

async fn drop_job_sender(jobs: &JobMap, id: &str) {
    let mut jobs = jobs.lock().await;
    jobs.remove(id);
}

pub(crate) fn print_log_line(id: &str, stream: LogStream, line: &str, width: usize) {
    let w = if width > 0 { width } else { id.len() };
    let prefix = format!("{id:<w$} |");
    match stream {
        LogStream::Stdout => println!("{} {}", prefix, line),
        LogStream::Stderr => eprintln!("{} {}", prefix, line),
    }
}

pub(crate) fn kill_process_group(pgid: i32) {
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

pub(crate) fn worker_command(command_line: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(command_line)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0);
    command
}
