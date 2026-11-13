use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use luchta_worker::sarif::{SarifFinding, SarifLevel};
use luchta_worker::{LogStream, WorkerResponse};
use tokio::io::{AsyncBufReadExt, AsyncWrite, BufReader};
use tokio::sync::Mutex;

use super::*;

fn finding() -> SarifFinding {
    SarifFinding {
        rule_id: Some("TS2322".to_owned()),
        level: SarifLevel::Error,
        message: "type mismatch".to_owned(),
        uri: Some("packages/pkg/src/index.ts".to_owned()),
        start_line: 1,
        start_column: 1,
        end_line: None,
        end_column: None,
    }
}

async fn finish(result: CompileResult) -> (InProcessOutcome, Vec<WorkerResponse>) {
    let (writer, reader) = tokio::io::duplex(16 * 1024);
    let ctx = JobContext::new("job".to_owned(), Arc::new(Mutex::new(Box::new(writer))));
    let outcome = finish_compile(result, &ctx).await;
    drop(ctx);
    let mut lines = BufReader::new(reader).lines();
    let mut responses = Vec::new();
    while let Some(line) = lines.next_line().await.unwrap() {
        responses.push(serde_json::from_str(&line).unwrap());
    }
    (outcome, responses)
}

#[tokio::test]
async fn finish_compile_logs_internal_errors_before_report_and_keeps_outputs() {
    let findings = vec![finding()];
    let report = sarif::build_sarif(&findings).unwrap();
    let (outcome, responses) = finish(CompileResult {
        exit_code: 1,
        outputs: vec!["dist/index.js".to_owned()],
        findings,
        stderr: vec!["internal failure".to_owned()],
    })
    .await;
    assert_eq!(
        responses,
        [
            WorkerResponse::log("job", LogStream::Stderr, "internal failure"),
            WorkerResponse::report("job", sarif::REPORT_FILENAME, sarif::MIME_TYPE, report),
        ]
    );
    assert_eq!(
        outcome,
        InProcessOutcome::Done {
            exit_code: 1,
            outputs: Some(vec!["dist/index.js".to_owned()]),
        }
    );
}

#[tokio::test]
async fn finish_compile_internal_error_has_no_report_or_outputs() {
    let (outcome, responses) = finish(CompileResult {
        exit_code: 1,
        stderr: vec!["clean outputs failed: outside package".to_owned()],
        ..CompileResult::default()
    })
    .await;
    assert_eq!(
        responses,
        [WorkerResponse::log(
            "job",
            LogStream::Stderr,
            "clean outputs failed: outside package"
        )]
    );
    assert_eq!(
        outcome,
        InProcessOutcome::Done {
            exit_code: 1,
            outputs: None,
        }
    );
}

#[derive(Default)]
struct FailingWriter {
    writes: Arc<AtomicUsize>,
}

impl AsyncWrite for FailingWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let message = if self.writes.fetch_add(1, Ordering::SeqCst) == 0 {
            "first write failed"
        } else {
            "second write failed"
        };
        Poll::Ready(Err(std::io::Error::other(message)))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn finish_compile_report_write_failure_returns_done_one_with_emitted_outputs() {
    let ctx = JobContext::new(
        "job".to_owned(),
        Arc::new(Mutex::new(Box::new(FailingWriter::default()))),
    );
    let outcome = finish_compile(
        CompileResult {
            // Verify transport failure itself turns success into failure.
            exit_code: 0,
            outputs: vec!["dist/index.js".to_owned()],
            findings: vec![finding()],
            stderr: Vec::new(),
        },
        &ctx,
    )
    .await;
    assert_eq!(
        outcome,
        InProcessOutcome::Done {
            exit_code: 1,
            outputs: Some(vec!["dist/index.js".to_owned()]),
        }
    );
}

// Go protocol_test: TestWriterRecordsFirstErrorAndKeepsEmitting. JobContext
// returns errors per call instead of storing Go Writer.Err's sticky error.
#[tokio::test]
async fn protocol_writer_returns_errors_and_keeps_emitting() {
    let writes = Arc::new(AtomicUsize::new(0));
    let ctx = JobContext::new(
        "job".to_owned(),
        Arc::new(Mutex::new(Box::new(FailingWriter {
            writes: writes.clone(),
        }))),
    );
    let first = ctx.emit_stdout("hello").await.unwrap_err();
    let second = ctx
        .emit_report("tsc.sarif", sarif::MIME_TYPE, "{}")
        .await
        .unwrap_err();
    assert!(first.to_string().contains("first write failed"));
    assert!(second.to_string().contains("second write failed"));
    assert_eq!(writes.load(Ordering::SeqCst), 2);
}
