//! Tests for skip accounting: "skipped" count = cache-hit ONLY.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use assert_fs::prelude::*;

fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn init_git(temp: &assert_fs::TempDir) {
    git(temp.path(), &["init"]);
    git(temp.path(), &["config", "user.name", "Luchta Tests"]);
    git(temp.path(), &["config", "user.email", "luchta@example.com"]);
    git(temp.path(), &["add", "."]);
    git(temp.path(), &["commit", "-m", "fixture"]);
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    set_executable(path);
}

fn shell_worker_with_done_fields(
    temp: &assert_fs::TempDir,
    done_fields: Option<&str>,
) -> assert_fs::fixture::ChildPath {
    let done_fields = done_fields.unwrap_or("");
    let script = temp.child("shell-worker.sh");
    script
        .write_str(&format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
      ;;
    *'"type":"run"'*)
      cmd=$(printf '%s\n' "$line" | sed -n 's/.*"command":"\([^"]*\)".*/\1/p' | sed 's/\\"/"/g; s/\\\\/\\/g')
      cwd=$(printf '%s\n' "$line" | sed -n 's/.*"cwd":"\([^"]*\)".*/\1/p' | sed 's/\\"/"/g; s/\\\\/\\/g')
      (cd "$cwd" && sh -lc "$cmd")
      code=$?
      printf '{{"type":"done","id":"%s","exitCode":%s{}}}\n' "$id" "$code"
      ;;
  esac
done
"#,
            done_fields
        ))
        .unwrap();
    set_executable(script.path());
    script
}

fn write_root_workspace(temp: &assert_fs::TempDir) {
    temp.child("package.json")
        .write_str(
            r#"{
  "name": "root",
  "private": true,
  "workspaces": ["packages/*"]
}"#,
        )
        .unwrap();
}

fn write_counter_task_config(temp: &assert_fs::TempDir, task_json: &str) {
    let worker = shell_worker_with_done_fields(temp, Some(",\"outputs\":[\"counter.txt\"]"));
    let worker_path = worker.path().display();
    let config_content = format!(
        r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"shell":{{"command":"{worker_path}"}}}},"tasks":{{{task_json}}}}}'
"#,
    );
    write_executable(temp.child("luchta-config.sh").path(), &config_content);
}

fn setup_pkgbuild_counter_workspace(
    temp: &assert_fs::TempDir,
    task_json: &str,
    extra_files: &[(&str, &str)],
) {
    write_root_workspace(temp);
    temp.child("yarn.lock").write_str("").unwrap();

    temp.child("packages/app").create_dir_all().unwrap();
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
    temp.child("packages/app/src.txt")
        .write_str("one\n")
        .unwrap();

    write_counter_task_config(temp, task_json);

    for (path, contents) in extra_files {
        temp.child(path).write_str(contents).unwrap();
    }

    init_git(temp);
}

/// Test 4: Skip accounting - second run all cache-hit, skipped count matches task count.
#[test]
fn skip_count_is_cache_hit_only() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_pkgbuild_counter_workspace(
        &temp,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt","out.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat src.txt > out.txt"}"#,
        &[],
    );

    // First run: executes task, no skips
    let output1 = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("first run");

    let stdout1 = String::from_utf8_lossy(&output1.stdout);
    assert!(
        output1.status.success(),
        "first run should succeed, stderr: {}",
        String::from_utf8_lossy(&output1.stderr)
    );
    // First run: 1 done, 0 skipped
    assert!(
        stdout1.contains("Done: 1 tasks done"),
        "first run stdout should contain 'Done: 1 tasks done', got: {stdout1}"
    );
    assert!(
        !stdout1.contains("skipped"),
        "first run should not mention 'skipped', got: {stdout1}"
    );
    temp.child("packages/app/counter.txt").assert("1\n");

    // Second run: cache-hit, skipped = 1
    let output2 = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("pkgbuild")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("second run");

    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    assert!(
        output2.status.success(),
        "second run should succeed, stderr: {}",
        String::from_utf8_lossy(&output2.stderr)
    );
    // Second run: 0 done, 1 skipped (cache-hit)
    assert!(
        stdout2.contains("Done: 0 tasks done"),
        "second run stdout should contain 'Done: 0 tasks done', got: {stdout2}"
    );
    assert!(
        stdout2.contains(", 1 skipped"),
        "second run should show ', 1 skipped' (cache-hit), got: {stdout2}"
    );
    // Counter unchanged (cache hit, not re-executed)
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.close().expect("cleanup temp dir");
}

/// Test 4b: no-command tasks are NOT counted as skipped.
#[test]
fn no_command_tasks_not_counted_as_skipped() {
    let temp = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&temp);
    temp.child("yarn.lock").write_str("").unwrap();

    // Two packages: a with cacheable task, b with no-command task
    temp.child("packages/a").create_dir_all().unwrap();
    temp.child("packages/a/package.json")
        .write_str(
            r#"{
  "name": "a",
  "scripts": {
    "build": "echo ignored"
  }
}"#,
        )
        .unwrap();
    temp.child("packages/a/src.txt").write_str("one\n").unwrap();

    temp.child("packages/b").create_dir_all().unwrap();
    temp.child("packages/b/package.json")
        .write_str(
            r#"{
  "name": "b",
  "scripts": {
    "build": "echo ignored"
  }
}"#,
        )
        .unwrap();

    let worker = shell_worker_with_done_fields(&temp, Some(",\"outputs\":[\"counter.txt\"]"));
    let worker_path = worker.path().display();
    let config_content = format!(
        r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"shell":{{"command":"{worker_path}"}}}},"tasks":{{"build":{{}},"a#build":{{"cache":{{}},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}}}}}}'
"#,
    );
    write_executable(temp.child("luchta-config.sh").path(), &config_content);

    init_git(&temp);

    // Run build task - a#build runs (cacheable), b#build has no command
    let output = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("run build");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "run should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The global `build` now expands only to child packages. `a#build` runs and
    // `b#build` is an ordering-only no-command node counted as done; root no
    // longer gets a `build` node from global expansion. None are skipped
    // (no-command nodes are NOT "skipped" — only cache hits are).
    assert!(
        stdout.contains("Done: 2 tasks done"),
        "stdout should contain 'Done: 2 tasks done', got: {stdout}"
    );
    assert!(
        !stdout.contains("skipped"),
        "no-command tasks should not be counted as 'skipped', got: {stdout}"
    );

    // Second run: a#build is cache-hit, b#build still has no command
    let output2 = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("second run");

    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    assert!(
        output2.status.success(),
        "second run should succeed, stderr: {}",
        String::from_utf8_lossy(&output2.stderr)
    );
    // Second run: `a#build` is a cache hit (the only "skipped"); only the
    // no-command `b#build` still counts as done because root no longer gets a
    // `build` node from global expansion.
    assert!(
        stdout2.contains("Done: 1 tasks done"),
        "second run should show 'Done: 1 tasks done', got: {stdout2}"
    );
    assert!(
        stdout2.contains(", 1 skipped"),
        "second run should show ', 1 skipped' (only cache-hit), got: {stdout2}"
    );

    temp.close().expect("cleanup temp dir");
}
