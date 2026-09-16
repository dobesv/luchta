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

fn setup_package_listing_workspace() -> assert_fs::TempDir {
    let temp = assert_fs::TempDir::new().unwrap();
    common::setup_workspace(&temp);

    temp.child("packages/c").create_dir_all().unwrap();
    temp.child("packages/c/package.json")
        .write_str(
            r#"{
  "name": "c",
  "version": "1.0.0",
  "scripts": { "build": "echo build-c" }
}"#,
        )
        .unwrap();
    temp.child("packages/d").create_dir_all().unwrap();
    temp.child("packages/d/package.json")
        .write_str(
            r#"{
  "name": "d",
  "version": "1.0.0"
}"#,
        )
        .unwrap();

    let worker = common::shell_worker(&temp);
    common::write_task_config_with_named_worker(
        &temp,
        "sh",
        worker.path(),
        "\"a#build\":{\"worker\":\"sh\"},\"a#test\":{\"worker\":\"sh\"},\"b#build\":{\"dependsOn\":[\"^build\"],\"worker\":\"sh\"},\"c#build\":{\"worker\":\"sh\"}",
    );
    common::git_commit_all(temp.path(), "add list package fixture");
    temp
}

fn successful_stdout(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "list failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("list stdout should be UTF-8")
}

fn task_headers(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|line| !line.starts_with("  "))
        .collect()
}

fn assert_no_run_noop_message(stdout: &str) {
    assert!(
        !stdout.contains("nothing to run"),
        "unexpected run message: {stdout}"
    );
    assert!(
        !stdout.contains("No packages changed"),
        "unexpected run message: {stdout}"
    );
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

#[test]
fn list_since_filters_tasks_to_changed_package_and_dependents() {
    let temp = setup_package_listing_workspace();
    temp.child("packages/a/foo.ts")
        .write_str("export const changed = true;\n")
        .unwrap();

    let stdout = successful_stdout(run_list(&temp, &["--since", "HEAD"]));

    assert_eq!(task_headers(&stdout), vec!["a#build", "a#test", "b#build"]);
    assert!(!stdout.contains("c#build"));
    temp.close().unwrap();
}

#[test]
fn list_packages_text_is_sorted_unique_and_uses_real_package_names() {
    let temp = setup_package_listing_workspace();

    let stdout = successful_stdout(run_list(&temp, &["--packages"]));

    assert_eq!(stdout.lines().collect::<Vec<_>>(), vec!["a", "b", "c"]);
    assert!(!stdout.contains("//root"));
    temp.close().unwrap();
}

#[test]
fn list_packages_json_has_sorted_names_and_workspace_relative_paths() {
    let temp = setup_package_listing_workspace();

    let stdout = successful_stdout(run_list(&temp, &["--packages", "--json"]));
    let parsed: Value = serde_json::from_str(&stdout).expect("valid package JSON");

    assert_eq!(
        parsed,
        serde_json::json!([
            {"package": "a", "path": "packages/a"},
            {"package": "b", "path": "packages/b"},
            {"package": "c", "path": "packages/c"}
        ])
    );
    for listing in parsed.as_array().unwrap() {
        let path = listing["path"].as_str().unwrap();
        assert!(
            !std::path::Path::new(path).is_absolute(),
            "absolute path: {path}"
        );
    }
    temp.close().unwrap();
}

#[test]
fn list_packages_task_filter_includes_only_packages_defining_task() {
    let temp = setup_package_listing_workspace();

    let stdout = successful_stdout(run_list(&temp, &["--packages", "test"]));

    assert_eq!(stdout.lines().collect::<Vec<_>>(), vec!["a"]);
    temp.close().unwrap();
}

#[test]
fn list_packages_package_glob_includes_only_matching_package() {
    let temp = setup_package_listing_workspace();

    let stdout = successful_stdout(run_list(&temp, &["--packages", "-p", "a"]));

    assert_eq!(stdout.lines().collect::<Vec<_>>(), vec!["a"]);
    temp.close().unwrap();
}

#[test]
fn list_packages_since_returns_changed_package_and_dependents() {
    let temp = setup_package_listing_workspace();
    temp.child("packages/a/foo.ts")
        .write_str("export const changed = true;\n")
        .unwrap();

    let stdout = successful_stdout(run_list(&temp, &["--packages", "--since", "HEAD"]));

    assert_eq!(stdout.lines().collect::<Vec<_>>(), vec!["a", "b"]);
    assert!(!stdout.lines().any(|package| package == "c"));
    temp.close().unwrap();
}

#[test]
fn list_packages_top_level_maps_root_sentinel_to_real_name_and_dot_path() {
    let temp = setup_list_workspace_with_root_task();

    let text = successful_stdout(run_list(&temp, &["-T", "--packages"]));
    assert_eq!(text, "root\n");
    assert!(!text.contains("//root"));

    let json = successful_stdout(run_list(&temp, &["-T", "--packages", "--json"]));
    assert_eq!(
        serde_json::from_str::<Value>(&json).expect("valid root package JSON"),
        serde_json::json!([{"package": "root", "path": "."}])
    );
    assert!(!json.contains("//root"));
    temp.close().unwrap();
}

#[test]
fn list_since_empty_affected_set_is_silent_for_task_and_package_modes() {
    let temp = setup_package_listing_workspace();

    for args in [
        &["--since", "HEAD"][..],
        &["--packages", "--since", "HEAD"][..],
    ] {
        let stdout = successful_stdout(run_list(&temp, args));
        assert_eq!(stdout, "");
        assert_no_run_noop_message(&stdout);
    }

    for args in [
        &["--since", "HEAD", "--json"][..],
        &["--packages", "--since", "HEAD", "--json"][..],
    ] {
        let stdout = successful_stdout(run_list(&temp, args));
        assert_eq!(
            serde_json::from_str::<Value>(&stdout).expect("valid empty JSON"),
            serde_json::json!([])
        );
        assert_no_run_noop_message(&stdout);
    }

    temp.close().unwrap();
}

#[test]
fn list_packages_excludes_package_without_selected_task() {
    let temp = setup_package_listing_workspace();

    let stdout = successful_stdout(run_list(&temp, &["--packages", "build"]));

    assert_eq!(stdout.lines().collect::<Vec<_>>(), vec!["a", "b", "c"]);
    assert!(!stdout.lines().any(|package| package == "d"));
    temp.close().unwrap();
}

#[test]
fn list_literal_task_since_empty_affected_set_returns_empty_output() {
    let temp = setup_package_listing_workspace();

    let text = successful_stdout(run_list(&temp, &["build", "--since", "HEAD"]));
    assert_eq!(text, "");
    assert_no_run_noop_message(&text);

    let json = successful_stdout(run_list(&temp, &["build", "--since", "HEAD", "--json"]));
    assert_eq!(json, "[]\n");
    assert_no_run_noop_message(&json);

    temp.close().unwrap();
}

#[test]
fn list_packages_literal_task_since_empty_affected_set_returns_empty_output() {
    let temp = setup_package_listing_workspace();

    let text = successful_stdout(run_list(&temp, &["--packages", "build", "--since", "HEAD"]));
    assert_eq!(text, "");
    assert_no_run_noop_message(&text);

    let json = successful_stdout(run_list(
        &temp,
        &["--packages", "build", "--since", "HEAD", "--json"],
    ));
    assert_eq!(json, "[]\n");
    assert_no_run_noop_message(&json);

    temp.close().unwrap();
}

#[test]
fn list_literal_missing_task_since_preserves_not_found_error() {
    let temp = setup_package_listing_workspace();

    let output = run_list(&temp, &["nonexistent", "--since", "HEAD"]);

    assert!(!output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("task 'nonexistent' not found in task graph"),
        "unexpected error: {stderr}"
    );
    temp.close().unwrap();
}

#[test]
fn list_literal_task_since_changed_package_keeps_affected_tasks_and_packages() {
    let temp = setup_package_listing_workspace();
    temp.child("packages/a/foo.ts")
        .write_str("export const changed = true;\n")
        .unwrap();

    let tasks = successful_stdout(run_list(&temp, &["build", "--since", "HEAD"]));
    assert_eq!(task_headers(&tasks), vec!["a#build", "b#build"]);

    let packages = successful_stdout(run_list(&temp, &["--packages", "build", "--since", "HEAD"]));
    assert_eq!(packages.lines().collect::<Vec<_>>(), vec!["a", "b"]);

    temp.close().unwrap();
}
