//! End-to-end tests of direct execution through the JSONL protocol.
#![cfg(unix)]

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use assert_cmd::cargo::CommandCargoExt;
use luchta_engine::{WorkerMessage, WorkerRequest};
use luchta_yarn_env::test_fixture::{FixtureOptions, PnpFixture};
use serde_json::Value;

/// Which stream a log line is expected on.
#[derive(Clone, Copy)]
enum LogStream {
    Stdout,
    Stderr,
}

impl LogStream {
    fn as_str(self) -> &'static str {
        match self {
            LogStream::Stdout => "stdout",
            LogStream::Stderr => "stderr",
        }
    }
}

/// Whether the worker runs with direct execution (the default) or with
/// `--no-direct`, wrapping the script in `yarn` instead.
enum Mode {
    Direct,
    NoDirect,
}

impl Mode {
    fn worker_args(&self) -> &'static [&'static str] {
        match self {
            Mode::Direct => &[],
            Mode::NoDirect => &["--no-direct"],
        }
    }
}

/// A single job to run against a `PnpFixture`'s `@fixture/app` workspace.
struct Scenario<'a> {
    /// Body of the fake `yarn` shim placed on PATH ahead of the real tools.
    yarn_body: &'a str,
    /// The yarn script (plus any extra args) to request.
    command: &'a str,
    mode: Mode,
}

/// One job's worth of responses from a worker run, all sharing `id`.
struct JobOutcome {
    id: String,
    responses: Vec<Value>,
}

impl JobOutcome {
    fn exit_code(&self) -> i64 {
        self.responses
            .iter()
            .find(|v| v["type"] == "done" && v["id"] == self.id)
            .unwrap()["exitCode"]
            .as_i64()
            .unwrap()
    }

    fn logs(&self, stream: LogStream) -> Vec<&str> {
        self.responses
            .iter()
            .filter(|v| v["type"] == "log" && v["id"] == self.id && v["stream"] == stream.as_str())
            .filter_map(|v| v["line"].as_str())
            .collect()
    }

    fn stderr(&self) -> String {
        self.logs(LogStream::Stderr).join("\n")
    }
}

/// PATH value for the run: a tool dir with a fake `node` and a `yarn` whose
/// body the scenario controls, followed by the system dirs so `sh`, `bash`,
/// and `env` resolve.
fn tools_path(temp: &Path, yarn_body: &str) -> String {
    let dir = temp.join("tools");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in [("node", "#!/bin/sh\necho v22.1.0\n"), ("yarn", yarn_body)] {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    format!("{}:/usr/bin:/bin", dir.display())
}

/// Spawns `luchta-yarn-worker` against `fixture`, feeds it a single `run`
/// request for `scenario` as job `"j1"`, and collects its JSONL responses.
fn run_job(fixture: &PnpFixture, scenario: Scenario) -> JobOutcome {
    let path_value = tools_path(fixture.root.parent().unwrap(), scenario.yarn_body);
    let env: HashMap<String, String> = HashMap::from([("PATH".to_owned(), path_value)]);
    let request = WorkerRequest::new("j1", scenario.command)
        .with_workspace("@fixture/app")
        .with_cwd(fixture.app_dir.to_string_lossy())
        .with_env(env);
    let input = format!(
        "{}\n",
        serde_json::to_string(&WorkerMessage::Run(request)).unwrap()
    );

    let mut worker = Command::cargo_bin("luchta-yarn-worker").expect("binary exists");
    worker
        .args(scenario.mode.worker_args())
        .current_dir(&fixture.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = worker.spawn().expect("spawn worker");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "worker failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    JobOutcome {
        id: "j1".to_owned(),
        responses,
    }
}

#[test]
fn direct_mode_runs_script_without_invoking_yarn() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());

    let outcome = run_job(
        &fixture,
        Scenario {
            yarn_body: "#!/bin/sh\necho 'yarn must not run' >&2\nexit 97\n",
            command: "build",
            mode: Mode::Direct,
        },
    );

    assert_eq!(outcome.exit_code(), 0, "{:?}", outcome.responses);
    assert_eq!(outcome.logs(LogStream::Stdout), vec!["building"]);
}

#[test]
fn direct_mode_exposes_yarn_env_and_shims() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());

    let outcome = run_job(
        &fixture,
        Scenario {
            yarn_body: "#!/bin/sh\nexit 97\n",
            command: "env",
            mode: Mode::Direct,
        },
    );
    let stdout = outcome.logs(LogStream::Stdout);

    assert!(
        stdout.contains(&"npm_package_name=@fixture/app"),
        "{stdout:?}"
    );
    assert!(
        stdout
            .iter()
            .any(|l| l.starts_with("NODE_OPTIONS=--require ") && l.ends_with(".pnp.cjs")),
        "{stdout:?}"
    );
    let bin_folder = stdout
        .iter()
        .find_map(|l| l.strip_prefix("BERRY_BIN_FOLDER="))
        .unwrap();
    assert!(Path::new(bin_folder).join("left-pad").is_file());
    assert!(
        stdout
            .iter()
            .any(|l| l.starts_with(&format!("PATH={bin_folder}:"))),
        "{stdout:?}"
    );
}

#[test]
fn direct_mode_env_across_pnp_layouts() {
    for inline_manifest in [false, true] {
        for with_loader in [false, true] {
            let combo = format!("inline_manifest={inline_manifest} with_loader={with_loader}");
            let temp = tempfile::tempdir().unwrap();
            let fixture = PnpFixture::write(
                &temp.path().join("repo"),
                FixtureOptions {
                    inline_manifest,
                    with_loader,
                    ..FixtureOptions::default()
                },
            );
            let root = std::fs::canonicalize(&fixture.root)
                .unwrap_or_else(|error| panic!("{combo}: canonicalize root failed: {error}"));

            let outcome = run_job(
                &fixture,
                Scenario {
                    yarn_body: "#!/bin/sh\nexit 97\n",
                    command: "env",
                    mode: Mode::Direct,
                },
            );
            let stdout = outcome.logs(LogStream::Stdout);

            let mut expected =
                format!("NODE_OPTIONS=--require {}", root.join(".pnp.cjs").display());
            if with_loader {
                expected.push_str(&format!(
                    " --experimental-loader file://{}",
                    root.join(".pnp.loader.mjs").display()
                ));
            }
            assert!(
                stdout.iter().any(|line| *line == expected),
                "{combo}: expected {expected:?} in {stdout:?}"
            );
        }
    }
}

#[test]
fn direct_mode_passes_extra_args() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());

    let outcome = run_job(
        &fixture,
        Scenario {
            yarn_body: "#!/bin/sh\nexit 97\n",
            command: "args --flag 'two words'",
            mode: Mode::Direct,
        },
    );

    assert_eq!(outcome.logs(LogStream::Stdout), vec!["--flag", "two words"]);
}

#[test]
fn no_direct_flag_routes_through_yarn() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());

    let outcome = run_job(
        &fixture,
        Scenario {
            yarn_body: "#!/bin/sh\necho \"yarn-called $*\"\n",
            command: "build",
            mode: Mode::NoDirect,
        },
    );

    assert_eq!(outcome.exit_code(), 0);
    assert_eq!(
        outcome.logs(LogStream::Stdout),
        vec!["yarn-called workspace @fixture/app build"]
    );
}

/// Runs `command` in direct mode against a fresh fixture, optionally with
/// `.pnp.cjs` removed first, asserts the worker reported failure, and
/// returns the stderr text for the caller to check.
fn failing_command_stderr(command: &str, remove_manifest: bool) -> String {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());
    if remove_manifest {
        std::fs::remove_file(fixture.root.join(".pnp.cjs")).unwrap();
    }

    let outcome = run_job(
        &fixture,
        Scenario {
            yarn_body: "#!/bin/sh\nexit 97\n",
            command,
            mode: Mode::Direct,
        },
    );

    assert_eq!(outcome.exit_code(), 1);
    outcome.stderr()
}

#[test]
fn direct_mode_fails_descriptively_without_manifest() {
    let stderr = failing_command_stderr("build", true);
    assert!(stderr.contains(".pnp.cjs"), "{stderr}");
    assert!(stderr.contains("--no-direct"), "{stderr}");
}

#[test]
fn direct_mode_fails_descriptively_for_unknown_script() {
    let stderr = failing_command_stderr("missing", false);
    assert!(stderr.contains("`missing`"), "{stderr}");
}

#[test]
fn direct_mode_fails_for_yarn_builtin_command() {
    // `install` is a yarn CLI command, not a `package.json` script; direct
    // mode looks it up as a script and fails the same way it would for any
    // other undeclared script name.
    let stderr = failing_command_stderr("install", false);
    assert!(stderr.contains("`install`"), "{stderr}");
    assert!(stderr.contains("--no-direct"), "{stderr}");
}
