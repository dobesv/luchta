use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use luchta_test_support::require_nextest;
use luchta_tsrs_worker::inputs::{resolve_inputs, resolve_task};
use luchta_worker::{ResolveDecision, ResolveMode, ResolveTask};
use luchta_yarn_env::test_fixture::{FixtureOptions, PnpFixture};
use serde_json::{json, Value};

fn write(root: &Path, file: &str, text: &str) {
    let path = root.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn assert_inputs(root: &Path, extra: &[&str]) {
    let mut expected = vec!["tsconfig.build.json", "tsconfig.json"];
    expected.extend_from_slice(extra);
    expected.sort_unstable();
    assert_eq!(resolve_inputs(root).unwrap(), expected);
}

fn request(root: &Path) -> ResolveTask {
    ResolveTask {
        id: "r1".to_owned(),
        name: "build".to_owned(),
        command: String::new(),
        package: "p".to_owned(),
        cwd: Some(root.to_string_lossy().into_owned()),
        scripts: Vec::new(),
        inputs: Vec::new(),
        mode: ResolveMode::Run,
    }
}

// Go compile_test: TestResolveInputs.
#[test]
fn resolve_inputs_from_config() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "tsconfig.json",
        r#"{"compilerOptions":{"outDir":"dist"},"include":["src/**/*"]}"#,
    );
    write(temp.path(), "src/index.ts", "export const x = 1;\n");
    assert_inputs(temp.path(), &["src/**/*"]);
}

// Go compile_test: TestResolveInputsNoTsconfig.
#[test]
fn resolve_inputs_no_tsconfig() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    assert_inputs(temp.path(), &["src/**"]);
}

// Go compile_test: TestResolveInputsIncludeGlobWithoutMatchingFiles.
#[test]
fn resolve_inputs_include_glob_without_matching_files() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "tsconfig.json",
        r#"{"compilerOptions":{"outDir":"dist"},"include":["lib/**/*.ts","types/**/*"]}"#,
    );
    assert_inputs(temp.path(), &["lib/**/*.ts", "types/**/*"]);
}

// Go compile_test: TestResolveInputsIncludeFromExtends.
#[test]
fn resolve_inputs_include_from_extends() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "tsconfig.base.json",
        r#"{"include":["packages/**/*.ts"]}"#,
    );
    write(
        temp.path(),
        "tsconfig.json",
        r#"{"extends":"./tsconfig.base.json","compilerOptions":{"outDir":"dist"}}"#,
    );
    assert_inputs(temp.path(), &["packages/**/*.ts", "tsconfig.base.json"]);
}

// Go compile_test: TestResolveInputsExtendsFromSubdirIsTracked.
#[test]
fn resolve_inputs_extends_from_subdir_is_tracked() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "config/tsconfig.base.json",
        r#"{"include":["lib/**/*.ts"]}"#,
    );
    write(
        temp.path(),
        "tsconfig.json",
        r#"{"extends":"./config/tsconfig.base.json","compilerOptions":{"outDir":"dist"}}"#,
    );
    assert_inputs(
        temp.path(),
        &["config/lib/**/*.ts", "config/tsconfig.base.json"],
    );
}

// Go compile_test: TestResolveInputsBareExtendsUnderPnp. PnpFixture's
// unplugged native-thing package replaces the Go fixture's sibling base package.
#[test]
fn resolve_inputs_bare_extends_under_pnp() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let root = tsrs_vfs::osvfs::fs().realpath(&temp.path().to_string_lossy());
    let fixture = PnpFixture::write(Path::new(&root), FixtureOptions::default());
    let base = ".yarn/unplugged/native-thing-npm-1.0.0-xyz/node_modules/native-thing";
    write(
        &fixture.root,
        &format!("{base}/strict.json"),
        r#"{"include":["shared/**/*.ts"]}"#,
    );
    write(
        &fixture.root,
        &format!("{base}/shared/existing.ts"),
        "export const shared = true;\n",
    );
    write(
        &fixture.app_dir,
        "tsconfig.json",
        r#"{"extends":"native-thing/strict.json","compilerOptions":{"outDir":"dist"}}"#,
    );
    assert_inputs(&fixture.app_dir, &[&format!("../../{base}/shared/**/*.ts")]);
}

// Go compile_test: TestResolveInputsUnresolvableBareExtendsFails.
#[test]
fn resolve_inputs_unresolvable_bare_extends_fails() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "tsconfig.json",
        r#"{"extends":"@company/missing-config/strict.json","include":["src/**/*"]}"#,
    );
    let error = resolve_inputs(temp.path()).unwrap_err();
    assert!(error.contains("tsconfig.json"), "{error}");
    assert!(
        error.contains("@company/missing-config/strict.json"),
        "{error}"
    );
    assert_eq!(
        resolve_task(&request(temp.path())).decision,
        ResolveDecision::Accept
    );
}

#[test]
fn resolve_inputs_default_include_with_existing_config() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    for config in ["{}", r#"{"include":[]}"#, r#"{"files":["index.ts"]}"#] {
        write(temp.path(), "tsconfig.json", config);
        write(temp.path(), "index.ts", "export {};\n");
        assert_inputs(temp.path(), &["src/**"]);
    }
}

#[test]
fn resolve_inputs_both_candidates_are_merged() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "tsconfig.json",
        r#"{"include":["src/**/*","shared/**/*"]}"#,
    );
    write(
        temp.path(),
        "tsconfig.build.json",
        r#"{"include":["lib/**/*","shared/**/*"]}"#,
    );
    assert_inputs(temp.path(), &["lib/**/*", "shared/**/*", "src/**/*"]);
}

#[test]
fn resolve_inputs_tracks_chain_but_not_escaping_configs() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let package = temp.path().join("package");
    write(
        temp.path(),
        "outside.json",
        r#"{"include":["external/**/*"]}"#,
    );
    write(
        &package,
        "config/base.json",
        r#"{"extends":"../../outside.json"}"#,
    );
    write(
        &package,
        "tsconfig.json",
        r#"{"extends":"./config/base.json"}"#,
    );
    assert_inputs(
        &package,
        &["config/../../external/**/*", "config/base.json"],
    );
}

#[test]
fn resolve_inputs_tracks_local_extends_array_and_chain() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "config/first.json",
        r#"{"include":["first/**/*"]}"#,
    );
    write(
        temp.path(),
        "config/second.json",
        r#"{"extends":"./first.json","include":["second/**/*"]}"#,
    );
    write(
        temp.path(),
        "tsconfig.json",
        r#"{"extends":["./config/first.json","./config/second.json"]}"#,
    );
    assert_inputs(
        temp.path(),
        &[
            "config/first.json",
            "config/second.json",
            "config/second/**/*",
        ],
    );
}

#[test]
fn resolve_task_parse_errors_accept_without_partial_inputs() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    // A valid first candidate must not hide an error in the second candidate.
    write(
        temp.path(),
        "tsconfig.build.json",
        r#"{"include":["build/**/*"]}"#,
    );
    for config in [
        r#"{"include":["src/**"],"compilerOptions":{"target":"not-a-target"}}"#,
        r#"{"extends":"./missing.json"}"#,
        r#"{"extends":"./tsconfig.json"}"#,
        r#"{"include":["src/**"]"#,
    ] {
        write(temp.path(), "tsconfig.json", config);
        assert!(resolve_inputs(temp.path()).is_err(), "{config}");
        assert_eq!(
            resolve_task(&request(temp.path())).decision,
            ResolveDecision::Accept,
            "{config}"
        );
    }
}

#[test]
fn resolve_task_without_cwd_accepts() {
    let temp = tempfile::tempdir().unwrap();
    let mut req = request(temp.path());
    req.cwd = None;
    assert_eq!(resolve_task(&req).decision, ResolveDecision::Accept);
}

fn serve_resolve(root: &Path, mode: &str, inputs: &[&str]) -> (Value, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_luchta-tsrs-worker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let message = json!({
        "type":"resolveTask", "id":"r1", "name":"build", "command":"", "package":"p",
        "cwd":root, "scripts":[], "mode":mode, "inputs":inputs
    });
    writeln!(child.stdin.take().unwrap(), "{message}").unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stderr}");
    let response = serde_json::from_slice(&output.stdout).unwrap();
    (response, stderr)
}

// Go worker_test: TestServeResolveTaskModifies.
#[test]
fn resolve_serve_task_modifies() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "tsconfig.json",
        r#"{"include":["src/**/*"],"compilerOptions":{"outDir":"dist"}}"#,
    );
    write(temp.path(), "src/index.ts", "export const x = 1;\n");
    let (response, stderr) = serve_resolve(temp.path(), "run", &[]);
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(response["type"], "resolved");
    assert_eq!(response["id"], "r1");
    assert_eq!(response["result"]["decision"], "modify");
    assert_eq!(
        response["result"]["inputs"],
        json!(["src/**/*", "tsconfig.build.json", "tsconfig.json"])
    );
    let lock = include_str!("../../../Cargo.lock");
    let compiler_rev = luchta_lockfile_version::package_git_rev(lock, "tsrs_compiler").unwrap();
    let core_rev = luchta_lockfile_version::package_git_rev(lock, "tsrs_core").unwrap();
    assert_eq!(
        response["result"]["toolVersion"],
        format!("tsrs_compiler={compiler_rev},tsrs_core={core_rev}")
    );
}

// Go worker_test: TestServeResolveTaskMergesDeclaredInputs.
#[test]
fn resolve_serve_task_merges_declared_inputs() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), "tsconfig.json", r#"{"include":["src/**/*"]}"#);
    write(temp.path(), "src/index.ts", "export const x = 1;\n");
    for mode in ["run", "check"] {
        let (response, stderr) = serve_resolve(
            temp.path(),
            mode,
            &["custom/**", "extra.txt", "src/**/*", "custom/**"],
        );
        assert!(stderr.is_empty(), "{stderr}");
        assert_eq!(response["result"]["decision"], "modify");
        assert_eq!(
            response["result"]["inputs"],
            json!([
                "custom/**",
                "extra.txt",
                "src/**/*",
                "tsconfig.build.json",
                "tsconfig.json"
            ])
        );
    }
}

#[test]
fn resolve_serve_parse_error_accepts() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "tsconfig.json",
        r#"{"extends":"./missing.json"}"#,
    );
    for mode in ["run", "check"] {
        let (response, stderr) = serve_resolve(temp.path(), mode, &["custom/**"]);
        assert_eq!(
            response,
            json!({"type":"resolved", "id":"r1", "result":{"decision":"accept"}})
        );
        assert!(stderr.contains("resolve inputs:"), "{stderr}");
        assert!(stderr.contains("missing.json"), "{stderr}");
    }
}

#[cfg(target_os = "linux")]
fn rss_kib() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn resolve_sequential_rss_is_bounded() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    // A generated paths table makes leaked config ASTs exceed allocator noise.
    let paths: serde_json::Map<String, Value> = (0..16384)
        .map(|index| {
            (
                format!("alias{index}"),
                json!([format!("./src/file{index}")]),
            )
        })
        .collect();
    write(
        temp.path(),
        "tsconfig.json",
        &json!({"compilerOptions": {"paths": paths}, "include": ["src/**/*"]}).to_string(),
    );
    write(temp.path(), "src/index.ts", "export const x = 1;\n");
    for _ in 0..3 {
        assert_inputs(temp.path(), &["src/**/*"]);
    }
    let baseline = rss_kib();
    // Stay on one thread, like a reused spawn_blocking worker thread.
    for _ in 0..20 {
        assert_inputs(temp.path(), &["src/**/*"]);
    }
    let final_rss = rss_kib();
    eprintln!("resolve RSS: baseline={baseline} KiB final={final_rss} KiB");
    assert!(
        final_rss <= baseline * 3 / 2 + 64 * 1024,
        "RSS grew: baseline={baseline} KiB final={final_rss} KiB"
    );
}
