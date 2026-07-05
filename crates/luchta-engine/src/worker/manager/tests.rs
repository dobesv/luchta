use std::{
    collections::HashMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use luchta_types::WorkerDefinition;
use tempfile::TempDir;
use tokio::{process::Command, sync::Barrier, time::Instant};

use super::{WorkerError, WorkerManager};
use crate::{task_graph::TaskResolver, WorkerRequest};
use luchta_worker::WorkerDonePayload;

#[derive(Clone, Copy)]
struct TestWorkerRef<'a> {
    name: &'a str,
}

impl<'a> TestWorkerRef<'a> {
    const fn new(name: &'a str) -> Self {
        Self { name }
    }
}

#[derive(Clone, Copy)]
struct TestJobRef<'a> {
    worker: &'a str,
    id: &'a str,
}

impl<'a> TestJobRef<'a> {
    const fn new(worker: &'a str, id: &'a str) -> Self {
        Self { worker, id }
    }
}

#[tokio::test]
async fn single_job_happy_path() {
    let temp = TempDir::new().expect("tempdir");
    let worker_path = echo_then_done_worker(temp.path(), "happy-worker.sh", Some("hello"), 7);
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);

    let outcome = run_one_job(&manager).await.expect("job succeeds");

    assert_eq!(outcome.0, 7);
    assert_eq!(outcome.1, None);
    assert!(outcome.2.is_empty());
    manager.shutdown().await;
}

#[tokio::test]
async fn sequential_jobs_reuse_single_process() {
    let temp = TempDir::new().expect("tempdir");
    let pid_file = temp.path().join("pid.txt");
    let count_file = temp.path().join("count.txt");
    let worker_path = write_worker_script(
        temp.path(),
        "reuse-worker.sh",
        &format!(
            r#"#!/bin/sh
if [ ! -f "{pid}" ]; then
  echo $$ > "{pid}"
fi
count=0
while IFS= read -r line; do
  count=$((count + 1))
  echo "$count" > "{count_file}"
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
done
"#,
            pid = pid_file.display(),
            count_file = count_file.display(),
        ),
    );
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);

    manager
        .run_job("fake", WorkerRequest::new("pkg#one", "echo one"), None)
        .await
        .expect("first job succeeds");
    manager
        .run_job("fake", WorkerRequest::new("pkg#two", "echo two"), None)
        .await
        .expect("second job succeeds");
    manager.shutdown().await;

    let pid = fs::read_to_string(&pid_file).expect("pid recorded");
    let count = fs::read_to_string(&count_file).expect("count recorded");
    assert!(!pid.trim().is_empty());
    assert_eq!(count.trim(), "2");
}

#[tokio::test]
async fn concurrent_jobs_interleave_without_crosstalk() {
    let temp = TempDir::new().expect("tempdir");
    let counter = Arc::new(AtomicU32::new(0));
    let gate_file = temp.path().join("gate.txt");
    let worker_path = write_worker_script(
        temp.path(),
        "concurrent-worker.sh",
        &format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  (
    printf '{{"type":"log","id":"%s","stream":"stdout","line":"start"}}\n' "$id"
    while [ ! -f "{gate}" ]; do
      sleep 0.01
    done
    printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
  ) &
done
wait
"#,
            gate = gate_file.display(),
        ),
    );
    let manager = Arc::new(manager_with_worker(
        TestWorkerRef::new("fake"),
        &worker_path,
    ));
    let barrier = Arc::new(Barrier::new(3));

    let spawn_job = |id: &'static str| {
        let manager = Arc::clone(&manager);
        let barrier = Arc::clone(&barrier);
        let counter = Arc::clone(&counter);
        tokio::spawn(async move {
            barrier.wait().await;
            let outcome = manager
                .run_job("fake", WorkerRequest::new(id, "echo hi"), None)
                .await
                .expect("job succeeds");
            counter.fetch_add(1, Ordering::SeqCst);
            outcome.0
        })
    };

    let first = spawn_job("pkg#one");
    let second = spawn_job("pkg#two");
    barrier.wait().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    fs::write(&gate_file, "go").expect("gate written");

    assert_eq!(first.await.expect("first joined"), 0);
    assert_eq!(second.await.expect("second joined"), 0);
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    Arc::try_unwrap(manager)
        .expect("manager only ref")
        .shutdown()
        .await;
}

/// Regression test for issue #64: a worker that emits far more log lines than
/// the per-job response channel capacity (64) must not lose any of them. The
/// shared stdout reader applies back-pressure (`send().await`) instead of
/// dropping responses when the channel is full, so every emitted line is
/// delivered to the consumer and captured by the sink.
#[tokio::test]
async fn high_log_volume_does_not_drop_lines() {
    const LINE_COUNT: usize = 1000;

    let temp = TempDir::new().expect("tempdir");
    // For each request, emit LINE_COUNT log lines (each tagged with its
    // sequence number) and then report done. LINE_COUNT greatly exceeds the
    // 64-slot per-job channel, so the old `try_send` path would have dropped
    // lines once the consumer fell behind.
    let worker_path = write_worker_script(
        temp.path(),
        "high-volume-worker.sh",
        &format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  i=1
  while [ "$i" -le {count} ]; do
    printf '{{"type":"log","id":"%s","stream":"stdout","line":"line-%s"}}\n' "$id" "$i"
    i=$((i + 1))
  done
  printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
done
"#,
            count = LINE_COUNT,
        ),
    );
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);
    let sink = crate::ExecutionLogSink::new();

    let outcome = manager
        .run_job(
            "fake",
            WorkerRequest::new("pkg#task", "echo hi"),
            Some(&sink),
        )
        .await
        .expect("job succeeds");
    assert_eq!(outcome.0, 0);

    let lines = sink.lines();
    assert_eq!(
        lines.len(),
        LINE_COUNT,
        "all worker log lines must be captured, none dropped"
    );
    // Every line must be present exactly once, in order, with no gaps.
    for (index, captured) in lines.iter().enumerate() {
        assert_eq!(captured.line, format!("line-{}", index + 1));
    }

    manager.shutdown().await;
}

#[tokio::test]
async fn concurrent_first_calls_spawn_single_worker_process() {
    let temp = TempDir::new().expect("tempdir");
    let pid_file = temp.path().join("pids.txt");
    let gate_file = temp.path().join("gate.txt");
    let worker_path = write_worker_script(
        temp.path(),
        "single-spawn-worker.sh",
        &format!(
            r#"#!/bin/sh
echo $$ >> "{pid_file}"
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  while [ ! -f "{gate_file}" ]; do
    sleep 0.01
  done
  printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
done
"#,
            pid_file = pid_file.display(),
            gate_file = gate_file.display(),
        ),
    );
    let manager = Arc::new(manager_with_worker(
        TestWorkerRef::new("fake"),
        &worker_path,
    ));
    let barrier = Arc::new(Barrier::new(9));

    let handles = (0..8)
        .map(|index| {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                manager
                    .run_job(
                        "fake",
                        WorkerRequest::new(format!("pkg#job-{index}"), "echo hi"),
                        None,
                    )
                    .await
                    .expect("job succeeds")
                    .0
            })
        })
        .collect::<Vec<_>>();

    barrier.wait().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    fs::write(&gate_file, "go").expect("gate written");

    for handle in handles {
        assert_eq!(handle.await.expect("job joined"), 0);
    }

    let pid_lines = fs::read_to_string(&pid_file)
        .expect("pid file written")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    assert_eq!(pid_lines, 1, "expected exactly one worker process");

    Arc::try_unwrap(manager)
        .expect("manager only ref")
        .shutdown()
        .await;
}

#[tokio::test]
async fn report_response_is_collected_and_run_completes_normally() {
    let temp = TempDir::new().expect("tempdir");
    let worker_path = write_worker_script(
        temp.path(),
        "report-worker.sh",
        r##"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{"type":"report","id":"%s","filename":"summary.md","mimeType":"text/markdown","content":"# hi"}\n' "$id"
  printf '{"type":"done","id":"%s","exitCode":0}\n' "$id"
done
"##,
    );
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);
    let sink = crate::ExecutionLogSink::new();

    let outcome = manager
        .run_job(
            "fake",
            WorkerRequest::new("pkg#task", "echo hi"),
            Some(&sink),
        )
        .await
        .expect("job succeeds");

    assert_eq!(outcome.0, 0);
    assert_eq!(
        sink.reports(),
        vec![crate::CollectedReport {
            filename: "summary.md".to_owned(),
            mime_type: "text/markdown".to_owned(),
            content: "# hi".to_owned(),
        }]
    );

    manager.shutdown().await;
}

#[tokio::test]
async fn invalid_report_filename_is_dropped_and_run_continues() {
    let temp = TempDir::new().expect("tempdir");
    let worker_path = write_worker_script(
        temp.path(),
        "invalid-report-worker.sh",
        r##"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{"type":"report","id":"%s","filename":"../escape.txt","mimeType":"text/plain","content":"bad"}\n' "$id"
  printf '{"type":"done","id":"%s","exitCode":0}\n' "$id"
done
"##,
    );
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);
    let sink = crate::ExecutionLogSink::new();

    let outcome = manager
        .run_job(
            "fake",
            WorkerRequest::new("pkg#task", "echo hi"),
            Some(&sink),
        )
        .await
        .expect("job succeeds");

    assert_eq!(outcome.0, 0);
    assert!(sink.reports().is_empty());

    manager.shutdown().await;
}

#[tokio::test]
async fn duplicate_report_filename_uses_last_wins() {
    let temp = TempDir::new().expect("tempdir");
    let worker_path = write_worker_script(
        temp.path(),
        "duplicate-report-worker.sh",
        r##"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{"type":"report","id":"%s","filename":"summary.md","mimeType":"text/plain","content":"first"}\n' "$id"
  printf '{"type":"report","id":"%s","filename":"summary.md","mimeType":"text/plain","content":"second"}\n' "$id"
  printf '{"type":"done","id":"%s","exitCode":0}\n' "$id"
done
"##,
    );
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);
    let sink = crate::ExecutionLogSink::new();

    let outcome = manager
        .run_job(
            "fake",
            WorkerRequest::new("pkg#task", "echo hi"),
            Some(&sink),
        )
        .await
        .expect("job succeeds");

    assert_eq!(outcome.0, 0);
    assert_eq!(
        sink.reports(),
        vec![crate::CollectedReport {
            filename: "summary.md".to_owned(),
            mime_type: "text/plain".to_owned(),
            content: "second".to_owned(),
        }]
    );

    manager.shutdown().await;
}

#[tokio::test]
async fn unexpected_done_response_to_resolve_returns_protocol_error_with_worker_and_id() {
    let temp = TempDir::new().expect("tempdir");
    let worker_path = write_worker_script(
        temp.path(),
        "unexpected-done-worker.sh",
        r#"#!/bin/sh
read -r line
id=$(printf '%s
' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"type":"done","id":"%s","exitCode":0}
' "$id"
while IFS= read -r _; do :; done
"#,
    );
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);

    let error = manager
        .resolve(
            "fake",
            luchta_worker::ResolveTask {
                id: "pkg#protocol".to_string(),
                name: "build".to_string(),
                command: String::new(),
                package: "pkg".to_string(),
                cwd: None,
                scripts: Vec::new(),
                inputs: Vec::new(),
                mode: luchta_worker::ResolveMode::Run,
            },
        )
        .await
        .expect_err("unexpected done response should fail protocol check");

    assert!(
        error.contains(
            "worker 'fake' protocol error for job 'pkg#protocol': unexpected 'done' response"
        ),
        "unexpected protocol display: {error}"
    );

    manager.shutdown().await;
}

#[tokio::test]
async fn worker_exit_without_done_returns_crashed() {
    let temp = TempDir::new().expect("tempdir");
    let worker_path = write_worker_script(
        temp.path(),
        "crash-worker.sh",
        r#"#!/bin/sh
read -r _
exit 0
"#,
    );
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);

    let error = manager
        .run_job("fake", WorkerRequest::new("pkg#task", "echo hi"), None)
        .await
        .expect_err("job should fail");

    assert_crashed_job(&error, TestJobRef::new("fake", "pkg#task"));
    manager.shutdown().await;
}

#[tokio::test]
async fn oversized_worker_stdout_line_returns_crashed() {
    let temp = TempDir::new().expect("tempdir");
    let worker_path = write_worker_script(
        temp.path(),
        "oversized-line-worker.sh",
        r#"#!/bin/sh
read -r line
id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
python3 - "$id" <<'PY'
import sys
job_id = sys.argv[1]
print('{"type":"log","id":"' + job_id + '","stream":"stdout","line":"' + ('x' * 1048600) + '"}')
PY
"#,
    );
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);

    let error = manager
        .run_job("fake", WorkerRequest::new("pkg#task", "echo hi"), None)
        .await
        .expect_err("oversized line should crash worker job");

    assert_crashed_job(&error, TestJobRef::new("fake", "pkg#task"));
    manager.shutdown().await;
}

#[tokio::test]
async fn post_crash_job_returns_within_timeout() {
    let temp = TempDir::new().expect("tempdir");
    let crash_count_file = temp.path().join("crash-count.txt");
    let worker_path = write_worker_script(
        temp.path(),
        "post-crash-timeout.sh",
        &format!(
            r#"#!/bin/sh
count_file="{count_file}"
count=0
if [ -f "$count_file" ]; then
  count=$(cat "$count_file")
fi
count=$((count + 1))
echo "$count" > "$count_file"
if [ "$count" -eq 1 ]; then
  read -r _
  echo "boom from first worker" >&2
  exit 17
fi
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
done
"#,
            count_file = crash_count_file.display(),
        ),
    );
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);

    let first_error = manager
        .run_job("fake", WorkerRequest::new("pkg#crash", "echo hi"), None)
        .await
        .expect_err("first job crashes worker");
    assert_crash_detail_contains(
        &first_error,
        &[
            "exited with code 17",
            "--- worker 'fake' stderr (last 1 lines) ---",
            "boom from first worker",
            "--- end worker 'fake' stderr ---",
        ],
    );

    let second = tokio::time::timeout(
        Duration::from_secs(2),
        manager.run_job("fake", WorkerRequest::new("pkg#after", "echo hi"), None),
    )
    .await
    .expect("post-crash job must not hang")
    .expect("dead worker handle should be evicted and respawned");
    assert_eq!(second.0, 0);

    manager.shutdown().await;
}

#[tokio::test]
async fn crashed_worker_is_evicted_and_respawned() {
    let temp = TempDir::new().expect("tempdir");
    let spawn_count_file = temp.path().join("spawn-count.txt");
    let pid_file = temp.path().join("pids.txt");
    let worker_path = write_worker_script(
        temp.path(),
        "respawn-worker.sh",
        &format!(
            r#"#!/bin/sh
count_file="{count_file}"
pid_file="{pid_file}"
count=0
if [ -f "$count_file" ]; then
  count=$(cat "$count_file")
fi
count=$((count + 1))
echo "$count" > "$count_file"
echo $$ >> "$pid_file"
if [ "$count" -eq 1 ]; then
  read -r _
  echo "first instance crashed" >&2
  exit 23
fi
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
done
"#,
            count_file = spawn_count_file.display(),
            pid_file = pid_file.display(),
        ),
    );
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);

    let error = manager
        .run_job("fake", WorkerRequest::new("pkg#crash", "echo hi"), None)
        .await
        .expect_err("first worker should crash");
    assert_crash_detail_contains(
        &error,
        &[
            "worker 'fake'",
            "pkg#crash",
            "exited with code 23",
            "--- worker 'fake' stderr (last 1 lines) ---",
            "first instance crashed",
            "--- end worker 'fake' stderr ---",
        ],
    );

    let outcome = manager
        .run_job("fake", WorkerRequest::new("pkg#ok", "echo hi"), None)
        .await
        .expect("second worker should succeed");
    assert_eq!(outcome.0, 0);

    let spawn_count = fs::read_to_string(&spawn_count_file).expect("spawn count recorded");
    assert_eq!(spawn_count.trim(), "2");

    let pid_contents = fs::read_to_string(&pid_file).expect("pid file written");
    let pid_lines = pid_contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(pid_lines.len(), 2, "expected crash to force respawn");
    assert_ne!(pid_lines[0], pid_lines[1], "expected different worker pids");

    manager.shutdown().await;
}

#[tokio::test]
async fn crash_error_includes_exit_status_and_stderr_detail() {
    let temp = TempDir::new().expect("tempdir");
    let worker_path = write_worker_script(
        temp.path(),
        "crash-detail-worker.sh",
        r#"#!/bin/sh
read -r _
echo "worker error: io error: Resource temporarily unavailable (os error 11)" >&2
exit 19
"#,
    );
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);

    let error = manager
        .run_job("fake", WorkerRequest::new("pkg#task", "echo hi"), None)
        .await
        .expect_err("job should fail with crash detail");

    assert_crash_detail_contains(
        &error,
        &[
            "exited with code 19",
            "--- worker 'fake' stderr (last 1 lines) ---",
            "worker error: io error: Resource temporarily unavailable (os error 11)",
            "--- end worker 'fake' stderr ---",
        ],
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn wait_error_is_recorded_in_crash_info() {
    let mut crash_state = super::super::handle::WorkerCrashState::default();
    crash_state.set_wait_error(std::io::Error::other("wait blew up"));

    let crash_info = crash_state.crash_info("fake").expect("crash info present");
    assert!(crash_info.detail.contains("wait error: wait blew up"));
}

#[tokio::test]
async fn try_reuse_worker_keeps_respawned_successor_registered() {
    let temp = TempDir::new().expect("tempdir");
    let worker_path = echo_then_done_worker(temp.path(), "successor-worker.sh", None, 0);
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);

    let stale = manager
        .spawn_worker("fake", &manager.definitions["fake"])
        .await
        .expect("spawn stale handle");
    let successor = manager
        .spawn_worker("fake", &manager.definitions["fake"])
        .await
        .expect("spawn successor handle");

    {
        let mut workers = manager.workers.lock().await;
        workers.insert("fake".to_owned(), Arc::clone(&successor));
    }

    // Drive the PRODUCTION eviction helper with the now-stale handle: because the
    // registry entry is the successor (a different Arc instance), the instance
    // guard must leave it in place.
    manager.evict_if_current("fake", &stale).await;

    let cached = {
        let workers = manager.workers.lock().await;
        workers
            .get("fake")
            .cloned()
            .expect("successor remains registered")
    };
    assert!(
        Arc::ptr_eq(&cached, &successor),
        "evict_if_current must not remove a respawned successor"
    );

    // Now evict the handle that IS the current registry entry: the guard matches,
    // so it is removed. This proves the helper isn't a no-op.
    manager.evict_if_current("fake", &successor).await;
    assert!(
        manager.workers.lock().await.get("fake").is_none(),
        "evict_if_current must remove the currently-registered handle"
    );

    let _ = &stale;

    manager.shutdown().await;
    stale.kill_now();
}

#[tokio::test]
async fn shutdown_is_idempotent() {
    let temp = TempDir::new().expect("tempdir");
    let worker_path = echo_then_done_worker(temp.path(), "idempotent-worker.sh", None, 0);
    let manager = manager_with_worker(TestWorkerRef::new("fake"), &worker_path);
    run_one_job(&manager).await.expect("job succeeds");

    // Calling shutdown twice must be safe and must not hang or panic.
    for _ in 0..2 {
        manager.shutdown().await;
    }
}

#[tokio::test]
async fn shutdown_kills_sleep_forever_worker_within_timeout() {
    let temp = TempDir::new().expect("tempdir");
    let pid_file = temp.path().join("pid.txt");
    let worker_path = write_worker_script(
        temp.path(),
        "sleep-forever.sh",
        &format!(
            r#"#!/bin/sh
echo $$ > "{pid}"
trap '' TERM INT
while IFS= read -r _; do
  while :; do
    sleep 60
  done
 done
"#,
            pid = pid_file.display(),
        ),
    );
    let manager = manager_with_worker_timeout(
        TestWorkerRef::new("fake"),
        &worker_path,
        Duration::from_millis(200),
    );

    let _ = manager.get_or_spawn("fake").await.expect("spawn worker");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let start = Instant::now();
    manager.shutdown().await;
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown took {elapsed:?}"
    );

    let pid = fs::read_to_string(&pid_file).expect("pid recorded");
    assert!(!process_exists(pid.trim().parse().expect("pid parse")).await);
}

fn manager_with_worker(worker: TestWorkerRef, worker_path: &Path) -> WorkerManager {
    manager_with_worker_timeout(worker, worker_path, Duration::from_secs(5))
}

fn manager_with_worker_timeout(
    worker: TestWorkerRef,
    worker_path: &Path,
    timeout: Duration,
) -> WorkerManager {
    let mut definitions = HashMap::new();
    definitions.insert(
        worker.name.to_owned(),
        WorkerDefinition {
            command: worker_path.display().to_string(),
            depends_on: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cache: None,
        },
    );
    WorkerManager::with_shutdown_timeout(definitions, timeout)
}

/// Runs a single representative job against `manager` and returns its result.
async fn run_one_job(manager: &WorkerManager) -> Result<WorkerDonePayload, crate::WorkerError> {
    let job = TestJobRef::new("fake", "pkg#task");
    manager
        .run_job(job.worker, WorkerRequest::new(job.id, "echo hi"), None)
        .await
}

/// Writes a worker script that, for each request, optionally logs a line and
/// then reports `done` with `exit_code`. Shared by the happy-path and shutdown
/// idempotency tests to avoid duplicated script bodies.
fn echo_then_done_worker(
    dir: &Path,
    name: &str,
    log_line: Option<&str>,
    exit_code: i32,
) -> PathBuf {
    let log = match log_line {
        Some(line) => format!(
            "  printf '{{\"type\":\"log\",\"id\":\"%s\",\"stream\":\"stdout\",\"line\":\"{line}\"}}\\n' \"$id\"\n"
        ),
        None => String::new(),
    };
    let body = format!(
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
{log}  printf '{{"type":"done","id":"%s","exitCode":{exit_code}}}\n' "$id"
done
"#
    );
    write_worker_script(dir, name, &body)
}

fn write_worker_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).expect("worker script written");
    let mut permissions = fs::metadata(&path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("chmod");
    path
}

fn assert_crashed_job(error: &WorkerError, job: TestJobRef) {
    assert!(matches!(
        error,
        WorkerError::Crashed {
            worker,
            id,
            detail: _,
            detail_suffix: _,
        } if worker == job.worker && id == job.id
    ));
}

fn assert_crash_detail_contains(error: &WorkerError, expected_parts: &[&str]) {
    let WorkerError::Crashed { detail, .. } = error else {
        panic!("expected crashed worker error, got {error:?}");
    };
    let rendered = error.to_string();
    detail.as_deref().expect("crash detail present");
    for part in expected_parts {
        assert!(
            rendered.contains(part),
            "expected crashed display '{rendered}' to contain '{part}'"
        );
    }
}

async fn process_exists(pid: i32) -> bool {
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 {pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    matches!(status, Ok(status) if status.success())
}
