//! Integration tests driving the `luchta-yarn-worker` binary over the JSONL
//! protocol.
//!
//! These live in `tests/` (not an inline `#[cfg(test)] mod tests` in `main.rs`)
//! so that `CARGO_BIN_EXE_luchta-yarn-worker` is set and `cargo nextest`/`cargo
//! test` build the binary before running them.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use assert_cmd::cargo::CommandCargoExt;
use luchta_engine::{WorkerMessage, WorkerRequest};
use serde_json::Value;

/// Serializes an execution request as a `run` worker message JSONL line.
fn run_line(request: WorkerRequest) -> String {
    serde_json::to_string(&WorkerMessage::Run(request)).expect("request json")
}

fn run_worker(input: &str) -> (Vec<Value>, String) {
    let mut command = Command::cargo_bin("luchta-yarn-worker").expect("binary exists");
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
fn done_response_preserves_declared_outputs_only() {
    let request = WorkerRequest::new("job-io", "echo hi")
        .with_inputs(["src/**/*.ts"])
        .with_outputs(["dist/**"]);
    let input = format!("{}\n", run_line(request));
    let (output, _stderr) = run_worker(&input);

    let done = output
        .iter()
        .find(|value| {
            value["type"].as_str() == Some("done") && value["id"].as_str() == Some("job-io")
        })
        .expect("done response present");
    assert!(
        done.get("inputs").is_none(),
        "done should not include inputs: {done}"
    );
    assert_eq!(
        done["outputs"],
        Value::Array(vec![Value::String("dist/**".to_owned())])
    );
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
