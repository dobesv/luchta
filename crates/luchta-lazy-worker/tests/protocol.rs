use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use assert_cmd::cargo::CommandCargoExt;
use luchta_worker::{ResolveMode, ResolveTask, WorkerMessage, WorkerRequest};
use serde_json::Value;
use tempfile::TempDir;

fn run_line(request: WorkerRequest) -> String {
    serde_json::to_string(&WorkerMessage::Run(request)).expect("request json")
}

fn resolve_line(resolve: ResolveTask) -> String {
    serde_json::to_string(&WorkerMessage::ResolveTask(resolve)).expect("resolve json")
}

fn resolve_task(id: &str) -> ResolveTask {
    ResolveTask {
        id: id.to_owned(),
        name: "build".to_owned(),
        command: "build".to_owned(),
        package: "@repo/app".to_owned(),
        cwd: Some("packages/app".to_owned()),
        scripts: vec!["build".to_owned()],
        mode: ResolveMode::Run,
    }
}

fn loopback_delegate_command(sentinel: &Path) -> Vec<String> {
    vec![
        "sh".to_owned(),
        "-c".to_owned(),
        format!(
            r#"touch '{}'
count_file='{}.count'
count=0
if [ -f "$count_file" ]; then
    count=$(cat "$count_file")
fi
count=$((count + 1))
printf '%s\n' "$count" > "$count_file"
while IFS= read -r line; do
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    command=$(printf '%s\n' "$line" | sed -n 's/.*"command":"\([^"]*\)".*/\1/p')
    case $line in
        *'"type":"run"'*)
            printf '{{"type":"log","id":"%s","stream":"stdout","line":"delegate saw %s"}}\n' "$id" "$command"
            if [ "$command" = 'fail' ]; then
                printf '{{"type":"done","id":"%s","exitCode":7}}\n' "$id"
            else
                printf '{{"type":"done","id":"%s","exitCode":0}}\n' "$id"
            fi
            ;;
        *)
            printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
            ;;
    esac
done
"#,
            shell_single_quote_path(sentinel),
            shell_single_quote_path(sentinel)
        ),
    ]
}

fn immediate_exit_delegate_command(sentinel: &Path) -> Vec<String> {
    vec![
        "sh".to_owned(),
        "-c".to_owned(),
        format!("touch '{}'; exit 9", shell_single_quote_path(sentinel)),
    ]
}

fn shell_single_quote_path(path: &Path) -> String {
    path.display().to_string().replace('\'', "'\\''")
}

fn run_worker(
    delegate_command: &[String],
    input: &str,
) -> (std::process::ExitStatus, Vec<Value>, String) {
    let mut command = Command::cargo_bin("luchta-lazy-worker").expect("binary exists");
    command
        .arg("--")
        .args(delegate_command)
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
    let status = output.status;
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let responses = stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid jsonl response"))
        .collect();
    let stderr = String::from_utf8(output.stderr).expect("stderr utf8");
    (status, responses, stderr)
}

fn read_count_file(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read count file")
}

fn sentinel_paths() -> (TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let sentinel = temp.path().join("delegate-started");
    let count = temp.path().join("delegate-started.count");
    (temp, sentinel, count)
}

#[test]
fn resolve_accepts_without_spawning_delegate() {
    let (_temp, sentinel, _count) = sentinel_paths();
    let input = format!("{}\n", resolve_line(resolve_task("job-resolve")));

    let (status, output, _stderr) = run_worker(&loopback_delegate_command(&sentinel), &input);
    assert!(status.success(), "worker failed");

    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"].as_str(), Some("resolved"));
    assert_eq!(output[0]["id"].as_str(), Some("job-resolve"));
    assert_eq!(output[0]["result"]["decision"].as_str(), Some("accept"));
    assert!(
        !sentinel.exists(),
        "delegate should not start on resolve-only input"
    );
}

#[test]
fn first_run_spawns_delegate_and_second_run_reuses_it() {
    let (_temp, sentinel, count) = sentinel_paths();
    let mut input = Vec::new();
    writeln!(input, "{}", run_line(WorkerRequest::new("job-1", "build"))).expect("write run");
    writeln!(input, "{}", run_line(WorkerRequest::new("job-2", "test"))).expect("write run");

    let (status, output, _stderr) = run_worker(
        &loopback_delegate_command(&sentinel),
        &String::from_utf8(input).expect("stdin utf8"),
    );
    assert!(status.success(), "worker failed");

    assert!(sentinel.exists(), "delegate should start on first run");
    assert_eq!(read_count_file(&count).trim(), "1");
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-1")
            && value["exitCode"].as_i64() == Some(0)
    }));
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-2")
            && value["exitCode"].as_i64() == Some(0)
    }));
}

#[test]
fn propagates_delegate_logs_and_exit_codes() {
    let (_temp, sentinel, _count) = sentinel_paths();
    let input = format!("{}\n", run_line(WorkerRequest::new("job-fail", "fail")));

    let (status, output, _stderr) = run_worker(&loopback_delegate_command(&sentinel), &input);
    assert!(status.success(), "worker failed");

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("log")
            && value["id"].as_str() == Some("job-fail")
            && value["stream"].as_str() == Some("stdout")
            && value["line"].as_str() == Some("delegate saw fail")
    }));
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-fail")
            && value["exitCode"].as_i64() == Some(7)
    }));
}

#[test]
fn delegate_failure_on_first_run_exits_nonzero_and_reports_stderr() {
    let (_temp, sentinel, _count) = sentinel_paths();
    let input = format!("{}\n", run_line(WorkerRequest::new("job-dead", "build")));

    let (status, output, stderr) = run_worker(&immediate_exit_delegate_command(&sentinel), &input);

    assert!(
        sentinel.exists(),
        "failing delegate should still have started"
    );
    assert!(!status.success(), "worker unexpectedly succeeded");
    assert!(
        stderr.contains("delegate failed"),
        "stderr missing delegate failure: {stderr}"
    );
    assert!(
        output.iter().all(|value| {
            !(value["type"].as_str() == Some("done") && value["id"].as_str() == Some("job-dead"))
        }),
        "unexpected terminal done emitted: {output:?}"
    );
}
