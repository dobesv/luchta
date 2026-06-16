//! Shared fixtures for `luchta` CLI integration tests.

use std::fs;

use assert_fs::prelude::*;

/// Writes a minimal two-package Yarn workspace (`a`, and `b` depending on `a`)
/// into `temp`, suitable for exercising `luchta run`/`check` against.
pub fn setup_workspace(temp: &assert_fs::TempDir) {
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
