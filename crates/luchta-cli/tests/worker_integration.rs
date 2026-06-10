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

/// Absolute path to built `luchta-yarn-worker` binary.
fn yarn_worker_bin() -> std::path::PathBuf {
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        escargot::CargoBuild::new()
            .bin("luchta-yarn-worker")
            .package("luchta-yarn-worker")
            .run()
            .expect("build luchta-yarn-worker")
            .path()
            .to_path_buf()
    })
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
fn write_sleeping_worker(
    temp: &assert_fs::TempDir,
    pid_marker: &Path,
    pgid_marker: &Path,
    ready: &Path,
) -> std::path::PathBuf {
    write_executable(
        temp,
        "sleeping-worker.sh",
        &format!(
            r#"#!/bin/sh
echo $$ >> {pid}
ps -o pgid= -p $$ | tr -d ' ' >> {pgid}
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{{"type":"log","id":"%s","stream":"stdout","line":"processed"}}\n' "$id"
  printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
  printf ready > {ready}
done
exec sleep 60
"#,
            pid = pid_marker.display(),
            pgid = pgid_marker.display(),
            ready = ready.display()
        ),
    )
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

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected success (stdout: {stdout}, stderr: {})",
        String::from_utf8_lossy(&output.stderr)
    );

    // Exactly one PID recorded -> a single resident worker served both tasks.
    assert_eq!(
        read_marker_pids(&pid_marker).len(),
        1,
        "expected single worker PID (resident reuse)"
    );
    assert!(
        stdout.contains("a#build") && stdout.contains("yarn-ran workspace=a script=build"),
        "expected a#build yarn output, got: {stdout}"
    );
    assert!(
        stdout.contains("b#build") && stdout.contains("yarn-ran workspace=b script=build"),
        "expected b#build yarn output, got: {stdout}"
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

    let worker = write_sleeping_worker(&temp, &pid_marker, &pgid_marker, worker_ready.path());
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
    assert!(
        stdout.contains("processed"),
        "stdout should include worker output: {stdout}"
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
    let worker = write_executable(
        &temp,
        "capture-worker.sh",
        &format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s
' "$line" >> "{capture}"
  id=$(printf '%s
' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{{"type":"done","id":"%s","exitCode":0}}
' "$id"
done
"#,
            capture = capture.path().display()
        ),
    );
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
    let worker = write_executable(
        &temp,
        "capture-root-worker.sh",
        &format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s
' "$line" >> "{capture}"
  id=$(printf '%s
' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{{"type":"done","id":"%s","exitCode":0}}
' "$id"
done
"#,
            capture = capture.path().display()
        ),
    );
    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":1}},"tasks":{{"build":{{"worker":"capture","command":"install"}}}},"workers":{{"capture":{{"command":"{}"}}}}}}"#,
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
    let worker = write_executable(
        &temp,
        "capture-root-default-worker.sh",
        &format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s
' "$line" >> "{capture}"
  id=$(printf '%s
' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{{"type":"done","id":"%s","exitCode":0}}
' "$id"
done
"#,
            capture = capture.path().display()
        ),
    );
    // Root/top-level worker task with NO command: defaults to the task name and
    // an empty workspace hint, so the worker runs `yarn build`.
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
    let worker = write_executable(
        &temp,
        "capture-blank-worker.sh",
        &format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s
' "$line" >> "{capture}"
  id=$(printf '%s
' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{{"type":"done","id":"%s","exitCode":0}}
' "$id"
done
"#,
            capture = capture.path().display()
        ),
    );
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
    let worker = write_executable(
        &temp,
        "capture-default-worker.sh",
        &format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s
' "$line" >> "{capture}"
  id=$(printf '%s
' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{{"type":"done","id":"%s","exitCode":0}}
' "$id"
done
"#,
            capture = capture.path().display()
        ),
    );
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
        .stdout(predicate::str::contains("myapp#build"))
        .stdout(predicate::str::contains(
            "yarn-ran workspace=myapp script=build",
        ));

    temp.close().expect("cleanup temp dir");
}

#[test]
fn tasks_config_works_without_workers() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_two_packages(&temp, true);
    write_config(
        &temp,
        r#"{"concurrency":{"maxWeight":4},"tasks":{"build":{"dependsOn":["^build"]}}}"#,
    );

    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("a#build").and(predicate::str::contains("built-a")))
        .stdout(predicate::str::contains("b#build").and(predicate::str::contains("built-b")));

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
