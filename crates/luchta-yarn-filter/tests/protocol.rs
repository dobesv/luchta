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

fn resolve_task(
    id: &str,
    name: &str,
    command: &str,
    cwd: Option<&str>,
    scripts: &[&str],
) -> ResolveTask {
    ResolveTask {
        id: id.to_owned(),
        name: name.to_owned(),
        command: command.to_owned(),
        package: "@repo/app".to_owned(),
        cwd: cwd.map(str::to_owned),
        scripts: scripts.iter().map(|script| script.to_string()).collect(),
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
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    if printf '%s\n' "$line" | grep -q '"type":"resolveTask"'; then
        printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
    elif printf '%s\n' "$line" | grep -q '"type":"run"'; then
        printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
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
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    if printf '%s\n' "$line" | grep -q '"type":"run"'; then
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
    std::env::var_os("CARGO_BIN_EXE_luchta-yarn-filter")
        .map(PathBuf::from)
        .expect("cargo sets binary path for integration tests")
}

fn run_worker(
    temp: &TempDir,
    stage_args: &[&str],
    delegate_command: &[String],
    input: &str,
) -> (Vec<Value>, String) {
    let mut command = Command::new(worker_bin());
    command.current_dir(temp.path());
    command.args(stage_args);
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

fn write_package_json(dir: &Path, value: &Value) {
    fs::write(
        dir.join("package.json"),
        serde_json::to_vec_pretty(value).expect("package json bytes"),
    )
    .expect("write package.json");
}

#[test]
fn default_resolve_prunes_when_task_script_missing_without_spawning_delegate() {
    let (temp, sentinel) = sentinel_paths();
    let package_dir = temp.path().join("packages/app");
    fs::create_dir_all(&package_dir).expect("create package dir");

    let input = format!(
        "{}\n",
        resolve_line(resolve_task(
            "job-default-prune",
            "build",
            "",
            Some("packages/app"),
            &["test"],
        ))
    );
    let (output, _stderr) = run_worker(&temp, &[], &loopback_delegate_command(&sentinel), &input);

    assert!(!sentinel.exists(), "delegate must not spawn for prune");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"].as_str(), Some("resolved"));
    assert_eq!(output[0]["id"].as_str(), Some("job-default-prune"));
    assert_eq!(output[0]["result"]["decision"].as_str(), Some("prune"));
    assert!(output[0]["result"].get("reason").is_none());
}

#[test]
fn default_resolve_forwards_when_task_script_present() {
    let (temp, sentinel) = sentinel_paths();
    let package_dir = temp.path().join("packages/app");
    fs::create_dir_all(&package_dir).expect("create package dir");

    let input = format!(
        "{}\n",
        resolve_line(resolve_task(
            "job-default-accept",
            "build",
            "",
            Some("packages/app"),
            &["build", "test"],
        ))
    );
    let (output, _stderr) = run_worker(&temp, &[], &loopback_delegate_command(&sentinel), &input);

    assert!(
        sentinel.exists(),
        "delegate should spawn for matching resolve"
    );
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"].as_str(), Some("resolved"));
    assert_eq!(output[0]["id"].as_str(), Some("job-default-accept"));
    assert_eq!(output[0]["result"]["decision"].as_str(), Some("accept"));
}

#[test]
fn dependency_check_forwards_when_present_in_dependencies() {
    let (temp, sentinel) = sentinel_paths();
    let package_dir = temp.path().join("packages/app");
    fs::create_dir_all(&package_dir).expect("create package dir");
    write_package_json(
        &package_dir,
        &serde_json::json!({
            "dependencies": {
                "babel": "^1.0.0"
            }
        }),
    );

    let input = format!(
        "{}\n",
        resolve_line(resolve_task(
            "job-dep-dependencies",
            "build",
            "",
            Some("packages/app"),
            &[],
        ))
    );
    let (output, _stderr) = run_worker(
        &temp,
        &["--dependency", "babel"],
        &loopback_delegate_command(&sentinel),
        &input,
    );

    assert!(
        sentinel.exists(),
        "delegate should spawn when dependency matches"
    );
    assert_eq!(output[0]["result"]["decision"].as_str(), Some("accept"));
}

#[test]
fn dependency_check_forwards_when_present_in_dev_dependencies() {
    let (temp, sentinel) = sentinel_paths();
    let package_dir = temp.path().join("packages/app");
    fs::create_dir_all(&package_dir).expect("create package dir");
    write_package_json(
        &package_dir,
        &serde_json::json!({
            "devDependencies": {
                "babel": "^1.0.0"
            }
        }),
    );

    let input = format!(
        "{}\n",
        resolve_line(resolve_task(
            "job-dep-dev-dependencies",
            "build",
            "",
            Some("packages/app"),
            &[],
        ))
    );
    let (output, _stderr) = run_worker(
        &temp,
        &["--dependency", "babel"],
        &loopback_delegate_command(&sentinel),
        &input,
    );

    assert!(
        sentinel.exists(),
        "delegate should spawn when devDependency matches"
    );
    assert_eq!(output[0]["result"]["decision"].as_str(), Some("accept"));
}

#[test]
fn dependency_check_prunes_when_dependency_absent() {
    let (temp, sentinel) = sentinel_paths();
    let package_dir = temp.path().join("packages/app");
    fs::create_dir_all(&package_dir).expect("create package dir");
    write_package_json(
        &package_dir,
        &serde_json::json!({
            "dependencies": {
                "typescript": "^5.0.0"
            }
        }),
    );

    let input = format!(
        "{}\n",
        resolve_line(resolve_task(
            "job-dep-prune",
            "build",
            "",
            Some("packages/app"),
            &["test"],
        ))
    );
    let (output, _stderr) = run_worker(
        &temp,
        &["--dependency", "babel"],
        &loopback_delegate_command(&sentinel),
        &input,
    );

    assert!(
        !sentinel.exists(),
        "delegate must not spawn when dependency missing"
    );
    assert_eq!(output[0]["result"]["decision"].as_str(), Some("prune"));
}

#[test]
fn dependency_only_mode_skips_default_task_name_script_check() {
    let (temp, sentinel) = sentinel_paths();
    let package_dir = temp.path().join("packages/app");
    fs::create_dir_all(&package_dir).expect("create package dir");
    write_package_json(
        &package_dir,
        &serde_json::json!({
            "dependencies": {
                "babel": "^1.0.0"
            }
        }),
    );

    let input = format!(
        "{}\n",
        resolve_line(resolve_task(
            "job-dep-only-skips-script",
            "build",
            "",
            Some("packages/app"),
            &["test"],
        ))
    );
    let (output, _stderr) = run_worker(
        &temp,
        &["--dependency", "babel"],
        &loopback_delegate_command(&sentinel),
        &input,
    );

    assert!(
        sentinel.exists(),
        "delegate should spawn when dependency matches even if task script missing"
    );
    assert_eq!(output[0]["result"]["decision"].as_str(), Some("accept"));
}

#[test]
fn script_flag_checks_named_script_instead_of_task_name() {
    let (temp, sentinel) = sentinel_paths();
    let package_dir = temp.path().join("packages/app");
    fs::create_dir_all(&package_dir).expect("create package dir");

    let input = format!(
        "{}\n",
        resolve_line(resolve_task(
            "job-script-override",
            "test",
            "",
            Some("packages/app"),
            &["build"],
        ))
    );
    let (output, _stderr) = run_worker(
        &temp,
        &["--script", "build"],
        &loopback_delegate_command(&sentinel),
        &input,
    );

    assert!(
        sentinel.exists(),
        "delegate should spawn when override script matches"
    );
    assert_eq!(output[0]["result"]["decision"].as_str(), Some("accept"));
}

#[test]
fn run_forwards_to_delegate() {
    let (temp, sentinel) = sentinel_paths();
    let input = format!("{}\n", run_line(WorkerRequest::new("job-run", "build")));

    let (output, _stderr) = run_worker(&temp, &[], &done_delegate_command(&sentinel), &input);

    assert!(sentinel.exists(), "delegate should spawn for run");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"].as_str(), Some("done"));
    assert_eq!(output[0]["id"].as_str(), Some("job-run"));
    assert_eq!(output[0]["exitCode"].as_i64(), Some(0));
}

#[test]
fn dependency_check_uses_current_dir_when_cwd_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_package_json(
        temp.path(),
        &serde_json::json!({
            "dependencies": {
                "babel": "^1.0.0"
            }
        }),
    );
    let sentinel = temp.path().join("delegate-started");

    let mut command = Command::new(worker_bin());
    command
        .current_dir(temp.path())
        .arg("--dependency")
        .arg("babel")
        .arg("--")
        .args(loopback_delegate_command(&sentinel))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().expect("spawn worker");
    {
        let mut stdin = child.stdin.take().expect("stdin piped");
        writeln!(
            stdin,
            "{}",
            resolve_line(resolve_task("job-root", "build", "", None, &[]))
        )
        .expect("write request");
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
        "delegate should spawn for root-level dependency match"
    );
    assert_eq!(response["type"].as_str(), Some("resolved"));
    assert_eq!(response["id"].as_str(), Some("job-root"));
    assert_eq!(response["result"]["decision"].as_str(), Some("accept"));
}
