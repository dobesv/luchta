//! Integration tests for `luchta` commands.

use std::fs;

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn make_worker_script(
    temp: &assert_fs::TempDir,
    name: &str,
    body: &str,
) -> assert_fs::fixture::ChildPath {
    let script = temp.child(name);
    script.write_str(body).expect("write worker script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(script.path(), perms).expect("chmod worker script");
    }
    script
}

fn shell_worker_body(done_json: &str, extra_json: &str) -> String {
    format!(
        "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"resolveTask\"'*)\n      id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p')\n      printf '{{\"type\":\"resolved\",\"id\":\"%s\",\"result\":{{\"decision\":\"accept\"}}}}\\n' \"$id\"\n      ;;\n    *'\"type\":\"run\"'*)\n      id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p')\n      {extra_json}      printf '{done_json}\\n' \"$id\"\n      ;;\n  esac\ndone\n"
    )
}

fn setup_workspace(temp: &assert_fs::TempDir) {
    let root_pkg = temp.child("package.json");
    root_pkg
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    let pkg_a_dir = temp.child("packages/a");
    fs::create_dir_all(pkg_a_dir.path()).expect("create packages/a dir");
    let pkg_a = temp.child("packages/a/package.json");
    pkg_a
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "echo built-a"
    }
}"#,
        )
        .expect("write packages/a/package.json");

    let pkg_b_dir = temp.child("packages/b");
    fs::create_dir_all(pkg_b_dir.path()).expect("create packages/b dir");
    let pkg_b = temp.child("packages/b/package.json");
    pkg_b
        .write_str(
            r#"{
    "name": "b",
    "scripts": {
        "build": "echo built-b"
    },
    "dependencies": {
        "a": "workspace:*"
    }
}"#,
        )
        .expect("write packages/b/package.json");
}

#[test]
#[ignore = "covered by worker_integration; shell worker fixture can hang under cargo test harness"]
fn run_executes_worker_task() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);
    let worker_script = make_worker_script(
        &temp,
        "fake-worker.sh",
        &shell_worker_body(
            r#"{"type":"done","id":"%s","success":true}"#,
            "      printf '{\"type\":\"log\",\"stream\":\"stdout\",\"id\":\"%s\",\"message\":\"worker-ran\"}\\n' \"$id\"\n",
        ),
    );

    temp.child("luchta-config.sh")
        .write_str(&format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"fake\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"worker\":\"fake\"}}}}}}'\n",
            worker_script.path().display()
        ))
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        // Successful task output is captured (not streamed) in default mode, so
        // the worker's "worker-ran" log line no longer appears on stdout. A
        // successful run is confirmed by the Done summary.
        .stdout(predicate::str::contains("Done: 1 tasks done after "))
        .stdout(predicate::str::contains("worker-ran").not());

    temp.close().expect("cleanup temp dir");
}

#[test]
fn run_skips_task_without_worker_and_command() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);
    temp.child("luchta-config.sh")
        .write_str(
            "#!/bin/sh\necho '{\"concurrency\":{\"maxWeight\":4},\"tasks\":{\"build\":{}}}'\n",
        )
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success();

    temp.close().expect("cleanup temp dir");
}

#[test]
fn run_rejects_command_without_worker() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);
    temp.child("luchta-config.sh")
        .write_str(
            "#!/bin/sh\necho '{\"concurrency\":{\"maxWeight\":4},\"tasks\":{\"build\":{\"command\":\"echo nope\"}}}'\n",
        )
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("defines a command but no worker"));

    temp.close().expect("cleanup temp dir");
}

#[test]
fn run_does_not_abort_unrelated_tasks_for_command_without_worker() {
    // A misconfigured task (command without worker) must NOT prevent unrelated
    // tasks from running. Running `lint` (a no-op) succeeds even though a
    // separate `build` task is misconfigured, because `build` is not part of
    // `lint`'s dependency closure.
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);
    temp.child("luchta-config.sh")
        .write_str(
            "#!/bin/sh\necho '{\"concurrency\":{\"maxWeight\":4},\"tasks\":{\"lint\":{},\"build\":{\"command\":\"echo nope\"}}}'\n",
        )
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("run")
        .arg("lint")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success();

    temp.close().expect("cleanup temp dir");
}

#[test]
fn check_reports_configuration_valid_for_valid_workspace() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    temp.child("package.json")
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    fs::create_dir_all(temp.child("packages/a").path()).expect("create packages/a dir");
    temp.child("packages/a/package.json")
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "echo built-a"
    }
}"#,
        )
        .expect("write packages/a/package.json");
    fs::create_dir_all(temp.child("packages/b").path()).expect("create packages/b dir");
    temp.child("packages/b/package.json")
        .write_str(
            r#"{
    "name": "b",
    "scripts": {
        "build": "echo built-b"
    },
    "dependencies": {
        "a": "workspace:*"
    }
}"#,
        )
        .expect("write packages/b/package.json");

    let worker_script = make_worker_script(
        &temp,
        "fake-worker-check.sh",
        &shell_worker_body(r#"{"type":"done","id":"%s","success":true}"#, ""),
    );

    temp.child("luchta-config.sh")
        .write_str(&format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"fake\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"worker\":\"fake\"}},\"b#build\":{{\"dependsOn\":[\"^build\"],\"worker\":\"fake\"}}}}}}'\n",
            worker_script.path().display()
        ))
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("check")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Configuration valid"));

    temp.close().expect("cleanup temp dir");
}

#[test]
fn check_reports_dead_dependencies() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    temp.child("package.json")
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");
    fs::create_dir_all(temp.child("packages/a").path()).expect("create package dir");
    temp.child("packages/a/package.json")
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "echo build"
    }
}"#,
        )
        .expect("write package manifest");
    temp.child("luchta-config.sh")
        .write_str(
            "#!/bin/sh\necho '{\"concurrency\":{\"maxWeight\":4},\"tasks\":{\"build\":{\"dependsOn\":[\"ghost\",\"missing#build\",\"#audit-licenses\"]}}}'\n",
        )
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("check")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("task validation failed")
                .and(predicate::str::contains("a#build -> ghost"))
                .and(predicate::str::contains("a#build -> missing#build"))
                .and(predicate::str::contains("a#build -> #audit-licenses")),
        );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn check_reports_command_without_worker() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);
    temp.child("luchta-config.sh")
        .write_str(
            "#!/bin/sh\necho '{\"concurrency\":{\"maxWeight\":4},\"tasks\":{\"build\":{\"command\":\"echo nope\"}}}'\n",
        )
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("check")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("defines a command but no worker"));

    temp.close().expect("cleanup temp dir");
}

#[test]
fn check_reports_env_conflict_in_task() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    temp.child("package.json")
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    fs::create_dir_all(temp.child("packages/a").path()).expect("create packages/a dir");
    temp.child("packages/a/package.json")
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "echo built-a"
    }
}"#,
        )
        .expect("write packages/a/package.json");

    let worker_script = make_worker_script(
        &temp,
        "fake-worker-env-check.sh",
        &shell_worker_body(r#"{"type":"done","id":"%s","success":true}"#, ""),
    );

    // Task with env that has BOTH value and default set (conflict)
    temp.child("luchta-config.sh")
        .write_str(&format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"fake\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"worker\":\"fake\",\"env\":{{\"MY_VAR\":{{\"value\":\"explicit\",\"default\":\"fallback\"}}}}}}}}}}'\n",
            worker_script.path().display()
        ))
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("check")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("task validation failed")
                .and(predicate::str::contains("MY_VAR"))
                .and(predicate::str::contains("task 'build'"))
                .and(predicate::str::contains("setDefault conflict")),
        );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn check_reports_env_conflict_in_global() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    temp.child("package.json")
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    fs::create_dir_all(temp.child("packages/a").path()).expect("create packages/a dir");
    temp.child("packages/a/package.json")
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "echo built-a"
    }
}"#,
        )
        .expect("write packages/a/package.json");

    let worker_script = make_worker_script(
        &temp,
        "fake-worker-global-env.sh",
        &shell_worker_body(r#"{"type":"done","id":"%s","success":true}"#, ""),
    );

    // Global env with conflict (value + default)
    temp.child("luchta-config.sh")
        .write_str(&format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"fake\":{{\"command\":\"{}\"}}}},\"env\":{{\"GLOBAL_VAR\":{{\"value\":\"x\",\"default\":\"y\"}}}},\"tasks\":{{\"build\":{{\"worker\":\"fake\"}}}}}}'\n",
            worker_script.path().display()
        ))
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("check")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("task validation failed")
                .and(predicate::str::contains("GLOBAL_VAR"))
                .and(predicate::str::contains("global"))
                .and(predicate::str::contains("setDefault conflict")),
        );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn check_reports_env_conflict_in_worker() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    temp.child("package.json")
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    fs::create_dir_all(temp.child("packages/a").path()).expect("create packages/a dir");
    temp.child("packages/a/package.json")
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "echo built-a"
    }
}"#,
        )
        .expect("write packages/a/package.json");

    let worker_script = make_worker_script(
        &temp,
        "fake-worker-conflict.sh",
        &shell_worker_body(r#"{"type":"done","id":"%s","success":true}"#, ""),
    );

    // Worker with env conflict
    temp.child("luchta-config.sh")
        .write_str(&format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"fake\":{{\"command\":\"{}\",\"env\":{{\"WORKER_VAR\":{{\"value\":\"x\",\"default\":\"y\"}}}}}}}},\"tasks\":{{\"build\":{{\"worker\":\"fake\"}}}}}}'\n",
            worker_script.path().display()
        ))
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("check")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("task validation failed")
                .and(predicate::str::contains("WORKER_VAR"))
                .and(predicate::str::contains("worker 'fake'"))
                .and(predicate::str::contains("setDefault conflict")),
        );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn run_ignores_env_conflict() {
    // `luchta run` must NOT error on env conflicts - arbitrary runtime behavior accepted
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    temp.child("package.json")
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    fs::create_dir_all(temp.child("packages/a").path()).expect("create packages/a dir");
    temp.child("packages/a/package.json")
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "echo built-a"
    }
}"#,
        )
        .expect("write packages/a/package.json");

    let worker_script = make_worker_script(
        &temp,
        "fake-worker-run-ignore.sh",
        &shell_worker_body(r#"{"type":"done","id":"%s","exitCode":0}"#, ""),
    );

    // Task with env conflict, but run should still succeed
    temp.child("luchta-config.sh")
        .write_str(&format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"fake\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"worker\":\"fake\",\"env\":{{\"MY_VAR\":{{\"value\":\"explicit\",\"default\":\"fallback\"}}}}}}}}}}'\n",
            worker_script.path().display()
        ))
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    // `luchta run` should succeed despite the conflict
    cmd.arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success();

    temp.close().expect("cleanup temp dir");
}

#[test]
#[ignore = "covered by worker_integration; shell worker fixture can hang under cargo test harness"]
fn run_fails_on_script_failure() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");

    let root_pkg = temp.child("package.json");
    root_pkg
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    let pkg_a_dir = temp.child("packages/a");
    fs::create_dir_all(pkg_a_dir.path()).expect("create packages/a dir");
    let pkg_a = temp.child("packages/a/package.json");
    pkg_a
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "exit 1"
    }
}"#,
        )
        .expect("write packages/a/package.json");

    let worker_script = make_worker_script(
        &temp,
        "failing-worker.sh",
        &shell_worker_body(r#"{"type":"done","id":"%s","success":false}"#, ""),
    );

    let config = temp.child("luchta-config.sh");
    config
        .write_str(&format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"fake\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"worker\":\"fake\"}}}}}}'\n",
            worker_script.path().display()
        ))
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .failure();

    temp.close().expect("cleanup temp dir");
}

/// Sets up a two-package workspace with `config_json`, runs `luchta run <task>
/// --dry-run`, and returns the assertion handle for the caller to check.
fn dry_run_assert(
    temp: &assert_fs::TempDir,
    config_json: &str,
    task: &str,
) -> assert_cmd::assert::Assert {
    setup_workspace(temp);
    temp.child("luchta-config.sh")
        .write_str(&format!("#!/bin/sh\necho '{config_json}'\n"))
        .expect("write luchta-config.sh");

    Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg(task)
        .arg("--dry-run")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .assert()
}

fn dry_run_assert_top_level(
    temp: &assert_fs::TempDir,
    config_json: &str,
    task: &str,
) -> assert_cmd::assert::Assert {
    setup_workspace(temp);
    temp.child("luchta-config.sh")
        .write_str(&format!("#!/bin/sh\necho '{config_json}'\n"))
        .expect("write luchta-config.sh");

    Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg(task)
        .arg("-T")
        .arg("--dry-run")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .assert()
}

#[test]
fn dry_run_global_build_excludes_named_root_tasks() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);
    temp.child("package.json")
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"],
    "scripts": {
        "build": "echo root-build"
    }
}"#,
        )
        .expect("write root package.json");
    let config = r#"{"concurrency":{"maxWeight":4},"tasks":{"build":{}}}"#;

    temp.child("luchta-config.sh")
        .write_str(&format!("#!/bin/sh\necho '{config}'\n"))
        .expect("write luchta-config.sh");

    Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--dry-run")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("a#build")
                .and(predicate::str::contains("b#build"))
                .and(predicate::str::contains("root#build").not())
                .and(predicate::str::contains("//root#build").not()),
        );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn dry_run_top_level_build_selects_only_root_task() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    let config = r##"{"concurrency":{"maxWeight":4},"tasks":{"#build":{}}}"##;

    dry_run_assert_top_level(&temp, config, "build")
        .success()
        .stdout(
            predicate::str::contains("#build")
                .and(predicate::str::contains("a#build").not())
                .and(predicate::str::contains("b#build").not())
                .and(predicate::str::contains("//root#build").not())
                .and(predicate::str::contains("root#build").not()),
        );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn dry_run_prints_waves_without_executing() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);

    // `build` depends on the upstream package's `build` (^build). Package `b`
    // depends on `a`, so a#build must run in an earlier wave than b#build.
    //
    // The worker participates in the resolution phase (it accepts every task so
    // the graph builds), but a task being *run* would append to the `executed`
    // marker. Dry-run must build the graph WITHOUT executing, so the marker
    // stays empty.
    let executed = temp.child("executed.log");
    executed.write_str("").expect("init executed marker");
    let worker_script = make_worker_script(
        &temp,
        "dry-run-worker.sh",
        &shell_worker_body(
            r#"{"type":"done","id":"%s","exitCode":0}"#,
            &format!("      printf ran >> '{}'\n", executed.path().display()),
        ),
    );
    let config = format!(
        r#"{{"concurrency":{{"maxWeight":4}},"workers":{{"yarn":{{"command":"{}"}}}},"tasks":{{"build":{{"dependsOn":["^build"],"worker":"yarn"}}}}}}"#,
        worker_script.path().display()
    );

    temp.child("luchta-config.sh")
        .write_str(&format!("#!/bin/sh\necho '{config}'\n"))
        .expect("write luchta-config.sh");

    Command::cargo_bin("luchta")
        .expect("find binary")
        .arg("run")
        .arg("build")
        .arg("--dry-run")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("dry-run:")
                .and(predicate::str::contains("Wave 1:"))
                .and(predicate::str::contains("Wave 2:"))
                .and(predicate::str::contains("a#build"))
                .and(predicate::str::contains("b#build"))
                .and(predicate::str::contains("worker 'yarn'")),
        );

    // No task was executed: the run marker is still empty.
    let ran = fs::read_to_string(executed.path()).expect("read executed marker");
    assert!(
        ran.is_empty(),
        "dry-run must not execute tasks, got: {ran:?}"
    );

    temp.close().expect("cleanup temp dir");
}

#[test]
fn dry_run_reports_unknown_task() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    let config = r#"{"concurrency":{"maxWeight":4},"tasks":{"build":{}}}"#;

    dry_run_assert(&temp, config, "does-not-exist")
        .failure()
        .stderr(predicate::str::contains("not found in task graph"));

    temp.close().expect("cleanup temp dir");
}
