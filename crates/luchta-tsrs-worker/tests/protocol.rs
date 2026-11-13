use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use luchta_tsrs_worker::compile::compile_package;
use serde_json::{json, Value};

const CONFIG: &str = r#"{
    "compilerOptions": {
        "outDir": "dist", "rootDir": "src", "declaration": true,
        "module": "nodenext", "moduleResolution": "nodenext"
    },
    "include": ["src/**/*"]
}"#;

fn package(repo: &Path, name: &str, source: &str, config: &str) -> PathBuf {
    let cwd = repo.join("packages").join(name);
    fs::create_dir_all(cwd.join("src")).unwrap();
    fs::write(cwd.join("src/index.ts"), source).unwrap();
    fs::write(cwd.join("tsconfig.json"), config).unwrap();
    cwd
}

fn run_request(id: &str, cwd: &Path) -> Value {
    json!({
        "type": "run", "id": id, "cwd": cwd,
        // Go ignores command and env. A shell fallback must fail this test.
        "command": "this-command-must-never-be-spawned",
        "env": {"TS_NODE_TRANSPILE_ONLY": "true"},
        "inputs": ["declared/**"], "outputs": ["not-an-emitted-file/**"]
    })
}

fn serve_raw(repo: &Path, input: &[u8]) -> Output {
    serve_with_stdout(repo, input, Stdio::piped())
}

fn serve_with_stdout(repo: &Path, input: &[u8], stdout: Stdio) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_luchta-tsrs-worker"))
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn serve(repo: &Path, requests: &[Value]) -> Vec<Value> {
    let mut input = String::new();
    for request in requests {
        input.push_str(&request.to_string());
        input.push('\n');
    }
    let output = serve_raw(repo, input.as_bytes());
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_done(messages: &[Value], id: &str, exit_code: i32, outputs: &[&str]) {
    let done: Vec<_> = messages
        .iter()
        .filter(|msg| msg["type"] == "done")
        .collect();
    let mut expected = json!({"type": "done", "id": id, "exitCode": exit_code});
    if !outputs.is_empty() {
        expected["outputs"] = json!(outputs);
    }
    // Exact shape also checks camelCase, no inputs, and empty outputs omitted.
    assert_eq!(done, [&expected]);
}

fn assert_resolved(messages: &[Value], id: &str) {
    let resolved: Vec<_> = messages
        .iter()
        .filter(|msg| msg["type"] == "resolved")
        .collect();
    assert_eq!(
        resolved,
        [&json!({
            "type": "resolved", "id": id,
            "result": {
                "decision": "modify",
                "inputs": ["declared/**", "src/**/*", "tsconfig.build.json", "tsconfig.json"],
                "toolVersion": env!("LUCHTA_TOOL_VERSION")
            }
        })]
    );
}

fn assert_type_error_report(messages: &[Value], id: &str, uri: &str) {
    let report = &messages[messages.len() - 2];
    assert_eq!(report["type"], "report");
    assert_eq!(report["id"], id);
    assert_eq!(report["filename"], "tsc.sarif");
    assert_eq!(report["mimeType"], "application/sarif+json");
    let sarif: Value = serde_json::from_str(report["content"].as_str().unwrap()).unwrap();
    assert_eq!(sarif["version"], "2.1.0");
    let findings = sarif["runs"][0]["results"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    for finding in findings {
        assert_eq!(finding["ruleId"], "TS2322");
        assert_eq!(
            finding["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            uri
        );
    }
}

// Go worker_test: TestServeRunEmitsLogAndDone (despite its name, success is silent).
#[test]
fn serve_run_emits_done_and_outputs_without_logs_or_report() {
    let repo = tempfile::tempdir().unwrap();
    let cwd = package(repo.path(), "pkg", "export const x = 1;\n", CONFIG);
    let messages = serve(repo.path(), &[run_request("t1", &cwd)]);
    assert_eq!(messages.len(), 1);
    assert_done(&messages, "t1", 0, &["dist/index.d.ts", "dist/index.js"]);
    assert!(cwd.join("dist/index.js").is_file());
    assert!(cwd.join("dist/index.d.ts").is_file());
}

#[test]
fn e2e_resolve_compile_clean_and_type_error_in_run_and_check_modes() {
    for mode in ["run", "check"] {
        let repo = tempfile::tempdir().unwrap();
        let mut requests = Vec::new();
        for (name, source) in [
            ("clean", "export const x: number = 1;\n"),
            ("error", "export const x: number = 'wrong';\n"),
        ] {
            let cwd = package(repo.path(), name, source, CONFIG);
            requests.push(json!({
                "type": "resolveTask", "id": name, "name": "build:types",
                "command": "", "package": name, "cwd": cwd,
                "scripts": [], "mode": mode, "inputs": ["declared/**"]
            }));
            requests.push(run_request(name, &cwd));
        }
        let messages = serve(repo.path(), &requests);
        assert_e2e_package(repo.path(), &messages, "clean", 0);
        assert_e2e_package(repo.path(), &messages, "error", 1);
    }
}

fn assert_e2e_package(repo: &Path, messages: &[Value], name: &str, exit_code: i32) {
    let messages: Vec<_> = messages
        .iter()
        .filter(|msg| msg["id"] == name)
        .cloned()
        .collect();
    assert_resolved(&messages, name);
    assert_done(
        &messages,
        name,
        exit_code,
        &["dist/index.d.ts", "dist/index.js"],
    );
    let execution: Vec<_> = messages
        .iter()
        .filter(|msg| msg["type"] != "resolved")
        .cloned()
        .collect();
    if exit_code == 1 {
        assert_eq!(execution.len(), 2);
        assert_type_error_report(&execution, name, &format!("packages/{name}/src/index.ts"));
    } else {
        assert_eq!(execution.len(), 1);
    }
    let cwd = repo.join("packages").join(name);
    assert!(cwd.join("dist/index.js").is_file());
    assert!(cwd.join("dist/index.d.ts").is_file());
}

#[tokio::test]
async fn serve_internal_error_matches_compile_stderr_without_report() {
    let repo = tempfile::tempdir().unwrap();
    let cwd = package(
        repo.path(),
        "pkg",
        "export const x = 1;\n",
        r#"{
        "compilerOptions": {"outDir": "../../outside", "declaration": true}
    }"#,
    );
    let result = compile_package(&cwd, repo.path()).await;
    assert_eq!(result.exit_code, 1);
    assert!(result.findings.is_empty());
    assert_eq!(result.stderr.len(), 1);
    let messages = serve(repo.path(), &[run_request("t1", &cwd)]);
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0],
        json!({
            "type": "log", "id": "t1", "stream": "stderr", "line": result.stderr[0]
        })
    );
    assert_done(&messages, "t1", 1, &[]);
    assert!(!repo.path().join("outside").exists());
}

#[test]
fn serve_no_config_is_success_without_empty_report_or_outputs() {
    let repo = tempfile::tempdir().unwrap();
    let messages = serve(repo.path(), &[run_request("t1", repo.path())]);
    assert_eq!(messages.len(), 1);
    assert_done(&messages, "t1", 0, &[]);
}

#[test]
fn serve_missing_cwd_fails_without_shell_fallback() {
    let repo = tempfile::tempdir().unwrap();
    for cwd in [Value::Null, json!("")] {
        let messages = serve(
            repo.path(),
            &[json!({
                "type": "run", "id": "t1", "command": "exit 0", "cwd": cwd
            })],
        );
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0],
            json!({
                "type": "log", "id": "t1", "stream": "stderr",
                "line": "luchta-tsrs-worker requires cwd"
            })
        );
        assert_done(&messages, "t1", 1, &[]);
    }
}

#[test]
fn serve_no_emit_on_error_reports_diagnostics_without_outputs() {
    let repo = tempfile::tempdir().unwrap();
    let config = CONFIG.replace(
        "\"declaration\": true",
        "\"declaration\": true, \"noEmitOnError\": true",
    );
    let cwd = package(
        repo.path(),
        "pkg",
        "export const x: number = 'wrong';\n",
        &config,
    );
    let messages = serve(repo.path(), &[run_request("t1", &cwd)]);
    assert_eq!(messages.len(), 2);
    assert_type_error_report(&messages, "t1", "packages/pkg/src/index.ts");
    assert_done(&messages, "t1", 1, &[]);
    assert!(!cwd.join("dist").exists());
}

// Go worker_test: TestServeMalformedLineGoesToStderr. Shared Rust runtime
// fails the process instead of continuing; stdout still stays protocol-only.
#[test]
fn serve_malformed_line_goes_to_stderr_only() {
    let repo = tempfile::tempdir().unwrap();
    let output = serve_raw(repo.path(), b"not json\n");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .starts_with("worker error: "));
}

#[test]
fn serve_eof_without_requests_is_success() {
    let repo = tempfile::tempdir().unwrap();
    assert!(serve(repo.path(), &[]).is_empty());
}

// Go worker_test: TestServeReturnsProtocolWriteError. The shared Rust runtime
// logs failed job writes, but unlike Go does not fail the worker process.
#[cfg(target_os = "linux")]
#[test]
fn serve_protocol_write_failure_is_reported_on_process_stderr() {
    let repo = tempfile::tempdir().unwrap();
    let request = run_request("t1", repo.path()).to_string() + "\n";
    let stdout = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let output = serve_with_stdout(repo.path(), request.as_bytes(), Stdio::from(stdout));
    assert!(
        output.status.success(),
        "shared runtime exit policy changed: {output:?}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("job failed:"), "{stderr}");
    assert!(stderr.contains("No space left on device"), "{stderr}");
}
