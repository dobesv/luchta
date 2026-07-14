//! E2E tests for `--no-cache` CLI flag and `LUCHTA_NO_CACHE` env var.
//!
//! When active:
//! - Tasks ALWAYS run (no local-skip)
//! - Shared cache not read/written
//! - Local workspace cache metadata IS still written (enables subsequent skip)

use std::fs;

use assert_cmd::Command;
use assert_fs::prelude::*;

mod common;

use common::{init_git, write_basic_package, write_counter_task_config, write_root_workspace};

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvVarGuard {
    name: &'static str,
    prior: Option<String>,
}

impl EnvVarGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let prior = std::env::var(name).ok();
        std::env::set_var(name, value);
        Self { name, prior }
    }

    fn remove(name: &'static str) -> Self {
        let prior = std::env::var(name).ok();
        std::env::remove_var(name);
        Self { name, prior }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = self.prior.take() {
            std::env::set_var(self.name, value);
        } else {
            std::env::remove_var(self.name);
        }
    }
}

fn run_luchta(temp: &assert_fs::TempDir, task: &str) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("run")
        .arg(task)
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
}

fn run_luchta_no_cache(temp: &assert_fs::TempDir, task: &str) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("run")
        .arg(task)
        .arg("--no-cache")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
}

fn read_counter(temp: &assert_fs::TempDir, relative_path: &str) -> u32 {
    fs::read_to_string(temp.path().join(relative_path))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn no_nonce_config() -> &'static str {
    r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#
}

fn write_pkgbuild_counter_workspace(temp: &assert_fs::TempDir, config: &str) {
    write_root_workspace(temp);
    write_counter_task_config(temp, config);
    write_basic_package(temp, "pkgbuild");
    temp.child("packages/app/src.txt")
        .write_str("input\n")
        .unwrap();
    init_git(temp);
}

#[test]
fn no_cache_flag_forces_rerun() {
    let temp = assert_fs::TempDir::new().unwrap();
    write_pkgbuild_counter_workspace(&temp, no_nonce_config());

    // First run: task executes, counter=1
    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 1);

    // Second run WITHOUT --no-cache: should skip (counter stays 1)
    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 1);

    // Third run WITH --no-cache: should force rerun (counter=2)
    run_luchta_no_cache(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 2);

    // Fourth run WITH --no-cache again: should force another rerun (counter=3)
    run_luchta_no_cache(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 3);

    // Fifth run WITHOUT --no-cache: should skip since local metadata was written
    // during the --no-cache runs (proves local skip still works)
    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 3);
}

#[test]
fn no_cache_env_var_forces_rerun() {
    let _lock = ENV_LOCK.lock().unwrap();

    let temp = assert_fs::TempDir::new().unwrap();
    write_pkgbuild_counter_workspace(&temp, no_nonce_config());

    // Ensure LUCHTA_NO_CACHE is not set from other tests
    let _guard = EnvVarGuard::remove("LUCHTA_NO_CACHE");

    // First run: task executes, counter=1
    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 1);

    // Second run WITHOUT LUCHTA_NO_CACHE: should skip (counter stays 1)
    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 1);

    // Set LUCHTA_NO_CACHE=1 to force reruns
    let _env_guard = EnvVarGuard::set("LUCHTA_NO_CACHE", "1");

    // Third run WITH LUCHTA_NO_CACHE=1: should force rerun (counter=2)
    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 2);

    // Fourth run WITH LUCHTA_NO_CACHE=1 still: should force another rerun (counter=3)
    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 3);

    // Drop the guard to unset LUCHTA_NO_CACHE (restores prior env state)
    drop(_env_guard);

    // Fifth run WITHOUT LUCHTA_NO_CACHE: should skip since local metadata was written
    // during the --no-cache runs (proves local skip still works)
    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 3);
}
