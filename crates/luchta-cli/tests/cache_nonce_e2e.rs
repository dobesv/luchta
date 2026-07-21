use std::fs;

use assert_cmd::Command;
use assert_fs::prelude::*;
use luchta_test_support::require_nextest;
use predicates::prelude::*;

mod common;

use common::{
    init_git, shell_worker, write_basic_package, write_counter_task_config, write_root_workspace,
};

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
    cmd.arg("run")
        .arg(task)
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
}

fn write_pkgbuild_counter_workspace(temp: &assert_fs::TempDir, task_json: &str) {
    write_root_workspace(temp);
    write_counter_task_config(temp, task_json);
    write_basic_package(temp, "pkgbuild");
    temp.child("packages/app/src.txt")
        .write_str("input\n")
        .unwrap();
    init_git(temp);
}

fn write_counter_file(temp: &assert_fs::TempDir, relative_path: &str, value: u32) {
    temp.child(relative_path)
        .write_str(&format!("{value}\n"))
        .unwrap();
}

fn read_counter(temp: &assert_fs::TempDir, relative_path: &str) -> u32 {
    fs::read_to_string(temp.path().join(relative_path))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn task_nonce_config(nonce: &str) -> String {
    format!(
        r#""app#pkgbuild":{{"cache":{{"nonce":"{}"}},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}}"#,
        nonce
    )
}

fn no_nonce_config() -> &'static str {
    r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#
}

fn write_global_nonce_config(temp: &assert_fs::TempDir, nonce: &str) {
    let worker = shell_worker(temp);
    common::write_executable(
        temp.child("luchta-config.sh").path(),
        &format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"cache\":{{\"nonce\":\"{}\"}},\"workers\":{{\"shell\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"app#pkgbuild\":{{\"cache\":{{}},\"worker\":\"shell\",\"inputs\":[\"src.txt\"],\"outputs\":[\"counter.txt\"],\"command\":\"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt\"}}}}}}'\n",
            nonce,
            worker.path().display(),
        ),
    );
}

fn write_worker_nonce_config(temp: &assert_fs::TempDir, worker_a_nonce: &str) {
    let worker_a = shell_worker(temp);
    let worker_b = shell_worker(temp);
    common::write_executable(
        temp.child("luchta-config.sh").path(),
        &format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"workerA\":{{\"command\":\"{}\",\"cache\":{{\"nonce\":\"{}\"}}}},\"workerB\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"app#taska\":{{\"cache\":{{}},\"worker\":\"workerA\",\"inputs\":[\"src-a.txt\"],\"outputs\":[\"counter-a.txt\"],\"command\":\"count=$(cat counter-a.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter-a.txt\"}},\"app#taskb\":{{\"cache\":{{}},\"worker\":\"workerB\",\"inputs\":[\"src-b.txt\"],\"outputs\":[\"counter-b.txt\"],\"command\":\"count=$(cat counter-b.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter-b.txt\"}}}}}}'\n",
            worker_a.path().display(),
            worker_a_nonce,
            worker_b.path().display(),
        ),
    );
}

#[test]
fn task_scope_nonce_busts_cache() {
    let temp = assert_fs::TempDir::new().unwrap();
    write_pkgbuild_counter_workspace(&temp, &task_nonce_config("v1"));

    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 1);

    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 1);

    write_counter_file(&temp, "packages/app/counter.txt", 1);
    write_counter_task_config(&temp, &task_nonce_config("v2"));
    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 2);

    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 2);
}

#[test]
fn env_nonce_busts_cache() {
    require_nextest();
    let _lock = ENV_LOCK.lock().unwrap();
    let _guard = EnvVarGuard::remove("LUCHTA_CACHE_NONCE");

    let temp = assert_fs::TempDir::new().unwrap();
    write_pkgbuild_counter_workspace(&temp, no_nonce_config());

    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 1);

    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 1);

    {
        let _guard = EnvVarGuard::set("LUCHTA_CACHE_NONCE", "e1");
        run_luchta(&temp, "pkgbuild").success();
        assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 2);

        run_luchta(&temp, "pkgbuild").success();
        assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 2);
    }

    {
        let _guard = EnvVarGuard::set("LUCHTA_CACHE_NONCE", "e2");
        run_luchta(&temp, "pkgbuild").success();
        assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 3);
    }
}

#[test]
fn global_scope_nonce_busts_cache() {
    let temp = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&temp);
    write_basic_package(&temp, "pkgbuild");
    temp.child("packages/app/src.txt")
        .write_str("input\n")
        .unwrap();
    write_global_nonce_config(&temp, "g1");
    init_git(&temp);

    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 1);

    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 1);

    write_counter_file(&temp, "packages/app/counter.txt", 1);
    write_global_nonce_config(&temp, "g2");
    run_luchta(&temp, "pkgbuild").success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 2);
}

#[test]
fn worker_scope_nonce_targets_only_matching_worker() {
    let temp = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&temp);
    write_basic_package(&temp, "taska");
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "taska": "echo ignored",
    "taskb": "echo ignored"
  }
}"#,
        )
        .unwrap();
    temp.child("packages/app/src-a.txt")
        .write_str("a\n")
        .unwrap();
    temp.child("packages/app/src-b.txt")
        .write_str("b\n")
        .unwrap();
    write_worker_nonce_config(&temp, "w1");
    init_git(&temp);

    run_luchta(&temp, "taska").success();
    run_luchta(&temp, "taskb").success();
    assert_eq!(read_counter(&temp, "packages/app/counter-a.txt"), 1);
    assert_eq!(read_counter(&temp, "packages/app/counter-b.txt"), 1);

    run_luchta(&temp, "taska").success();
    run_luchta(&temp, "taskb").success();
    assert_eq!(read_counter(&temp, "packages/app/counter-a.txt"), 1);
    assert_eq!(read_counter(&temp, "packages/app/counter-b.txt"), 1);

    write_counter_file(&temp, "packages/app/counter-a.txt", 1);
    write_counter_file(&temp, "packages/app/counter-b.txt", 1);
    write_worker_nonce_config(&temp, "w2");
    run_luchta(&temp, "taska").success();
    run_luchta(&temp, "taskb").success();
    assert_eq!(read_counter(&temp, "packages/app/counter-a.txt"), 2);
    assert_eq!(read_counter(&temp, "packages/app/counter-b.txt"), 1);
}

#[test]
fn logs_show_resolved_task_nonce() {
    let temp = assert_fs::TempDir::new().unwrap();
    write_pkgbuild_counter_workspace(&temp, &task_nonce_config("v1"));

    run_luchta(&temp, "pkgbuild").success();

    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("logs")
        .arg("--show-cache-nonce")
        .arg("--workspace-root")
        .arg(temp.path());
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Cache nonce: task=v1"));
}
