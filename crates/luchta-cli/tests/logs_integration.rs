//! Integration tests for `luchta logs` command.

use assert_cmd::Command;
use assert_fs::prelude::*;

mod common;

const SINGLE_BUILD_TASK: &str = r#""app#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"echo build > out.txt"}"#;
const SINGLE_BUILD_FILES: &[(&str, &str)] = &[("packages/app/src.txt", "test\n")];
const BUILD_AND_TEST_TASKS: &str = r#""app#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"echo build > out.txt"},"app#test":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["test-out.txt"],"command":"echo test > test-out.txt"}"#;

struct LogsScenario<'a> {
    task_json: &'a str,
    extra_files: &'a [(&'a str, &'a str)],
}

fn setup_logs_workspace(scenario: LogsScenario<'_>) -> assert_fs::TempDir {
    let temp = assert_fs::TempDir::new().unwrap();
    common::setup_pkgbuild_counter_workspace(
        &temp,
        common::YARN1_LOCK_LEFT_PAD_1_0_0,
        scenario.task_json,
        scenario.extra_files,
    );
    temp
}

fn setup_single_build_workspace() -> assert_fs::TempDir {
    setup_logs_workspace(LogsScenario {
        task_json: SINGLE_BUILD_TASK,
        extra_files: SINGLE_BUILD_FILES,
    })
}

fn run_logs(temp: &assert_fs::TempDir, args: &[&str]) -> String {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("logs");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg("--workspace-root").arg(temp.path());
    let output = cmd.assert().success();
    String::from_utf8_lossy(&output.get_output().stdout).into_owned()
}

struct LogAssertion<'a> {
    needle: &'a str,
    present: bool,
    message: &'a str,
}

struct LogsCase<'a> {
    scenario: LogsScenario<'a>,
    run_tasks: &'a [&'a str],
    args: &'a [&'a str],
    assertions: &'a [LogAssertion<'a>],
}

fn assert_logs_case(case: LogsCase<'_>) {
    let temp = setup_logs_workspace(case.scenario);
    for task in case.run_tasks {
        common::run_luchta(&temp, task).success();
    }
    let stdout = run_logs(&temp, case.args);
    for assertion in case.assertions {
        let found = stdout.contains(assertion.needle);
        assert_eq!(
            found, assertion.present,
            "{}: {}",
            assertion.message, stdout
        );
    }
}

#[test]
fn logs_no_args_shows_all_cached_tasks() {
    let temp = setup_logs_workspace(LogsScenario {
        task_json: r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"echo hello > counter.txt"}"#,
        extra_files: SINGLE_BUILD_FILES,
    });

    common::run_luchta(&temp, "pkgbuild").success();
    let stdout = run_logs(&temp, &[]);

    assert!(
        stdout.contains("╭─"),
        "expected header marker in output: {stdout}"
    );
    assert!(
        stdout.contains("╰─"),
        "expected footer marker in output: {stdout}"
    );
    assert!(
        stdout.contains("app#pkgbuild") || stdout.contains("app"),
        "expected task label in output: {stdout}"
    );
}

fn assert_build_vs_test_logs(args: &[&str]) {
    assert_logs_case(LogsCase {
        scenario: LogsScenario {
            task_json: BUILD_AND_TEST_TASKS,
            extra_files: SINGLE_BUILD_FILES,
        },
        run_tasks: &["build", "test"],
        args,
        assertions: &[
            LogAssertion {
                needle: "build",
                present: true,
                message: "expected build task in output",
            },
            LogAssertion {
                needle: "app#test",
                present: false,
                message: "expected no test task in output",
            },
        ],
    });
}

#[test]
fn logs_filters_task_selection() {
    assert_build_vs_test_logs(&["build"]);
    assert_build_vs_test_logs(&["b*"]);
    assert_logs_case(LogsCase {
        scenario: LogsScenario {
            task_json: SINGLE_BUILD_TASK,
            extra_files: SINGLE_BUILD_FILES,
        },
        run_tasks: &["build"],
        args: &["build", "-p", "app"],
        assertions: &[LogAssertion {
            needle: "app#build",
            present: true,
            message: "expected app#build task in output",
        }],
    });
    assert_logs_case(LogsCase {
        scenario: LogsScenario {
            task_json: BUILD_AND_TEST_TASKS,
            extra_files: SINGLE_BUILD_FILES,
        },
        run_tasks: &["build", "test"],
        args: &["-p", "app"],
        assertions: &[
            LogAssertion {
                needle: "╭─",
                present: true,
                message: "expected header marker in output",
            },
            LogAssertion {
                needle: "╰─",
                present: true,
                message: "expected footer marker in output",
            },
            LogAssertion {
                needle: "app#build",
                present: true,
                message: "expected app#build task in output",
            },
            LogAssertion {
                needle: "app#test",
                present: true,
                message: "expected app#test task in output",
            },
        ],
    });
    assert_logs_case(LogsCase {
        scenario: LogsScenario {
            task_json: r#""app#fast":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["fast.txt"],"command":"echo fast > fast.txt"},"app#slow":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["slow.txt"],"command":"sleep 0.2 && echo slow > slow.txt"}"#,
            extra_files: SINGLE_BUILD_FILES,
        },
        run_tasks: &["fast", "slow"],
        args: &["--time-taken", "200"],
        assertions: &[
            LogAssertion {
                needle: "slow",
                present: true,
                message: "expected slow task in output",
            },
            LogAssertion {
                needle: "fast",
                present: false,
                message: "expected no fast task in output",
            },
        ],
    });
}

#[test]
fn logs_package_filter_errors_when_no_packages_match() {
    let temp = setup_logs_workspace(LogsScenario {
        task_json: BUILD_AND_TEST_TASKS,
        extra_files: SINGLE_BUILD_FILES,
    });
    common::run_luchta(&temp, "build").success();

    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("logs")
        .arg("-p")
        .arg("bogus")
        .arg("--workspace-root")
        .arg(temp.path());
    let output = cmd.assert().failure();
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);

    assert!(
        stderr.contains("No packages matched: [bogus]. -p matches package names, not paths."),
        "expected helpful package mismatch error: {stderr}"
    );
}

#[test]
fn logs_shows_metadata_views() {
    let cases = [
        LogsCase {
            scenario: LogsScenario {
                task_json: r#""app#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"cat src.txt > out.txt"}"#,
                extra_files: &[("packages/app/src.txt", "input content\n")],
            },
            run_tasks: &["build"],
            args: &["--show-inputs"],
            assertions: &[
                LogAssertion {
                    needle: "input patterns (declared):",
                    present: true,
                    message: "expected input patterns section with declared marker in output",
                },
                LogAssertion {
                    needle: "inputs:",
                    present: true,
                    message: "expected inputs section in output",
                },
                LogAssertion {
                    needle: "src.txt",
                    present: true,

                    message: "expected input file path in output",
                },
            ],
        },
        LogsCase {
            scenario: LogsScenario {
                task_json: r#""app#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"echo output > out.txt"}"#,
                extra_files: SINGLE_BUILD_FILES,
            },
            run_tasks: &["build"],
            args: &["--show-outputs"],
            assertions: &[
                LogAssertion {
                    needle: "output patterns (declared):",
                    present: true,
                    message: "expected output patterns section with declared marker in output",
                },
                LogAssertion {
                    needle: "outputs:",
                    present: true,
                    message: "expected outputs section in output",
                },
                LogAssertion {
                    needle: "out.txt",
                    present: true,
                    message: "expected output file path in output",
                },
            ],
        },
    ];

    for case in cases {
        assert_logs_case(case);
    }
}

#[test]
fn logs_filters_top_level() {
    let temp = assert_fs::TempDir::new().unwrap();

    common::WorkspaceBuilder {
        yarn_lock: Some(common::YARN1_LOCK_LEFT_PAD_1_0_0),
        task_json: Some(r#""build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"echo pkg > out.txt"}"#),
        script_name: Some("build"),
        extra_files: &[],
    }
    .build(&temp);

    temp.child("packages/app/src.txt")
        .write_str("test\n")
        .unwrap();
    common::run_luchta(&temp, "build").success();

    let stdout = run_logs(&temp, &["-T"]);
    assert!(
        !stdout.contains("app#build"),
        "expected no package tasks in output: {stdout}"
    );
}

#[test]
fn logs_filters_failed() {
    let temp = assert_fs::TempDir::new().unwrap();
    common::setup_pkgbuild_counter_workspace(
        &temp,
        common::YARN1_LOCK_LEFT_PAD_1_0_0,
        r#""app#success":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"echo ok > out.txt"},"app#failure":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["fail-out.txt"],"command":"exit 1"}"#,
        SINGLE_BUILD_FILES,
    );

    let mut cmd = Command::cargo_bin("luchta").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd.arg("run").arg("success").arg("failure");
    cmd.arg("--workspace-root").arg(temp.path());
    let _ = cmd.assert();

    let stdout = run_logs(&temp, &["--failed"]);
    assert!(
        stdout.contains("failure"),
        "expected failure task in output: {stdout}"
    );
    assert!(
        !stdout.contains("success"),
        "expected no success task in output: {stdout}"
    );
}

#[test]
fn logs_shows_notice_for_uncached_tasks() {
    let temp = setup_single_build_workspace();
    let stdout = run_logs(&temp, &[]);
    assert!(
        stdout.contains("no cached output for"),
        "expected no-cache notice in output: {stdout}"
    );
}

#[test]
fn logs_header_contains_start() {
    let temp = setup_single_build_workspace();
    common::run_luchta(&temp, "build").success();

    let stdout = run_logs(&temp, &[]);
    let header = stdout
        .lines()
        .find(|line| line.contains("╭─"))
        .unwrap_or_else(|| panic!("expected header marker: {stdout}"));
    assert!(
        header.contains(" · "),
        "expected start timestamp separator in header: {stdout}"
    );
}

#[test]
fn logs_footer_contains_duration_exit_cache() {
    let temp = setup_single_build_workspace();
    common::run_luchta(&temp, "build").success();

    let stdout = run_logs(&temp, &[]);
    let footer = stdout
        .lines()
        .find(|line| line.contains("╰─"))
        .unwrap_or_else(|| panic!("expected footer marker in output: {stdout}"));
    assert!(
        footer.contains(" · exit "),
        "expected duration + exit delimiter in footer: {stdout}"
    );
    assert!(
        footer.contains("exit 0"),
        "expected exit status in footer: {stdout}"
    );
    assert!(
        footer.contains("cache "),
        "expected cache hash in footer: {stdout}"
    );
}
