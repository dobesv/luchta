use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use luchta_worker::{ResolveMode, ResolveTask, WorkerMessage, WorkerRequest};
use serde_json::Value;
use tempfile::TempDir;

fn run_line(request: WorkerRequest) -> String {
    serde_json::to_string(&WorkerMessage::Run(request)).expect("request json")
}

fn resolve_line(resolve: ResolveTask) -> String {
    serde_json::to_string(&WorkerMessage::ResolveTask(resolve)).expect("resolve json")
}

fn resolve_task(id: &str, cwd: Option<&str>) -> ResolveTask {
    ResolveTask {
        id: id.to_owned(),
        name: "build".to_owned(),
        command: "build".to_owned(),
        package: "@repo/app".to_owned(),
        cwd: cwd.map(str::to_owned),
        scripts: vec!["build".to_owned()],
        inputs: Vec::new(),
        mode: ResolveMode::Run,
    }
}

fn loopback_delegate_command(sentinel: &Path) -> Vec<String> {
    vec![
        "sh".to_owned(),
        "-c".to_owned(),
        format!(
            r#"touch '{sentinel}'
while IFS= read -r line; do
    type=$(printf '%s\n' "$line" | sed -n 's/.*"type":"\([^"]*\)".*/\1/p')
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    if [ "$type" = "resolveTask" ]; then
        printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
    elif [ "$type" = "run" ]; then
        command=$(printf '%s\n' "$line" | sed -n 's/.*"command":"\([^"]*\)".*/\1/p')
        printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
        printf '%s\n' "$command" > /dev/null
    fi
done
"#,
            sentinel = shell_single_quote_path(sentinel)
        ),
    ]
}

fn done_delegate_command(sentinel: &Path) -> Vec<String> {
    vec![
        "sh".to_owned(),
        "-c".to_owned(),
        format!(
            r#"touch '{sentinel}'
while IFS= read -r line; do
    type=$(printf '%s\n' "$line" | sed -n 's/.*"type":"\([^"]*\)".*/\1/p')
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    if [ "$type" = "run" ]; then
        printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
    fi
done
"#,
            sentinel = shell_single_quote_path(sentinel)
        ),
    ]
}

fn shell_single_quote_path(path: &Path) -> String {
    path.display().to_string().replace('\'', "'\\''")
}

fn worker_bin() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_luchta-file-exists-filter")
        .map(PathBuf::from)
        .expect("cargo sets binary path for integration tests")
}

fn run_worker(
    temp: &TempDir,
    patterns: &[&str],
    delegate_command: &[String],
    input: &str,
) -> (Vec<Value>, String) {
    let mut command = Command::new(worker_bin());
    command.current_dir(temp.path());
    command.args(patterns);
    command.arg("--");
    command.args(delegate_command);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().expect("spawn worker");
    {
        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin
            .write_all(input.as_bytes())
            .expect("write requests to stdin");
    }

    let output = child.wait_with_output().expect("wait for worker output");
    assert!(
        output.status.success(),
        "worker failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let responses = stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid jsonl response"))
        .collect();
    let stderr = String::from_utf8(output.stderr).expect("stderr utf8");
    (responses, stderr)
}

fn sentinel_paths() -> (TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let sentinel = temp.path().join("delegate-started");
    (temp, sentinel)
}

#[test]
fn resolve_forwards_when_pattern_present() {
    let (temp, sentinel) = sentinel_paths();
    let package_dir = temp.path().join("packages/app");
    fs::create_dir_all(&package_dir).expect("create package dir");
    fs::write(
        package_dir.join("babel.config.js"),
        "module.exports = {};\n",
    )
    .expect("write file");

    let input = format!(
        "{}\n",
        resolve_line(resolve_task("job-resolve", Some("packages/app")))
    );
    let (output, _stderr) = run_worker(
        &temp,
        &["babel.config.*"],
        &loopback_delegate_command(&sentinel),
        &input,
    );

    assert!(
        sentinel.exists(),
        "delegate should spawn for matching resolve"
    );
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"].as_str(), Some("resolved"));
    assert_eq!(output[0]["id"].as_str(), Some("job-resolve"));
    assert_eq!(output[0]["result"]["decision"].as_str(), Some("accept"));
}

#[test]
fn resolve_prunes_when_pattern_absent_without_spawning_delegate() {
    let (temp, sentinel) = sentinel_paths();
    let package_dir = temp.path().join("packages/app");
    fs::create_dir_all(&package_dir).expect("create package dir");

    let input = format!(
        "{}\n",
        resolve_line(resolve_task("job-prune", Some("packages/app")))
    );
    let (output, _stderr) = run_worker(
        &temp,
        &["babel.config.*"],
        &loopback_delegate_command(&sentinel),
        &input,
    );

    assert!(!sentinel.exists(), "delegate must not spawn for prune");
    assert_eq!(output.len(), 1, "worker must emit single prune response");
    assert_eq!(output[0]["type"].as_str(), Some("resolved"));
    assert_eq!(output[0]["id"].as_str(), Some("job-prune"));
    assert_eq!(output[0]["result"]["decision"].as_str(), Some("prune"));
    assert!(output[0]["result"].get("reason").is_none());
}

#[test]
fn resolve_forwards_when_any_pattern_matches() {
    let (temp, sentinel) = sentinel_paths();
    let package_dir = temp.path().join("packages/app/src");
    fs::create_dir_all(&package_dir).expect("create package dir");
    fs::write(package_dir.join("index.ts"), "export {};\n").expect("write file");

    let input = format!(
        "{}\n",
        resolve_line(resolve_task("job-any", Some("packages/app")))
    );
    let (output, _stderr) = run_worker(
        &temp,
        &["babel.config.*", "src/**/*.ts"],
        &loopback_delegate_command(&sentinel),
        &input,
    );

    assert!(
        sentinel.exists(),
        "delegate should spawn when any pattern matches"
    );
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"].as_str(), Some("resolved"));
    assert_eq!(output[0]["id"].as_str(), Some("job-any"));
    assert_eq!(output[0]["result"]["decision"].as_str(), Some("accept"));
}

#[test]
fn run_forwards_to_delegate() {
    let (temp, sentinel) = sentinel_paths();
    let input = format!("{}\n", run_line(WorkerRequest::new("job-run", "build")));

    let (output, _stderr) = run_worker(
        &temp,
        &["anything"],
        &done_delegate_command(&sentinel),
        &input,
    );

    assert!(sentinel.exists(), "delegate should spawn for run");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"].as_str(), Some("done"));
    assert_eq!(output[0]["id"].as_str(), Some("job-run"));
    assert_eq!(output[0]["exitCode"].as_i64(), Some(0));
}

#[test]
fn resolve_uses_current_dir_when_cwd_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::write(temp.path().join("workspace.config"), "present\n").expect("write file");
    let sentinel = temp.path().join("delegate-started");

    let mut command = Command::new(worker_bin());
    command
        .current_dir(temp.path())
        .arg("workspace.config")
        .arg("--")
        .args(loopback_delegate_command(&sentinel))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().expect("spawn worker");
    {
        let mut stdin = child.stdin.take().expect("stdin piped");
        writeln!(stdin, "{}", resolve_line(resolve_task("job-root", None))).expect("write request");
    }

    let stdout = child.stdout.take().expect("stdout piped");
    let mut stdout = BufReader::new(stdout);
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read response line");

    let status = child.wait().expect("wait child");
    drop(child);

    let mut rest = String::new();
    stdout
        .read_to_string(&mut rest)
        .expect("read trailing stdout");

    assert!(status.success(), "worker failed");
    assert!(rest.is_empty(), "unexpected extra stdout: {rest:?}");
    let response: Value = serde_json::from_str(line.trim_end()).expect("valid json");
    assert!(
        sentinel.exists(),
        "delegate should spawn for root-level match"
    );
    assert_eq!(response["type"].as_str(), Some("resolved"));
    assert_eq!(response["id"].as_str(), Some("job-root"));
    assert_eq!(response["result"]["decision"].as_str(), Some("accept"));
}
