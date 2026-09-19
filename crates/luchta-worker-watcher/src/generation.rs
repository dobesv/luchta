use std::collections::HashMap;
use std::process::ExitStatus;

use luchta_worker::{ProxyError, RawDelegate, SharedWriter, WorkerMessage, WorkerResponse};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InFlightKind {
    Run,
    Resolve,
}

pub struct Generation {
    id: u64,
    command: Vec<String>,
    delegate: RawDelegate,
    in_flight: HashMap<String, InFlightKind>,
}

impl Generation {
    pub fn new(
        id: u64,
        command: Vec<String>,
        stderr_writer: SharedWriter,
    ) -> Result<(Self, mpsc::Receiver<String>), ProxyError> {
        let mut delegate = RawDelegate::spawn_with_stderr(command.clone(), stderr_writer)?;
        let stdout = delegate
            .take_stdout()
            .ok_or(ProxyError::MissingPipe("stdout"))?;
        Ok((
            Self {
                id,
                command,
                delegate,
                in_flight: HashMap::new(),
            },
            stdout,
        ))
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn command(&self) -> &[String] {
        &self.command
    }

    pub async fn exit_status(&self) -> Option<ExitStatus> {
        self.delegate.exit_status().await
    }

    pub fn send(&mut self, msg: &WorkerMessage) -> Result<(), ProxyError> {
        let line = serde_json::to_string(msg)?;
        self.delegate.send_line(line)?;
        self.in_flight
            .insert(msg.id().to_owned(), in_flight_kind(msg));
        Ok(())
    }

    /// Records a response against this generation's in-flight set, dropping the id
    /// once its terminal (`done`/`resolved`) arrives.
    pub fn on_response(&mut self, resp: &WorkerResponse) {
        if is_terminal(resp) {
            self.in_flight.remove(resp.id());
        }
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

    /// Closes the delegate's stdin without terminating it. Used by tests to
    /// exercise the send-failure path.
    pub async fn close_stdin(&self) {
        self.delegate.close_stdin().await;
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

    use luchta_worker::{LogStream, ResolveMode, ResolveTask, TaskProgress, WorkerRequest};

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
        generation.on_response(&WorkerResponse::done("a", 0));
        assert_eq!(generation.in_flight_len(), 0);

        generation.shutdown().await.expect("shutdown succeeds");
    }

    #[tokio::test]
    async fn non_terminal_response_keeps_in_flight_id() {
        let mut generation = cat_generation(2).await;
        generation.send(&run_message("a")).expect("send succeeds");

        generation.on_response(&WorkerResponse::log("a", LogStream::Stdout, "line"));
        assert_eq!(generation.in_flight_len(), 1);

        generation.shutdown().await.expect("shutdown succeeds");
    }

    #[tokio::test]
    async fn progress_response_keeps_in_flight_id() {
        let mut generation = cat_generation(20).await;
        generation.send(&run_message("a")).expect("send succeeds");

        generation.on_response(&WorkerResponse::progress(
            "a",
            TaskProgress {
                completed: 1,
                pending: 2,
                ..TaskProgress::default()
            },
        ));
        assert_eq!(generation.in_flight_len(), 1);

        generation.shutdown().await.expect("shutdown succeeds");
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
    async fn generation_exposes_command_and_exit_status() {
        let (generation, mut stdout) = Generation::new(
            8,
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "printf 'ready\n'; exit 9".to_owned(),
            ],
            test_stderr_writer(),
        )
        .expect("spawn generation");
        assert_eq!(
            generation.command(),
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf 'ready\n'; exit 9".to_owned()
            ]
        );

        let line = tokio::time::timeout(Duration::from_secs(2), stdout.recv())
            .await
            .expect("recv should complete")
            .expect("stdout line");
        assert_eq!(line, "ready");
        let closed = tokio::time::timeout(Duration::from_secs(2), stdout.recv())
            .await
            .expect("recv should complete after exit");
        assert!(closed.is_none());

        tokio::time::sleep(Duration::from_millis(100)).await;
        let status = generation
            .exit_status()
            .await
            .expect("exit status should be captured");
        assert_eq!(status.code(), Some(9));

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
