//! Cache integration tests covering yarn lockfile (v1 and Berry) interactions:
//! editing an input reruns, and a transitive dependency version bump in the
//! lockfile reruns the dependent task.
//!
//! Split out of `cache_e2e.rs` so the lockfile fixtures form a cohesive unit.

use assert_fs::prelude::*;

mod common;

use common::{
    assert_pkgbuild_runs_then_skips_then_reruns, git_commit_all, setup_lockfile_workspace,
    setup_skip_edit_workspace, YARN1_LOCK_LEFT_PAD_1_0_0, YARN1_LOCK_LEFT_PAD_1_1_0,
    YARN1_LOCK_LEFT_PAD_TRANSITIVE_3_0_0, YARN1_LOCK_LEFT_PAD_TRANSITIVE_3_0_1,
    YARN_BERRY_LEFT_PAD_1_0_0, YARN_BERRY_LEFT_PAD_1_1_0, YARN_BERRY_LEFT_PAD_TRANSITIVE_3_0_0,
    YARN_BERRY_LEFT_PAD_TRANSITIVE_3_0_1,
};

fn assert_pkgbuild_input_edit_reruns(
    temp: &assert_fs::TempDir,
    yarn_lock: &str,
    expect_out_file: bool,
) {
    setup_skip_edit_workspace(temp, yarn_lock);

    assert_pkgbuild_runs_then_skips_then_reruns(
        temp,
        ("packages/app/counter.txt", "1\n"),
        |temp| {
            temp.child("packages/app/src.txt")
                .write_str("two\n")
                .unwrap();
        },
        ("packages/app/counter.txt", "2\n"),
    );

    if expect_out_file {
        temp.child("packages/app/out.txt").assert("two\n");
    }
}

fn assert_pkgbuild_lockfile_bump_reruns(
    temp: &assert_fs::TempDir,
    yarn_lock: &str,
    next_yarn_lock: &str,
    mutate_input: bool,
) {
    setup_lockfile_workspace(temp, yarn_lock);

    assert_pkgbuild_runs_then_skips_then_reruns(
        temp,
        ("packages/app/counter.txt", "1\n"),
        |temp| {
            temp.child("packages/app/package.json")
                .write_str(
                    r#"{
  "name": "app",
  "dependencies": {
    "left-pad": "^1.1.0"
  },
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#,
                )
                .unwrap();
            temp.child("yarn.lock").write_str(next_yarn_lock).unwrap();
            if mutate_input {
                temp.child("packages/app/src.txt")
                    .write_str("two\n")
                    .unwrap();
            }
            git_commit_all(temp.path(), "bump left-pad");
        },
        ("packages/app/counter.txt", "2\n"),
    );
}

#[test]
fn cache_yarn_v1_skips_unchanged_and_reruns_on_input_edit() {
    let temp = assert_fs::TempDir::new().unwrap();
    assert_pkgbuild_input_edit_reruns(&temp, YARN1_LOCK_LEFT_PAD_1_0_0, true);
}

#[test]
fn cache_yarn_berry_skips_unchanged_and_reruns_on_input_edit() {
    let temp = assert_fs::TempDir::new().unwrap();
    assert_pkgbuild_input_edit_reruns(&temp, YARN_BERRY_LEFT_PAD_1_0_0, false);
}

#[test]
fn cache_yarn_v1_lockfile_version_bump_reruns() {
    let temp = assert_fs::TempDir::new().unwrap();
    assert_pkgbuild_lockfile_bump_reruns(
        &temp,
        YARN1_LOCK_LEFT_PAD_1_0_0,
        YARN1_LOCK_LEFT_PAD_1_1_0,
        true,
    );
}

#[test]
fn cache_yarn_berry_lockfile_version_bump_reruns() {
    let temp = assert_fs::TempDir::new().unwrap();
    assert_pkgbuild_lockfile_bump_reruns(
        &temp,
        YARN_BERRY_LEFT_PAD_1_0_0,
        YARN_BERRY_LEFT_PAD_1_1_0,
        false,
    );
}

/// Helper for transitive-only lockfile bump tests.
/// Unlike `assert_pkgbuild_lockfile_bump_reruns`, this does NOT change the
/// package.json specifier — only the lockfile's transitive dep version changes.
fn assert_pkgbuild_transitive_bump_reruns(
    temp: &assert_fs::TempDir,
    yarn_lock: &str,
    next_yarn_lock: &str,
) {
    setup_lockfile_workspace(temp, yarn_lock);

    assert_pkgbuild_runs_then_skips_then_reruns(
        temp,
        ("packages/app/counter.txt", "1\n"),
        |temp| {
            // Do NOT change package.json — only swap the lockfile.
            temp.child("yarn.lock").write_str(next_yarn_lock).unwrap();
            git_commit_all(temp.path(), "transitive dep bump");
        },
        ("packages/app/counter.txt", "2\n"),
    );
}

#[test]
fn cache_yarn_v1_transitive_dep_bump_reruns() {
    let temp = assert_fs::TempDir::new().unwrap();
    assert_pkgbuild_transitive_bump_reruns(
        &temp,
        YARN1_LOCK_LEFT_PAD_TRANSITIVE_3_0_0,
        YARN1_LOCK_LEFT_PAD_TRANSITIVE_3_0_1,
    );
}

#[test]
fn cache_yarn_berry_transitive_dep_bump_reruns() {
    let temp = assert_fs::TempDir::new().unwrap();
    assert_pkgbuild_transitive_bump_reruns(
        &temp,
        YARN_BERRY_LEFT_PAD_TRANSITIVE_3_0_0,
        YARN_BERRY_LEFT_PAD_TRANSITIVE_3_0_1,
    );
}

/// Test that per-task `dependencies` filter narrows cache invalidation.
/// Scenario: task has `dependencies: ["left-pad"]` (filter selecting only left-pad).
/// 1. Run once → cached.
/// 2. Bump NON-selected dep (chalk) → task STILL SKIPS (cache hit).
/// 3. Bump SELECTED dep (left-pad) → task RE-RUNS (cache miss).
#[test]
fn cache_yarn_v1_dependencies_filter_narrows_invalidation() {
    use common::{
        run_luchta, setup_filtered_deps_workspace, YARN1_LOCK_LEFT_PAD_AND_CHALK_CHALK_BUMP,
        YARN1_LOCK_LEFT_PAD_AND_CHALK_LEFT_PAD_BUMP, YARN1_LOCK_LEFT_PAD_AND_CHALK_V1,
    };

    let temp = assert_fs::TempDir::new().unwrap();

    // Setup with filter selecting only "left-pad"
    setup_filtered_deps_workspace(&temp, YARN1_LOCK_LEFT_PAD_AND_CHALK_V1, &["left-pad"]);

    // Run 1: task executes, counter = 1
    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    // Run 2: cache hit, counter still = 1
    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    // Step b: bump NON-selected dep (chalk 5.0.0 → 5.1.0)
    // Task should SKIP (cache hit) because chalk is filtered out
    temp.child("yarn.lock")
        .write_str(YARN1_LOCK_LEFT_PAD_AND_CHALK_CHALK_BUMP)
        .unwrap();
    common::git_commit_all(temp.path(), "chalk version bump");
    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n"); // still 1 = cache hit

    // Step c: bump SELECTED dep (left-pad 1.0.0 → 1.1.0)
    // Task should RE-RUN (cache miss) because left-pad is in the filter
    temp.child("yarn.lock")
        .write_str(YARN1_LOCK_LEFT_PAD_AND_CHALK_LEFT_PAD_BUMP)
        .unwrap();
    common::git_commit_all(temp.path(), "left-pad version bump");
    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("2\n"); // incremented = cache miss
}
