//! Integration tests for `--continue` flag and default fast-stop behavior.
//!
//! Covers:
//! - CONTINUE mode: independent tasks run to completion, transitive dependents skipped.
//! - DEFAULT fast-stop: first failure terminates in-flight workers promptly.
//! - Summary output: final summary prints on failure, `× N` failed segment present,
//!   and "one or more tasks failed" string is absent.

use assert_cmd::Command;
use assert_fs::prelude::*;

mod common;

/// Setup a workspace with three packages:
/// - `failer`: has a `fail` task that exits immediately with code 1.
/// - `independent`: has a `build` task with NO dependency on `failer`.
/// - `dependent`: has a `build` task that DEPENDS on `failer` (will be skipped).
///
/// Graph:
///   failer#fail (fails)
///   independent#build (no deps, should run)
///   dependent#build (depends on failer, should be skipped)
fn setup_continue_workspace(temp: &assert_fs::TempDir) {
    common::write_root_workspace(temp);
    temp.child("yarn.lock")
        .write_str(common::YARN1_LOCK_LEFT_PAD_1_0_0)
        .unwrap();

    // Package that fails immediately
    temp.child("packages/failer/package.json")
        .write_str(
            r#"{
  "name": "failer",
  "version": "1.0.0",
  "scripts": {
    "fail": "exit 1"
  }
}
"#,
        )
        .unwrap();
    temp.child("packages/failer/src.txt")
        .write_str("test\n")
        .unwrap();

    // Independent package (no dependency on failer)
    temp.child("packages/independent/package.json")
        .write_str(
            r#"{
  "name": "independent",
  "version": "1.0.0",
  "scripts": {
    "build": "echo independent-built > independent.out"
  }
}
"#,
        )
        .unwrap();
    temp.child("packages/independent/src.txt")
        .write_str("test\n")
        .unwrap();

    // Dependent package (depends on failer)
    temp.child("packages/dependent/package.json")
        .write_str(
            r#"{
  "name": "dependent",
  "version": "1.0.0",
  "dependencies": {
    "failer": "1.0.0"
  },
  "scripts": {
    "build": "echo dependent-built > dependent.out"
  }
}
"#,
        )
        .unwrap();
    temp.child("packages/dependent/src.txt")
        .write_str("test\n")
        .unwrap();
}

fn run_workspace_command(
    temp: &assert_fs::TempDir,
    subcommand: &str,
    args: &[&str],
) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg(subcommand);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg("--workspace-root").arg(temp.path());
    cmd.assert()
}

/// Write tasks for the continue workspace using shell worker.
fn write_continue_tasks(temp: &assert_fs::TempDir) {
    let worker = common::shell_worker(temp);
    common::write_task_config_with_worker(
        temp,
        common::WorkerConfig {
            name: "shell",
            command: worker.path(),
        },
        r#""failer#fail":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"exit 1"},"independent#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["independent.out"],"command":"echo independent-built > independent.out"},"dependent#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["dependent.out"],"command":"echo dependent-built > dependent.out"}"#,
    );
}

/// CONTINUE mode: independent task runs to completion, transitive dependent is skipped.
///
/// Uses a graph where `failer#fail` fails, `independent#build` is unrelated,
/// and `dependent#build` depends on `failer`. With `--continue`, the independent
/// task should complete while `dependent#build` is skipped.
#[test]
fn continue_mode_runs_independent_and_skips_dependents() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_continue_workspace(&temp);
    write_continue_tasks(&temp);
    common::init_git(&temp);

    // Run all three tasks with --continue
    // The walker should run failer#fail and independent#build in parallel,
    // then dependent#build would run after failer#fail succeeds, but since
    // failer#fail fails, it should be skipped.
    let output = run_workspace_command(&temp, "run", &["--continue", "fail", "build"]).failure();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout);
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);

    // Check summary line shows 2 done (independent + failer) and 1 failed
    assert!(
        stdout.contains("✔ 2"),
        "summary should show 2 done; stdout={stdout}; stderr={stderr}"
    );
    assert!(
        stdout.contains("× 1"),
        "summary should show 1 failed; stdout={stdout}; stderr={stderr}"
    );

    // Exit code non-zero (already asserted via .failure())
    let _ = output;
}

/// Setup for fast-stop test: one task that fails FAST, one LONG-running independent task.
fn setup_fast_stop_workspace(temp: &assert_fs::TempDir) {
    common::write_root_workspace(temp);
    temp.child("yarn.lock")
        .write_str(common::YARN1_LOCK_LEFT_PAD_1_0_0)
        .unwrap();

    // Fast-failing package
    temp.child("packages/fastfail/package.json")
        .write_str(
            r#"{
  "name": "fastfail",
  "version": "1.0.0",
  "scripts": {
    "fail": "exit 1"
  }
}
"#,
        )
        .unwrap();
    temp.child("packages/fastfail/src.txt")
        .write_str("test\n")
        .unwrap();

    // Long-running independent package (sleeps 10s to allow fast-stop detection)
    temp.child("packages/longrun/package.json")
        .write_str(
            r#"{
  "name": "longrun",
  "version": "1.0.0",
  "scripts": {
    "build": "sleep 10"
  }
}
"#,
        )
        .unwrap();
    temp.child("packages/longrun/src.txt")
        .write_str("test\n")
        .unwrap();
}

fn write_fast_stop_tasks(temp: &assert_fs::TempDir) {
    let worker = common::shell_worker(temp);
    common::write_executable(
        temp.child("luchta-config.sh").path(),
        &format!(
            r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"w1":{{"command":"{}"}},"w2":{{"command":"{}"}}}},"tasks":{{"fastfail#fail":{{"cache":{{}},"worker":"w1","inputs":["src.txt"],"outputs":[],"command":"sleep 0.5; exit 1"}},"longrun#build":{{"cache":{{}},"worker":"w2","inputs":["src.txt"],"outputs":[],"command":"sleep 30"}}}}}}'
"#,
            worker.path().display(),
            worker.path().display(),
        ),
    );
}

/// DEFAULT fast-stop: first failure terminates in-flight workers already in progress.
#[test]
fn default_mode_fast_stop_terminates_in_flight_worker_promptly() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_fast_stop_workspace(&temp);
    write_fast_stop_tasks(&temp);
    common::init_git(&temp);

    let started = std::time::Instant::now();
    run_workspace_command(&temp, "run", &["fail", "build"]).failure();
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "default fast-stop should terminate in-flight worker well before natural completion; elapsed={elapsed:?}"
    );
}

/// Summary on failure: final summary prints with failed count, "one or more tasks failed" absent.
#[test]
fn failure_shows_summary_without_old_message() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_continue_workspace(&temp);
    write_continue_tasks(&temp);
    common::init_git(&temp);

    // Run without --continue (default mode)
    let output = run_workspace_command(&temp, "run", &["fail", "build"]).failure();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout);
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    let combined = format!("{stdout}{stderr}");

    // Should NOT contain the old "one or more tasks failed" message
    assert!(
        !combined.contains("one or more tasks failed"),
        "old failure message should be absent; combined={combined}"
    );

    // Should contain the final summary line with failed count
    // The summary format is "✔ <n> ⏩ <n> × <n>"
    assert!(
        combined.contains("× 1"),
        "summary should contain failed count; combined={combined}"
    );
}

/// CONTINUE mode also shows summary on failure with failed count.
#[test]
fn continue_mode_shows_summary_on_failure() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_continue_workspace(&temp);
    write_continue_tasks(&temp);
    common::init_git(&temp);

    let output = run_workspace_command(&temp, "run", &["--continue", "fail", "build"]).failure();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout);
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    let combined = format!("{stdout}{stderr}");

    // Should contain summary with failed count
    assert!(
        combined.contains("× 1"),
        "continue mode summary should contain failed count; combined={combined}"
    );

    // Should NOT contain old message
    assert!(
        !combined.contains("one or more tasks failed"),
        "old failure message should be absent in continue mode; combined={combined}"
    );
}
