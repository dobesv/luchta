//! Integration tests for the `--max-weight` CLI flag and
//! `LUCHTA_MAX_WEIGHT` environment variable.

mod common;

use assert_cmd::Command;
use assert_fs::fixture::PathChild;
use predicates::prelude::*;

use common::{run_luchta, setup_workspace};

struct MaxWeightCase {
    args: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
    expected_success: bool,
}

impl MaxWeightCase {
    fn command(&self, temp: &assert_fs::TempDir) -> Command {
        let mut cmd = Command::cargo_bin("luchta").expect("find binary");
        cmd.arg("run")
            .arg("build")
            .arg("--workspace-root")
            .arg(temp.path())
            .env("NO_COLOR", "1");
        for arg in self.args {
            cmd.arg(arg);
        }
        for (key, value) in self.env {
            cmd.env(key, value);
        }
        cmd
    }
}

fn assert_max_weight_case(case: MaxWeightCase, expected: impl predicates::Predicate<str>) {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);
    let expected_success = case.expected_success;
    let assert = case.command(&temp).assert();
    let assert = if expected_success {
        assert.success()
    } else {
        assert.failure()
    };
    assert.stderr(expected);
}

fn setup_weighted_task_workspace(temp: &assert_fs::TempDir) {
    setup_workspace(temp);
    let worker = common::shell_worker(temp);
    let worker_command = worker.path().display().to_string();
    common::write_executable(
        temp.child("luchta-config.sh").path(),
        &format!(
            r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":1}},"workers":{{"shell":{{"command":"{worker_command}"}}}},"tasks":{{"build":{{"dependsOn":["^build"]}},"a#build":{{"worker":"shell","command":"echo a","weight":2}},"b#build":{{"dependsOn":["^build"],"worker":"shell","command":"echo b","weight":2}}}}}}'
"#
        ),
    );
}

#[test]
fn invalid_max_weight_flag_exits_with_error() {
    assert_max_weight_case(
        MaxWeightCase {
            args: &["--max-weight", "bogus"],
            env: &[],
            expected_success: false,
        },
        predicate::str::contains("Invalid --max-weight value"),
    );
}

#[test]
fn invalid_max_weight_env_var_exits_with_error() {
    assert_max_weight_case(
        MaxWeightCase {
            args: &[],
            env: &[("LUCHTA_MAX_WEIGHT", "bogus")],
            expected_success: false,
        },
        predicate::str::contains("Invalid --max-weight value"),
    );
}

#[test]
fn cli_flag_overrides_invalid_max_weight_env_var() {
    assert_max_weight_case(
        MaxWeightCase {
            args: &["--max-weight", "2"],
            env: &[("LUCHTA_MAX_WEIGHT", "bogus")],
            expected_success: false,
        },
        predicate::str::contains("Invalid --max-weight value").not(),
    );
}

#[test]
fn zero_max_weight_exits_with_error() {
    assert_max_weight_case(
        MaxWeightCase {
            args: &["--max-weight", "0"],
            env: &[],
            expected_success: false,
        },
        predicate::str::contains("must be greater than 0"),
    );
}

#[test]
fn env_var_overrides_config_max_weight_for_weighted_task() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_weighted_task_workspace(&temp);

    run_luchta(&temp, "build")
        .failure()
        .stderr(predicate::str::contains("exceeds executor max weight 1"));

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .env("LUCHTA_MAX_WEIGHT", "2");
    cmd.assert()
        .stderr(predicate::str::contains("exceeds executor max weight 1").not());
}

#[test]
fn cli_overrides_env_and_config_max_weight_for_weighted_task() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_weighted_task_workspace(&temp);

    run_luchta(&temp, "build")
        .failure()
        .stderr(predicate::str::contains("exceeds executor max weight 1"));

    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("run")
        .arg("build")
        .arg("--max-weight")
        .arg("2")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .env("LUCHTA_MAX_WEIGHT", "1");
    cmd.assert()
        .stderr(predicate::str::contains("exceeds executor max weight 1").not());
}
