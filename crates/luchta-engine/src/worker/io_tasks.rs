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
    // Register interest in the notification BEFORE checking `exited`. The
    // reaper sets `exited` and then calls `notify_waiters()`. `enable()`
    // registers this waiter immediately (and reports whether a notification
    // already arrived), so a `notify_waiters()` that fires after this point
    // still wakes us. This closes the missed-wakeup race where the reaper
    // notifies in the gap between the flag check and awaiting the future.
    let notified = exit_notify.notified();
    tokio::pin!(notified);

    if notified.as_mut().enable() || exited.load(Ordering::SeqCst) {
        return Ok(());
    }

    timeout(shutdown_timeout, notified).await
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

/// Politely ask the worker's process group to terminate (SIGTERM), giving the
/// worker and its children (e.g. node/babel) a chance to exit cleanly instead
/// of being hard-killed mid-write, which avoids stack-trace spam on the
/// terminal.
pub(crate) fn terminate_process_group(pgid: i32) {
    // SAFETY: `libc::kill` is async-signal-safe. `pgid` is the worker's process
    // group id captured at spawn (the worker runs in its own group via
    // `process_group(0)`), so signalling `-pgid` targets only that worker and
    // its descendants. A stale pgid simply yields ESRCH, which is harmless.
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
}

/// Forcibly kill the worker's process group (SIGKILL). Used as the fallback
/// after a SIGTERM grace period, or when an immediate, unconditional kill is
/// required.
pub(crate) fn kill_process_group(pgid: i32) {
    // SAFETY: see `terminate_process_group` — `libc::kill` is async-signal-safe
    // and `-pgid` targets only the worker's own process group.
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

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    };

    use tokio::sync::Notify;

    use super::wait_for_exit_signal;

    #[tokio::test]
    async fn returns_immediately_when_already_exited() {
        let notify = Arc::new(Notify::new());
        let exited = Arc::new(AtomicBool::new(true));

        // Already exited: must return Ok without waiting for the timeout.
        let result = wait_for_exit_signal(&notify, &exited, Duration::from_secs(30)).await;
        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observes_notification_without_missed_wakeup() {
        // Reproduces the missed-wakeup race: a concurrent reaper sets `exited`
        // and calls `notify_waiters()` right around when the waiter registers.
        // With the `enable()` fix the waiter never misses the notification, so
        // it returns well before the long timeout.
        for _ in 0..200 {
            let notify = Arc::new(Notify::new());
            let exited = Arc::new(AtomicBool::new(false));

            let reaper_notify = Arc::clone(&notify);
            let reaper_exited = Arc::clone(&exited);
            let reaper = tokio::spawn(async move {
                reaper_exited.store(true, Ordering::SeqCst);
                reaper_notify.notify_waiters();
            });

            let waited = tokio::time::timeout(
                Duration::from_secs(5),
                wait_for_exit_signal(&notify, &exited, Duration::from_secs(30)),
            )
            .await
            .expect("wait_for_exit_signal must not hang");
            assert!(waited.is_ok());
            reaper.await.expect("reaper task");
        }
    }
}
