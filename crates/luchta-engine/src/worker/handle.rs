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
pub(crate) type WriterSender = mpsc::Sender<WorkerMessage>;
pub(crate) type SharedWriterTx = Arc<Mutex<Option<WriterSender>>>;
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
    pub(crate) wait_error: Option<String>,
    pub(crate) stderr_tail: VecDeque<String>,
}

impl Default for WorkerCrashState {
    fn default() -> Self {
        Self {
            status: None,
            wait_error: None,
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

    pub(crate) fn set_wait_error(&mut self, error: std::io::Error) {
        self.wait_error = Some(error.to_string());
    }

    pub(crate) fn crash_info(&self, worker_name: &str) -> Option<WorkerCrashInfo> {
        let mut detail = Vec::new();

        if let Some(status) = self.status {
            detail.push(format_exit_status(status));
        }

        if let Some(wait_error) = &self.wait_error {
            detail.push(format!("wait error: {wait_error}"));
        }

        let mut detail = detail.join("; ");

        if !self.stderr_tail.is_empty() {
            let stderr_block = format_stderr_block(worker_name, &self.stderr_tail);
            if !detail.is_empty() {
                detail.push('\n');
            }
            detail.push_str(&stderr_block);
        }

        if detail.is_empty() {
            None
        } else {
            Some(WorkerCrashInfo { detail })
        }
    }
}

#[derive(Debug)]
pub(crate) struct WorkerHandle {
    pub(crate) writer_tx: SharedWriterTx,
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

        // Give worker chance to exit on its own (it closes once its stdin is
        // gone and its in-flight job finishes). `shutdown_timeout` may be zero
        // on an interrupt, in which case we move straight to signalling.
        if super::io_tasks::wait_for_exit_signal(&self.exit_notify, &self.exited, shutdown_timeout)
            .await
            .is_err()
        {
            // Escalate gracefully: SIGTERM process group first so worker and
            // its children (node/babel/yarn) can exit cleanly and quietly,
            // then SIGKILL only if they ignore it.
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
        // (blocked on `rx.recv()`) observes channel close and returns. Reader,
        // writer, reaper crash paths intentionally skip this while
        // `is_shutdown` is set (to avoid spurious "worker crashed" reporting),
        // so shutdown must clear jobs itself. Without this, interrupted run
        // would hang forever waiting for walker to drain.
        super::io_tasks::crash_all_jobs(&self.jobs).await;

        self.abort_tasks().await;
        self.child.lock().await.take();
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

    pub(crate) async fn crash_info(&self, worker_name: &str) -> Option<WorkerCrashInfo> {
        self.crash_state.lock().await.crash_info(worker_name)
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
            return format!("exited with code {code}");
        }
        if let Some(signal) = status.signal() {
            return match signal_name(signal) {
                Some(name) => format!("killed by signal {name} ({signal})"),
                None => format!("killed by signal {signal}"),
            };
        }
    }

    match status.code() {
        Some(code) => format!("exited with code {code}"),
        None => "exit status unknown".to_owned(),
    }
}

#[cfg(unix)]
fn signal_name(signal: i32) -> Option<&'static str> {
    match signal {
        6 => Some("SIGABRT"),
        8 => Some("SIGFPE"),
        9 => Some("SIGKILL"),
        11 => Some("SIGSEGV"),
        15 => Some("SIGTERM"),
        _ => None,
    }
}

fn format_stderr_block(worker_name: &str, stderr_tail: &VecDeque<String>) -> String {
    let line_count = stderr_tail.len();
    let mut block = format!("--- worker '{worker_name}' stderr (last {line_count} lines) ---");

    for line in stderr_tail {
        block.push('\n');
        block.push_str(line);
    }

    block.push_str(&format!("\n--- end worker '{worker_name}' stderr ---"));
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crash_detail(state: WorkerCrashState, worker_name: &str) -> String {
        state
            .crash_info(worker_name)
            .expect("crash info present")
            .detail
    }

    #[cfg(unix)]
    fn exit_status_from_code(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;

        ExitStatus::from_raw(code << 8)
    }

    #[cfg(not(unix))]
    fn exit_status_from_code(code: i32) -> ExitStatus {
        std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
            .args(if cfg!(windows) {
                vec!["/C", &format!("exit {code}")]
            } else {
                vec!["-c", &format!("exit {code}")]
            })
            .status()
            .expect("spawn exit-status helper")
    }

    #[test]
    fn crash_info_renders_exit_code() {
        let mut state = WorkerCrashState::default();
        state.set_status(exit_status_from_code(1));

        assert_eq!(crash_detail(state, "yarn"), "exited with code 1");
    }

    #[cfg(unix)]
    #[test]
    fn crash_info_renders_known_signal_name() {
        use std::os::unix::process::ExitStatusExt;

        let mut state = WorkerCrashState::default();
        state.set_status(ExitStatus::from_raw(9));

        assert_eq!(crash_detail(state, "yarn"), "killed by signal SIGKILL (9)");
    }

    #[cfg(unix)]
    #[test]
    fn crash_info_renders_unknown_signal_number() {
        use std::os::unix::process::ExitStatusExt;

        let mut state = WorkerCrashState::default();
        state.set_status(ExitStatus::from_raw(31));

        assert_eq!(crash_detail(state, "yarn"), "killed by signal 31");
    }

    #[test]
    fn crash_info_includes_stderr_block_when_tail_present() {
        let mut state = WorkerCrashState::default();
        state.set_status(exit_status_from_code(1));
        state.record_stderr_line("line 1".to_owned());
        state.record_stderr_line("line 2".to_owned());

        assert_eq!(
            crash_detail(state, "builder"),
            "exited with code 1\n--- worker 'builder' stderr (last 2 lines) ---\nline 1\nline 2\n--- end worker 'builder' stderr ---"
        );
    }

    #[test]
    fn crash_info_omits_stderr_block_when_tail_empty() {
        let mut state = WorkerCrashState::default();
        state.set_wait_error(std::io::Error::other("wait blew up"));

        let detail = crash_detail(state, "builder");
        assert_eq!(detail, "wait error: wait blew up");
        assert!(!detail.contains("stderr"));
    }
}
