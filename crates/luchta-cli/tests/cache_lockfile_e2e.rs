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
    YARN_BERRY_LEFT_PAD_1_0_0, YARN_BERRY_LEFT_PAD_1_1_0,
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
