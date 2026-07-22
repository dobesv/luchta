//! E2E tests for per-task `cache.sharing` behavior.
//!
//! Verifies:
//! - `sharing: "none"` disables shared cache read+write
//! - `sharing: "local"` matches current `none` gate behavior for shared cache
//! - omitted `sharing` keeps default remote/shared behavior unchanged
//! - local skip + metadata persistence remain unchanged when shared cache is off

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use assert_fs::prelude::*;

mod common;

use common::{init_git, write_basic_package, write_counter_task_config, write_root_workspace};

fn write_pkgbuild_counter_workspace(temp: &assert_fs::TempDir, cache_config_json: &str) {
    write_root_workspace(temp);
    write_counter_task_config(
        temp,
        &format!(
            r#""app#pkgbuild":{{"cache":{cache_config_json},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"sleep 0.15 && count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}}"#
        ),
    );
    write_basic_package(temp, "pkgbuild");
    temp.child("packages/app/src.txt")
        .write_str("input\n")
        .unwrap();
    init_git(temp);
}

fn run_pkgbuild_with_shared_cache(
    temp: &assert_fs::TempDir,
    shared_cache_dir: &tempfile::TempDir,
) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
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
}

fn read_counter(temp: &assert_fs::TempDir) -> u32 {
    fs::read_to_string(temp.path().join("packages/app/counter.txt"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn wipe_local_cache(temp: &assert_fs::TempDir) {
    std::fs::remove_dir_all(temp.child(".luchta/cache").path()).unwrap();
}

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

fn assert_no_shared_cache_store(shared_cache_dir: &tempfile::TempDir) {
    let blobs_dir = shared_cache_dir.path().join("blobs");
    let snapshots_dir = shared_cache_dir.path().join("snapshots");

    assert!(
        !blobs_dir.exists() || std::fs::read_dir(&blobs_dir).unwrap().next().is_none(),
        "task must not store blobs in shared cache"
    );
    assert!(
        !snapshots_dir.exists() || snapshot_shard_paths(&snapshots_dir).is_empty(),
        "task must not store snapshots in shared cache"
    );
}

#[test]
fn sharing_none_disables_shared_cache() {
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();
    write_pkgbuild_counter_workspace(&temp, r#"{"sharing":"none"}"#);

    run_pkgbuild_with_shared_cache(&temp, &shared_cache_dir).success();
    assert_eq!(read_counter(&temp), 1);
    assert_no_shared_cache_store(&shared_cache_dir);

    wipe_local_cache(&temp);

    run_pkgbuild_with_shared_cache(&temp, &shared_cache_dir).success();
    assert_eq!(read_counter(&temp), 2);
    assert_no_shared_cache_store(&shared_cache_dir);
}

#[test]
fn sharing_local_disables_shared_cache_same_as_none() {
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();
    write_pkgbuild_counter_workspace(&temp, r#"{"sharing":"local"}"#);

    run_pkgbuild_with_shared_cache(&temp, &shared_cache_dir).success();
    assert_eq!(read_counter(&temp), 1);
    assert_no_shared_cache_store(&shared_cache_dir);

    wipe_local_cache(&temp);

    run_pkgbuild_with_shared_cache(&temp, &shared_cache_dir).success();
    assert_eq!(read_counter(&temp), 2);
    assert_no_shared_cache_store(&shared_cache_dir);
}

#[test]
fn sharing_remote_default_unchanged() {
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();
    write_pkgbuild_counter_workspace(&temp, "{}");

    run_pkgbuild_with_shared_cache(&temp, &shared_cache_dir).success();
    assert_eq!(read_counter(&temp), 1);

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

    wipe_local_cache(&temp);

    let second = run_pkgbuild_with_shared_cache(&temp, &shared_cache_dir)
        .success()
        .get_output()
        .stdout
        .clone();
    let second_stdout = String::from_utf8(second).unwrap();

    assert_eq!(read_counter(&temp), 1);
    assert!(
        second_stdout.contains("📥 1"),
        "second build should report shared hit stats, stdout was:\n{second_stdout}"
    );
    assert!(
        second_stdout.contains("⏩ 1 📥 1"),
        "second build summary should report shared hit stats, stdout was:\n{second_stdout}"
    );
    assert!(
        temp.child(".luchta/cache").path().exists(),
        "local cache should be hydrated after shared cache hit"
    );
}

#[test]
fn sharing_none_still_persists_local_metadata() {
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();
    write_pkgbuild_counter_workspace(&temp, r#"{"sharing":"none"}"#);

    run_pkgbuild_with_shared_cache(&temp, &shared_cache_dir).success();
    assert_eq!(read_counter(&temp), 1);

    run_pkgbuild_with_shared_cache(&temp, &shared_cache_dir).success();
    assert_eq!(read_counter(&temp), 1);

    assert!(
        temp.child(".luchta/cache").path().exists(),
        "local cache should exist after run"
    );
    assert_no_shared_cache_store(&shared_cache_dir);
}
