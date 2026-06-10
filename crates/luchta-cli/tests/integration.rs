//! Integration tests for `luchta run` command.

use std::fs;

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn setup_workspace(temp: &assert_fs::TempDir) {
    // Root package.json with workspaces
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

    // Package a - no dependencies
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

    // Package b - depends on a
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

    // luchta executable config
    let config = temp.child("luchta-config.sh");
    config
        .write_str(
            "#!/bin/sh\necho '{\"concurrency\":{\"maxWeight\":4},\"pipeline\":{\"build\":{\"dependsOn\":[\"^build\"]}}}'\n",
        )
        .expect("write luchta-config.sh");
}

#[test]
fn run_build_succeeds_and_runs_in_order() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("a#build | built-a")
                .and(predicate::str::contains("b#build | built-b")),
        );

    // Verify order: a comes before b
    temp.close().expect("cleanup temp dir");
}

#[test]
fn run_fails_on_script_failure() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");

    // Root package.json with workspaces
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

    // Single package with failing script
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

    // luchta executable config
    let config = temp.child("luchta-config.sh");
    config
        .write_str(
            r#"#!/bin/sh
echo '{"concurrency":{"maxWeight":4},"pipeline":{"build":{}}}'
"#,
        )
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

#[test]
fn run_fails_on_malformed_package_json() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");

    // Root package.json with workspaces
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

    // Package a with valid package.json (depends on b)
    let pkg_a_dir = temp.child("packages/a");
    fs::create_dir_all(pkg_a_dir.path()).expect("create packages/a dir");
    let pkg_a = temp.child("packages/a/package.json");
    pkg_a
        .write_str(
            r#"{
    "name": "a",
    "scripts": {
        "build": "echo built-a"
    },
    "dependencies": {
        "b": "workspace:*"
    }
}"#,
        )
        .expect("write packages/a/package.json");

    // Package b with malformed package.json (truncated/invalid JSON)
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
    // missing closing brace and invalid JSON trailing
"#,
        )
        .expect("write packages/b/package.json (malformed)");

    // luchta executable config
    let config = temp.child("luchta-config.sh");
    config
        .write_str(
            "#!/bin/sh\necho '{\"concurrency\":{\"maxWeight\":4},\"pipeline\":{\"build\":{\"dependsOn\":[\"^build\"]}}}'\n",
        )
        .expect("write luchta-config.sh");

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .failure()
        // Error can come from workspace discovery or resolve_command, both should surface parse error
        .stderr(
            predicate::str::contains("parse")
                .and(predicate::str::contains("packages/b/package.json")),
        );

    temp.close().expect("cleanup temp dir");
}
