use std::mem;

use luchta_worker::{ProxyError, ResolveResult, SharedWriter, WorkerMessage, WorkerResponse};
use tokio::io::{stderr, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::generation::{in_flight_kind, Generation, InFlightKind};

pub enum RouterEvent {
    Inbound(WorkerMessage),
    Response(u64, WorkerResponse),
    StdoutClosed(u64),
    FileChanged,
    ShutdownAll,
}

pub struct MessageRouter {
    current: Option<Generation>,
    draining: Vec<Generation>,
    next_gen_id: u64,
    command: Vec<String>,
    stdout: Box<dyn AsyncWrite + Unpin + Send>,
    stderr_writer: SharedWriter,
    events_tx: mpsc::Sender<RouterEvent>,
    shutting_down: bool,
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
        spawn_stdout_reader(0, stdout_rx, events_tx.clone());
        Ok(Self {
            current: Some(current),
            draining: Vec::new(),
            next_gen_id: 1,
            command,
            stdout,
            stderr_writer,
            events_tx,
            shutting_down: false,
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
            RouterEvent::FileChanged => self.rotate().await,
            RouterEvent::ShutdownAll => self.handle_shutdown_all().await,
        }
    }

    async fn handle_inbound(&mut self, message: WorkerMessage) -> Result<(), ProxyError> {
        if self.shutting_down {
            return Ok(());
        }

        if let Some(current) = self.current.as_mut() {
            if let Err(error) = current.send(&message) {
                log_router_error(format!(
                    "router failed to send inbound message {} to generation {}: {error}",
                    message.id(),
                    current.id()
                ))
                .await;
                self.synthesize_terminal_for_failed_send(message.id(), in_flight_kind(&message))
                    .await?;
            }
        }
        Ok(())
    }

    async fn handle_response(
        &mut self,
        gen_id: u64,
        response: WorkerResponse,
    ) -> Result<(), ProxyError> {
        self.write_response(&response).await?;

        let drained = match self.find_gen_mut(gen_id) {
            Some(generation) => generation.on_response(&response),
            None => false,
        };

        if drained {
            self.remove_drained_gen(gen_id).await?;
        }

        Ok(())
    }

    async fn handle_stdout_closed(&mut self, gen_id: u64) -> Result<(), ProxyError> {
        if let Some(current) = self.current.as_ref() {
            if current.id() == gen_id {
                self.handle_current_stdout_closed().await?;
                return Ok(());
            }
        }

        self.handle_draining_stdout_closed(gen_id).await
    }

    async fn handle_current_stdout_closed(&mut self) -> Result<(), ProxyError> {
        let Some(current) = self.current.take() else {
            return Ok(());
        };

        self.synthesize_terminals_for(&current).await?;
        current.shutdown().await?;

        if self.shutting_down {
            return Ok(());
        }

        let (next, stdout_rx) = Generation::new(
            self.next_gen_id,
            self.command.clone(),
            std::sync::Arc::clone(&self.stderr_writer),
        )?;
        spawn_stdout_reader(self.next_gen_id, stdout_rx, self.events_tx.clone());
        self.current = Some(next);
        self.next_gen_id += 1;
        Ok(())
    }

    async fn handle_draining_stdout_closed(&mut self, gen_id: u64) -> Result<(), ProxyError> {
        self.remove_draining_generation(gen_id, true).await
    }

    async fn handle_shutdown_all(&mut self) -> Result<(), ProxyError> {
        if self.shutting_down {
            return Ok(());
        }

        self.shutting_down = true;
        self.move_current_to_draining().await
    }

    async fn rotate(&mut self) -> Result<(), ProxyError> {
        if self.shutting_down {
            return Ok(());
        }

        let (next, stdout_rx) = Generation::new(
            self.next_gen_id,
            self.command.clone(),
            std::sync::Arc::clone(&self.stderr_writer),
        )?;
        let next_id = next.id();
        spawn_stdout_reader(next_id, stdout_rx, self.events_tx.clone());
        let old_current = self.current.replace(next);
        self.next_gen_id += 1;

        if let Some(mut generation) = old_current {
            generation.mark_draining().await;
            self.draining.push(generation);
        }

        Ok(())
    }

    async fn move_current_to_draining(&mut self) -> Result<(), ProxyError> {
        if let Some(mut generation) = self.current.take() {
            generation.mark_draining().await;
            self.draining.push(generation);
        }
        Ok(())
    }

    async fn remove_drained_gen(&mut self, gen_id: u64) -> Result<(), ProxyError> {
        self.remove_draining_generation(gen_id, false).await
    }

    async fn remove_draining_generation(
        &mut self,
        gen_id: u64,
        synthesize: bool,
    ) -> Result<(), ProxyError> {
        if let Some(generation) = self.take_draining_generation(gen_id) {
            if synthesize {
                self.synthesize_terminals_for(&generation).await?;
            }
            generation.shutdown().await?;
        }
        Ok(())
    }

    fn take_draining_generation(&mut self, gen_id: u64) -> Option<Generation> {
        let index = self
            .draining
            .iter()
            .position(|generation| generation.id() == gen_id)?;
        Some(self.draining.swap_remove(index))
    }

    fn find_gen_mut(&mut self, gen_id: u64) -> Option<&mut Generation> {
        if let Some(current) = self.current.as_mut() {
            if current.id() == gen_id {
                return Some(current);
            }
        }
        self.draining
            .iter_mut()
            .find(|generation| generation.id() == gen_id)
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
        self.shutting_down && self.current.is_none() && self.draining.is_empty()
    }

    async fn shutdown_remaining(&mut self) -> Result<(), ProxyError> {
        if let Some(current) = self.current.take() {
            current.shutdown().await?;
        }

        let draining = mem::take(&mut self.draining);
        for generation in draining {
            generation.shutdown().await?;
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
                    log_router_error(format!(
                        "router failed to parse generation {gen_id} stdout line as worker response: {error}"
                    ))
                    .await;
                }
            }
        }

        let _ = events_tx.send(RouterEvent::StdoutClosed(gen_id)).await;
    });
}

async fn log_router_error(message: String) {
    let mut stderr = stderr();
    let _ = stderr.write_all(message.as_bytes()).await;
    let _ = stderr.write_all(b"\n").await;
    let _ = stderr.flush().await;
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
    async fn rotate_sends_new_work_to_new_current_and_old_work_still_finishes() {
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
        assert!(output.contains(&WorkerResponse::done("old", 0)));
        assert!(output.contains(&WorkerResponse::done("new", 0)));
    }

    #[tokio::test]
    async fn draining_generation_is_shutdown_when_last_terminal_arrives() {
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

        let output = finish_router(events_tx, router_task, reader_task).await;
        assert!(output.contains(&WorkerResponse::done("old", 0)));
    }

    #[tokio::test]
    async fn multiple_draining_generations_drain_independently() {
        let (events_tx, router_task, reader_task) =
            spawn_router(delayed_loopback_delegate_command()).await;

        events_tx
            .send(RouterEvent::Inbound(run_message("first", "slow-first")))
            .await
            .expect("send first inbound");
        events_tx
            .send(RouterEvent::FileChanged)
            .await
            .expect("send first rotation");
        events_tx
            .send(RouterEvent::Inbound(run_message("second", "slow-second")))
            .await
            .expect("send second inbound");
        events_tx
            .send(RouterEvent::FileChanged)
            .await
            .expect("send second rotation");
        events_tx
            .send(RouterEvent::Inbound(run_message("third", "build")))
            .await
            .expect("send third inbound");

        let output = finish_router(events_tx, router_task, reader_task).await;
        assert!(output.contains(&WorkerResponse::done("first", 0)));
        assert!(output.contains(&WorkerResponse::done("second", 0)));
        assert!(output.contains(&WorkerResponse::done("third", 0)));
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
        current.mark_draining().await;

        let router_task = tokio::spawn(async move { router.run(events_rx).await });
        events_tx
            .send(RouterEvent::Inbound(message))
            .await
            .expect("send inbound");
        finish_router(events_tx, router_task, reader_task).await
    }
}
