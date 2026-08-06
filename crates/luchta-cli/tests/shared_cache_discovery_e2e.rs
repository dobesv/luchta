mod common;

use assert_cmd::Command;
use assert_fs::prelude::*;
use common::{init_git, write_counter_task_config, write_root_workspace};

fn git(temp: &assert_fs::TempDir, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(temp.path())
        .status()
        .expect("git command");
    assert!(status.success(), "git {args:?} failed");
}

fn run(temp: &assert_fs::TempDir, cache_dir: &std::path::Path) -> String {
    let out = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env("LUCHTA_SHARED_CACHE_DIR", cache_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

/// Regression for #277: cache entries written on one branch must be findable
/// from an unrelated branch. Discovery used to walk first-parent ancestry from
/// HEAD, so a build on an ephemeral merge commit — the Prow case — could never
/// see, or be seen by, any other build.
#[test]
fn cache_written_on_one_branch_is_found_from_an_unrelated_branch() {
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&temp);

    write_counter_task_config(
        &temp,
        r#""app#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"sleep 0.15 && count=$(cat ../../run-count.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > ../../run-count.txt; echo built > out.txt"}}"#,
    );

    temp.child("packages/app/src.txt")
        .write_str("source\n")
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(r#"{"name":"app","scripts":{"build":"echo ignored"}}"#)
        .unwrap();
    init_git(&temp);

    // Build on a feature branch, one commit ahead of master. The extra commit
    // touches nothing the task reads, so it can't affect the cache decision —
    // its only purpose is to make feature-one's HEAD a genuinely different,
    // non-ancestor commit from master's. Without this, `checkout -b` alone
    // leaves feature-one and feature-two pointing at the identical commit,
    // and the old ancestry-walk discovery would trivially "find" it again —
    // proving nothing about cross-branch discovery.
    git(&temp, &["checkout", "-b", "feature-one"]);
    git(
        &temp,
        &["commit", "--allow-empty", "-m", "advance feature-one"],
    );
    run(&temp, shared_cache_dir.path());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("run-count.txt")).unwrap(),
        "1\n"
    );

    // Move to a sibling branch whose history does NOT contain feature-one's commit,
    // and drop the local cache so only the shared cache can serve the task.
    git(&temp, &["checkout", "-b", "feature-two", "master"]);
    std::fs::remove_dir_all(temp.path().join(".luchta/cache")).unwrap();
    std::fs::remove_file(temp.path().join("packages/app/out.txt")).ok();

    let second = run(&temp, shared_cache_dir.path());

    assert!(
        second.contains("📥 1"),
        "expected a shared hit, got:\n{second}"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("run-count.txt")).unwrap(),
        "1\n",
        "the task body should not have run a second time"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("packages/app/out.txt")).unwrap(),
        "built\n"
    );
}
