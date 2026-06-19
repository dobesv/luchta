// Linux-only: the reaping assertions read `/proc/<pid>/stat`. Gating on
// `target_os = "linux"` (not all of `unix`) keeps these tests honest on
// macOS/BSD where `/proc` is unavailable.
#![cfg(target_os = "linux")]

//! Worker integration tests for `luchta run` command.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use assert_fs::prelude::*;
use predicates::prelude::*;

/// A workspace package: name, build script, and whether it depends on `a`.
struct Pkg {
    name: &'static str,
    script: &'static str,
    depends_on_a: bool,
}

/// Writes a root `package.json` plus the given workspace packages.
fn write_workspace(temp: &assert_fs::TempDir, packages: &[Pkg]) {
    temp.child("package.json")
        .write_str(r#"{ "name": "root", "private": true, "workspaces": ["packages/*"] }"#)
        .expect("write root package.json");

    for pkg in packages {
        let deps = if pkg.depends_on_a {
            r#", "dependencies": { "a": "workspace:*" }"#
        } else {
            ""
        };
        let dir = temp.child(format!("packages/{}", pkg.name));
        fs::create_dir_all(dir.path()).expect("create package dir");
        temp.child(format!("packages/{}/package.json", pkg.name))
            .write_str(&format!(
                r#"{{ "name": "{}", "scripts": {{ "build": "{}" }}{} }}"#,
                pkg.name, pkg.script, deps
            ))
            .expect("write package.json");
    }
}

/// Two packages `a` and `b`; `b` depends on `a` when `b_depends_on_a` is set.
fn setup_two_packages(temp: &assert_fs::TempDir, b_depends_on_a: bool) {
    write_workspace(
        temp,
        &[
            Pkg {
                name: "a",
                script: "echo built-a",
                depends_on_a: false,
            },
            Pkg {
                name: "b",
                script: "echo built-b",
                depends_on_a: b_depends_on_a,
            },
        ],
    );
}

/// Writes `contents` to `name` under `temp` and marks it executable.
fn write_executable(temp: &assert_fs::TempDir, name: &str, contents: &str) -> std::path::PathBuf {
    let file = temp.child(name);
    file.write_str(contents).expect("write executable file");
    let mut perms = fs::metadata(file.path())
        .expect("file metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(file.path(), perms).expect("chmod file");
    file.path().to_path_buf()
}

/// Writes an executable `luchta-config.sh` emitting the given JSON config body.
fn write_config(temp: &assert_fs::TempDir, json: &str) {
    write_executable(
        temp,
        "luchta-config.sh",
        &format!("#!/bin/sh\necho '{json}'\n"),
    );
}

/// Builds a worker binary by name and returns its absolute path.
fn build_worker_bin(name: &str) -> std::path::PathBuf {
    escargot::CargoBuild::new()
        .bin(name)
        .package(name)
        .run()
        .unwrap_or_else(|e| panic!("build {name}: {e}"))
        .path()
        .to_path_buf()
}

/// Absolute path to built `luchta-yarn-worker` binary.
fn yarn_worker_bin() -> std::path::PathBuf {
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(|| build_worker_bin("luchta-yarn-worker"))
        .clone()
}

/// Absolute path to built `luchta-bash-worker` binary.
fn bash_worker_bin() -> std::path::PathBuf {
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(|| build_worker_bin("luchta-bash-worker"))
        .clone()
}

/// Empty marker file the worker scripts append to.
fn init_marker(temp: &assert_fs::TempDir, name: &str) -> std::path::PathBuf {
    let marker = temp.child(name);
    marker.write_str("").expect("initialize marker");
    marker.path().to_path_buf()
}

fn write_fake_yarn(temp: &assert_fs::TempDir) -> std::path::PathBuf {
    let bin_dir = temp.child("bin");
    fs::create_dir_all(bin_dir.path()).expect("create fake yarn bin dir");
    write_executable(
        temp,
        "bin/yarn",
        r#"#!/bin/sh
if [ "$1" = "workspace" ]; then
  ws="$2"
  script="$3"
  shift 3
  echo "yarn-ran workspace=$ws script=$script args=$*"
else
  script="$1"
  shift
  echo "yarn-ran root script=$script args=$*"
fi
"#,
    );
    bin_dir.path().to_path_buf()
}

fn path_with_prepend(bin_dir: &Path) -> String {
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// A resident worker that records its PID/PGID, serves jobs, signals `ready`,
/// then `exec sleep 60` so it must be SIGKILLed at shutdown.
/// Writes a fake worker that records each *run* request line to `capture_path`
/// and replies `done`. During the resolution phase it accepts every task (so it
/// enters the graph) without recording the resolve request.
fn write_capture_worker(
    temp: &assert_fs::TempDir,
    file_name: &str,
    capture_path: &Path,
) -> std::path::PathBuf {
    write_executable(
        temp,
        file_name,
        &format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
      continue
      ;;
  esac
  printf '%s\n' "$line" >> "{capture}"
  printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
done
"#,
            capture = capture_path.display()
        ),
    )
}

/// Markers a recording worker writes so the test can locate its process group.
struct RecordingMarkers<'a> {
    pid: &'a Path,
    pgid: &'a Path,
}

/// Defines a recording worker script: its file name, the per-request shell body
/// (already indented), and a trailer run once the request loop ends.
struct RecordingWorkerSpec<'a> {
    file_name: &'a str,
    per_request_body: &'a str,
    trailer: &'a str,
}

/// Writes a worker script that records its pid/pgid, then for each request runs
/// `spec.per_request_body`, and finally `spec.trailer` once the request loop
/// ends. Shared scaffold for the recording worker variants.
fn write_recording_worker(
    temp: &assert_fs::TempDir,
    markers: RecordingMarkers<'_>,
    spec: RecordingWorkerSpec<'_>,
) -> std::path::PathBuf {
    write_executable(
        temp,
        spec.file_name,
        &format!(
            r#"#!/bin/sh
echo $$ >> {pid}
ps -o pgid= -p $$ | tr -d ' ' >> {pgid}
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      # Resolution phase: accept every task so it enters the graph, then wait
      # for the actual run request.
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
      continue
      ;;
  esac
{body}done
{trailer}"#,
            pid = markers.pid.display(),
            pgid = markers.pgid.display(),
            body = spec.per_request_body,
            trailer = spec.trailer,
        ),
    )
}

/// Per-request shell body for a worker that processes a job (log + done) and
/// signals readiness.
fn processed_request_body(ready: &Path) -> String {
    format!(
        r#"  printf '{{"type":"log","id":"%s","stream":"stdout","line":"processed"}}\n' "$id"
  printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
  printf ready > {ready}
"#,
        ready = ready.display()
    )
}

/// Per-request shell body for a worker that signals readiness and then blocks
/// forever WITHOUT sending a `done` response, keeping the job in flight.
fn never_done_request_body(ready: &Path) -> String {
    format!(
        r#"  printf '{{"type":"log","id":"%s","stream":"stdout","line":"started"}}\n' "$id"
  printf ready > {ready}
  # Never emit a `done` response: the job stays in flight until killed.
  exec sleep 60
"#,
        ready = ready.display()
    )
}

#[test]
fn interrupt_shuts_down_promptly_without_orphans() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_two_packages(&temp, false);

    let pid_marker = init_marker(&temp, "worker.pid");
    let pgid_marker = init_marker(&temp, "worker.pgid");
    let worker_ready = temp.child("worker.ready");
    let stdout_log = temp.child("luchta.stdout");
    let stderr_log = temp.child("luchta.stderr");

    let worker = write_recording_worker(
        &temp,
        RecordingMarkers {
            pid: &pid_marker,
            pgid: &pgid_marker,
        },
        RecordingWorkerSpec {
            file_name: "busy-worker.sh",
            per_request_body: &never_done_request_body(worker_ready.path()),
            trailer: "",
        },
    );
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":1}},"tasks":{{"build":{{"worker":"busy-worker"}}}},"workers":{{"busy-worker":{{"command":"{}"}}}}}}"#,
            worker.display()
        ),
    );

    let mut child = Command::new(cargo_bin("luchta"))
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .stdout(Stdio::from(
            fs::File::create(stdout_log.path()).expect("create stdout log"),
        ))
        .stderr(Stdio::from(
            fs::File::create(stderr_log.path()).expect("create stderr log"),
        ))
        .spawn()
        .expect("spawn luchta");

    // Wait until a job is actually in flight, then interrupt luchta.
    wait_for_file(worker_ready.path(), Duration::from_secs(5));

    let luchta_pid = child.id() as i32;
    // SAFETY: sending SIGINT to a known child pid.
    unsafe {
        libc::kill(luchta_pid, libc::SIGINT);
    }

    let started = Instant::now();
    let status = child.wait().expect("wait for luchta");
    let elapsed = started.elapsed();

    let stderr = fs::read_to_string(stderr_log.path()).unwrap_or_default();

    // It must exit promptly on interrupt (not wait for the worker's 60s sleep).
    assert!(
        elapsed < Duration::from_secs(10),
        "luchta should exit promptly after SIGINT; elapsed: {elapsed:?}, stderr: {stderr}"
    );
    // Interrupted runs are a failure (non-zero exit).
    assert!(
        !status.success(),
        "interrupted run should exit non-zero; stderr: {stderr}"
    );
    // No post-exit noise: neither broken-pipe spam from orphaned workers nor a
    // per-task crash/Debug "wall of text" for jobs killed by the interrupt.
    assert!(
        !stderr.contains("Broken pipe")
            && !stderr.contains("job failed")
            && !stderr.contains("Worker {")
            && !stderr.contains("Crashed"),
        "interrupt must not leave broken-pipe / crash noise; stderr: {stderr}"
    );

    // The worker and its process group must be gone (no orphans).
    assert_worker_group_reaped(&pid_marker, &pgid_marker);

    temp.close().expect("cleanup temp dir");
}

/// Asserts the recorded worker pids/pgids are non-empty and fully reaped.
fn assert_worker_group_reaped(pid_marker: &Path, pgid_marker: &Path) {
    let worker_pids = read_marker_pids(pid_marker);
    assert!(!worker_pids.is_empty(), "worker should record its PID");
    let worker_pgids = read_marker_pids(pgid_marker);
    assert!(
        !worker_pgids.is_empty(),
        "worker should record its process group"
    );
    assert_processes_gone_before_timeout(&worker_pids, &worker_pgids, Duration::from_secs(5));
}

/// Non-empty trimmed lines of a marker file parsed as PIDs.
fn read_marker_pids(path: &Path) -> Vec<i32> {
    fs::read_to_string(path)
        .expect("read marker")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.parse::<i32>().expect("marker should contain pid"))
        .collect()
}

#[test]
fn resident_worker_reuse_and_output_streaming() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_two_packages(&temp, false);

    let pid_marker = init_marker(&temp, "worker.pid");
    let fake_yarn_bin = write_fake_yarn(&temp);
    let wrapper = write_executable(
        &temp,
        "yarn-wrapper.sh",
        &format!(
            "#!/bin/sh\necho $$ >> \"{pid}\"\nexec \"{worker}\" \"$@\"\n",
            pid = pid_marker.display(),
            worker = yarn_worker_bin().display()
        ),
    );
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":1}},"tasks":{{"build":{{"worker":"yarn"}}}},"workers":{{"yarn":{{"command":"{}"}}}}}}"#,
            wrapper.display()
        ),
    );

    let output = assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .env("PATH", path_with_prepend(&fake_yarn_bin))
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .output()
        .expect("run command");

    assert!(
        output.status.success(),
        "expected success (stdout: {}, stderr: {})",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Exactly one PID recorded -> a single resident worker served both tasks.
    assert_eq!(
        read_marker_pids(&pid_marker).len(),
        1,
        "expected single worker PID (resident reuse)"
    );

    let worker_invocations = fs::read_to_string(&pid_marker).expect("read worker pid marker");
    assert_eq!(
        worker_invocations.lines().count(),
        1,
        "expected one worker process to service both tasks: {worker_invocations}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("☑️ 2 ⏭️ 0"),
        "expected done summary, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn worker_crash_propagates_task_failure() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_two_packages(&temp, true);

    let worker = write_executable(
        &temp,
        "crashing-worker.sh",
        "#!/bin/sh\nwhile IFS= read -r line; do\n  exit 1\ndone\n",
    );
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":4}},"tasks":{{"build":{{"dependsOn":["^build"],"worker":"crash-worker"}}}},"workers":{{"crash-worker":{{"command":"{}"}}}}}}"#,
            worker.display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .failure();

    temp.close().expect("cleanup temp dir");
}

#[test]
fn worker_is_reaped_after_run() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_two_packages(&temp, false);

    let pid_marker = init_marker(&temp, "worker.pid");
    let pgid_marker = init_marker(&temp, "worker.pgid");
    let worker_ready = temp.child("worker.ready");
    let stdout_log = temp.child("luchta.stdout");
    let stderr_log = temp.child("luchta.stderr");

    let worker = write_recording_worker(
        &temp,
        RecordingMarkers {
            pid: &pid_marker,
            pgid: &pgid_marker,
        },
        RecordingWorkerSpec {
            file_name: "sleeping-worker.sh",
            per_request_body: &processed_request_body(worker_ready.path()),
            trailer: "exec sleep 60\n",
        },
    );
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":1}},"tasks":{{"build":{{"worker":"sleep-worker"}}}},"workers":{{"sleep-worker":{{"command":"{}"}}}}}}"#,
            worker.display()
        ),
    );

    // Run luchta with output redirected to FILES (not pipes) so the test
    // process never blocks waiting for an orphaned worker to close a pipe.
    let mut child = Command::new(cargo_bin("luchta"))
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .stdout(Stdio::from(
            fs::File::create(stdout_log.path()).expect("create stdout log"),
        ))
        .stderr(Stdio::from(
            fs::File::create(stderr_log.path()).expect("create stderr log"),
        ))
        .spawn()
        .expect("spawn luchta");

    wait_for_file(worker_ready.path(), Duration::from_secs(2));

    let started = Instant::now();
    let status = child.wait().expect("wait for luchta");
    let elapsed = started.elapsed();

    let stdout = fs::read_to_string(stdout_log.path()).unwrap_or_default();
    let stderr = fs::read_to_string(stderr_log.path()).unwrap_or_default();
    assert!(
        status.success(),
        "luchta should exit successfully (stdout: {stdout}, stderr: {stderr})"
    );
    // Successful task output is captured (not streamed) in default mode, so the
    // worker's "processed" line no longer appears on stdout. The run still emits
    // the Done summary; that the worker actually ran is asserted below via the
    // recorded PID/PGID markers.
    assert!(
        stdout.contains("☑️ 2 ⏭️ 0") && stdout.contains("🌊 1 / 1"),
        "stdout should include the Done summary: {stdout}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "luchta should not wait for the worker's trailing sleep; elapsed: {elapsed:?}"
    );

    let worker_pids = read_marker_pids(&pid_marker);
    assert!(!worker_pids.is_empty(), "worker should record its PID");
    let worker_pgids = read_marker_pids(&pgid_marker);
    assert!(
        !worker_pgids.is_empty(),
        "worker should record its process group"
    );
    assert_processes_gone_before_timeout(&worker_pids, &worker_pgids, Duration::from_secs(2));

    temp.close().expect("cleanup temp dir");
}

#[test]
fn explicit_worker_command_sends_workspace_in_request_json() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    write_workspace(
        &temp,
        &[
            Pkg {
                name: "a",
                script: "echo built-a",
                depends_on_a: false,
            },
            Pkg {
                name: "b",
                script: "echo built-b",
                depends_on_a: false,
            },
        ],
    );

    let capture = temp.child("worker-requests.log");
    capture.write_str("").expect("init capture file");
    let worker = write_capture_worker(&temp, "capture-worker.sh", capture.path());
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":1}},"tasks":{{"build":{{"worker":"capture","command":"build --flag"}}}},"workers":{{"capture":{{"command":"{}"}}}}}}"#,
            worker.display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success();

    let requests = fs::read_to_string(capture.path()).expect("read captured requests");
    assert!(
        requests.contains(r#""workspace":"a""#),
        "expected package workspace in worker request: {requests}"
    );
    assert!(
        requests.contains(r#""command":"build --flag""#),
        "expected explicit command in worker request: {requests}"
    );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn explicit_root_worker_command_sends_empty_workspace_in_request_json() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    temp.child("package.json")
        .write_str(
            r#"{ "name": "root", "private": true, "workspaces": ["packages/*"], "scripts": { "build": "echo root-build" } }"#,
        )
        .expect("write root package.json");

    let capture = temp.child("worker-requests.log");
    capture.write_str("").expect("init capture file");
    let worker = write_capture_worker(&temp, "capture-root-worker.sh", capture.path());
    write_config(
        &temp,
        &format!(
            r##"{{"concurrency":{{"maxWeight":1}},"tasks":{{"#build":{{"worker":"capture","command":"install"}}}},"workers":{{"capture":{{"command":"{}"}}}}}}"##,
            worker.display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("-T")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success();

    let requests = fs::read_to_string(capture.path()).expect("read captured requests");
    assert!(
        requests.contains(r#""workspace":"""#),
        "expected root worker request to use empty workspace hint: {requests}"
    );
    assert!(
        requests.contains(r#""command":"install""#),
        "expected explicit root command in worker request: {requests}"
    );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn root_worker_task_without_command_defaults_to_task_name() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    temp.child("package.json")
        .write_str(
            r#"{ "name": "root", "private": true, "workspaces": ["packages/*"], "scripts": { "build": "echo root-build" } }"#,
        )
        .expect("write root package.json");

    let capture = temp.child("worker-requests.log");
    capture.write_str("").expect("init capture file");
    let worker = write_capture_worker(&temp, "capture-root-default-worker.sh", capture.path());
    // Root `#build` worker task with NO command: defaults to task name and an
    // empty workspace hint, so worker runs root `build` command.
    write_config(
        &temp,
        &format!(
            r##"{{"concurrency":{{"maxWeight":1}},"tasks":{{"#build":{{"worker":"capture"}}}},"workers":{{"capture":{{"command":"{}"}}}}}}"##,
            worker.display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("-T")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success();

    let requests = fs::read_to_string(capture.path()).expect("read captured requests");
    assert!(
        requests.contains(r#""workspace":"""#),
        "expected root worker request to use empty workspace hint: {requests}"
    );
    assert!(
        requests.contains(r#""command":"build""#),
        "expected missing root worker command to default to task name: {requests}"
    );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn blank_explicit_worker_command_defaults_to_task_name() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    write_workspace(
        &temp,
        &[Pkg {
            name: "a",
            script: "echo built-a",
            depends_on_a: false,
        }],
    );

    let capture = temp.child("worker-requests.log");
    capture.write_str("").expect("init capture file");
    let worker = write_capture_worker(&temp, "capture-blank-worker.sh", capture.path());
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":1}},"tasks":{{"build":{{"worker":"capture","command":"   "}}}},"workers":{{"capture":{{"command":"{}"}}}}}}"#,
            worker.display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success();

    let requests = fs::read_to_string(capture.path()).expect("read captured requests");
    assert!(
        requests.contains(r#""workspace":"a""#),
        "expected package workspace in worker request: {requests}"
    );
    assert!(
        requests.contains(r#""command":"build""#),
        "expected blank explicit command to default to task name in worker request: {requests}"
    );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn worker_task_without_command_defaults_to_task_name() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    write_workspace(
        &temp,
        &[Pkg {
            name: "a",
            script: "echo built-a",
            depends_on_a: false,
        }],
    );

    let capture = temp.child("worker-requests.log");
    capture.write_str("").expect("init capture file");
    let worker = write_capture_worker(&temp, "capture-default-worker.sh", capture.path());
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":1}},"tasks":{{"build":{{"worker":"capture"}}}},"workers":{{"capture":{{"command":"{}"}}}}}}"#,
            worker.display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success();

    let requests = fs::read_to_string(capture.path()).expect("read captured requests");
    assert!(
        requests.contains(r#""workspace":"a""#),
        "expected package workspace in worker request: {requests}"
    );
    assert!(
        requests.contains(r#""command":"build""#),
        "expected missing worker command to default to task name in worker request: {requests}"
    );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn real_yarn_worker_e2e() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    write_workspace(
        &temp,
        &[Pkg {
            name: "myapp",
            script: "echo built-via-yarn-worker",
            depends_on_a: false,
        }],
    );
    let fake_yarn_bin = write_fake_yarn(&temp);
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":4}},"tasks":{{"build":{{"worker":"yarn"}}}},"workers":{{"yarn":{{"command":"{}"}}}}}}"#,
            yarn_worker_bin().display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .env("PATH", path_with_prepend(&fake_yarn_bin))
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("☑️ 1 ⏭️ 0"))
        .stdout(predicate::str::contains("yarn-ran workspace=myapp script=build").not());

    temp.close().expect("cleanup temp dir");
}

#[test]
fn tasks_config_without_workers_skips_noop_tasks() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_two_packages(&temp, true);
    write_config(
        &temp,
        r#"{"concurrency":{"maxWeight":4},"tasks":{"build":{"dependsOn":["^build"]},"test":{}}}"#,
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("built-a").not())
        .stdout(predicate::str::contains("built-b").not());

    temp.close().expect("cleanup temp dir");
}

/// Writes a workspace where `has-build` declares a `build` script and
/// `no-build` declares only a `lint` script (no `build`). Returns nothing; the
/// caller drives `luchta run build` against it.
fn write_mixed_scripts_workspace(temp: &assert_fs::TempDir) {
    temp.child("package.json")
        .write_str(r#"{ "name": "root", "private": true, "workspaces": ["packages/*"] }"#)
        .expect("write root package.json");

    fs::create_dir_all(temp.child("packages/has-build").path()).expect("mkdir has-build");
    temp.child("packages/has-build/package.json")
        .write_str(r#"{ "name": "has-build", "scripts": { "build": "echo built-has" } }"#)
        .expect("write has-build package.json");

    fs::create_dir_all(temp.child("packages/no-build").path()).expect("mkdir no-build");
    temp.child("packages/no-build/package.json")
        .write_str(r#"{ "name": "no-build", "scripts": { "lint": "echo linted" } }"#)
        .expect("write no-build package.json");
}

#[test]
fn global_task_prunes_packages_missing_the_script() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    write_mixed_scripts_workspace(&temp);
    let fake_yarn_bin = write_fake_yarn(&temp);
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":4}},"tasks":{{"build":{{"worker":"yarn"}}}},"workers":{{"yarn":{{"command":"{}"}}}}}}"#,
            yarn_worker_bin().display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .env("PATH", path_with_prepend(&fake_yarn_bin))
        .env("NO_COLOR", "1")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("☑️ 1 ⏭️ 0"))
        .stdout(predicate::str::contains("yarn-ran workspace=has-build").not())
        // no-build#build is pruned (no prune output on run path).
        .stdout(predicate::str::contains("yarn-ran workspace=no-build").not());

    temp.close().expect("cleanup temp dir");
}

#[test]
fn explicit_command_resolves_against_scripts_for_pruning() {
    // Task name is `start` (absent everywhere) but an explicit `command:"build"`
    // points at the `build` script. has-build keeps it; no-build is pruned
    // because it has no `build` script.
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    write_mixed_scripts_workspace(&temp);
    let fake_yarn_bin = write_fake_yarn(&temp);
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":4}},"tasks":{{"start":{{"worker":"yarn","command":"build"}}}},"workers":{{"yarn":{{"command":"{}"}}}}}}"#,
            yarn_worker_bin().display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .env("PATH", path_with_prepend(&fake_yarn_bin))
        .env("NO_COLOR", "1")
        .arg("run")
        .arg("start")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("☑️ 1 ⏭️ 0"))
        .stdout(predicate::str::contains("yarn-ran workspace=has-build").not())
        .stdout(predicate::str::contains("yarn-ran workspace=no-build").not());

    temp.close().expect("cleanup temp dir");
}

#[test]
fn check_reports_prunes_without_failing() {
    // `luchta check` must succeed (Prune is informational) while still listing
    // the pruned task.
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    write_mixed_scripts_workspace(&temp);
    let fake_yarn_bin = write_fake_yarn(&temp);
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":4}},"tasks":{{"build":{{"worker":"yarn"}}}},"workers":{{"yarn":{{"command":"{}"}}}}}}"#,
            yarn_worker_bin().display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .env("PATH", path_with_prepend(&fake_yarn_bin))
        .env("NO_COLOR", "1")
        .arg("check")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("pruned during resolution")
                .and(predicate::str::contains("no-build#build")),
        )
        .stdout(predicate::str::contains("Configuration valid"))
        // `check` resolves only — it must never reach the run phase, so the
        // surviving task is never executed through the (fake) yarn worker.
        .stdout(predicate::str::contains("yarn-ran").not());

    temp.close().expect("cleanup temp dir");
}

#[test]
fn root_worker_task_resolves_against_root_package_scripts() {
    // A `#task` root worker task must resolve against the workspace-root
    // package's scripts (not against an empty set), so a root `build` script
    // keeps the task instead of pruning it.
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    temp.child("package.json")
        .write_str(
            r#"{ "name": "root", "private": true, "workspaces": ["packages/*"], "scripts": { "release": "echo released" } }"#,
        )
        .expect("write root package.json");
    fs::create_dir_all(temp.child("packages/app").path()).expect("mkdir app");
    temp.child("packages/app/package.json")
        .write_str(r#"{ "name": "app", "scripts": { "build": "echo built" } }"#)
        .expect("write app package.json");

    let fake_yarn_bin = write_fake_yarn(&temp);
    write_config(
        &temp,
        &format!(
            r##"{{"concurrency":{{"maxWeight":4}},"tasks":{{"#release":{{"worker":"yarn"}}}},"workers":{{"yarn":{{"command":"{}"}}}}}}"##,
            yarn_worker_bin().display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .env("PATH", path_with_prepend(&fake_yarn_bin))
        .env("NO_COLOR", "1")
        .arg("run")
        .arg("-T")
        .arg("release")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        // The root task ran and was NOT pruned; successful worker output stays hidden.
        .stdout(predicate::str::contains("☑️ 1 ⏭️ 0"))
        .stdout(predicate::str::contains("yarn-ran root script=release").not())
        .stdout(predicate::str::contains("pruned during resolution").not());

    temp.close().expect("cleanup temp dir");
}

#[test]
fn bash_worker_blank_command_check_fails_and_run_skips() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_two_packages(&temp, false);
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":1}},"tasks":{{"build":{{"worker":"bash","command":"   "}}}},"workers":{{"bash":{{"command":"{}"}}}}}}"#,
            bash_worker_bin().display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .env("NO_COLOR", "1")
        .arg("check")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("blank command"));

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .env("NO_COLOR", "1")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("blank command").not())
        .stdout(predicate::str::contains("was pruned from every package"))
        .stdout(predicate::str::contains("Running 0 task(s)").not());

    temp.close().expect("cleanup temp dir");
}

#[test]
fn real_bash_worker_clears_ambient_env_end_to_end() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    write_workspace(
        &temp,
        &[Pkg {
            name: "myapp",
            script: "echo package-build-script-unused",
            depends_on_a: false,
        }],
    );
    let env_capture = init_marker(&temp, "env_capture.txt");
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":1}},"tasks":{{"build":{{"worker":"bash","command":"env > {capture}","env":{{"DECLARED_VAR":{{"value":"declared-value"}}}}}}}},"workers":{{"bash":{{"command":"{}"}}}}}}"#,
            bash_worker_bin().display(),
            capture = env_capture.display()
        ),
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .env("NO_COLOR", "1")
        .env("LUCHTA_AMBIENT_LEAK_TEST", "should-not-appear")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("☑️ 1 ⏭️ 0"))
        .stdout(predicate::str::contains("package-build-script-unused").not())
        .stdout(predicate::str::contains("pruned during resolution").not());

    let captured = fs::read_to_string(&env_capture).expect("read env capture");
    assert!(
        captured.contains("DECLARED_VAR=declared-value"),
        "declared env should reach subprocess: {captured}"
    );
    assert!(
        captured.contains("PATH="),
        "whitelisted PATH should reach subprocess: {captured}"
    );
    assert!(
        !captured.contains("LUCHTA_AMBIENT_LEAK_TEST=should-not-appear"),
        "ambient env should be cleared before subprocess spawn: {captured}"
    );

    temp.close().expect("cleanup temp dir");
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_processes_gone_before_timeout(pids: &[i32], pgids: &[i32], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let live_pids: Vec<_> = pids
            .iter()
            .copied()
            .filter(|p| process_is_running(*p))
            .collect();
        let live_pgids: Vec<_> = pgids
            .iter()
            .copied()
            .filter(|g| process_group_has_running_member(*g))
            .collect();

        if live_pids.is_empty() && live_pgids.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "worker processes should be gone after run; live pids: {live_pids:?}; live pgids: {live_pgids:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn process_is_running(pid: i32) -> bool {
    process_stat_fields(pid)
        .and_then(|fields| {
            fields
                .split_whitespace()
                .next()
                .and_then(|s| s.chars().next())
        })
        .is_some_and(|state| state != 'Z')
}

fn process_group_has_running_member(pgid: i32) -> bool {
    fs::read_dir("/proc")
        .expect("read /proc")
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<i32>().ok())
        .any(|pid| process_is_running(pid) && process_group_id(pid) == Some(pgid))
}

fn process_group_id(pid: i32) -> Option<i32> {
    process_stat_fields(pid).and_then(|fields| fields.split_whitespace().nth(2)?.parse().ok())
}

fn process_stat_fields(pid: i32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| stat.rsplit_once(") ").map(|(_, fields)| fields.to_owned()))
}
