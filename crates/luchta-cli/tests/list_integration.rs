//! Integration tests for `luchta list` command.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;
use serde_json::Value;

mod common;

const LIST_TASK: &str = r#""app#build":{"cache":{},"worker":"shell","description":"Build app bundle","inputs":["src.txt"],"outputs":["out.txt"],"command":"cat src.txt > out.txt"}"#;
const LIST_FILES: &[(&str, &str)] = &[("packages/app/src.txt", "test\n")];
const LIST_WITH_ROOT_TASKS: &str = r##""#rootbuild":{"cache":{},"worker":"shell","description":"Build root task","inputs":["root.txt"],"outputs":["root.out"],"command":"cat root.txt > root.out"},"app#build":{"cache":{},"worker":"shell","description":"Build app bundle","inputs":["src.txt"],"outputs":["out.txt"],"command":"cat src.txt > out.txt"}"##;
const LIST_WITH_ROOT_FILES: &[(&str, &str)] =
    &[("root.txt", "root\n"), ("packages/app/src.txt", "test\n")];

fn setup_list_workspace() -> assert_fs::TempDir {
    let temp = assert_fs::TempDir::new().unwrap();
    common::setup_pkgbuild_counter_workspace(
        &temp,
        common::YARN1_LOCK_LEFT_PAD_1_0_0,
        LIST_TASK,
        LIST_FILES,
    );
    temp
}

fn setup_list_workspace_with_root_task() -> assert_fs::TempDir {
    let temp = assert_fs::TempDir::new().unwrap();
    common::setup_pkgbuild_counter_workspace(
        &temp,
        common::YARN1_LOCK_LEFT_PAD_1_0_0,
        LIST_WITH_ROOT_TASKS,
        LIST_WITH_ROOT_FILES,
    );
    temp.child("package.json")
        .write_str(
            r#"{
  "name": "root",
  "private": true,
  "workspaces": ["packages/*"],
  "scripts": {
    "rootbuild": "echo ignored"
  }
}"#,
        )
        .unwrap();
    common::git_commit_all(temp.path(), "add root task");
    temp
}

fn run_list(temp: &assert_fs::TempDir, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("list");
    cmd.args(args);
    cmd.arg("--workspace-root").arg(temp.path());
    cmd.output().expect("failed to run list")
}

#[test]
fn list_prints_human_readable_non_default_fields() {
    let temp = setup_list_workspace();

    let output = run_list(&temp, &["-p", "app", "build"]);
    assert!(
        output.status.success(),
        "list failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("app#build"),
        "expected task header: {stdout}"
    );
    assert!(
        stdout.contains("  description: Build app bundle"),
        "expected description line: {stdout}"
    );
    assert!(
        stdout.contains("  worker: shell"),
        "expected worker line: {stdout}"
    );
    assert!(
        stdout.contains("  inputs: [src.txt]"),
        "expected inputs line: {stdout}"
    );
    assert!(
        stdout.contains("  outputs: [out.txt]"),
        "expected outputs line: {stdout}"
    );
    assert!(
        stdout.contains("  command: cat src.txt > out.txt"),
        "expected command line: {stdout}"
    );
    assert!(
        !stdout.contains("  weight:"),
        "default weight should be omitted: {stdout}"
    );
    assert!(
        !stdout.contains("  dependencies:"),
        "default dependencies should be omitted: {stdout}"
    );
    assert!(
        !stdout.contains("  env:"),
        "empty env should be omitted: {stdout}"
    );
    assert!(
        !stdout.contains("  depends_on:"),
        "empty depends_on should be omitted: {stdout}"
    );
}

#[test]
fn list_no_args_lists_all_tasks() {
    let temp = setup_list_workspace();

    let output = run_list(&temp, &[]);
    assert!(
        output.status.success(),
        "list with no args failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("app#build"),
        "expected all-tasks listing to include app task: {stdout}"
    );
}

#[test]
fn list_package_only_lists_matching_package_tasks() {
    let temp = setup_list_workspace();

    let output = run_list(&temp, &["-p", "app"]);
    assert!(
        output.status.success(),
        "list package-only failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("app#build"),
        "expected package-only listing to include app task: {stdout}"
    );
}

#[test]
fn list_top_level_only_lists_root_task_with_single_hash_header() {
    let temp = setup_list_workspace_with_root_task();

    let output = run_list(&temp, &["-T"]);
    assert!(
        output.status.success(),
        "list top-level failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("#rootbuild"),
        "expected top-level listing to include root task: {stdout}"
    );
    assert!(
        !stdout.contains("##rootbuild"),
        "root task header must use single hash: {stdout}"
    );
    assert!(
        !stdout.contains("app#build"),
        "top-level only should exclude package tasks: {stdout}"
    );
}

#[test]
fn list_json_output_is_parseable_and_contains_expected_fields() {
    let temp = setup_list_workspace();

    let output = run_list(&temp, &["--json", "-p", "app", "build"]);
    assert!(
        output.status.success(),
        "list --json failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).expect("valid json output");
    let tasks = parsed.as_array().expect("json output should be an array");
    assert_eq!(tasks.len(), 1, "expected single listed task: {stdout}");

    let task = &tasks[0];
    assert_eq!(task["task_id"], "app#build");
    assert_eq!(task["package"], "app");
    assert_eq!(task["task"], "build");
    assert_eq!(task["description"], "Build app bundle");
    assert_eq!(task["worker"], "shell");
    assert_eq!(task["command"], "cat src.txt > out.txt");
    assert_eq!(task["inputs"], serde_json::json!(["src.txt"]));
    assert_eq!(task["outputs"], serde_json::json!(["out.txt"]));
    assert_eq!(task["weight"], 1);
    assert_eq!(task["cache"], serde_json::json!({"nonce": null}));
}

#[test]
fn list_unmatched_task_selection_errors() {
    let temp = setup_list_workspace();

    let output = run_list(&temp, &["-p", "app", "nonexistent-xyz"]);
    assert!(
        !output.status.success(),
        "list with unmatched task should fail: stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("task 'nonexistent-xyz' not found in task graph"),
        "expected unmatched task error: {stderr}"
    );
}

#[test]
fn list_errors_for_unmatched_package() {
    let temp = setup_list_workspace();

    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("list");
    cmd.arg("-p").arg("nonexistent");
    cmd.arg("build");
    cmd.arg("--workspace-root").arg(temp.path());

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("No packages matched"));
}
