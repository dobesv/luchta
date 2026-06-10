use std::{collections::HashMap, io, time::Duration};

#[cfg(unix)]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use luchta_types::WorkerDefinition;
#[cfg(unix)]
use tokio::sync::Mutex;

use crate::worker::protocol::WorkerRequest;
#[cfg(unix)]
use crate::worker::{io_tasks::ReaperContext, protocol::WorkerResponse};

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

    pub async fn run_job(
        &self,
        worker_name: &str,
        request: WorkerRequest,
    ) -> Result<i32, WorkerError> {
        let handle = self.get_or_spawn(worker_name).await?;
        let worker = worker_name.to_owned();
        let job_id = request.id.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

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
        let send_result = writer_tx.send(request).await;

        if send_result.is_err() {
            self.remove_job(&handle, &job_id).await;
            return Err(WorkerError::Crashed { worker, id: job_id });
        }

        let result = loop {
            match rx.recv().await {
                Some(WorkerResponse::Log { stream, line, .. }) => {
                    print_log_line(&job_id, stream, &line, self.prefix_width)
                }
                Some(WorkerResponse::Done { exit_code, .. }) => break Ok(exit_code),
                None => {
                    break Err(WorkerError::Crashed {
                        worker: worker.clone(),
                        id: job_id.clone(),
                    })
                }
            }
        };

        self.remove_job(&handle, &job_id).await;
        result
    }

    pub async fn shutdown(&self) {
        self.shutdown_all().await;
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

    async fn shutdown_all(&self) {
        if self.is_shutdown.swap(true, Ordering::SeqCst) {
            return;
        }

        let handles = collect_worker_handles(&self.workers).await;
        for handle in handles {
            handle.shutdown(self.shutdown_timeout).await;
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

    pub async fn run_job(
        &self,
        worker_name: &str,
        request: WorkerRequest,
    ) -> Result<i32, WorkerError> {
        let _ = (&self.definitions, self.shutdown_timeout, self.prefix_width);
        Err(WorkerError::Unsupported {
            worker: worker_name.to_owned(),
            id: request.id,
        })
    }

    pub async fn shutdown(&self) {}
}
