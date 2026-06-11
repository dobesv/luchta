use std::{collections::HashMap, io, time::Duration};

#[cfg(unix)]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use luchta_types::WorkerDefinition;
#[cfg(unix)]
use tokio::sync::{mpsc, Mutex};

use crate::worker::protocol::{ResolveResult, ResolveTask, WorkerRequest};
#[cfg(unix)]
use crate::worker::{
    io_tasks::ReaperContext,
    protocol::{WorkerMessage, WorkerResponse},
};
use crate::TaskResolver;

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
    #[error("worker '{worker}' crashed during job '{id}'")]
    Crashed { worker: String, id: String },
    #[error("worker protocol error for job '{id}': {detail}")]
    Protocol { id: String, detail: String },
    #[error("resident workers are only supported on Unix (worker '{worker}', job '{id}')")]
    Unsupported { worker: String, id: String },
}

#[cfg(unix)]
use super::{
    handle::{JobMap, WorkerHandle, WriterContext},
    io_tasks::{
        collect_worker_handles, print_log_line, spawn_reader_task, spawn_reaper_task,
        spawn_writer_task, ReaderContext,
    },
    spawn::spawn_worker_process,
};

#[cfg(unix)]
#[derive(Debug)]
pub struct WorkerManager {
    definitions: HashMap<String, WorkerDefinition>,
    workers: Arc<Mutex<HashMap<String, Arc<WorkerHandle>>>>,
    spawn_lock: Arc<Mutex<()>>,
    shutdown_timeout: Duration,
    is_shutdown: Arc<AtomicBool>,
    prefix_width: usize,
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
        }
    }

    pub fn with_prefix_width(mut self, width: usize) -> Self {
        self.prefix_width = width;
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
            return Err(WorkerError::Crashed {
                worker: worker_name.to_owned(),
                id: job_id,
            });
        }

        let handle = self.get_or_spawn(worker_name).await?;
        let worker = worker_name.to_owned();
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
                    return Err(WorkerError::Crashed { worker, id: job_id });
                }
            }
        };

        if writer_tx.send(message).await.is_err() {
            self.remove_job(&handle, &job_id).await;
            return Err(WorkerError::Crashed { worker, id: job_id });
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
        mut select: impl FnMut(WorkerResponse) -> Option<T>,
    ) -> Result<T, WorkerError> {
        let (handle, job_id, mut rx) = self.dispatch_message(worker_name, message).await?;

        let outcome = loop {
            match rx.recv().await {
                Some(WorkerResponse::Log { stream, line, .. }) => {
                    print_log_line(&job_id, stream, &line, self.prefix_width)
                }
                Some(response) => {
                    let kind = response.kind();
                    match select(response) {
                        Some(value) => break Ok(value),
                        None => {
                            break Err(WorkerError::Protocol {
                                id: job_id.clone(),
                                detail: format!("unexpected '{kind}' response"),
                            })
                        }
                    }
                }
                None => {
                    break Err(WorkerError::Crashed {
                        worker: worker_name.to_owned(),
                        id: job_id.clone(),
                    })
                }
            }
        };

        self.remove_job(&handle, &job_id).await;
        outcome
    }

    pub async fn run_job(
        &self,
        worker_name: &str,
        request: WorkerRequest,
    ) -> Result<i32, WorkerError> {
        // Terminal response is `Done`; logs are streamed, other responses ignored.
        self.round_trip(
            worker_name,
            WorkerMessage::Run(request),
            WorkerResponse::into_exit_code,
        )
        .await
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
        if let Some(existing) = self.workers.lock().await.get(worker_name).cloned() {
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
        if let Some(existing) = self.workers.lock().await.get(worker_name).cloned() {
            return Ok(existing);
        }

        let handle = Arc::new(self.spawn_worker(worker_name, &definition).await?);
        let mut workers = self.workers.lock().await;
        workers.insert(worker_name.to_owned(), Arc::clone(&handle));
        Ok(handle)
    }

    async fn spawn_worker(
        &self,
        worker_name: &str,
        definition: &WorkerDefinition,
    ) -> Result<WorkerHandle, WorkerError> {
        let mut child = spawn_worker_process(worker_name, &definition.command).await?;
        let pgid = child.id().expect("worker pid available") as i32;
        let stdin = child.stdin.take().expect("worker stdin piped");
        let stdout = child.stdout.take().expect("worker stdout piped");
        let jobs: JobMap = Arc::new(Mutex::new(HashMap::new()));
        let child = Arc::new(Mutex::new(Some(child)));
        let exit_notify = Arc::new(tokio::sync::Notify::new());
        let exited = Arc::new(AtomicBool::new(false));
        let (writer_tx, writer_rx) = tokio::sync::mpsc::channel(64);
        let is_shutdown = Arc::new(AtomicBool::new(false));

        let reader_task = spawn_reader_task(
            ReaderContext {
                jobs: Arc::clone(&jobs),
                is_shutdown: Arc::clone(&is_shutdown),
            },
            stdout,
        );
        let writer_task = spawn_writer_task(WriterContext {
            worker: worker_name.to_owned(),
            stdin,
            writer_rx,
            jobs: Arc::clone(&jobs),
            is_shutdown: Arc::clone(&is_shutdown),
        });
        let reaper_task = spawn_reaper_task(ReaperContext {
            child: Arc::clone(&child),
            exit_notify: Arc::clone(&exit_notify),
            exited: Arc::clone(&exited),
            jobs: Arc::clone(&jobs),
            is_shutdown: Arc::clone(&is_shutdown),
        });

        Ok(WorkerHandle {
            writer_tx: Mutex::new(Some(writer_tx)),
            jobs,
            child,
            exit_notify,
            exited,
            pgid,
            tasks: Mutex::new(vec![reader_task, writer_task]),
            reaper_task: Mutex::new(Some(reaper_task)),
            is_shutdown,
        })
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
        if self.is_shutdown.swap(true, Ordering::SeqCst) {
            return;
        }

        if let Ok(mut workers) = self.workers.try_lock() {
            let handles = workers
                .drain()
                .map(|(_, handle)| handle)
                .collect::<Vec<_>>();
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
        }
    }

    pub fn with_prefix_width(mut self, width: usize) -> Self {
        self.prefix_width = width;
        self
    }

    fn unsupported<T>(&self, worker_name: &str, id: String) -> Result<T, WorkerError> {
        let _ = (&self.definitions, self.shutdown_timeout, self.prefix_width);
        Err(WorkerError::Unsupported {
            worker: worker_name.to_owned(),
            id,
        })
    }

    pub async fn run_job(
        &self,
        worker_name: &str,
        request: WorkerRequest,
    ) -> Result<i32, WorkerError> {
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
        self.round_trip(
            worker,
            WorkerMessage::ResolveTask(request),
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
