//! Integration tests for `--output` flag and build-output behavior.

use std::fs;

use assert_cmd::Command;
use assert_fs::prelude::*;

fn make_worker_script(
    temp: &assert_fs::TempDir,
    name: &str,
    body: &str,
) -> assert_fs::fixture::ChildPath {
    let script = temp.child(name);
    script.write_str(body).expect("write worker script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(script.path(), perms).expect("chmod worker script");
    }
    script
}

fn shell_worker_body(done_json: &str, extra_json: &str) -> String {
    format!(
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"resolveTask"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
      ;;
    *'"type":"run"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      {extra_json}      printf '{done_json}\n' "$id"
      ;;
  esac
done
"#
    )
}

fn setup_workspace(temp: &assert_fs::TempDir) {
    let root_pkg = temp.child("package.json");
    root_pkg
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    let pkg_a_dir = temp.child("packages/a");
    fs::create_dir_all(pkg_a_dir.path()).expect("create packages/a dir");
    let pkg_a = temp.child("packages/a/package.json");
    pkg_a
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "echo built-a"
    }
}"#,
        )
        .expect("write packages/a/package.json");

    let pkg_b_dir = temp.child("packages/b");
    fs::create_dir_all(pkg_b_dir.path()).expect("create packages/b dir");
    let pkg_b = temp.child("packages/b/package.json");
    pkg_b
        .write_str(
            r#"{
    "name": "b",
    "scripts": {
        "build": "echo built-b"
    },
    "dependencies": {
        "a": "workspace:*"
    }
}"#,
        )
        .expect("write packages/b/package.json");
}

/// Test 1: Summary mode prints ONLY Done line, no wave progress.
#[test]
fn summary_mode_prints_only_done_line() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);

    let worker_script = make_worker_script(
        &temp,
        "summary-worker.sh",
        &shell_worker_body(
            r#"{"type":"done","id":"%s","success":true,"exitCode":0}"#,
            "",
        ),
    );

    let worker_path = worker_script.path().display();
    let config_content = format!(
        r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"fake":{{"command":"{worker_path}"}}}},"tasks":{{"build":{{"dependsOn":["^build"],"worker":"fake"}}}}}}'
"#,
    );
    temp.child("luchta-config.sh")
        .write_str(&config_content)
        .expect("write luchta-config.sh");

    let output = Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--output")
        .arg("summary")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Summary mode: stdout contains Done, no Wave progress
    assert!(
        stdout.contains("Done:"),
        "summary mode stdout should contain 'Done:', got: {stdout}"
    );
    assert!(
        !stdout.contains("Wave "),
        "summary mode stdout should NOT contain 'Wave ', got: {stdout}"
    );
    assert!(
        !stderr.contains("Wave "),
        "summary mode stderr should NOT contain 'Wave ', got: {stderr}"
    );

    // Success
    assert!(
        output.status.success(),
        "run should succeed, stderr: {stderr}"
    );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn long_run_emits_periodic_progress_only_in_default_mode() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");

    temp.child("package.json")
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    let pkg_dir = temp.child("packages/a");
    fs::create_dir_all(pkg_dir.path()).expect("create packages/a dir");
    temp.child("packages/a/package.json")
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "echo ignored"
    }
}"#,
        )
        .expect("write packages/a/package.json");

    let worker_script = make_worker_script(
        &temp,
        "slow-worker.sh",
        // The task sleeps ~1s so the run outlasts the (test-shortened, 100ms)
        // progress interval and at least one progress tick fires. The extra
        // statement runs before the worker emits `done`; it must be a complete
        // line (trailing newline) or it merges into the done printf.
        &shell_worker_body(r#"{"type":"done","id":"%s","exitCode":0}"#, "sleep 1\n"),
    );

    let worker_path = worker_script.path().display();
    let config_content = format!(
        r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"fake":{{"command":"{worker_path}"}}}},"tasks":{{"build":{{"worker":"fake"}}}}}}'
"#,
    );
    temp.child("luchta-config.sh")
        .write_str(&config_content)
        .expect("write luchta-config.sh");

    let default_output = Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        // Shorten the progress interval so the ~1s task triggers a tick quickly.
        .env("LUCHTA_PROGRESS_INTERVAL_MS", "100")
        .output()
        .expect("run default command");

    let default_stdout = String::from_utf8_lossy(&default_output.stdout);
    let default_stderr = String::from_utf8_lossy(&default_output.stderr);
    assert!(
        default_output.status.success(),
        "default run should succeed, stderr: {default_stderr}"
    );
    assert!(
        default_stderr.contains("running:"),
        "default run should emit periodic progress on stderr, got: {default_stderr}"
    );
    assert!(
        default_stdout.contains("Done:"),
        "default run should still print final Done line, got: {default_stdout}"
    );

    let summary_output = Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--output")
        .arg("summary")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        // Same short interval: summary mode must STILL emit no progress lines.
        .env("LUCHTA_PROGRESS_INTERVAL_MS", "100")
        .output()
        .expect("run summary command");

    let summary_stdout = String::from_utf8_lossy(&summary_output.stdout);
    let summary_stderr = String::from_utf8_lossy(&summary_output.stderr);
    assert!(
        summary_output.status.success(),
        "summary run should succeed, stderr: {summary_stderr}"
    );
    assert!(
        summary_stdout.contains("Done:"),
        "summary run should print final Done line, got: {summary_stdout}"
    );
    assert!(
        !summary_stdout.contains("running:"),
        "summary stdout should not contain periodic wave progress, got: {summary_stdout}"
    );
    assert!(
        !summary_stderr.contains("running:"),
        "summary stderr should not contain periodic wave progress, got: {summary_stderr}"
    );

    temp.close().expect("cleanup temp dir");
}

/// Test 2: Default mode success (fast run <5s) prints Done summary, no wave progress, no per-task spam.
#[test]
fn default_mode_fast_run_prints_summary_no_progress() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);

    let worker_script = make_worker_script(
        &temp,
        "default-worker.sh",
        &shell_worker_body(
            r#"{"type":"done","id":"%s","success":true,"exitCode":0}"#,
            "",
        ),
    );

    let worker_path = worker_script.path().display();
    let config_content = format!(
        r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"fake":{{"command":"{worker_path}"}}}},"tasks":{{"build":{{"dependsOn":["^build"],"worker":"fake"}}}}}}'
"#,
    );
    temp.child("luchta-config.sh")
        .write_str(&config_content)
        .expect("write luchta-config.sh");

    let output = Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Done summary on stdout
    assert!(
        stdout.contains("Done:"),
        "default mode stdout should contain 'Done:', got: {stdout}"
    );
    // Fast run (<5s): no periodic wave progress (either stream)
    assert!(
        !stderr.contains("Wave "),
        "fast run should not emit wave progress on stderr, got: {stderr}"
    );
    assert!(
        !stdout.contains("Wave "),
        "fast run should not emit wave progress on stdout, got: {stdout}"
    );
    // No per-task start/finish spam
    assert!(
        !stdout.contains("(no command, skipping)"),
        "should not contain per-task spam '(no command, skipping)', got: {stdout}"
    );
    assert!(
        !stdout.contains("(skipped due to previous failure)"),
        "should not contain per-task spam '(skipped due to previous failure)', got: {stdout}"
    );
    assert!(
        !stderr.contains("(no command, skipping)"),
        "should not contain per-task spam on stderr, got: {stderr}"
    );
    assert!(
        !stderr.contains("(skipped due to previous failure)"),
        "should not contain per-task spam on stderr, got: {stderr}"
    );

    // Success
    assert!(
        output.status.success(),
        "run should succeed, stderr: {stderr}"
    );

    temp.close().expect("cleanup temp dir");
}

/// Test 3: dry-run output unchanged - wave plan, not Done summary.
#[test]
fn dry_run_output_is_wave_plan_not_summary() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);

    let executed = temp.child("executed.log");
    executed.write_str("").expect("init executed marker");

    let worker_script = make_worker_script(
        &temp,
        "dry-run-worker.sh",
        &shell_worker_body(
            r#"{"type":"done","id":"%s","success":true,"exitCode":0}"#,
            &format!("      printf ran >> '{}'\n", executed.path().display()),
        ),
    );

    let worker_path = worker_script.path().display();
    let config_content = format!(
        r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"yarn":{{"command":"{worker_path}"}}}},"tasks":{{"build":{{"dependsOn":["^build"],"worker":"yarn"}}}}}}'
"#,
    );
    temp.child("luchta-config.sh")
        .write_str(&config_content)
        .expect("write luchta-config.sh");

    let output = Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--dry-run")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("run command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // dry-run shows wave plan
    assert!(
        stdout.contains("dry-run:"),
        "dry-run stdout should contain 'dry-run:', got: {stdout}"
    );
    assert!(
        stdout.contains("Wave 1:"),
        "dry-run stdout should contain 'Wave 1:', got: {stdout}"
    );
    assert!(
        stdout.contains("Wave 2:"),
        "dry-run stdout should contain 'Wave 2:', got: {stdout}"
    );
    assert!(
        stdout.contains("a#build"),
        "dry-run stdout should contain 'a#build', got: {stdout}"
    );
    assert!(
        stdout.contains("b#build"),
        "dry-run stdout should contain 'b#build', got: {stdout}"
    );
    // dry-run does NOT print Done summary
    assert!(
        !stdout.contains("Done:"),
        "dry-run should NOT contain 'Done:' summary, got: {stdout}"
    );

    // No execution
    let ran = fs::read_to_string(executed.path()).expect("read executed marker");
    assert!(
        ran.is_empty(),
        "dry-run must not execute tasks, got: {ran:?}"
    );

    // Success
    assert!(
        output.status.success(),
        "dry-run should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    temp.close().expect("cleanup temp dir");
}

/// Test 5: Failure dumps captured output, exits 1, no Done line.
#[test]
fn failing_task_exits_nonzero_no_done_line() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");

    // Single package with a task that fails
    temp.child("package.json")
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    let pkg_dir = temp.child("packages/a");
    fs::create_dir_all(pkg_dir.path()).expect("create packages/a dir");
    temp.child("packages/a/package.json")
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "echo built-a"
    }
}"#,
        )
        .expect("write packages/a/package.json");

    // Worker that fails with proper error format
    let worker_script = make_worker_script(
        &temp,
        "failing-worker.sh",
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"resolveTask"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      printf '{"type":"resolved","id":"%s","result":{"decision":"accept"}}\n' "$id"
      ;;
    *'"type":"run"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      printf '{"type":"log","stream":"stdout","id":"%s","message":"task-failed-output"}\n' "$id"
      printf '{"type":"done","id":"%s","success":false,"exitCode":1}\n' "$id"
      ;;
  esac
done
"#,
    );

    let worker_path = worker_script.path().display();
    let config_content = format!(
        r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"fake":{{"command":"{worker_path}"}}}},"tasks":{{"build":{{"worker":"fake"}}}}}}'
"#,
    );
    temp.child("luchta-config.sh")
        .write_str(&config_content)
        .expect("write luchta-config.sh");

    let output = Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    // Failure: no Done line
    assert!(
        !stdout.contains("Done:"),
        "failing run should NOT print 'Done:' on stdout, got: {stdout}"
    );
    assert!(
        !stderr.contains("Done:"),
        "failing run should NOT print 'Done:' on stderr, got: {stderr}"
    );
    // Failure: non-zero exit
    assert!(
        !output.status.success(),
        "failing task should exit non-zero"
    );
    // Failure output contains failure indication
    assert!(
        combined.contains("failed") || combined.contains("failed-output"),
        "failure should indicate failure in output, combined: {combined}"
    );

    temp.close().expect("cleanup temp dir");
}

/// Test 6: Interrupt signal (Linux-gated) prints diagnostic and exits non-zero.
///
/// NOTE: This test is intentionally IGNORED because it is inherently flaky in CI environments.
/// The test requires spawning a child process with a long-running worker, waiting for it to
/// start, then sending SIGTERM. The shell-based worker script often fails to initialize
/// properly in the test harness (worker crashes during resolution), making this test unreliable.
/// The interrupt behavior is covered by the existing `interrupt_shuts_down_promptly_without_orphans`
/// test in `worker_integration.rs` which uses the compiled `luchta-yarn-worker` binary instead
/// of a shell script.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "flaky: shell worker often crashes during test spawn; covered by worker_integration::interrupt_shuts_down_promptly_without_orphans"]
fn interrupt_prints_diagnostic_and_exits_nonzero() {
    use std::process::{Command as StdCommand, Stdio};
    use std::time::{Duration, Instant};

    let temp = assert_fs::TempDir::new().expect("create temp dir");

    // Single package
    temp.child("package.json")
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    let pkg_dir = temp.child("packages/a");
    fs::create_dir_all(pkg_dir.path()).expect("create packages/a dir");
    temp.child("packages/a/package.json")
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "echo built-a"
    }
}"#,
        )
        .expect("write packages/a/package.json");

    // Worker that sleeps for a long time (simulates slow task)
    let worker_script = temp.child("sleepy-worker.sh");
    worker_script
        .write_str(
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"resolveTask"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      printf '{"type":"resolved","id":"%s","result":{"decision":"accept"}}\n' "$id"
      ;;
    *'"type":"run"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      printf '{"type":"log","stream":"stdout","id":"%s","message":"starting"}\n' "$id"
      # Marker file to signal task started
      printf 'started\n' > /tmp/luchta-test-interrupt-marker
      sleep 60
      printf '{"type":"done","id":"%s","success":true,"exitCode":0}\n' "$id"
      ;;
  esac
done
"#,
        )
        .expect("write sleep worker script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(worker_script.path(), fs::Permissions::from_mode(0o755))
            .expect("chmod sleep worker");
    }

    let worker_path = worker_script.path().display();
    let config_content = format!(
        r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"sleepy":{{"command":"{worker_path}"}}}},"tasks":{{"build":{{"worker":"sleepy"}}}}}}'
"#,
    );
    temp.child("luchta-config.sh")
        .write_str(&config_content)
        .expect("write luchta-config.sh");

    let stderr_log = temp.child("luchta.stderr");
    let stderr_file = fs::File::create(stderr_log.path()).expect("create stderr log");

    // Spawn luchta
    let binary_path = assert_cmd::cargo::cargo_bin("luchta");
    let mut child = StdCommand::new(binary_path)
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn luchta");

    // Wait for task to start (marker file)
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if fs::metadata("/tmp/luchta-test-interrupt-marker").is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Send SIGTERM
    let pid = child.id() as i32;
    // SAFETY: sending SIGTERM to a child we just spawned
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    // Wait for exit with timeout
    let wait_start = Instant::now();
    let status;
    loop {
        match child.try_wait().expect("try_wait luchta") {
            Some(s) => {
                status = s;
                break;
            }
            None => {
                if wait_start.elapsed() > Duration::from_secs(10) {
                    // Kill if still running
                    let _ = child.kill();
                    status = child.wait().expect("wait after kill");
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    let stderr = fs::read_to_string(stderr_log.path()).unwrap_or_default();

    // Clean up marker
    let _ = fs::remove_file("/tmp/luchta-test-interrupt-marker");

    // Interrupted runs exit non-zero
    assert!(
        !status.success(),
        "interrupted run should exit non-zero, stderr: {stderr}"
    );

    // Diagnostic printed to stderr
    assert!(
        stderr.contains("Interrupted by SIGTERM:"),
        "interrupt should print 'Interrupted by SIGTERM:' on stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("RSS:"),
        "interrupt should print RSS on stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("tasks running after"),
        "interrupt should print 'tasks running after' on stderr, got: {stderr}"
    );

    // No Done line
    assert!(
        !stderr.contains("Done:"),
        "interrupt should NOT print 'Done:', got: {stderr}"
    );

    temp.close().expect("cleanup temp dir");
}
