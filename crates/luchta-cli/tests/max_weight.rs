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
echo '{{"concurrency":{{"maxWeight":1}},"workers":{{"shell":{{"command":"{worker_command}"}}}},"tasks":{{"build":{{"dependsOn":["^build"]}},"a#build":{{"worker":"shell","command":"true","weight":2}},"b#build":{{"dependsOn":["^build"],"worker":"shell","command":"true","weight":2}}}}}}'
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
fn weighted_task_exceeding_config_max_weight_is_clamped_and_runs() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_weighted_task_workspace(&temp);

    // Config sets maxWeight=1 but tasks declare weight=2. The oversized weight
    // is clamped to the executor max instead of erroring, so the run succeeds.
    run_luchta(&temp, "build")
        .success()
        .stderr(predicate::str::contains("exceeds executor max weight").not());
}

#[test]
fn env_var_overrides_config_max_weight_for_weighted_task() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_weighted_task_workspace(&temp);

    // Raising the max weight above the task weight via env var also succeeds.
    let mut cmd = Command::cargo_bin("luchta").expect("find binary");
    cmd.arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("NO_COLOR", "1")
        .env("LUCHTA_MAX_WEIGHT", "2");
    cmd.assert()
        .success()
        .stderr(predicate::str::contains("exceeds executor max weight").not());
}

#[test]
fn cli_overrides_env_and_config_max_weight_for_weighted_task() {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_weighted_task_workspace(&temp);

    // CLI flag (2) overrides both the env var (1) and config (1); the run
    // succeeds with the task weight no longer exceeding the max.
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
        .success()
        .stderr(predicate::str::contains("exceeds executor max weight").not());
}
