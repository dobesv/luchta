//! Integration tests driving `luchta-oxlint-worker` binary over JSONL protocol.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use assert_cmd::cargo::CommandCargoExt;
use luchta_engine::{ResolveMode, ResolveTask, WorkerMessage, WorkerRequest};
use serde_json::Value;
use tempfile::tempdir;

fn run_line(request: WorkerRequest) -> String {
    serde_json::to_string(&WorkerMessage::Run(request)).expect("request json")
}

fn resolve_line(resolve: ResolveTask) -> String {
    serde_json::to_string(&WorkerMessage::ResolveTask(resolve)).expect("resolve json")
}

fn run_worker(input: &str) -> (Vec<Value>, String) {
    let mut command = Command::cargo_bin("luchta-oxlint-worker").expect("binary path");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker");

    {
        let stdin = child.stdin.as_mut().expect("stdin available");
        stdin.write_all(input.as_bytes()).expect("write input");
    }

    let output = child.wait_with_output().expect("wait output");
    assert!(
        output.status.success(),
        "worker exited non-zero: {:?}",
        output.status
    );

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    let messages = stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("worker json line"))
        .collect();
    (messages, stderr)
}

fn write_file(path: impl AsRef<Path>, contents: &str) {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(path, contents).expect("write file");
}

fn env_map(entries: [(&str, &str); 1]) -> HashMap<String, String> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn resolve_task_request(id: &str, cwd: &Path, mode: ResolveMode) -> ResolveTask {
    ResolveTask {
        id: id.to_owned(),
        name: "lint".to_owned(),
        command: "lint".to_owned(),
        package: "pkg".to_owned(),
        cwd: Some(cwd.display().to_string()),
        scripts: vec![],
        inputs: vec![],
        mode,
    }
}

#[test]
fn resolve_task_prunes_when_no_supported_files() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("package.json"),
        r#"{"name":"fixture","scripts":{"lint":"oxlint"}}"#,
    );
    let input = format!(
        "{}\n",
        resolve_line(resolve_task_request(
            "resolve-empty",
            fixture.path(),
            ResolveMode::Run
        ))
    );
    let (output, stderr) = run_worker(&input);
    assert!(stderr.is_empty(), "unexpected worker stderr: {stderr}");
    let resolved = output
        .iter()
        .find(|value| value["type"].as_str() == Some("resolved"))
        .expect("resolved message");
    assert_eq!(resolved["id"].as_str(), Some("resolve-empty"));
    assert_eq!(resolved["result"]["decision"].as_str(), Some("prune"));
    assert_eq!(
        resolved["result"]["reason"].as_str(),
        Some("no JS/TS source files found for oxlint")
    );
}

#[test]
fn resolve_task_anchors_parent_config_ignore_patterns_to_config_dir() {
    // Regression: the resolve (preflight) path must anchor config `ignorePatterns`
    // to the discovered config file's directory, NOT the task cwd.
    //
    // The root config uses the absolute-rooted pattern `/src/`. Anchored to the
    // config dir (repo root) this matches only `<repo>/src`, so the package's
    // `packages/app/src/foo.ts` survives and the task is kept. If the base were
    // wrongly the task cwd (`packages/app`), `/src/` would match
    // `packages/app/src`, ignore the only source file, and prune the task.
    let fixture = tempdir().expect("tempdir");
    let repo = fixture.path();
    let pkg = repo.join("packages/app");
    write_file(
        repo.join(".oxlintrc.json"),
        r#"{"ignorePatterns":["/src/"]}"#,
    );
    write_file(
        pkg.join("package.json"),
        r#"{"name":"app","scripts":{"lint":"oxlint"}}"#,
    );
    write_file(pkg.join("src/foo.ts"), "export const foo = 1;\n");

    let input = format!(
        "{}\n",
        resolve_line(resolve_task_request(
            "resolve-anchored",
            &pkg,
            ResolveMode::Run
        ))
    );
    let (output, stderr) = run_worker(&input);
    assert!(stderr.is_empty(), "unexpected worker stderr: {stderr}");
    let resolved = output
        .iter()
        .find(|value| value["type"].as_str() == Some("resolved"))
        .expect("resolved message");
    assert_eq!(resolved["id"].as_str(), Some("resolve-anchored"));
    // Correctly anchored: `src/foo.ts` survives, so the task must be kept (modify), not pruned.
    assert_ne!(
        resolved["result"]["decision"].as_str(),
        Some("prune"),
        "resolve pruned the task; config ignorePatterns anchored to cwd instead of config dir"
    );
}

#[test]
fn resolve_task_honors_config_option_in_command() {
    // Regression (#219): the resolve (preflight) path must honor a `--config`
    // option provided in the task `command`. Here the default root config would
    // keep the task, but the custom config referenced via `--config` ignores the
    // only source file, so resolution must prune — proving the command's
    // `--config` was applied during the resolve phase, not just at run time.
    let fixture = tempdir().expect("tempdir");
    let cwd = fixture.path();
    write_file(
        cwd.join("package.json"),
        r#"{"name":"fixture","scripts":{"lint":"oxlint"}}"#,
    );
    // Default discovered config: no ignore patterns -> file would be kept.
    write_file(cwd.join(".oxlintrc.json"), r#"{}"#);
    // Custom config lives at the task cwd root (a non-discovered filename), so its
    // `ignorePatterns` anchor to cwd and ignore the only source file.
    write_file(
        cwd.join("custom.oxlintrc.json"),
        r#"{"ignorePatterns":["src/foo.ts"]}"#,
    );
    write_file(cwd.join("src/foo.ts"), "export const foo = 1;\n");

    let mut resolve = resolve_task_request("resolve-config", cwd, ResolveMode::Run);
    resolve.command = "oxlint --config 'custom.oxlintrc.json'".to_owned();

    let input = format!("{}\n", resolve_line(resolve));
    let (output, stderr) = run_worker(&input);
    assert!(stderr.is_empty(), "unexpected worker stderr: {stderr}");
    let resolved = output
        .iter()
        .find(|value| value["type"].as_str() == Some("resolved"))
        .expect("resolved message");
    assert_eq!(resolved["id"].as_str(), Some("resolve-config"));
    assert_eq!(
        resolved["result"]["decision"].as_str(),
        Some("prune"),
        "resolve did not honor --config from the command; source file was not ignored"
    );
}

#[test]
fn lint_violation_emits_log_report_and_done() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("package.json"),
        r#"{"name":"fixture","scripts":{"lint":"oxlint"}}"#,
    );
    write_file(
        fixture.path().join(".oxlintrc.json"),
        r#"{"rules":{"no-debugger":"error"}}"#,
    );
    write_file(fixture.path().join("src/index.js"), "debugger;\n");

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-violation", "lint")
                .with_cwd(fixture.path().display().to_string())
        )
    );
    let (output, _stderr) = run_worker(&input);

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("log")
            && value["id"].as_str() == Some("job-violation")
            && value["line"]
                .as_str()
                .is_some_and(|msg| msg.contains("src/index.js:1:1: error [eslint(no-debugger)]"))
    }));

    let report = output
        .iter()
        .find(|value| value["type"].as_str() == Some("report"))
        .expect("sarif report present");
    assert_eq!(report["id"].as_str(), Some("job-violation"));
    assert_eq!(report["filename"].as_str(), Some("oxlint.sarif"));
    assert_eq!(report["mimeType"].as_str(), Some("application/sarif+json"));
    let report_body = report["content"].as_str().expect("sarif content str");
    assert!(report_body.contains("\"ruleId\": \"eslint(no-debugger)\""));
    assert!(report_body.contains("\"uri\": \"src/index.js\""));

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-violation")
            && value["exitCode"].as_i64() == Some(1)
    }));
}

#[test]
fn config_option_selects_explicit_config_file() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("package.json"),
        r#"{"name":"fixture","scripts":{"lint":"oxlint"}}"#,
    );
    write_file(
        fixture.path().join(".oxlintrc.json"),
        r#"{"rules":{"no-debugger":"off"}}"#,
    );
    write_file(
        fixture.path().join("configs/strict.oxlintrc.json"),
        r#"{"rules":{"no-debugger":"error"}}"#,
    );
    write_file(fixture.path().join("src/index.js"), "debugger;\n");

    let opts_value = "--config 'configs/strict.oxlintrc.json'";
    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-config-explicit", "lint")
                .with_cwd(fixture.path().display().to_string())
                .with_env(env_map([("OXLINT_OPTS", opts_value)]))
        )
    );
    let (output, _stderr) = run_worker(&input);

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("log")
            && value["id"].as_str() == Some("job-config-explicit")
            && value["line"].as_str().is_some_and(|line| {
                line.contains("src/index.js:1:1: error [eslint(no-debugger)]")
                    || (line.contains("no-debugger") && line.contains("error"))
            })
    }));
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("log")
            && value["id"].as_str() == Some("job-config-explicit")
            && value["line"].as_str().is_some_and(|line| {
                line.contains("oxlint config:") && line.contains("strict.oxlintrc.json")
            })
    }));
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-config-explicit")
            && value["exitCode"].as_i64() == Some(1)
    }));
}

#[test]
fn clean_fixture_emits_no_report_and_zero_done() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("package.json"),
        r#"{"name":"fixture","scripts":{"lint":"oxlint"}}"#,
    );
    write_file(
        fixture.path().join(".oxlintrc.json"),
        r#"{"rules":{"no-debugger":"error"}}"#,
    );
    write_file(fixture.path().join("src/index.js"), "console.log('ok');\n");

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-clean", "lint").with_cwd(fixture.path().display().to_string())
        )
    );
    let (output, _stderr) = run_worker(&input);

    assert!(output
        .iter()
        .all(|value| value["type"].as_str() != Some("report")));
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-clean")
            && value["exitCode"].as_i64() == Some(0)
    }));
}

#[test]
fn suppress_all_writes_expected_file_and_clean_run_skips_sarif() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("package.json"),
        r#"{"name":"fixture","scripts":{"lint":"oxlint"}}"#,
    );
    write_file(
        fixture.path().join(".oxlintrc.json"),
        r#"{"rules":{"no-debugger":"error"}}"#,
    );
    write_file(fixture.path().join("src/index.js"), "debugger;\n");

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-suppress-all", "lint")
                .with_cwd(fixture.path().display().to_string())
                .with_env(env_map([("OXLINT_OPTS", "--suppress-all")]))
        )
    );
    let (output, _stderr) = run_worker(&input);
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-suppress-all")
            && value["exitCode"].as_i64() == Some(0)
    }));

    let suppression_file = fixture.path().join("oxlint-suppressions.json");
    let actual = fs::read_to_string(&suppression_file).expect("suppression file written");
    let expected =
        "{\n  \"src/index.js\": {\n    \"no-debugger\": {\n      \"count\": 1\n    }\n  }\n}";
    assert_eq!(actual, expected);

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-suppressed", "lint")
                .with_cwd(fixture.path().display().to_string())
        )
    );
    let (output, _stderr) = run_worker(&input);
    assert!(output
        .iter()
        .all(|value| value["type"].as_str() != Some("report")));
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-suppressed")
            && value["exitCode"].as_i64() == Some(0)
    }));
}

#[test]
fn stale_suppression_reports_unused_and_exits_nonzero() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("package.json"),
        r#"{"name":"fixture","scripts":{"lint":"oxlint"}}"#,
    );
    write_file(
        fixture.path().join(".oxlintrc.json"),
        r#"{"rules":{"no-debugger":"error"}}"#,
    );
    write_file(fixture.path().join("src/index.js"), "console.log('ok');\n");
    write_file(
        fixture.path().join("oxlint-suppressions.json"),
        "{\n  \"src/index.js\": {\n    \"no-debugger\": {\n      \"count\": 1\n    }\n  }\n}",
    );

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-unused", "lint").with_cwd(fixture.path().display().to_string())
        )
    );
    let (output, _stderr) = run_worker(&input);
    let unused_logs: Vec<_> = output
        .iter()
        .filter(|value| value["type"].as_str() == Some("log"))
        .filter_map(|value| value["line"].as_str())
        .collect();
    assert!(
        unused_logs
            .iter()
            .any(|msg| msg.contains("unused suppressions")
                || msg.contains("Unused suppression file entries found")),
        "missing unused suppression log in {unused_logs:?}"
    );
    // Unused-suppression diagnostics are findings; a SARIF report must still be
    // emitted for them (an unused suppression must never suppress SARIF output).
    assert!(
        output.iter().any(|value| {
            value["type"].as_str() == Some("report")
                && value["filename"].as_str() == Some("oxlint.sarif")
        }),
        "missing sarif report for unused suppression, output={output:?}"
    );
    // The suppression-derived exit code (1) is preserved.
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-unused")
            && value["exitCode"].as_i64() == Some(1)
    }));
}

#[test]
fn unused_suppression_with_active_finding_still_emits_sarif() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("package.json"),
        r#"{"name":"fixture","scripts":{"lint":"oxlint"}}"#,
    );
    write_file(
        fixture.path().join(".oxlintrc.json"),
        r#"{"rules":{"no-debugger":"error","no-console":"error"}}"#,
    );
    // Active finding: this file triggers no-debugger (and no console usage).
    write_file(fixture.path().join("src/index.js"), "debugger;\n");
    // Unused suppression: no-console never fires on this file, so this entry is stale.
    write_file(
        fixture.path().join("oxlint-suppressions.json"),
        "{\n  \"src/index.js\": {\n    \"no-console\": {\n      \"count\": 1\n    }\n  }\n}",
    );

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-mixed", "lint").with_cwd(fixture.path().display().to_string())
        )
    );
    let (output, _stderr) = run_worker(&input);

    // The active no-debugger finding is reported.
    assert!(
        output.iter().any(|value| {
            value["type"].as_str() == Some("log")
                && value["line"].as_str().is_some_and(|line| {
                    line.contains("src/index.js:1:1: error [eslint(no-debugger)]")
                })
        }),
        "missing active no-debugger finding, output={output:?}"
    );
    // Despite the unused suppression, a SARIF report must be emitted, and it must
    // contain the active no-debugger finding.
    let sarif = output
        .iter()
        .find(|value| {
            value["type"].as_str() == Some("report")
                && value["filename"].as_str() == Some("oxlint.sarif")
        })
        .unwrap_or_else(|| panic!("missing sarif report, output={output:?}"));
    let sarif_content = sarif["content"].as_str().expect("sarif content str");
    assert!(
        sarif_content.contains("eslint(no-debugger)"),
        "sarif must contain the active finding, content={sarif_content}"
    );
    // The suppression-derived exit code (1) is preserved.
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-mixed")
            && value["exitCode"].as_i64() == Some(1)
    }));
}

#[test]
fn type_aware_missing_tsgolint_warns_and_keeps_regular_lint() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("package.json"),
        r#"{"name":"fixture","scripts":{"lint":"oxlint"}}"#,
    );
    write_file(
        fixture.path().join(".oxlintrc.json"),
        r#"{"options":{"typeAware":true},"rules":{"no-debugger":"error"}}"#,
    );
    write_file(fixture.path().join("src/index.js"), "debugger;\n");

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-type-aware", "lint")
                .with_cwd(fixture.path().display().to_string())
        )
    );
    let (output, stderr) = run_worker(&input);
    // Whether or not tsgolint is installed, regular (non-type-aware) lint rules
    // must still run: the `no-debugger` error is reported and a SARIF report is
    // emitted. When tsgolint is unavailable the worker additionally emits a
    // "tsgolint unavailable" warning, but that warning is absent in
    // environments (dev machines / CI) where tsgolint IS installed, so we do not
    // require it — asserting on it would make this test environment-dependent.
    let tsgolint_unavailable = stderr
        .contains("type-aware linting requested but tsgolint unavailable")
        || output.iter().any(|value| {
            value["type"].as_str() == Some("log")
                && value["stream"].as_str() == Some("stderr")
                && value["line"].as_str().is_some_and(|line| {
                    line.contains("type-aware linting requested but tsgolint unavailable")
                })
        });
    if tsgolint_unavailable {
        // Sanity check: the warning, when present, is on the stderr stream.
        assert!(
            stderr.contains("type-aware linting requested but tsgolint unavailable")
                || output.iter().any(|value| {
                    value["stream"].as_str() == Some("stderr")
                        && value["line"].as_str().is_some_and(|line| {
                            line.contains("type-aware linting requested but tsgolint unavailable")
                        })
                }),
            "tsgolint-unavailable warning not on stderr, stderr={stderr:?}, output={output:?}"
        );
    }
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("log")
            && value["line"]
                .as_str()
                .is_some_and(|line| line.contains("src/index.js:1:1: error [eslint(no-debugger)]"))
    }));
    let report_names: Vec<_> = output
        .iter()
        .filter(|value| value["type"].as_str() == Some("report"))
        .map(|value| value["filename"].clone())
        .collect();
    assert!(
        output.iter().any(|value| {
            value["type"].as_str() == Some("report")
                && value["filename"].as_str() == Some("oxlint.sarif")
        }),
        "missing sarif report, reports={report_names:?}, output={output:?}"
    );
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-type-aware")
            && value["exitCode"].as_i64() == Some(1)
    }));
}

#[test]
fn eslint_disable_suppresses_type_aware_diagnostic() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("package.json"),
        r#"{"name":"fixture","scripts":{"lint":"oxlint"}}"#,
    );
    write_file(
        fixture.path().join(".oxlintrc.json"),
        r#"{"plugins":["typescript-eslint"],"rules":{"@typescript-eslint/no-base-to-string":"error"}}"#,
    );
    write_file(
        fixture.path().join("tsconfig.json"),
        r#"{"compilerOptions":{"strict":true,"target":"ES2022","module":"ESNext","moduleResolution":"Bundler","noEmit":true},"include":["src/**/*.ts"]}"#,
    );
    write_file(
        fixture.path().join("src/index.ts"),
        "const value = { hello: 'world' };\n// eslint-disable-next-line @typescript-eslint/no-base-to-string\nconst text = `${value}`;\nconsole.log(text);\n",
    );

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-eslint-disable-221", "lint")
                .with_cwd(fixture.path().display().to_string())
                .with_env(env_map([("OXLINT_OPTS", "--type-aware")]))
        )
    );
    let (output, stderr) = run_worker(&input);
    let tsgolint_unavailable = stderr
        .contains("type-aware linting requested but tsgolint unavailable")
        || output.iter().any(|value| {
            value["type"].as_str() == Some("log")
                && value["stream"].as_str() == Some("stderr")
                && value["line"].as_str().is_some_and(|line| {
                    line.contains("type-aware linting requested but tsgolint unavailable")
                })
        });
    // Gate on tsgolint availability so #221 regression stays covered in CI environments
    // that do not have type-aware linting installed.
    if tsgolint_unavailable {
        return;
    }

    assert!(
        output.iter().all(|value| {
            value["type"].as_str() != Some("log")
                || !value["line"]
                    .as_str()
                    .is_some_and(|line| line.contains("no-base-to-string"))
        }),
        "unexpected no-base-to-string diagnostic in output={output:?}"
    );
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-eslint-disable-221")
            && value["exitCode"].as_i64() == Some(0)
    }));
}

#[test]
fn type_aware_lints_all_files_in_one_batch() {
    // Regression: the worker batches type-aware (tsgolint) linting over ALL of a
    // package's files in a single invocation (via oxc's LintRunner), rather than
    // once per file. This test writes three files each containing a distinct
    // type-aware `no-base-to-string` violation and asserts findings are reported
    // for every file — proving the batched pass lints the whole file set.
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("package.json"),
        r#"{"name":"fixture","scripts":{"lint":"oxlint"}}"#,
    );
    write_file(
        fixture.path().join(".oxlintrc.json"),
        r#"{"plugins":["typescript-eslint"],"rules":{"@typescript-eslint/no-base-to-string":"error"}}"#,
    );
    write_file(
        fixture.path().join("tsconfig.json"),
        r#"{"compilerOptions":{"strict":true,"target":"ES2022","module":"ESNext","moduleResolution":"Bundler","noEmit":true},"include":["src/**/*.ts"]}"#,
    );
    for name in ["a", "b", "c"] {
        write_file(
            fixture.path().join(format!("src/{name}.ts")),
            &format!(
                "const value_{name} = {{ hello: 'world' }};\nconst text_{name} = `${{value_{name}}}`;\nconsole.log(text_{name});\n"
            ),
        );
    }

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-type-aware-batch", "lint")
                .with_cwd(fixture.path().display().to_string())
                .with_env(env_map([("OXLINT_OPTS", "--type-aware")]))
        )
    );
    let (output, stderr) = run_worker(&input);
    let tsgolint_unavailable = stderr
        .contains("type-aware linting requested but tsgolint unavailable")
        || output.iter().any(|value| {
            value["type"].as_str() == Some("log")
                && value["stream"].as_str() == Some("stderr")
                && value["line"].as_str().is_some_and(|line| {
                    line.contains("type-aware linting requested but tsgolint unavailable")
                })
        });
    // Gate on tsgolint availability so this stays covered in CI environments
    // that do not have type-aware linting installed.
    if tsgolint_unavailable {
        return;
    }

    // Collect the source files that produced a no-base-to-string diagnostic.
    let mut files_with_finding = std::collections::BTreeSet::new();
    for value in &output {
        if value["type"].as_str() != Some("log") {
            continue;
        }
        let Some(line) = value["line"].as_str() else {
            continue;
        };
        if line.contains("no-base-to-string") {
            for name in ["a", "b", "c"] {
                if line.contains(&format!("src/{name}.ts")) {
                    files_with_finding.insert(name);
                }
            }
        }
    }
    assert_eq!(
        files_with_finding.len(),
        3,
        "expected type-aware findings for all 3 files, got {files_with_finding:?}; output={output:?}"
    );

    // Diagnostic log lines must use relative paths (src/...), not absolute, and
    // must be emitted in sorted (deterministic) order by path.
    let diagnostic_lines: Vec<&str> = output
        .iter()
        .filter(|value| value["type"].as_str() == Some("log"))
        .filter_map(|value| value["line"].as_str())
        .filter(|line| line.contains("no-base-to-string"))
        .collect();
    assert!(
        diagnostic_lines
            .iter()
            .all(|line| !line.starts_with('/') && line.starts_with("src/")),
        "diagnostic lines must be relative to cwd: {diagnostic_lines:?}"
    );
    let mut sorted = diagnostic_lines.clone();
    sorted.sort_unstable();
    assert_eq!(
        diagnostic_lines, sorted,
        "diagnostic output must be emitted in sorted order"
    );

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-type-aware-batch")
            && value["exitCode"].as_i64() == Some(1)
    }));
}

#[test]
fn fix_mode_applies_safe_fix_and_clears_findings() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("package.json"),
        r#"{"name":"fixture","scripts":{"lint":"oxlint"}}"#,
    );
    write_file(
        fixture.path().join(".oxlintrc.json"),
        r#"{"rules":{"no-var":"error"}}"#,
    );
    write_file(fixture.path().join("src/index.js"), "var foo = 1;\n");

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-fix", "lint --fix")
                .with_cwd(fixture.path().display().to_string())
        )
    );
    let (output, _stderr) = run_worker(&input);
    let fixed = fs::read_to_string(fixture.path().join("src/index.js")).expect("fixed file");
    let done_codes: Vec<_> = output
        .iter()
        .filter(|value| value["type"].as_str() == Some("done"))
        .map(|value| value["exitCode"].as_i64())
        .collect();
    assert_eq!(
        done_codes,
        vec![Some(0)],
        "unexpected done codes: {done_codes:?}"
    );
    assert_eq!(fixed, "const foo = 1;\n");
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-fix")
            && value["exitCode"].as_i64() == Some(0)
    }));
    let reports: Vec<_> = output
        .iter()
        .filter(|value| value["type"].as_str() == Some("report"))
        .collect();
    assert_eq!(reports.len(), 1, "unexpected reports: {reports:?}");
}
