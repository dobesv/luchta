//! `cargo xtask quality` fails when a change adds rustqual findings or makes
//! existing ones worse, compared with the change's merge base.
//!
//! rustqual analyses a whole tree. Its own regression gate (`--compare` with
//! `--fail-on-regression`) compares one score for the whole workspace, which
//! a well-factored function added anywhere can lift enough to hide a new
//! problem elsewhere, so this analyses both trees and compares the findings
//! themselves.
//!
//! `cargo xtask rustqual [ARGS]` runs the same pinned rustqual directly, for
//! the details behind a finding.

mod findings;
mod trees;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};

use clap::Args;
use serde::Deserialize;

use crate::{cargo_bin, exit_code_label, git_command, repo_root, run_command};
use findings::{Comparison, Finding, Regression};
use trees::Trees;

/// The crate that pins rustqual's version; see the comment in it.
const RUSTQUAL_MANIFEST: &str = "xtask/rustqual/Cargo.toml";
const CONFIG: &str = "rustqual.toml";

/// Where a change starts when no `--base` is given: its merge base with the
/// remote's default branch.
const DEFAULT_BASE_REFS: [&str; 2] = ["origin/HEAD", "origin/main"];

#[derive(Debug, Args)]
pub struct QualityArgs {
    /// Compare with the merge base of REV [default: origin/HEAD, else origin/main].
    #[arg(long, value_name = "REV")]
    base: Option<String>,
    /// Analyse REV instead of the working tree.
    #[arg(long, value_name = "REV")]
    head: Option<String>,
}

#[derive(Debug, Args)]
pub struct RustqualArgs {
    /// Arguments passed to rustqual unchanged (see `cargo xtask rustqual --help`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<OsString>,
}

pub fn quality(args: QualityArgs) -> ExitCode {
    match try_quality(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn try_quality(args: &QualityArgs) -> Result<(), String> {
    let root = repo_root()?;
    let head = args.head.as_deref();
    let base = merge_base(&root, args.base.as_deref(), head.unwrap_or("HEAD"))?;
    let rustqual = build_rustqual(&root)?;
    let scratch = tempfile::Builder::new()
        .prefix("luchta-quality-")
        .tempdir()
        .map_err(|error| format!("failed to create a scratch directory: {error}"))?;
    let trees = Trees::export(&root, &base, head, scratch.path())?;
    let config = root.join(CONFIG);
    // rustqual analyses on one thread, so the two trees run side by side.
    let base_run = start_analysis(&rustqual, &config, &trees.base)?;
    let head_run = start_analysis(&rustqual, &config, &trees.head)?;
    let old = finish_analysis(base_run)?;
    let new = finish_analysis(head_run)?;
    let renames = trees::renames(&root, &base, head)?;
    let comparison = findings::compare(&old, &new, &renames);
    print_comparison(&comparison, short(&base));
    verdict(comparison.regressions.len())
}

/// `cargo xtask rustqual [ARGS]`: runs the pinned rustqual in the current
/// directory, with the workspace's settings unless ARGS name a config.
pub fn rustqual(args: RustqualArgs) -> ExitCode {
    match try_rustqual(&args.args) {
        // rustqual's exit code says whether it found something (1) or could
        // not read its config (2), so pass it on.
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn try_rustqual(args: &[OsString]) -> Result<u8, String> {
    let root = repo_root()?;
    let mut command = Command::new(build_rustqual(&root)?);
    if !names_config(args) {
        command.arg("--config").arg(root.join(CONFIG));
    }
    let status = run_command(command.args(args), "failed to run rustqual")?;
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}

fn merge_base(root: &Path, base: Option<&str>, head: &str) -> Result<String, String> {
    let output = match base {
        Some(base) => git_output(root, &["merge-base", base, head])?,
        None => default_merge_base(root, head)?,
    };
    Ok(output.trim().to_owned())
}

/// The merge base of `head` with the first of [`DEFAULT_BASE_REFS`] that
/// exists.
fn default_merge_base(root: &Path, head: &str) -> Result<String, String> {
    DEFAULT_BASE_REFS
        .iter()
        .find_map(|candidate| git_output(root, &["merge-base", head, candidate]).ok())
        .ok_or_else(|| format!("no merge base with {DEFAULT_BASE_REFS:?}; pass --base <REV>"))
}

/// Runs git in `dir` and returns its standard output, failing with its
/// standard error when it exits unsuccessfully.
fn git_output(dir: &Path, args: &[&str]) -> Result<String, String> {
    let label = format!("git {}", args.join(" "));
    let output = git_command(dir)
        .args(args)
        .output()
        .map_err(|error| format!("failed to run {label}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{label} exited with {}: {}",
            exit_code_label(output.status.code()),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("{label} output is not UTF-8: {error}"))
}

/// Builds the pinned rustqual and returns its path. It isn't a workspace
/// member, so it gets a target directory of its own. The first build takes
/// about a minute.
fn build_rustqual(root: &Path) -> Result<PathBuf, String> {
    let target_dir = target_directory()?.join("rustqual");
    let mut build = Command::new(cargo_bin());
    build
        .current_dir(root)
        .args([
            "build",
            "--release",
            "--locked",
            "--quiet",
            "--manifest-path",
        ])
        .arg(RUSTQUAL_MANIFEST)
        .arg("--target-dir")
        .arg(&target_dir);
    let status = run_command(&mut build, "failed to run cargo build for rustqual")?;
    if !status.success() {
        return Err(format!(
            "cargo build for rustqual exited with {}",
            exit_code_label(status.code())
        ));
    }
    let binary = format!("rustqual{}", std::env::consts::EXE_SUFFIX);
    Ok(target_dir.join("release").join(binary))
}

/// The workspace's target directory, which honors `CARGO_TARGET_DIR` and
/// `build.target-dir`.
fn target_directory() -> Result<PathBuf, String> {
    #[derive(Deserialize)]
    struct Metadata {
        target_directory: PathBuf,
    }

    let output = Command::new(cargo_bin())
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|error| format!("failed to run cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata exited with {}",
            exit_code_label(output.status.code())
        ));
    }
    serde_json::from_slice::<Metadata>(&output.stdout)
        .map(|metadata| metadata.target_directory)
        .map_err(|error| format!("failed to parse cargo metadata JSON: {error}"))
}

/// Starts rustqual on `tree`. It runs from inside the tree with `.` as its
/// path because it skips any file whose path has a component named `target`
/// or starting with a dot, and the scratch directory's own path may have one.
fn start_analysis(rustqual: &Path, config: &Path, tree: &Path) -> Result<Child, String> {
    let mut command = Command::new(rustqual);
    command
        .current_dir(tree)
        .args([".", "--no-fail", "--format", "ai-json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if config.is_file() {
        command.arg("--config").arg(config);
    }
    command
        .spawn()
        .map_err(|error| format!("failed to start rustqual: {error}"))
}

fn finish_analysis(run: Child) -> Result<Vec<Finding>, String> {
    let output = run
        .wait_with_output()
        .map_err(|error| format!("failed to wait for rustqual: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "rustqual exited with {}: {}",
            exit_code_label(output.status.code()),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    findings::parse_report(&output.stdout)
}

fn names_config(args: &[OsString]) -> bool {
    args.iter().any(|arg| {
        arg.to_str()
            .is_some_and(|arg| arg == "-c" || arg == "--config" || arg.starts_with("--config="))
    })
}

fn verdict(regressions: usize) -> Result<(), String> {
    match regressions {
        0 => Ok(()),
        1 => Err("1 rustqual finding added or made worse".to_string()),
        count => Err(format!("{count} rustqual findings added or made worse")),
    }
}

fn print_comparison(comparison: &Comparison, base: &str) {
    let annotate = std::env::var_os("GITHUB_ACTIONS").is_some_and(|value| value == "true");
    for regression in &comparison.regressions {
        println!("{}", format_regression(regression, annotate));
    }
    println!(
        "rustqual against {base}: {} added or made worse, {} resolved",
        comparison.regressions.len(),
        comparison.resolved
    );
    if !comparison.regressions.is_empty() {
        println!(
            "Fix them, or mark a deliberate exception with `// qual:allow(...)` and a reason \
             (see AGENTS.md). `cargo xtask rustqual <path> --verbose` shows the details."
        );
    }
}

/// One line per regression: a GitHub workflow command, which GitHub shows on
/// the pull request's diff, when `annotate` is set, otherwise `file:line`.
fn format_regression(regression: &Regression, annotate: bool) -> String {
    let finding = &regression.finding;
    let message = describe(regression);
    if annotate {
        format!(
            "::error file={},line={}::{}",
            finding.file,
            finding.line,
            escape_workflow_message(&message)
        )
    } else {
        format!("{}:{}  {message}", finding.file, finding.line)
    }
}

fn describe(regression: &Regression) -> String {
    let finding = &regression.finding;
    let location = Some(&finding.function)
        .filter(|function| !function.is_empty() && !finding.detail.contains(function.as_str()))
        .map(|function| format!(" in {function}"))
        .unwrap_or_default();
    let was = regression
        .was
        .map(|was| format!(" (was {was})"))
        .unwrap_or_default();
    format!("{}: {}{location}{was}", finding.category, finding.detail)
}

fn escape_workflow_message(message: &str) -> String {
    message
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn short(revision: &str) -> &str {
    revision.get(..12).unwrap_or(revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regression(function: &str, detail: &str, was: Option<u64>) -> Regression {
        Regression {
            finding: Finding {
                file: "src/a.rs".to_owned(),
                line: 7,
                category: "long_function".to_owned(),
                function: function.to_owned(),
                detail: detail.to_owned(),
            },
            was,
        }
    }

    #[test]
    fn a_config_named_in_the_arguments_is_kept() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();

        assert!(names_config(&args(&["--config", "other.toml"])));
        assert!(names_config(&args(&["--config=other.toml"])));
        assert!(names_config(&args(&["-c", "other.toml", "src"])));
        assert!(!names_config(&args(&["crates/luchta-cli", "--verbose"])));
    }

    #[test]
    fn workflow_messages_escape_line_breaks() {
        assert_eq!(escape_workflow_message("50% of\nlines"), "50%25 of%0Alines");
    }

    #[test]
    fn a_regression_names_its_function_and_old_metric() {
        let line = format_regression(&regression("run", "70 (max 60)", Some(62)), false);

        assert_eq!(
            line,
            "src/a.rs:7  long_function: 70 (max 60) in run (was 62)"
        );
    }

    #[test]
    fn a_function_named_in_the_detail_is_not_repeated() {
        let line = format_regression(&regression("run", "run: 7 (max 5)", None), false);

        assert_eq!(line, "src/a.rs:7  long_function: run: 7 (max 5)");
    }

    #[test]
    fn regressions_become_annotations_on_github() {
        let line = format_regression(&regression("", "70 (max 60)", None), true);

        assert_eq!(
            line,
            "::error file=src/a.rs,line=7::long_function: 70 (max 60)"
        );
    }

    #[test]
    fn the_verdict_counts_regressions() {
        assert_eq!(verdict(0), Ok(()));
        assert_eq!(
            verdict(1),
            Err("1 rustqual finding added or made worse".to_owned())
        );
        assert_eq!(
            verdict(3),
            Err("3 rustqual findings added or made worse".to_owned())
        );
    }

    #[test]
    fn revisions_are_shortened_for_display() {
        assert_eq!(short("0123456789abcdef"), "0123456789ab");
        assert_eq!(short("HEAD"), "HEAD");
    }

    /// Builds the pinned rustqual on its first run, which takes about a
    /// minute, so a rustqual update that no longer builds, or whose report no
    /// longer parses, fails here.
    #[test]
    fn a_revision_compared_with_itself_passes() {
        let args = QualityArgs {
            base: Some("HEAD".to_owned()),
            head: Some("HEAD".to_owned()),
        };

        assert_eq!(quality(args), ExitCode::SUCCESS);
    }

    #[test]
    fn rustqual_runs_with_the_arguments_given() {
        let args = RustqualArgs {
            args: vec![OsString::from("--version")],
        };

        assert_eq!(rustqual(args), ExitCode::SUCCESS);
    }

    #[test]
    fn the_merge_base_of_head_with_itself_is_head() {
        let root = repo_root().unwrap();
        let head = git_output(&root, &["rev-parse", "HEAD"]).unwrap();

        assert_eq!(
            merge_base(&root, Some("HEAD"), "HEAD").unwrap(),
            head.trim()
        );
    }
}
