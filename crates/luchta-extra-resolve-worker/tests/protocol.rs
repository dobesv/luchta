use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use assert_cmd::cargo::cargo_bin;
use serde_json::{json, Value};

const CONCURRENCY_GATE_MAX_ATTEMPTS: usize = 500;
const CONCURRENCY_GATE_POLL_SECONDS: f64 = 0.01;

#[derive(Clone, Copy)]
struct CaseId(&'static str);

#[derive(Clone, Copy)]
struct Text(&'static str);

#[derive(Clone, Copy)]
struct InputGlob(&'static str);

#[derive(Clone, Copy)]
enum ResolveDecisionSpec {
    Accept,
    Prune,
    Reject(Text),
    ModifyCommand(Text),
    ModifyInputs(&'static [InputGlob]),
}

#[derive(Clone, Copy)]
enum DelegateBehavior<'a> {
    Ignore,
    Crash,
    Sentinel(&'a Path),
    Resolve(ResolveDecisionSpec),
    LogThenResolve {
        line: Text,
        result: ResolveDecisionSpec,
    },
    RunDone,
}

#[derive(Clone, Copy)]
enum ExpectedOutcome {
    Prune,
    Reject { message: Text },
    ModifyCommand { command: Text },
    ModifyInputs { inputs: &'static [InputGlob] },
}

#[derive(Clone, Copy)]
struct ResolveCase<'a> {
    label: &'static str,
    id: CaseId,
    resolve: ResolveDecisionSpec,
    delegate: DelegateBehavior<'a>,
    expected: ExpectedOutcome,
}

impl CaseId {
    fn as_str(self) -> &'static str {
        self.0
    }
}

impl Text {
    fn as_str(self) -> &'static str {
        self.0
    }
}

impl InputGlob {
    fn as_str(self) -> &'static str {
        self.0
    }
}

fn resolve_message(id: CaseId) -> String {
    format!(
        r#"{{"type":"resolveTask","id":"{}","name":"build","command":"echo hi","package":"@repo/app","cwd":null,"scripts":["build"],"mode":"run"}}"#,
        id.as_str()
    )
}

fn run_message(id: CaseId) -> String {
    format!(
        r#"{{"type":"run","id":"{}","name":"build","command":"echo hi","package":"@repo/app","cwd":null,"env":{{}},"shell":"/bin/sh"}}"#,
        id.as_str()
    )
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    let text = String::from_utf8(stdout.to_vec()).expect("utf8 stdout");
    text.lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid jsonl line"))
        .collect()
}

fn shell_program(body: &str) -> Vec<String> {
    vec!["sh".to_owned(), "-c".to_owned(), body.to_owned()]
}

fn globs_json(inputs: &'static [InputGlob]) -> Vec<&'static str> {
    inputs.iter().map(|glob| glob.as_str()).collect()
}

fn resolve_response(id: CaseId, decision: ResolveDecisionSpec) -> String {
    let response = match decision {
        ResolveDecisionSpec::Accept => {
            json!({"type":"resolved","id":id.as_str(),"result":{"decision":"accept"}})
        }
        ResolveDecisionSpec::Prune => {
            json!({"type":"resolved","id":id.as_str(),"result":{"decision":"prune"}})
        }
        ResolveDecisionSpec::Reject(message) => {
            json!({"type":"resolved","id":id.as_str(),"result":{"decision":"reject","message":message.as_str()}})
        }
        ResolveDecisionSpec::ModifyCommand(command) => {
            json!({"type":"resolved","id":id.as_str(),"result":{"decision":"modify","command":command.as_str()}})
        }
        ResolveDecisionSpec::ModifyInputs(inputs) => {
            json!({"type":"resolved","id":id.as_str(),"result":{"decision":"modify","inputs":globs_json(inputs)}})
        }
    };
    response.to_string()
}

fn resolve_worker_argv(id: CaseId, decision: ResolveDecisionSpec) -> Vec<String> {
    let response = resolve_response(id, decision);
    let script = format!(
        "while IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"resolveTask\"'*)\n      printf '%s\\n' '{}'\n      ;;\n  esac\ndone\n",
        response
    );
    shell_program(&script)
}

fn delegate_argv(id: CaseId, behavior: DelegateBehavior<'_>) -> Vec<String> {
    let script = match behavior {
        DelegateBehavior::Ignore => "while IFS= read -r _line; do :; done\n".to_owned(),
        DelegateBehavior::Crash => "read -r _line; exit 42\n".to_owned(),
        DelegateBehavior::Sentinel(path) => {
            format!(
                "while IFS= read -r _line; do\n  touch '{}'\ndone\n",
                path.display()
            )
        }
        DelegateBehavior::Resolve(spec) => {
            let response = resolve_response(id, spec);
            format!(
                "while IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"resolveTask\"'*)\n      printf '%s\\n' '{}'\n      ;;\n  esac\ndone\n",
                response
            )
        }
        DelegateBehavior::LogThenResolve { line, result } => {
            let log = json!({
                "type":"log",
                "id":id.as_str(),
                "stream":"stdout",
                "line":line.as_str()
            })
            .to_string();
            let resolved = resolve_response(id, result);
            format!(
                "while IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"resolveTask\"'*)\n      printf '%s\\n' '{}'\n      printf '%s\\n' '{}'\n      ;;\n  esac\ndone\n",
                log, resolved
            )
        }
        DelegateBehavior::RunDone => format!(
            "while IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"run\"'*)\n      printf '%s\\n' '{}'\n      ;;\n  esac\ndone\n",
            json!({"type":"done","id":id.as_str(),"exitCode":0})
        ),
    };
    shell_program(&script)
}

fn run_worker(resolve_argv: &[String], delegate_argv: &[String], input_lines: &[String]) -> Output {
    let mut argv = resolve_argv.to_vec();
    argv.push("--".to_owned());
    argv.extend(delegate_argv.iter().cloned());

    let mut child = Command::new(cargo_bin("luchta-extra-resolve-worker"))
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for line in input_lines {
            writeln!(stdin, "{line}").expect("write input line");
        }
    }

    child.wait_with_output().expect("wait for worker")
}

fn assert_resolved_count_one(output: &Output) -> Vec<Value> {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = parse_jsonl(&output.stdout);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["type"], "resolved");
    responses
}

fn run_case(case: ResolveCase<'_>) -> Vec<Value> {
    let output = run_worker(
        &resolve_worker_argv(case.id, case.resolve),
        &delegate_argv(case.id, case.delegate),
        &[resolve_message(case.id)],
    );
    assert_resolved_count_one(&output)
}

fn assert_resolve_case(case: ResolveCase<'_>) {
    let responses = run_case(case);
    assert_eq!(responses[0]["id"], case.id.as_str(), "case {}", case.label);
    match case.expected {
        ExpectedOutcome::Prune => {
            assert_eq!(
                responses[0]["result"]["decision"], "prune",
                "case {}",
                case.label
            );
        }
        ExpectedOutcome::Reject { message } => {
            assert_eq!(
                responses[0]["result"]["decision"], "reject",
                "case {}",
                case.label
            );
            assert_eq!(
                responses[0]["result"]["message"],
                message.as_str(),
                "case {}",
                case.label
            );
        }
        ExpectedOutcome::ModifyCommand { command } => {
            assert_eq!(
                responses[0]["result"]["decision"], "modify",
                "case {}",
                case.label
            );
            assert_eq!(
                responses[0]["result"]["command"],
                command.as_str(),
                "case {}",
                case.label
            );
        }
        ExpectedOutcome::ModifyInputs { inputs } => {
            assert_eq!(
                responses[0]["result"]["decision"], "modify",
                "case {}",
                case.label
            );
            assert_eq!(
                responses[0]["result"]["inputs"],
                json!(globs_json(inputs)),
                "case {}",
                case.label
            );
        }
    }
}

fn assert_failed_without_resolved(output: &Output) {
    assert!(
        !output.status.success(),
        "worker unexpectedly succeeded: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        parse_jsonl(&output.stdout)
            .iter()
            .all(|response| response["type"] != "resolved"),
        "worker emitted synthetic resolved response: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn resolve_worker_crash_fails_without_pruning() {
    let id = CaseId("resolve-error");
    let output = run_worker(
        &shell_program("exit 1"),
        &delegate_argv(id, DelegateBehavior::Ignore),
        &[resolve_message(id)],
    );

    assert_failed_without_resolved(&output);
}

#[test]
fn run_delegate_crash_after_accept_fails_without_pruning() {
    let id = CaseId("accept-delegate-crash");
    let output = run_worker(
        &resolve_worker_argv(id, ResolveDecisionSpec::Accept),
        &delegate_argv(id, DelegateBehavior::Crash),
        &[resolve_message(id)],
    );

    assert_failed_without_resolved(&output);
}

#[test]
fn run_delegate_crash_after_modify_fails_without_pruning() {
    let id = CaseId("modify-delegate-crash");
    let output = run_worker(
        &resolve_worker_argv(
            id,
            ResolveDecisionSpec::ModifyCommand(Text("build:modified")),
        ),
        &delegate_argv(id, DelegateBehavior::Crash),
        &[resolve_message(id)],
    );

    assert_failed_without_resolved(&output);
}

#[test]
fn resolve_worker_short_circuit_cases() {
    let prune_dir = tempfile::tempdir().expect("tempdir");
    let prune_sentinel = prune_dir.path().join("delegate-called");
    let reject_dir = tempfile::tempdir().expect("tempdir");
    let reject_sentinel = reject_dir.path().join("delegate-called");

    let cases = [
        ResolveCase {
            label: "resolve prune short circuit",
            id: CaseId("resolve-prune"),
            resolve: ResolveDecisionSpec::Prune,
            delegate: DelegateBehavior::Sentinel(&prune_sentinel),
            expected: ExpectedOutcome::Prune,
        },
        ResolveCase {
            label: "resolve reject short circuit",
            id: CaseId("resolve-reject"),
            resolve: ResolveDecisionSpec::Reject(Text("bad")),
            delegate: DelegateBehavior::Sentinel(&reject_sentinel),
            expected: ExpectedOutcome::Reject {
                message: Text("bad"),
            },
        },
    ];

    for case in cases {
        assert_resolve_case(case);
    }

    assert!(
        !prune_sentinel.exists(),
        "delegate should not receive input on prune"
    );
    assert!(
        !reject_sentinel.exists(),
        "delegate should not receive input on reject"
    );
}

#[test]
fn resolve_worker_resolution_matrix() {
    const DIST: &[InputGlob] = &[InputGlob("dist/**")];
    let cases = [
        ResolveCase {
            label: "accept delegate modifies",
            id: CaseId("accept-delegate-modify"),
            resolve: ResolveDecisionSpec::Accept,
            delegate: DelegateBehavior::Resolve(ResolveDecisionSpec::ModifyInputs(DIST)),
            expected: ExpectedOutcome::ModifyInputs { inputs: DIST },
        },
        ResolveCase {
            label: "accept delegate prunes",
            id: CaseId("accept-delegate-prune"),
            resolve: ResolveDecisionSpec::Accept,
            delegate: DelegateBehavior::Resolve(ResolveDecisionSpec::Prune),
            expected: ExpectedOutcome::Prune,
        },
        ResolveCase {
            label: "modify delegate accepts",
            id: CaseId("modify-delegate-accept"),
            resolve: ResolveDecisionSpec::ModifyCommand(Text("build:prod")),
            delegate: DelegateBehavior::Resolve(ResolveDecisionSpec::Accept),
            expected: ExpectedOutcome::ModifyCommand {
                command: Text("build:prod"),
            },
        },
        ResolveCase {
            label: "modify delegate also modifies",
            id: CaseId("modify-delegate-modify"),
            resolve: ResolveDecisionSpec::ModifyCommand(Text("build:A")),
            delegate: DelegateBehavior::Resolve(ResolveDecisionSpec::ModifyCommand(Text(
                "build:B",
            ))),
            expected: ExpectedOutcome::ModifyCommand {
                command: Text("build:B"),
            },
        },
        ResolveCase {
            label: "accept delegate log then resolves",
            id: CaseId("accept-delegate-log-first"),
            resolve: ResolveDecisionSpec::Accept,
            delegate: DelegateBehavior::LogThenResolve {
                line: Text("hello"),
                result: ResolveDecisionSpec::ModifyInputs(DIST),
            },
            expected: ExpectedOutcome::ModifyInputs { inputs: DIST },
        },
    ];

    for case in cases {
        assert_resolve_case(case);
    }
}

#[test]
fn run_forwards_done_from_delegate() {
    let id = CaseId("run-done");
    let output = run_worker(
        &resolve_worker_argv(id, ResolveDecisionSpec::Accept),
        &delegate_argv(id, DelegateBehavior::RunDone),
        &[run_message(id)],
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = parse_jsonl(&output.stdout);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["type"], "done");
    assert_eq!(responses[0]["id"], id.as_str());
    assert_eq!(responses[0]["exitCode"], 0);
}

#[test]
fn independent_runs_reach_delegate_concurrently() {
    let temp = tempfile::tempdir().expect("tempdir");
    let started = temp.path().join("delegate.started");
    let gate = temp.path().join("delegate.gate");
    let delegate_script = format!(
        r#"while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"run"'*)
      printf '%s\n' "$id" >> {started}
      if [ "$(wc -l < {started})" -ge 2 ]; then
        touch {gate}
      fi
      (
        attempts=0
        while [ ! -f {gate} ] && [ "$attempts" -lt {max_attempts} ]; do
          sleep {poll_seconds}
          attempts=$((attempts + 1))
        done
        if [ -f {gate} ]; then
          exit_code=0
        else
          exit_code=1
        fi
        printf '{{"type":"done","id":"%s","exitCode":%s}}\n' "$id" "$exit_code"
      ) &
      ;;
  esac
done
wait
"#,
        started = started.display(),
        gate = gate.display(),
        max_attempts = CONCURRENCY_GATE_MAX_ATTEMPTS,
        poll_seconds = CONCURRENCY_GATE_POLL_SECONDS,
    );
    let output = run_worker(
        &shell_program("while IFS= read -r _line; do :; done\n"),
        &shell_program(&delegate_script),
        &[
            run_message(CaseId("run-one")),
            run_message(CaseId("run-two")),
        ],
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = parse_jsonl(&output.stdout);
    assert_eq!(responses.len(), 2, "responses: {responses:?}");
    assert!(responses
        .iter()
        .all(|response| { response["type"] == "done" && response["exitCode"] == 0 }));
    let started_ids = std::fs::read_to_string(&started).expect("read started ids");
    assert_eq!(started_ids.lines().count(), 2, "started ids: {started_ids}");
    assert!(
        gate.exists(),
        "delegate never observed two simultaneous in-flight runs"
    );
}

#[test]
fn concurrent_resolve_suppression_preserves_run_response() {
    let temp = tempfile::tempdir().expect("tempdir");
    let resolve_id = CaseId("resolve-concurrent");
    let run_id = CaseId("run-concurrent");
    let delegate_argv = concurrent_resolve_run_delegate(temp.path(), resolve_id, run_id);
    let output = run_worker(
        &resolve_worker_argv(resolve_id, ResolveDecisionSpec::Accept),
        &delegate_argv,
        &[resolve_message(resolve_id), run_message(run_id)],
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_concurrent_resolve_run_responses(&parse_jsonl(&output.stdout), resolve_id, run_id);
}

fn concurrent_resolve_run_delegate(temp: &Path, resolve_id: CaseId, run_id: CaseId) -> Vec<String> {
    let resolve_seen = temp.join("delegate.resolve-seen");
    let run_done = temp.join("delegate.run-done");
    let resolved = resolve_response(resolve_id, ResolveDecisionSpec::Accept);
    let timed_out = resolve_response(
        resolve_id,
        ResolveDecisionSpec::Reject(Text("run did not arrive concurrently")),
    );
    let done = json!({"type":"done","id":run_id.as_str(),"exitCode":0}).to_string();
    let failed_done = json!({"type":"done","id":run_id.as_str(),"exitCode":1}).to_string();
    shell_program(&format!(
        r#"while IFS= read -r line; do
  case "$line" in
    *'"type":"resolveTask"'*)
      touch {resolve_seen}
      (
        attempts=0
        while [ ! -f {run_done} ] && [ "$attempts" -lt {max_attempts} ]; do
          sleep {poll_seconds}
          attempts=$((attempts + 1))
        done
        if [ -f {run_done} ]; then
          printf '%s\n' '{resolved}'
        else
          printf '%s\n' '{timed_out}'
        fi
      ) &
      ;;
    *'"type":"run"'*)
      (
        attempts=0
        while [ ! -f {resolve_seen} ] && [ "$attempts" -lt {max_attempts} ]; do
          sleep {poll_seconds}
          attempts=$((attempts + 1))
        done
        if [ -f {resolve_seen} ]; then
          printf '%s\n' '{done}'
        else
          printf '%s\n' '{failed_done}'
        fi
        touch {run_done}
      ) &
      ;;
  esac
done
wait
"#,
        resolve_seen = resolve_seen.display(),
        run_done = run_done.display(),
        max_attempts = CONCURRENCY_GATE_MAX_ATTEMPTS,
        poll_seconds = CONCURRENCY_GATE_POLL_SECONDS,
    ))
}

fn assert_concurrent_resolve_run_responses(
    responses: &[Value],
    resolve_id: CaseId,
    run_id: CaseId,
) {
    assert_eq!(responses.len(), 2, "responses: {responses:?}");
    let run_response = responses
        .iter()
        .find(|response| response["id"] == run_id.as_str())
        .expect("run response");
    assert_eq!(run_response["type"], "done");
    assert_eq!(run_response["exitCode"], 0);
    let resolve_response = responses
        .iter()
        .find(|response| response["id"] == resolve_id.as_str())
        .expect("resolve response");
    assert_eq!(resolve_response["type"], "resolved");
    assert_eq!(resolve_response["result"]["decision"], "accept");
}
