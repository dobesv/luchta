//! Shared-cache output-capture integration tests.

use std::path::Path;

use assert_cmd::Command;
use assert_fs::prelude::*;

mod common;

const CAPTURED_STDOUT: &str = "captured shared stdout";
const CAPTURED_STDERR: &str = "captured shared stderr";

fn logging_worker(temp: &assert_fs::TempDir) -> assert_fs::fixture::ChildPath {
    let worker = temp.child("logging-worker.sh");
    worker
        .write_str(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{"type":"resolved","id":"%s","result":{"decision":"accept"}}\n' "$id"
      ;;
    *'"type":"run"'*)
      printf '{"type":"log","id":"%s","stream":"stdout","line":"captured shared stdout"}\n' "$id"
      printf '{"type":"log","id":"%s","stream":"stderr","line":"captured shared stderr"}\n' "$id"
      printf '{"type":"done","id":"%s","exitCode":0}\n' "$id"
      ;;
  esac
done
"#,
        )
        .unwrap();
    common::set_executable(worker.path());
    worker
}

fn run_with_shared_cache(
    temp: &assert_fs::TempDir,
    shared_cache_dir: &Path,
) -> std::process::Output {
    Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .env("LUCHTA_SHARED_CACHE", "1")
        .env("LUCHTA_SHARED_CACHE_MIN_DURATION_MS", "0")
        .env("LUCHTA_SHARED_CACHE_DIR", shared_cache_dir)
        .assert()
        .success()
        .get_output()
        .clone()
}

fn assert_captured_output_is_quiet(output: &std::process::Output, context: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains(CAPTURED_STDOUT),
        "{context} printed captured stdout: {stdout}"
    );
    assert!(
        !stderr.contains(CAPTURED_STDERR),
        "{context} printed captured stderr: {stderr}"
    );
}

#[test]
fn shared_cache_hit_keeps_restored_task_output_captured() {
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();
    common::setup_workspace(&temp);

    let worker = logging_worker(&temp);
    common::write_task_config_with_shell_worker(
        &temp,
        worker.path(),
        r#""a#build":{"cache":{},"worker":"shell","inputs":[],"outputs":[],"command":"ignored"}"#,
    );
    common::init_git(&temp);

    let first = run_with_shared_cache(&temp, shared_cache_dir.path());
    assert_captured_output_is_quiet(&first, "fresh run");

    std::fs::remove_dir_all(temp.child(".luchta/cache").path()).unwrap();

    let second = run_with_shared_cache(&temp, shared_cache_dir.path());
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        second_stdout.contains("📥 1"),
        "second build should restore from shared cache: {second_stdout}"
    );
    assert_captured_output_is_quiet(&second, "shared-cache hit");

    let logs = Command::cargo_bin("luchta")
        .unwrap()
        .env("NO_COLOR", "1")
        .arg("logs")
        .arg("build")
        .arg("-p")
        .arg("a")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let logs = String::from_utf8(logs).unwrap();
    assert!(logs.contains(CAPTURED_STDOUT), "logs: {logs}");
    assert!(logs.contains(CAPTURED_STDERR), "logs: {logs}");
}
