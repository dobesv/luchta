use std::{
    collections::{HashMap, VecDeque},
    process::ExitStatus,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use tokio::{
    process::{Child, ChildStderr, ChildStdin},
    sync::{mpsc, Mutex, Notify},
    task::JoinHandle,
};

use super::protocol::{WorkerMessage, WorkerResponse};

pub(crate) type JobSender = mpsc::Sender<WorkerResponse>;
pub(crate) type JobMap = Arc<Mutex<HashMap<String, JobSender>>>;
pub(crate) type WorkerRegistry = Arc<Mutex<HashMap<String, Arc<WorkerHandle>>>>;

pub(crate) const STDERR_TAIL_LIMIT: usize = 32;

#[derive(Debug, Clone)]
pub(crate) struct WorkerCrashInfo {
    pub(crate) detail: String,
}

#[derive(Debug)]
pub(crate) struct WorkerCrashState {
    pub(crate) status: Option<ExitStatus>,
    pub(crate) stderr_tail: VecDeque<String>,
}

impl Default for WorkerCrashState {
    fn default() -> Self {
        Self {
            status: None,
            stderr_tail: VecDeque::with_capacity(STDERR_TAIL_LIMIT),
        }
    }
}

impl WorkerCrashState {
    pub(crate) fn record_stderr_line(&mut self, line: String) {
        if self.stderr_tail.len() == STDERR_TAIL_LIMIT {
            self.stderr_tail.pop_front();
        }
        self.stderr_tail.push_back(line);
    }

    pub(crate) fn set_status(&mut self, status: ExitStatus) {
        self.status = Some(status);
    }

    pub(crate) fn crash_info(&self) -> Option<WorkerCrashInfo> {
        let mut parts = Vec::new();

        if let Some(status) = self.status {
            parts.push(format!("exit status {}", format_exit_status(status)));
        }

        if !self.stderr_tail.is_empty() {
            parts.push(format!(
                "stderr: {}",
                self.stderr_tail
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(" | ")
            ));
        }

        if parts.is_empty() {
            None
        } else {
            Some(WorkerCrashInfo {
                detail: parts.join("; "),
            })
        }
    }
}

#[derive(Debug)]
pub(crate) struct WorkerHandle {
    pub(crate) writer_tx: Mutex<Option<mpsc::Sender<WorkerMessage>>>,
    pub(crate) jobs: JobMap,
    pub(crate) child: Arc<Mutex<Option<Child>>>,
    pub(crate) exit_notify: Arc<Notify>,
    pub(crate) exited: Arc<AtomicBool>,
    pub(crate) pgid: i32,
    pub(crate) tasks: Mutex<Vec<JoinHandle<()>>>,
    pub(crate) reaper_task: Mutex<Option<JoinHandle<()>>>,
    pub(crate) is_shutdown: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) crash_state: Arc<Mutex<WorkerCrashState>>,
}

pub(crate) struct WriterContext {
    pub(crate) worker: String,
    pub(crate) stdin: ChildStdin,
    pub(crate) writer_rx: mpsc::Receiver<WorkerMessage>,
    pub(crate) jobs: JobMap,
    pub(crate) is_shutdown: Arc<std::sync::atomic::AtomicBool>,
}

pub(crate) struct StderrContext {
    pub(crate) worker: String,
    pub(crate) stderr: ChildStderr,
    pub(crate) crash_state: Arc<Mutex<WorkerCrashState>>,
}

pub(crate) struct WriterRuntime<'a> {
    pub(crate) worker: &'a str,
    pub(crate) stdin: &'a mut ChildStdin,
    pub(crate) jobs: &'a JobMap,
    pub(crate) is_shutdown: &'a Arc<std::sync::atomic::AtomicBool>,
}

impl WorkerHandle {
    /// Grace period to wait after SIGTERM before escalating to SIGKILL. Long
    /// enough for a well-behaved child (node/babel/yarn) to flush and exit
    /// cleanly, short enough to keep Ctrl-C responsive.
    const TERMINATE_GRACE: Duration = Duration::from_secs(1);

    pub(crate) async fn shutdown(&self, shutdown_timeout: Duration) {
        use std::sync::atomic::Ordering;

        if self.is_shutdown.swap(true, Ordering::SeqCst) {
            return;
        }

        self.writer_tx.lock().await.take();

        // Give the worker a chance to exit on its own (it closes once its stdin
        // is gone and its in-flight job finishes). `shutdown_timeout` may be
        // zero on an interrupt, in which case we move straight to signalling.
        if super::io_tasks::wait_for_exit_signal(&self.exit_notify, &self.exited, shutdown_timeout)
            .await
            .is_err()
        {
            // Escalate gracefully: SIGTERM the process group first so the
            // worker and its children (node/babel/yarn) can exit cleanly and
            // quietly, then SIGKILL only if they ignore it.
            super::io_tasks::terminate_process_group(self.pgid);
            if super::io_tasks::wait_for_exit_signal(
                &self.exit_notify,
                &self.exited,
                Self::TERMINATE_GRACE,
            )
            .await
            .is_err()
            {
                super::io_tasks::kill_process_group(self.pgid);
            }
        }

        super::io_tasks::wait_for_reaper_completion(&self.reaper_task).await;

        // Drop every pending job sender so that any in-flight `run_job` call
        // (blocked on `rx.recv()`) observes the channel close and returns. The
        // reader/writer/reaper crash paths intentionally skip this while
        // `is_shutdown` is set (to avoid spurious "worker crashed" reporting),
        // so shutdown must clear the jobs itself. Without this, an interrupted
        // run would hang forever waiting for the walker to drain.
        super::io_tasks::crash_all_jobs(&self.jobs).await;

        self.abort_tasks().await;
        let mut child = self.child.lock().await;
        child.take();
    }

    pub(crate) fn kill_now(&self) {
        use std::sync::atomic::Ordering;

        self.is_shutdown.store(true, Ordering::SeqCst);
        super::io_tasks::kill_process_group(self.pgid);
        super::io_tasks::clear_writer_sender(&self.writer_tx);
        super::io_tasks::abort_task_handles(&self.tasks);
    }

    pub(crate) async fn is_alive(&self) -> bool {
        if self.exited.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }

        self.writer_tx.lock().await.is_some()
    }

    pub(crate) async fn crash_info(&self) -> Option<WorkerCrashInfo> {
        self.crash_state.lock().await.crash_info()
    }

    async fn abort_tasks(&self) {
        let mut tasks = self.tasks.lock().await;
        for task in tasks.drain(..) {
            task.abort();
        }
    }
}

fn format_exit_status(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(code) = status.code() {
            return format!("code {code}");
        }
        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }

    match status.code() {
        Some(code) => format!("code {code}"),
        None => "unknown".to_owned(),
    }
}
