//! Integration tests for the passive `luchta await` readiness barrier.

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

use assert_cmd::cargo::cargo_bin;
use assert_fs::{prelude::*, TempDir};

mod common;

// Await polls once per second. Negative readiness assertions sleep slightly
// longer so they prove that at least one subsequent poll rejected the state.
const POLL_CYCLE_PLUS_MARGIN: Duration = Duration::from_millis(1_200);

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("child already taken")
    }

    fn wait(mut self) -> ExitStatus {
        self.0
            .take()
            .expect("child already taken")
            .wait()
            .expect("wait for child")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn binary_path() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| cargo_bin("luchta"))
}

fn setup_workspace() -> TempDir {
    let temp = TempDir::new().expect("create temp workspace");
    common::write_root_workspace(&temp);
    common::write_basic_package(&temp, "build");
    let worker = common::shell_worker(&temp);
    write_await_config(&temp, worker.path(), r#""app#gate":{"dependsOn":["dep"]}"#);
    temp.child("packages/app/dep.in")
        .write_str("one\n")
        .unwrap();
    temp.child("packages/app/build.in")
        .write_str("one\n")
        .unwrap();
    temp.child("packages/app/other.in")
        .write_str("other\n")
        .unwrap();
    temp.child("packages/app/shared.in")
        .write_str("shared\n")
        .unwrap();
    temp.child("root.in").write_str("root\n").unwrap();
    common::init_git(&temp);
    temp
}

fn write_await_config(temp: &TempDir, worker: &Path, gate_definition: &str) {
    let task_json = [
        r##"
"app#dep":{"cache":{},"worker":"shell","inputs":["dep.in"],"outputs":["dep.out"],"command":"printf dep-$(cat dep.in) > dep.out"},
"##,
        gate_definition,
        r##",
"app#build":{"cache":{},"worker":"shell","dependsOn":["gate"],"inputs":["build.in"],"outputs":["build.out"],"command":"printf build-$(cat build.in) > build.out"},
"app#other":{"cache":{},"worker":"shell","inputs":["other.in"],"outputs":["other.out"],"command":"cp other.in other.out"},
"app#flaky":{"cache":{},"worker":"shell","outputs":["flaky.out"],"command":"if [ -f fail ]; then exit 7; fi; printf ready > flaky.out"},
"app#noncache":{"worker":"shell","dependsOn":["dep"],"outputs":["noncache.count"],"command":"count=$(cat noncache.count 2>/dev/null || echo 0); count=$((count+1)); printf %s $count > noncache.count"},
"app#shared":{"cache":{},"worker":"shell","inputs":["shared.in"],"outputs":["shared.out"],"command":"sleep 0.15; cp shared.in shared.out"},
"app#hold":{"worker":"shell","command":"printf held > ../../held; while [ ! -f ../../release ]; do sleep 0.05; done"},
"#rootbuild":{"cache":{},"worker":"shell","inputs":["root.in"],"outputs":["root.out"],"command":"cp root.in root.out"}
"##,
    ]
    .concat();
    common::write_task_config_with_shell_worker(temp, worker, &task_json);
}

fn run_luchta(temp: &TempDir, subcommand: &str, args: &[&str]) -> Output {
    Command::new(binary_path())
        .arg(subcommand)
        .args(args)
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("run luchta")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn spawn_await(
    temp: &TempDir,
    args: &[&str],
    stdout_path: &Path,
    environment: &[(&str, &OsStr)],
) -> ChildGuard {
    let stdout = fs::File::create(stdout_path).expect("create await stdout");
    let mut command = Command::new(binary_path());
    command
        .arg("await")
        .args(args)
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .stdout(Stdio::from(stdout));
    for (name, value) in environment {
        command.env(name, value);
    }
    ChildGuard::new(command.spawn().expect("spawn luchta await"))
}

fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {description}");
}

fn wait_for_message(path: &Path, message: &str) {
    wait_for(
        Duration::from_secs(20),
        || {
            fs::read_to_string(path)
                .unwrap_or_default()
                .contains(message)
        },
        message,
    );
}

fn wait_for_success(mut child: ChildGuard) {
    wait_for(
        Duration::from_secs(20),
        || {
            child
                .child_mut()
                .try_wait()
                .expect("poll await process")
                .is_some()
        },
        "await process to exit",
    );
    assert!(
        child.wait().success(),
        "await process should exit successfully"
    );
}

#[test]
fn current_tasks_complete_immediately_for_package_glob_top_level_and_implicit_package() {
    let temp = setup_workspace();
    assert_success(&run_luchta(&temp, "run", &["build", "-p", "app"]));
    assert_success(&run_luchta(&temp, "run", &["rootbuild", "--top-level"]));

    let package = run_luchta(&temp, "await", &["build", "-p", "a*"]);
    assert_success(&package);
    assert!(String::from_utf8_lossy(&package.stdout).contains("All awaited tasks are current."));
    assert!(!String::from_utf8_lossy(&package.stdout).contains("Waiting for tasks"));

    let top_level = run_luchta(&temp, "await", &["rootbuild", "-T"]);
    assert_success(&top_level);

    let implicit = Command::new(binary_path())
        .arg("await")
        .arg("build")
        .current_dir(temp.path().join("packages/app"))
        .env("NO_COLOR", "1")
        .output()
        .expect("run implicit-package await");
    assert_success(&implicit);
}

#[test]
fn invalid_selected_tasks_fail_before_polling() {
    let temp = setup_workspace();
    let worker = common::shell_worker(&temp);
    common::write_task_config_with_shell_worker(
        &temp,
        worker.path(),
        r#""app#bad":{"command":"bad"},"app#worse":{"command":"worse"}"#,
    );

    let output = run_luchta(&temp, "await", &["bad", "worse", "-p", "app"]);
    assert!(!output.status.success(), "invalid task must fail await");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("task 'app#bad' defines a command")
            && stderr.contains("task 'app#worse' defines a command")
            && stderr.matches("no worker").count() == 2,
        "unexpected stderr: {stderr}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Waiting for tasks"));
}

#[test]
fn cache_state_errors_fail_instead_of_polling_forever() {
    let temp = setup_workspace();
    temp.child("yarn.lock")
        .write_str("this is not a valid lockfile\n")
        .unwrap();

    let output = run_luchta(&temp, "await", &["build", "-p", "app"]);
    assert!(
        !output.status.success(),
        "cache-state errors must fail await"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to inspect readiness for task 'app#dep'"),
        "unexpected stderr: {stderr}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Waiting for tasks"));
}

#[test]
fn waits_for_selected_transitive_subgraph_without_executing_or_accepting_unrelated_work() {
    let temp = setup_workspace();
    let stdout_path = temp.path().join("await.stdout");
    let mut awaiting = spawn_await(&temp, &["build", "-p", "app"], &stdout_path, &[]);
    wait_for_message(&stdout_path, "Waiting for tasks to become current ...");

    assert!(!temp.path().join("packages/app/dep.out").exists());
    assert!(!temp.path().join("packages/app/build.out").exists());

    assert_success(&run_luchta(&temp, "run", &["other", "-p", "app"]));
    thread::sleep(POLL_CYCLE_PLUS_MARGIN);
    assert!(
        awaiting
            .child_mut()
            .try_wait()
            .expect("poll await")
            .is_none(),
        "an unrelated build must not satisfy await"
    );

    assert_success(&run_luchta(&temp, "run", &["build", "-p", "app"]));
    wait_for_success(awaiting);
    let stdout = fs::read_to_string(&stdout_path).expect("read await stdout");
    assert_eq!(
        stdout
            .matches("Waiting for tasks to become current ...")
            .count(),
        1,
        "the waiting notice should not repeat: {stdout}"
    );
    assert!(stdout.contains("All awaited tasks are current."));
}

#[test]
fn stale_and_failed_cacheable_records_remain_pending_until_a_successful_current_build() {
    let temp = setup_workspace();
    assert_success(&run_luchta(&temp, "run", &["build", "-p", "app"]));
    temp.child("packages/app/build.in")
        .write_str("two\n")
        .unwrap();

    let stale_stdout = temp.path().join("stale-await.stdout");
    let stale = spawn_await(&temp, &["build", "-p", "app"], &stale_stdout, &[]);
    wait_for_message(&stale_stdout, "Waiting for tasks to become current ...");
    assert_success(&run_luchta(&temp, "run", &["build", "-p", "app"]));
    wait_for_success(stale);

    temp.child("packages/app/fail").write_str("fail\n").unwrap();
    let failed_run = run_luchta(&temp, "run", &["flaky", "-p", "app"]);
    assert!(!failed_run.status.success(), "fixture run should fail");

    let failed_stdout = temp.path().join("failed-await.stdout");
    let failed = spawn_await(&temp, &["flaky", "-p", "app"], &failed_stdout, &[]);
    wait_for_message(&failed_stdout, "Waiting for tasks to become current ...");
    fs::remove_file(temp.path().join("packages/app/fail")).expect("clear failure marker");
    assert_success(&run_luchta(&temp, "run", &["flaky", "-p", "app"]));
    wait_for_success(failed);
}

#[test]
fn non_cacheable_task_must_run_again_after_its_dependency_output_changes() {
    let temp = setup_workspace();
    assert_success(&run_luchta(&temp, "run", &["noncache", "-p", "app"]));
    temp.child("packages/app/noncache.count").assert("1");

    temp.child("packages/app/dep.in")
        .write_str("two\n")
        .unwrap();
    assert_success(&run_luchta(&temp, "run", &["dep", "-p", "app"]));

    let stdout_path = temp.path().join("noncache-await.stdout");
    let awaiting = spawn_await(&temp, &["noncache", "-p", "app"], &stdout_path, &[]);
    wait_for_message(&stdout_path, "Waiting for tasks to become current ...");
    temp.child("packages/app/noncache.count").assert("1");

    assert_success(&run_luchta(&temp, "run", &["noncache", "-p", "app"]));
    wait_for_success(awaiting);
    temp.child("packages/app/noncache.count").assert("2");
}

#[test]
fn cacheable_task_ignores_a_stale_record_for_an_ordering_only_dependency() {
    let temp = setup_workspace();
    let worker = common::shell_worker(&temp);
    write_await_config(
        &temp,
        worker.path(),
        r#""app#gate":{"cache":{},"worker":"shell","dependsOn":["dep"],"outputs":["gate.out"],"command":"printf gate > gate.out"}"#,
    );
    assert_success(&run_luchta(&temp, "run", &["build", "-p", "app"]));

    write_await_config(&temp, worker.path(), r#""app#gate":{"dependsOn":["dep"]}"#);
    assert_success(&run_luchta(&temp, "run", &["build", "-p", "app"]));

    let output = run_luchta(&temp, "await", &["build", "-p", "app"]);
    assert_success(&output);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Waiting for tasks"));
}

#[test]
fn await_does_not_restore_a_shared_cache_hit() {
    let temp = setup_workspace();
    let shared_cache = tempfile::tempdir().expect("create shared cache");
    let first = Command::new(binary_path())
        .args(["run", "shared", "-p", "app", "--workspace-root"])
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .env("LUCHTA_SHARED_CACHE", "1")
        .env("LUCHTA_SHARED_CACHE_DIR", shared_cache.path())
        .output()
        .expect("populate shared cache");
    assert_success(&first);

    fs::remove_dir_all(temp.path().join(".luchta/cache")).expect("remove local cache metadata");
    fs::remove_file(temp.path().join("packages/app/shared.out")).expect("remove local output");

    let stdout_path = temp.path().join("shared-await.stdout");
    let mut awaiting = spawn_await(
        &temp,
        &["shared", "-p", "app"],
        &stdout_path,
        &[
            ("LUCHTA_SHARED_CACHE", OsStr::new("1")),
            ("LUCHTA_SHARED_CACHE_DIR", shared_cache.path().as_os_str()),
        ],
    );
    wait_for_message(&stdout_path, "Waiting for tasks to become current ...");
    thread::sleep(POLL_CYCLE_PLUS_MARGIN);
    assert!(!temp.path().join("packages/app/shared.out").exists());
    assert!(
        awaiting
            .child_mut()
            .try_wait()
            .expect("poll shared-cache await")
            .is_none(),
        "a remote cache entry alone must not satisfy await"
    );

    let restore = Command::new(binary_path())
        .args(["run", "shared", "-p", "app", "--workspace-root"])
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .env("LUCHTA_SHARED_CACHE", "1")
        .env("LUCHTA_SHARED_CACHE_DIR", shared_cache.path())
        .output()
        .expect("restore shared cache through run");
    assert_success(&restore);
    wait_for_success(awaiting);
    assert!(temp.path().join("packages/app/shared.out").exists());
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_poll_sleep_cleanly() {
    let temp = setup_workspace();
    let stdout_path = temp.path().join("cancel-await.stdout");
    let mut awaiting = spawn_await(&temp, &["build", "-p", "app"], &stdout_path, &[]);
    wait_for_message(&stdout_path, "Waiting for tasks to become current ...");

    let result = unsafe { libc::kill(awaiting.child_mut().id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(result, 0, "send SIGINT to await process");
    wait_for_success(awaiting);
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_build_lock_wait_cleanly() {
    let temp = setup_workspace();
    let mut builder = ChildGuard::new(
        Command::new(binary_path())
            .args(["run", "hold", "-p", "app", "--workspace-root"])
            .arg(temp.path())
            .env("NO_COLOR", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lock-holding build"),
    );
    wait_for(
        Duration::from_secs(20),
        || temp.path().join("held").exists(),
        "builder to hold the build lock",
    );

    let stdout_path = temp.path().join("lock-cancel-await.stdout");
    let mut awaiting = spawn_await(&temp, &["build", "-p", "app"], &stdout_path, &[]);
    thread::sleep(Duration::from_secs(1));
    let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
    assert!(
        !stdout.contains("Waiting for tasks to become current ..."),
        "await must be blocked on the build lock, not polling: {stdout}"
    );
    assert!(
        awaiting
            .child_mut()
            .try_wait()
            .expect("poll lock-waiting await")
            .is_none(),
        "await should still be waiting for the build lock"
    );

    let result = unsafe { libc::kill(awaiting.child_mut().id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(result, 0, "send SIGINT to lock-waiting await process");
    wait_for_success(awaiting);

    fs::write(temp.path().join("release"), "release").expect("release builder");
    wait_for(
        Duration::from_secs(20),
        || {
            builder
                .child_mut()
                .try_wait()
                .expect("poll lock-holding builder")
                .is_some()
        },
        "lock-holding builder to exit",
    );
    assert!(
        builder.wait().success(),
        "lock-holding build should succeed"
    );
}
