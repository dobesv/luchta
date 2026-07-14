use std::time::Duration;

use luchta_worker::split_delegate_argv;
use thiserror::Error;

/// Parsed CLI arguments for the worker-watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// One or more glob patterns to watch.
    pub watch_globs: Vec<String>,
    /// Debounce duration for file system events.
    pub debounce: Duration,
    /// The delegate command to run (everything after `--`).
    pub delegate_command: Vec<String>,
}

/// Errors from CLI parsing.
#[derive(Debug, Error)]
pub enum CliError {
    /// No `--watch` globs were provided.
    #[error("missing required --watch <glob> (at least one required)")]
    MissingWatch,

    /// No delegate command was provided after `--`.
    #[error("missing delegate command after --")]
    MissingDelegateCommand,

    /// `--debounce-ms` was provided but the value was invalid.
    #[error("invalid --debounce-ms value: {0}")]
    InvalidDebounce(String),

    /// A flag requiring a value was missing one.
    #[error("missing value for {0}")]
    MissingValue(&'static str),

    /// An unknown flag was encountered before `--`.
    #[error("unknown flag: {0}")]
    UnknownFlag(String),
}

/// Parses command-line arguments.
///
/// Accepts the full argv including the program name. Everything after the first
/// `--` is treated as the delegate command and its args. Before `--`, supports:
/// - `--watch <glob>` (repeatable, at least one required)
/// - `--debounce-ms <u64>` (optional, default 300)
pub fn parse<I>(args: I) -> Result<Cli, CliError>
where
    I: IntoIterator<Item = String>,
{
    let all_args: Vec<String> = args.into_iter().collect();
    let split = split_delegate_argv(all_args.iter().skip(1).cloned());

    if split.delegate_command.is_empty() {
        return Err(CliError::MissingDelegateCommand);
    }

    let mut watch_globs = Vec::new();
    let mut debounce = Duration::from_millis(300);

    let mut i = 0;
    while i < split.stage_args.len() {
        match split.stage_args[i].as_str() {
            "--watch" => {
                i += 1;
                let value = split
                    .stage_args
                    .get(i)
                    .ok_or(CliError::MissingValue("--watch"))?;
                watch_globs.push(value.clone());
            }
            "--debounce-ms" => {
                i += 1;
                let value = split
                    .stage_args
                    .get(i)
                    .ok_or(CliError::MissingValue("--debounce-ms"))?;
                let millis = value
                    .parse::<u64>()
                    .map_err(|_| CliError::InvalidDebounce(value.clone()))?;
                debounce = Duration::from_millis(millis);
            }
            arg if arg.starts_with("--") => {
                return Err(CliError::UnknownFlag(arg.to_string()));
            }
            _ => {
                // Ignore positional args before -- for now
            }
        }
        i += 1;
    }

    if watch_globs.is_empty() {
        return Err(CliError::MissingWatch);
    }

    Ok(Cli {
        watch_globs,
        debounce,
        delegate_command: split.delegate_command,
    })
}

/// Returns a usage/help string.
pub fn usage() -> &'static str {
    "Usage: luchta-worker-watcher --watch <glob> [--watch <glob> ...] [--debounce-ms <ms>] -- <delegate command> [args...]"
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case<'a> {
        argv: &'a [&'a str],
        check: fn(CliError),
    }

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    fn parse_ok(parts: &[&str]) -> Cli {
        parse(args(parts)).expect("cli parses")
    }

    fn parse_err(parts: &[&str]) -> CliError {
        parse(args(parts)).expect_err("cli should fail")
    }

    fn assert_missing_watch(error: CliError) {
        assert!(matches!(error, CliError::MissingWatch));
    }

    fn assert_missing_delegate_command(error: CliError) {
        assert!(matches!(error, CliError::MissingDelegateCommand));
    }

    fn assert_invalid_debounce(error: CliError) {
        assert!(matches!(error, CliError::InvalidDebounce(_)));
    }

    fn assert_unknown_flag(error: CliError) {
        assert!(matches!(error, CliError::UnknownFlag(_)));
    }

    fn assert_missing_debounce_value(error: CliError) {
        assert!(matches!(error, CliError::MissingValue("--debounce-ms")));
    }

    fn assert_missing_watch_value(error: CliError) {
        assert!(matches!(error, CliError::MissingValue("--watch")));
    }

    #[test]
    fn valid_parse_single_watch() {
        let cli = parse_ok(&["prog", "--watch", "src/**/*.rs", "--", "cat"]);
        assert_eq!(cli.watch_globs, vec!["src/**/*.rs"]);
        assert_eq!(cli.debounce, Duration::from_millis(300));
        assert_eq!(cli.delegate_command, vec!["cat"]);
    }

    #[test]
    fn valid_parse_multiple_watch() {
        let cli = parse_ok(&[
            "prog",
            "--watch",
            "src/**/*.rs",
            "--watch",
            "dist/**/*.js",
            "--",
            "cat",
            "arg",
        ]);
        assert_eq!(cli.watch_globs, vec!["src/**/*.rs", "dist/**/*.js"]);
        assert_eq!(cli.debounce, Duration::from_millis(300));
        assert_eq!(cli.delegate_command, vec!["cat", "arg"]);
    }

    #[test]
    fn positional_args_before_double_dash_ignored() {
        let cli = parse_ok(&["prog", "foo", "--watch", "src/**/*.rs", "bar", "--", "cat"]);
        assert_eq!(cli.watch_globs, vec!["src/**/*.rs"]);
        assert_eq!(cli.delegate_command, vec!["cat"]);
    }

    #[test]
    fn parse_error_cases() {
        let cases = [
            Case {
                argv: &["prog", "--", "cat"],
                check: assert_missing_watch,
            },
            Case {
                argv: &["prog", "--watch", "x", "--"],
                check: assert_missing_delegate_command,
            },
            Case {
                argv: &["prog", "--watch", "x", "--debounce-ms", "abc", "--", "cat"],
                check: assert_invalid_debounce,
            },
            Case {
                argv: &["prog", "--watch", "x", "--unknown", "--", "cat"],
                check: assert_unknown_flag,
            },
            Case {
                argv: &["prog", "--watch", "x", "--debounce-ms", "--", "cat"],
                check: assert_missing_debounce_value,
            },
            Case {
                argv: &["prog", "--watch", "--", "cat"],
                check: assert_missing_watch_value,
            },
        ];

        for case in cases {
            (case.check)(parse_err(case.argv));
        }
    }
}

#[cfg(test)]
mod version_passthrough_tests {
    use luchta_worker::split_delegate_argv;

    #[test]
    fn version_flag_after_double_dash_stays_with_delegate() {
        let argv = ["--watch", "src/**/*.rs", "--", "node", "--version"]
            .into_iter()
            .map(str::to_owned);
        let split = split_delegate_argv(argv);

        assert_eq!(split.stage_args, vec!["--watch", "src/**/*.rs"]);
        assert_eq!(split.delegate_command, vec!["node", "--version"]);
    }
}
