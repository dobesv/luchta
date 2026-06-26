//! Integration tests for `--output` flag and build-output behavior.

use std::fs;

use assert_cmd::Command;
use assert_fs::prelude::*;

/// Expected tokens for the emoji done line: `✔ done ⏩ skipped` plus
/// `🌊 waves / waves`.
#[derive(Clone, Copy)]
struct DoneLine {
    done: usize,
    skipped: usize,
    waves: usize,
}

/// Captured stdout/stderr of a `luchta` invocation, with chainable custom
/// assertions for the emoji progress output. Encapsulating the repeated
/// assertion blocks here keeps each test small and cohesive.
struct ProgressOutput {
    label: String,
    stdout: String,
    stderr: String,
}

impl ProgressOutput {
    fn new(label: &str, output: &std::process::Output) -> Self {
        Self {
            label: label.to_string(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn stdout(&self) -> &str {
        &self.stdout
    }

    fn stderr(&self) -> &str {
        &self.stderr
    }

    /// Asserts the run exited successfully.
    fn assert_success(&self, status: std::process::ExitStatus) -> &Self {
        assert!(
            status.success(),
            "{} should succeed, stderr: {}",
            self.label,
            self.stderr
        );
        self
    }

    /// Asserts the emoji done line (`✔ … 🌊 …`) is present and the old
    /// `Done:` text is gone.
    fn assert_done_line(&self, expected: DoneLine) -> &Self {
        let label = &self.label;
        let done_token = format!("✔ {} ⏩ {}", expected.done, expected.skipped);
        let wave_token = format!("🌊 {} / {}", expected.waves, expected.waves);
        assert!(
            self.stdout.contains(&done_token),
            "{label} stdout should contain '{done_token}', got: {}",
            self.stdout
        );
        assert!(
            self.stdout.contains(&wave_token),
            "{label} stdout should contain '{wave_token}', got: {}",
            self.stdout
        );
        assert!(
            !self.stdout.contains("Done:"),
            "{label} stdout should not contain old 'Done:', got: {}",
            self.stdout
        );
        self
    }

    /// Asserts neither stream contains wave-progress lines.
    fn assert_no_wave_progress(&self) -> &Self {
        for (stream, out) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            assert!(
                !out.contains("Wave "),
                "{} {stream} should not emit wave progress, got: {out}",
                self.label
            );
        }
        self
    }

    /// Asserts neither stream contains per-task start/finish/skip spam.
    fn assert_no_per_task_spam(&self) -> &Self {
        for (stream, out) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            for needle in [
                "(no command, skipping)",
                "(skipped due to previous failure)",
            ] {
                assert!(
                    !out.contains(needle),
                    "{} {stream} should not contain per-task spam '{needle}', got: {out}",
                    self.label
                );
            }
        }
        self
    }
}

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

fn run_build(
    temp: &assert_fs::TempDir,
    worker_body: &str,
    tasks_json: &str,
    summary_mode: bool,
    label: &str,
    extra_env: &[(&str, &str)],
) -> ProgressOutput {
    let worker = make_worker_script(temp, "worker.sh", worker_body);
    let worker_path = worker.path().display();
    let config = format!(
        "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"fake\":{{\"command\":\"{worker_path}\"}}}},\"tasks\":{{{tasks_json}}}}}'\n"
    );
    temp.child("luchta-config.sh")
        .write_str(&config)
        .expect("write luchta-config.sh");
    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("run").arg("build");
    if summary_mode {
        cmd.arg("--output").arg("summary");
    }
    cmd.arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("run command");
    let progress = ProgressOutput::new(label, &output);
    progress.assert_success(output.status);
    progress
}

/// Test 1: Summary mode prints ONLY Done line, no wave progress.
#[test]
fn summary_mode_prints_only_done_line() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);

    let out = run_build(
        &temp,
        &shell_worker_body(
            r#"{"type":"done","id":"%s","success":true,"exitCode":0}"#,
            "",
        ),
        r#""build":{"dependsOn":["^build"],"worker":"fake"}"#,
        true,
        "summary mode",
        &[],
    );

    out.assert_done_line(DoneLine {
        done: 2,
        skipped: 0,
        waves: 2,
    })
    .assert_no_wave_progress();

    temp.close().expect("cleanup temp dir");
}

#[test]
fn long_run_default_mode_emits_periodic_progress() {
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

    let out = run_build(
        &temp,
        &shell_worker_body(r#"{"type":"done","id":"%s","exitCode":0}"#, "sleep 1\n"),
        r#""build":{"worker":"fake"}"#,
        false,
        "default run",
        &[("LUCHTA_PROGRESS_INTERVAL_MS", "100")],
    );

    assert!(
        out.stderr().contains("🏃") && out.stderr().contains("✔"),
        "default run should emit periodic progress status line markers on stderr, got: {}",
        out.stderr()
    );
    out.assert_done_line(DoneLine {
        done: 1,
        skipped: 0,
        waves: 1,
    });

    temp.close().expect("cleanup temp dir");
}

#[test]
fn long_run_summary_mode_still_prints_done_line() {
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

    let out = run_build(
        &temp,
        &shell_worker_body(r#"{"type":"done","id":"%s","exitCode":0}"#, "sleep 1\n"),
        r#""build":{"worker":"fake"}"#,
        true,
        "summary run",
        &[("LUCHTA_PROGRESS_INTERVAL_MS", "100")],
    );

    out.assert_done_line(DoneLine {
        done: 1,
        skipped: 0,
        waves: 1,
    });
    assert!(
        !out.stdout().contains("running:"),
        "summary stdout should not contain periodic wave progress, got: {}",
        out.stdout()
    );
    assert!(
        !out.stderr().contains("running:"),
        "summary stderr should not contain periodic wave progress, got: {}",
        out.stderr()
    );

    temp.close().expect("cleanup temp dir");
}

/// Test 2: Default mode success (fast run <5s) prints Done summary, no wave progress, no per-task spam.
#[test]
fn default_mode_fast_run_prints_summary_no_progress() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);

    let out = run_build(
        &temp,
        &shell_worker_body(
            r#"{"type":"done","id":"%s","success":true,"exitCode":0}"#,
            "",
        ),
        r#""build":{"dependsOn":["^build"],"worker":"fake"}"#,
        false,
        "default mode",
        &[],
    );

    out.assert_done_line(DoneLine {
        done: 2,
        skipped: 0,
        waves: 2,
    })
    .assert_no_wave_progress()
    .assert_no_per_task_spam();

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
        "dry-run should NOT contain old 'Done:' summary, got: {stdout}"
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

#[test]
fn dry_run_hides_pruned_connectors_but_keeps_runnable_and_config_errors() {
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

    let pkg_a_dir = temp.child("packages/a");
    fs::create_dir_all(pkg_a_dir.path()).expect("create packages/a dir");
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

    let pkg_b_dir = temp.child("packages/b");
    fs::create_dir_all(pkg_b_dir.path()).expect("create packages/b dir");
    temp.child("packages/b/package.json")
        .write_str(
            r#"{
    "name": "b"
}"#,
        )
        .expect("write packages/b/package.json");

    let worker_script = make_worker_script(
        &temp,
        "selection-worker.sh",
        &shell_worker_body(
            r#"{"type":"done","id":"%s","success":true,"exitCode":0}"#,
            "",
        ),
    );

    let config_content = format!(
        r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"fake":{{"command":"{}"}}}},"tasks":{{"a#build":{{"worker":"fake"}},"check":{{"command":"echo invalid-without-worker"}}}}}}'
"#,
        worker_script.path().display()
    );
    temp.child("luchta-config.sh")
        .write_str(&config_content)
        .expect("write luchta-config.sh");

    let output = Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("check")
        .arg("build")
        .arg("--dry-run")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("run command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "dry-run should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("pruned during resolution"),
        "dry-run should not print pruned note, got: {stdout}"
    );
    assert!(
        !stdout.contains("(no command, would be skipped)"),
        "dry-run should hide skipped connector rows, got: {stdout}"
    );
    assert!(
        !stdout.contains("b#build"),
        "dry-run should hide pruned no-command task rows, got: {stdout}"
    );
    assert!(
        stdout.contains("a#build"),
        "dry-run should keep runnable tasks visible, got: {stdout}"
    );
    assert!(
        stdout.contains("check") && stdout.contains("config error"),
        "dry-run should keep config-error tasks visible, got: {stdout}"
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
        "failing run should NOT print old 'Done:' on stdout, got: {stdout}"
    );
    assert!(
        !stderr.contains("Done:"),
        "failing run should NOT print old 'Done:' on stderr, got: {stderr}"
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
        "interrupt should NOT print old 'Done:', got: {stderr}"
    );

    temp.close().expect("cleanup temp dir");
}
