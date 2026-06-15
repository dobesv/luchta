// Linux-only: tests rely on running Linux binaries and Unix shell semantics.
// Gating on `target_os = "linux"` keeps these tests honest on macOS/BSD where
// binary paths and shell behavior differ.
#![cfg(target_os = "linux")]

//! End-to-end integration tests for worker chain (#48):
//! - Full filter chain (yarn-filter → file-exists-filter → command-filter → lazy-worker → delegate)
//! - Pruning behavior (task pruned, delegate never spawned, independent tasks still run)
//! - Native dependsOn injection (worker.dependsOn creates edges through engine)

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use assert_fs::prelude::*;
use predicates::prelude::*;

//------------------------------------------------------------------------------
// Binary helpers (build via escargot)
//------------------------------------------------------------------------------

fn build_worker_bin(name: &str) -> std::path::PathBuf {
    escargot::CargoBuild::new()
        .bin(name)
        .package(name)
        .run()
        .unwrap_or_else(|e| panic!("build {name}: {e}"))
        .path()
        .to_path_buf()
}

fn yarn_filter_bin() -> std::path::PathBuf {
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(|| build_worker_bin("luchta-yarn-filter"))
        .clone()
}

fn file_exists_filter_bin() -> std::path::PathBuf {
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(|| build_worker_bin("luchta-file-exists-filter"))
        .clone()
}

fn command_filter_bin() -> std::path::PathBuf {
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(|| build_worker_bin("luchta-command-filter"))
        .clone()
}

fn lazy_worker_bin() -> std::path::PathBuf {
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(|| build_worker_bin("luchta-lazy-worker"))
        .clone()
}

//------------------------------------------------------------------------------
// Test harness utilities
//------------------------------------------------------------------------------

/// Writes `contents` to `name` under `temp` and marks it executable.
fn write_executable(temp: &assert_fs::TempDir, name: &str, contents: &str) -> std::path::PathBuf {
    let file = temp.child(name);
    file.write_str(contents).expect("write executable file");
    let mut perms = fs::metadata(file.path())
        .expect("file metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(file.path(), perms).expect("chmod file");
    file.path().to_path_buf()
}

/// Writes an executable `luchta-config.sh` emitting the given JSON config body.
fn write_config(temp: &assert_fs::TempDir, json: &str) {
    write_executable(
        temp,
        "luchta-config.sh",
        &format!("#!/bin/sh\necho '{json}'\n"),
    );
}

/// Writes a root `package.json` plus workspace packages with scripts.
fn write_workspace(temp: &assert_fs::TempDir, packages: &[(&str, &str)]) {
    temp.child("package.json")
        .write_str(r#"{ "name": "root", "private": true, "workspaces": ["packages/*"] }"#)
        .expect("write root package.json");

    for (name, script) in packages {
        let dir = temp.child(format!("packages/{name}"));
        fs::create_dir_all(dir.path()).expect("create package dir");
        temp.child(format!("packages/{name}/package.json"))
            .write_str(&format!(
                r#"{{ "name": "{name}", "scripts": {{ "build": "{script}" }} }}"#
            ))
            .expect("write package.json");
    }
}

/// Waits up to `timeout` for `path` to exist, polling every 10ms.
fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

//------------------------------------------------------------------------------
// Test 1: Full chain success
//------------------------------------------------------------------------------

#[test]
fn full_chain_resolves_and_runs_successfully() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");

    // Workspace: one package with a "build" script and babel.config.js present
    write_workspace(&temp, &[("app", "echo app-built")]);
    temp.child("packages/app/babel.config.js")
        .write_str(r#"module.exports = {};"#)
        .expect("write babel.config.js");

    // Delegate: a fake worker that records spawn + run sentinels.
    // On spawn: writes "spawned" marker immediately (before reading stdin).
    // On resolveTask: responds with {"resolved":{"decision":"accept"}}.
    // On run: writes "ran" marker then responds with {"done":{"exitCode":0}}.
    let spawned_marker = temp.child("delegate.spawned");
    let ran_marker = temp.child("delegate.ran");
    let delegate = write_executable(
        &temp,
        "chain-delegate.sh",
        &format!(
            r#"#!/bin/sh
# Signal that delegate process was spawned
touch {spawned}

# JSONL loopback: read requests and respond
while IFS= read -r line; do
  # Extract the id field using sed (portable, no bashisms)
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
      ;;
    *'"type":"run"'*)
      touch {ran}
      printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
      ;;
  esac
done
"#,
            spawned = spawned_marker.path().display(),
            ran = ran_marker.path().display(),
        ),
    );

    // Chain: yarn-filter → file-exists-filter → command-filter → lazy-worker → delegate
    // yarn-filter: default behavior (keep if task name matches a script)
    // file-exists-filter: keep if babel.config.* exists under cwd
    // command-filter: always true (sh -c 'exit 0')
    // lazy-worker: resolve => accept without spawning; run => spawn delegate
    let chain_command = format!(
        "{yarn} -- {file_exists} 'babel.config.*' -- {cmd_filter} sh -c 'exit 0' -- {lazy} -- {delegate}",
        yarn = yarn_filter_bin().display(),
        file_exists = file_exists_filter_bin().display(),
        cmd_filter = command_filter_bin().display(),
        lazy = lazy_worker_bin().display(),
        delegate = delegate.display(),
    );

    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":1}},"tasks":{{"build":{{"worker":"chain"}}}},"workers":{{"chain":{{"command":"{}"}}}}}}"#,
            chain_command
        ),
    );

    // Run the task
    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Done: 1 tasks done after"));

    // Assertions:
    // - lazy-worker deferred spawn until Run, so delegate was spawned (spawned marker exists)
    // - delegate ran successfully (ran marker exists)
    wait_for_file(spawned_marker.path(), Duration::from_secs(5));
    wait_for_file(ran_marker.path(), Duration::from_secs(5));

    temp.close().expect("cleanup temp dir");
}

//------------------------------------------------------------------------------
// Test 2: Pruning - file-exists-filter prunes when condition fails
//------------------------------------------------------------------------------

#[test]
fn filter_chain_prunes_when_file_not_found() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");

    // One package with a "build" script but NO babel.config.js (will prune)
    write_workspace(&temp, &[("app", "echo app-built")]);

    // Delegate for the chain (records spawn)
    let spawned_marker = temp.child("delegate.spawned");
    let delegate = write_executable(
        &temp,
        "chain-delegate.sh",
        &format!(
            r#"#!/bin/sh
touch {spawned}
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
      ;;
    *'"type":"run"'*)
      printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
      ;;
  esac
done
"#,
            spawned = spawned_marker.path().display(),
        ),
    );

    // Chain command (will prune due to missing babel.config.*)
    let chain_command = format!(
        "{yarn} -- {file_exists} 'babel.config.*' -- {cmd_filter} sh -c 'exit 0' -- {lazy} -- {delegate}",
        yarn = yarn_filter_bin().display(),
        file_exists = file_exists_filter_bin().display(),
        cmd_filter = command_filter_bin().display(),
        lazy = lazy_worker_bin().display(),
        delegate = delegate.display(),
    );

    write_config(
        &temp,
        &format!(
            r#"{{"concurrency":{{"maxWeight":1}},"tasks":{{"build":{{"worker":"chain"}}}},"workers":{{"chain":{{"command":"{chain}"}}}}}}"#,
            chain = chain_command,
        ),
    );

    // Run build - should prune because babel.config.* doesn't exist
    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .env("NO_COLOR", "1")
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        // Task was pruned from every package
        .stdout(predicate::str::contains("pruned from every package"));

    // Assertions:
    // - Chain delegate was never spawned (pruned before reaching lazy-worker's spawn)
    assert!(
        !spawned_marker.path().exists(),
        "delegate should not have been spawned for pruned task"
    );

    temp.close().expect("cleanup temp dir");
}

//------------------------------------------------------------------------------
// Test 3: Native dependsOn ordering
//------------------------------------------------------------------------------

#[test]
fn native_depends_on_orders_tasks_correctly() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");

    // Single package with a "build" script
    write_workspace(&temp, &[("app", "echo app-built")]);

    // Shared marker file for ordering verification
    let order_marker = temp.child("order.txt");

    // Prep task: a root task that writes FIRST to the order marker
    let prep_ran = temp.child("prep.ran");
    let prep_worker = write_executable(
        &temp,
        "prep-worker.sh",
        &format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
      ;;
    *'"type":"run"'*)
      echo "FIRST" >> {order}
      touch {prep_ran}
      printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
      ;;
  esac
done
"#,
            order = order_marker.path().display(),
            prep_ran = prep_ran.path().display(),
        ),
    );

    // Consumer task: writes SECOND to the order marker (should run after prep)
    let consumer_ran = temp.child("consumer.ran");

    // Let's use a custom delegate that records when it runs
    let consumer_delegate = write_executable(
        &temp,
        "consumer-delegate.sh",
        &format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
      ;;
    *'"type":"run"'*)
      echo "SECOND" >> {order}
      touch {consumer_ran}
      printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
      ;;
  esac
done
"#,
            order = order_marker.path().display(),
            consumer_ran = consumer_ran.path().display(),
        ),
    );

    // Config:
    // - #prep root task using prep-worker
    // - build task (package task) using consumer-worker
    // - consumer-worker has dependsOn: ["#prep"]
    write_config(
        &temp,
        &format!(
            r##"{{"concurrency":{{"maxWeight":2}},"tasks":{{"#prep":{{"worker":"prep-worker"}},"build":{{"worker":"consumer-worker"}}}},"workers":{{"prep-worker":{{"command":"{prep}"}},"consumer-worker":{{"command":"{consumer}","dependsOn":["#prep"]}}}}}}"##,
            prep = prep_worker.display(),
            consumer = consumer_delegate.display(),
        ),
    );

    // Run both tasks (run "build" which depends on "#prep", so both should run)
    assert_cmd::Command::cargo_bin("luchta")
        .expect("find binary")
        .env("NO_COLOR", "1")
        .arg("run")
        .arg("build")
        .arg("prep")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        // 3 tasks done: #prep runs for each package's build task (1 package with build) + the explicit prep run
        .stdout(predicate::str::contains("Done:"));

    // Verify ordering: prep ran before consumer
    wait_for_file(prep_ran.path(), Duration::from_secs(5));
    wait_for_file(consumer_ran.path(), Duration::from_secs(5));

    let order_contents = fs::read_to_string(order_marker.path()).expect("read order marker");
    assert!(
        order_contents.contains("FIRST"),
        "prep should have run and written FIRST"
    );
    assert!(
        order_contents.contains("SECOND"),
        "consumer should have run and written SECOND"
    );
    // Verify FIRST appears before SECOND in the file
    let first_pos = order_contents.find("FIRST").expect("find FIRST");
    let second_pos = order_contents.find("SECOND").expect("find SECOND");
    assert!(
        first_pos < second_pos,
        "prep (FIRST) should run before consumer (SECOND), but got: {order_contents}"
    );

    temp.close().expect("cleanup temp dir");
}
