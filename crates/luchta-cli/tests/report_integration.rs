//! End-to-end integration tests for worker report pipeline.
//!
//! Exercises the full path: worker emits report → engine collects → cache persists → CLI renders.
//! These are the ONLY exercisers of the report pipeline in this repo (no real report emitters).

use assert_cmd::Command;
use assert_fs::prelude::*;

mod common;

// ============================================================================
// TEST FIXTURES - minimal valid SARIF, CTRF, and plain-text reports
// ============================================================================

/// Minimal SARIF 2.1.0 with one result containing uri:line:col for IDE clickability.
/// Note: Using compact JSON to avoid shell escaping issues in worker scripts.
const SARIF_FIXTURE: &str = r#"{"version":"2.1.0","$schema":"https://json.schemastore.org/sarif-2.1.0.json","runs":[{"tool":{"driver":{"name":"test-linter","version":"1.0.0"}},"results":[{"level":"error","message":{"text":"Unused variable in function"},"locations":[{"physicalLocation":{"artifactLocation":{"uri":"src/lib.rs"},"region":{"startLine":42,"startColumn":8}}}]}]}]}"#;

/// Minimal CTRF with one passed and one failed test.
/// Note: Using compact JSON to avoid shell escaping issues in worker scripts.
const CTRF_FIXTURE: &str = r#"{"results":{"tool":{"name":"test-runner"},"summary":{"tests":2,"passed":1,"failed":1,"pending":0,"skipped":0,"start":0,"stop":100},"tests":[{"name":"add numbers","status":"passed","duration":15},{"name":"subtract numbers","status":"failed","message":"expected 5 but got 3","trace":"at src/math.rs:22"}]}}"#;

/// Plain text report for unknown-MIME verbatim test.
const PLAIN_TEXT_FIXTURE: &str = "Build Summary:\n- Compiled 5 files\n- 0 warnings\n- 0 errors\n";

// ============================================================================
// HELPER: Shell worker that emits report(s) before done
// ============================================================================

/// Write a simple task config shell worker that uses the given worker script.
fn write_report_task_config(
    temp: &assert_fs::TempDir,
    worker_command: &std::path::Path,
    task_json: &str,
) {
    common::write_executable(
        temp.child("luchta-config.sh").path(),
        &format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"shell\":{{\"command\":\"{}\"}}}},\"tasks\":{{{}}}}}'\n",
            worker_command.display(),
            task_json
        ),
    );
}

/// Run luchta and return stdout as String.
fn run_luchta_get_stdout(temp: &assert_fs::TempDir, args: &[&str]) -> String {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.args(args);
    cmd.arg("--workspace-root").arg(temp.path());
    let output = cmd.assert().success();
    String::from_utf8_lossy(&output.get_output().stdout).to_string()
}

/// Helper to set up a minimal workspace with a single package for report e2e tests.
fn setup_report_workspace(temp: &assert_fs::TempDir) {
    temp.child("package.json")
        .write_str(r#"{"name": "root", "private": true, "workspaces": ["packages/*"]}"#)
        .unwrap();
    temp.child("packages/a").create_dir_all().unwrap();
    temp.child("packages/a/package.json")
        .write_str(r#"{"name": "a", "scripts": {"build": "echo build", "test": "echo test", "lint": "echo lint", "check": "echo check", "task1": "echo task1"}}"#)
        .unwrap();
    temp.child("packages/a/src.txt").write_str("src\n").unwrap();
}

/// Helper to set up a minimal workspace with TWO packages for multi-task report e2e tests.
fn setup_two_package_report_workspace(temp: &assert_fs::TempDir) {
    temp.child("package.json")
        .write_str(r#"{"name": "root", "private": true, "workspaces": ["packages/*"]}"#)
        .unwrap();
    temp.child("packages/a").create_dir_all().unwrap();
    temp.child("packages/a/package.json")
        .write_str(r#"{"name": "a", "scripts": {"build": "echo build-a"}}"#)
        .unwrap();
    temp.child("packages/a/src.txt").write_str("src\n").unwrap();
    temp.child("packages/b").create_dir_all().unwrap();
    temp.child("packages/b/package.json")
        .write_str(r#"{"name": "b", "scripts": {"build": "echo build-b"}}"#)
        .unwrap();
    temp.child("packages/b/src.txt").write_str("src\n").unwrap();
}

// ============================================================================
// CACHE TESTS - V1 migration, byte-equality, filename safety, duplicate last-wins
// ============================================================================

#[test]
fn cache_write_read_report_byte_exact() {
    use luchta_cache::{
        Cache, ReportInput, RunArtifacts, TaskRunRecord, CACHE_DIR_NAME, LUCHTA_DIR_NAME,
    };
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    let temp_dir = tempdir().unwrap();
    let cache = Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();

    // Create record with reports
    let record = TaskRunRecord {
        schema_version: 2,
        task_spec_hash: [1u8; 32],
        input_patterns: vec!["src/**/*.ts".to_string()],
        inputs: vec![],
        output_patterns: vec!["dist/**/*.js".to_string()],
        outputs: vec![],
        detected_input_patterns: false,
        detected_output_patterns: false,
        outputs_hash: [2u8; 32],
        env_hash: [3u8; 32],
        pkg_dep_hash: [4u8; 32],
        dep_outputs: BTreeMap::new(),
        exit_status: 0,
        succeeded: true,
        start_unix_ms: 1000,
        end_unix_ms: 2000,
        reports: vec![
            luchta_cache::ReportMeta {
                filename: "sarif.json".to_string(),
                mime_type: "application/sarif+json".to_string(),
            },
            luchta_cache::ReportMeta {
                filename: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
            },
        ],
    };

    // Content with specific bytes including trailing newline
    let sarif_content = "{\"version\":\"2.1.0\"}\n";
    let txt_content = "Hello\nWorld\n";

    let reports = vec![
        ReportInput {
            filename: "sarif.json".to_string(),
            mime_type: "application/sarif+json".to_string(),
            content: sarif_content.to_string(),
        },
        ReportInput {
            filename: "report.txt".to_string(),
            mime_type: "text/plain".to_string(),
            content: txt_content.to_string(),
        },
    ];

    cache
        .write(
            "test#task",
            RunArtifacts {
                record: &record,
                stdout: b"stdout",
                stderr: b"stderr",
                reports: &reports,
            },
        )
        .unwrap();

    // Read back and verify byte-exact
    let read_back = cache.read("test#task").expect("record should exist");
    assert_eq!(read_back.reports.len(), 2);
    assert_eq!(read_back.reports[0].filename, "sarif.json");

    // Byte-exact content verification
    let sarif_bytes = cache
        .read_report("test#task", "sarif.json")
        .expect("sarif report");
    let txt_bytes = cache
        .read_report("test#task", "report.txt")
        .expect("txt report");

    assert_eq!(
        sarif_bytes,
        sarif_content.as_bytes(),
        "sarif content must be byte-exact"
    );
    assert_eq!(
        txt_bytes,
        txt_content.as_bytes(),
        "txt content must be byte-exact"
    );
}

#[test]
fn cache_v1_schema_returns_none() {
    // V1 schema (without reports field) should return None (cache miss)
    // Use the cache module's inline test approach - write corrupted bytes
    use luchta_cache::{Cache, CACHE_DIR_NAME, LUCHTA_DIR_NAME};
    use std::fs;
    use tempfile::tempdir;

    let temp_dir = tempdir().unwrap();
    let cache = Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();
    let task_dir = cache.task_dir("pkg#build");
    fs::create_dir_all(&task_dir).unwrap();

    // Write plain text that will fail bincode decode - this simulates V1/any decode error
    fs::write(task_dir.join("meta.bincode"), b"not-valid-bincode").unwrap();

    // Reading corrupted/V1 should return None (no panic)
    assert!(
        cache.read("pkg#build").is_none(),
        "V1/corrupted schema should return None"
    );
}

#[test]
fn cache_duplicate_filename_last_wins() {
    use luchta_cache::{
        Cache, ReportInput, RunArtifacts, TaskRunRecord, CACHE_DIR_NAME, LUCHTA_DIR_NAME,
    };
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    let temp_dir = tempdir().unwrap();
    let cache = Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();

    let record = TaskRunRecord {
        schema_version: 2,
        task_spec_hash: [1u8; 32],
        input_patterns: vec![],
        inputs: vec![],
        output_patterns: vec![],
        outputs: vec![],
        detected_input_patterns: false,
        detected_output_patterns: false,
        outputs_hash: [0u8; 32],
        env_hash: [0u8; 32],
        pkg_dep_hash: [0u8; 32],
        dep_outputs: BTreeMap::new(),
        exit_status: 0,
        succeeded: true,
        start_unix_ms: 0,
        end_unix_ms: 100,
        reports: vec![luchta_cache::ReportMeta {
            filename: "report.json".to_string(),
            mime_type: "application/json".to_string(),
        }],
    };

    // Two reports with same filename - last should win
    let reports = vec![
        ReportInput {
            filename: "report.json".to_string(),
            mime_type: "application/json".to_string(),
            content: "{\"first\":true}".to_string(),
        },
        ReportInput {
            filename: "report.json".to_string(),
            mime_type: "application/json".to_string(),
            content: "{\"second\":true}".to_string(),
        },
    ];

    cache
        .write(
            "test#dup",
            RunArtifacts {
                record: &record,
                stdout: b"",
                stderr: b"",
                reports: &reports,
            },
        )
        .unwrap();

    // Should have only the last content
    let bytes = cache
        .read_report("test#dup", "report.json")
        .expect("report should exist");
    assert_eq!(
        bytes, b"{\"second\":true}",
        "duplicate filename should keep last content"
    );
}

#[test]
fn cache_rejects_traversal_filename() {
    use luchta_cache::{
        Cache, CacheError, ReportInput, RunArtifacts, TaskRunRecord, CACHE_DIR_NAME,
        LUCHTA_DIR_NAME,
    };
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    let temp_dir = tempdir().unwrap();
    let cache = Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();

    let record = TaskRunRecord {
        schema_version: 2,
        task_spec_hash: [1u8; 32],
        input_patterns: vec![],
        inputs: vec![],
        output_patterns: vec![],
        outputs: vec![],
        detected_input_patterns: false,
        detected_output_patterns: false,
        outputs_hash: [0u8; 32],
        env_hash: [0u8; 32],
        pkg_dep_hash: [0u8; 32],
        dep_outputs: BTreeMap::new(),
        exit_status: 0,
        succeeded: true,
        start_unix_ms: 0,
        end_unix_ms: 0,
        reports: vec![],
    };

    // Try to write with path traversal filename
    let reports = vec![ReportInput {
        filename: "../escape.txt".to_string(),
        mime_type: "text/plain".to_string(),
        content: "escaped".to_string(),
    }];

    let result = cache.write(
        "test#traversal",
        RunArtifacts {
            record: &record,
            stdout: b"",
            stderr: b"",
            reports: &reports,
        },
    );

    assert!(result.is_err(), "traversal filename should be rejected");
    let err = result.unwrap_err();
    assert!(
        matches!(err, CacheError::InputExpansion(_)),
        "should be InputExpansion error"
    );

    // Verify file was NOT created outside cache
    assert!(
        !temp_dir.path().join("escape.txt").exists(),
        "file should not escape cache"
    );
}

#[test]
fn cache_rejects_reserved_filename() {
    use luchta_cache::{
        Cache, ReportInput, RunArtifacts, TaskRunRecord, CACHE_DIR_NAME, LUCHTA_DIR_NAME,
    };
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    let temp_dir = tempdir().unwrap();
    let cache = Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();

    let record = TaskRunRecord {
        schema_version: 2,
        task_spec_hash: [1u8; 32],
        input_patterns: vec![],
        inputs: vec![],
        output_patterns: vec![],
        outputs: vec![],
        detected_input_patterns: false,
        detected_output_patterns: false,
        outputs_hash: [0u8; 32],
        env_hash: [0u8; 32],
        pkg_dep_hash: [0u8; 32],
        dep_outputs: BTreeMap::new(),
        exit_status: 0,
        succeeded: true,
        start_unix_ms: 0,
        end_unix_ms: 0,
        reports: vec![],
    };

    // Try to write with reserved filename
    for reserved_name in ["stdout.log", "stderr.log", "meta.bincode"] {
        let reports = vec![ReportInput {
            filename: reserved_name.to_string(),
            mime_type: "text/plain".to_string(),
            content: "reserved".to_string(),
        }];

        let result = cache.write(
            "test#reserved",
            RunArtifacts {
                record: &record,
                stdout: b"",
                stderr: b"",
                reports: &reports,
            },
        );

        assert!(
            result.is_err(),
            "reserved filename {} should be rejected",
            reserved_name
        );
    }
}

// ============================================================================
// E2E CLI TESTS - SARIF/CTRF pretty, --file raw, no-match error
// ============================================================================

#[test]
fn e2e_sarif_pretty_prints_clickable_location() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_report_workspace(&temp);

    // Create worker that emits SARIF report
    let worker = common::shell_worker_with_reports(
        &temp,
        &[("sarif.json", "application/sarif+json", SARIF_FIXTURE)],
    );

    write_report_task_config(
        &temp,
        worker.path(),
        r#""a#lint":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"echo running"}"#,
    );

    common::init_git(&temp);

    // Run the task
    common::run_luchta(&temp, "lint").success();

    // Check logs output contains clickable path:line:col
    let stdout = run_luchta_get_stdout(&temp, &["logs"]);

    // Should contain the message
    assert!(
        stdout.contains("Unused variable"),
        "should contain SARIF message, got: {}",
        stdout
    );

    // Should contain clickable location (without color since NO_COLOR=1)
    assert!(
        stdout.contains("src/lib.rs:42:8"),
        "should contain clickable location, got: {}",
        stdout
    );
}

#[test]
fn e2e_ctrf_pretty_prints_failed_test() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_report_workspace(&temp);

    let worker = common::shell_worker_with_reports(
        &temp,
        &[("ctrf.json", "application/vnd.ctrf+json", CTRF_FIXTURE)],
    );

    write_report_task_config(
        &temp,
        worker.path(),
        r#""a#test":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"echo running"}"#,
    );

    common::init_git(&temp);

    common::run_luchta(&temp, "test").success();

    let stdout = run_luchta_get_stdout(&temp, &["logs"]);

    // Should contain summary counts
    assert!(
        stdout.contains("1 passed"),
        "should contain passed count, got: {}",
        stdout
    );
    assert!(
        stdout.contains("1 failed"),
        "should contain failed count, got: {}",
        stdout
    );

    // Should contain failed test name and message
    assert!(
        stdout.contains("subtract numbers"),
        "should contain failed test name, got: {}",
        stdout
    );
    assert!(
        stdout.contains("expected 5 but got 3"),
        "should contain failure message, got: {}",
        stdout
    );
}

#[test]
fn e2e_unknown_mime_dumps_verbatim() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_report_workspace(&temp);

    let worker = common::shell_worker_with_reports(
        &temp,
        &[("build.txt", "text/plain", PLAIN_TEXT_FIXTURE)],
    );

    write_report_task_config(
        &temp,
        worker.path(),
        r#""a#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"echo running"}"#,
    );

    common::init_git(&temp);

    common::run_luchta(&temp, "build").success();

    let stdout = run_luchta_get_stdout(&temp, &["logs"]);

    // Should dump content verbatim
    assert!(
        stdout.contains("Build Summary:"),
        "should contain verbatim content, got: {}",
        stdout
    );
    assert!(
        stdout.contains("Compiled 5 files"),
        "should contain verbatim content, got: {}",
        stdout
    );
}

#[test]
fn e2e_file_flag_raw_passthrough_byte_exact() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_report_workspace(&temp);

    let worker = common::shell_worker_with_reports(
        &temp,
        &[("results.sarif", "application/sarif+json", SARIF_FIXTURE)],
    );

    write_report_task_config(
        &temp,
        worker.path(),
        r#""a#lint":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"echo running"}"#,
    );

    common::init_git(&temp);

    common::run_luchta(&temp, "lint").success();

    // Request raw file via --file
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("logs");
    cmd.arg("--file").arg("results.sarif");
    cmd.arg("--workspace-root").arg(temp.path());

    let output = cmd.assert().success();
    let stdout_bytes = output.get_output().stdout.clone();

    // Should be byte-exact with the fixture
    assert_eq!(
        stdout_bytes,
        SARIF_FIXTURE.as_bytes(),
        "--file should dump exact bytes without pretty-print"
    );
}

#[test]
fn e2e_file_union_across_tasks() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_report_workspace(&temp);

    let worker = common::shell_worker_with_reports(
        &temp,
        &[("report-a.json", "application/json", r#"{"task":"a"}"#)],
    );

    write_report_task_config(
        &temp,
        worker.path(),
        r#""a#task1":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"echo a"}"#,
    );

    common::init_git(&temp);

    // Run task
    common::run_luchta(&temp, "task1").success();

    // Request report-a.json (task has it)
    let stdout = run_luchta_get_stdout(&temp, &["logs", "--file", "report-a.json"]);
    assert!(
        stdout.contains(r#"{"task":"a"}"#),
        "should contain report content for task a"
    );
}

#[test]
fn e2e_file_no_match_error() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_report_workspace(&temp);

    let worker =
        common::shell_worker_with_reports(&temp, &[("existing.json", "application/json", "{}")]);

    write_report_task_config(
        &temp,
        worker.path(),
        r#""a#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"echo running"}"#,
    );

    common::init_git(&temp);

    common::run_luchta(&temp, "build").success();

    // Request non-existent file - should fail with error message
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("logs");
    cmd.arg("--file").arg("missing.json");
    cmd.arg("--workspace-root").arg(temp.path());

    let output = cmd.assert().failure();
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);

    assert!(
        stderr.contains("No task matching") || stderr.contains("requested files"),
        "should have clear error message, got: {}",
        stderr
    );
}

#[test]
fn e2e_multiple_reports_one_task() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_report_workspace(&temp);

    // Emit both SARIF and CTRF for same task
    let worker = common::shell_worker_with_reports(
        &temp,
        &[
            ("lint.sarif", "application/sarif+json", SARIF_FIXTURE),
            ("test.ctrf", "application/vnd.ctrf+json", CTRF_FIXTURE),
        ],
    );

    write_report_task_config(
        &temp,
        worker.path(),
        r#""a#check":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"echo running"}"#,
    );

    common::init_git(&temp);

    common::run_luchta(&temp, "check").success();

    let stdout = run_luchta_get_stdout(&temp, &["logs"]);

    // Should contain output from both reports
    assert!(
        stdout.contains("src/lib.rs:42:8"),
        "should contain SARIF location, got: {}",
        stdout
    );
    assert!(
        stdout.contains("1 passed, 1 failed, 0 skipped"),
        "should contain CTRF summary, got: {}",
        stdout
    );
    assert!(
        stdout.contains("subtract numbers"),
        "should contain CTRF failed test, got: {}",
        stdout
    );
}

/// Create a shell worker script that emits reports, with a unique script/template name.
fn shell_worker_with_reports_named(
    temp: &assert_fs::TempDir,
    script_name: &str,
    reports: &[(&str, &str, &str)],
) -> assert_fs::fixture::ChildPath {
    const RUN_ID_TOKEN: &str = "@@LUCHTA_RUN_ID@@";

    let script = temp.child(script_name);
    let templates_path = temp.child(format!("{}.tmpl", script_name));

    let template_lines = reports
        .iter()
        .map(|(filename, mime_type, content)| {
            serde_json::to_string(&luchta_worker::WorkerResponse::report(
                RUN_ID_TOKEN,
                *filename,
                *mime_type,
                *content,
            ))
            .expect("serialize report template")
        })
        .collect::<Vec<_>>()
        .join("\n");
    templates_path
        .write_str(&format!("{template_lines}\n"))
        .expect("write report templates");

    let script_body = format!(
        r#"#!/bin/sh
reports_tmpl='{reports_tmpl}'
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
      ;;
    *'"type":"run"'*)
      cmd=$(printf '%s\n' "$line" | sed -n 's/.*"command":"\([^"]*\)".*/\1/p' | sed 's/\\\\/"/g; s/\\\\\\\\/\\/g')
      cwd=$(printf '%s\n' "$line" | sed -n 's/.*"cwd":"\([^"]*\)".*/\1/p' | sed 's/\\\\/"/g; s/\\\\\\\\/\\/g')
      (cd "$cwd" && sh -lc "$cmd") >/dev/null
      code=$?
      while IFS= read -r report_line; do
        [ -n "$report_line" ] || continue
        printf '%s\n' "$report_line" | sed 's|{run_id_token}|'"$id"'|g'
      done < "$reports_tmpl"
      printf '{{"type":"done","id":"%s","exitCode":%s}}\n' "$id" "$code"
      ;;
  esac
done
"#,
        reports_tmpl = templates_path.path().display(),
        run_id_token = RUN_ID_TOKEN,
    );
    script
        .write_str(&script_body)
        .expect("write report worker script");
    common::set_executable(script.path());
    script
}

#[test]
fn e2e_file_multi_task_same_filename_errors_ambiguous() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_two_package_report_workspace(&temp);

    // Two workers, each emitting the same filename but with distinct content
    // Use named helpers to avoid overwriting shared script/template files
    let worker_a = shell_worker_with_reports_named(
        &temp,
        "report-worker-a.sh",
        &[("results.json", "application/json", r#"{"task":"a"}"#)],
    );
    let worker_b = shell_worker_with_reports_named(
        &temp,
        "report-worker-b.sh",
        &[("results.json", "application/json", r#"{"task":"b"}"#)],
    );

    // Configure two tasks in different packages using different workers
    common::write_executable(
        temp.child("luchta-config.sh").path(),
        &format!(
            r#"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"shell-a":{{"command":"{}"}},"shell-b":{{"command":"{}"}}}},"tasks":{{"a#build":{{"cache":{{}},"worker":"shell-a","inputs":["src.txt"],"outputs":[],"command":"echo a"}},"b#build":{{"cache":{{}},"worker":"shell-b","inputs":["src.txt"],"outputs":[],"command":"echo b"}}}}}}'
"#,
            worker_a.path().display(),
            worker_b.path().display()
        ),
    );

    common::init_git(&temp);

    // Run both tasks (run all build tasks)
    common::run_luchta(&temp, "build").success();

    // Request results.json -- should ERROR because it's ambiguous across a#build and b#build
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("logs");
    cmd.arg("--file").arg("results.json");
    cmd.arg("--workspace-root").arg(temp.path());

    let output = cmd.assert().failure();
    let stderr = String::from_utf8_lossy(&output.get_output().stderr).to_string();

    assert!(
        stderr.contains("ambiguous: it was found on multiple tasks"),
        "stderr should contain ambiguous error message, got: {}",
        stderr
    );
    assert!(
        stderr.contains("a#build"),
        "stderr should contain task id 'a#build', got: {}",
        stderr
    );
    assert!(
        stderr.contains("b#build"),
        "stderr should contain task id 'b#build', got: {}",
        stderr
    );
}
