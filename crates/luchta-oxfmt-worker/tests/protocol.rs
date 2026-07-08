use std::collections::HashMap;
use std::process::Stdio;

use assert_fs::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[tokio::test]
async fn write_mode_rewrites_unformatted_fixture() {
    let temp = TempDir::new().expect("tempdir");
    let cwd = temp.path();
    tokio::fs::create_dir_all(cwd.join("src"))
        .await
        .expect("src dir");
    let file = cwd.join("src/index.ts");
    let original = "export const value={foo:'bar'}\n";
    tokio::fs::write(&file, original).await.expect("fixture");

    let response = run_worker_jsonl(cwd, HashMap::new()).await;

    assert_eq!(response.exit_code, 0);
    assert_eq!(response.stdout_lines, vec!["reformatted: src/index.ts"]);
    assert!(
        response.stderr_lines.is_empty(),
        "stderr: {:?}",
        response.stderr_lines
    );
    let rewritten = tokio::fs::read_to_string(&file).await.expect("rewritten");
    assert_ne!(rewritten, original);
}

#[tokio::test]
async fn check_mode_reports_nonzero_without_writing() {
    let temp = TempDir::new().expect("tempdir");
    let cwd = temp.path();
    tokio::fs::create_dir_all(cwd.join("src"))
        .await
        .expect("src dir");
    let file = cwd.join("src/index.ts");
    let original = "export const value={foo:'bar'}\n";
    tokio::fs::write(&file, original).await.expect("fixture");

    let mut env = HashMap::new();
    env.insert("OXFMT_OPTS".to_owned(), "--check".to_owned());
    let response = run_worker_jsonl(cwd, env).await;

    assert_eq!(response.exit_code, 1);
    assert_eq!(response.stdout_lines, vec!["would reformat: src/index.ts"]);
    assert!(
        response.stderr_lines.is_empty(),
        "stderr: {:?}",
        response.stderr_lines
    );
    let after = tokio::fs::read_to_string(&file).await.expect("after");
    assert_eq!(after, original);
}

#[tokio::test]
async fn formatted_fixture_is_noop_in_both_modes() {
    let temp = TempDir::new().expect("tempdir");
    let cwd = temp.path();
    tokio::fs::create_dir_all(cwd.join("src"))
        .await
        .expect("src dir");
    let file = cwd.join("src/index.ts");
    let formatted = "export const value = { foo: \"bar\" };\n";
    tokio::fs::write(&file, formatted).await.expect("fixture");

    let write_response = run_worker_jsonl(cwd, HashMap::new()).await;
    assert_eq!(write_response.exit_code, 0);
    assert!(
        write_response.stdout_lines.is_empty(),
        "stdout: {:?}",
        write_response.stdout_lines
    );
    assert!(
        write_response.stderr_lines.is_empty(),
        "stderr: {:?}",
        write_response.stderr_lines
    );
    assert_eq!(
        tokio::fs::read_to_string(&file).await.expect("after write"),
        formatted
    );

    let mut env = HashMap::new();
    env.insert("OXFMT_OPTS".to_owned(), "--check".to_owned());
    let check_response = run_worker_jsonl(cwd, env).await;
    assert_eq!(check_response.exit_code, 0);
    assert!(
        check_response.stdout_lines.is_empty(),
        "stdout: {:?}",
        check_response.stdout_lines
    );
    assert!(
        check_response.stderr_lines.is_empty(),
        "stderr: {:?}",
        check_response.stderr_lines
    );
    assert_eq!(
        tokio::fs::read_to_string(&file).await.expect("after check"),
        formatted
    );
}

struct WorkerRun {
    exit_code: i32,
    stdout_lines: Vec<String>,
    stderr_lines: Vec<String>,
}

async fn run_worker_jsonl(cwd: &std::path::Path, env: HashMap<String, String>) -> WorkerRun {
    let mut child = Command::new(env!("CARGO_BIN_EXE_luchta-oxfmt-worker"))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn worker");

    let request = serde_json::json!({
        "type": "run",
        "id": "pkg#format",
        "command": "format",
        "cwd": cwd.to_string_lossy(),
        "env": env,
    });
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .await
        .expect("write request");
    drop(stdin);

    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();
    let mut stdout_lines = Vec::new();
    let mut stderr_lines = Vec::new();
    let mut exit_code = None;

    while let Some(line) = lines.next_line().await.expect("read line") {
        let value: serde_json::Value = serde_json::from_str(&line).expect("json response");
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("log") => {
                let stream = value
                    .get("stream")
                    .and_then(serde_json::Value::as_str)
                    .expect("stream");
                let line = value
                    .get("line")
                    .and_then(serde_json::Value::as_str)
                    .expect("line")
                    .to_owned();
                match stream {
                    "stdout" => stdout_lines.push(line),
                    "stderr" => stderr_lines.push(line),
                    other => panic!("unexpected stream {other}"),
                }
            }
            Some("done") => {
                exit_code = Some(
                    value
                        .get("exitCode")
                        .and_then(serde_json::Value::as_i64)
                        .expect("exit code") as i32,
                );
            }
            other => panic!("unexpected response type {other:?}"),
        }
    }

    let status = child.wait().await.expect("wait child");
    assert!(status.success(), "worker process failed: {status}");

    WorkerRun {
        exit_code: exit_code.expect("done response"),
        stdout_lines,
        stderr_lines,
    }
}
