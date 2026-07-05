//! Cross-package input dependency integration tests.
//!
//! Tests for prefixed input patterns: `#path` (repo root), `pkg#path` (named package),
//! `^glob` (direct upstream), `^^glob` (transitive upstream).

use assert_cmd::Command;
use assert_fs::prelude::*;

mod common;

use common::{init_git, write_counter_task_config, write_root_workspace};

fn run_luchta(temp: &assert_fs::TempDir, task: &str) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.arg("run")
        .arg(task)
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
}

// =============================================================================
// Setup fixtures
// =============================================================================

// =============================================================================
// Security: invalid / escaping input patterns must HARD-FAIL the task
// (resolve-time declared / worker-modified inputs must not silently resolve or skip).
// =============================================================================

/// Build a single-package `app#build` workspace whose declared `inputs` contain
/// the given (deliberately invalid) prefixed pattern, then run it. Returns the
/// run assertion so callers can assert failure.
fn run_declared_input(temp: &assert_fs::TempDir, bad_input: &str) -> assert_cmd::assert::Assert {
    write_root_workspace(temp);
    temp.child("yarn.lock").write_str("").unwrap();
    write_counter_task_config(
        temp,
        &format!(
            r##""app#build":{{"cache":{{}},"worker":"shell","inputs":["{bad_input}"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}}"##
        ),
    );
    temp.child("packages/app").create_dir_all().unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "build": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(temp);
    run_luchta(temp, "build")
}

#[test]
fn declared_input_path_escape_fails_task() {
    let temp = assert_fs::TempDir::new().unwrap();
    // `#../...` escapes the repo root via `..`.
    run_declared_input(&temp, "#../escape.txt").failure();
}

#[test]
fn declared_input_unknown_package_fails_task() {
    let temp = assert_fs::TempDir::new().unwrap();
    // References a package that does not exist in the workspace graph.
    run_declared_input(&temp, "nonexistent#file.txt").failure();
}
