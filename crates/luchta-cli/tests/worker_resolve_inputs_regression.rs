use std::{process::Command, thread, time::{Duration, Instant}};

use assert_cmd::prelude::*;
use assert_fs::prelude::*;
use assert_fs::TempDir;
use predicates::prelude::*;

mod common;

fn run_luchta(temp: &TempDir, task: &str) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.arg("run")
        .arg(task)
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
}

fn wait_for_path(path: &std::path::Path, timeout: Duration, label: &str) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {label}: {}", path.display());
}

fn write_resolve_inputs_worker(temp: &TempDir, resolved_inputs_json: &str) {
    let script = temp.child("resolve-inputs-worker.sh");
    script
        .write_str(&format!(
            r#"#!/bin/sh
set -eu
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"modify","inputs":{}}}}}\n' "$id"
      ;;
    *'"type":"run"'*)
      cmd=$(printf '%s\n' "$line" | sed -n 's/.*"command":"\([^"]*\)".*/\1/p' | sed 's/\\"/"/g; s/\\\\/\\/g')
      cwd=$(printf '%s\n' "$line" | sed -n 's/.*"cwd":"\([^"]*\)".*/\1/p' | sed 's/\\"/"/g; s/\\\\/\\/g')
      (
        cd "$cwd"
        sh -lc "$cmd"
      )
      code=$?
      printf '{{"type":"done","id":"%s","exitCode":%s}}\n' "$id" "$code"
      ;;
  esac
done
"#,
            resolved_inputs_json
        ))
        .unwrap();
    common::set_executable(script.path());
}

fn init_basic_worker_workspace(temp: &TempDir, worker_inputs_json: &str, task_json: &str) {
    common::write_root_workspace(temp);
    temp.child("yarn.lock").write_str("").unwrap();
    write_resolve_inputs_worker(temp, worker_inputs_json);
    common::write_task_config_with_shell_worker(
        temp,
        temp.child("resolve-inputs-worker.sh").path(),
        task_json,
    );
    temp.child("packages/app").create_dir_all().unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "build": "echo ignored"
  }
}"#,
        )
        .unwrap();
    common::init_git(temp);
}

#[test]
fn worker_resolve_inputs_toctou_edit_skips_cache_write_and_reruns() {
    let temp = TempDir::new().unwrap();
    let task_json = r##""app#build":{"cache":{},"worker":"shell","inputs":["declared-ignored.txt"],"outputs":["counter.txt","out.txt"],"command":"cp resolved-input.txt out.txt; printf started > started-run; while [ ! -f allow-finish ]; do sleep 0.01; done; count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"##;
    init_basic_worker_workspace(&temp, r#"["resolved-input.txt"]"#, task_json);

    let pkg = temp.child("packages/app");
    let started = pkg.child("started-run");
    let allow_finish = pkg.child("allow-finish");
    pkg.child("resolved-input.txt").write_str("H1\n").unwrap();
    pkg.child("declared-ignored.txt").write_str("declared\n").unwrap();

    let workspace = temp.path().to_path_buf();
    let handle = thread::spawn(move || {
        let mut cmd = Command::cargo_bin("luchta").unwrap();
        cmd.arg("run")
            .arg("build")
            .arg("--workspace-root")
            .arg(&workspace)
            .output()
            .expect("luchta run completes")
    });

    wait_for_path(started.path(), Duration::from_secs(30), "worker start sentinel");
    pkg.child("resolved-input.txt").write_str("H2\n").unwrap();
    allow_finish.write_str("go\n").unwrap();

    let output = handle.join().expect("thread join");
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("skipping cache write"), "stderr: {stderr}");
    pkg.child("counter.txt").assert("1\n");
    pkg.child("out.txt").assert("H1\n");

    run_luchta(&temp, "build").success();
    pkg.child("counter.txt").assert("2\n");
    pkg.child("out.txt").assert("H2\n");
}

#[test]
fn worker_resolve_inputs_narrowing_replaces_declared_inputs_end_to_end() {
    let temp = TempDir::new().unwrap();
    let task_json = r##""app#build":{"cache":{},"worker":"shell","inputs":["kept.txt","ignored.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"##;
    init_basic_worker_workspace(&temp, r#"["kept.txt"]"#, task_json);

    let pkg = temp.child("packages/app");
    pkg.child("kept.txt").write_str("A\n").unwrap();
    pkg.child("ignored.txt").write_str("X\n").unwrap();

    run_luchta(&temp, "build").success();
    pkg.child("counter.txt").assert("1\n");

    run_luchta(&temp, "build").success();
    pkg.child("counter.txt").assert("1\n");

    pkg.child("ignored.txt").write_str("Y\n").unwrap();
    common::git_commit_paths(temp.path(), &["packages/app/ignored.txt"], "edit ignored file");
    run_luchta(&temp, "build").success();
    pkg.child("counter.txt").assert("1\n");

    pkg.child("kept.txt").write_str("B\n").unwrap();
    common::git_commit_paths(temp.path(), &["packages/app/kept.txt"], "edit kept file");
    run_luchta(&temp, "build").success();
    pkg.child("counter.txt").assert("2\n");
}

#[test]
fn worker_resolve_inputs_escape_path_fails_like_declared_inputs() {
    let temp = TempDir::new().unwrap();
    let task_json = r##""app#build":{"cache":{},"worker":"shell","outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"##;
    init_basic_worker_workspace(&temp, "[\"#../escape.txt\"]", task_json);

    run_luchta(&temp, "build")
        .failure()
        .stderr(predicate::str::contains("escape").or(predicate::str::contains("outside")));
}
