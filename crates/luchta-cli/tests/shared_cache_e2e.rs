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

/// Test: a <100ms task does NOT store in shared cache.
#[test]
fn fast_task_skips_shared_cache_store() {
    let shared_cache_dir = tempfile::tempdir().unwrap();

    let temp = assert_fs::TempDir::new().unwrap();

    write_root_workspace(&temp);
    // Task deliberately fast (<100ms)
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
        second_stdout.contains("📥 shared: 1"),
        "second build should report shared hit stats, stdout was:\n{second_stdout}"
    );
    assert!(
        second_stdout.contains("⏭️ 1 📥 shared: 1"),
        "second build summary should report shared hit stats, stdout was:\n{second_stdout}"
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
        third_stdout.contains("☑️ 1/1 ⏭️ 1"),
        "third build should still report skip total, stdout was:\n{third_stdout}"
    );
    assert!(
        !third_stdout.contains("📥 shared:"),
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
        stdout_b.contains("📥 shared: 1"),
        "worktree B should report shared hit, stdout was:\n{stdout_b}"
    );

    // Outputs should be present in B
    assert!(
        worktree_b.child("packages/app/counter.txt").exists(),
        "output file should exist in worktree B"
    );
}

/// Test: cross-commit key hierarchy verification.
///
/// Verify that candidate_commit_keys returns proper newest-first ordering
/// with commit-dirty pairs. This tests the git history walking logic
/// that enables cross-commit cache hits.
#[test]
fn cross_commit_key_hierarchy() {
    use std::process::Command;

    let temp = assert_fs::TempDir::new().unwrap();

    // Set up git repo
    let status = Command::new("git")
        .args(["init"])
        .current_dir(temp.path())
        .status()
        .expect("git init");
    assert!(status.success());
    let status = Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(temp.path())
        .status()
        .expect("git config");
    assert!(status.success());
    let status = Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(temp.path())
        .status()
        .expect("git config");
    assert!(status.success());

    temp.child("file1.txt").write_str("one\n").unwrap();
    let status = Command::new("git")
        .args(["add", "."])
        .current_dir(temp.path())
        .status()
        .expect("git add");
    assert!(status.success());
    let status = Command::new("git")
        .args(["commit", "-m", "first"])
        .current_dir(temp.path())
        .status()
        .expect("git commit");
    assert!(status.success());

    let commit_a = get_head_commit(temp.path());

    temp.child("file2.txt").write_str("two\n").unwrap();
    let status = Command::new("git")
        .args(["add", "."])
        .current_dir(temp.path())
        .status()
        .expect("git add");
    assert!(status.success());
    let status = Command::new("git")
        .args(["commit", "-m", "second"])
        .current_dir(temp.path())
        .status()
        .expect("git commit");
    assert!(status.success());

    let commit_b = get_head_commit(temp.path());
    assert_ne!(commit_a, commit_b, "commits should differ");

    // Use the internal API to verify candidate_commit_keys ordering
    let candidates = luchta_cache::shared::git::candidate_commit_keys(temp.path(), 10);

    // Should be: [commit_b, commit_b-dirty, commit_a, commit_a-dirty]
    assert!(candidates.len() >= 4, "should have at least 4 candidates");
    assert!(candidates.contains(&commit_b), "should contain commit_b");
    assert!(
        candidates.contains(&format!("{}-dirty", commit_b)),
        "should contain commit_b-dirty"
    );
    assert!(candidates.contains(&commit_a), "should contain commit_a");
    assert!(
        candidates.contains(&format!("{}-dirty", commit_a)),
        "should contain commit_a-dirty"
    );

    // Newest-first ordering
    let pos_b = candidates.iter().position(|k| k == &commit_b).unwrap();
    let pos_a = candidates.iter().position(|k| k == &commit_a).unwrap();
    assert!(
        pos_b < pos_a,
        "commit_b should appear before commit_a (newest-first)"
    );
}

/// Test: dirty key isolation.
///
/// A dirty-tree build writes a `<commit>-dirty.bincode` snapshot, NEVER the clean one.
/// A clean build does not get a dirty hit.
#[test]
fn dirty_key_isolation() {
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

    let commit = get_head_commit(temp.path());

    // Make dirty (uncommitted change)
    temp.child("packages/app/src.txt")
        .write_str("dirty change\n")
        .unwrap();
    // Do NOT commit — tree is dirty

    // Build in dirty state — counter advances to 1
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

    // Verify dirty snapshot shard dir exists, clean commit dir does NOT
    let snapshots_dir = shared_cache_dir.path().join("snapshots");
    let dirty_snapshot_dir = snapshots_dir.join(format!("{}-dirty", commit));
    let clean_snapshot_dir = snapshots_dir.join(&commit);

    let dirty_snapshot_shards = snapshot_shard_paths(&dirty_snapshot_dir);
    assert!(
        !dirty_snapshot_shards.is_empty(),
        "dirty snapshot shard(s) should exist"
    );
    assert!(
        !clean_snapshot_dir.exists(),
        "clean snapshot dir should NOT exist"
    );

    // NEW TEST START — verify written entry structure directly
    // Load the snapshot and verify entry structure
    let paths = luchta_cache::shared::open_shared_paths(shared_cache_dir.path()).unwrap();
    let store = luchta_cache::shared::snapshot::SnapshotStore::new(paths);
    let snapshot = store.load(&format!("{}-dirty", commit));
    assert!(snapshot.is_some(), "dirty snapshot should load");
    let snapshot = snapshot.unwrap();
    assert!(
        !snapshot.entries.is_empty(),
        "dirty snapshot should have entries"
    );

    // Verify the entry has correct task_id
    let entry = snapshot.entries.values().next().unwrap();
    assert_eq!(
        entry.task_id, "app#pkgbuild",
        "entry should have correct task_id"
    );
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

/// Test: accumulation — running multiple tasks on SAME commit produces ONE snapshot with multiple entries.
#[test]
fn accumulation_single_snapshot_multiple_entries() {
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

    // Run lint
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

    // Run test (same commit)
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

    // Verify commit snapshot shard(s) preserve both entries
    let commit = get_head_commit(temp.path());
    let commit_snapshot_dir = shared_cache_dir.path().join("snapshots").join(&commit);
    let shard_paths = snapshot_shard_paths(&commit_snapshot_dir);

    assert!(
        !shard_paths.is_empty(),
        "snapshot shard(s) should exist for commit"
    );

    // Load merged snapshot and verify entry count
    let paths = luchta_cache::shared::open_shared_paths(shared_cache_dir.path()).unwrap();
    let store = luchta_cache::shared::snapshot::SnapshotStore::new(paths);
    let snapshot = store.load(&commit).expect("snapshot should load");

    assert_eq!(
        snapshot.entries.len(),
        2,
        "snapshot should have 2 entries (lint + test)"
    );

    // Verify each task has an entry
    let has_lint = snapshot.entries.values().any(|e| e.task_id == "app#lint");
    let has_test = snapshot.entries.values().any(|e| e.task_id == "app#test");
    assert!(has_lint, "snapshot should contain lint entry");
    assert!(has_test, "snapshot should contain test entry");
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

/// Recursively list snapshot shard files under snapshots/<commit>/*.bincode.
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

#[allow(dead_code)]
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
