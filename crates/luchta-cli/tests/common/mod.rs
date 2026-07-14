//! Shared fixtures and helpers for `luchta` CLI integration tests.
//!
//! Not every integration-test binary uses every helper here, so dead-code
//! warnings for unused helpers are expected and suppressed module-wide.
#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use assert_cmd::Command;
use assert_fs::prelude::*;

/// Writes a minimal two-package Yarn workspace (`a`, and `b` depending on `a`)
/// into `temp`, suitable for exercising `luchta run`/`check` against.
pub fn setup_workspace(temp: &assert_fs::TempDir) {
    let root_pkg = temp.child("package.json");
    root_pkg
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    let pkg_a_dir = temp.child("packages/a");
    pkg_a_dir.create_dir_all().expect("create package a dir");
    pkg_a_dir
        .child("package.json")
        .write_str(
            r#"{
    "name": "a",
    "version": "1.0.0",
    "scripts": { "build": "echo build-a" }
}"#,
        )
        .expect("write package a package.json");

    let pkg_b_dir = temp.child("packages/b");
    pkg_b_dir.create_dir_all().expect("create package b dir");
    pkg_b_dir
        .child("package.json")
        .write_str(
            r#"{
    "name": "b",
    "version": "1.0.0",
    "dependencies": { "a": "workspace:*" },
    "scripts": { "build": "echo build-b" }
}"#,
        )
        .expect("write package b package.json");

    init_git(temp);
}

/// Write a generic executable file and set executable bit on Unix.
pub fn write_executable(path: &std::path::Path, body: &str) {
    fs::write(path, body).unwrap();
    set_executable(path);
}

/// Helper: set executable permissions on Unix; no-op elsewhere.
pub fn set_executable(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }
}

/// Create a workspace manifest with the provided JSON string.
pub fn write_root_workspace_manifest(temp: &assert_fs::TempDir, manifest_json: &str) {
    temp.child("package.json").write_str(manifest_json).unwrap();
}

/// Create a standard root workspace manifest using `packages/*`.
pub fn write_root_workspace(temp: &assert_fs::TempDir) {
    write_root_workspace_manifest(
        temp,
        r#"{
  "name": "root",
  "private": true,
  "workspaces": ["packages/*"]
}"#,
    );
}

/// Initialize a git repo and commit all files.
pub fn init_git(temp: &assert_fs::TempDir) {
    use std::process::Command;
    let status = Command::new("git")
        .arg("init")
        .current_dir(temp.path())
        .status()
        .expect("git init");
    assert!(status.success(), "git init failed");
    let status = Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(temp.path())
        .status()
        .expect("git config user.email");
    assert!(status.success(), "git config user.email failed");
    let status = Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(temp.path())
        .status()
        .expect("git config user.name");
    assert!(status.success(), "git config user.name failed");
    let status = Command::new("git")
        .args(["add", "."])
        .current_dir(temp.path())
        .status()
        .expect("git add .");
    assert!(status.success(), "git add . failed");
    let status = Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(temp.path())
        .status()
        .expect("git commit");
    assert!(status.success(), "git commit failed");
}

/// Run a `git` subcommand in `repo`, asserting it succeeds.
fn git_in(repo: &std::path::Path, args: &[&str]) {
    use std::process::Command;
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap()
            .success(),
        "git {args:?} failed"
    );
}

/// Commit all current changes in the temp repo.
pub fn git_commit_all(repo: &std::path::Path, message: &str) {
    git_in(repo, &["add", "."]);
    git_in(repo, &["commit", "-m", message]);
}

/// Commit specific paths. Useful for testing git-tracked input semantics.
pub fn git_commit_paths(repo: &std::path::Path, paths: &[&str], message: &str) {
    let mut add_args = vec!["add"];
    add_args.extend_from_slice(paths);
    git_in(repo, &add_args);
    git_in(repo, &["commit", "-m", message]);
}

/// Extra JSON fragments injected into shell-worker `done` messages.
pub struct WorkerDoneFields<'a> {
    pub json_fragment: Option<&'a str>,
}

pub struct ShellWorkerResolve<'a> {
    pub cache_nonce: Option<&'a str>,
}

impl ShellWorkerResolve<'_> {
    fn resolved_json(&self) -> String {
        let mut resolved = serde_json::json!({ "decision": "accept" });
        if let Some(cache_nonce) = self.cache_nonce {
            resolved["cacheNonce"] = serde_json::Value::String(cache_nonce.to_owned());
        }
        resolved.to_string()
    }
}

/// Create a shell worker script that handles resolveTask and run messages.
pub fn shell_worker(temp: &assert_fs::TempDir) -> assert_fs::fixture::ChildPath {
    shell_worker_with_resolve(temp, ShellWorkerResolve { cache_nonce: None })
}

/// Create a shell worker script whose resolveTask response includes `cache_nonce`.
pub fn shell_worker_with_cache_nonce(
    temp: &assert_fs::TempDir,
    cache_nonce: &str,
) -> assert_fs::fixture::ChildPath {
    shell_worker_with_resolve(
        temp,
        ShellWorkerResolve {
            cache_nonce: Some(cache_nonce),
        },
    )
}

fn shell_worker_with_resolve(
    temp: &assert_fs::TempDir,
    resolve: ShellWorkerResolve<'_>,
) -> assert_fs::fixture::ChildPath {
    let resolved_json = resolve.resolved_json();
    shell_worker_with_resolved_json(temp, &resolved_json)
}

/// Create a shell worker script with optional extra done fields.
pub fn shell_worker_with_done_fields(
    temp: &assert_fs::TempDir,
    done_fields: WorkerDoneFields<'_>,
) -> assert_fs::fixture::ChildPath {
    shell_worker_with_resolved_json_and_done_fields(temp, r#"{"decision":"accept"}"#, done_fields)
}

fn shell_worker_with_resolved_json(
    temp: &assert_fs::TempDir,
    resolved_json: &str,
) -> assert_fs::fixture::ChildPath {
    shell_worker_with_resolved_json_and_done_fields(
        temp,
        resolved_json,
        WorkerDoneFields {
            json_fragment: None,
        },
    )
}

fn shell_worker_with_resolved_json_and_done_fields(
    temp: &assert_fs::TempDir,
    resolved_json: &str,
    done_fields: WorkerDoneFields<'_>,
) -> assert_fs::fixture::ChildPath {
    let script = temp.child("shell-worker.sh");
    let done_fields = done_fields.json_fragment.unwrap_or("");
    script
        .write_str(&format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{{"type":"resolved","id":"%s","result":{}}}\n' "$id"
      ;;
    *'"type":"run"'*)
      cmd=$(printf '%s\n' "$line" | sed -n 's/.*"command":"\([^"]*\)".*/\1/p' | sed 's/\\"/"/g; s/\\\\/\\/g')
      cwd=$(printf '%s\n' "$line" | sed -n 's/.*"cwd":"\([^"]*\)".*/\1/p' | sed 's/\\"/"/g; s/\\\\/\\/g')
      (cd "$cwd" && sh -lc "$cmd")
      code=$?
      printf '{{"type":"done","id":"%s","exitCode":%s{}}}\n' "$id" "$code"
      ;;
  esac
done
"#,
            resolved_json, done_fields
        ))
        .unwrap();
    set_executable(script.path());
    script
}

/// Create shell worker script that emits pre-rendered report JSONL templates before done.
pub fn shell_worker_with_reports(
    temp: &assert_fs::TempDir,
    reports: &[(&str, &str, &str)],
) -> assert_fs::fixture::ChildPath {
    const RUN_ID_TOKEN: &str = "@@LUCHTA_RUN_ID@@";

    let script = temp.child("report-worker.sh");
    let templates_path = temp.child("reports.jsonl.tmpl");

    let template_lines = reports
        .iter()
        .map(|(filename, mime_type, content)| {
            serde_json::to_string(&luchta_worker::WorkerResponse::report(
                RUN_ID_TOKEN,
                *filename,
                *mime_type,
                *content,
            ))
            .expect("serialize report template")
        })
        .collect::<Vec<_>>()
        .join("\n");
    templates_path
        .write_str(&format!("{template_lines}\n"))
        .expect("write report templates");

    let script_body = format!(
        r#"#!/bin/sh
reports_tmpl='{reports_tmpl}'
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
      ;;
    *'"type":"run"'*)
      cmd=$(printf '%s\n' "$line" | sed -n 's/.*"command":"\([^"]*\)".*/\1/p' | sed 's/\\"/"/g; s/\\\\/\\/g')
      cwd=$(printf '%s\n' "$line" | sed -n 's/.*"cwd":"\([^"]*\)".*/\1/p' | sed 's/\\"/"/g; s/\\\\/\\/g')
      (cd "$cwd" && sh -lc "$cmd") >/dev/null
      code=$?
      while IFS= read -r report_line; do
        [ -n "$report_line" ] || continue
        printf '%s\n' "$report_line" | sed 's|{run_id_token}|'"$id"'|g'
      done < "$reports_tmpl"
      printf '{{"type":"done","id":"%s","exitCode":%s}}\n' "$id" "$code"
      ;;
  esac
done
"#,
        reports_tmpl = templates_path.path().display(),
        run_id_token = RUN_ID_TOKEN,
    );
    script
        .write_str(&script_body)
        .expect("write report worker script");
    set_executable(script.path());
    script
}

/// Write a task config with a counter command.
pub fn write_counter_task_config(temp: &assert_fs::TempDir, task_json: &str) {
    let worker = shell_worker(temp);
    write_task_config_with_shell_worker(temp, worker.path(), task_json);
}

pub struct WorkerConfig<'a> {
    pub name: &'a str,
    pub command: &'a Path,
}

/// Write a task config script that invokes given worker.
pub fn write_task_config_with_worker(
    temp: &assert_fs::TempDir,
    worker: WorkerConfig<'_>,
    task_json: &str,
) {
    write_executable(
        temp.child("luchta-config.sh").path(),
        &format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"{}\":{{\"command\":\"{}\"}}}},\"tasks\":{{{}}}}}'\n",
            worker.name,
            worker.command.display(),
            task_json
        ),
    );
}

/// Write a task config script that invokes shell worker.
pub fn write_task_config_with_shell_worker(
    temp: &assert_fs::TempDir,
    worker_command: &Path,
    task_json: &str,
) {
    write_task_config_with_named_worker(temp, "shell", worker_command, task_json);
}

/// Write a task config script that invokes a named worker.
pub fn write_task_config_with_named_worker(
    temp: &assert_fs::TempDir,
    worker_name: &str,
    worker_command: &Path,
    task_json: &str,
) {
    write_task_config_with_worker(
        temp,
        WorkerConfig {
            name: worker_name,
            command: worker_command,
        },
        task_json,
    );
}

/// Write a basic package.json with just a script name.
pub fn write_basic_package(temp: &assert_fs::TempDir, script_name: &str) {
    temp.child("packages/app").create_dir_all().unwrap();
    temp.child("packages/app/package.json")
        .write_str(&format!(
            r#"{{
  "name": "app",
  "scripts": {{
    "{}": "echo ignored"
  }}
}}"#,
            script_name
        ))
        .unwrap();
}

/// Invoke `luchta run <task>` with `extra_args` inserted after the task, scoped
/// to `temp` via `--workspace-root`. Shared by the public runners below.
pub fn run_luchta_with_args(
    temp: &assert_fs::TempDir,
    task: &str,
    extra_args: &[&str],
) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("run").arg(task).args(extra_args);
    cmd.arg("--workspace-root").arg(temp.path()).assert()
}

/// Run `luchta run <task>` against a temporary workspace.
pub fn run_luchta(temp: &assert_fs::TempDir, task: &str) -> assert_cmd::assert::Assert {
    run_luchta_with_args(temp, task, &[])
}

/// Run top-level `luchta run -T <task>` against a temporary workspace.
pub fn run_luchta_top_level(temp: &assert_fs::TempDir, task: &str) -> assert_cmd::assert::Assert {
    run_luchta_with_args(temp, task, &["-T"])
}

/// Absolute path to built `luchta-yarn-worker` binary.
pub fn yarn_worker_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| assert_cmd::cargo::cargo_bin("luchta-yarn-worker"))
        .clone()
}

/// Prepend test bin dir to PATH.
pub fn path_with_prepend(bin_dir: &Path) -> String {
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Small fixture builder for cache integration workspaces.
pub struct WorkspaceBuilder<'a> {
    pub yarn_lock: Option<&'a str>,
    pub task_json: Option<&'a str>,
    pub script_name: Option<&'a str>,
    pub extra_files: &'a [(&'a str, &'a str)],
}

impl WorkspaceBuilder<'_> {
    pub fn build(self, temp: &assert_fs::TempDir) {
        write_root_workspace(temp);
        if let Some(yarn_lock) = self.yarn_lock {
            temp.child("yarn.lock").write_str(yarn_lock).unwrap();
        }
        if let Some(task_json) = self.task_json {
            write_counter_task_config(temp, task_json);
        }
        if let Some(script_name) = self.script_name {
            write_basic_package(temp, script_name);
        }
        for (path, contents) in self.extra_files {
            temp.child(path).write_str(contents).unwrap();
        }
        init_git(temp);
    }
}

/// Yarn v1 lockfile fixture: left-pad@1.0.0 with transitive dep repeat-string@3.0.0.
/// Used to test that a transitive-only version bump invalidates the cache.
pub const YARN1_LOCK_LEFT_PAD_TRANSITIVE_3_0_0: &str = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-XI5MPzVNApjAyhQzZWn7Q4oJ4kBzP4Qzy6wKzH8w0v7lidh+NROm4tW7x1YgnPZXoqBYwygJyI072QtdgQXl3g==
  dependencies:
    repeat-string "^3.0.0"

repeat-string@^3.0.0:
  version "3.0.0"
  resolved "https://registry.yarnpkg.com/repeat-string/-/repeat-string-3.0.0.tgz#abc123def456"
  integrity sha512-repeat0
"#;

/// Yarn v1 lockfile fixture: left-pad@1.0.0 with transitive dep repeat-string@3.0.1.
/// Only the transitive dep version differs from `YARN1_LOCK_LEFT_PAD_TRANSITIVE_3_0_0`.
pub const YARN1_LOCK_LEFT_PAD_TRANSITIVE_3_0_1: &str = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-XI5MPzVNApjAyhQzZWn7Q4oJ4kBzP4Qzy6wKzH8w0v7lidh+NROm4tW7x1YgnPZXoqBYwygJyI072QtdgQXl3g==
  dependencies:
    repeat-string "^3.0.0"

repeat-string@^3.0.0:
  version "3.0.1"
  resolved "https://registry.yarnpkg.com/repeat-string/-/repeat-string-3.0.1.tgz#abc123def789"
  integrity sha512-repeat1
"#;

/// Yarn Berry lockfile fixture: left-pad@1.0.0 with transitive dep repeat-string@3.0.0.
/// Used to test that a transitive-only version bump invalidates the cache.
pub const YARN_BERRY_LEFT_PAD_TRANSITIVE_3_0_0: &str = r#"__metadata:
  version: 8
  cacheKey: 10

"left-pad@npm:^1.0.0":
  version: 1.0.0
  resolution: "left-pad@npm:1.0.0"
  checksum: 10/c84e2417581bbb8eaf2b9e3d7a122e572ab1af37
  languageName: node
  linkType: hard
  dependencies:
    repeat-string: "npm:^3.0.0"

"repeat-string@npm:^3.0.0":
  version: 3.0.0
  resolution: "repeat-string@npm:3.0.0"
  checksum: 10/abc123def456
  languageName: node
  linkType: hard

"app@workspace:packages/app":
  version: 0.0.0-use.local
  resolution: "app@workspace:packages/app"
  dependencies:
    left-pad: "npm:^1.0.0"
  languageName: node
  linkType: soft
"#;

/// Yarn Berry lockfile fixture: left-pad@1.0.0 with transitive dep repeat-string@3.0.1.
/// Only the transitive dep version differs from `YARN_BERRY_LEFT_PAD_TRANSITIVE_3_0_0`.
pub const YARN_BERRY_LEFT_PAD_TRANSITIVE_3_0_1: &str = r#"__metadata:
  version: 8
  cacheKey: 10

"left-pad@npm:^1.0.0":
  version: 1.0.0
  resolution: "left-pad@npm:1.0.0"
  checksum: 10/c84e2417581bbb8eaf2b9e3d7a122e572ab1af37
  languageName: node
  linkType: hard
  dependencies:
    repeat-string: "npm:^3.0.0"

"repeat-string@npm:^3.0.0":
  version: 3.0.1
  resolution: "repeat-string@npm:3.0.1"
  checksum: 10/abc123def789
  languageName: node
  linkType: hard

"app@workspace:packages/app":
  version: 0.0.0-use.local
  resolution: "app@workspace:packages/app"
  dependencies:
    left-pad: "npm:^1.0.0"
  languageName: node
  linkType: soft
"#;

/// Yarn v1 lockfile fixture pinning `left-pad@^1.0.0`.
pub const YARN1_LOCK_LEFT_PAD_1_0_0: &str = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-XI5MPzVNApjAyhQzZWn7Q4oJ4kBzP4Qzy6wKzH8w0v7lidh+NROm4tW7x1YgnPZXoqBYwygJyI072QtdgQXl3g==
"#;

/// Yarn v1 lockfile fixture pinning `left-pad@^1.1.0`.
pub const YARN1_LOCK_LEFT_PAD_1_1_0: &str = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.1.0:
  version "1.1.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.1.0.tgz#47a2daf581ede454334dee6c6036cae00d912e4d"
  integrity sha512-gzzVNpfkTJpfr3xNbSl9AxW8EBttkTBeIBxECUfSpwvJOCtVXiRHeGCXAvsXAZpXmYR52paNtKxwyq8XynDoRg==
"#;

/// Yarn Berry lockfile fixture pinning `left-pad@npm:^1.0.0`.
pub const YARN_BERRY_LEFT_PAD_1_0_0: &str = r#"__metadata:
  version: 8
  cacheKey: 10

"left-pad@npm:^1.0.0":
  version: 1.0.0
  resolution: "left-pad@npm:1.0.0"
  checksum: 10/c84e2417581bbb8eaf2b9e3d7a122e572ab1af37
  languageName: node
  linkType: hard

"app@workspace:packages/app":
  version: 0.0.0-use.local
  resolution: "app@workspace:packages/app"
  dependencies:
    left-pad: "npm:^1.0.0"
  languageName: node
  linkType: soft
"#;

/// Yarn Berry lockfile fixture pinning `left-pad@npm:^1.1.0`.
pub const YARN_BERRY_LEFT_PAD_1_1_0: &str = r#"__metadata:
  version: 8
  cacheKey: 10

"left-pad@npm:^1.1.0":
  version: 1.1.0
  resolution: "left-pad@npm:1.1.0"
  checksum: 10/47a2daf581ede454334dee6c6036cae00d912e4d
  languageName: node
  linkType: hard

"app@workspace:packages/app":
  version: 0.0.0-use.local
  resolution: "app@workspace:packages/app"
  dependencies:
    left-pad: "npm:^1.1.0"
  languageName: node
  linkType: soft
"#;

/// Cached `app#pkgbuild` fixture with a `left-pad@^1.0.0` dependency, using the
/// supplied `yarn_lock` contents.
pub fn setup_lockfile_workspace(temp: &assert_fs::TempDir, yarn_lock: &str) {
    let package_json = r#"{
  "name": "app",
  "dependencies": {
    "left-pad": "^1.0.0"
  },
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#;

    setup_pkgbuild_counter_workspace(
        temp,
        yarn_lock,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
        &[("packages/app/src.txt", "one\n")],
    );
    temp.child("packages/app/package.json")
        .write_str(package_json)
        .unwrap();
    git_commit_all(temp.path(), "fixture");
}

/// Standard skip/edit fixture: a cached `app#pkgbuild` task whose `src.txt`
/// input is copied to `out.txt`.
pub fn setup_skip_edit_workspace(temp: &assert_fs::TempDir, yarn_lock: &str) {
    setup_pkgbuild_counter_workspace(
        temp,
        yarn_lock,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt","out.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat src.txt > out.txt"}"#,
        &[("packages/app/src.txt", "one\n")],
    );
}

/// A `(path, expected_contents)` assertion target for a cache run.
pub type FileExpectation<'a> = (&'a str, &'a str);

/// Run `pkgbuild` (asserting a stable `before`), apply `edit`, then run again
/// and assert `after` — the canonical run→skip→rerun cache check. Each argument
/// is a `(path, expected_contents)` pair.
pub fn assert_pkgbuild_runs_then_skips_then_reruns(
    temp: &assert_fs::TempDir,
    before: FileExpectation<'_>,
    edit: impl FnOnce(&assert_fs::TempDir),
    after: FileExpectation<'_>,
) {
    run_luchta(temp, "pkgbuild").success();
    temp.child(before.0).assert(before.1);

    run_luchta(temp, "pkgbuild").success();
    temp.child(before.0).assert(before.1);

    edit(temp);

    run_luchta(temp, "pkgbuild").success();
    temp.child(after.0).assert(after.1);
}

/// Build standard cache fixture with pkgbuild counter task.
pub fn setup_pkgbuild_counter_workspace(
    temp: &assert_fs::TempDir,
    yarn_lock: &str,
    task_json: &str,
    extra_files: &[(&str, &str)],
) {
    WorkspaceBuilder {
        yarn_lock: Some(yarn_lock),
        task_json: Some(task_json),
        script_name: Some("pkgbuild"),
        extra_files,
    }
    .build(temp);
}

/// Rewrite env-sensitive cache fixture config.
pub fn write_env_config(temp: &assert_fs::TempDir, foo_value: &str, bar_input: bool) {
    write_counter_task_config(
        temp,
        &format!(
            r#""app#pkgbuild":{{"cache":{{}},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"env":{{"FOO":{{"value":"{foo_value}"}},"BAR":{{"input":{bar_input}}}}},"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}}"#
        ),
    );
}

/// Write fake `yarn` shim into temp/bin and return bin dir.
pub fn write_fake_yarn(temp: &assert_fs::TempDir, body: &str) -> PathBuf {
    let bin_dir = temp.child("bin");
    bin_dir.create_dir_all().unwrap();
    write_executable(
        temp.child("bin/yarn").path(),
        &format!("#!/bin/sh\nset -eu\n{body}\n"),
    );
    bin_dir.path().to_path_buf()
}

/// Setup a workspace with two deps (left-pad and chalk) and a task with a dependencies filter.
/// The task's `dependencies` field selects which package deps contribute to cache invalidation.
pub fn setup_filtered_deps_workspace(
    temp: &assert_fs::TempDir,
    yarn_lock: &str,
    dependencies_filter: &[&str],
) {
    // Build the dependencies filter JSON array
    let deps_json = dependencies_filter
        .iter()
        .map(|d| format!("\"{}\"", d))
        .collect::<Vec<_>>()
        .join(",");

    let task_json = format!(
        r#""app#pkgbuild":{{"cache":{{}},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"dependencies":[{}],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}}"#,
        deps_json
    );

    let package_json = r#"{
  "name": "app",
  "dependencies": {
    "left-pad": "^1.0.0",
    "chalk": "^5.0.0"
  },
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#;

    setup_pkgbuild_counter_workspace(
        temp,
        yarn_lock,
        &task_json,
        &[("packages/app/src.txt", "one\n")],
    );
    temp.child("packages/app/package.json")
        .write_str(package_json)
        .unwrap();
    git_commit_all(temp.path(), "fixture");
}

/// Yarn v1 lockfile fixture: app depends on left-pad@1.0.0 and chalk@5.0.0.
/// Used to test the per-task `dependencies` filter narrowing cache invalidation.
pub const YARN1_LOCK_LEFT_PAD_AND_CHALK_V1: &str = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-XI5MPzVNApjAyhQzZWn7Q4oJ4kBzP4Qzy6wKzH8w0v7lidh+NROm4tW7x1YgnPZXoqBYwygJyI072QtdgQXl3g==

chalk@^5.0.0:
  version "5.0.0"
  resolved "https://registry.yarnpkg.com/chalk/-/chalk-5.0.0.tgz#ae417bf7adye0"
  integrity sha512-chalk0
"#;

/// Yarn v1 lockfile fixture: same as above but chalk bumped to 5.1.0.
/// Used to test that NON-selected dep bump does NOT invalidate cache.
pub const YARN1_LOCK_LEFT_PAD_AND_CHALK_CHALK_BUMP: &str = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-XI5MPzVNApjAyhQzZWn7Q4oJ4kBzP4Qzy6wKzH8w0v7lidh+NROm4tW7x1YgnPZXoqBYwygJyI072QtdgQXl3g==

chalk@^5.0.0:
  version "5.1.0"
  resolved "https://registry.yarnpkg.com/chalk/-/chalk-5.1.0.tgz#bbf1d8b6e7b5"
  integrity sha512-chalk1
"#;

/// Yarn v1 lockfile fixture: same as V1 but left-pad bumped to 1.1.0.
/// Used to test that SELECTED dep bump DOES invalidate cache.
pub const YARN1_LOCK_LEFT_PAD_AND_CHALK_LEFT_PAD_BUMP: &str = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.1.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.1.0.tgz#47a2daf581ede454334dee6c6036cae00d912e4d"
  integrity sha512-gzzVNpfkTJpfr3xNbSl9AxW8EBttkTBeIBxECUfSpwvJOCtVXiRHeGCXAvsXAZpXmYR52paNtKxwyq8XynDoRg==

chalk@^5.0.0:
  version "5.0.0"
  resolved "https://registry.yarnpkg.com/chalk/-/chalk-5.0.0.tgz#ae417bf7adye0"
  integrity sha512-chalk0
"#;
