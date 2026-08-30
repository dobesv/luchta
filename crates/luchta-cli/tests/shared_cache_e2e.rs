//! Integration tests for shared cache read/write path (P4.2/P4.3).
//!
//! Tests verify:
//! - Shared cache disabled by default (no regression)
//! - Store path: >100ms task writes blob+snapshot, <100ms task does not
//! - Cross-package-output task does NOT write to shared cache but runs fine
//! - Genuine E2E: build populates shared cache, wipe local, rebuild restores (cross-build)
//!
//! Path-escape at write time is covered by unit tests in shared/mod.rs.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_fs::prelude::*;

mod common;

use common::{init_git, write_counter_task_config, write_root_workspace};

fn assert_shared_cache_dir_inactive(shared_cache_root: &Path) {
    assert!(
        !shared_cache_root.exists(),
        "shared cache root should stay absent when shared cache is disabled"
    );
    assert!(
        !shared_cache_root.join("blobs").exists(),
        "shared cache blobs dir should stay absent when shared cache is disabled"
    );
    assert!(
        !shared_cache_root.join("snapshots").exists(),
        "shared cache snapshots dir should stay absent when shared cache is disabled"
    );
}

/// Test: shared cache is disabled by default (no regression).
#[test]
fn shared_cache_disabled_by_default() {
    let temp = assert_fs::TempDir::new().unwrap();
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let shared_cache_root = shared_cache_dir.path().join("disabled-default");

    write_root_workspace(&temp);
    write_counter_task_config(
        &temp,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
    );
    temp.child("packages/app/src.txt")
        .write_str("one\n")
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    // First run WITHOUT shared cache enabled
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env_remove("LUCHTA_SHARED_CACHE")
        .env("LUCHTA_SHARED_CACHE_DIR", &shared_cache_root)
        .assert()
        .success();

    assert!(
        temp.child(".luchta/cache").path().exists(),
        "local cache should exist after run"
    );

    // Second run should use local cache
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env_remove("LUCHTA_SHARED_CACHE")
        .env("LUCHTA_SHARED_CACHE_DIR", &shared_cache_root)
        .assert()
        .success();

    assert_shared_cache_dir_inactive(&shared_cache_root);
}

/// Test: explicit falsey shared cache gate stays disabled.
#[test]
fn shared_cache_falsey_gate_disables_shared_cache() {
    let temp = assert_fs::TempDir::new().unwrap();
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let shared_cache_root = shared_cache_dir.path().join("disabled-falsey");

    write_root_workspace(&temp);
    write_counter_task_config(
        &temp,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
    );
    temp.child("packages/app/src.txt")
        .write_str("one\n")
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "0")
        .env("LUCHTA_SHARED_CACHE_DIR", &shared_cache_root)
        .assert()
        .success();

    assert!(
        temp.child(".luchta/cache").path().exists(),
        "local cache should still exist after run"
    );
    assert_shared_cache_dir_inactive(&shared_cache_root);
}

/// Test: a >100ms task stores blob and snapshot entry in shared cache.

#[test]
fn non_cacheable_task_stays_out_of_shared_cache_but_writes_local_record() {
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();

    write_root_workspace(&temp);
    write_counter_task_config(
        &temp,
        r#""app#pkgbuild":{"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"sleep 0.15 && count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
    );
    temp.child("packages/app/src.txt")
        .write_str("one\n")
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success();

    let cache_dir = temp
        .path()
        .join(".luchta")
        .join("cache")
        .join(blake3::hash("app#pkgbuild".as_bytes()).to_hex().as_str());
    assert!(cache_dir.join("meta.bincode").exists());
    assert!(cache_dir.join("stdout.log").exists());
    assert!(cache_dir.join("stderr.log").exists());

    let blobs_dir = shared_cache_dir.path().join("blobs");
    let snapshots_dir = shared_cache_dir.path().join("snapshots");
    assert!(
        !blobs_dir.exists() || std::fs::read_dir(&blobs_dir).unwrap().next().is_none(),
        "non-cacheable task must not store blobs in shared cache"
    );
    assert!(
        !snapshots_dir.exists() || snapshot_shard_paths(&snapshots_dir).is_empty(),
        "non-cacheable task must not store snapshots in shared cache"
    );
}
#[test]
fn slow_task_stores_in_shared_cache() {
    let shared_cache_dir = tempfile::tempdir().unwrap();

    let temp = assert_fs::TempDir::new().unwrap();

    write_root_workspace(&temp);
    // Task with sleep to ensure duration > 100ms
    write_counter_task_config(
        &temp,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"sleep 0.15 && count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
    );
    temp.child("packages/app/src.txt")
        .write_str("one\n")
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    // Run with shared cache enabled
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success();

    // Verify blob and snapshot exist in shared cache
    let blobs_dir = shared_cache_dir.path().join("blobs");
    let snapshots_dir = shared_cache_dir.path().join("snapshots");

    assert!(blobs_dir.exists(), "blobs dir should exist");
    assert!(snapshots_dir.exists(), "snapshots dir should exist");

    let blob_count = std::fs::read_dir(&blobs_dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .ok()
                .map(|e| e.file_name().to_string_lossy().ends_with(".tar.zst"))
                .unwrap_or(false)
        })
        .count();
    assert!(
        blob_count > 0,
        "at least one blob should exist after >100ms task"
    );

    let snapshot_count = snapshot_shard_paths(&snapshots_dir).len();
    assert!(
        snapshot_count > 0,
        "at least one snapshot should exist after >100ms task"
    );

    assert!(
        temp.child(".luchta/cache").path().exists(),
        "local cache should exist after run"
    );
}

/// Test: a task below the duration threshold does NOT store in shared cache.
///
/// The threshold is raised via `LUCHTA_SHARED_CACHE_MIN_DURATION_MS` rather
/// than relying on the task beating the 100ms default. Racing the clock made
/// this fail under CPU oversubscription — the task crossed 100ms, was cached
/// legitimately, and the failure read as a cache regression (#290).
#[test]
fn fast_task_skips_shared_cache_store() {
    let shared_cache_dir = tempfile::tempdir().unwrap();

    let temp = assert_fs::TempDir::new().unwrap();

    write_root_workspace(&temp);
    write_counter_task_config(
        &temp,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
    );
    temp.child("packages/app/src.txt")
        .write_str("one\n")
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        // An hour. No task in this test can cross it however loaded the
        // machine is, so "below the threshold" is a fact rather than a race.
        .env("LUCHTA_SHARED_CACHE_MIN_DURATION_MS", "3600000")
        .assert()
        .success();

    let blobs_dir = shared_cache_dir.path().join("blobs");
    let snapshots_dir = shared_cache_dir.path().join("snapshots");

    if blobs_dir.exists() {
        let blob_count = std::fs::read_dir(&blobs_dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .ok()
                    .map(|e| e.file_name().to_string_lossy().ends_with(".tar.zst"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(blob_count, 0, "fast task should not store blob");
    }

    if snapshots_dir.exists() {
        // Count .bincode files, excluding .lock sidecar files
        let snapshot_count = std::fs::read_dir(&snapshots_dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .ok()
                    .map(|e| {
                        let file_name = e.file_name();
                        let name = file_name.to_string_lossy();
                        name.ends_with(".bincode") && !name.ends_with(".lock")
                    })
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(snapshot_count, 0, "fast task should not store snapshot");
    }

    assert!(
        temp.child(".luchta/cache").path().exists(),
        "local cache should exist after run"
    );
}

/// Test: cross-package output task runs and writes local cache, but skips shared cache.
#[test]
fn cross_package_output_skips_shared_cache() {
    let shared_cache_dir = tempfile::tempdir().unwrap();

    let temp = assert_fs::TempDir::new().unwrap();

    write_root_workspace(&temp);
    // Task that outputs to parent directory (cross-package)
    write_counter_task_config(
        &temp,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["../output.txt"],"command":"echo cross-pkg > ../output.txt"}"#,
    );
    temp.child("packages/app/src.txt")
        .write_str("one\n")
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success();

    let blobs_dir = shared_cache_dir.path().join("blobs");
    let snapshots_dir = shared_cache_dir.path().join("snapshots");

    if blobs_dir.exists() {
        let blob_count = std::fs::read_dir(&blobs_dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .ok()
                    .map(|e| e.file_name().to_string_lossy().ends_with(".tar.zst"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(blob_count, 0, "cross-package task should not store blob");
    }

    if snapshots_dir.exists() {
        // Count .bincode files, excluding .lock sidecar files
        let snapshot_count = std::fs::read_dir(&snapshots_dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .ok()
                    .map(|e| {
                        let file_name = e.file_name();
                        let name = file_name.to_string_lossy();
                        name.ends_with(".bincode") && !name.ends_with(".lock")
                    })
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(
            snapshot_count, 0,
            "cross-package task should not store snapshot"
        );
    }

    assert!(
        temp.child(".luchta/cache").path().exists(),
        "local cache should exist after run"
    );

    assert!(
        temp.child("packages/output.txt").path().exists(),
        "cross-package output should exist"
    );
}

#[test]
fn shared_cache_hit_restores_reports_into_local_cache_and_logs_file() {
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();
    common::setup_workspace(&temp);

    let worker = common::shell_worker_with_reports(
        &temp,
        &[("shared.json", "application/json", "{\"shared\":true}\n")],
    );
    common::write_task_config_with_shell_worker(
        &temp,
        worker.path(),
        r#""a#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"sleep 0.15 && count=$(cat ../../run-count.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > ../../run-count.txt; echo running"}"#,
    );
    common::init_git(&temp);

    Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env("LUCHTA_SHARED_CACHE_DIR", shared_cache_dir.path())
        .assert()
        .success();
    temp.child("run-count.txt").assert("1\n");

    std::fs::remove_dir_all(temp.child(".luchta/cache").path()).unwrap();

    let second = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env("LUCHTA_SHARED_CACHE_DIR", shared_cache_dir.path())
        .env("LUCHTA_SHARED_CACHE_STATS", "1")
        .assert()
        .success()
        .get_output()
        .clone();
    let second_stdout = String::from_utf8(second.stdout).unwrap();
    let second_stderr = String::from_utf8(second.stderr).unwrap();
    assert!(
        second_stdout.contains("📥 1"),
        "stdout was:
{second_stdout}"
    );
    temp.child("run-count.txt").assert("1\n");
    assert!(second_stderr.contains("inline_hits=1 fallback_meta_gets=0 blob_gets=0"));

    let cache = luchta_cache::Cache::open(&temp.path().join(".luchta/cache")).unwrap();
    let report = cache
        .read_report("a#build", "shared.json")
        .expect("restored report");
    assert_eq!(report, b"{\"shared\":true}\n");

    let output = Command::cargo_bin("luchta")
        .unwrap()
        .env("NO_COLOR", "1")
        .arg("logs")
        .arg("--file")
        .arg("shared.json")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(output, b"{\"shared\":true}\n");
}

/// Test: genuine E2E cross-build cache flow.
#[test]
fn e2e_cross_build_shared_cache_hit() {
    let shared_cache_dir = tempfile::tempdir().unwrap();

    let temp = assert_fs::TempDir::new().unwrap();

    write_root_workspace(&temp);
    // Task with sleep to ensure duration > 100ms for shared cache eligibility
    write_counter_task_config(
        &temp,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"sleep 0.15 && count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
    );
    temp.child("packages/app/src.txt")
        .write_str("one\n")
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    // First build with shared cache enabled
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success();

    // Counter should be "1" after first build
    temp.child("packages/app/counter.txt").assert("1\n");

    let blobs_dir = shared_cache_dir.path().join("blobs");
    let snapshots_dir = shared_cache_dir.path().join("snapshots");

    let snapshot_count = snapshot_shard_paths(&snapshots_dir).len();
    assert!(snapshot_count > 0, "first build should store snapshot");

    let blob_count = std::fs::read_dir(&blobs_dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .ok()
                .map(|e| e.file_name().to_string_lossy().ends_with(".tar.zst"))
                .unwrap_or(false)
        })
        .count();
    assert!(blob_count > 0, "first build should store blob");

    // Wipe local cache only (keep shared cache)
    std::fs::remove_dir_all(temp.child(".luchta/cache").path()).unwrap();

    // Second build should restore from shared cache (counter unchanged)
    let second = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let second_stdout = String::from_utf8(second).unwrap();

    temp.child("packages/app/counter.txt").assert("1\n");

    assert!(
        second_stdout.contains("📥 1"),
        "second build should report shared hit stats, stdout was:\n{second_stdout}"
    );
    assert!(
        second_stdout.contains("✔ 1 📥 1") && !second_stdout.contains("⏩"),
        "second build should not count a shared hit as skipped, stdout was:\n{second_stdout}"
    );

    assert!(
        temp.child(".luchta/cache").path().exists(),
        "local cache should be hydrated after shared cache hit"
    );

    let third = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let third_stdout = String::from_utf8(third).unwrap();

    assert!(
        third_stdout.contains("✔ 1 ⏩ 1"),
        "third build should still report skip total, stdout was:\n{third_stdout}"
    );
    assert!(
        !third_stdout.contains("📥"),
        "third build should be local skip, not shared, stdout was:\n{third_stdout}"
    );
}

/// Test: cross-worktree hit (different absolute repo path).
///
/// Build in repo A at commit X populates shared cache.
/// Create separate worktree B at same commit X with empty local cache.
/// Build B → task RESTORED from shared cache (not executed), outputs present.
/// Counter/side-effect probe unchanged in B.
#[test]
fn cross_worktree_shared_cache_hit() {
    let shared_cache_dir = tempfile::tempdir().unwrap();

    // === Worktree A: initial build populates shared cache ===
    let worktree_a = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&worktree_a);
    write_counter_task_config(
        &worktree_a,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"sleep 0.15 && count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
    );
    worktree_a
        .child("packages/app/src.txt")
        .write_str("content-a\n")
        .unwrap();
    worktree_a
        .child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&worktree_a);

    // First build in A
    Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(worktree_a.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success();

    worktree_a.child("packages/app/counter.txt").assert("1\n");

    // Get the commit hash from A
    let commit_a = get_head_commit(worktree_a.path());

    // === Worktree B: separate clone at same commit ===
    let worktree_b = assert_fs::TempDir::new().unwrap();

    // Clone repo A to B (same commit, different absolute path)
    clone_repo_to(worktree_a.path(), worktree_b.path());

    // Verify B is at same commit
    let commit_b = get_head_commit(worktree_b.path());
    assert_eq!(commit_a, commit_b, "worktrees should be at same commit");

    // B should have empty local cache
    assert!(
        !worktree_b.child(".luchta/cache").path().exists(),
        "worktree B should start with empty local cache"
    );

    // Second build in B should restore from shared cache (counter unchanged)
    let output_b = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(worktree_b.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout_b = String::from_utf8(output_b).unwrap();

    // Counter should NOT have incremented (restored from cache)
    worktree_b.child("packages/app/counter.txt").assert("1\n");

    assert!(
        stdout_b.contains("📥 1"),
        "worktree B should report shared hit, stdout was:\n{stdout_b}"
    );

    // Outputs should be present in B
    assert!(
        worktree_b.child("packages/app/counter.txt").exists(),
        "output file should exist in worktree B"
    );
}

/// Test: a dirty-tree build's entry is not reused by a later clean build.
///
/// Bucket keys carry no notion of git state, so a clean build can now land
/// in the same bucket as a dirty build's entry just like any other
/// candidate — nothing about the bucket's name keeps them apart anymore.
/// Isolation instead comes from the `input_key` itself: `derive_input_key`
/// folds in `inputs_hash`, a hash of the task's resolved input CONTENT, so
/// the dirty build's key and the clean build's key differ once the file
/// content differs. The clean build's shared-cache lookup for ITS key simply
/// finds no candidate — `decide_shared_restore` (which compares a found
/// candidate's recorded inputs against the caller's already-resolved
/// `inputs_hash`, see its doc comment) is never reached, because
/// `try_restore_candidates` never returns the dirty build's entry for a
/// different key in the first place.
#[test]
fn dirty_tree_entry_is_not_reused_by_clean_build() {
    let shared_cache_dir = tempfile::tempdir().unwrap();

    let temp = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&temp);
    write_counter_task_config(
        &temp,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"sleep 0.15 && count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
    );
    temp.child("packages/app/src.txt")
        .write_str("initial\n")
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    // Make dirty (uncommitted change)
    temp.child("packages/app/src.txt")
        .write_str("dirty change\n")
        .unwrap();
    // Do NOT commit — tree is dirty

    // Build in dirty state — counter advances to 1. The entry is recorded
    // against the dirty content's input_key.
    Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success();

    temp.child("packages/app/counter.txt").assert("1\n");

    // Finalize with content DIFFERENT from what was dirty-built, and commit
    // it. Same task, env, and deps as the dirty build, but different file
    // content means a different `inputs_hash` and therefore a different
    // `input_key` — the dirty build's entry lives under a key this build
    // will never look up.
    temp.child("packages/app/src.txt")
        .write_str("final change\n")
        .unwrap();
    git_commit_all(temp.path(), "commit the change");

    // Wipe local cache so the second build has to go through the shared cache.
    std::fs::remove_dir_all(temp.child(".luchta/cache").path()).unwrap();

    // Second, clean build must NOT reuse the dirty build's entry: its own
    // input_key (content-keyed) differs from the dirty build's, so the
    // shared-cache lookup finds no candidate at all — a genuine miss — and
    // the task runs again.
    let second = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let second_stdout = String::from_utf8(second).unwrap();

    assert!(
        !second_stdout.contains("📥"),
        "clean build must not restore the dirty build's entry, stdout was:\n{second_stdout}"
    );
    temp.child("packages/app/counter.txt").assert("2\n");
}

/// Test: cross-commit shared cache hit — the headline CI value.
///
/// Verifies that the cross-commit candidate window + content-keyed input_key
/// behavior works end-to-end at the CLI level:
///
/// 1. Commit A: build populates shared cache (run-count=1)
/// 2. Commit B: edits UNRELATED file (not in task inputs) → input_key UNCHANGED
///    → wipe local cache → build HITS shared cache (run-count stays 1)
/// 3. Commit C: edits TASK INPUT file → input_key CHANGES
///    → wipe local cache → build MISSES shared cache (run-count=2)
///
/// Proves: unchanged-inputs commit → restored-not-run; changed-inputs commit → runs.
///
/// Test design:
/// - The task's DECLARED OUTPUT is `out.txt` (what gets cached/restored)
/// - The run-count probe `run-count.txt` is at workspace root (NOT a declared output)
/// - The run-count file is NEVER deleted between builds, so it accurately tracks
///   how many times the task actually executed
#[test]
fn cross_commit_shared_cache_hit() {
    use std::process::Command as StdCommand;

    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();

    write_root_workspace(&temp);
    // Task with:
    // - inputs=["src.txt"]
    // - outputs=["out.txt"] (the cached output)
    // - run-count probe at workspace root (NOT a declared output, never deleted)
    write_counter_task_config(
        &temp,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"sleep 0.15 && count=$(cat ../../run-count.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > ../../run-count.txt; echo 'output' > out.txt"}"#,
    );
    temp.child("packages/app/src.txt")
        .write_str("initial content\n")
        .unwrap();
    temp.child("packages/app/unrelated.txt")
        .write_str("unrelated initial\n")
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    // === COMMIT A: Initial build populates shared cache ===
    Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success();

    let commit_a = get_head_commit(temp.path());

    // Run count should be 1 — first run
    temp.child("run-count.txt").assert("1\n");
    // Output should exist
    assert!(
        temp.child("packages/app/out.txt").exists(),
        "output should exist"
    );

    // === COMMIT B: Edit UNRELATED file (not in inputs) → input_key SAME → should HIT ===
    temp.child("packages/app/unrelated.txt")
        .write_str("unrelated modified\n")
        .unwrap();

    // Commit the unrelated change (only the unrelated file, not generated files)
    StdCommand::new("git")
        .args(["add", "packages/app/unrelated.txt"])
        .current_dir(temp.path())
        .status()
        .expect("git add");
    StdCommand::new("git")
        .args(["commit", "-m", "edit unrelated file"])
        .current_dir(temp.path())
        .status()
        .expect("git commit");

    let commit_b = get_head_commit(temp.path());
    assert_ne!(commit_a, commit_b, "commits A and B should differ");

    // Wipe local cache to force shared cache lookup
    let local_cache_path = temp.path().join(".luchta/cache");
    if local_cache_path.exists() {
        std::fs::remove_dir_all(&local_cache_path).expect("remove local cache");
    }

    // Remove output file to verify restoration
    let output_path = temp.path().join("packages/app/out.txt");
    std::fs::remove_file(&output_path).ok(); // ignore if missing

    // Build at commit B — should HIT commit A's snapshot via candidate window
    Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success();

    // Run count should STILL be 1 — task was RESTORED, not re-run
    temp.child("run-count.txt").assert("1\n");
    // Output should be restored
    assert!(
        temp.child("packages/app/out.txt").exists(),
        "output should be restored"
    );

    // === COMMIT C: Edit TASK INPUT file → input_key CHANGES → should MISS ===
    temp.child("packages/app/src.txt")
        .write_str("modified task input\n")
        .unwrap();

    // Commit the input change (only the input file)
    StdCommand::new("git")
        .args(["add", "packages/app/src.txt"])
        .current_dir(temp.path())
        .status()
        .expect("git add");
    StdCommand::new("git")
        .args(["commit", "-m", "edit task input"])
        .current_dir(temp.path())
        .status()
        .expect("git commit");

    let commit_c = get_head_commit(temp.path());
    assert_ne!(commit_b, commit_c, "commits B and C should differ");

    // Wipe local cache again
    if local_cache_path.exists() {
        std::fs::remove_dir_all(&local_cache_path).expect("remove local cache");
    }

    // Remove output file
    std::fs::remove_file(&output_path).ok(); // ignore if missing

    // Build at commit C — should MISS and RUN (input_key changed)
    Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success();

    // Run count should be 2 — task RAN
    temp.child("run-count.txt").assert("2\n");
}

/// Test: entries from separate `luchta run` invocations are both discoverable.
///
/// Each write picks its bucket as `<YYYYMMDD>-<shard>` from a nonce, so
/// `lint` and `test` may land in the same bucket or different ones — nothing
/// here pins them apart. What matters is that a build's read window covers
/// every bucket it could have written to, so a later build can restore
/// either task's entry regardless of which bucket each landed in.
#[test]
fn entries_from_separate_runs_are_both_discoverable() {
    let shared_cache_dir = tempfile::tempdir().unwrap();

    let temp = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&temp);

    // Two separate counter tasks in one config
    write_counter_task_config(
        &temp,
        r#""app#lint":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["lint-counter.txt"],"command":"sleep 0.15 && count=$(cat lint-counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > lint-counter.txt"},"app#test":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["test-counter.txt"],"command":"sleep 0.15 && count=$(cat test-counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > test-counter.txt"}}"#,
    );

    temp.child("packages/app/src.txt")
        .write_str("source\n")
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "lint": "echo ignored",
    "test": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    // Run lint.
    Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("lint")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success();
    temp.child("packages/app/lint-counter.txt").assert("1\n");

    // Run test — a separate invocation.
    Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("test")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success();
    temp.child("packages/app/test-counter.txt").assert("1\n");

    // Wipe local cache so both re-runs have to go through the shared cache.
    std::fs::remove_dir_all(temp.child(".luchta/cache").path()).unwrap();

    // Re-run lint: restored from the shared cache, counter unchanged.
    let lint_rerun = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("lint")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let lint_rerun_stdout = String::from_utf8(lint_rerun).unwrap();
    assert!(
        lint_rerun_stdout.contains("📥 1"),
        "lint re-run should report a shared hit, stdout was:\n{lint_rerun_stdout}"
    );
    temp.child("packages/app/lint-counter.txt").assert("1\n");

    // Re-run test: restored from the shared cache, counter unchanged.
    let test_rerun = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("test")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let test_rerun_stdout = String::from_utf8(test_rerun).unwrap();
    assert!(
        test_rerun_stdout.contains("📥 1"),
        "test re-run should report a shared hit, stdout was:\n{test_rerun_stdout}"
    );
    temp.child("packages/app/test-counter.txt").assert("1\n");
}

/// Test: over-size-cap task is NOT cached.
///
/// Set LUCHTA_SHARED_CACHE_MAX_OUTPUT_MB very small, produce larger output → no blob/entry.
#[test]
fn over_size_cap_task_not_cached() {
    let shared_cache_dir = tempfile::tempdir().unwrap();

    let temp = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&temp);

    // Task that produces ~10KB output
    write_counter_task_config(
        &temp,
        r#""app#largebuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["large.txt"],"command":"sleep 0.15 && dd if=/dev/zero bs=1024 count=10 2>/dev/null | base64 > large.txt"}"#,
    );

    temp.child("packages/app/src.txt")
        .write_str("source\n")
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "largebuild": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    // Run with ZERO size cap — nothing can be stored
    Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("largebuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env(
            "LUCHTA_SHARED_CACHE_DIR",
            shared_cache_dir.path().to_str().unwrap(),
        )
        .env("LUCHTA_SHARED_CACHE_MAX_OUTPUT_MB", "0") // zero cap = nothing stored
        .assert()
        .success();

    // Verify task actually ran (output exists)
    assert!(
        temp.child("packages/app/large.txt").exists(),
        "large output file should exist"
    );

    // Verify NO blob (sized out)
    let blobs_dir = shared_cache_dir.path().join("blobs");
    if blobs_dir.exists() {
        let blob_count = std::fs::read_dir(&blobs_dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .ok()
                    .map(|e| e.file_name().to_string_lossy().ends_with(".tar.zst"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(blob_count, 0, "over-size task should not store blob");
    }

    // Snapshot may or may not exist with entry, but if it does, entry should record skip
    // The key invariant: no blob was written for over-size output
}

/// Recursively list snapshot shard files under snapshots/<YYYYMMDD>-<shard>/*.bincode.
fn snapshot_shard_paths(root: &Path) -> Vec<PathBuf> {
    let mut shards = Vec::new();
    collect_snapshot_shards(root, &mut shards);
    shards.sort();
    shards
}

fn collect_snapshot_shards(path: &Path, shards: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };

    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry_path.is_dir() {
            collect_snapshot_shards(&entry_path, shards);
            continue;
        }

        let Some(ext) = entry_path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        if ext != "bincode" {
            continue;
        }

        shards.push(entry_path);
    }
}

// === Helper functions ===

/// Get HEAD commit hash as hex string.
fn get_head_commit(repo_path: &Path) -> String {
    use std::process::Command;
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .expect("git rev-parse HEAD");
    assert!(output.status.success(), "git rev-parse HEAD failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Commit all changes in repo.
fn git_commit_all(repo_path: &Path, message: &str) {
    use std::process::Command;
    let status = Command::new("git")
        .args(["add", "."])
        .current_dir(repo_path)
        .status()
        .expect("git add");
    assert!(status.success(), "git add failed");
    let status = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(repo_path)
        .status()
        .expect("git commit");
    assert!(status.success(), "git commit failed");
}

/// Clone repo A to B.
fn clone_repo_to(source: &Path, dest: &Path) {
    use std::process::Command;
    let status = Command::new("git")
        .args(["clone", &source.to_string_lossy(), &dest.to_string_lossy()])
        .status()
        .expect("git clone");
    assert!(status.success(), "git clone failed");
}
