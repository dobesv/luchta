//! Cross-package input dependency integration tests.
//!
//! Tests for prefixed input patterns: `#path` (repo root), `pkg#path` (named package),
//! `^glob` (direct upstream), `^^glob` (transitive upstream).

use std::fs;

use assert_cmd::Command;
use assert_fs::prelude::*;

mod common;

use common::{
    git_commit_paths, init_git, shell_worker_with_done_fields, write_counter_task_config,
    write_root_workspace, write_root_workspace_manifest, write_task_config_with_shell_worker,
};

fn run_luchta(temp: &assert_fs::TempDir, task: &str) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.arg("run")
        .arg(task)
        .arg("--workspace-root")
        .arg(temp.path())
        .assert()
}

// =============================================================================
// Setup fixtures
// =============================================================================

fn setup_repo_root_input_workspace(temp: &assert_fs::TempDir) {
    write_root_workspace(temp);
    temp.child("yarn.lock").write_str("").unwrap();
    write_counter_task_config(
        temp,
        r##""app#build":{"cache":{},"worker":"shell","inputs":["#some-root-file.txt"],"outputs":["counter.txt","out.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat ../../some-root-file.txt > out.txt"}"##,
    );
    temp.child("some-root-file.txt")
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
    init_git(temp);
}

fn setup_cross_package_literal_input_workspace(temp: &assert_fs::TempDir) {
    write_root_workspace(temp);
    temp.child("yarn.lock").write_str("").unwrap();
    write_counter_task_config(
        temp,
        r#""app#build":{"cache":{},"worker":"shell","inputs":["lib#file.txt"],"outputs":["counter.txt","out.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat ../lib/file.txt > out.txt 2>/dev/null || :"}"#,
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
    temp.child("packages/lib/file.txt")
        .write_str("lib-one\n")
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
    init_git(temp);
}

fn setup_direct_upstream_glob_workspace(temp: &assert_fs::TempDir) {
    write_root_workspace(temp);
    temp.child("yarn.lock").write_str("").unwrap();
    write_counter_task_config(
        temp,
        r#""build":{"dependsOn":["^build"]},"lib#build":{"worker":"shell","command":"cat lib.ts > out.txt"},"app#build":{"cache":{},"dependsOn":["^build"],"worker":"shell","inputs":["^*.ts"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
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
    temp.child("packages/lib/lib.ts")
        .write_str("export const lib = 1;\n")
        .unwrap();
    temp.child("packages/other").create_dir_all().unwrap();
    temp.child("packages/other/package.json")
        .write_str(
            r#"{
  "name": "other",
  "scripts": {
    "build": "echo ignored"
  }
}"#,
        )
        .unwrap();
    temp.child("packages/other/other.ts")
        .write_str("export const other = 1;\n")
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

/// One package in a multi-package glob fixture: its directory, name, upstream
/// dependency package names (each pinned to `workspace:*`), and a single source
/// file (`src.0` name, `src.1` body).
struct GlobPackage<'a> {
    dir: &'a str,
    name: &'a str,
    deps: &'a [&'a str],
    src: (&'a str, &'a str),
}

/// Write the `package.json` + source file for one [`GlobPackage`]. Keeps the
/// multi-package fixtures compact.
fn write_glob_package(temp: &assert_fs::TempDir, pkg: &GlobPackage<'_>) {
    temp.child(pkg.dir).create_dir_all().unwrap();
    let deps_json = if pkg.deps.is_empty() {
        String::new()
    } else {
        let entries: Vec<String> = pkg
            .deps
            .iter()
            .map(|d| format!("    \"{d}\": \"workspace:*\""))
            .collect();
        format!("  \"dependencies\": {{\n{}\n  }},\n", entries.join(",\n"))
    };
    temp.child(format!("{}/package.json", pkg.dir))
        .write_str(&format!(
            "{{\n  \"name\": \"{}\",\n{deps_json}  \"scripts\": {{\n    \"build\": \"echo ignored\"\n  }}\n}}",
            pkg.name
        ))
        .unwrap();
    temp.child(format!("{}/{}", pkg.dir, pkg.src.0))
        .write_str(pkg.src.1)
        .unwrap();
}

fn setup_transitive_upstream_glob_workspace(temp: &assert_fs::TempDir) {
    write_root_workspace_manifest(
        temp,
        r#"{
  "name": "root",
  "private": true,
  "workspaces": ["packages/*", "vendors/*"]
}"#,
    );
    temp.child("yarn.lock").write_str("").unwrap();
    write_counter_task_config(
        temp,
        r#""build":{"dependsOn":["^build"]},"core#build":{"worker":"shell","command":"cat core.ts > out.txt"},"util#build":{"dependsOn":["^build"],"worker":"shell","command":"cat util.ts > out.txt"},"mirror#build":{"dependsOn":["^build"],"worker":"shell","command":"cat mirror.ts > out.txt"},"app#build":{"cache":{},"dependsOn":["^build"],"worker":"shell","inputs":["^^*.ts"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
    );
    write_glob_package(
        temp,
        &GlobPackage {
            dir: "vendors/core",
            name: "core",
            deps: &[],
            src: ("core.ts", "export const core = 1;\n"),
        },
    );
    write_glob_package(
        temp,
        &GlobPackage {
            dir: "packages/util",
            name: "util",
            deps: &["core"],
            src: ("util.ts", "export const util = 1;\n"),
        },
    );
    write_glob_package(
        temp,
        &GlobPackage {
            dir: "packages/mirror",
            name: "mirror",
            deps: &["core"],
            src: ("mirror.ts", "export const mirror = 1;\n"),
        },
    );
    temp.child("packages/app").create_dir_all().unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "dependencies": {
    "util": "workspace:*",
    "mirror": "workspace:*"
  },
  "scripts": {
    "build": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(temp);
}

fn setup_detected_prefixed_input_workspace(temp: &assert_fs::TempDir) {
    write_root_workspace(temp);
    temp.child("yarn.lock").write_str("").unwrap();
    let worker = shell_worker_with_done_fields(
        temp,
        common::WorkerDoneFields {
            json_fragment: Some(",\"inputs\":[\"#shared.txt\",\"^*.ts\"]"),
        },
    );
    write_task_config_with_shell_worker(
        temp,
        worker.path(),
        r##""build":{"dependsOn":["^build"]},"lib#build":{"worker":"shell","command":"cat lib.ts > out.txt"},"app#build":{"cache":{},"dependsOn":["^build"],"worker":"shell","outputs":["counter.txt","out.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; cat ../../shared.txt ../lib/lib.ts > out.txt"}}"##,
    );
    temp.child("shared.txt").write_str("shared-one\n").unwrap();
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
    temp.child("packages/lib/lib.ts")
        .write_str("export const lib = 1;\n")
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

// =============================================================================
// Tests
// =============================================================================

#[test]
fn cache_repo_root_input_prefix_reruns_on_root_edit() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_repo_root_input_workspace(&temp);

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("1\n");
    temp.child("packages/app/out.txt").assert("root-one\n");

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.child("some-root-file.txt")
        .write_str("root-two\n")
        .unwrap();
    git_commit_paths(temp.path(), &["some-root-file.txt"], "edit repo root input");

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("2\n");
    temp.child("packages/app/out.txt").assert("root-two\n");
}

#[test]
fn cache_cross_package_literal_input_handles_absent_then_readded_file() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_cross_package_literal_input_workspace(&temp);

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("1\n");
    temp.child("packages/app/out.txt").assert("lib-one\n");

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    fs::remove_file(temp.child("packages/lib/file.txt").path()).unwrap();
    git_commit_paths(
        temp.path(),
        &["packages/lib/file.txt"],
        "remove shared file",
    );

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("2\n");
    temp.child("packages/app/out.txt").assert("");

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("2\n");

    temp.child("packages/lib/file.txt")
        .write_str("lib-two\n")
        .unwrap();
    git_commit_paths(
        temp.path(),
        &["packages/lib/file.txt"],
        "restore shared file",
    );

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("3\n");
    temp.child("packages/app/out.txt").assert("lib-two\n");
}

#[test]
fn cache_direct_upstream_glob_input_reruns_on_direct_dependency_edit() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_direct_upstream_glob_workspace(&temp);

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.child("packages/other/other.ts")
        .write_str("export const other = 2;\n")
        .unwrap();
    git_commit_paths(
        temp.path(),
        &["packages/other/other.ts"],
        "edit unrelated package",
    );

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.child("packages/lib/lib.ts")
        .write_str("export const lib = 2;\n")
        .unwrap();
    git_commit_paths(
        temp.path(),
        &["packages/lib/lib.ts"],
        "edit direct upstream input",
    );

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("2\n");
}

#[test]
fn cache_transitive_upstream_glob_input_reruns_on_transitive_edit() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_transitive_upstream_glob_workspace(&temp);

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.child("vendors/core/core.ts")
        .write_str("export const core = 2;\n")
        .unwrap();
    git_commit_paths(
        temp.path(),
        &["vendors/core/core.ts"],
        "edit transitive upstream input",
    );

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("2\n");

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("2\n");
}

#[test]
fn cache_worker_detected_prefixed_inputs_rerun_on_root_and_upstream_edits() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_detected_prefixed_input_workspace(&temp);

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("1\n");
    temp.child("packages/app/out.txt")
        .assert("shared-one\nexport const lib = 1;\n");

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("1\n");

    temp.child("shared.txt").write_str("shared-two\n").unwrap();
    git_commit_paths(temp.path(), &["shared.txt"], "edit detected root input");

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("2\n");
    temp.child("packages/app/out.txt")
        .assert("shared-two\nexport const lib = 1;\n");

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("2\n");

    temp.child("packages/lib/lib.ts")
        .write_str("export const lib = 2;\n")
        .unwrap();
    git_commit_paths(
        temp.path(),
        &["packages/lib/lib.ts"],
        "edit detected upstream input",
    );

    run_luchta(&temp, "build").success();
    temp.child("packages/app/counter.txt").assert("3\n");
    temp.child("packages/app/out.txt")
        .assert("shared-two\nexport const lib = 2;\n");
}

// =============================================================================
// Security: invalid / escaping input patterns must HARD-FAIL the task
// (untrusted worker-reported inputs must not silently resolve or skip).
// =============================================================================

/// Build a single-package `app#build` workspace whose declared `inputs` contain
/// the given (deliberately invalid) prefixed pattern, then run it. Returns the
/// run assertion so callers can assert failure.
fn run_declared_input(temp: &assert_fs::TempDir, bad_input: &str) -> assert_cmd::assert::Assert {
    write_root_workspace(temp);
    temp.child("yarn.lock").write_str("").unwrap();
    write_counter_task_config(
        temp,
        &format!(
            r##""app#build":{{"cache":{{}},"worker":"shell","inputs":["{bad_input}"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}}"##
        ),
    );
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
    init_git(temp);
    run_luchta(temp, "build")
}

#[test]
fn declared_input_path_escape_fails_task() {
    let temp = assert_fs::TempDir::new().unwrap();
    // `#../...` escapes the repo root via `..`.
    run_declared_input(&temp, "#../escape.txt").failure();
}

#[test]
fn declared_input_unknown_package_fails_task() {
    let temp = assert_fs::TempDir::new().unwrap();
    // References a package that does not exist in the workspace graph.
    run_declared_input(&temp, "nonexistent#file.txt").failure();
}

#[test]
fn detected_input_path_escape_fails_task() {
    let temp = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&temp);
    temp.child("yarn.lock").write_str("").unwrap();
    // Worker reports an escaping prefixed detected input — untrusted; must fail.
    let worker = shell_worker_with_done_fields(
        &temp,
        common::WorkerDoneFields {
            json_fragment: Some(",\"inputs\":[\"#../escape.txt\"]"),
        },
    );
    write_task_config_with_shell_worker(
        &temp,
        worker.path(),
        r##""app#build":{"cache":{},"worker":"shell","outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"##,
    );
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
    init_git(&temp);
    run_luchta(&temp, "build").failure();
}
