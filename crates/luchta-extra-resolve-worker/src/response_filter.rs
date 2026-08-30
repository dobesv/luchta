use std::collections::HashSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use luchta_worker::WorkerResponse;
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
    use luchta_worker::{LogStream, ResolveResult};
    use tokio::io::AsyncWriteExt;

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
}
