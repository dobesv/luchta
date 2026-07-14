use std::fs;

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

mod common;

use common::{
    init_git, shell_worker, shell_worker_with_cache_nonce, write_basic_package,
    write_counter_task_config, write_root_workspace,
};

const COUNTER_COMMAND: &str =
    "count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt";
const COUNTER_A_COMMAND: &str = "count=$(cat counter-a.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter-a.txt";
const COUNTER_B_COMMAND: &str = "count=$(cat counter-b.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter-b.txt";

#[derive(Clone, Copy)]
struct LuchtaRun<'a> {
    task: &'a str,
    env_nonce: Option<&'a str>,
}

impl<'a> LuchtaRun<'a> {
    fn new(task: &'a str) -> Self {
        Self {
            task,
            env_nonce: None,
        }
    }

    fn with_env_nonce(mut self, nonce: &'a str) -> Self {
        self.env_nonce = Some(nonce);
        self
    }
}

fn run_luchta(temp: &assert_fs::TempDir, run: LuchtaRun<'_>) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    if let Some(nonce) = run.env_nonce {
        cmd.env("LUCHTA_CACHE_NONCE", nonce);
    } else {
        cmd.env_remove("LUCHTA_CACHE_NONCE");
    }
    cmd.arg("run")
        .arg(run.task)
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
}

fn write_pkgbuild_counter_workspace(temp: &assert_fs::TempDir, task_json: &str) {
    write_counter_workspace(
        temp,
        CounterWorkspaceConfig {
            task_json,
            config_script: ConfigScript::CounterTask,
            sources: &[FileFixture::new("packages/app/src.txt", "input\n")],
        },
    );
}

fn write_counter_workspace(temp: &assert_fs::TempDir, config: CounterWorkspaceConfig<'_>) {
    write_root_workspace(temp);
    write_basic_package(temp, "pkgbuild");
    for source in config.sources {
        temp.child(source.path).write_str(source.contents).unwrap();
    }
    config.config_script.write(temp, config.task_json);
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
        r#""app#pkgbuild":{{"cache":{{"nonce":"{}"}},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"{}"}}"#,
        nonce, COUNTER_COMMAND
    )
}

fn no_nonce_config() -> String {
    format!(
        r#""app#pkgbuild":{{"cache":{{}},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"{}"}}"#,
        COUNTER_COMMAND
    )
}

struct FileFixture<'a> {
    path: &'a str,
    contents: &'a str,
}

impl<'a> FileFixture<'a> {
    const fn new(path: &'a str, contents: &'a str) -> Self {
        Self { path, contents }
    }
}

enum ConfigScript<'a> {
    CounterTask,
    GlobalNonce(&'a str),
    WorkerNonce { worker_a_nonce: &'a str },
    WorkerRuntimeNonce { worker_runtime_nonce: &'a str },
}

impl ConfigScript<'_> {
    fn write(&self, temp: &assert_fs::TempDir, task_json: &str) {
        let script_body = match self {
            Self::CounterTask => format!(
                "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"shell\":{{\"command\":\"{}\"}}}},\"tasks\":{{{task_json}}}}}'\n",
                shell_worker(temp).path().display(),
            ),
            Self::GlobalNonce(nonce) => format!(
                "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"cache\":{{\"nonce\":\"{}\"}},\"workers\":{{\"shell\":{{\"command\":\"{}\"}}}},\"tasks\":{{{task_json}}}}}'\n",
                nonce,
                shell_worker(temp).path().display(),
            ),
            Self::WorkerNonce { worker_a_nonce } => {
                let worker_a = shell_worker(temp);
                let worker_b = shell_worker(temp);
                format!(
                    "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"workerA\":{{\"command\":\"{}\",\"cache\":{{\"nonce\":\"{}\"}}}},\"workerB\":{{\"command\":\"{}\"}}}},\"tasks\":{{{task_json}}}}}'\n",
                    worker_a.path().display(),
                    worker_a_nonce,
                    worker_b.path().display(),
                )
            }
            Self::WorkerRuntimeNonce {
                worker_runtime_nonce,
            } => format!(
                "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"shell\":{{\"command\":\"{}\"}}}},\"tasks\":{{{task_json}}}}}'\n",
                shell_worker_with_cache_nonce(temp, worker_runtime_nonce)
                    .path()
                    .display(),
            ),
        };
        common::write_executable(temp.child("luchta-config.sh").path(), &script_body);
    }
}

struct CounterWorkspaceConfig<'a> {
    task_json: &'a str,
    config_script: ConfigScript<'a>,
    sources: &'a [FileFixture<'a>],
}

fn worker_scope_task_config() -> String {
    format!(
        concat!(
            r#""app#taska":{{"cache":{{}},"worker":"workerA","inputs":["src-a.txt"],"outputs":["counter-a.txt"],"command":"{}"}},"#,
            r#""app#taskb":{{"cache":{{}},"worker":"workerB","inputs":["src-b.txt"],"outputs":["counter-b.txt"],"command":"{}"}}"#
        ),
        COUNTER_A_COMMAND, COUNTER_B_COMMAND
    )
}

fn assert_counter_run(temp: &assert_fs::TempDir, run: LuchtaRun<'_>, expected: u32) {
    run_luchta(temp, run).success();
    assert_eq!(read_counter(temp, "packages/app/counter.txt"), expected);
}

fn assert_counter_repeat(temp: &assert_fs::TempDir, run: LuchtaRun<'_>, expected: u32) {
    assert_counter_run(temp, run, expected);
    assert_counter_run(temp, run, expected);
}

#[test]
fn task_scope_nonce_busts_cache() {
    let temp = assert_fs::TempDir::new().unwrap();
    let task_json = task_nonce_config("v1");
    write_pkgbuild_counter_workspace(&temp, &task_json);

    assert_counter_repeat(&temp, LuchtaRun::new("pkgbuild"), 1);

    write_counter_file(&temp, "packages/app/counter.txt", 1);
    let task_json = task_nonce_config("v2");
    write_counter_task_config(&temp, &task_json);
    assert_counter_repeat(&temp, LuchtaRun::new("pkgbuild"), 2);
}

#[test]
fn env_nonce_busts_cache() {
    let temp = assert_fs::TempDir::new().unwrap();
    let task_json = no_nonce_config();
    write_pkgbuild_counter_workspace(&temp, &task_json);

    assert_counter_repeat(&temp, LuchtaRun::new("pkgbuild"), 1);
    assert_counter_repeat(&temp, LuchtaRun::new("pkgbuild").with_env_nonce("e1"), 2);
    assert_counter_run(&temp, LuchtaRun::new("pkgbuild").with_env_nonce("e2"), 3);
}

#[test]
fn global_scope_nonce_busts_cache() {
    let temp = assert_fs::TempDir::new().unwrap();
    let task_json = no_nonce_config();
    write_counter_workspace(
        &temp,
        CounterWorkspaceConfig {
            task_json: &task_json,
            config_script: ConfigScript::GlobalNonce("g1"),
            sources: &[FileFixture::new("packages/app/src.txt", "input\n")],
        },
    );

    assert_counter_repeat(&temp, LuchtaRun::new("pkgbuild"), 1);

    write_counter_file(&temp, "packages/app/counter.txt", 1);
    write_counter_workspace(
        &temp,
        CounterWorkspaceConfig {
            task_json: &task_json,
            config_script: ConfigScript::GlobalNonce("g2"),
            sources: &[FileFixture::new("packages/app/src.txt", "input\n")],
        },
    );
    assert_counter_run(&temp, LuchtaRun::new("pkgbuild"), 2);
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
    let task_json = worker_scope_task_config();
    ConfigScript::WorkerNonce {
        worker_a_nonce: "w1",
    }
    .write(&temp, &task_json);
    init_git(&temp);

    run_luchta(&temp, LuchtaRun::new("taska")).success();
    run_luchta(&temp, LuchtaRun::new("taskb")).success();
    assert_eq!(read_counter(&temp, "packages/app/counter-a.txt"), 1);
    assert_eq!(read_counter(&temp, "packages/app/counter-b.txt"), 1);

    run_luchta(&temp, LuchtaRun::new("taska")).success();
    run_luchta(&temp, LuchtaRun::new("taskb")).success();
    assert_eq!(read_counter(&temp, "packages/app/counter-a.txt"), 1);
    assert_eq!(read_counter(&temp, "packages/app/counter-b.txt"), 1);

    write_counter_file(&temp, "packages/app/counter-a.txt", 1);
    write_counter_file(&temp, "packages/app/counter-b.txt", 1);
    ConfigScript::WorkerNonce {
        worker_a_nonce: "w2",
    }
    .write(&temp, &task_json);
    run_luchta(&temp, LuchtaRun::new("taska")).success();
    run_luchta(&temp, LuchtaRun::new("taskb")).success();
    assert_eq!(read_counter(&temp, "packages/app/counter-a.txt"), 2);
    assert_eq!(read_counter(&temp, "packages/app/counter-b.txt"), 1);
}

#[test]
fn worker_version_cache_runtime_nonce_change_busts_cache() {
    let temp = assert_fs::TempDir::new().unwrap();
    let task_json = no_nonce_config();
    write_counter_workspace(
        &temp,
        CounterWorkspaceConfig {
            task_json: &task_json,
            config_script: ConfigScript::WorkerRuntimeNonce {
                worker_runtime_nonce: "1.0.0",
            },
            sources: &[FileFixture::new("packages/app/src.txt", "input\n")],
        },
    );

    run_luchta(&temp, LuchtaRun::new("pkgbuild")).success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 1);

    run_luchta(&temp, LuchtaRun::new("pkgbuild")).success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 1);

    write_counter_file(&temp, "packages/app/counter.txt", 1);
    write_counter_workspace(
        &temp,
        CounterWorkspaceConfig {
            task_json: &task_json,
            config_script: ConfigScript::WorkerRuntimeNonce {
                worker_runtime_nonce: "1.0.1",
            },
            sources: &[FileFixture::new("packages/app/src.txt", "input\n")],
        },
    );
    run_luchta(&temp, LuchtaRun::new("pkgbuild")).success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 2);

    run_luchta(&temp, LuchtaRun::new("pkgbuild")).success();
    assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 2);
}

#[test]
fn logs_show_resolved_task_nonce() {
    let temp = assert_fs::TempDir::new().unwrap();
    let task_json = task_nonce_config("v1");
    write_pkgbuild_counter_workspace(&temp, &task_json);

    run_luchta(&temp, LuchtaRun::new("pkgbuild")).success();

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
