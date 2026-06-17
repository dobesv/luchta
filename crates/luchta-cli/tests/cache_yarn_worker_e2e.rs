//! Cache integration test for the yarn worker: editing a package's
//! `package.json` script (a worker-detected input) invalidates the cache.
//!
//! Split out of `cache_e2e.rs` so the yarn-worker fixture helpers form their
//! own cohesive unit.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_fs::prelude::*;

mod common;

use common::{git_commit_all, init_git, write_executable, write_fake_yarn, WorkspaceBuilder};

fn yarn_worker_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("luchta-yarn-worker")
}

fn path_with_prepend(bin_dir: &Path) -> String {
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn setup_yarn_worker_workspace(temp: &assert_fs::TempDir) -> PathBuf {
    WorkspaceBuilder {
        yarn_lock: Some(""),
        task_json: None,
        script_name: Some("build"),
        extra_files: &[("packages/app/src.txt", "one\n")],
    }
    .build(temp);
    let fake_yarn_bin = write_fake_yarn(
        temp,
        r#"if [ "$1" = "workspace" ]; then
  ws="$2"
  script="$3"
  shift 3
else
  script="$1"
  shift
fi
count=$(cat counter.txt 2>/dev/null || echo 0)
count=$((count+1))
echo $count > counter.txt
cat src.txt > out.txt
"#,
    );
    write_executable(
        temp.child("luchta-config.sh").path(),
        &format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"yarn\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"cache\":{{}},\"worker\":\"yarn\",\"inputs\":[\"src.txt\"],\"outputs\":[\"counter.txt\",\"out.txt\"]}}}}}}'\n",
            yarn_worker_bin().display()
        ),
    );
    init_git(temp);
    fake_yarn_bin
}

#[test]
fn cache_yarn_worker_detected_package_json_input_reruns_on_package_edit() {
    let temp = assert_fs::TempDir::new().unwrap();
    let fake_yarn_bin = setup_yarn_worker_workspace(&temp);

    let path = path_with_prepend(&fake_yarn_bin);
    let mut cmd1 = Command::cargo_bin("luchta").unwrap();
    cmd1.env("PATH", &path)
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success();
    temp.child("packages/app/counter.txt").assert("1\n");

    let mut cmd2 = Command::cargo_bin("luchta").unwrap();
    cmd2.env("PATH", &path)
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success();
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "build": "echo changed"
  }
}"#,
        )
        .unwrap();
    git_commit_all(temp.path(), "edit package script");

    let mut cmd3 = Command::cargo_bin("luchta").unwrap();
    cmd3.env("PATH", &path)
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
        .success();
    temp.child("packages/app/counter.txt").assert("2\n");
}
