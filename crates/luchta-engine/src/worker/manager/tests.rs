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
use crate::WorkerRequest;
use luchta_worker::WorkerDonePayload;

#[tokio::test]
async fn single_job_happy_path() {
    let temp = TempDir::new().expect("tempdir");
    let worker_path = echo_then_done_worker(temp.path(), "happy-worker.sh", Some("hello"), 7);
    let manager = manager_with_worker("fake", &worker_path);

    let outcome = run_one_job(&manager).await.expect("job succeeds");

    assert_eq!(outcome.0, 7);
    assert_eq!(outcome.1, None);
    assert_eq!(outcome.2, None);
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
    let manager = manager_with_worker("fake", &worker_path);

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
    let manager = Arc::new(manager_with_worker("fake", &worker_path));
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
    let manager = Arc::new(manager_with_worker("fake", &worker_path));
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
    let manager = manager_with_worker("fake", &worker_path);

    let error = manager
        .run_job("fake", WorkerRequest::new("pkg#task", "echo hi"), None)
        .await
        .expect_err("job should fail");

    assert!(matches!(
        error,
        WorkerError::Crashed { worker, id } if worker == "fake" && id == "pkg#task"
    ));
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
    let manager = manager_with_worker("fake", &worker_path);

    let error = manager
        .run_job("fake", WorkerRequest::new("pkg#task", "echo hi"), None)
        .await
        .expect_err("oversized line should crash worker job");

    assert!(matches!(
        error,
        WorkerError::Crashed { worker, id } if worker == "fake" && id == "pkg#task"
    ));
    manager.shutdown().await;
}

#[tokio::test]
async fn shutdown_is_idempotent() {
    let temp = TempDir::new().expect("tempdir");
    let worker_path = echo_then_done_worker(temp.path(), "idempotent-worker.sh", None, 0);
    let manager = manager_with_worker("fake", &worker_path);
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
    let manager = manager_with_worker_timeout("fake", &worker_path, Duration::from_millis(200));

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

fn manager_with_worker(name: &str, worker_path: &Path) -> WorkerManager {
    manager_with_worker_timeout(name, worker_path, Duration::from_secs(5))
}

fn manager_with_worker_timeout(name: &str, worker_path: &Path, timeout: Duration) -> WorkerManager {
    let mut definitions = HashMap::new();
    definitions.insert(
        name.to_owned(),
        WorkerDefinition {
            command: worker_path.display().to_string(),
        },
    );
    WorkerManager::with_shutdown_timeout(definitions, timeout)
}

/// Runs a single representative job against `manager` and returns its result.
async fn run_one_job(manager: &WorkerManager) -> Result<WorkerDonePayload, crate::WorkerError> {
    manager
        .run_job("fake", WorkerRequest::new("pkg#task", "echo hi"), None)
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
