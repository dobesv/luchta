mod common;

use assert_cmd::Command;
use assert_fs::prelude::*;
use common::{init_git, write_counter_task_config, write_root_workspace};

fn run(temp: &assert_fs::TempDir, cache_dir: &std::path::Path, task: &str) -> String {
    let out = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg(task)
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env("LUCHTA_SHARED_CACHE_DIR", cache_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

/// Regression for #278: every task with no declared outputs used to map to the
/// single blob named by `combined_outputs_hash(&[])`, so only the first one to
/// store could ever be restored.
#[test]
fn two_no_output_tasks_both_hit_the_shared_cache() {
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&temp);

    write_counter_task_config(
        &temp,
        r#""app#lint":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"sleep 0.15 && count=$(cat ../../lint-counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > ../../lint-counter.txt"},"app#test":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"sleep 0.15 && count=$(cat ../../test-counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > ../../test-counter.txt"}}"#,
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

    run(&temp, shared_cache_dir.path(), "lint");
    run(&temp, shared_cache_dir.path(), "test");

    // Drop the local cache so the second pass has to come from the shared cache.
    std::fs::remove_dir_all(temp.path().join(".luchta/cache")).unwrap();

    let second_lint = run(&temp, shared_cache_dir.path(), "lint");
    let second_test = run(&temp, shared_cache_dir.path(), "test");

    assert!(
        second_lint.contains("📥 1"),
        "lint should be a shared hit, got:\n{second_lint}"
    );
    assert!(
        second_test.contains("📥 1"),
        "test should be a shared hit, got:\n{second_test}"
    );

    // Neither command body ran a second time.
    assert_eq!(
        std::fs::read_to_string(temp.path().join("lint-counter.txt")).unwrap(),
        "1\n"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("test-counter.txt")).unwrap(),
        "1\n"
    );
}
