//! End-to-end coverage for advisory `cacheFiles` task state.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use assert_fs::prelude::*;

mod common;

fn setup(temp: &assert_fs::TempDir, command: &str, outputs: &str) {
    common::write_root_workspace(temp);
    common::write_basic_package(temp, "lint");
    let worker = common::shell_worker(temp);
    common::write_task_config_with_shell_worker(
        temp,
        worker.path(),
        &format!(
            r#""app#lint":{{"cache":{{}},"worker":"shell","inputs":["src.txt"],"outputs":{outputs},"cacheFiles":["**/.toolcache"],"command":"{command}"}}"#
        ),
    );
    temp.child("packages/app/src.txt")
        .write_str("one\n")
        .unwrap();
    common::init_git(temp);
}

fn run_command(temp: &assert_fs::TempDir, shared_cache: &Path, no_cache: bool) -> Command {
    let mut command = Command::cargo_bin("luchta").unwrap();
    command
        .arg("run")
        .arg("lint")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .env("LUCHTA_SHARED_CACHE", "1")
        .env("LUCHTA_SHARED_CACHE_MIN_DURATION_MS", "0")
        .env("LUCHTA_SHARED_CACHE_DIR", shared_cache);
    if no_cache {
        command.arg("--no-cache");
    }
    command
}

fn run(temp: &assert_fs::TempDir, shared_cache: &Path, no_cache: bool) -> std::process::Output {
    run_command(temp, shared_cache, no_cache)
        .assert()
        .success()
        .get_output()
        .clone()
}

fn trace(temp: &assert_fs::TempDir) -> String {
    fs::read_to_string(temp.path().join("packages/app/trace.txt")).unwrap()
}

#[test]
fn changed_inputs_execute_with_restored_warm_state_and_local_state_wins() {
    let temp = assert_fs::TempDir::new().unwrap();
    let shared = tempfile::tempdir().unwrap();
    setup(
        &temp,
        "sleep 0.01; (cat .toolcache 2>/dev/null || echo cold) >> trace.txt; cp src.txt .toolcache",
        "[]",
    );

    run(&temp, shared.path(), false);
    assert_eq!(trace(&temp), "cold\n");

    temp.child("packages/app/src.txt")
        .write_str("two\n")
        .unwrap();
    fs::remove_file(temp.path().join("packages/app/.toolcache")).unwrap();
    run(&temp, shared.path(), false);
    assert_eq!(trace(&temp), "cold\none\n");

    temp.child("packages/app/src.txt")
        .write_str("three\n")
        .unwrap();
    temp.child("packages/app/.toolcache")
        .write_str("local\n")
        .unwrap();
    run(&temp, shared.path(), false);
    assert_eq!(trace(&temp), "cold\none\nlocal\n");
}

#[test]
fn exact_shared_output_hit_does_not_restore_cache_files() {
    let temp = assert_fs::TempDir::new().unwrap();
    let shared = tempfile::tempdir().unwrap();
    setup(
        &temp,
        "count=$(cat runs.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > runs.txt; echo output > result.txt; echo warm > .toolcache",
        "[\"result.txt\"]",
    );

    run(&temp, shared.path(), false);
    fs::remove_dir_all(temp.path().join(".luchta/cache")).unwrap();
    fs::remove_file(temp.path().join("packages/app/result.txt")).unwrap();
    fs::remove_file(temp.path().join("packages/app/.toolcache")).unwrap();

    let second = run(&temp, shared.path(), false);
    assert!(String::from_utf8_lossy(&second.stdout).contains("📥 1"));
    assert_eq!(
        fs::read_to_string(temp.path().join("packages/app/runs.txt")).unwrap(),
        "1\n"
    );
    assert!(temp.path().join("packages/app/result.txt").exists());
    assert!(!temp.path().join("packages/app/.toolcache").exists());
}

#[test]
fn corrupt_selected_state_runs_cold_and_no_cache_publishes_nothing() {
    let temp = assert_fs::TempDir::new().unwrap();
    let shared = tempfile::tempdir().unwrap();
    setup(
        &temp,
        "(cat .toolcache 2>/dev/null || echo cold) >> trace.txt; cp src.txt .toolcache",
        "[]",
    );

    run(&temp, shared.path(), false);
    let blob = fs::read_dir(shared.path().join("cache-files"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(blob, b"corrupt").unwrap();
    temp.child("packages/app/src.txt")
        .write_str("two\n")
        .unwrap();
    fs::remove_file(temp.path().join("packages/app/.toolcache")).unwrap();
    run(&temp, shared.path(), false);
    assert_eq!(trace(&temp), "cold\ncold\n");

    let no_cache_temp = assert_fs::TempDir::new().unwrap();
    let no_cache_shared = tempfile::tempdir().unwrap();
    setup(&no_cache_temp, "echo warm > .toolcache", "[]");
    run(&no_cache_temp, no_cache_shared.path(), true);
    assert!(
        !no_cache_shared.path().join("cache-files").exists()
            || fs::read_dir(no_cache_shared.path().join("cache-files"))
                .unwrap()
                .next()
                .is_none()
    );
}

#[test]
fn empty_successful_state_records_tombstone() {
    let temp = assert_fs::TempDir::new().unwrap();
    let shared = tempfile::tempdir().unwrap();
    setup(
        &temp,
        "sleep 0.01; (cat .toolcache 2>/dev/null || echo cold) >> trace.txt; if [ $(cat src.txt) = none ]; then rm -f .toolcache; else cp src.txt .toolcache; fi",
        "[]",
    );

    run(&temp, shared.path(), false);
    temp.child("packages/app/src.txt")
        .write_str("none\n")
        .unwrap();
    fs::remove_file(temp.path().join("packages/app/.toolcache")).unwrap();
    run(&temp, shared.path(), false);
    assert_eq!(trace(&temp), "cold\none\n");
    assert!(!temp.path().join("packages/app/.toolcache").exists());

    temp.child("packages/app/src.txt")
        .write_str("three\n")
        .unwrap();
    run(&temp, shared.path(), false);
    assert_eq!(trace(&temp), "cold\none\ncold\n");
}

#[test]
fn failed_and_input_unstable_runs_do_not_publish_state() {
    let failed = assert_fs::TempDir::new().unwrap();
    let failed_shared = tempfile::tempdir().unwrap();
    setup(&failed, "echo warm > .toolcache; exit 7", "[]");
    run_command(&failed, failed_shared.path(), false)
        .assert()
        .failure();
    assert_cache_file_store_is_empty(failed_shared.path());

    let unstable = assert_fs::TempDir::new().unwrap();
    let unstable_shared = tempfile::tempdir().unwrap();
    setup(
        &unstable,
        "echo changed > src.txt; echo warm > .toolcache",
        "[]",
    );
    let output = run(&unstable, unstable_shared.path(), false);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("changed during execution"),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_cache_file_store_is_empty(unstable_shared.path());
}

fn assert_cache_file_store_is_empty(shared_cache: &Path) {
    let cache_files = shared_cache.join("cache-files");
    assert!(
        !cache_files.exists() || fs::read_dir(cache_files).unwrap().next().is_none(),
        "run unexpectedly published advisory state"
    );
}
