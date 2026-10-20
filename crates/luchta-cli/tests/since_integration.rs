mod common;

use assert_cmd::Command as AssertCommand;
use assert_fs::prelude::*;
use common::{
    git_commit_all, git_commit_paths, setup_workspace, write_task_config_with_named_worker,
};
use predicates::prelude::*;
use std::{process::Command, sync::OnceLock};

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

fn write_selection_inspector_worker(temp: &assert_fs::TempDir) -> std::path::PathBuf {
    let worker = common::shell_worker(temp);
    worker.path().to_path_buf()
}

fn write_default_luchta_config(temp: &assert_fs::TempDir) {
    let worker = write_selection_inspector_worker(temp);
    write_task_config_with_named_worker(
        temp,
        "sh",
        &worker,
        "\"build\":{\"dependsOn\":[\"^build\"],\"worker\":\"sh\"},\"a#build\":{\"worker\":\"sh\"},\"b#build\":{\"dependsOn\":[\"^build\"],\"worker\":\"sh\"},\"c#build\":{\"worker\":\"sh\"}",
    );
}

fn write_top_level_luchta_config(temp: &assert_fs::TempDir) {
    let worker = write_selection_inspector_worker(temp);
    write_task_config_with_named_worker(
        temp,
        "sh",
        &worker,
        "\"build\":{\"dependsOn\":[\"^build\"],\"worker\":\"sh\"},\"#build\":{\"worker\":\"sh\"},\"a#build\":{\"worker\":\"sh\"},\"b#build\":{\"dependsOn\":[\"^build\"],\"worker\":\"sh\"}",
    );
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
    assert_run_tasks(temp, &[task], extra_args)
}

fn assert_run_tasks(
    temp: &assert_fs::TempDir,
    tasks: &[&str],
    extra_args: &[&str],
) -> assert_cmd::assert::Assert {
    let mut cmd = AssertCommand::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1")
        .arg("run")
        .args(tasks)
        .args(extra_args)
        .arg("--workspace-root")
        .arg(temp.path());
    cmd.assert()
}

fn bash_worker_bin() -> std::path::PathBuf {
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        escargot::CargoBuild::new()
            .bin("luchta-bash-worker")
            .package("luchta-bash-worker")
            .run()
            .expect("build luchta-bash-worker")
            .path()
            .to_path_buf()
    })
    .clone()
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
fn since_allows_multiple_literal_tasks_pruned_to_nothing() {
    let temp = setup();
    let worker = bash_worker_bin();
    write_task_config_with_named_worker(
        &temp,
        "bash",
        &worker,
        "\"test\":{\"worker\":\"bash\",\"command\":\"   \"},\"run-storybook-tests\":{\"worker\":\"bash\",\"command\":\"   \"}",
    );
    temp.child("packages/a/changed.ts")
        .write_str("export const changed = true;\n")
        .unwrap();

    assert_run_tasks(
        &temp,
        &["test", "run-storybook-tests"],
        &["--since", "HEAD"],
    )
    .success()
    .stdout(
        predicate::str::contains(
            "task 'test' was pruned from every package during resolution; nothing to run",
        )
        .and(predicate::str::contains(
            "task 'run-storybook-tests' was pruned from every package during resolution; nothing to run",
        ))
        .and(predicate::str::contains("No tasks matched filter").not()),
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

#[derive(Clone, Copy)]
enum ExpectedTasks {
    None,
    AAndB,
    OnlyC,
    AllNonRoot,
}

impl ExpectedTasks {
    fn task_ids(self) -> &'static [&'static str] {
        match self {
            Self::None => &[],
            Self::AAndB => &["a#build", "b#build"],
            Self::OnlyC => &["c#build"],
            Self::AllNonRoot => &["a#build", "b#build", "c#build"],
        }
    }
}

struct LockfileSinceFixture {
    temp: assert_fs::TempDir,
    base: String,
}

impl LockfileSinceFixture {
    fn new(lockfile: &str) -> Self {
        let temp = setup();
        add_third_package(&temp);
        for (path, value) in [
            (
                "packages/a/package.json",
                serde_json::json!({"left-pad": "^1.0.0"}),
            ),
            ("package.json", serde_json::json!({"dev-tool": "^2.0.0"})),
        ] {
            let file = temp.child(path);
            let mut manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
            manifest["dependencies"] = value;
            file.write_str(&manifest.to_string()).unwrap();
        }
        let file = temp.child("packages/c/package.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
        manifest["devDependencies"] = serde_json::json!({"dev-tool": "^2.0.0"});
        file.write_str(&manifest.to_string()).unwrap();
        temp.child("yarn.lock").write_str(lockfile).unwrap();
        git_commit_all(temp.path(), "dependency closure baseline");
        let base = git_stdout(&temp, &["rev-parse", "HEAD"]);
        Self { temp, base }
    }

    fn change_lockfile(&self, content: &str, commit: bool) {
        self.temp.child("yarn.lock").write_str(content).unwrap();
        if commit {
            git_commit_paths(
                self.temp.path(),
                &["yarn.lock"],
                "change dependency closure",
            );
            assert_eq!(
                git_stdout(&self.temp, &["diff", "--name-only", &self.base, "HEAD"]),
                "yarn.lock"
            );
        } else {
            assert_eq!(git_stdout(&self.temp, &["rev-parse", "HEAD"]), self.base);
            assert_eq!(
                git_stdout(&self.temp, &["diff", "--name-only"]),
                "yarn.lock"
            );
        }
    }

    fn assert_dry_run_tasks(&self, expected: ExpectedTasks) {
        let result =
            assert_run(&self.temp, "build", &["--since", &self.base, "--dry-run"]).success();
        let stdout = std::str::from_utf8(&result.get_output().stdout).unwrap();
        let mut actual: Vec<_> = stdout
            .lines()
            .filter_map(|line| line.strip_prefix("  "))
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        actual.sort_unstable();
        let mut expected = expected.task_ids().to_vec();
        expected.sort_unstable();
        assert_eq!(actual, expected, "unexpected dry-run task set: {stdout}");
    }

    fn assert_list_tasks(&self, expected: ExpectedTasks) {
        let result = AssertCommand::cargo_bin("luchta")
            .unwrap()
            .env("NO_COLOR", "1")
            .args([
                "list",
                "build",
                "--since",
                &self.base,
                "--json",
                "--workspace-root",
            ])
            .arg(self.temp.path())
            .assert()
            .success();
        let tasks: Vec<serde_json::Value> =
            serde_json::from_slice(&result.get_output().stdout).unwrap();
        let actual: Vec<_> = tasks
            .iter()
            .map(|task| task["task_id"].as_str().unwrap())
            .collect();
        assert_eq!(actual, expected.task_ids());
    }

    fn assert_selection(&self, expected: ExpectedTasks) {
        self.assert_dry_run_tasks(expected);
        self.assert_list_tasks(expected);
    }
}

#[test]
fn since_lockfile_committed_transitive_bumps_select_dependents_in_run_and_list() {
    let berry_head = common::YARN_BERRY_DEP_CLOSURES.replace("3.0.1", "3.0.2");
    for (base, head) in [
        (common::YARN_BERRY_DEP_CLOSURES, berry_head.as_str()),
        (
            common::YARN1_LOCK_LEFT_PAD_TRANSITIVE_3_0_0,
            common::YARN1_LOCK_LEFT_PAD_TRANSITIVE_3_0_1,
        ),
    ] {
        let fixture = LockfileSinceFixture::new(base);
        fixture.assert_dry_run_tasks(ExpectedTasks::None);
        fixture.change_lockfile(head, true);
        fixture.assert_selection(ExpectedTasks::AAndB);
    }
}

#[test]
fn since_lockfile_dev_dependency_resolution_change_selects_only_its_package() {
    let fixture = LockfileSinceFixture::new(common::YARN_BERRY_DEP_CLOSURES);
    fixture.change_lockfile(
        &common::YARN_BERRY_DEP_CLOSURES.replace("2.0.1", "2.0.2"),
        true,
    );
    fixture.assert_selection(ExpectedTasks::OnlyC);
}

#[test]
fn since_lockfile_patch_identity_only_changes_select_dependents() {
    for head in [
        common::YARN_BERRY_DEP_CLOSURES.replace("checksum: 10/leftpad", "checksum: 10/changed"),
        common::YARN_BERRY_DEP_CLOSURES.replace("hash=abc123", "hash=def456"),
    ] {
        let fixture = LockfileSinceFixture::new(common::YARN_BERRY_DEP_CLOSURES);
        fixture.change_lockfile(&head, true);
        fixture.assert_selection(ExpectedTasks::AAndB);
    }
}

#[test]
fn since_lockfile_garbage_base_selects_all_non_root_packages() {
    for head in [common::YARN_BERRY_DEP_CLOSURES, "not-a-lockfile"] {
        let fixture = LockfileSinceFixture::new("not-a-lockfile");
        if head != "not-a-lockfile" {
            fixture.change_lockfile(head, true);
        }
        fixture.assert_selection(ExpectedTasks::AllNonRoot);
    }
}

#[test]
fn since_lockfile_unchanged_with_readme_change_selects_nothing() {
    let fixture = LockfileSinceFixture::new(common::YARN_BERRY_DEP_CLOSURES);
    fixture
        .temp
        .child("README.md")
        .write_str("root-only change\n")
        .unwrap();
    git_commit_paths(fixture.temp.path(), &["README.md"], "root-only change");
    assert_run(
        &fixture.temp,
        "build",
        &["--since", &fixture.base, "--dry-run"],
    )
    .success()
    .stdout(predicate::str::contains(format!(
        "No packages changed since {}; nothing to run.",
        fixture.base
    )));
    fixture.assert_selection(ExpectedTasks::None);
}

#[test]
fn since_lockfile_uncommitted_transitive_change_selects_dependents() {
    let fixture = LockfileSinceFixture::new(common::YARN_BERRY_DEP_CLOSURES);
    fixture.change_lockfile(
        &common::YARN_BERRY_DEP_CLOSURES.replace("3.0.1", "3.0.2"),
        false,
    );
    fixture.assert_selection(ExpectedTasks::AAndB);
}
