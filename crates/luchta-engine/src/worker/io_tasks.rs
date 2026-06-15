use std::{
    io,
    process::{ExitStatus, Stdio},
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

use crate::ExecutionLogSink;

use super::{
    handle::{
        JobMap, SharedWriterTx, StderrContext, WorkerCrashState, WorkerHandle, WorkerRegistry,
        WriterContext, WriterRuntime,
    },
    protocol::{LogStream, WorkerMessage, WorkerResponse},
};

const MAX_LINE_LENGTH: usize = 1 << 20;

pub(crate) struct ReaderContext {
    pub(crate) jobs: JobMap,
    pub(crate) is_shutdown: Arc<std::sync::atomic::AtomicBool>,
}

pub(crate) struct LogLineContext<'a> {
    pub(crate) id: &'a str,
    pub(crate) width: usize,
    pub(crate) sink: Option<&'a ExecutionLogSink>,
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

pub(crate) fn spawn_stderr_task(context: StderrContext) -> JoinHandle<()> {
    tokio::spawn(async move {
        let StderrContext {
            worker: _worker,
            stderr,
            crash_state,
        } = context;
        let mut reader = FramedRead::new(stderr, LinesCodec::new_with_max_length(MAX_LINE_LENGTH));

        while let Some(result) = reader.next().await {
            match result {
                Ok(line) => crash_state.lock().await.record_stderr_line(line),
                Err(error) => {
                    crash_state
                        .lock()
                        .await
                        .record_stderr_line(format!("failed to read worker stderr: {error}"));
                    return;
                }
            }
        }
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
    pub(crate) worker: String,
    pub(crate) workers: WorkerRegistry,
    pub(crate) handle: std::sync::Weak<WorkerHandle>,
    pub(crate) child: Arc<Mutex<Option<Child>>>,
    pub(crate) exit_notify: Arc<Notify>,
    pub(crate) exited: Arc<AtomicBool>,
    pub(crate) writer_tx: SharedWriterTx,
    pub(crate) jobs: JobMap,
    pub(crate) is_shutdown: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) crash_state: Arc<Mutex<WorkerCrashState>>,
}

pub(crate) fn spawn_reaper_task(context: ReaperContext) -> JoinHandle<()> {
    tokio::spawn(async move {
        let reap_result = reap_child(Arc::clone(&context.child)).await;
        match reap_result {
            Ok(Some(status)) => context.crash_state.lock().await.set_status(status),
            Ok(None) => {}
            Err(error) => context.crash_state.lock().await.set_wait_error(error),
        }
        context.exited.store(true, Ordering::SeqCst);
        context.exit_notify.notify_waiters();

        clear_writer_sender(context.writer_tx.as_ref());
        evict_worker_handle(&context.workers, &context.worker, &context.handle).await;

        if !context
            .is_shutdown
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            crash_all_jobs(&context.jobs).await;
        }
    })
}

async fn evict_worker_handle(
    workers: &WorkerRegistry,
    worker_name: &str,
    handle: &std::sync::Weak<WorkerHandle>,
) {
    let Some(handle) = handle.upgrade() else {
        return;
    };

    let mut workers = workers.lock().await;
    if workers
        .get(worker_name)
        .is_some_and(|current| Arc::ptr_eq(current, &handle))
    {
        workers.remove(worker_name);
    }
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
    let sender = {
        let jobs = jobs.lock().await;
        jobs.get(response.id()).cloned()
    };

    if let Some(sender) = sender {
        // Back-pressure rather than drop: a build tool must not lose worker
        // output. Awaiting the bounded send pauses the shared stdout reader
        // when a job's consumer falls behind, which in turn stalls the worker
        // process's stdout until the consumer catches up. A `SendError` means
        // the job already finished and removed its receiver; that is benign and
        // ignored. Every dispatched job has a dedicated active consumer
        // (`round_trip`), so this cannot deadlock sibling jobs.
        let _ = sender.send(response).await;
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

fn prepare_writer_action(message: WorkerMessage) -> WriterAction {
    match serde_json::to_string(&message) {
        Ok(line) => WriterAction::Write(line),
        Err(_error) => WriterAction::DropJob(message.id().to_owned()),
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
    workers: &WorkerRegistry,
) -> Vec<Arc<super::handle::WorkerHandle>> {
    let mut workers = workers.lock().await;
    workers.drain().map(|(_, handle)| handle).collect()
}

pub(crate) fn clear_writer_sender(writer_tx: &Mutex<Option<mpsc::Sender<WorkerMessage>>>) {
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

async fn reap_child(child: Arc<Mutex<Option<Child>>>) -> io::Result<Option<ExitStatus>> {
    let mut child_guard = child.lock().await;
    let Some(process) = child_guard.as_mut() else {
        return Ok(None);
    };

    let status = process.wait().await;
    child_guard.take();
    status.map(Some)
}

pub(crate) async fn crash_all_jobs(jobs: &JobMap) {
    let mut jobs = jobs.lock().await;
    jobs.clear();
}

async fn drop_job_sender(jobs: &JobMap, id: &str) {
    let mut jobs = jobs.lock().await;
    jobs.remove(id);
}

pub(crate) fn print_log_line(context: LogLineContext<'_>, stream: LogStream, line: &str) {
    let w = if context.width > 0 {
        context.width
    } else {
        context.id.len()
    };
    let prefix = format!("{:<w$} |", context.id);
    if let Some(sink) = context.sink {
        sink.push(stream, line.to_string());
        return;
    }
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
        .stderr(Stdio::piped())
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
