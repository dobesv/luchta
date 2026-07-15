use std::{collections::HashMap, io, time::Duration};

#[cfg(unix)]
const CRASH_DETAIL_WAIT_TIMEOUT: Duration = Duration::from_millis(250);

#[cfg(unix)]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use luchta_types::WorkerDefinition;
use luchta_worker::WorkerDonePayload;
#[cfg(unix)]
use tokio::sync::{mpsc, Mutex};

use crate::worker::protocol::{ResolveResult, ResolveTask, WorkerRequest};
#[cfg(unix)]
use crate::worker::{
    io_tasks::ReaperContext,
    protocol::{LogStream, WorkerMessage, WorkerResponse},
};
use crate::{ExecutionLogSink, TaskResolver};
// `CollectedReport` is only referenced from the `#[cfg(unix)]` worker
// implementation, so gate the import to avoid an unused-import error on
// non-unix targets (e.g. Windows CI).
#[cfg(unix)]
use crate::CollectedReport;

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("worker '{worker}' not defined in config")]
    Undefined { worker: String },
    #[error("failed to spawn worker '{worker}': {source}")]
    Spawn {
        worker: String,
        #[source]
        source: io::Error,
    },
    #[error("worker '{worker}' crashed during job '{id}'{detail_suffix}")]
    Crashed {
        worker: String,
        id: String,
        detail: Option<String>,
        detail_suffix: String,
    },
    #[error("worker '{worker}' protocol error for job '{id}': {detail}")]
    Protocol {
        worker: String,
        id: String,
        detail: String,
    },
    #[error("resident workers are only supported on Unix (worker '{worker}', job '{id}')")]
    Unsupported { worker: String, id: String },
}

#[cfg(unix)]
use super::{
    handle::{JobMap, WorkerCrashState, WorkerHandle, WorkerRegistry, WriterContext},
    io_tasks::{
        collect_worker_handles, print_log_line, spawn_reader_task, spawn_reaper_task,
        spawn_stderr_task, spawn_writer_task, LogLineContext, ReaderContext,
    },
    spawn::spawn_worker_process,
};

#[cfg(unix)]
#[derive(Debug)]
pub struct WorkerManager {
    definitions: HashMap<String, WorkerDefinition>,
    workers: WorkerRegistry,
    spawn_lock: Arc<Mutex<()>>,
    shutdown_timeout: Duration,
    is_shutdown: Arc<AtomicBool>,
    prefix_width: usize,
    workspace_root: std::path::PathBuf,
}

#[cfg(unix)]
impl WorkerManager {
    pub fn new(definitions: HashMap<String, WorkerDefinition>) -> Self {
        Self::with_shutdown_timeout(definitions, Duration::from_secs(5))
    }

    pub fn with_shutdown_timeout(
        definitions: HashMap<String, WorkerDefinition>,
        shutdown_timeout: Duration,
    ) -> Self {
        Self {
            definitions,
            workers: Arc::new(Mutex::new(HashMap::new())),
            spawn_lock: Arc::new(Mutex::new(())),
            shutdown_timeout,
            is_shutdown: Arc::new(AtomicBool::new(false)),
            prefix_width: 0,
            workspace_root: std::path::PathBuf::new(),
        }
    }

    pub fn with_prefix_width(mut self, width: usize) -> Self {
        self.prefix_width = width;
        self
    }

    pub fn with_workspace_root(mut self, root: std::path::PathBuf) -> Self {
        self.workspace_root = root;
        self
    }

    /// Spawns (or reuses) the worker, registers a job channel keyed by the
    /// message's id, and sends the message. Returns the worker handle, the job
    /// id, and the receiver for that job's responses. On any failure the job is
    /// removed and a `Crashed` error returned. Shared by `run_job` /
    /// `resolve_task`, which differ only in how they consume the responses.
    async fn dispatch_message(
        &self,
        worker_name: &str,
        message: WorkerMessage,
    ) -> Result<(Arc<WorkerHandle>, String, mpsc::Receiver<WorkerResponse>), WorkerError> {
        let job_id = message.id().to_owned();

        // Once a shutdown is underway, refuse to start new work. Without this a
        // run interrupted mid-flight could spawn a fresh worker for a queued
        // task (e.g. under a concurrency limit), leaving an orphaned worker
        // that shutdown never sees.
        if self.is_shutdown.load(Ordering::SeqCst) {
            return Err(self.crashed_error(worker_name, &job_id, None));
        }

        let handle = self.get_or_spawn(worker_name).await?;
        let (tx, rx) = mpsc::channel(64);

        {
            let mut jobs = handle.jobs.lock().await;
            jobs.insert(job_id.clone(), tx);
        }

        let writer_tx = {
            let writer_tx = handle.writer_tx.lock().await;
            match writer_tx.as_ref() {
                Some(writer_tx) => writer_tx.clone(),
                None => {
                    self.remove_job(&handle, &job_id).await;
                    return Err(self.crashed_error_for(worker_name, &handle, &job_id).await);
                }
            }
        };

        if writer_tx.send(message).await.is_err() {
            self.remove_job(&handle, &job_id).await;
            return Err(self.crashed_error_for(worker_name, &handle, &job_id).await);
        }

        Ok((handle, job_id, rx))
    }

    /// Sends `message` to the worker and drains the job's response channel —
    /// printing `Log` lines — until `select` extracts a terminal value from the
    /// next non-log response. A non-log response that `select` does not accept
    /// (e.g. a `Done` for a resolve job, or a `Resolved` for a run job, from a
    /// buggy or version-skewed worker) is a protocol error and fails fast rather
    /// than looping forever. Returns `Crashed` if the channel closes before a
    /// terminal arrives. This is the single round-trip primitive shared by
    /// `run_job` and `resolve_task`; they differ only in the message sent and
    /// the response they select.
    async fn round_trip<T>(
        &self,
        worker_name: &str,
        message: WorkerMessage,
        mut on_log: impl FnMut(&str, LogStream, &str),
        sink: Option<&ExecutionLogSink>,
        mut select: impl FnMut(WorkerResponse) -> Option<T>,
    ) -> Result<T, WorkerError> {
        let (handle, job_id, mut rx) = self.dispatch_message(worker_name, message).await?;

        let outcome = loop {
            match rx.recv().await {
                Some(WorkerResponse::Log { stream, line, .. }) => {
                    on_log(&job_id, stream, &line);
                    print_log_line(
                        LogLineContext {
                            id: &job_id,
                            width: self.prefix_width,
                            sink,
                        },
                        stream,
                        &line,
                    )
                }
                Some(WorkerResponse::Report {
                    filename,
                    mime_type,
                    content,
                    ..
                }) => {
                    if !luchta_worker::is_valid_report_filename(&filename) {
                        eprintln!(
                            "warning: dropping worker report with invalid filename for worker '{}' job '{}': {}",
                            worker_name, job_id, filename
                        );
                        continue;
                    }

                    if let Some(sink) = sink {
                        sink.push_report(CollectedReport {
                            filename,
                            mime_type,
                            content,
                        });
                    }
                }
                Some(response) => {
                    let kind = response.kind();
                    match select(response) {
                        Some(value) => break Ok(value),
                        None => {
                            break Err(WorkerError::Protocol {
                                worker: worker_name.to_owned(),
                                id: job_id.clone(),
                                detail: format!("unexpected '{kind}' response"),
                            })
                        }
                    }
                }
                None => break Err(self.crashed_error_for(worker_name, &handle, &job_id).await),
            }
        };

        self.remove_job(&handle, &job_id).await;
        outcome
    }

    async fn round_trip_retry_once_on_crash<T, F>(
        &self,
        worker_name: &str,
        first_message: WorkerMessage,
        retry_message: WorkerMessage,
        sink: Option<&ExecutionLogSink>,
        select: F,
    ) -> Result<T, WorkerError>
    where
        F: Fn(WorkerResponse) -> Option<T> + Copy,
    {
        match self
            .round_trip(worker_name, first_message, |_, _, _| {}, sink, select)
            .await
        {
            // A crash mid-run is retried once. But a channel close observed
            // while the manager is shutting down is not a real crash — the
            // worker was intentionally killed during teardown — so suppress the
            // warning and the retry and surface the error as-is.
            Err(WorkerError::Crashed { worker, id, .. })
                if !self.is_shutdown.load(Ordering::SeqCst) =>
            {
                eprintln!("warning: worker '{worker}' crashed during job '{id}', retrying once");
                self.round_trip(worker_name, retry_message, |_, _, _| {}, sink, select)
                    .await
            }
            other => other,
        }
    }

    pub async fn run_job(
        &self,
        worker_name: &str,
        request: WorkerRequest,
        sink: Option<&ExecutionLogSink>,
    ) -> Result<WorkerDonePayload, WorkerError> {
        let retry_request = request.clone();

        // Terminal response is `Done`; logs are streamed, other responses ignored.
        self.round_trip_retry_once_on_crash(
            worker_name,
            WorkerMessage::Run(request),
            WorkerMessage::Run(retry_request),
            sink,
            WorkerResponse::into_done,
        )
        .await
    }

    pub fn is_shutdown(&self) -> bool {
        self.is_shutdown.load(Ordering::SeqCst)
    }

    pub async fn shutdown(&self) {
        self.shutdown_all(self.shutdown_timeout).await;
    }

    /// Shut down without granting the graceful exit window: worker process
    /// groups are killed right away. Used on the interrupt (Ctrl-C / SIGTERM)
    /// path where the run is being aborted and responsiveness matters more than
    /// letting in-flight jobs finish.
    pub async fn shutdown_immediate(&self) {
        self.shutdown_all(Duration::ZERO).await;
    }

    async fn get_or_spawn(&self, worker_name: &str) -> Result<Arc<WorkerHandle>, WorkerError> {
        if let Some(existing) = self.try_reuse_worker(worker_name).await {
            return Ok(existing);
        }

        let definition =
            self.definitions
                .get(worker_name)
                .cloned()
                .ok_or_else(|| WorkerError::Undefined {
                    worker: worker_name.to_owned(),
                })?;

        let _spawn_guard = self.spawn_lock.lock().await;
        if let Some(existing) = self.try_reuse_worker(worker_name).await {
            return Ok(existing);
        }

        let handle = self.spawn_worker(worker_name, &definition).await?;
        let mut workers = self.workers.lock().await;
        workers.insert(worker_name.to_owned(), Arc::clone(&handle));
        Ok(handle)
    }

    async fn try_reuse_worker(&self, worker_name: &str) -> Option<Arc<WorkerHandle>> {
        let existing = {
            let workers = self.workers.lock().await;
            workers.get(worker_name).cloned()
        }?;

        if existing.is_alive().await {
            return Some(existing);
        }

        self.evict_if_current(worker_name, &existing).await;
        None
    }

    async fn evict_if_current(&self, worker_name: &str, dead: &Arc<WorkerHandle>) {
        let mut workers = self.workers.lock().await;
        if workers
            .get(worker_name)
            .is_some_and(|current| Arc::ptr_eq(current, dead))
        {
            workers.remove(worker_name);
        }
    }

    /// Build a `Crashed` error for a job whose worker died: evict the dead handle
    /// (instance-guarded) so the next dispatch respawns rather than reusing it,
    /// wait briefly for the reaper to record exit status, then attach whatever
    /// crash detail (exit status + stderr tail) is available. Shared by the
    /// dispatch and round-trip crash paths.
    async fn crashed_error_for(
        &self,
        worker_name: &str,
        handle: &Arc<WorkerHandle>,
        job_id: &str,
    ) -> WorkerError {
        self.evict_if_current(worker_name, handle).await;
        self.wait_for_crash_detail(handle).await;
        let detail = handle.crash_info(worker_name).await;
        self.crashed_error(worker_name, job_id, detail)
    }

    async fn wait_for_crash_detail(&self, handle: &WorkerHandle) {
        let _ = super::io_tasks::wait_for_exit_signal(
            &handle.exit_notify,
            &handle.exited,
            CRASH_DETAIL_WAIT_TIMEOUT,
        )
        .await;
    }

    async fn spawn_worker(
        &self,
        worker_name: &str,
        definition: &WorkerDefinition,
    ) -> Result<Arc<WorkerHandle>, WorkerError> {
        let mut child =
            spawn_worker_process(worker_name, &definition.command, &self.workspace_root).await?;
        let pgid = child.id().expect("worker pid available") as i32;
        let stdin = child.stdin.take().expect("worker stdin piped");
        let stdout = child.stdout.take().expect("worker stdout piped");
        let stderr = child.stderr.take().expect("worker stderr piped");
        let jobs: JobMap = Arc::new(Mutex::new(HashMap::new()));
        let child = Arc::new(Mutex::new(Some(child)));
        let exit_notify = Arc::new(tokio::sync::Notify::new());
        let exited = Arc::new(AtomicBool::new(false));
        let (writer_sender, writer_rx) = tokio::sync::mpsc::channel(64);
        let writer_tx = Arc::new(Mutex::new(Some(writer_sender)));
        let is_shutdown = Arc::new(AtomicBool::new(false));
        let crash_state = Arc::new(Mutex::new(WorkerCrashState::default()));

        let reader_task = spawn_reader_task(
            ReaderContext {
                jobs: Arc::clone(&jobs),
                is_shutdown: Arc::clone(&is_shutdown),
                crash_state: Arc::clone(&crash_state),
            },
            stdout,
        );
        let stderr_task = spawn_stderr_task(super::handle::StderrContext {
            worker: worker_name.to_owned(),
            stderr,
            crash_state: Arc::clone(&crash_state),
        });
        let writer_task = spawn_writer_task(WriterContext {
            worker: worker_name.to_owned(),
            stdin,
            writer_rx,
            jobs: Arc::clone(&jobs),
            is_shutdown: Arc::clone(&is_shutdown),
        });
        let handle = Arc::new(WorkerHandle {
            writer_tx: Arc::clone(&writer_tx),
            jobs,
            child,
            exit_notify,
            exited,
            pgid,
            tasks: Mutex::new(vec![reader_task, stderr_task, writer_task]),
            reaper_task: Mutex::new(None),
            is_shutdown,
            crash_state,
        });
        let reaper_task = spawn_reaper_task(ReaperContext {
            worker: worker_name.to_owned(),
            workers: Arc::clone(&self.workers),
            handle: Arc::downgrade(&handle),
            child: Arc::clone(&handle.child),
            exit_notify: Arc::clone(&handle.exit_notify),
            exited: Arc::clone(&handle.exited),
            writer_tx: Arc::clone(&handle.writer_tx),
            jobs: Arc::clone(&handle.jobs),
            is_shutdown: Arc::clone(&handle.is_shutdown),
            crash_state: Arc::clone(&handle.crash_state),
        });
        *handle.reaper_task.lock().await = Some(reaper_task);

        // Return the same `Arc` the reaper holds a `Weak` to, so the registry
        // stores that identical allocation. Re-wrapping a moved-out value in a
        // fresh `Arc` here would orphan the reaper's `Weak` (it could never
        // upgrade), silently disabling reaper-side eviction.
        Ok(handle)
    }

    fn crashed_error(
        &self,
        worker_name: &str,
        job_id: &str,
        detail: Option<super::handle::WorkerCrashInfo>,
    ) -> WorkerError {
        let detail = detail.map(|info| info.detail);
        let detail_suffix = detail
            .as_ref()
            .map_or_else(String::new, |detail| format!(": {detail}"));
        WorkerError::Crashed {
            worker: worker_name.to_owned(),
            id: job_id.to_owned(),
            detail,
            detail_suffix,
        }
    }

    async fn remove_job(&self, handle: &Arc<WorkerHandle>, job_id: &str) {
        let mut jobs = handle.jobs.lock().await;
        jobs.remove(job_id);
    }

    async fn shutdown_all(&self, timeout: Duration) {
        if self.is_shutdown.swap(true, Ordering::SeqCst) {
            return;
        }

        // Loop until no workers remain. A job that passed the `is_shutdown`
        // check in `run_job` just before the flag was set may spawn a worker
        // after the first drain; collecting again catches those stragglers.
        loop {
            let handles = collect_worker_handles(&self.workers).await;
            if handles.is_empty() {
                break;
            }
            for handle in handles {
                handle.shutdown(timeout).await;
            }
        }
    }
}

#[cfg(unix)]
impl Drop for WorkerManager {
    fn drop(&mut self) {
        if self.is_shutdown.load(Ordering::SeqCst) {
            return;
        }

        self.is_shutdown.store(true, Ordering::SeqCst);
        if let Ok(mut workers) = self.workers.try_lock() {
            let handles: Vec<_> = workers.drain().map(|(_, handle)| handle).collect();
            drop(workers);
            for handle in handles {
                handle.kill_now();
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(not(unix))]
#[derive(Debug)]
pub struct WorkerManager {
    definitions: HashMap<String, WorkerDefinition>,
    shutdown_timeout: Duration,
    prefix_width: usize,
    workspace_root: std::path::PathBuf,
}

#[cfg(not(unix))]
impl WorkerManager {
    pub fn new(definitions: HashMap<String, WorkerDefinition>) -> Self {
        Self::with_shutdown_timeout(definitions, Duration::from_secs(5))
    }

    pub fn with_shutdown_timeout(
        definitions: HashMap<String, WorkerDefinition>,
        shutdown_timeout: Duration,
    ) -> Self {
        Self {
            definitions,
            shutdown_timeout,
            prefix_width: 0,
            workspace_root: std::path::PathBuf::new(),
        }
    }

    pub fn with_prefix_width(mut self, width: usize) -> Self {
        self.prefix_width = width;
        self
    }

    pub fn with_workspace_root(mut self, root: std::path::PathBuf) -> Self {
        self.workspace_root = root;
        self
    }

    fn unsupported<T>(&self, worker_name: &str, id: String) -> Result<T, WorkerError> {
        let _ = (
            &self.definitions,
            self.shutdown_timeout,
            self.prefix_width,
            &self.workspace_root,
        );
        Err(WorkerError::Unsupported {
            worker: worker_name.to_owned(),
            id,
        })
    }

    pub async fn run_job(
        &self,
        worker_name: &str,
        request: WorkerRequest,
        sink: Option<&ExecutionLogSink>,
    ) -> Result<WorkerDonePayload, WorkerError> {
        let _ = sink;
        self.unsupported(worker_name, request.id)
    }

    pub async fn shutdown(&self) {}

    pub async fn shutdown_immediate(&self) {}
}

#[cfg(unix)]
impl TaskResolver for WorkerManager {
    /// Resolves a task by sending a `ResolveTask` to its worker and awaiting the
    /// single `Resolved` decision (logs are streamed; other responses ignored).
    async fn resolve(&self, worker: &str, request: ResolveTask) -> Result<ResolveResult, String> {
        let retry_request = request.clone();

        self.round_trip_retry_once_on_crash(
            worker,
            WorkerMessage::ResolveTask(request),
            WorkerMessage::ResolveTask(retry_request),
            None,
            WorkerResponse::into_resolve_result,
        )
        .await
        .map_err(|error| error.to_string())
    }
}

#[cfg(not(unix))]
impl TaskResolver for WorkerManager {
    async fn resolve(&self, worker: &str, request: ResolveTask) -> Result<ResolveResult, String> {
        self.unsupported::<ResolveResult>(worker, request.id)
            .map_err(|error| error.to_string())
    }
}
