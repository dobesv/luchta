use std::io::Write;
use std::process::{Command, Stdio};

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;

fn resolve_message(id: &str) -> String {
    format!(
        r#"{{"type":"resolveTask","id":"{id}","name":"build","command":"echo hi","package":"@repo/app","cwd":null,"scripts":["build"],"mode":"run"}}"#
    )
}

fn run_message(id: &str) -> String {
    format!(
        r#"{{"type":"run","id":"{id}","name":"build","command":"echo hi","package":"@repo/app","cwd":null,"env":{{}},"shell":"/bin/sh"}}"#
    )
}

fn done_response(id: &str) -> String {
    format!(r#"{{"type":"done","id":"{id}","exitCode":0}}"#)
}

fn run_filter(
    predicate: &[&str],
    delegate_script: &str,
    input_lines: &[String],
) -> std::process::Output {
    let mut argv = predicate
        .iter()
        .map(|token| token.to_string())
        .collect::<Vec<_>>();
    argv.push("--".to_owned());
    argv.push("sh".to_owned());
    argv.push("-c".to_owned());
    argv.push(delegate_script.to_owned());

    let mut child = Command::new(cargo_bin("luchta-command-filter"))
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn filter");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for line in input_lines {
            writeln!(stdin, "{line}").expect("write input line");
        }
    }

    child.wait_with_output().expect("wait for filter")
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    let text = String::from_utf8(stdout.to_vec()).expect("utf8 stdout");
    text.lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid jsonl line"))
        .collect()
}

#[test]
fn resolve_forwards_when_predicate_exits_zero() {
    let output = run_filter(
        &["/bin/true"],
        r#"while IFS= read -r line; do
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '%s\n' '{"type":"resolved","id":"resolve-accept","result":{"decision":"accept"}}'
      ;;
  esac
done
"#,
        &[resolve_message("resolve-accept")],
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = parse_jsonl(&output.stdout);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["type"], "resolved");
    assert_eq!(responses[0]["id"], "resolve-accept");
    assert_eq!(responses[0]["result"]["decision"], "accept");
}

#[test]
fn resolve_prunes_when_predicate_exits_nonzero_without_spawning_delegate() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sentinel_path = temp.path().join("delegate-spawned");
    let delegate_script = format!(
        "printf spawned > '{}'\nwhile IFS= read -r _line; do :; done\n",
        sentinel_path.display()
    );

    let mut child = Command::new(cargo_bin("luchta-command-filter"))
        .args(["sh", "-c", "exit 1", "--", "sh", "-c", &delegate_script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn filter");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{}", resolve_message("resolve-prune")).expect("write resolve");
    }
    let output = child.wait_with_output().expect("wait");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !sentinel_path.exists(),
        "delegate should not spawn on prune"
    );
    let responses = parse_jsonl(&output.stdout);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["type"], "resolved");
    assert_eq!(responses[0]["id"], "resolve-prune");
    assert_eq!(responses[0]["result"]["decision"], "prune");
}

#[test]
fn predicate_output_never_contaminates_protocol_stdout() {
    let output = run_filter(
        &[
            "sh",
            "-c",
            "i=0; while [ $i -lt 20 ]; do echo CONTAMINATION-$i; echo CONTAMINATION-$i 1>&2; i=$((i+1)); done; exit 0",
        ],
        r#"while IFS= read -r _line; do
  printf '%s\n' '{"type":"resolved","id":"resolve-clean","result":{"decision":"accept"}}'
done
"#,
        &[resolve_message("resolve-clean")],
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout_text = String::from_utf8(output.stdout.clone()).expect("utf8 stdout");
    assert!(
        !stdout_text.contains("CONTAMINATION"),
        "stdout leaked predicate output: {stdout_text}"
    );
    let responses = parse_jsonl(&output.stdout);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["type"], "resolved");
}

#[test]
fn run_forwards_done_from_delegate() {
    let expected = done_response("run-done");
    let delegate_script = format!(
        "while IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"run\"'*)\n      printf '%s\\n' '{}'\n      ;;\n  esac\ndone\n",
        expected
    );
    let output = run_filter(
        &["sh", "-c", "exit 0"],
        &delegate_script,
        &[run_message("run-done")],
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = parse_jsonl(&output.stdout);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["type"], "done");
    assert_eq!(responses[0]["id"], "run-done");
    assert_eq!(responses[0]["exitCode"], 0);
}
