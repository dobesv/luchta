use std::collections::HashMap;

use luchta_worker::{ProxyError, RawDelegate, SharedWriter, WorkerMessage, WorkerResponse};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InFlightKind {
    Run,
    Resolve,
}

pub struct Generation {
    id: u64,
    delegate: RawDelegate,
    in_flight: HashMap<String, InFlightKind>,
    draining: bool,
}

impl Generation {
    pub fn new(
        id: u64,
        command: Vec<String>,
        stderr_writer: SharedWriter,
    ) -> Result<(Self, mpsc::Receiver<String>), ProxyError> {
        let mut delegate = RawDelegate::spawn_with_stderr(command, stderr_writer)?;
        let stdout = delegate
            .take_stdout()
            .ok_or(ProxyError::MissingPipe("stdout"))?;
        Ok((
            Self {
                id,
                delegate,
                in_flight: HashMap::new(),
                draining: false,
            },
            stdout,
        ))
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn send(&mut self, msg: &WorkerMessage) -> Result<(), ProxyError> {
        let line = serde_json::to_string(msg)?;
        self.delegate.send_line(line)?;
        self.in_flight
            .insert(msg.id().to_owned(), in_flight_kind(msg));
        Ok(())
    }

    pub fn on_response(&mut self, resp: &WorkerResponse) -> bool {
        if is_terminal(resp) {
            self.in_flight.remove(resp.id());
        }
        self.is_drained()
    }

    pub async fn mark_draining(&mut self) {
        self.draining = true;
        self.delegate.close_stdin().await;
    }

    pub fn is_drained(&self) -> bool {
        self.draining && self.in_flight.is_empty()
    }

    pub fn is_draining(&self) -> bool {
        self.draining
    }

    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    pub fn drain_in_flight(&self) -> Vec<(String, InFlightKind)> {
        self.in_flight
            .iter()
            .map(|(id, kind)| (id.clone(), *kind))
            .collect()
    }

    pub async fn shutdown(self) -> Result<(), ProxyError> {
        self.delegate.shutdown().await
    }
}

pub fn in_flight_kind(message: &WorkerMessage) -> InFlightKind {
    match message {
        WorkerMessage::Run(_) => InFlightKind::Run,
        WorkerMessage::ResolveTask(_) => InFlightKind::Resolve,
    }
}

fn is_terminal(resp: &WorkerResponse) -> bool {
    matches!(
        resp,
        WorkerResponse::Done { .. } | WorkerResponse::Resolved { .. }
    )
}

#[cfg(test)]
fn test_stderr_writer() -> SharedWriter {
    std::sync::Arc::new(tokio::sync::Mutex::new(Box::new(tokio::io::stderr())))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use luchta_worker::{LogStream, ResolveMode, ResolveResult, ResolveTask, WorkerRequest};

    use super::*;

    fn run_message(id: &str) -> WorkerMessage {
        WorkerMessage::Run(WorkerRequest::new(id, "build"))
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

    async fn cat_generation(id: u64) -> Generation {
        let (generation, _stdout) =
            Generation::new(id, vec!["cat".to_owned()], test_stderr_writer())
                .expect("spawn generation");
        generation
    }

    #[tokio::test]
    async fn terminal_response_removes_in_flight_id() {
        let mut generation = cat_generation(1).await;
        generation.send(&run_message("a")).expect("send succeeds");

        assert_eq!(generation.in_flight_len(), 1);
        assert!(!generation.on_response(&WorkerResponse::done("a", 0)));
        assert_eq!(generation.in_flight_len(), 0);

        generation.shutdown().await.expect("shutdown succeeds");
    }

    #[tokio::test]
    async fn non_terminal_response_keeps_in_flight_id() {
        let mut generation = cat_generation(2).await;
        generation.send(&run_message("a")).expect("send succeeds");

        assert!(!generation.on_response(&WorkerResponse::log("a", LogStream::Stdout, "line")));
        assert_eq!(generation.in_flight_len(), 1);

        generation.shutdown().await.expect("shutdown succeeds");
    }

    #[tokio::test]
    async fn mark_draining_then_terminal_response_marks_generation_drained() {
        let mut generation = cat_generation(3).await;
        generation.send(&run_message("a")).expect("send succeeds");

        generation.mark_draining().await;

        assert!(generation.on_response(&WorkerResponse::done("a", 0)));
        assert!(generation.is_drained());

        generation.shutdown().await.expect("shutdown succeeds");
    }

    #[tokio::test]
    async fn is_drained_tracks_draining_and_in_flight_state() {
        let mut empty_generation = cat_generation(4).await;
        assert!(!empty_generation.is_drained());
        empty_generation.mark_draining().await;
        assert!(empty_generation.is_drained());
        empty_generation
            .shutdown()
            .await
            .expect("shutdown succeeds");

        let mut busy_generation = cat_generation(5).await;
        busy_generation
            .send(&resolve_message("a"))
            .expect("send succeeds");
        assert!(!busy_generation.is_drained());
        busy_generation.mark_draining().await;
        assert!(!busy_generation.is_drained());
        assert!(
            busy_generation.on_response(&WorkerResponse::resolved("a", ResolveResult::accept()))
        );
        assert!(busy_generation.is_drained());
        busy_generation.shutdown().await.expect("shutdown succeeds");
    }

    #[tokio::test]
    async fn drain_in_flight_ids_returns_outstanding_ids() {
        let mut generation = cat_generation(6).await;
        generation.send(&run_message("a")).expect("send succeeds");
        generation
            .send(&resolve_message("b"))
            .expect("send succeeds");

        let mut in_flight = generation.drain_in_flight();
        in_flight.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            in_flight,
            vec![
                ("a".to_owned(), InFlightKind::Run),
                ("b".to_owned(), InFlightKind::Resolve),
            ]
        );

        generation.shutdown().await.expect("shutdown succeeds");
    }

    #[tokio::test]
    async fn new_returns_generation_id_and_stdout_receiver() {
        let (generation, mut stdout) =
            Generation::new(7, vec!["cat".to_owned()], test_stderr_writer())
                .expect("spawn generation");
        assert_eq!(generation.id(), 7);
        drop(generation);
        let closed = tokio::time::timeout(Duration::from_secs(2), stdout.recv())
            .await
            .expect("recv should complete after drop");
        assert!(closed.is_none());
    }
}
