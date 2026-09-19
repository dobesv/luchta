//! End-to-end coverage of yarn direct execution against a real Yarn PnP
//! fixture, driven through the full `luchta` CLI: engine -> resident
//! `luchta-yarn-worker` -> bash. The worker's own binary tests
//! (`crates/luchta-yarn-worker/tests/direct.rs`) exercise the JSONL protocol
//! directly and skip the engine's resolve step entirely; these tests instead
//! run `luchta run` against a real workspace so the resolve-time contract
//! (`ResolveTask`/`scripts`), the `req.cwd`/`workspace` hint, env passthrough,
//! failure surfacing, and caching are all covered together.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use assert_fs::prelude::*;
use luchta_yarn_env::test_fixture::{FixtureOptions, PnpFixture};

mod common;

use common::{init_git, path_with_prepend, write_executable, yarn_worker_bin};

/// Fake `yarn` that fails loudly if invoked, for direct-mode tests where any
/// call to `yarn` is itself the bug under test.
const FAKE_YARN_TRIPWIRE: &str = "#!/bin/sh\necho \"yarn-called $*\"\nexit 97\n";

/// Fake `yarn` that just echoes its invocation, for the `--no-direct` worker.
const FAKE_YARN_OK: &str = "#!/bin/sh\necho \"yarn-called $*\"\n";

/// Concatenates stdout and stderr for a single "what did luchta print" check.
fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A PnP-fixture workspace with its own tools dir, `luchta-config.sh`, and
/// git repo, ready for `luchta run`/`luchta logs`. Each test builds its own so
/// there is no shared state between them.
struct Workspace {
    temp: assert_fs::TempDir,
    path: String,
}

impl Workspace {
    /// Writes `PnpFixture::write`'s PnP project plus an empty `yarn.lock`, a
    /// `bin/node` + `bin/yarn` (`yarn_body` chooses the fake yarn's behavior)
    /// tools dir, a `luchta-config.sh` wiring `build`/`build-cli`/`args`/`env`
    /// tasks to `luchta-yarn-worker`, and a git repo (the cache task needs
    /// one).
    fn new(yarn_body: &str, fixture_options: FixtureOptions) -> Self {
        let temp = assert_fs::TempDir::new().unwrap();
        PnpFixture::write(temp.path(), fixture_options);
        temp.child("yarn.lock").write_str("").unwrap();

        let bin_dir = temp.child("bin");
        bin_dir.create_dir_all().unwrap();
        write_executable(bin_dir.child("node").path(), "#!/bin/sh\necho v22.1.0\n");
        write_executable(bin_dir.child("yarn").path(), yarn_body);

        let worker = yarn_worker_bin();
        write_executable(
            temp.child("luchta-config.sh").path(),
            &format!(
                "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\n \"workers\":{{\"yarn\":{{\"command\":\"{worker}\"}},\"yarn-cli\":{{\"command\":\"{worker} --no-direct\"}}}},\n \"tasks\":{{\n   \"build\":{{\"worker\":\"yarn\",\"cache\":{{}},\"inputs\":[\"package.json\"]}},\n   \"build-cli\":{{\"worker\":\"yarn-cli\",\"command\":\"build\",\"cache\":{{}}}},\n   \"args\":{{\"worker\":\"yarn\",\"command\":\"args --flag '\"'\"'two words'\"'\"'\",\"cache\":{{}}}},\n   \"env\":{{\"worker\":\"yarn\",\"cache\":{{}}}}\n }}}}'\n",
                worker = worker.display(),
            ),
        );

        init_git(&temp);
        let path = path_with_prepend(bin_dir.path());
        Self { temp, path }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn app_dir(&self) -> PathBuf {
        self.temp.path().join("packages/app")
    }

    /// Runs `luchta <subcommand> <task> --workspace-root <root>`. Shared by
    /// `run` and `logs` below, which only differ in the subcommand.
    fn luchta(&self, subcommand: Subcommand, task: &str) -> Output {
        Command::cargo_bin("luchta")
            .unwrap()
            .env("PATH", &self.path)
            .arg(subcommand.as_str())
            .arg(task)
            .arg("--workspace-root")
            .arg(self.temp.path())
            .output()
            .unwrap()
    }

    /// Runs `luchta run <task> --workspace-root <root>`.
    fn run(&self, task: &str) -> Output {
        self.luchta(Subcommand::Run, task)
    }

    /// Runs `luchta logs <task> --workspace-root <root>`, returning the
    /// cached run's stdout/stderr as luchta pretty-prints it. Only tasks
    /// declaring `cache: {}` have anything to show.
    fn logs(&self, task: &str) -> Output {
        self.luchta(Subcommand::Logs, task)
    }
}

/// Which `luchta` subcommand `Workspace::luchta` should invoke.
#[derive(Clone, Copy)]
enum Subcommand {
    Run,
    Logs,
}

impl Subcommand {
    fn as_str(self) -> &'static str {
        match self {
            Subcommand::Run => "run",
            Subcommand::Logs => "logs",
        }
    }
}

#[test]
fn direct_mode_runs_script_through_cli() {
    let ws = Workspace::new(FAKE_YARN_TRIPWIRE, FixtureOptions::default());

    let run = ws.run("build");
    assert!(run.status.success(), "run build failed: {}", combined(&run));
    // The tripwire yarn exits 97 on any invocation, so `run.status.success()`
    // above is the real proof direct mode never called it; this only guards
    // against the "yarn-called" marker leaking into `run`'s own output.
    assert!(
        !combined(&run).contains("yarn-called"),
        "yarn-called marker leaked into run output: {}",
        combined(&run)
    );

    // `luchta run` only prints a progress summary on success; fetch the
    // cached run's own stdout via `luchta logs` to confirm the script itself
    // actually executed under bash.
    let logs = combined(&ws.logs("build"));
    assert!(
        logs.contains("building"),
        "expected the script's stdout in the cached log: {logs}"
    );
    assert!(
        !logs.contains("yarn-called"),
        "direct mode must not invoke yarn: {logs}"
    );
}

#[test]
fn direct_mode_passes_extra_args_through_cli() {
    let ws = Workspace::new(FAKE_YARN_TRIPWIRE, FixtureOptions::default());

    let run = ws.run("args");
    assert!(run.status.success(), "run args failed: {}", combined(&run));
    // The tripwire yarn exits 97 on any invocation, so `run.status.success()`
    // above is the real proof direct mode never called it; this only guards
    // against the "yarn-called" marker leaking into `run`'s own output.
    assert!(
        !combined(&run).contains("yarn-called"),
        "yarn-called marker leaked into run output: {}",
        combined(&run)
    );

    let logs = combined(&ws.logs("args"));
    assert!(
        logs.lines().any(|line| line == "--flag"),
        "expected the first extra arg on its own line: {logs}"
    );
    assert!(
        logs.lines().any(|line| line == "two words"),
        "expected the quoted extra arg preserved as one line: {logs}"
    );
}

#[test]
fn no_direct_worker_routes_through_yarn_through_cli() {
    let ws = Workspace::new(FAKE_YARN_OK, FixtureOptions::default());

    let run = ws.run("build-cli");
    assert!(
        run.status.success(),
        "run build-cli failed: {}",
        combined(&run)
    );

    let logs = combined(&ws.logs("build-cli"));
    assert!(
        logs.contains("yarn-called workspace @fixture/app build"),
        "--no-direct should route the task through yarn: {logs}"
    );
    assert!(
        !logs.contains("building"),
        "yarn (faked) wrapped the call, so the script's own echo must not appear: {logs}"
    );
}

#[test]
fn direct_mode_exposes_yarn_env_through_cli() {
    let ws = Workspace::new(FAKE_YARN_TRIPWIRE, FixtureOptions::default());
    // Direct mode canonicalizes paths; canonicalize the expected values too
    // (on Linux temp dirs are usually already canonical, but this keeps the
    // comparison exact either way).
    let root = std::fs::canonicalize(ws.root()).unwrap();
    let app_dir = std::fs::canonicalize(ws.app_dir()).unwrap();

    let run = ws.run("env");
    assert!(run.status.success(), "run env failed: {}", combined(&run));

    let logs = combined(&ws.logs("env"));
    assert!(logs.contains("npm_package_name=@fixture/app"), "{logs}");
    assert!(logs.contains("npm_lifecycle_event=env"), "{logs}");
    assert!(
        logs.contains(&format!("PROJECT_CWD={}", root.display())),
        "{logs}"
    );
    assert!(
        logs.contains(&format!("INIT_CWD={}", app_dir.display())),
        "{logs}"
    );
    let expected_node_options = format!("--require {}/.pnp.cjs", root.display());
    assert!(
        logs.lines().any(|line| {
            line.starts_with("NODE_OPTIONS=") && line.ends_with(&expected_node_options)
        }),
        "{logs}"
    );
}

#[test]
fn direct_mode_fails_descriptively_without_manifest_through_cli() {
    let ws = Workspace::new(FAKE_YARN_TRIPWIRE, FixtureOptions::default());
    std::fs::remove_file(ws.root().join(".pnp.cjs")).unwrap();
    std::fs::remove_file(ws.root().join(".pnp.data.json")).unwrap();

    let run = ws.run("build");
    let text = combined(&run);
    assert!(
        !run.status.success(),
        "run build should fail without a PnP manifest: {text}"
    );
    assert!(text.contains(".pnp.cjs"), "{text}");
    assert!(text.contains("--no-direct"), "{text}");
    assert!(!text.contains("yarn-called"), "{text}");
}

#[test]
fn direct_mode_task_is_cached_on_second_run() {
    let ws = Workspace::new(FAKE_YARN_TRIPWIRE, FixtureOptions::default());

    let first = ws.run("build");
    let first_text = combined(&first);
    assert!(
        first.status.success(),
        "first run build failed: {first_text}"
    );
    assert!(
        !first_text.contains("⏩"),
        "first run should not report a cache skip: {first_text}"
    );

    let second = ws.run("build");
    let second_text = combined(&second);
    assert!(
        second.status.success(),
        "second run build failed: {second_text}"
    );
    assert!(
        second_text.contains("⏩ 1"),
        "second run should be a cache hit: {second_text}"
    );
}

#[test]
fn direct_mode_runs_across_pnp_layouts() {
    for inline_manifest in [false, true] {
        for with_loader in [false, true] {
            let combo = format!("inline_manifest={inline_manifest} with_loader={with_loader}");
            let ws = Workspace::new(
                FAKE_YARN_TRIPWIRE,
                FixtureOptions {
                    inline_manifest,
                    with_loader,
                    ..FixtureOptions::default()
                },
            );
            let root = std::fs::canonicalize(ws.root())
                .unwrap_or_else(|error| panic!("{combo}: canonicalize root failed: {error}"));

            let run = ws.run("env");
            assert!(
                run.status.success(),
                "{combo}: run env failed: {}",
                combined(&run)
            );

            let logs = combined(&ws.logs("env"));
            let mut expected =
                format!("NODE_OPTIONS=--require {}", root.join(".pnp.cjs").display());
            if with_loader {
                expected.push_str(&format!(
                    " --experimental-loader file://{}",
                    root.join(".pnp.loader.mjs").display()
                ));
            }
            assert!(
                logs.lines().any(|line| line == expected),
                "{combo}: expected {expected:?} in {logs}"
            );
        }
    }
}
