use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

/// Process-wide lock to serialize env-mutating tests.
/// Prevents races when multiple tests use set_var/remove_var concurrently.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Guard that restores an environment variable to its prior value on drop.
/// Captures the current value on construction (if any) and restores it
/// (or removes if it was absent) when dropped, even on panic.
struct EnvVarGuard {
    name: &'static str,
    prior: Option<String>,
}

impl EnvVarGuard {
    /// Set an env var and return a guard that will restore the prior value.
    pub fn set(name: &'static str, value: &str) -> Self {
        let prior = std::env::var(name).ok();
        std::env::set_var(name, value);
        Self { name, prior }
    }

    /// Remove an env var and return a guard that will restore the prior value.
    #[allow(dead_code)]
    pub fn remove(name: &'static str) -> Self {
        let prior = std::env::var(name).ok();
        std::env::remove_var(name);
        Self { name, prior }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(ref value) = self.prior {
            std::env::set_var(self.name, value);
        } else {
            std::env::remove_var(self.name);
        }
    }
}

fn shell_worker(temp: &assert_fs::TempDir) -> assert_fs::fixture::ChildPath {
    shell_worker_with_done_fields(temp, None)
}

fn shell_worker_with_done_fields(
    temp: &assert_fs::TempDir,
    done_fields: Option<&str>,
) -> assert_fs::fixture::ChildPath {
    let script = temp.child("shell-worker.sh");
    let done_fields = done_fields.unwrap_or("");
    script
        .write_str(&format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
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
            done_fields
        ))
        .unwrap();
    set_executable(script.path());
    script
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    set_executable(path);
}

fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn git_commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", message]);
}

fn git_commit_paths(repo: &Path, paths: &[&str], message: &str) {
    let mut args = vec!["add"];
    args.extend_from_slice(paths);
    git(repo, &args);
    git(repo, &["commit", "-m", message]);
}

fn write_root_workspace(temp: &assert_fs::TempDir) {
    write_root_workspace_manifest(
        temp,
        r#"{
  "name": "root",
  "private": true,
  "workspaces": ["packages/*"]
}"#,
    );
}

fn write_root_workspace_manifest(temp: &assert_fs::TempDir, manifest: &str) {
    temp.child("package.json").write_str(manifest).unwrap();
}

fn init_git(temp: &assert_fs::TempDir) {
    git(temp.path(), &["init"]);
    git(temp.path(), &["config", "user.name", "Luchta Tests"]);
    git(temp.path(), &["config", "user.email", "luchta@example.com"]);
    git_commit_all(temp.path(), "fixture");
}

fn run_luchta(temp: &assert_fs::TempDir, task: &str) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.arg("run")
        .arg(task)
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
}

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

fn write_fake_yarn(temp: &assert_fs::TempDir, body: &str) -> std::path::PathBuf {
    let bin_dir = temp.child("bin");
    bin_dir.create_dir_all().unwrap();
    write_executable(
        temp.child("bin/yarn").path(),
        &format!("#!/bin/sh\nset -eu\n{body}\n"),
    );
    bin_dir.path().to_path_buf()
}

fn write_basic_package(temp: &assert_fs::TempDir, script_name: &str) {
    temp.child("packages/app").create_dir_all().unwrap();
    temp.child("packages/app/package.json")
        .write_str(&format!(
            r#"{{
  "name": "app",
  "scripts": {{
    "{script_name}": "echo ignored"
  }}
}}"#
        ))
        .unwrap();
}

fn write_counter_task_config(temp: &assert_fs::TempDir, task_json: &str) {
    let worker = shell_worker(temp);
    write_task_config_with_worker(temp, worker.path(), task_json);
}

fn write_task_config_with_worker(
    temp: &assert_fs::TempDir,
    worker_command: &Path,
    task_json: &str,
) {
    write_executable(
        temp.child("luchta-config.sh").path(),
        &format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"shell\":{{\"command\":\"{}\"}}}},\"tasks\":{{{}}}}}'\n",
            worker_command.display(),
            task_json
        ),
    );
}

struct WorkspaceBuilder<'a> {
    yarn_lock: Option<&'a str>,
    task_json: Option<&'a str>,
    script_name: Option<&'a str>,
    extra_files: &'a [(&'a str, &'a str)],
}

impl WorkspaceBuilder<'_> {
    fn build(self, temp: &assert_fs::TempDir) {
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

fn setup_pkgbuild_counter_workspace(
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

fn setup_skip_edit_workspace(temp: &assert_fs::TempDir, yarn_lock: &str) {
    setup_pkgbuild_counter_workspace(
        temp,
        yarn_lock,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt","out.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat src.txt > out.txt"}"#,
        &[("packages/app/src.txt", "one\n")],
    );
}

fn setup_glob_workspace(temp: &assert_fs::TempDir) {
    setup_pkgbuild_counter_workspace(
        temp,
        "",
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src/**/*.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
        &[("packages/app/src/seed.txt", "seed\n")],
    );
}

fn setup_output_workspace(temp: &assert_fs::TempDir) {
    setup_pkgbuild_counter_workspace(
        temp,
        "",
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt","out.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat src.txt > out.txt"}"#,
        &[("packages/app/src.txt", "one\n")],
    );
}

fn write_env_config(temp: &assert_fs::TempDir, foo_value: &str, bar_input: bool) {
    write_counter_task_config(
        temp,
        &format!(
            r#""app#pkgbuild":{{"cache":{{}},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"env":{{"FOO":{{"value":"{foo_value}"}},"BAR":{{"input":{bar_input}}}}},"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}}"#
        ),
    );
}

fn setup_env_workspace(temp: &assert_fs::TempDir) {
    setup_pkgbuild_counter_workspace(
        temp,
        "",
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"env":{"FOO":{"value":"alpha"},"BAR":{"input":false}},"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
        &[("packages/app/src.txt", "one\n")],
    );
}

const YARN1_LOCK_LEFT_PAD_1_0_0: &str = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-XI5MPzVNApjAyhQzZWn7Q4oJ4kBzP4Qzy6wKzH8w0v7lidh+NROm4tW7x1YgnPZXoqBYwygJyI072QtdgQXl3g==
"#;

const YARN1_LOCK_LEFT_PAD_1_1_0: &str = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.1.0:
  version "1.1.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.1.0.tgz#47a2daf581ede454334dee6c6036cae00d912e4d"
  integrity sha512-gzzVNpfkTJpfr3xNbSl9AxW8EBttkTBeIBxECUfSpwvJOCtVXiRHeGCXAvsXAZpXmYR52paNtKxwyq8XynDoRg==
"#;

const YARN_BERRY_LEFT_PAD_1_0_0: &str = r#"__metadata:
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

const YARN_BERRY_LEFT_PAD_1_1_0: &str = r#"__metadata:
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

fn setup_lockfile_workspace(temp: &assert_fs::TempDir, yarn_lock: &str, dep_version: &str) {
    let package_json = format!(
        r#"{{
  "name": "app",
  "dependencies": {{
    "left-pad": "^{dep_version}"
  }},
  "scripts": {{
    "pkgbuild": "echo ignored"
  }}
}}"#
    );

    setup_pkgbuild_counter_workspace(
        temp,
        yarn_lock,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
        &[("packages/app/src.txt", "one\n")],
    );
    temp.child("packages/app/package.json")
        .write_str(&package_json)
        .unwrap();
    git_commit_all(temp.path(), "fixture");
}

fn setup_optional_dependency_workspace(
    temp: &assert_fs::TempDir,
    optional_version: &str,
    output_phrase: &str,
) {
    let package_json = format!(
        r#"{{
  "name": "app",
  "optionalDependencies": {{
    "left-pad": "^{optional_version}"
  }},
  "scripts": {{
    "pkgbuild": "echo ignored"
  }}
}}"#
    );

    setup_pkgbuild_counter_workspace(
        temp,
        YARN1_LOCK_LEFT_PAD_1_0_0,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt","out.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat optional.txt > out.txt"}"#,
        &[("packages/app/src.txt", "one\n")],
    );
    temp.child("packages/app/package.json")
        .write_str(&package_json)
        .unwrap();
    temp.child("packages/app/optional.txt")
        .write_str(&format!("{output_phrase}\n"))
        .unwrap();
    git_commit_all(temp.path(), "fixture");
}

fn setup_dependency_workspace(temp: &assert_fs::TempDir) {
    write_root_workspace(temp);
    temp.child("yarn.lock").write_str("").unwrap();
    let worker = shell_worker(temp);
    write_executable(
        temp.child("luchta-config.sh").path(),
        &format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"shell\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"dependsOn\":[\"^build\"]}},\"lib#build\":{{\"worker\":\"shell\",\"inputs\":[\"src.txt\"],\"outputs\":[\"out.txt\"],\"command\":\"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat src.txt > out.txt\"}},\"app#build\":{{\"cache\":{{}},\"dependsOn\":[\"^build\"],\"worker\":\"shell\",\"inputs\":[\"src.txt\"],\"outputs\":[\"counter.txt\",\"out.txt\"],\"command\":\"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat src.txt ../lib/out.txt > out.txt\"}}}}}}'\n",
            worker.path().display()
        ),
    );

    temp.child("packages/lib").create_dir_all().unwrap();
    temp.child("packages/lib/package.json")
        .write_str(
            r#"{
  "name": "lib",
  "scripts": {
    "build": "echo ignored"
  }
}"#,
        )
        .unwrap();
    temp.child("packages/lib/src.txt")
        .write_str("lib-one\n")
        .unwrap();

    temp.child("packages/app").create_dir_all().unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "dependencies": {
    "lib": "workspace:*"
  },
  "scripts": {
    "build": "echo ignored"
  }
}"#,
        )
        .unwrap();
    temp.child("packages/app/src.txt")
        .write_str("app-one\n")
        .unwrap();
    init_git(temp);
}

fn setup_detected_dependency_workspace(temp: &assert_fs::TempDir) {
    write_root_workspace(temp);
    temp.child("yarn.lock").write_str("").unwrap();
    let worker =
        shell_worker_with_done_fields(temp, Some(",\"outputs\":[\"detected-output.txt\"]"));
    write_task_config_with_worker(
        temp,
        worker.path(),
        r#""build":{"dependsOn":["^build"]},"lib#build":{"worker":"shell","command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cp value.txt detected-output.txt; echo declared-stable > declared-output.txt"},"app#build":{"cache":{},"dependsOn":["^build"],"worker":"shell","outputs":["counter.txt","app-output.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat ../lib/detected-output.txt > app-output.txt"}}"#,
    );

    temp.child("packages/lib").create_dir_all().unwrap();
    temp.child("packages/lib/package.json")
        .write_str(
            r#"{
  "name": "lib",
  "scripts": {
    "build": "echo ignored"
  }
}"#,
        )
        .unwrap();
    temp.child("packages/lib/value.txt")
        .write_str("lib-one\n")
        .unwrap();

    temp.child("packages/app").create_dir_all().unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "dependencies": {
    "lib": "workspace:*"
  },
  "scripts": {
    "build": "echo ignored"
  }
}"#,
        )
        .unwrap();

    init_git(temp);
}

fn setup_root_cache_workspace(temp: &assert_fs::TempDir, root_manifest: &str) {
    write_root_workspace_manifest(temp, root_manifest);
    temp.child("yarn.lock").write_str("").unwrap();
    let worker = shell_worker(temp);
    write_task_config_with_worker(
        temp,
        worker.path(),
        r##""#build":{"cache":{},"worker":"shell","inputs":["root-input.txt"],"outputs":["root-counter.txt","root-output.txt"],"command":"count=$(cat root-counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > root-counter.txt; cat root-input.txt > root-output.txt"}"##,
    );
    temp.child("root-input.txt")
        .write_str("root-one\n")
        .unwrap();
    init_git(temp);
}

fn setup_root_dependency_workspace(temp: &assert_fs::TempDir) {
    write_root_workspace(temp);
    temp.child("yarn.lock").write_str("").unwrap();
    let worker = shell_worker(temp);
    write_task_config_with_worker(
        temp,
        worker.path(),
        r##""#build":{"cache":{},"worker":"shell","inputs":["root-input.txt"],"outputs":["root-counter.txt","root-output.txt"],"command":"count=$(cat root-counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > root-counter.txt; cat root-input.txt > root-output.txt"},"app#build":{"cache":{},"dependsOn":["#build"],"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt","app-output.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat src.txt ../../root-output.txt > app-output.txt"}}"##,
    );
    temp.child("root-input.txt")
        .write_str("root-one\n")
        .unwrap();
    temp.child("packages/app").create_dir_all().unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "build": "echo ignored"
  }
}"#,
        )
        .unwrap();
    temp.child("packages/app/src.txt")
        .write_str("app-one\n")
        .unwrap();
    init_git(temp);
}

fn setup_failed_workspace(temp: &assert_fs::TempDir) {
    WorkspaceBuilder {
        yarn_lock: Some(""),
        task_json: Some(
            r#""app#pkgfail":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["fail-count.txt"],"command":"count=$(cat fail-count.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > fail-count.txt; exit 1"}"#,
        ),
        script_name: Some("pkgfail"),
        extra_files: &[("packages/app/src.txt", "one\n")],
    }
    .build(temp);
}

fn setup_yarn_worker_workspace(temp: &assert_fs::TempDir) -> std::path::PathBuf {
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

fn assert_pkgbuild_runs_then_skips_then_reruns(
    temp: &assert_fs::TempDir,
    first_file: &str,
    first_expected: &str,
    edit: impl FnOnce(&assert_fs::TempDir),
    second_file: &str,
    second_expected: &str,
) {
    run_luchta(temp, "pkgbuild").success();
    temp.child(first_file).assert(first_expected);

    run_luchta(temp, "pkgbuild").success();
    temp.child(first_file).assert(first_expected);

    edit(temp);

    run_luchta(temp, "pkgbuild").success();
    temp.child(second_file).assert(second_expected);
}

fn assert_pkgbuild_input_edit_reruns(
    temp: &assert_fs::TempDir,
    yarn_lock: &str,
    expect_out_file: bool,
) {
    setup_skip_edit_workspace(temp, yarn_lock);

    assert_pkgbuild_runs_then_skips_then_reruns(
        temp,
        "packages/app/counter.txt",
        "1\n",
        |temp| {
            temp.child("packages/app/src.txt")
                .write_str("two\n")
                .unwrap();
        },
        "packages/app/counter.txt",
        "2\n",
    );

    if expect_out_file {
        temp.child("packages/app/out.txt").assert("two\n");
    }
}

fn assert_pkgbuild_lockfile_bump_reruns(
    temp: &assert_fs::TempDir,
    yarn_lock: &str,
    next_yarn_lock: &str,
    mutate_input: bool,
) {
    setup_lockfile_workspace(temp, yarn_lock, "1.0.0");

    assert_pkgbuild_runs_then_skips_then_reruns(
        temp,
        "packages/app/counter.txt",
        "1\n",
        |temp| {
            temp.child("packages/app/package.json")
                .write_str(
                    r#"{
  "name": "app",
  "dependencies": {
    "left-pad": "^1.1.0"
  },
  "scripts": {
    "pkgbuild": "echo ignored"
  }
}"#,
                )
                .unwrap();
            temp.child("yarn.lock").write_str(next_yarn_lock).unwrap();
            if mutate_input {
                temp.child("packages/app/src.txt")
                    .write_str("two\n")
                    .unwrap();
            }
            git_commit_all(temp.path(), "bump left-pad");
        },
        "packages/app/counter.txt",
        "2\n",
    );
}

#[test]
fn cache_yarn_v1_skips_unchanged_and_reruns_on_input_edit() {
    let temp = assert_fs::TempDir::new().unwrap();
    assert_pkgbuild_input_edit_reruns(&temp, YARN1_LOCK_LEFT_PAD_1_0_0, true);
}

#[test]
fn cache_yarn_berry_skips_unchanged_and_reruns_on_input_edit() {
    let temp = assert_fs::TempDir::new().unwrap();
    assert_pkgbuild_input_edit_reruns(&temp, YARN_BERRY_LEFT_PAD_1_0_0, false);
}

#[test]
fn cache_new_glob_match_reruns_without_git_add_and_ignored_file_skips() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_glob_workspace(&temp);

    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.child("packages/app/notes.md")
        .write_str("untracked\n")
        .unwrap();
    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.child("packages/app/src/new.txt")
        .write_str("new\n")
        .unwrap();
    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("2\n");

    temp.child("packages/app/.gitignore")
        .write_str("ignored/\n")
        .unwrap();
    git_commit_paths(
        temp.path(),
        &["packages/app/.gitignore"],
        "ignore generated dir",
    );
    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("2\n");

    temp.child("packages/app/ignored/skip.txt")
        .write_str("skip\n")
        .unwrap();
    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("2\n");
}

#[test]
fn cache_deleted_output_reruns_task() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_output_workspace(&temp);

    assert_pkgbuild_runs_then_skips_then_reruns(
        &temp,
        "packages/app/counter.txt",
        "1\n",
        |temp| {
            fs::remove_file(temp.child("packages/app/out.txt").path()).unwrap();
        },
        "packages/app/counter.txt",
        "2\n",
    );
}

#[test]
fn cache_significant_env_change_reruns_but_input_false_change_skips() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _guard = EnvVarGuard::set("BAR", "first");
    let temp = assert_fs::TempDir::new().unwrap();
    setup_env_workspace(&temp);

    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    // Update env var while still holding the lock and guard
    // The guard will restore to "first" on drop; we just need the lock for serialization
    std::env::set_var("BAR", "second");
    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    write_env_config(&temp, "beta", false);
    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("2\n");
}

#[test]
fn cache_yarn_v1_lockfile_version_bump_reruns() {
    let temp = assert_fs::TempDir::new().unwrap();
    assert_pkgbuild_lockfile_bump_reruns(
        &temp,
        YARN1_LOCK_LEFT_PAD_1_0_0,
        YARN1_LOCK_LEFT_PAD_1_1_0,
        true,
    );
}

#[test]
fn cache_yarn_berry_lockfile_version_bump_reruns() {
    let temp = assert_fs::TempDir::new().unwrap();
    assert_pkgbuild_lockfile_bump_reruns(
        &temp,
        YARN_BERRY_LEFT_PAD_1_0_0,
        YARN_BERRY_LEFT_PAD_1_1_0,
        false,
    );
}

#[test]
fn cache_uncached_dependency_output_change_reruns_downstream_then_skips_when_stable() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_dependency_workspace(&temp);

    run_luchta(&temp, "build").success();
    temp.child("packages/lib/counter.txt").assert("1\n");
    temp.child("packages/app/counter.txt").assert("1\n");

    run_luchta(&temp, "build").success();
    temp.child("packages/lib/counter.txt").assert("2\n");
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.child("packages/lib/src.txt")
        .write_str("lib-two\n")
        .unwrap();
    run_luchta(&temp, "build").success();
    temp.child("packages/lib/counter.txt").assert("3\n");
    temp.child("packages/app/counter.txt").assert("2\n");

    run_luchta(&temp, "build").success();
    temp.child("packages/lib/counter.txt").assert("4\n");
    temp.child("packages/app/counter.txt").assert("2\n");
}

#[test]
fn cache_uncached_detected_dependency_output_change_reruns_downstream_then_skips_when_stable() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_detected_dependency_workspace(&temp);

    run_luchta(&temp, "build").success();
    temp.child("packages/lib/counter.txt").assert("1\n");
    temp.child("packages/app/counter.txt").assert("1\n");
    temp.child("packages/app/app-output.txt")
        .assert("lib-one\n");

    run_luchta(&temp, "build").success();
    temp.child("packages/lib/counter.txt").assert("2\n");
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.child("packages/lib/value.txt")
        .write_str("lib-two\n")
        .unwrap();
    run_luchta(&temp, "build").success();
    temp.child("packages/lib/counter.txt").assert("3\n");
    temp.child("packages/app/counter.txt").assert("2\n");
    temp.child("packages/app/app-output.txt")
        .assert("lib-two\n");

    run_luchta(&temp, "build").success();
    temp.child("packages/lib/counter.txt").assert("4\n");
    temp.child("packages/app/counter.txt").assert("2\n");
}

#[test]
fn cache_corrupt_lockfile_forces_run_and_skips_cache_write() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_lockfile_workspace(&temp, YARN1_LOCK_LEFT_PAD_1_0_0, "1.0.0");

    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.child("yarn.lock")
        .write_str("this is not a valid lockfile\n")
        .unwrap();
    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("2\n");

    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("3\n");
}

#[test]
fn cache_resolve_error_skips_write_and_warns() {
    let temp = assert_fs::TempDir::new().unwrap();
    WorkspaceBuilder {
        yarn_lock: Some(""),
        task_json: Some(
            r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["missing["],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
        ),
        script_name: Some("pkgbuild"),
        extra_files: &[("packages/app/src.txt", "one\n")],
    }
    .build(&temp);

    run_luchta(&temp, "pkgbuild")
        .success()
        .stderr(predicate::str::contains(
        "warning: skipping cache write for task 'app#pkgbuild': failed to resolve cache outputs",
    ));
    temp.child("packages/app/counter.txt").assert("1\n");

    run_luchta(&temp, "pkgbuild")
        .success()
        .stderr(predicate::str::contains(
        "warning: skipping cache write for task 'app#pkgbuild': failed to resolve cache outputs",
    ));
    temp.child("packages/app/counter.txt").assert("2\n");
}

#[test]
fn cache_missing_yarn_lockfile_still_skips_on_second_run() {
    let temp = assert_fs::TempDir::new().unwrap();
    WorkspaceBuilder {
        yarn_lock: None,
        task_json: Some(
            r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
        ),
        script_name: Some("pkgbuild"),
        extra_files: &[("packages/app/src.txt", "one\n")],
    }
    .build(&temp);

    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");
}

#[test]
fn cache_failed_task_always_reruns() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_failed_workspace(&temp);

    run_luchta(&temp, "pkgfail")
        .failure()
        .stderr(predicate::str::contains("task 'app#pkgfail' failed"));
    temp.child("packages/app/fail-count.txt").assert("1\n");

    run_luchta(&temp, "pkgfail")
        .failure()
        .stderr(predicate::str::contains("task 'app#pkgfail' failed"));
    temp.child("packages/app/fail-count.txt").assert("2\n");
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

#[test]
fn cache_root_task_skips_unchanged_and_reruns_on_input_edit() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_root_cache_workspace(
        &temp,
        r#"{
  "name": "root",
  "private": true,
  "workspaces": ["packages/*"]
}"#,
    );

    run_luchta(&temp, "build").success();
    temp.child("root-counter.txt").assert("1\n");
    temp.child("root-output.txt").assert("root-one\n");

    run_luchta(&temp, "build").success();
    temp.child("root-counter.txt").assert("1\n");

    temp.child("root-input.txt")
        .write_str("root-two\n")
        .unwrap();
    run_luchta(&temp, "build").success();
    temp.child("root-counter.txt").assert("2\n");
    temp.child("root-output.txt").assert("root-two\n");
}

#[test]
fn cache_root_task_output_change_reruns_downstream() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_root_dependency_workspace(&temp);

    run_luchta(&temp, "build").success();
    temp.child("root-counter.txt").assert("1\n");
    temp.child("packages/app/counter.txt").assert("1\n");
    temp.child("packages/app/app-output.txt")
        .assert("app-one\nroot-one\n");

    run_luchta(&temp, "build").success();
    temp.child("root-counter.txt").assert("1\n");
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.child("root-input.txt")
        .write_str("root-two\n")
        .unwrap();
    run_luchta(&temp, "build").success();
    temp.child("root-counter.txt").assert("2\n");
    temp.child("packages/app/counter.txt").assert("2\n");
    temp.child("packages/app/app-output.txt")
        .assert("app-one\nroot-two\n");
}

#[test]
fn cache_declared_input_change_without_detected_patterns_reruns() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_skip_edit_workspace(&temp, "");

    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    write_counter_task_config(
        &temp,
        r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["renamed.txt"],"outputs":["counter.txt","out.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat renamed.txt > out.txt"}"#,
    );
    temp.child("packages/app/renamed.txt")
        .write_str("two\n")
        .unwrap();

    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("2\n");
    temp.child("packages/app/out.txt").assert("two\n");
}

#[test]
fn cache_optional_dependency_change_reruns() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_optional_dependency_workspace(&temp, "1.0.0", "optional-v1");

    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");
    temp.child("packages/app/out.txt").assert("optional-v1\n");

    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    setup_optional_dependency_workspace(&temp, "1.1.0", "optional-v2");
    temp.child("yarn.lock")
        .write_str(YARN1_LOCK_LEFT_PAD_1_1_0)
        .unwrap();
    git_commit_all(temp.path(), "optional dep bump");

    run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("2\n");
    temp.child("packages/app/out.txt").assert("optional-v2\n");
}
