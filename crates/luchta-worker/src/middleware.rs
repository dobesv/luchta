use std::future::Future;
use std::sync::Arc;

use thiserror::Error;
use tokio::io::{stdin, AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::task::{JoinError, JoinSet};

use crate::WorkerMessage;

/// Read middleware protocol messages continuously and dispatch every request as
/// an independent task. A middleware stage must not await one delegate terminal
/// response before reading the next request: doing so silently turns a
/// multiplexed worker into a serial one.
pub async fn run_concurrent_middleware<H, F>(handler: H) -> Result<(), MiddlewareError>
where
    H: Fn(WorkerMessage) -> F + Send + Sync + 'static,
    F: Future<Output = Result<(), String>> + Send + 'static,
{
    run_concurrent_middleware_from(BufReader::new(stdin()), handler).await
}

async fn run_concurrent_middleware_from<R, H, F>(
    reader: R,
    handler: H,
) -> Result<(), MiddlewareError>
where
    R: AsyncBufRead + Unpin,
    H: Fn(WorkerMessage) -> F + Send + Sync + 'static,
    F: Future<Output = Result<(), String>> + Send + 'static,
{
    let handler = Arc::new(handler);
    let mut lines = reader.lines();
    let mut jobs = JoinSet::new();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line.map_err(MiddlewareError::Read)? else {
                    break;
                };
                let message = serde_json::from_str(&line).map_err(MiddlewareError::Parse)?;
                let handler = Arc::clone(&handler);
                jobs.spawn(async move { handler(message).await });
            }
            result = jobs.join_next(), if !jobs.is_empty() => {
                check_job(result.expect("non-empty join set returned no task"))?;
            }
        }
    }

    while let Some(result) = jobs.join_next().await {
        check_job(result)?;
    }

    Ok(())
}

fn check_job(result: Result<Result<(), String>, JoinError>) -> Result<(), MiddlewareError> {
    result
        .map_err(MiddlewareError::Join)?
        .map_err(MiddlewareError::Request)
}

#[derive(Debug, Error)]
/// Failures encountered while reading or dispatching middleware requests.
pub enum MiddlewareError {
    /// Reading the worker's stdin stream failed.
    #[error("failed to read worker stdin: {0}")]
    Read(std::io::Error),
    /// A stdin JSONL record was not a valid worker message.
    #[error("failed to parse worker message: {0}")]
    Parse(serde_json::Error),
    /// A dispatched middleware request failed.
    #[error("{0}")]
    Request(String),
    /// A dispatched request task panicked or was cancelled.
    #[error("middleware request task failed: {0}")]
    Join(JoinError),
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::WorkerRequest;

    use super::*;

    fn input(messages: &[WorkerMessage]) -> Cursor<Vec<u8>> {
        let mut input = Vec::new();
        for message in messages {
            serde_json::to_writer(&mut input, message).expect("serialize message");
            input.push(b'\n');
        }
        Cursor::new(input)
    }

    #[tokio::test]
    async fn eof_after_requests_drains_every_handler() {
        let handled = Arc::new(AtomicUsize::new(0));
        let handled_by_task = Arc::clone(&handled);
        let messages = [
            WorkerMessage::Run(WorkerRequest::new("one", "true")),
            WorkerMessage::Run(WorkerRequest::new("two", "true")),
        ];

        run_concurrent_middleware_from(input(&messages), move |_| {
            let handled = Arc::clone(&handled_by_task);
            async move {
                handled.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .await
        .expect("dispatch requests");

        assert_eq!(handled.load(Ordering::SeqCst), messages.len());
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_error() {
        let error =
            run_concurrent_middleware_from(Cursor::new(b"not-json\n"), |_| async { Ok(()) })
                .await
                .expect_err("malformed input should fail");

        assert!(matches!(error, MiddlewareError::Parse(_)));
    }

    #[tokio::test]
    async fn handler_error_is_propagated_after_dispatch() {
        let message = WorkerMessage::Run(WorkerRequest::new("failed", "false"));
        let error = run_concurrent_middleware_from(input(&[message]), |_| async {
            Err("request failed".to_owned())
        })
        .await
        .expect_err("handler should fail");

        assert!(matches!(
            error,
            MiddlewareError::Request(message) if message == "request failed"
        ));
    }
}
