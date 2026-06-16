//! Integration tests for the `--mem-usage-threshold` / `--mem-free-threshold`
//! CLI flags and their `LUCHTA_MEM_*_THRESHOLD` environment variables.

mod common;

use assert_cmd::Command;
use predicates::prelude::*;

use common::setup_workspace;

struct MemoryThresholdCase {
    args: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
}

impl MemoryThresholdCase {
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

fn assert_threshold_case(case: MemoryThresholdCase, expected: impl predicates::Predicate<str>) {
    let temp = assert_fs::TempDir::new().expect("create temp dir");
    setup_workspace(&temp);
    case.command(&temp).assert().stderr(expected);
}

#[test]
fn invalid_mem_usage_threshold_exits_with_error() {
    assert_threshold_case(
        MemoryThresholdCase {
            args: &["--mem-usage-threshold", "bogus"],
            env: &[],
        },
        predicate::str::contains("Invalid --mem-usage-threshold value"),
    );
}

#[test]
fn invalid_mem_free_threshold_exits_with_error() {
    assert_threshold_case(
        MemoryThresholdCase {
            args: &["--mem-free-threshold", "12XB"],
            env: &[],
        },
        predicate::str::contains("Invalid --mem-free-threshold value"),
    );
}

#[test]
fn valid_percent_threshold_parses_without_error() {
    assert_threshold_case(
        MemoryThresholdCase {
            args: &["--mem-usage-threshold", "50%"],
            env: &[],
        },
        predicate::str::contains("threshold").not(),
    );
}

#[test]
fn valid_absolute_threshold_with_units_parses() {
    assert_threshold_case(
        MemoryThresholdCase {
            args: &["--mem-free-threshold", "4GiB"],
            env: &[],
        },
        predicate::str::contains("threshold").not(),
    );
}

#[test]
fn env_var_mem_usage_threshold_honored() {
    assert_threshold_case(
        MemoryThresholdCase {
            args: &[],
            env: &[("LUCHTA_MEM_USAGE_THRESHOLD", "bogus_env")],
        },
        predicate::str::contains("Invalid --mem-usage-threshold value 'bogus_env'"),
    );
}

#[test]
fn env_var_mem_free_threshold_honored() {
    assert_threshold_case(
        MemoryThresholdCase {
            args: &[],
            env: &[("LUCHTA_MEM_FREE_THRESHOLD", "invalid_env")],
        },
        predicate::str::contains("Invalid --mem-free-threshold value 'invalid_env'"),
    );
}

#[test]
fn cli_flag_overrides_env_var_threshold() {
    assert_threshold_case(
        MemoryThresholdCase {
            args: &["--mem-usage-threshold", "75%"],
            env: &[("LUCHTA_MEM_USAGE_THRESHOLD", "bogus")],
        },
        predicate::str::contains("threshold").not(),
    );
}
