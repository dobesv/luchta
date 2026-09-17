//! Integration tests for the `--no-mem-pressure` flag and its
//! `LUCHTA_NO_MEM_PRESSURE` environment variable, and for the removal of the
//! `--mem-pressure` sensitivity ladder it replaced.

mod common;

use assert_cmd::Command;
use predicates::prelude::*;

use common::setup_workspace;

/// Runs `luchta run build` against a scratch workspace with the given extra
/// args and environment. The workspace's build task is expected to fail, so
/// these tests assert only on stderr content, never on exit status.
fn run_with(args: &[&str], env: &[(&str, &str)]) -> assert_cmd::assert::Assert {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1");
    for arg in args {
        cmd.arg(arg);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.assert()
}

#[test]
fn no_mem_pressure_flag_is_accepted() {
    run_with(&["--no-mem-pressure"], &[]).stderr(predicate::str::contains("unexpected").not());
}

#[test]
fn no_mem_pressure_env_var_is_honored() {
    run_with(&[], &[("LUCHTA_NO_MEM_PRESSURE", "1")])
        .stderr(predicate::str::contains("unexpected").not());
}

#[test]
fn removed_mem_pressure_flag_is_rejected() {
    run_with(&["--mem-pressure", "off"], &[])
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn removed_threshold_flags_are_rejected() {
    for flag in ["--mem-usage-threshold", "--mem-free-threshold"] {
        run_with(&[flag, "50%"], &[])
            .failure()
            .stderr(predicate::str::contains("unexpected argument"));
    }
}
