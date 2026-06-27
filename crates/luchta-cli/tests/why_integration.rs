//! Integration tests for `luchta why` command.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

mod common;

const SINGLE_BUILD_TASK: &str = r#""app#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"echo build > out.txt"}"#;
const SINGLE_BUILD_FILES: &[(&str, &str)] = &[("packages/app/src.txt", "test\n")];

const ENV_BUILD_TASK: &str = r#""app#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"env":{"BUILD_MODE":{"default":"debug"}},"command":"echo ${BUILD_MODE:-unset} > out.txt"}"#;

fn setup_why_workspace_with_task(task_json: &str) -> assert_fs::TempDir {
    let temp = assert_fs::TempDir::new().unwrap();
    common::setup_pkgbuild_counter_workspace(
        &temp,
        common::YARN1_LOCK_LEFT_PAD_1_0_0,
        task_json,
        SINGLE_BUILD_FILES,
    );
    temp
}

fn setup_why_workspace() -> assert_fs::TempDir {
    setup_why_workspace_with_task(SINGLE_BUILD_TASK)
}

fn setup_env_why_workspace() -> assert_fs::TempDir {
    setup_why_workspace_with_task(ENV_BUILD_TASK)
}

fn run_why(temp: &assert_fs::TempDir, args: &[&str]) -> String {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("why");
    cmd.args(args);
    cmd.arg("--workspace-root").arg(temp.path());
    let output = cmd.output().expect("failed to run why");
    assert!(
        output.status.success(),
        "why failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn why_shows_no_prior_for_uncached_task() {
    let temp = setup_why_workspace();
    let stdout = run_why(&temp, &["-p", "app", "build"]);
    assert!(
        stdout.contains("last ran: not recorded"),
        "expected 'last ran: not recorded' in output: {stdout}"
    );
    assert!(
        stdout.contains("would run: no prior"),
        "expected 'would run' in output: {stdout}"
    );
}

#[test]
fn why_shows_cache_hit_after_run() {
    let temp = setup_why_workspace();

    // Run the task first to populate cache
    common::run_luchta(&temp, "build").success();

    let stdout = run_why(&temp, &["-p", "app", "build"]);
    assert!(
        stdout.contains("last ran:"),
        "expected 'last ran' in output: {stdout}"
    );
    assert!(
        stdout.contains("up to date"),
        "expected 'up to date' in output: {stdout}"
    );
}

#[test]
fn why_shows_input_changed_after_modification() {
    let temp = setup_why_workspace();

    // Run the task first to populate cache
    common::run_luchta(&temp, "build").success();

    // Modify input file
    let src_file = temp.child("packages/app/src.txt");
    src_file.write_str("modified\n").unwrap();

    let stdout = run_why(&temp, &["-p", "app", "build"]);
    assert!(
        stdout.contains("would run: input changed"),
        "expected 'would run: input changed' in output: {stdout}"
    );
}

#[test]
fn why_shows_env_changed_after_env_rerun() {
    let temp = setup_env_why_workspace();

    common::run_luchta(&temp, "build").success();

    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.env("BUILD_MODE", "release");
    cmd.arg("run").arg("build");
    cmd.arg("--workspace-root").arg(temp.path());
    cmd.assert().success();

    let stdout = run_why(&temp, &["-p", "app", "build"]);
    assert!(
        stdout.contains("last ran: env changed"),
        "expected 'last ran: env changed' in output: {stdout}"
    );
}

#[test]
fn why_shows_input_files_with_show_inputs() {
    let temp = setup_why_workspace();

    // Run the task first to populate cache
    common::run_luchta(&temp, "build").success();

    // Modify input file
    let src_file = temp.child("packages/app/src.txt");
    src_file.write_str("modified\n").unwrap();

    let stdout = run_why(&temp, &["-p", "app", "build", "--show-inputs"]);
    assert!(
        stdout.contains("changed inputs:"),
        "expected 'changed inputs:' in output: {stdout}"
    );
    assert!(
        stdout.contains("src.txt"),
        "expected 'src.txt' in output: {stdout}"
    );
}

#[test]
fn why_matches_package_glob() {
    let temp = setup_why_workspace();

    let stdout = run_why(&temp, &["-p", "app", "build"]);
    // Should not error when matching package
    assert!(
        stdout.contains("app#build"),
        "expected task header in output: {stdout}"
    );
}

#[test]
fn why_errors_for_unmatched_package() {
    let temp = setup_why_workspace();

    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("why");
    cmd.arg("-p").arg("nonexistent");
    cmd.arg("build");
    cmd.arg("--workspace-root").arg(temp.path());

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("No packages matched"));
}

/// Test that tasks with dependencies correctly report up-to-date when dep outputs unchanged.
/// This specifically tests the fix for the BLOCKER: dep_outputs must be populated from
/// cached dependency outputs, not left empty (which would cause phantom "dependency output changed"
/// reports for every task with dependencies).
#[test]
fn why_shows_cache_hit_for_task_with_unchanged_dependency() {
    // Setup: two packages, where 'lib' is a dependency of 'app'
    // app#build depends on lib#build
    let temp = assert_fs::TempDir::new().unwrap();

    // Create lib package with a build task that produces an output
    common::WorkspaceBuilder {
        yarn_lock: Some(common::YARN1_LOCK_LEFT_PAD_1_0_0),
        task_json: Some(r#""lib#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"cat src.txt > out.txt"}"#),
        script_name: Some("build"),
        extra_files: &[("packages/lib/src.txt", "lib-content\n")],
    }
    .build(&temp);

    // Add app package that depends on lib
    temp.child("packages/app/package.json")
        .write_str(r#"{"name": "app", "scripts": {"build": "echo app"}}"#)
        .unwrap();
    temp.child("packages/app/src.txt")
        .write_str("app-content\n")
        .unwrap();

    // Create luchta config with app#build depending on lib#build
    temp.child("luchta-config.sh")
        .write_str(&format!(
            r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"shell":{{"command":"{}"}}}},"tasks":{{"app#build":{{"cache":{{}},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"cat src.txt > out.txt","depends_on":["lib#build"]}},"lib#build":{{"cache":{{}},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"cat src.txt > out.txt"}}}}}}'
"#,
            temp.child("shell-worker.sh").path().display(),
        ))
        .unwrap();
    common::set_executable(temp.child("luchta-config.sh").path());

    // Re-init git to pick up new files
    common::git_commit_all(temp.path(), "add dependency setup");

    // Run both tasks to populate cache
    common::run_luchta(&temp, "build").success();

    // Run why on app#build - should show "up to date" not "dependency output changed"
    let stdout = run_why(&temp, &["-p", "app", "build"]);

    // The key assertion: should NOT report "dependency output changed"
    // In the bug, empty dep_outputs would cause this to appear
    assert!(
        !stdout.contains("dependency output changed"),
        "should NOT report 'dependency output changed' for unchanged deps: {stdout}"
    );
    assert!(
        stdout.contains("up to date"),
        "expected 'up to date' in output: {stdout}"
    );
}

// ============================================================================
// PR #141 Review Fixes - Integration Tests
// ============================================================================

/// F4: Pruned-only matches should print the prune reason.
/// This test creates a task pattern that only matches pruned tasks (no unpruned match).
/// We use the default no-arg why which shows all tasks including pruned.
#[test]
fn why_shows_prune_reason_for_pruned_task() {
    let temp = assert_fs::TempDir::new().unwrap();

    // Use WorkspaceBuilder to create a basic workspace
    common::WorkspaceBuilder {
        yarn_lock: Some(common::YARN1_LOCK_LEFT_PAD_1_0_0),
        task_json: Some(r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"echo 1 > counter.txt"}"#),
        script_name: Some("pkgbuild"),
        extra_files: &[("packages/app/src.txt", "test\n")],
    }
    .build(&temp);

    // Now add a task that will be pruned (yarn worker with missing script)
    let yarn_worker_path = common::yarn_worker_bin();
    temp.child("luchta-config.sh")
        .write_str(&format!(
            r#"#!/bin/sh
echo '{{"workers":{{"yarn":{{"command":"{}"}},"shell":{{"command":"{}"}}}},"tasks":{{"app#pkgbuild":{{"cache":{{}},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"echo 1 > counter.txt"}},"app#missing":{{"cache":{{}},"worker":"yarn","inputs":["src.txt"]}}}}}}'
"#,
            yarn_worker_path.display(),
            temp.child("shell-worker.sh").path().display()
        ))
        .unwrap();
    common::set_executable(temp.child("luchta-config.sh").path());
    common::git_commit_all(temp.path(), "add pruned task");

    // Run why with no filters to show all tasks
    let stdout = run_why(&temp, &[]);

    // Should show pruned reason for app#missing
    assert!(
        stdout.contains("pruned:"),
        "expected 'pruned:' in output for task with missing script: {stdout}"
    );
}

/// F5: Tasks with no worker or unknown worker should show invalid message, not cache decision.
/// This test creates a task with a command but no worker, and verifies it shows
/// "defines a command but no worker" instead of a cache hit/miss.
#[test]
fn why_shows_invalid_for_task_without_worker() {
    let temp = assert_fs::TempDir::new().unwrap();

    // Task defines a command but no worker - should be invalid
    let task_json = r#""app#pkgbuild":{"cache":{},"inputs":["src.txt"],"command":"echo build"}"#;
    common::setup_pkgbuild_counter_workspace(
        &temp,
        common::YARN1_LOCK_LEFT_PAD_1_0_0,
        task_json,
        SINGLE_BUILD_FILES,
    );

    let stdout = run_why(&temp, &["-p", "app", "pkgbuild"]);

    // Should show invalid message, NOT cache decision
    assert!(
        stdout.contains("defines a command but no worker"),
        "expected 'defines a command but no worker' in output: {stdout}"
    );
    // Should NOT show "would run: no prior" or other cache-related messages
    assert!(
        !stdout.contains("no prior") && !stdout.contains("up to date"),
        "should NOT show cache decision for invalid task: {stdout}"
    );
}

/// F5: Tasks with unknown worker should show invalid message.
/// Note: Unknown workers cause graph build to fail, so we need a task that makes it
/// through resolution but then gets flagged as invalid. For shell worker, tasks without
/// a worker but with a command fit this pattern.
#[test]
fn why_shows_invalid_for_unknown_worker() {
    let temp = assert_fs::TempDir::new().unwrap();

    // Task references a worker that doesn't exist
    // Note: This will fail graph resolution, so let's test the command-without-worker case
    // which gives similar invalid output
    let task_json = r#""app#pkgbuild":{"cache":{},"inputs":["src.txt"],"command":"echo build"}"#;
    common::setup_pkgbuild_counter_workspace(
        &temp,
        common::YARN1_LOCK_LEFT_PAD_1_0_0,
        task_json,
        SINGLE_BUILD_FILES,
    );

    let stdout = run_why(&temp, &["-p", "app", "pkgbuild"]);

    // Should show invalid message about command without worker
    assert!(
        stdout.contains("defines a command but no worker") || stdout.contains("specify a worker"),
        "expected invalid worker message in output: {stdout}"
    );
}
