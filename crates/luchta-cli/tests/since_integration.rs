mod common;

use assert_cmd::Command as AssertCommand;
use assert_fs::prelude::*;
use common::{git_commit_all, git_commit_paths, setup_workspace};
use predicates::prelude::*;
use std::{fs, process::Command};

fn add_third_package(temp: &assert_fs::TempDir) {
    temp.child("packages/c").create_dir_all().unwrap();
    temp.child("packages/c/package.json")
        .write_str(
            r#"{
  "name": "c",
  "version": "1.0.0",
  "scripts": { "build": "echo build-c" }
}"#,
        )
        .unwrap();
}

fn write_default_luchta_config(temp: &assert_fs::TempDir) {
    fs::write(
        temp.child("luchta-config.sh").path(),
        "#!/bin/sh\necho '{\"concurrency\":{\"maxWeight\":4},\"tasks\":{\"build\":{\"dependsOn\":[\"^build\"]},\"a#build\":{},\"b#build\":{\"dependsOn\":[\"^build\"]},\"c#build\":{}}}'\n",
    )
    .unwrap();
    common::set_executable(temp.child("luchta-config.sh").path());
}

fn write_top_level_luchta_config(temp: &assert_fs::TempDir) {
    fs::write(
        temp.child("luchta-config.sh").path(),
        "#!/bin/sh\necho '{\"concurrency\":{\"maxWeight\":4},\"tasks\":{\"build\":{\"dependsOn\":[\"^build\"]},\"#build\":{},\"a#build\":{},\"b#build\":{\"dependsOn\":[\"^build\"]}}}'\n",
    )
    .unwrap();
    common::set_executable(temp.child("luchta-config.sh").path());
}

fn git_stdout(repo: &assert_fs::TempDir, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo.path())
        .output()
        .expect("run git command");
    assert!(
        output.status.success(),
        "git command failed: git {:?}\nstdout:{}\nstderr:{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn temp_repo_path(temp: &assert_fs::TempDir) -> &std::path::Path {
    temp.path()
}

/// Create a temp workspace (packages `a`, `b`-depends-on-`a`) with the default
/// luchta config written. Most `--since` tests start from exactly this state.
fn setup() -> assert_fs::TempDir {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_workspace(&temp);
    write_default_luchta_config(&temp);
    temp
}

/// Like [`setup`] but also commits the config so `--since HEAD` sees a clean
/// tree (used by the no-op / intersection cases that expect no changes).
fn setup_committed() -> assert_fs::TempDir {
    let temp = setup();
    git_commit_all(temp_repo_path(&temp), "add config");
    temp
}

fn assert_run(
    temp: &assert_fs::TempDir,
    task: &str,
    extra_args: &[&str],
) -> assert_cmd::assert::Assert {
    let mut cmd = AssertCommand::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1")
        .arg("run")
        .arg(task)
        .args(extra_args)
        .arg("--workspace-root")
        .arg(temp.path());
    cmd.assert()
}

#[test]
fn since_changed_package_selects_changed_package_and_dependent() {
    let temp = setup();

    temp.child("packages/a/src.ts")
        .write_str("export const a = 1;\n")
        .unwrap();

    assert_run(&temp, "build", &["--since", "HEAD", "--dry-run"])
        .success()
        .stdout(
            predicate::str::contains("dry-run:")
                .and(predicate::str::contains("a#build"))
                .and(predicate::str::contains("b#build")),
        );

    temp.close().unwrap();
}

#[test]
fn since_change_in_dependency_selects_transitive_dependents() {
    let temp = setup();

    let base = git_stdout(&temp, &["rev-parse", "HEAD"]);
    temp.child("packages/a/dep.ts")
        .write_str("export const dep = true;\n")
        .unwrap();
    git_commit_paths(temp_repo_path(&temp), &["packages/a/dep.ts"], "change a");

    assert_run(&temp, "build", &["--since", &base, "--dry-run"])
        .success()
        .stdout(predicate::str::contains("a#build").and(predicate::str::contains("b#build")));

    temp.close().unwrap();
}

#[test]
fn since_change_in_b_does_not_select_unrelated_package() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_workspace(&temp);
    add_third_package(&temp);
    write_default_luchta_config(&temp);
    git_commit_all(temp_repo_path(&temp), "add c and config");

    let base = git_stdout(&temp, &["rev-parse", "HEAD"]);
    temp.child("packages/b/feature.ts")
        .write_str("export const b = 1;\n")
        .unwrap();
    git_commit_paths(
        temp_repo_path(&temp),
        &["packages/b/feature.ts"],
        "change b",
    );

    // `b` is the changed goal; `a` is its prerequisite (b#build dependsOn
    // ^build) and must still expand via the normal dependency expansion
    // (goal-not-filter). `c` is unrelated and must NOT appear.
    assert_run(&temp, "build", &["--since", &base, "--dry-run"])
        .success()
        .stdout(
            predicate::str::contains("b#build")
                .and(predicate::str::contains("a#build"))
                .and(predicate::str::contains("c#build").not()),
        );

    temp.close().unwrap();
}

#[test]
fn since_worktree_change_in_package_marks_it_changed() {
    // Staged, unstaged, and untracked changes inside package `a` must all mark
    // it changed, so `--since HEAD` selects `a#build`.
    let cases = [
        ("packages/a/staged.ts", true),
        ("packages/a/unstaged.ts", false),
        ("packages/a/new-file.ts", false),
    ];

    for (path, stage) in cases {
        let temp = setup();
        temp.child(path)
            .write_str("export const changed = true;\n")
            .unwrap();
        if stage {
            git_stdout(&temp, &["add", path]);
        }

        assert_run(&temp, "build", &["--since", "HEAD", "--dry-run"])
            .success()
            .stdout(predicate::str::contains("a#build"));

        temp.close().unwrap();
    }
}

#[test]
fn since_gitignored_file_does_not_mark_package_changed() {
    let temp = setup_committed();

    temp.child(".gitignore")
        .write_str("packages/a/ignored.log\n")
        .unwrap();
    git_commit_paths(temp_repo_path(&temp), &[".gitignore"], "add ignore");
    temp.child("packages/a/ignored.log")
        .write_str("ignored\n")
        .unwrap();

    assert_run(&temp, "build", &["--since", "HEAD", "--dry-run"])
        .success()
        .stdout(predicate::str::contains(
            "No packages changed since HEAD; nothing to run.",
        ));

    temp.close().unwrap();
}

#[test]
fn since_with_no_affected_packages_is_noop() {
    // With a clean tree, `--since HEAD` finds nothing to run — both on its own
    // and when intersected with a `-p` package filter (an unchanged package).
    let arg_sets: [&[&str]; 2] = [
        &["--since", "HEAD", "--dry-run"],
        &["-p", "a", "--since", "HEAD", "--dry-run"],
    ];

    for args in arg_sets {
        let temp = setup_committed();

        assert_run(&temp, "build", args)
            .success()
            .stdout(predicate::str::contains(
                "No packages changed since HEAD; nothing to run.",
            ));

        temp.close().unwrap();
    }
}

#[test]
fn since_invalid_ref_reports_ref_name() {
    let temp = setup();

    assert_run(&temp, "build", &["--since", "no-such-ref", "--dry-run"])
        .failure()
        .stderr(predicate::str::contains("no-such-ref"));

    temp.close().unwrap();
}

#[test]
fn since_non_git_workspace_reports_actionable_error() {
    let temp = assert_fs::TempDir::new().unwrap();
    temp.child("package.json")
        .write_str(
            r#"{
  "name": "root",
  "private": true,
  "workspaces": ["packages/*"]
}"#,
        )
        .unwrap();
    temp.child("packages/a").create_dir_all().unwrap();
    temp.child("packages/a/package.json")
        .write_str(
            r#"{
  "name": "a",
  "version": "1.0.0",
  "scripts": { "build": "echo build-a" }
}"#,
        )
        .unwrap();
    write_default_luchta_config(&temp);

    assert_run(&temp, "build", &["--since", "HEAD", "--dry-run"])
        .failure()
        .stderr(predicate::str::contains("Not a git repository"));

    temp.close().unwrap();
}

#[test]
fn run_without_since_preserves_existing_selection() {
    let temp = setup();

    assert_run(&temp, "build", &["--dry-run"])
        .success()
        .stdout(predicate::str::contains("a#build").and(predicate::str::contains("b#build")));

    temp.close().unwrap();
}

#[test]
fn top_level_tasks_bypass_since_when_affected_set_non_empty() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_workspace(&temp);
    write_top_level_luchta_config(&temp);

    temp.child("packages/a/root-bypass.ts")
        .write_str("export const changed = true;\n")
        .unwrap();

    assert_run(&temp, "build", &["-T", "--since", "HEAD", "--dry-run"])
        .success()
        .stdout(predicate::str::contains("#build").and(predicate::str::contains("a#build").not()));

    temp.close().unwrap();
}

#[test]
fn top_level_tasks_run_with_since_even_when_no_package_changed() {
    // A root-only change yields an empty affected package set. With `-T` the
    // top-level task bypasses the since filter and must STILL run — the run must
    // NOT short-circuit to the "nothing to run" no-op.
    let temp = assert_fs::TempDir::new().unwrap();
    setup_workspace(&temp);
    write_top_level_luchta_config(&temp);

    temp.child("README.md")
        .write_str("root only change\n")
        .unwrap();

    assert_run(&temp, "build", &["-T", "--since", "HEAD", "--dry-run"])
        .success()
        .stdout(
            predicate::str::contains("#build")
                .and(predicate::str::contains("a#build").not())
                .and(predicate::str::contains("nothing to run").not()),
        );

    temp.close().unwrap();
}

#[test]
fn since_root_only_change_is_noop() {
    let temp = setup();

    temp.child("README.md")
        .write_str("root only change\n")
        .unwrap();

    assert_run(&temp, "build", &["--since", "HEAD", "--dry-run"])
        .success()
        .stdout(predicate::str::contains(
            "No packages changed since HEAD; nothing to run.",
        ));

    temp.close().unwrap();
}
