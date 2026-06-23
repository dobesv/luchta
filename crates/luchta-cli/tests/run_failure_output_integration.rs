use assert_cmd::Command;
use assert_fs::prelude::*;

mod common;

fn setup_failure_workspace(temp: &assert_fs::TempDir) {
    common::write_root_workspace(temp);
    temp.child("yarn.lock")
        .write_str(common::YARN1_LOCK_LEFT_PAD_1_0_0)
        .unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "version": "1.0.0",
  "scripts": {
    "build": "echo build"
  }
}
"#,
        )
        .unwrap();
    temp.child("packages/app/src.txt")
        .write_str("test\n")
        .unwrap();
}

fn run_workspace_command(
    temp: &assert_fs::TempDir,
    subcommand: &str,
    args: &[&str],
) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg(subcommand);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg("--workspace-root").arg(temp.path());
    cmd.assert()
}

fn install_long_output_worker(temp: &assert_fs::TempDir) -> assert_fs::fixture::ChildPath {
    let worker = temp.child("long-output-worker.sh");
    worker
        .write_str(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{"type":"resolved","id":"%s","result":{"decision":"accept"}}\n' "$id"
      ;;
    *'"type":"run"'*)
      WORKER_ID="$id" python3 - <<'PY'
import json
import os

worker_id = os.environ["WORKER_ID"]
for i in range(1, 151):
    print(json.dumps({"type": "log", "id": worker_id, "stream": "stdout", "line": f"line {i}"}))
print(json.dumps({"type": "done", "id": worker_id, "exitCode": 7}))
PY
      ;;
  esac
done
"#,
        )
        .unwrap();
    std::fs::set_permissions(
        worker.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    worker
}

#[test]
fn footer_duration_ms_parses_seconds_and_minute_forms() {
    let cases = [
        ("╰─ 0.2s · exit 7 · cache unknown\n", 200),
        ("╰─ 1m 5s · exit 1 · cache unknown\n", 65_000),
        ("╰─ 92.3s · exit 1 · cache unknown\n", 92_300),
    ];

    for (stderr, expected_ms) in cases {
        let start = stderr
            .find("╰─ ")
            .unwrap_or_else(|| panic!("missing duration footer format: {stderr}"))
            + "╰─ ".len();
        let rest = &stderr[start..];
        let end = rest
            .find(" · exit ")
            .unwrap_or_else(|| panic!("missing exit footer format after duration: {stderr}"));
        let trimmed = rest[..end].trim();
        let token = trimmed
            .strip_prefix("\u{1b}[2m")
            .unwrap_or(trimmed)
            .strip_suffix("\u{1b}[0m")
            .unwrap_or(trimmed);

        let duration_ms = if let Some((minutes, seconds)) = token.split_once("m ") {
            let minutes_ms = minutes
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("invalid minute duration token: {token}"));
            let seconds_ms = seconds
                .strip_suffix('s')
                .unwrap_or_else(|| panic!("invalid second duration token: {token}"))
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("invalid second duration token: {token}"));
            (minutes_ms * 60_000) + (seconds_ms * 1_000)
        } else {
            let seconds = token
                .strip_suffix('s')
                .unwrap_or_else(|| panic!("unsupported duration token: {token}"));
            (seconds
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("invalid seconds duration token: {token}"))
                * 1_000.0)
                .round() as u64
        };

        assert_eq!(duration_ms, expected_ms);
    }
}

fn assert_contains_all(haystack: &str, needles: &[&str]) {
    for n in needles {
        assert!(
            haystack.contains(n),
            "expected to contain {n:?}: {haystack}"
        );
    }
}

fn assert_contains_none(haystack: &str, needles: &[&str]) {
    for n in needles {
        assert!(
            !haystack.contains(n),
            "expected NOT to contain {n:?}: {haystack}"
        );
    }
}

struct WrappedFailureView<'a> {
    stdout: &'a str,
    stderr: &'a str,
}

impl WrappedFailureView<'_> {
    fn assert_wrapped_failure(&self) {
        assert!(
            !self.stdout.contains("╭─") && !self.stdout.contains("╰─"),
            "expected wrapped failure output on stderr only, stdout was: {}",
            self.stdout
        );
        for needle in [
            "╭─",
            "app#fail",
            " · ",
            "╰─",
            "exit ",
            "7",
            "cache ",
            "task 'app#fail' failed with status 7",
        ] {
            assert!(
                self.stderr.contains(needle),
                "expected wrapped failure markers in stderr: missing {needle}; text={}",
                self.stderr
            );
        }
    }
}

#[test]
fn run_failure_output_is_wrapped_and_success_output_stays_suppressed() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_failure_workspace(&temp);
    let worker = common::shell_worker(&temp);
    common::write_task_config_with_shell_worker(
        &temp,
        worker.path(),
        r#""app#pass":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["pass.txt"],"command":"echo hidden-success > pass.txt"},"app#fail":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["fail.txt"],"command":"exit 7"}"#,
    );
    common::init_git(&temp);

    let output = run_workspace_command(&temp, "run", &["pass", "fail"]).failure();

    let stdout = String::from_utf8_lossy(&output.get_output().stdout);
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);

    WrappedFailureView {
        stdout: &stdout,
        stderr: &stderr,
    }
    .assert_wrapped_failure();
    assert!(
        !stderr.contains("hidden-success") && !stdout.contains("hidden-success"),
        "expected success task output to remain suppressed; stdout={stdout}; stderr={stderr}"
    );
    assert!(
        !stderr.contains("app#pass") && !stdout.contains("app#pass"),
        "expected success task to stay unwrapped; stdout={stdout}; stderr={stderr}"
    );
}

#[test]
fn run_failure_output_truncates_live_replay_but_logs_stays_full() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_failure_workspace(&temp);
    let worker = install_long_output_worker(&temp);
    common::write_task_config_with_worker(
        &temp,
        common::WorkerConfig {
            name: "long-output",
            command: worker.path(),
        },
        r#""app#fail":{"cache":{},"worker":"long-output","inputs":["src.txt"],"outputs":["fail.txt"],"command":"ignored"}"#,
    );
    common::init_git(&temp);

    let run_output = run_workspace_command(&temp, "run", &["fail"]).failure();
    let run_stderr = String::from_utf8_lossy(&run_output.get_output().stderr);

    assert_contains_all(
        &run_stderr,
        &[
            "line 1",
            "line 30",
            "… 50 lines hidden — run `luchta logs -p app fail` for full output",
            "line 81",
            "line 150",
        ],
    );
    assert_contains_none(&run_stderr, &["line 31", "line 80"]);

    let logs_output = run_workspace_command(&temp, "logs", &["-p", "app", "fail"]).success();
    let logs_stdout = String::from_utf8_lossy(&logs_output.get_output().stdout);

    assert_contains_all(&logs_stdout, &["line 1", "line 31", "line 80", "line 150"]);
    assert_contains_none(&logs_stdout, &["lines hidden", "luchta logs -p app fail"]);
}

#[test]
fn run_failure_output_uses_real_start_time_for_non_cacheable_failures() {
    let temp = assert_fs::TempDir::new().unwrap();
    setup_failure_workspace(&temp);
    let worker = common::shell_worker(&temp);
    common::write_task_config_with_shell_worker(
        &temp,
        worker.path(),
        r#""app#fail":{"worker":"shell","inputs":["src.txt"],"outputs":["fail.txt"],"command":"sleep 0.2 && exit 7"}"#,
    );
    common::init_git(&temp);

    let output = run_workspace_command(&temp, "run", &["fail"]).failure();
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    let duration_start = stderr
        .find("╰─ ")
        .unwrap_or_else(|| panic!("missing duration footer format: {stderr}"))
        + "╰─ ".len();
    let duration_rest = &stderr[duration_start..];
    let duration_end = duration_rest
        .find(" · exit ")
        .unwrap_or_else(|| panic!("missing exit footer format after duration: {stderr}"));
    let duration_trimmed = duration_rest[..duration_end].trim();
    let duration_token = duration_trimmed
        .strip_prefix("\u{1b}[2m")
        .unwrap_or(duration_trimmed)
        .strip_suffix("\u{1b}[0m")
        .unwrap_or(duration_trimmed)
        .strip_suffix('s')
        .unwrap_or_else(|| panic!("unsupported duration token: {stderr}"));
    let duration_ms = (duration_token
        .parse::<f64>()
        .unwrap_or_else(|_| panic!("invalid seconds duration token: {duration_token}"))
        * 1_000.0)
        .round() as u64;

    assert!(
        duration_ms >= 100,
        "expected non-cacheable failed task to report real elapsed time, got {duration_ms}ms; stderr={stderr}"
    );
    assert!(
        !stderr.contains("╰─ 0.0s · exit"),
        "expected failure footer duration to avoid 0.0s fallback; stderr={stderr}"
    );
}
