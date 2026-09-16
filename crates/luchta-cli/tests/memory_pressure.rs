//! Integration tests for the `--mem-pressure` flag and its `LUCHTA_MEM_PRESSURE`
//! environment variable, and for the removal of the threshold flags it replaced.

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
fn every_sensitivity_level_is_accepted() {
    for level in ["off", "low", "normal", "high"] {
        run_with(&["--mem-pressure", level], &[])
            .stderr(predicate::str::contains("mem-pressure").not());
    }
}

#[test]
fn invalid_sensitivity_is_rejected_by_clap() {
    run_with(&["--mem-pressure", "bogus"], &[])
        .failure()
        .stderr(predicate::str::contains("invalid value 'bogus'"));
}

#[test]
fn env_var_sensitivity_is_honored() {
    run_with(&[], &[("LUCHTA_MEM_PRESSURE", "bogus_env")])
        .failure()
        .stderr(predicate::str::contains(
            "Invalid LUCHTA_MEM_PRESSURE value 'bogus_env'",
        ));
}

#[test]
fn flag_overrides_env_var() {
    // A valid flag must win over an invalid env var — if precedence were
    // reversed this would fail with the env-var parse error.
    run_with(
        &["--mem-pressure", "off"],
        &[("LUCHTA_MEM_PRESSURE", "bogus")],
    )
    .stderr(predicate::str::contains("LUCHTA_MEM_PRESSURE").not());
}

#[test]
fn removed_threshold_flags_are_rejected() {
    for flag in ["--mem-usage-threshold", "--mem-free-threshold"] {
        run_with(&[flag, "50%"], &[])
            .failure()
            .stderr(predicate::str::contains("unexpected argument"));
    }
}
