use std::collections::VecDeque;
use std::time::{Duration, Instant};

use luchta_worker::{ProxyError, ResolveResult, SharedWriter, WorkerMessage, WorkerResponse};
use tokio::io::{stderr, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::generation::{in_flight_kind, Generation, InFlightKind};

pub enum RouterEvent {
    Inbound(WorkerMessage),
    Response(u64, WorkerResponse),
    StdoutClosed(u64),
    FileChanged,
    /// A generation has been draining longer than [`DRAIN_TIMEOUT`]; carries the
    /// draining generation's id so a stale timer for an already-finished drain is
    /// ignored.
    DrainTimeout(u64),
    ShutdownAll,
    /// The shutdown grace period ([`SHUTDOWN_GRACE`]) elapsed; force-terminate any
    /// worker that has not exited on its own.
    ShutdownTimeout,
}

/// Restart throttle: after `RESTART_BURST` restarts within `RESTART_WINDOW`, each
/// further restart waits `RESTART_BACKOFF`, so a crash loop or a noisy file watch
/// cannot spawn worker processes without bound.
const RESTART_WINDOW: Duration = Duration::from_secs(10);
const RESTART_BURST: usize = 8;
const RESTART_BACKOFF: Duration = Duration::from_secs(1);

/// Safety valve for draining. A restart waits for the old worker to finish its
/// in-flight tasks before starting the replacement; if a task never completes (a
/// wedged worker), the drain would otherwise block all queued work forever. After
/// this long the old worker is force-terminated and its stragglers are failed.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(600);

/// Grace period for shutdown. On shutdown the live worker's stdin is closed so it
/// can finish in-flight work and exit on its own; a worker that ignores stdin EOF
/// (or hangs) would otherwise keep the process alive forever, so after this long
/// any remaining worker is force-terminated.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

pub struct MessageRouter {
    /// The single live worker. All new work is routed here. `None` only while a
    /// predecessor is draining before its replacement starts: there is never more
    /// than one worker process at a time.
    current: Option<Generation>,
    /// A predecessor finishing its in-flight tasks before the replacement starts.
    /// Present only while `current` is `None`; the two are never live together.
    draining: Option<Generation>,
    /// Work received while draining, replayed to the replacement once the old
    /// worker has finished and exited (no work runs on two worker versions at once).
    pending: VecDeque<WorkerMessage>,
    next_gen_id: u64,
    command: Vec<String>,
    stdout: Box<dyn AsyncWrite + Unpin + Send>,
    stderr_writer: SharedWriter,
    events_tx: mpsc::Sender<RouterEvent>,
    shutting_down: bool,
    /// Timestamps of recent restarts, pruned to `RESTART_WINDOW`, for throttling.
    restarts: VecDeque<Instant>,
    /// Opt-in lifecycle logging, enabled by `LUCHTA_WORKER_WATCHER_DEBUG`.
    debug: bool,
}

impl MessageRouter {
    pub async fn new(
        command: Vec<String>,
        events_tx: mpsc::Sender<RouterEvent>,
        stdout: Box<dyn AsyncWrite + Unpin + Send>,
    ) -> Result<Self, ProxyError> {
        let stderr_writer = shared_stderr_writer();
        let (current, stdout_rx) =
            Generation::new(0, command.clone(), std::sync::Arc::clone(&stderr_writer))?;
        spawn_stdout_reader(
            0,
            stdout_rx,
            events_tx.clone(),
            std::sync::Arc::clone(&stderr_writer),
        );
        Ok(Self {
            current: Some(current),
            draining: None,
            pending: VecDeque::new(),
            next_gen_id: 1,
            command,
            stdout,
            stderr_writer,
            events_tx,
            shutting_down: false,
            restarts: VecDeque::new(),
            debug: std::env::var_os("LUCHTA_WORKER_WATCHER_DEBUG").is_some(),
        })
    }

    pub async fn run(
        mut self,
        mut events_rx: mpsc::Receiver<RouterEvent>,
    ) -> Result<(), ProxyError> {
        while let Some(event) = events_rx.recv().await {
            self.handle_event(event).await?;
            if self.should_stop() {
                break;
            }
        }

        self.shutdown_remaining().await
    }

    async fn handle_event(&mut self, event: RouterEvent) -> Result<(), ProxyError> {
        match event {
            RouterEvent::Inbound(message) => self.handle_inbound(message).await,
            RouterEvent::Response(gen_id, response) => self.handle_response(gen_id, response).await,
            RouterEvent::StdoutClosed(gen_id) => self.handle_stdout_closed(gen_id).await,
            RouterEvent::FileChanged => self.restart().await,
            RouterEvent::DrainTimeout(gen_id) => self.handle_drain_timeout(gen_id).await,
            RouterEvent::ShutdownAll => self.handle_shutdown_all().await,
            RouterEvent::ShutdownTimeout => self.handle_shutdown_timeout().await,
        }
    }

    async fn handle_inbound(&mut self, message: WorkerMessage) -> Result<(), ProxyError> {
        if self.shutting_down {
            return Ok(());
        }
        if self.current.is_some() {
            self.send_to_current(message).await
        } else {
            // A predecessor is draining; hold new work until the replacement starts.
            self.pending.push_back(message);
            Ok(())
        }
    }

    async fn send_to_current(&mut self, message: WorkerMessage) -> Result<(), ProxyError> {
        let id = message.id().to_owned();
        let kind = in_flight_kind(&message);
        match self.current.as_mut() {
            Some(current) => {
                if let Err(error) = current.send(&message) {
                    let gen_id = current.id();
                    log_router_error(
                        &self.stderr_writer,
                        format!(
                            "router failed to send message {id} to generation {gen_id}: {error}"
                        ),
                    )
                    .await;
                    self.synthesize_terminal_for_failed_send(&id, kind).await?;
                }
                Ok(())
            }
            // No live worker (only reachable during shutdown teardown). Fail the job
            // so the engine is not left waiting for a response.
            None => self.synthesize_terminal_for_failed_send(&id, kind).await,
        }
    }

    async fn handle_response(
        &mut self,
        gen_id: u64,
        response: WorkerResponse,
    ) -> Result<(), ProxyError> {
        if self.current.as_ref().is_some_and(|c| c.id() == gen_id) {
            self.current.as_mut().unwrap().on_response(&response);
            return self.write_response(&response).await;
        }
        if self.draining.as_ref().is_some_and(|d| d.id() == gen_id) {
            self.draining.as_mut().unwrap().on_response(&response);
            self.write_response(&response).await?;
            if self.draining.as_ref().unwrap().in_flight_len() == 0 {
                // The old worker has finished every in-flight task. Terminate it and
                // start the replacement, replaying any work queued during the drain.
                let drained = self.draining.take().unwrap();
                self.log_lifecycle(format!(
                    "generation {} finished draining; terminating",
                    drained.id()
                ))
                .await;
                drained.shutdown().await?;
                return self.complete_drain().await;
            }
            return Ok(());
        }
        // A straggler from an already-reaped generation: its in-flight ids were
        // terminated when it was reaped, so drop this to avoid double-reporting.
        Ok(())
    }

    async fn handle_stdout_closed(&mut self, gen_id: u64) -> Result<(), ProxyError> {
        if self.current.as_ref().is_some_and(|c| c.id() == gen_id) {
            let current = self.current.take().unwrap();
            if self.shutting_down {
                // Exited after stdin EOF during shutdown: fail any stragglers, reap.
                self.synthesize_terminals_for(&current).await?;
                current.shutdown().await?;
                return Ok(());
            }
            // The live worker crashed. Fail its in-flight tasks (the engine treats a
            // done/resolved as terminal) and respawn.
            log_generation_exit(&current, &self.stderr_writer).await;
            self.synthesize_terminals_for(&current).await?;
            current.shutdown().await?;
            self.throttle_restart().await;
            return self.spawn_current().await;
        }
        if self.draining.as_ref().is_some_and(|d| d.id() == gen_id) {
            // The draining worker exited on its own. If it still had in-flight tasks
            // it crashed mid-drain: fail them. Then start the replacement.
            let drained = self.draining.take().unwrap();
            self.synthesize_terminals_for(&drained).await?;
            drained.shutdown().await?;
            return self.complete_drain().await;
        }
        Ok(())
    }

    /// Begins replacing the current worker when a watched file changes.
    ///
    /// There is only ever one worker process. If the current worker is idle it is
    /// terminated at once and the replacement starts immediately. If it is busy, it
    /// is moved to the draining slot to finish its in-flight tasks; new work is
    /// queued (see [`Self::handle_inbound`]) until it has finished and exited, at
    /// which point the replacement starts and the queue is replayed. Running two
    /// worker versions at once is deliberately avoided: it is low value (a restart
    /// only happens when the worker source is edited or rebuilt) and a correctness
    /// hazard. An earlier model let idle draining generations accumulate — one per
    /// restart — because an idle generation was reaped only on a response or stdout
    /// close that never came.
    async fn restart(&mut self) -> Result<(), ProxyError> {
        if self.shutting_down {
            return Ok(());
        }
        if self.draining.is_some() {
            // Already draining a predecessor; its replacement will be fresh. Nothing
            // to do — coalesce this change into the restart already in progress.
            self.log_lifecycle("watched file changed while draining; coalescing".to_owned())
                .await;
            return Ok(());
        }
        let Some(old) = self.current.take() else {
            return Ok(());
        };
        if old.in_flight_len() == 0 {
            self.log_lifecycle(format!(
                "watched file changed; generation {} idle, terminating",
                old.id()
            ))
            .await;
            old.shutdown().await?;
            return self.complete_drain().await;
        }
        self.log_lifecycle(format!(
            "watched file changed; draining generation {} ({} in-flight), queueing new work",
            old.id(),
            old.in_flight_len()
        ))
        .await;
        let gen_id = old.id();
        self.draining = Some(old);
        self.schedule_drain_timeout(gen_id);
        Ok(())
    }

    async fn handle_drain_timeout(&mut self, gen_id: u64) -> Result<(), ProxyError> {
        if !self.draining.as_ref().is_some_and(|d| d.id() == gen_id) {
            // The drain already finished; this is a stale timer.
            return Ok(());
        }
        let drained = self.draining.take().unwrap();
        log_router_error(
            &self.stderr_writer,
            format!(
                "worker-watcher: generation {} did not finish draining within {DRAIN_TIMEOUT:?}; force-terminating and failing {} in-flight task(s)",
                drained.id(),
                drained.in_flight_len()
            ),
        )
        .await;
        self.synthesize_terminals_for(&drained).await?;
        drained.shutdown().await?;
        self.complete_drain().await
    }

    /// Starts the replacement worker once a predecessor has finished draining and
    /// replays work queued during the drain. The caller has already reaped the
    /// drained generation. During shutdown this still runs queued work — so the
    /// engine is not left waiting — then closes the replacement's stdin so it exits.
    async fn complete_drain(&mut self) -> Result<(), ProxyError> {
        if self.shutting_down && self.pending.is_empty() {
            return Ok(());
        }
        self.throttle_restart().await;
        self.spawn_current().await?;
        let pending = std::mem::take(&mut self.pending);
        for message in pending {
            self.send_to_current(message).await?;
        }
        if self.shutting_down {
            if let Some(current) = self.current.as_ref() {
                current.close_stdin().await;
            }
        }
        Ok(())
    }

    async fn spawn_current(&mut self) -> Result<(), ProxyError> {
        let (next, stdout_rx) = Generation::new(
            self.next_gen_id,
            self.command.clone(),
            std::sync::Arc::clone(&self.stderr_writer),
        )?;
        spawn_stdout_reader(
            self.next_gen_id,
            stdout_rx,
            self.events_tx.clone(),
            std::sync::Arc::clone(&self.stderr_writer),
        );
        self.log_lifecycle(format!("started worker generation {}", self.next_gen_id))
            .await;
        self.current = Some(next);
        self.next_gen_id += 1;
        Ok(())
    }

    fn schedule_drain_timeout(&self, gen_id: u64) {
        let events_tx = self.events_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DRAIN_TIMEOUT).await;
            let _ = events_tx.send(RouterEvent::DrainTimeout(gen_id)).await;
        });
    }

    /// Rate-limits restarts so a crash loop (a worker that exits immediately) or a
    /// noisy file watch cannot spawn workers as fast as the CPU allows. After
    /// `RESTART_BURST` restarts within `RESTART_WINDOW`, each further restart waits
    /// `RESTART_BACKOFF` and logs a warning.
    async fn throttle_restart(&mut self) {
        let now = Instant::now();
        while self
            .restarts
            .front()
            .is_some_and(|t| now.duration_since(*t) > RESTART_WINDOW)
        {
            self.restarts.pop_front();
        }
        if self.restarts.len() >= RESTART_BURST {
            log_router_error(
                &self.stderr_writer,
                format!(
                    "worker-watcher: {} restarts within {RESTART_WINDOW:?}; backing off {RESTART_BACKOFF:?} before the next (possible crash loop or noisy watch)",
                    self.restarts.len(),
                ),
            )
            .await;
            tokio::time::sleep(RESTART_BACKOFF).await;
        }
        self.restarts.push_back(Instant::now());
    }

    async fn log_lifecycle(&mut self, message: String) {
        if self.debug {
            log_router_error(&self.stderr_writer, format!("worker-watcher: {message}")).await;
        }
    }

    async fn handle_shutdown_all(&mut self) -> Result<(), ProxyError> {
        if self.shutting_down {
            return Ok(());
        }
        self.shutting_down = true;
        // Stop new dispatch and let the live worker finish its in-flight jobs and
        // exit on its own: closing stdin signals EOF, and the run loop reaps it when
        // its stdout closes (see `handle_stdout_closed`). If a drain is in progress
        // it finishes normally, any queued work still runs on the replacement (which
        // is then closed too, see `complete_drain`), and `should_stop` ends the loop
        // once nothing remains. A worker that ignores stdin EOF is force-terminated
        // after `SHUTDOWN_GRACE` (see `handle_shutdown_timeout`) so shutdown cannot
        // hang.
        if let Some(current) = self.current.as_ref() {
            current.close_stdin().await;
        }
        self.schedule_shutdown_timeout();
        Ok(())
    }

    fn schedule_shutdown_timeout(&self) {
        let events_tx = self.events_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SHUTDOWN_GRACE).await;
            let _ = events_tx.send(RouterEvent::ShutdownTimeout).await;
        });
    }

    async fn handle_shutdown_timeout(&mut self) -> Result<(), ProxyError> {
        if let Some(current) = self.current.take() {
            log_router_error(
                &self.stderr_writer,
                format!(
                    "worker-watcher: generation {} did not exit within {SHUTDOWN_GRACE:?} of shutdown; force-terminating",
                    current.id()
                ),
            )
            .await;
            self.synthesize_terminals_for(&current).await?;
            current.shutdown().await?;
        }
        if let Some(draining) = self.draining.take() {
            self.synthesize_terminals_for(&draining).await?;
            draining.shutdown().await?;
        }
        self.pending.clear();
        Ok(())
    }

    async fn synthesize_terminals_for(
        &mut self,
        generation: &Generation,
    ) -> Result<(), ProxyError> {
        let in_flight = generation.drain_in_flight();
        for (id, kind) in in_flight {
            self.synthesize_terminal_for_failed_send(&id, kind).await?;
        }
        Ok(())
    }

    async fn synthesize_terminal_for_failed_send(
        &mut self,
        id: &str,
        kind: InFlightKind,
    ) -> Result<(), ProxyError> {
        self.write_response(&failed_send_response(id, kind)).await
    }

    async fn write_response(&mut self, response: &WorkerResponse) -> Result<(), ProxyError> {
        let line = serde_json::to_string(response)?;
        self.stdout.write_all(line.as_bytes()).await?;
        self.stdout.write_all(b"\n").await?;
        self.stdout.flush().await?;
        Ok(())
    }

    fn should_stop(&self) -> bool {
        self.shutting_down
            && self.current.is_none()
            && self.draining.is_none()
            && self.pending.is_empty()
    }

    async fn shutdown_remaining(&mut self) -> Result<(), ProxyError> {
        if let Some(current) = self.current.take() {
            current.shutdown().await?;
        }
        if let Some(draining) = self.draining.take() {
            draining.shutdown().await?;
        }
        Ok(())
    }
}

fn failed_send_response(id: &str, kind: InFlightKind) -> WorkerResponse {
    match kind {
        InFlightKind::Run => WorkerResponse::done(id, 1),
        InFlightKind::Resolve => WorkerResponse::resolved(
            id,
            ResolveResult::reject("worker restarted before resolve completed"),
        ),
    }
}

fn shared_stderr_writer() -> SharedWriter {
    std::sync::Arc::new(tokio::sync::Mutex::new(Box::new(stderr())))
}

pub fn spawn_stdout_reader(
    gen_id: u64,
    mut rx: mpsc::Receiver<String>,
    events_tx: mpsc::Sender<RouterEvent>,
    stderr_writer: SharedWriter,
) {
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            match serde_json::from_str::<WorkerResponse>(&line) {
                Ok(response) => {
                    if events_tx
                        .send(RouterEvent::Response(gen_id, response))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    log_router_error(
                        &stderr_writer,
                        format!(
                            "router failed to parse generation {gen_id} stdout line as worker response: {error}"
                        ),
                    )
                    .await;
                }
            }
        }

        let _ = events_tx.send(RouterEvent::StdoutClosed(gen_id)).await;
    });
}

async fn log_router_error(stderr_writer: &SharedWriter, message: String) {
    let mut stderr = stderr_writer.lock().await;
    let _ = stderr.write_all(message.as_bytes()).await;
    let _ = stderr.write_all(b"\n").await;
    let _ = stderr.flush().await;
}

async fn log_generation_exit(generation: &Generation, stderr_writer: &SharedWriter) {
    let exit = generation
        .exit_status()
        .await
        .map(|status| status.to_string())
        .unwrap_or_else(|| "<unknown>".to_owned());
    log_router_error(
        stderr_writer,
        format!(
            "delegate exited: command={:?}, exit={exit}",
            generation.command()
        ),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use luchta_worker::{ResolveDecision, ResolveMode, ResolveResult, ResolveTask, WorkerRequest};
    use tokio::io::{duplex, AsyncBufReadExt, BufReader, DuplexStream};

    use super::*;

    fn run_message(id: &str, command: &str) -> WorkerMessage {
        WorkerMessage::Run(WorkerRequest::new(id, command))
    }

    fn resolve_message(id: &str) -> WorkerMessage {
        WorkerMessage::ResolveTask(ResolveTask {
            id: id.to_owned(),
            name: "build".to_owned(),
            command: String::new(),
            package: "pkg".to_owned(),
            cwd: None,
            scripts: Vec::new(),
            inputs: Vec::new(),
            mode: ResolveMode::Run,
        })
    }

    fn read_lines_task(reader: DuplexStream) -> tokio::task::JoinHandle<Vec<String>> {
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            let mut out = Vec::new();
            while let Some(line) = lines.next_line().await.expect("read line") {
                out.push(line);
            }
            out
        })
    }

    fn loopback_delegate_command() -> Vec<String> {
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            r#"while IFS= read -r line; do
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    case $line in
        *'"type":"run"'*)
            printf '{"type":"done","id":"%s","exitCode":0}\n' "$id"
            ;;
        *)
            printf '{"type":"resolved","id":"%s","result":{"decision":"accept"}}\n' "$id"
            ;;
    esac
done
"#
            .to_owned(),
        ]
    }

    fn delayed_loopback_delegate_command() -> Vec<String> {
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            r#"while IFS= read -r line; do
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    cmd=$(printf '%s\n' "$line" | sed -n 's/.*"command":"\([^"]*\)".*/\1/p')
    case $cmd in
        slow*)
            sleep 0.2
            ;;
        *)
            ;;
    esac
    case $line in
        *'"type":"run"'*)
            printf '{"type":"done","id":"%s","exitCode":0}\n' "$id"
            ;;
        *)
            printf '{"type":"resolved","id":"%s","result":{"decision":"accept"}}\n' "$id"
            ;;
    esac
done
"#
            .to_owned(),
        ]
    }

    fn crash_delegate_command() -> Vec<String> {
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            r#"IFS= read -r _line
exit 1
"#
            .to_owned(),
        ]
    }

    async fn collect_json_lines(
        reader_task: tokio::task::JoinHandle<Vec<String>>,
    ) -> Vec<WorkerResponse> {
        reader_task
            .await
            .expect("reader task joins")
            .into_iter()
            .map(|line| serde_json::from_str(&line).expect("json response"))
            .collect()
    }

    async fn spawn_router(
        command: Vec<String>,
    ) -> (
        mpsc::Sender<RouterEvent>,
        tokio::task::JoinHandle<Result<(), ProxyError>>,
        tokio::task::JoinHandle<Vec<String>>,
    ) {
        let (writer, reader) = duplex(16 * 1024);
        let reader_task = read_lines_task(reader);
        let (events_tx, events_rx) = mpsc::channel(128);
        let router = MessageRouter::new(command, events_tx.clone(), Box::new(writer))
            .await
            .expect("create router");
        let router_task = tokio::spawn(async move { router.run(events_rx).await });
        (events_tx, router_task, reader_task)
    }

    async fn finish_router(
        events_tx: mpsc::Sender<RouterEvent>,
        router_task: tokio::task::JoinHandle<Result<(), ProxyError>>,
        reader_task: tokio::task::JoinHandle<Vec<String>>,
    ) -> Vec<WorkerResponse> {
        events_tx
            .send(RouterEvent::ShutdownAll)
            .await
            .expect("send shutdown");
        drop(events_tx);
        finish_router_without_shutdown(router_task, reader_task).await
    }

    async fn finish_router_without_shutdown(
        router_task: tokio::task::JoinHandle<Result<(), ProxyError>>,
        reader_task: tokio::task::JoinHandle<Vec<String>>,
    ) -> Vec<WorkerResponse> {
        tokio::time::timeout(Duration::from_secs(3), router_task)
            .await
            .expect("router should finish")
            .expect("router join")
            .expect("router ok");
        collect_json_lines(reader_task).await
    }

    async fn run_loopback_router(message: WorkerMessage) -> Vec<WorkerResponse> {
        let (events_tx, router_task, reader_task) = spawn_router(loopback_delegate_command()).await;
        events_tx
            .send(RouterEvent::Inbound(message))
            .await
            .expect("send inbound");
        finish_router(events_tx, router_task, reader_task).await
    }

    #[tokio::test]
    async fn inbound_routes_to_current_generation() {
        let output = run_loopback_router(run_message("job-1", "build")).await;
        assert!(output.contains(&WorkerResponse::done("job-1", 0)));
    }

    #[tokio::test]
    async fn response_is_forwarded_to_stdout_verbatim() {
        let output = run_loopback_router(resolve_message("resolve-1")).await;
        assert!(output.contains(&WorkerResponse::resolved(
            "resolve-1",
            ResolveResult::accept()
        )));
    }

    #[tokio::test]
    async fn restart_queues_new_work_until_old_worker_drains() {
        let (events_tx, router_task, reader_task) =
            spawn_router(delayed_loopback_delegate_command()).await;

        events_tx
            .send(RouterEvent::Inbound(run_message("old", "slow-build")))
            .await
            .expect("send slow inbound");
        events_tx
            .send(RouterEvent::FileChanged)
            .await
            .expect("send file change");
        events_tx
            .send(RouterEvent::Inbound(run_message("new", "build")))
            .await
            .expect("send new inbound");

        let output = finish_router(events_tx, router_task, reader_task).await;
        // Single-worker model: the old worker keeps running its in-flight job to
        // completion (no spurious failure), while new work is queued during the
        // drain and then runs on the fresh worker once the old one has finished.
        assert!(output.contains(&WorkerResponse::done("old", 0)));
        assert!(output.contains(&WorkerResponse::done("new", 0)));
    }

    #[tokio::test]
    async fn repeated_file_changes_keep_routing_and_do_not_wedge() {
        // Regression test for the generation leak: before the single-worker fix,
        // each FileChanged spawned a new generation and left the old one draining
        // forever. Here many changes must still leave exactly one working worker
        // that routes new work to completion.
        let (events_tx, router_task, reader_task) = spawn_router(loopback_delegate_command()).await;

        for _ in 0..5 {
            events_tx
                .send(RouterEvent::FileChanged)
                .await
                .expect("send file change");
        }
        events_tx
            .send(RouterEvent::Inbound(run_message("after", "build")))
            .await
            .expect("send inbound after rotations");

        let output = finish_router(events_tx, router_task, reader_task).await;
        assert!(output.contains(&WorkerResponse::done("after", 0)));
    }

    #[tokio::test]
    async fn shutdown_force_terminates_worker_that_ignores_stdin_eof() {
        // A delegate that never exits on stdin EOF must not hang shutdown: after the
        // grace period the run loop force-terminates it and stops.
        let (events_tx, router_task, reader_task) =
            spawn_router(vec!["sleep".to_owned(), "30".to_owned()]).await;

        events_tx
            .send(RouterEvent::ShutdownAll)
            .await
            .expect("send shutdown");
        drop(events_tx);

        tokio::time::timeout(SHUTDOWN_GRACE + Duration::from_secs(3), router_task)
            .await
            .expect("router shuts down within the grace period plus margin")
            .expect("router join")
            .expect("router ok");
        let output = collect_json_lines(reader_task).await;
        assert!(output.is_empty(), "no responses expected, got {output:?}");
    }

    #[tokio::test]
    async fn crash_synthesizes_terminal_done_for_outstanding_id() {
        let (events_tx, router_task, reader_task) = spawn_router(crash_delegate_command()).await;

        events_tx
            .send(RouterEvent::Inbound(run_message("crash-id", "build")))
            .await
            .expect("send inbound");

        let output = finish_router(events_tx, router_task, reader_task).await;
        assert!(output.contains(&WorkerResponse::done("crash-id", 1)));
    }

    #[tokio::test]
    async fn send_failure_synthesizes_terminal_done_for_inbound_id() {
        let output = run_failed_send_router(run_message("send-fail", "build")).await;
        assert!(output.contains(&WorkerResponse::done("send-fail", 1)));
    }

    #[tokio::test]
    async fn resolve_send_failure_synthesizes_rejected_resolved_response() {
        let output = run_failed_send_router(resolve_message("resolve-fail")).await;
        assert!(output.iter().any(|response| {
            matches!(
                response,
                WorkerResponse::Resolved { id, result }
                    if id == "resolve-fail"
                        && matches!(
                            result.decision,
                            ResolveDecision::Reject { ref message }
                                if message == "worker restarted before resolve completed"
                        )
            )
        }));
    }

    async fn run_failed_send_router(message: WorkerMessage) -> Vec<WorkerResponse> {
        let (writer, reader) = duplex(16 * 1024);
        let reader_task = read_lines_task(reader);
        let (events_tx, events_rx) = mpsc::channel(128);
        let mut router = MessageRouter::new(
            loopback_delegate_command(),
            events_tx.clone(),
            Box::new(writer),
        )
        .await
        .expect("create router");

        let current = router.current.as_mut().expect("current generation");
        current.close_stdin().await;

        let router_task = tokio::spawn(async move { router.run(events_rx).await });
        events_tx
            .send(RouterEvent::Inbound(message))
            .await
            .expect("send inbound");
        finish_router(events_tx, router_task, reader_task).await
    }
}
