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
    assert!(output
        .iter()
        .all(|value| value["type"].as_str() != Some("report")));
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-unused")
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
    assert!(
        stderr.contains("type-aware linting requested but tsgolint unavailable")
            || output.iter().any(|value| {
                value["type"].as_str() == Some("log")
                    && value["stream"].as_str() == Some("stderr")
                    && value["line"].as_str().is_some_and(|line| {
                        line.contains("type-aware linting requested but tsgolint unavailable")
                    })
            }),
        "missing tsgolint warning, stderr={stderr:?}, output={output:?}"
    );
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
fn fix_mode_still_reports_active_findings() {
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
            WorkerRequest::new("job-fix", "lint")
                .with_cwd(fixture.path().display().to_string())
                .with_env(env_map([("OXLINT_OPTS", "--fix")]))
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
        vec![Some(1)],
        "unexpected done codes: {done_codes:?}"
    );
    assert_eq!(fixed, "var foo = 1;\n");
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-fix")
            && value["exitCode"].as_i64() == Some(1)
    }));
    assert!(output
        .iter()
        .any(|value| value["type"].as_str() == Some("report")));
}
