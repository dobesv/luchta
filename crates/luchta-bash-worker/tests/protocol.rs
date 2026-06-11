//! Integration tests driving the `luchta-bash-worker` binary over the JSONL
//! protocol.
//!
//! These live in `tests/` (not an inline `#[cfg(test)] mod tests` in `main.rs`)
//! so that `CARGO_BIN_EXE_luchta-bash-worker` is set and `cargo nextest`/`cargo
//! test` build the binary before running them.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use assert_cmd::cargo::CommandCargoExt;
use luchta_engine::{ResolveMode, ResolveTask, WorkerMessage, WorkerRequest};
use serde_json::Value;

/// Serializes an execution request as a `run` worker message JSONL line.
fn run_line(request: WorkerRequest) -> String {
    serde_json::to_string(&WorkerMessage::Run(request)).expect("request json")
}

fn resolve_line(resolve: ResolveTask) -> String {
    serde_json::to_string(&WorkerMessage::ResolveTask(resolve)).expect("resolve json")
}

fn run_worker(input: &str) -> (Vec<Value>, String) {
    let mut command = Command::cargo_bin("luchta-bash-worker").expect("binary exists");
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

fn resolve_task(id: &str, name: &str, command: &str, mode: ResolveMode) -> ResolveTask {
    ResolveTask {
        id: id.to_owned(),
        name: name.to_owned(),
        command: command.to_owned(),
        package: "@repo/app".to_owned(),
        cwd: Some("packages/app".to_owned()),
        scripts: vec!["ignored".to_owned()],
        mode,
    }
}

#[test]
fn echo_request_emits_log_and_done() {
    let input = format!("{}\n", run_line(WorkerRequest::new("job-1", "echo hi")));
    let (output, _stderr) = run_worker(&input);

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("log")
            && value["id"].as_str() == Some("job-1")
            && value["line"]
                .as_str()
                .is_some_and(|line| line.contains("hi"))
    }));
    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-1")
            && value["exitCode"].as_i64() == Some(0)
    }));
}

#[test]
fn failing_request_preserves_exit_code() {
    let input = format!("{}\n", run_line(WorkerRequest::new("job-2", "exit 3")));
    let (output, _stderr) = run_worker(&input);

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-2")
            && value["exitCode"].as_i64() == Some(3)
    }));
}

#[test]
fn invalid_cwd_still_emits_done_with_non_zero_exit_code() {
    let request = WorkerRequest::new("job-bad-cwd", "echo hi").with_cwd(
        Path::new("/definitely/missing/luchta-worker-cwd-xyz")
            .display()
            .to_string(),
    );
    let input = format!("{}\n", run_line(request));
    let (output, _stderr) = run_worker(&input);

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("done")
            && value["id"].as_str() == Some("job-bad-cwd")
            && value["exitCode"].as_i64() == Some(1)
    }));
}

#[test]
fn blank_command_in_run_mode_prunes() {
    let input = format!(
        "{}\n",
        resolve_line(resolve_task(
            "job-resolve-run",
            "build",
            "   ",
            ResolveMode::Run
        ))
    );
    let (output, _stderr) = run_worker(&input);

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("resolved")
            && value["id"].as_str() == Some("job-resolve-run")
            && value["result"]["decision"].as_str() == Some("prune")
    }));
}

#[test]
fn blank_command_in_check_mode_rejects() {
    let input = format!(
        "{}\n",
        resolve_line(resolve_task(
            "job-resolve-check",
            "build",
            "",
            ResolveMode::Check
        ))
    );
    let (output, _stderr) = run_worker(&input);

    assert!(output.iter().any(|value| {
        value["type"].as_str() == Some("resolved")
            && value["id"].as_str() == Some("job-resolve-check")
            && value["result"]["decision"].as_str() == Some("reject")
    }));
}

#[test]
fn concurrent_requests_emit_done_for_both_ids() {
    let requests = [
        WorkerRequest::new("job-a", "sleep 0.1; echo first"),
        WorkerRequest::new("job-b", "echo second"),
    ];
    let mut input = Vec::new();
    for request in requests {
        writeln!(input, "{}", run_line(request)).expect("write request");
    }

    let (output, _stderr) = run_worker(&String::from_utf8(input).expect("stdin utf8"));
    let done_ids: Vec<&str> = output
        .iter()
        .filter(|value| value["type"].as_str() == Some("done"))
        .filter_map(|value| value["id"].as_str())
        .collect();

    assert!(done_ids.contains(&"job-a"));
    assert!(done_ids.contains(&"job-b"));
}
