use std::collections::HashMap;
use std::process::Stdio;

use assert_fs::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Creates a temp package dir with `src/<name>` containing `source`. Returns the
/// TempDir (kept alive by the caller) so tests share fixture setup.
async fn ts_package(name: &str, source: &str) -> (TempDir, std::path::PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let cwd = temp.path();
    tokio::fs::create_dir_all(cwd.join("src"))
        .await
        .expect("src dir");
    let file = cwd.join("src").join(name);
    tokio::fs::write(&file, source).await.expect("fixture");
    (temp, file)
}

#[tokio::test]
async fn write_mode_rewrites_unformatted_fixture() {
    let original = "export const value={foo:'bar'}\n";
    let (temp, file) = ts_package("index.ts", original).await;

    let response = run_worker_jsonl(temp.path(), HashMap::new()).await;

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
async fn sort_imports_from_oxfmtrc_reorders_imports_end_to_end() {
    // Regression for issue #242: the worker must honor `.oxfmtrc` `sortImports`.
    // Spawns the real worker binary, which discovers the config from disk and
    // applies it, then confirms imports are reordered in the written file.
    let (temp, file) = ts_package(
        "index.ts",
        "import z from \"z\";\nimport a from \"a\";\n\nexport { z, a };\n",
    )
    .await;
    tokio::fs::write(temp.path().join(".oxfmtrc.json"), r#"{"sortImports":true}"#)
        .await
        .expect("config");

    let response = run_worker_jsonl(temp.path(), HashMap::new()).await;

    assert_eq!(response.exit_code, 0);
    assert!(
        response.stderr_lines.is_empty(),
        "stderr: {:?}",
        response.stderr_lines
    );
    let rewritten = tokio::fs::read_to_string(&file).await.expect("rewritten");
    assert_eq!(
        rewritten, "import a from \"a\";\nimport z from \"z\";\n\nexport { z, a };\n",
        "sortImports from .oxfmtrc should reorder imports end-to-end"
    );
}

#[tokio::test]
async fn unsupported_oxfmtrc_key_still_runs_and_warns() {
    // Regression: an `.oxfmtrc` with an unrecognized key (here `plugins`) must
    // NOT prevent the oxfmt task from running. The worker formats normally and
    // emits an informational notice on stderr.
    let (temp, file) = ts_package("index.ts", "export const value={foo:'bar'}\n").await;
    tokio::fs::write(temp.path().join(".oxfmtrc.json"), r#"{"plugins":["x"]}"#)
        .await
        .expect("config");

    let response = run_worker_jsonl(temp.path(), HashMap::new()).await;

    // Task ran and reformatted (exit 0), and the unsupported key surfaced as a
    // stderr notice rather than aborting.
    assert_eq!(response.exit_code, 0);
    assert!(
        response
            .stderr_lines
            .iter()
            .any(|line| line.contains("unsupported .oxfmtrc option `plugins`")),
        "expected unsupported-key notice on stderr, got: {:?}",
        response.stderr_lines
    );
    let rewritten = tokio::fs::read_to_string(&file).await.expect("rewritten");
    assert_ne!(rewritten, "export const value={foo:'bar'}\n");
}

#[tokio::test]
async fn malformed_oxfmtrc_fails_the_task_at_execution() {
    // Regression (#242 follow-up): a genuinely broken `.oxfmtrc` must FAIL the
    // task at execution (non-zero exit + error on stderr), NOT be pruned/hidden
    // at resolution as "nothing to run".
    let (temp, _file) = ts_package("index.ts", "export const value={foo:'bar'}\n").await;
    tokio::fs::write(temp.path().join(".oxfmtrc.json"), "{not valid json")
        .await
        .expect("config");

    let response = run_worker_jsonl(temp.path(), HashMap::new()).await;

    assert_eq!(response.exit_code, 1, "malformed config must fail the task");
    assert!(
        response
            .stderr_lines
            .iter()
            .any(|line| line.contains("failed to parse .oxfmtrc")),
        "expected a config parse error on stderr, got: {:?}",
        response.stderr_lines
    );
}

#[tokio::test]
async fn check_mode_reports_nonzero_without_writing() {
    let original = "export const value={foo:'bar'}\n";
    let (temp, file) = ts_package("index.ts", original).await;

    let mut env = HashMap::new();
    env.insert("OXFMT_OPTS".to_owned(), "--check".to_owned());
    let response = run_worker_jsonl(temp.path(), env).await;

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

/// Guards the wiring in `run_in_process` that computes `repo_root` from the
/// worker process's own cwd (mirroring the engine spawning workers with
/// cwd = workspace root) and threads it into `format_file` separately from
/// `req.cwd`. If a future change reverted the `format_file` closure at
/// worker.rs to use `&cwd` instead of `&repo_root`, the emitted path would
/// regress to `src/x.ts` instead of the repo-root-relative
/// `packages/app/src/x.ts`, and this test would catch it.
#[tokio::test]
async fn check_mode_reports_repo_root_relative_path_for_sub_package_cwd() {
    let temp = TempDir::new().expect("tempdir");
    let repo_root = temp.path();
    let package_cwd = repo_root.join("packages/app");
    tokio::fs::create_dir_all(package_cwd.join("src"))
        .await
        .expect("src dir");
    let file = package_cwd.join("src/x.ts");
    let original = "export const value={foo:'bar'}\n";
    tokio::fs::write(&file, original).await.expect("fixture");

    let mut env = HashMap::new();
    env.insert("OXFMT_OPTS".to_owned(), "--check".to_owned());
    let response = run_worker_jsonl_in(repo_root, &package_cwd, env).await;

    assert_eq!(response.exit_code, 1);
    assert_eq!(
        response.stdout_lines,
        vec!["would reformat: packages/app/src/x.ts"]
    );
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
    run_worker_jsonl_in(cwd, cwd, env).await
}

/// Runs the worker with the spawned process's `current_dir` set to
/// `repo_root` while the `WorkerRequest.cwd` is set to `req_cwd`, mirroring
/// the engine spawning workers with cwd = workspace root while dispatching
/// tasks against a package subdirectory. This lets tests exercise the
/// `repo_root != cwd` case that `run_worker_jsonl` collapses.
/// Spawns the worker binary (with `current_dir = repo_root`), sends a single
/// JSONL request, drains all response lines, and asserts the process exits
/// cleanly. Shared by the run and resolve harnesses.
async fn send_request(
    repo_root: &std::path::Path,
    request: serde_json::Value,
) -> Vec<serde_json::Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_luchta-oxfmt-worker"))
        .current_dir(repo_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn worker");

    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .await
        .expect("write request");
    drop(stdin);

    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();
    let mut responses = Vec::new();
    while let Some(line) = lines.next_line().await.expect("read line") {
        responses.push(serde_json::from_str(&line).expect("json response"));
    }

    let status = child.wait().await.expect("wait child");
    assert!(status.success(), "worker process failed: {status}");
    responses
}

async fn run_worker_jsonl_in(
    repo_root: &std::path::Path,
    req_cwd: &std::path::Path,
    env: HashMap<String, String>,
) -> WorkerRun {
    let request = serde_json::json!({
        "type": "run",
        "id": "pkg#format",
        "command": "format",
        "cwd": req_cwd.to_string_lossy(),
        "env": env,
    });

    let mut stdout_lines = Vec::new();
    let mut stderr_lines = Vec::new();
    let mut exit_code = None;

    for value in send_request(repo_root, request).await {
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

    WorkerRun {
        exit_code: exit_code.expect("done response"),
        stdout_lines,
        stderr_lines,
    }
}

/// Drives a `resolveTask` request through the real worker binary and returns the
/// resolve decision name (e.g. `"modify"`, `"prune"`, `"reject"`).
async fn resolve_decision(cwd: &std::path::Path) -> String {
    let request = serde_json::json!({
        "type": "resolveTask",
        "id": "pkg#oxfmt",
        "name": "oxfmt",
        "package": "pkg",
        "cwd": cwd.to_string_lossy(),
    });

    send_request(cwd, request)
        .await
        .iter()
        .find(|value| value.get("type").and_then(serde_json::Value::as_str) == Some("resolved"))
        .and_then(|value| value["result"]["decision"].as_str())
        .expect("resolved response")
        .to_owned()
}

/// Regression (#242 follow-up): resolution must NOT prune/reject a task over
/// config problems — that silently hides the task ("nothing to run"). Config
/// errors are deferred to execution (which fails the task). The only legitimate
/// resolve-time prune is a genuine absence of JS/TS sources.
#[tokio::test]
async fn resolve_does_not_prune_on_config_problems() {
    // Unsupported `.oxfmtrc` key with sources present -> keep the task (modify).
    let (temp, _file) = ts_package("index.ts", "export const x = 1;\n").await;
    tokio::fs::write(temp.path().join(".oxfmtrc.json"), r#"{"plugins":["x"]}"#)
        .await
        .expect("config");
    assert_eq!(
        resolve_decision(temp.path()).await,
        "modify",
        "unsupported .oxfmtrc key must not prune/reject the task"
    );

    // Malformed `.oxfmtrc` -> still keep the task (execution reports the error).
    tokio::fs::write(temp.path().join(".oxfmtrc.json"), "{not valid json")
        .await
        .expect("config");
    assert_eq!(
        resolve_decision(temp.path()).await,
        "modify",
        "malformed config must defer to execution, not prune"
    );
}

/// The one legitimate resolve-time prune: no JS/TS sources to format.
#[tokio::test]
async fn resolve_prunes_when_no_source_files() {
    let temp = TempDir::new().expect("tempdir");
    tokio::fs::create_dir_all(temp.path().join("src"))
        .await
        .expect("src dir");
    assert_eq!(resolve_decision(temp.path()).await, "prune");
}
