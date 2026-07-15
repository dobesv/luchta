//! Integration tests driving `luchta-ast-grep-worker` binary over JSONL protocol.

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
    run_worker_with_env(input, &[])
}

fn run_worker_with_env(input: &str, envs: &[(&str, &str)]) -> (Vec<Value>, String) {
    run_worker_full(input, None, envs)
}

/// Runs the worker with the process cwd set to `repo_root`, mirroring the
/// engine spawning workers with cwd = workspace root so diagnostic output
/// paths are relative to `repo_root` rather than the test binary's own cwd.
fn run_worker_in(repo_root: &Path, input: &str) -> (Vec<Value>, String) {
    run_worker_full(input, Some(repo_root), &[])
}

fn run_worker_full(
    input: &str,
    repo_root: Option<&Path>,
    envs: &[(&str, &str)],
) -> (Vec<Value>, String) {
    let mut command = Command::cargo_bin("luchta-ast-grep-worker").expect("binary path");
    if let Some(repo_root) = repo_root {
        command.current_dir(repo_root);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
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

fn resolve_task_request(id: &str, cwd: Option<&Path>, mode: ResolveMode) -> ResolveTask {
    ResolveTask {
        id: id.to_owned(),
        name: "lint".to_owned(),
        command: "lint".to_owned(),
        package: "pkg".to_owned(),
        cwd: cwd.map(|path| path.display().to_string()),
        scripts: vec![],
        inputs: vec![],
        mode,
    }
}

fn write_fixture_rule_set(root: &Path) {
    write_file(root.join("sgconfig.yml"), "ruleDirs:\n  - rules\n");
    write_file(
        root.join("rules/no-console-log.yml"),
        "id: no-console-log\nlanguage: TypeScript\nseverity: error\nmessage: \"No console.log allowed\"\nrule:\n  pattern: console.log($$$)\n",
    );
}

fn write_language_globs_fixture_rule_set(root: &Path) {
    write_file(
        root.join("sgconfig.yml"),
        "ruleDirs:\n  - rules\nlanguageGlobs:\n  tsx:\n    - '*.ts'\n",
    );
    write_file(
        root.join("rules/no-console-log.yml"),
        "id: no-console-log\nlanguage: tsx\nseverity: error\nmessage: \"No console.log allowed\"\nrule:\n  pattern: console.log($$$)\n",
    );
}

#[test]
fn resolve_prunes_when_no_sgconfig() {
    let fixture = tempdir().expect("tempdir");
    let input = format!(
        "{}\n",
        resolve_line(resolve_task_request(
            "resolve-no-config",
            Some(fixture.path()),
            ResolveMode::Run
        ))
    );
    let (output, stderr) = run_worker(&input);
    assert!(stderr.is_empty(), "unexpected worker stderr: {stderr}");
    let resolved = output
        .iter()
        .find(|value| value["type"].as_str() == Some("resolved"))
        .expect("resolved message");
    assert_eq!(resolved["id"].as_str(), Some("resolve-no-config"));
    assert_eq!(resolved["result"]["decision"].as_str(), Some("prune"));
    assert_eq!(
        resolved["result"]["reason"].as_str(),
        Some("no sgconfig.yml found; skipping ast-grep")
    );
}

#[test]
fn resolve_rejects_without_cwd() {
    let input = format!(
        "{}\n",
        resolve_line(resolve_task_request(
            "resolve-no-cwd",
            None,
            ResolveMode::Run
        ))
    );
    let (output, stderr) = run_worker(&input);
    assert!(stderr.is_empty(), "unexpected worker stderr: {stderr}");
    let resolved = output
        .iter()
        .find(|value| value["type"].as_str() == Some("resolved"))
        .expect("resolved message");
    assert_eq!(resolved["id"].as_str(), Some("resolve-no-cwd"));
    assert_eq!(resolved["result"]["decision"].as_str(), Some("reject"));
    assert_eq!(
        resolved["result"]["message"].as_str(),
        Some("ast-grep worker requires cwd")
    );
}

#[test]
fn resolve_prunes_when_rule_dirs_empty() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("sgconfig.yml"),
        "ruleDirs:\n  - rules\n",
    );
    let input = format!(
        "{}\n",
        resolve_line(resolve_task_request(
            "resolve-no-rules",
            Some(fixture.path()),
            ResolveMode::Run
        ))
    );
    let (output, stderr) = run_worker(&input);
    assert!(stderr.is_empty(), "unexpected worker stderr: {stderr}");
    let resolved = output
        .iter()
        .find(|value| value["type"].as_str() == Some("resolved"))
        .expect("resolved message");
    assert_eq!(resolved["id"].as_str(), Some("resolve-no-rules"));
    assert_eq!(resolved["result"]["decision"].as_str(), Some("prune"));
    assert_eq!(
        resolved["result"]["reason"].as_str(),
        Some("sgconfig.yml found but no rule files; skipping ast-grep")
    );
}

#[test]
fn resolve_modifies_inputs_when_rules_exist() {
    let fixture = tempdir().expect("tempdir");
    write_fixture_rule_set(fixture.path());
    let input = format!(
        "{}\n",
        resolve_line(resolve_task_request(
            "resolve-rules",
            Some(fixture.path()),
            ResolveMode::Run
        ))
    );
    let (output, stderr) = run_worker(&input);
    assert!(stderr.is_empty(), "unexpected worker stderr: {stderr}");
    let resolved = output
        .iter()
        .find(|value| value["type"].as_str() == Some("resolved"))
        .expect("resolved message");
    assert_eq!(resolved["id"].as_str(), Some("resolve-rules"));
    assert_eq!(resolved["result"]["decision"].as_str(), Some("modify"));
    assert_eq!(
        resolved["result"]["inputs"],
        serde_json::json!([
            "**/*",
            ".gitignore",
            "package.json",
            "rules/no-console-log.yml",
            "sgconfig.yml"
        ])
    );
}

#[test]
fn resolve_preserves_repo_root_hash_inputs_when_rules_exist() {
    let fixture = tempdir().expect("tempdir");
    write_fixture_rule_set(fixture.path());
    let mut request =
        resolve_task_request("resolve-rules-hash", Some(fixture.path()), ResolveMode::Run);
    request.inputs = vec![
        "#sgconfig.yml".to_owned(),
        "#etc/ast-grep/rules/**/*.yml".to_owned(),
        "src/**".to_owned(),
    ];
    let input = format!("{}\n", resolve_line(request));
    let (output, stderr) = run_worker(&input);
    assert!(stderr.is_empty(), "unexpected worker stderr: {stderr}");
    let resolved = output
        .iter()
        .find(|value| value["type"].as_str() == Some("resolved"))
        .expect("resolved message");
    assert_eq!(resolved["id"].as_str(), Some("resolve-rules-hash"));
    assert_eq!(resolved["result"]["decision"].as_str(), Some("modify"));
    assert_eq!(
        resolved["result"]["inputs"],
        serde_json::json!([
            "#etc/ast-grep/rules/**/*.yml",
            "#sgconfig.yml",
            "**/*",
            ".gitignore",
            "package.json",
            "rules/no-console-log.yml",
            "sgconfig.yml"
        ])
    );
}

#[test]
fn violation_emits_log_report_and_done_exit_1() {
    let fixture = tempdir().expect("tempdir");
    write_fixture_rule_set(fixture.path());
    write_file(fixture.path().join("src/index.ts"), "console.log('hi');\n");

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-violation", "lint")
                .with_cwd(fixture.path().display().to_string())
        )
    );
    let (output, _stderr) = run_worker_in(fixture.path(), &input);

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("log")
            && value["id"].as_str() == Some("job-violation")
            && value["line"].as_str().is_some_and(|msg| {
                msg.contains("src/index.ts:1:1: error [no-console-log] No console.log allowed")
            })
    }));

    let report = output
        .iter()
        .find(|value| value["type"].as_str() == Some("report"))
        .expect("sarif report present");
    assert_eq!(report["id"].as_str(), Some("job-violation"));
    assert_eq!(report["filename"].as_str(), Some("ast-grep.sarif"));
    assert_eq!(report["mimeType"].as_str(), Some("application/sarif+json"));
    let report_body = report["content"].as_str().expect("sarif content str");
    assert!(report_body.contains("\"ruleId\": \"no-console-log\""));
    assert!(report_body.contains("\"uri\": \"src/index.ts\""));

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-violation")
            && value["exitCode"].as_i64() == Some(1)
    }));
}

#[test]
fn language_globs_fixture_emits_violation_for_tsx_rule_on_ts_file() {
    let fixture = tempdir().expect("tempdir");
    write_language_globs_fixture_rule_set(fixture.path());
    write_file(fixture.path().join("src/index.ts"), "console.log('hi');\n");

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-language-globs", "lint")
                .with_cwd(fixture.path().display().to_string())
        )
    );
    let (output, _stderr) = run_worker_in(fixture.path(), &input);

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("log")
            && value["id"].as_str() == Some("job-language-globs")
            && value["line"].as_str().is_some_and(|msg| {
                msg.contains("src/index.ts:1:1: error [no-console-log] No console.log allowed")
            })
    }));
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-language-globs")
            && value["exitCode"].as_i64() == Some(1)
    }));
}

#[test]
fn severity_off_fixture_emits_no_log_report_or_failure() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("sgconfig.yml"),
        "ruleDirs:\n  - rules\n",
    );
    write_file(
        fixture.path().join("rules/no-console-log.yml"),
        "id: no-console-log\nlanguage: TypeScript\nseverity: off\nmessage: \"No console.log allowed\"\nrule:\n  pattern: console.log($$$)\n",
    );
    write_file(fixture.path().join("src/index.ts"), "console.log('hi');\n");

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-off", "lint").with_cwd(fixture.path().display().to_string())
        )
    );
    let (output, stderr) = run_worker(&input);

    assert!(stderr.is_empty(), "unexpected worker stderr: {stderr}");
    assert!(output
        .iter()
        .all(|value| value["type"].as_str() != Some("log")));
    assert!(output
        .iter()
        .all(|value| value["type"].as_str() != Some("report")));
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-off")
            && value["exitCode"].as_i64() == Some(0)
    }));
}

#[test]
fn unsupported_sgconfig_keys_emit_warning() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("sgconfig.yml"),
        "ruleDirs:\n  - rules\nutilDirs:\n  - utils\n",
    );
    write_file(
        fixture.path().join("rules/no-console-log.yml"),
        "id: no-console-log\n",
    );
    write_file(fixture.path().join("src/index.ts"), "const ok = 1;\n");

    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-warning", "lint")
                .with_cwd(fixture.path().display().to_string())
        )
    );
    let (_output, stderr) = run_worker_with_env(&input, &[("RUST_LOG", "")]);

    assert!(
        stderr.contains("ast-grep worker: 'utilDirs' not yet supported; ignoring"),
        "missing unsupported-key warning: {stderr}"
    );
}

#[test]
fn clean_fixture_emits_no_report_done_exit_0() {
    let fixture = tempdir().expect("tempdir");
    write_fixture_rule_set(fixture.path());
    write_file(fixture.path().join("src/index.ts"), "const ok = 1;\n");

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
fn fix_flag_in_command_rewrites_file_end_to_end() {
    let fixture = tempdir().expect("tempdir");
    write_file(
        fixture.path().join("sgconfig.yml"),
        "ruleDirs:\n  - rules\n",
    );
    write_file(
        fixture.path().join("rules/no-console-log.yml"),
        "id: no-console-log\nlanguage: TypeScript\nseverity: error\nmessage: \"No console.log allowed\"\nrule:\n  pattern: console.log($ARG)\nfix: logger.info($ARG)\n",
    );
    let source = fixture.path().join("src/index.ts");
    write_file(&source, "console.log('hi');\n");

    // `--fix` is passed in the task command string, mirroring how other
    // options are set (issue #226).
    let input = format!(
        "{}\n",
        run_line(
            WorkerRequest::new("job-fix", "lint --fix")
                .with_cwd(fixture.path().display().to_string())
        )
    );
    let (output, _stderr) = run_worker_in(fixture.path(), &input);

    // File is rewritten on disk by the fixer.
    assert_eq!(
        fs::read_to_string(&source).expect("read rewritten source"),
        "logger.info('hi');\n"
    );
    // Worker emits a `fixed:` log for the rewritten file, and after fixing the
    // violation is gone so the run succeeds (exit 0).
    assert_log_line_contains(&output, "job-fix", "fixed: src/index.ts");
    assert_done_with_exit(&output, "job-fix", 0);
}

fn assert_log_line_contains(output: &[Value], id: &str, substring: &str) {
    assert!(
        output.iter().any(|value| {
            value["type"].as_str() == Some("log")
                && value["id"].as_str() == Some(id)
                && value["line"]
                    .as_str()
                    .is_some_and(|msg| msg.contains(substring))
        }),
        "missing log line containing {substring:?} for {id}"
    );
}

fn assert_done_with_exit(output: &[Value], id: &str, exit_code: i64) {
    assert!(
        output.iter().any(|value| {
            value["type"].as_str() == Some("done")
                && value["id"].as_str() == Some(id)
                && value["exitCode"].as_i64() == Some(exit_code)
        }),
        "missing done(exit={exit_code}) for {id}"
    );
}
