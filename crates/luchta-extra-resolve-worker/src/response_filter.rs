use std::collections::HashSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use luchta_worker::{SharedWriter, WorkerResponse};
use tokio::io::AsyncWrite;

#[derive(Clone, Default)]
pub(crate) struct ResponseFilter {
    suppressed_ids: Arc<Mutex<HashSet<String>>>,
}

impl ResponseFilter {
    pub(crate) fn suppress(&self, id: String) {
        self.lock_suppressed_ids().insert(id);
    }

    fn should_forward(&self, response: &WorkerResponse) -> bool {
        let mut suppressed_ids = self.lock_suppressed_ids();
        let should_forward = !suppressed_ids.contains(response.id());
        if is_terminal_response(response) {
            suppressed_ids.remove(response.id());
        }
        should_forward
    }

    fn lock_suppressed_ids(&self) -> MutexGuard<'_, HashSet<String>> {
        // Each critical section performs one HashSet operation, so recovering
        // from poison preserves a usable set and avoids a second panic while
        // the worker is already unwinding another request failure.
        self.suppressed_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Filters complete JSONL responses by correlation id before forwarding them.
///
/// The process proxy serializes each validated response as one JSONL record.
/// Buffering until the newline keeps filtering correct even when an underlying
/// writer accepts a record through multiple `poll_write` calls.
pub(crate) struct ResponseFilteringWriter<W> {
    inner: W,
    filter: ResponseFilter,
    input: Vec<u8>,
    output: Vec<u8>,
    output_position: usize,
}

impl<W> ResponseFilteringWriter<W> {
    pub(crate) fn new(inner: W, filter: ResponseFilter) -> Self {
        Self {
            inner,
            filter,
            input: Vec::new(),
            output: Vec::new(),
            output_position: 0,
        }
    }

    fn route_complete_lines(&mut self) {
        while let Some(newline) = self.input.iter().position(|byte| *byte == b'\n') {
            let line = self.input.drain(..=newline).collect::<Vec<_>>();
            let should_forward = serde_json::from_slice::<WorkerResponse>(&line[..newline])
                .map_or(true, |response| self.filter.should_forward(&response));
            if should_forward {
                self.output.extend_from_slice(&line);
            }
        }
    }

    fn move_incomplete_input_to_output(&mut self) {
        if !self.input.is_empty() {
            self.output.append(&mut self.input);
        }
    }
}

pub(crate) fn shared_response_writer<W>(inner: W, filter: ResponseFilter) -> SharedWriter
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    Arc::new(tokio::sync::Mutex::new(Box::new(
        ResponseFilteringWriter::new(inner, filter),
    )))
}

impl<W: AsyncWrite + Unpin> ResponseFilteringWriter<W> {
    fn poll_output(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        while self.output_position < self.output.len() {
            let position = self.output_position;
            let written = match Pin::new(&mut self.inner).poll_write(cx, &self.output[position..]) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()));
                }
                Poll::Ready(Ok(written)) => written,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            };
            self.output_position += written;
        }

        self.output.clear();
        self.output_position = 0;
        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for ResponseFilteringWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.poll_output(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        }

        self.input.extend_from_slice(buf);
        self.route_complete_lines();
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.move_incomplete_input_to_output();
        match self.poll_output(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut self.inner).poll_flush(cx),
            other => other,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.as_mut().poll_flush(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut self.inner).poll_shutdown(cx),
            other => other,
        }
    }
}

fn is_terminal_response(response: &WorkerResponse) -> bool {
    matches!(
        response,
        WorkerResponse::Resolved { .. } | WorkerResponse::Done { .. }
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use luchta_worker::{write_worker_response, LogStream, ResolveResult};
    use tokio::io::AsyncWriteExt;
    use tokio::task::JoinSet;

    use super::*;

    #[tokio::test]
    async fn suppresses_only_the_selected_request() {
        let filter = ResponseFilter::default();
        filter.suppress("internal-resolve".to_owned());
        let mut writer = ResponseFilteringWriter::new(Vec::new(), filter);
        let forwarded = WorkerResponse::done("forwarded-run", 0);
        let suppressed = WorkerResponse::resolved("internal-resolve", ResolveResult::accept());

        for response in [&forwarded, &suppressed] {
            writer
                .write_all(serde_json::to_string(response).unwrap().as_bytes())
                .await
                .unwrap();
            writer.write_all(b"\n").await.unwrap();
        }
        writer.flush().await.unwrap();

        assert_eq!(
            String::from_utf8(writer.inner).unwrap(),
            format!("{}\n", serde_json::to_string(&forwarded).unwrap())
        );
    }

    #[tokio::test]
    async fn suppression_survives_streaming_responses_and_ends_at_terminal_response() {
        let filter = ResponseFilter::default();
        filter.suppress("internal-resolve".to_owned());
        let mut writer = ResponseFilteringWriter::new(Vec::new(), filter);
        let streamed = WorkerResponse::log("internal-resolve", LogStream::Stdout, "working");
        let terminal = WorkerResponse::resolved("internal-resolve", ResolveResult::accept());
        let after_terminal = WorkerResponse::done("internal-resolve", 0);

        for response in [&streamed, &terminal] {
            let line = serde_json::to_string(response).unwrap();
            for chunk in line.as_bytes().chunks(3) {
                writer.write_all(chunk).await.unwrap();
            }
            writer.write_all(b"\n").await.unwrap();
        }
        writer
            .write_all(serde_json::to_string(&after_terminal).unwrap().as_bytes())
            .await
            .unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.flush().await.unwrap();

        assert_eq!(
            String::from_utf8(writer.inner).unwrap(),
            format!("{}\n", serde_json::to_string(&after_terminal).unwrap())
        );
    }

    struct YieldingBuffer {
        bytes: Arc<Mutex<Vec<u8>>>,
        yield_before_write: bool,
    }

    impl YieldingBuffer {
        fn new(bytes: Arc<Mutex<Vec<u8>>>) -> Self {
            Self {
                bytes,
                yield_before_write: true,
            }
        }
    }

    impl AsyncWrite for YieldingBuffer {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.yield_before_write {
                self.yield_before_write = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }

            self.yield_before_write = true;
            let written = buf.len().min(17);
            let bytes = Arc::clone(&self.bytes);
            bytes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(&buf[..written]);
            Poll::Ready(Ok(written))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn concurrent_synthetic_and_forwarded_responses_are_valid_jsonl() {
        const RESPONSE_PAIRS: usize = 32;

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let filter = ResponseFilter::default();
        let writer =
            shared_response_writer(YieldingBuffer::new(Arc::clone(&bytes)), filter.clone());

        // The delegate's internal resolve response is filtered before the
        // middleware emits the synthetic response with the same id.
        for index in 0..RESPONSE_PAIRS {
            let id = format!("resolve-{index}");
            filter.suppress(id.clone());
            write_worker_response(
                &writer,
                &WorkerResponse::resolved(id, ResolveResult::accept()),
            )
            .await
            .unwrap();
        }

        let mut writes = JoinSet::new();
        for index in 0..RESPONSE_PAIRS {
            let synthetic_writer = Arc::clone(&writer);
            let synthetic = WorkerResponse::resolved(
                format!("resolve-{index}"),
                ResolveResult::reject("synthetic ".repeat(128)),
            );
            writes.spawn(async move {
                write_worker_response(&synthetic_writer, &synthetic)
                    .await
                    .unwrap();
            });

            let forwarded_writer = Arc::clone(&writer);
            let forwarded = WorkerResponse::log(
                format!("run-{index}"),
                LogStream::Stdout,
                "forwarded ".repeat(128),
            );
            writes.spawn(async move {
                write_worker_response(&forwarded_writer, &forwarded)
                    .await
                    .unwrap();
            });
        }

        while let Some(result) = writes.join_next().await {
            result.unwrap();
        }

        let output = bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let output = String::from_utf8(output).unwrap();
        let responses = output
            .lines()
            .map(|line| serde_json::from_str::<WorkerResponse>(line).expect("valid JSONL record"))
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), RESPONSE_PAIRS * 2);

        let response_ids = responses
            .iter()
            .map(|response| response.id())
            .collect::<HashSet<_>>();
        for index in 0..RESPONSE_PAIRS {
            assert!(response_ids.contains(format!("resolve-{index}").as_str()));
            assert!(response_ids.contains(format!("run-{index}").as_str()));
        }
    }
}
