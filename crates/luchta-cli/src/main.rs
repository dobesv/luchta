mod await_cmd;
mod build_lock;
mod cache_ctx;
mod cache_nonce;
mod cli;
mod config;
mod dep_outputs;
mod env_conflict;
mod env_merge;
mod format;
mod list;
mod live_cache_state;
mod logs;
mod memory_pressure;
mod outcome;
mod progress;
mod progress_task_list;
mod reports;
mod rss;
mod run;
mod since;
mod watch;
mod why;

use std::path::Path;

use clap::{Parser, ValueEnum};
use cli::{Cli, Commands, OutputMode};
use logs::LogsOptions;
use luchta_engine::{
    DependencyValidationError, ResolveMode, TaskGraph, TaskValidationDiagnostic,
    TaskValidationReason,
};
use miette::IntoDiagnostic;
use miette::{Report, Result};

use crate::outcome::TasksFailed;
use crate::run::setup::{no_cache_env, no_mem_pressure_env};

#[tokio::main]
async fn main() {
    // Leave SIGPIPE ignored and handle broken pipes where they happen. See
    // `install_broken_pipe_guard`.
    install_broken_pipe_guard();

    let result = run(Cli::parse()).await;
    let exit_code = match result {
        Ok(()) => 0,
        Err(err) if is_tasks_failed(&err) => 1,
        Err(err) => {
            eprintln!("{:?}", err);
            1
        }
    };
    std::process::exit(exit_code);
}

fn is_tasks_failed(err: &Report) -> bool {
    err.downcast_ref::<TasksFailed>().is_some()
}

/// Exit quietly when a print to stdout/stderr fails because the reader is
/// gone, so `luchta run ... | head` doesn't spew a panic.
///
/// This used to be done by resetting SIGPIPE to `SIG_DFL`, which is the usual
/// CLI trick but is wrong for a process that also writes to pipes it doesn't
/// own. `SIG_DFL` kills on a broken pipe of *any* fd, and luchta writes task
/// requests to worker stdin (`worker/io_tasks.rs`). During failure teardown a
/// worker can exit before the write lands, and luchta died of signal 13
/// mid-teardown — after printing the failure block, before the summary (#282).
/// `write_worker_request` already handles a failed write by crashing that
/// worker's jobs; the signal just never let it see the error.
///
/// So SIGPIPE stays ignored (Rust's default, which turns those writes into
/// ordinary `EPIPE` errors) and the one case that actually wants to end the
/// process — nobody left reading our own output — is handled here instead.
fn install_broken_pipe_guard() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if is_broken_pipe_panic(info) {
            // Success: the reader closed the pipe on purpose, as `head` does.
            std::process::exit(0);
        }
        default_hook(info);
    }));
}

/// Whether a panic is the standard library failing to write to stdout/stderr
/// because the pipe is closed.
///
/// Both halves are required. The prefix is the exact wording the `print!`
/// family uses when its write fails, and matching it alone would silently
/// turn a genuine write failure (a full disk, say) into exit 0.
fn is_broken_pipe_panic(info: &std::panic::PanicHookInfo<'_>) -> bool {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    is_broken_pipe_message(message)
}

fn is_broken_pipe_message(message: &str) -> bool {
    message.starts_with("failed printing to")
        // `Display` for the error is `strerror`, which can be localised, so
        // accept the raw errno spelling too. EPIPE is 32 on Linux and macOS.
        && (message.contains("Broken pipe") || message.contains("os error 32"))
}

#[cfg(test)]
mod broken_pipe_tests {
    use super::is_broken_pipe_message;

    #[test]
    fn recognizes_a_closed_output_pipe() {
        // The exact wording the `print!` family panics with.
        assert!(is_broken_pipe_message(
            "failed printing to stdout: Broken pipe (os error 32)"
        ));
        assert!(is_broken_pipe_message(
            "failed printing to stderr: Broken pipe (os error 32)"
        ));
        // Same thing under a non-English locale, where `strerror` is
        // translated but the errno spelling is not.
        assert!(is_broken_pipe_message(
            "failed printing to stdout: Tubo roto (os error 32)"
        ));
    }

    #[test]
    fn leaves_every_other_panic_alone() {
        // A write that failed for some other reason must reach the real hook
        // and abort loudly. Exiting 0 here would turn a full disk into a
        // build that looks like it succeeded.
        assert!(!is_broken_pipe_message(
            "failed printing to stdout: No space left on device (os error 28)"
        ));
        // Broken pipe from somewhere that isn't our own output: a worker
        // pipe. That must not take the process down at all -- it's the whole
        // point of #282.
        assert!(!is_broken_pipe_message(
            "called `Result::unwrap()` on an `Err` value: Os { code: 32, kind: BrokenPipe, message: \"Broken pipe\" }"
        ));
        assert!(!is_broken_pipe_message("index out of bounds"));
        assert!(!is_broken_pipe_message(""));
    }
}

async fn run(cli: Cli) -> Result<()> {
    let workspace_root = run::resolve_workspace_root(cli.workspace_root)?;

    match cli.command {
        command @ Commands::Run { .. } => run_command(&workspace_root, command).await,
        command @ Commands::Watch { .. } => watch_command(&workspace_root, command).await,
        Commands::Await {
            tasks,
            packages,
            top_level,
        } => dispatch_await(&workspace_root, tasks, packages, top_level).await,
        Commands::Logs {
            tasks,
            packages,
            top_level,
            time_taken,
            failed,
            show_inputs,
            show_outputs,
            show_cache_nonce,
            files,
        } => {
            let packages = apply_implicit_package(packages, top_level, &workspace_root)?;
            dispatch_logs(
                &workspace_root,
                LogsOptions {
                    tasks: &tasks,
                    packages: &packages,
                    top_level,
                    time_taken,
                    failed,
                    show_inputs,
                    show_outputs,
                    show_cache_nonce,
                    files: &files,
                },
            )
            .await
        }
        Commands::Why {
            tasks,
            packages,
            top_level,
            show_inputs,
            show_outputs,
        } => {
            let packages = apply_implicit_package(packages, top_level, &workspace_root)?;
            dispatch_why(
                &workspace_root,
                why::WhyOptions {
                    tasks: &tasks,
                    packages: &packages,
                    top_level,
                    show_inputs,
                    show_outputs,
                },
            )
            .await
        }
        command @ Commands::List { .. } => dispatch_list(&workspace_root, command).await,
        Commands::Check => dispatch_check(&workspace_root).await,
    }
}

fn apply_implicit_package(
    packages: Vec<String>,
    top_level: bool,
    workspace_root: &Path,
) -> Result<Vec<String>> {
    if top_level || !packages.is_empty() {
        return Ok(packages);
    }

    let cwd = std::env::current_dir().into_diagnostic()?;
    Ok(run::detect_implicit_package(&cwd, workspace_root)
        .map(|package| vec![package])
        .unwrap_or(packages))
}

async fn dispatch_logs(workspace_root: &Path, options: LogsOptions<'_>) -> Result<()> {
    logs::execute_logs(workspace_root, &options).await
}

async fn dispatch_await(
    workspace_root: &Path,
    tasks: Vec<String>,
    packages: Vec<String>,
    top_level: bool,
) -> Result<()> {
    let packages = apply_implicit_package(packages, top_level, workspace_root)?;
    if tasks.is_empty() {
        return Err(miette::miette!("no tasks specified for await command"));
    }
    await_cmd::execute_await(
        workspace_root,
        &await_cmd::AwaitOptions {
            tasks: &tasks,
            packages: &packages,
            top_level,
        },
    )
    .await
}

async fn dispatch_why(workspace_root: &Path, options: why::WhyOptions<'_>) -> Result<()> {
    why::execute_why(workspace_root, &options).await
}

async fn dispatch_list(workspace_root: &Path, command: Commands) -> Result<()> {
    let Commands::List {
        tasks,
        packages,
        top_level,
        since,
        packages_mode,
        json,
    } = command
    else {
        unreachable!("dispatch_list only accepts the list command");
    };
    let packages = apply_implicit_package(packages, top_level, workspace_root)?;
    list::execute_list(
        workspace_root,
        tasks,
        packages,
        top_level,
        since,
        packages_mode,
        json,
    )
    .await
}

async fn dispatch_check(workspace_root: &Path) -> Result<()> {
    // Check mode: a worker `Reject` during resolution is a hard error
    // (surfaced from prepare_workspace); a `Prune` is informational and
    // intentionally not reported — the pruned-task list is noise on large
    // workspaces (see GitHub issue #46).
    let prepared = run::prepare_workspace(workspace_root, ResolveMode::Check, None).await?;
    prepared.worker_manager.shutdown().await;

    let dep_diagnostics = match TaskGraph::validate_tasks_with_pruned(
        &prepared.package_graph,
        &prepared.pipeline,
        &prepared.workers,
        &prepared.pruned_ids,
    ) {
        Ok(()) => Vec::new(),
        Err(DependencyValidationError::InvalidTasks { diagnostics }) => diagnostics
            .into_iter()
            .map(task_validation_diagnostic_report)
            .collect::<Vec<_>>(),
    };

    let env_conflicts =
        env_conflict::detect_env_conflicts(&prepared.env, &prepared.workers, &prepared.pipeline);
    let env_diagnostics: Vec<_> = env_conflicts
        .into_iter()
        .map(|conflict| conflict.to_diagnostic())
        .collect();

    let mut all_diagnostics = dep_diagnostics;
    all_diagnostics.extend(env_diagnostics);

    if all_diagnostics.is_empty() {
        println!("Configuration valid");
        Ok(())
    } else {
        Err(CheckValidationError {
            diagnostics: all_diagnostics,
        }
        .into())
    }
}

fn task_validation_diagnostic_report(diagnostic: TaskValidationDiagnostic) -> miette::Report {
    match diagnostic.reason {
        TaskValidationReason::DeadDependencyReference { dependency, reason } => {
            miette::miette!("{} -> {}: {}", diagnostic.task_id, dependency, reason)
        }
        TaskValidationReason::CommandWithoutWorker => miette::miette!(
            "task '{}' defines a command but no worker; specify a worker to execute it",
            diagnostic.task_id
        ),
        TaskValidationReason::UnknownWorker { worker } => miette::miette!(
            "task '{}' references unknown worker '{}'",
            diagnostic.task_id,
            worker
        ),
        TaskValidationReason::CacheFilesRequireCache => miette::miette!(
            "task '{}' declares cacheFiles but does not enable cache; add cache: {{}}",
            diagnostic.task_id
        ),
        TaskValidationReason::InvalidCacheFilePattern { pattern } => miette::miette!(
            "task '{}' cacheFiles pattern '{}' must be package-relative and may not traverse a parent directory",
            diagnostic.task_id,
            pattern
        ),
        TaskValidationReason::CacheFileOverlap {
            cache_file,
            role,
            pattern,
        } => miette::miette!(
            "task '{}' cacheFiles pattern '{}' overlaps declared {} pattern '{}'",
            diagnostic.task_id,
            cache_file,
            role,
            pattern
        ),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("task validation failed")]
struct CheckValidationError {
    diagnostics: Vec<miette::Report>,
}

impl miette::Diagnostic for CheckValidationError {
    fn related<'a>(&'a self) -> Option<Box<dyn Iterator<Item = &'a dyn miette::Diagnostic> + 'a>> {
        Some(Box::new(self.diagnostics.iter().map(|diagnostic| {
            diagnostic.as_ref() as &dyn miette::Diagnostic
        })))
    }
}

struct RunArgs {
    tasks: Vec<String>,
    packages: Vec<String>,
    top_level: bool,
    dry_run: bool,
    output: Option<OutputMode>,
    no_mem_pressure: bool,
    continue_on_failure: bool,
    no_cache: bool,
    max_weight_cli: Option<String>,
    since: Option<String>,
}

fn command_run_args(command: Commands) -> RunArgs {
    match command {
        Commands::Run {
            tasks,
            packages,
            top_level,
            dry_run,
            output,
            no_mem_pressure,
            max_weight,
            since,
            continue_on_failure,
            no_cache,
        } => RunArgs {
            tasks,
            packages,
            top_level,
            dry_run,
            output,
            no_mem_pressure,
            continue_on_failure,
            no_cache,
            max_weight_cli: max_weight,
            since,
        },
        Commands::Watch { .. }
        | Commands::Await { .. }
        | Commands::Logs { .. }
        | Commands::Why { .. }
        | Commands::List { .. }
        | Commands::Check => unreachable!("checked by caller"),
    }
}

async fn run_command(workspace_root: &Path, command: Commands) -> Result<()> {
    let mut args = command_run_args(command);
    let output = resolve_output_mode(args.output, output_mode_env().as_deref())?;
    validate_mem_pressure_tuning_env()?;
    let memory_pressure_enabled = !(args.no_mem_pressure || no_mem_pressure_env());
    args.no_cache = args.no_cache || no_cache_env();
    args.packages = apply_implicit_package(args.packages, args.top_level, workspace_root)?;
    if args.tasks.is_empty() {
        return Err(miette::miette!("no tasks specified for run command"));
    }

    let selection = run::TaskSelection {
        requested_tasks: &args.tasks,
        packages: &args.packages,
        top_level: args.top_level,
        since: args.since.as_deref(),
    };
    let max_weight_override = resolve_max_weight_override(
        args.max_weight_cli.as_deref(),
        "LUCHTA_MAX_WEIGHT",
        "max-weight",
    )?;

    if args.dry_run {
        run::dry_run_tasks(workspace_root, &selection).await
    } else {
        run::run_tasks(run::RunTasksRequest {
            workspace_root,
            selection: &selection,
            output,
            continue_on_failure: args.continue_on_failure,
            no_cache: args.no_cache,
            memory_pressure_enabled,
            max_weight_override,
        })
        .await
    }
}

async fn watch_command(workspace_root: &Path, command: Commands) -> Result<()> {
    let Commands::Watch {
        tasks,
        packages,
        top_level,
        output,
        no_mem_pressure,
        max_weight,
        continue_on_failure,
        no_cache,
        debounce,
        show_changed_files,
    } = command
    else {
        unreachable!("checked by caller");
    };
    let packages = apply_implicit_package(packages, top_level, workspace_root)?;

    if tasks.is_empty() {
        return Err(miette::miette!("no tasks specified for watch command"));
    }

    let no_cache = no_cache || no_cache_env();
    let output = resolve_output_mode(output, output_mode_env().as_deref())?;
    validate_mem_pressure_tuning_env()?;
    let memory_pressure_enabled = !(no_mem_pressure || no_mem_pressure_env());
    let max_weight_override =
        resolve_max_weight_override(max_weight.as_deref(), "LUCHTA_MAX_WEIGHT", "max-weight")?;

    let Some(session) =
        watch::session::WatchSession::new(workspace_root, max_weight_override).await?
    else {
        return Ok(());
    };
    let (watcher_handle, changes_rx) = watch::watcher::spawn_watcher(workspace_root, debounce)
        .await
        .into_diagnostic()?;
    let selection = watch::driver::OwnedSelection {
        requested_tasks: tasks,
        packages,
        top_level,
    };
    let config = watch::driver::WatchRunConfig {
        output,
        continue_on_failure,
        no_cache,
        memory_pressure_enabled,
        show_changed_files,
    };

    watch::driver::run_watch(watch::driver::WatchInputs {
        session: session.into(),
        watcher_handle,
        changes_rx,
        selection,
        config,
    })
    .await
}

/// Environment variable selecting the progress output mode.
const OUTPUT_MODE_ENV: &str = "LUCHTA_OUTPUT";

fn output_mode_env() -> Option<String> {
    std::env::var(OUTPUT_MODE_ENV).ok()
}

/// Resolves the effective progress output mode.
///
/// The `--output` flag wins; otherwise `LUCHTA_OUTPUT` applies.
fn resolve_output_mode(
    cli_value: Option<OutputMode>,
    env_value: Option<&str>,
) -> Result<OutputMode, miette::Report> {
    if let Some(mode) = cli_value {
        return Ok(mode);
    }

    let Some(raw) = env_value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(OutputMode::default());
    };

    OutputMode::from_str(raw, true).map_err(|_| {
        let known = OutputMode::value_variants()
            .iter()
            .filter_map(|variant| variant.to_possible_value())
            .map(|value| value.get_name().to_owned())
            .collect::<Vec<_>>()
            .join(", ");
        miette::miette!("Invalid {OUTPUT_MODE_ENV} value '{raw}': expected one of {known}")
    })
}

/// Environment variable tuning the Linux PSI threshold (percent of `full
/// avg10`) above which dispatch pauses. See
/// `memory_pressure::tuning::psi_threshold` for parsing and the default.
const MEM_PSI_THRESHOLD_ENV: &str = "LUCHTA_MEM_PSI_THRESHOLD";

/// Environment variable choosing the minimum macOS
/// `kern.memorystatus_vm_pressure_level` at which dispatch pauses. See
/// `memory_pressure::tuning::macos_min_level` for parsing and the default.
const MEM_MACOS_LEVEL_ENV: &str = "LUCHTA_MEM_MACOS_LEVEL";

/// Rejects an unparseable platform memory-pressure tuning env var at startup
/// rather than silently falling back to the default, which would leave a user
/// believing they had tuned something. An unset or blank value passes through
/// untouched; the relevant backend applies its own default.
///
/// Both variables are validated regardless of the host platform: the parsers
/// live in `memory_pressure::tuning`, compiled everywhere, so there is no
/// reason to let a typo through just because it happens to be the "wrong"
/// platform's variable.
fn validate_mem_pressure_tuning_env() -> Result<(), miette::Report> {
    reject_unparseable_env(
        MEM_PSI_THRESHOLD_ENV,
        memory_pressure::tuning::parse_psi_threshold,
        "expected a nonnegative percentage (e.g. 60)",
    )?;
    reject_unparseable_env(
        MEM_MACOS_LEVEL_ENV,
        memory_pressure::tuning::parse_macos_level,
        "expected 'warning' or 'critical'",
    )?;
    Ok(())
}

/// Errors if `env_var` is set to a non-blank value that `parse` rejects.
/// Unset or blank passes through silently — the caller's parser applies its
/// own default for those cases.
fn reject_unparseable_env<T>(
    env_var: &str,
    parse: impl FnOnce(&str) -> Option<T>,
    expected: &str,
) -> Result<(), miette::Report> {
    let Some(raw) = std::env::var(env_var)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };

    if parse(&raw).is_some() {
        return Ok(());
    }

    Err(miette::miette!(
        "Invalid {env_var} value '{raw}': {expected}"
    ))
}

fn resolve_max_weight_override(
    cli_value: Option<&str>,
    env_var: &str,
    flag_name: &str,
) -> Result<Option<u32>, miette::Report> {
    let raw = cli_value
        .map(|s| s.to_string())
        .or_else(|| std::env::var(env_var).ok().filter(|s| !s.is_empty()));

    match raw {
        None => Ok(None),
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(miette::miette!(
                    "Invalid --{} value '{}': must be a positive integer",
                    flag_name,
                    value
                ));
            }

            let parsed = trimmed.parse::<u32>().map_err(|_| {
                miette::miette!(
                    "Invalid --{} value '{}': must be a positive integer",
                    flag_name,
                    value
                )
            })?;

            if parsed == 0 {
                return Err(miette::miette!(
                    "Invalid --{} value '{}': must be greater than 0",
                    flag_name,
                    value
                ));
            }

            Ok(Some(parsed))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::OutputMode;
    use std::sync::Mutex;

    /// Serializes tests that mutate process-wide env vars, so they don't race
    /// each other within one test binary.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Sets or removes an env var for the duration of a test, restoring its
    /// prior value on drop.
    struct EnvVarGuard {
        name: &'static str,
        prior: Option<String>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let prior = std::env::var(name).ok();
            std::env::set_var(name, value);
            Self { name, prior }
        }

        fn remove(name: &'static str) -> Self {
            let prior = std::env::var(name).ok();
            std::env::remove_var(name);
            Self { name, prior }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(ref value) = self.prior {
                std::env::set_var(self.name, value);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }

    #[test]
    fn output_flag_overrides_the_environment_variable() {
        let mode = resolve_output_mode(Some(OutputMode::Summary), Some("plain"))
            .expect("explicit flag resolves");
        assert_eq!(mode, OutputMode::Summary);
    }

    #[test]
    fn output_environment_variable_applies_when_the_flag_is_absent() {
        for value in ["plain", "PLAIN", " Plain "] {
            let mode = resolve_output_mode(None, Some(value)).expect("env value resolves");
            assert_eq!(mode, OutputMode::Plain, "value: {value:?}");
        }
    }

    #[test]
    fn output_mode_defaults_when_neither_flag_nor_environment_is_set() {
        assert_eq!(
            resolve_output_mode(None, None).expect("unset resolves"),
            OutputMode::Default
        );
        assert_eq!(
            resolve_output_mode(None, Some("   ")).expect("blank env resolves"),
            OutputMode::Default
        );
    }

    #[test]
    fn unrecognized_output_environment_value_is_rejected() {
        let error = resolve_output_mode(None, Some("bogus")).expect_err("invalid env value errors");
        let message = format!("{error}");
        assert!(
            message.contains("LUCHTA_OUTPUT") && message.contains("bogus"),
            "error should name the variable and the bad value: {message}"
        );
    }

    #[test]
    fn mem_pressure_tuning_env_validation_accepts_unset_blank_and_valid_values() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _psi_guard = EnvVarGuard::remove(MEM_PSI_THRESHOLD_ENV);
        let _macos_guard = EnvVarGuard::remove(MEM_MACOS_LEVEL_ENV);
        assert!(validate_mem_pressure_tuning_env().is_ok());

        let _psi_guard = EnvVarGuard::set(MEM_PSI_THRESHOLD_ENV, "  ");
        let _macos_guard = EnvVarGuard::set(MEM_MACOS_LEVEL_ENV, "  ");
        assert!(validate_mem_pressure_tuning_env().is_ok());

        let _psi_guard = EnvVarGuard::set(MEM_PSI_THRESHOLD_ENV, "30");
        let _macos_guard = EnvVarGuard::set(MEM_MACOS_LEVEL_ENV, "warning");
        assert!(validate_mem_pressure_tuning_env().is_ok());
    }

    /// Sets `env_var` to `bad_value` (the other tuning var left unset) and
    /// asserts `validate_mem_pressure_tuning_env` rejects it, naming both the
    /// variable and the bad value. Shared by the PSI-threshold and
    /// macOS-level cases below, which differ only in which variable is set.
    fn assert_tuning_env_rejects(env_var: &'static str, bad_value: &str) {
        let _lock = ENV_LOCK.lock().unwrap();
        let _psi_guard = EnvVarGuard::remove(MEM_PSI_THRESHOLD_ENV);
        let _macos_guard = EnvVarGuard::remove(MEM_MACOS_LEVEL_ENV);
        let _bad_guard = EnvVarGuard::set(env_var, bad_value);

        let error =
            validate_mem_pressure_tuning_env().expect_err("unparseable tuning value must error");
        let message = format!("{error}");
        assert!(
            message.contains(env_var) && message.contains(bad_value),
            "error should name the variable and the bad value: {message}"
        );
    }

    #[test]
    fn mem_pressure_tuning_env_validation_rejects_unparseable_psi_threshold() {
        assert_tuning_env_rejects(MEM_PSI_THRESHOLD_ENV, "bogus");
    }

    #[test]
    fn mem_pressure_tuning_env_validation_rejects_unparseable_macos_level() {
        assert_tuning_env_rejects(MEM_MACOS_LEVEL_ENV, "extreme");
    }

    #[test]
    fn watch_command_parses_plain_output_mode() {
        let cli = Cli::try_parse_from(["luchta", "watch", "build", "--output", "plain"])
            .expect("parse watch with plain output");
        let Commands::Watch { output, .. } = cli.command else {
            panic!("expected watch command");
        };
        assert_eq!(output, Some(OutputMode::Plain));
    }

    #[test]
    fn await_command_parses_task_and_selection_flags() {
        let cli = Cli::try_parse_from([
            "luchta",
            "await",
            "build",
            "test*",
            "-p",
            "app-*",
            "-p",
            "lib",
            "-T",
            "--workspace-root",
            "/workspace",
        ])
        .expect("await arguments should parse");

        assert_eq!(
            cli.workspace_root,
            Some(std::path::PathBuf::from("/workspace"))
        );
        let Commands::Await {
            tasks,
            packages,
            top_level,
        } = cli.command
        else {
            panic!("expected await command");
        };
        assert_eq!(tasks, ["build", "test*"]);
        assert_eq!(packages, ["app-*", "lib"]);
        assert!(top_level);
    }

    #[tokio::test]
    async fn await_command_errors_when_no_tasks_specified() {
        let cli = Cli {
            workspace_root: None,
            command: Commands::Await {
                tasks: Vec::new(),
                packages: Vec::new(),
                top_level: false,
            },
        };

        let error = run(cli).await.expect_err("await without tasks must fail");
        assert!(
            error
                .to_string()
                .contains("no tasks specified for await command"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn run_command_errors_when_no_tasks_specified() {
        let cli = Cli {
            workspace_root: None,
            command: Commands::Run {
                tasks: Vec::new(),
                packages: Vec::new(),
                top_level: false,
                dry_run: true,
                output: None,
                no_mem_pressure: false,
                max_weight: None,
                since: None,
                continue_on_failure: false,
                no_cache: false,
            },
        };

        let error = run(cli).await.expect_err("run without tasks must fail");
        assert!(
            error
                .to_string()
                .contains("no tasks specified for run command"),
            "unexpected error: {error}"
        );
    }
}
