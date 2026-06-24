mod cache_ctx;
mod cache_nonce;
mod cli;
mod config;
mod env_conflict;
mod env_merge;
mod format;
mod logs;
mod memory_pressure;
mod outcome;
mod progress;
mod progress_task_list;
mod reports;
mod rss;
mod run;
mod since;

use clap::Parser;
use cli::{Cli, Commands, OutputMode};
use logs::LogsOptions;
use luchta_engine::{
    DependencyValidationError, ResolveMode, TaskGraph, TaskValidationDiagnostic,
    TaskValidationReason,
};
use miette::{Report, Result};

use crate::outcome::TasksFailed;

#[tokio::main]
async fn main() {
    // Restore the default SIGPIPE disposition. Rust ignores SIGPIPE by default,
    // which turns writes to a closed pipe into `EPIPE` errors that make
    // `println!`/`eprintln!` panic (e.g. `luchta run ... | head`). Resetting it
    // to `SIG_DFL` makes the process terminate quietly on a broken pipe, which
    // is the expected behavior for a CLI that streams task output.
    reset_sigpipe();

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

/// Reset SIGPIPE to its default disposition so broken-pipe writes terminate the
/// process quietly instead of panicking. No-op on non-Unix platforms.
#[cfg(unix)]
fn reset_sigpipe() {
    // SAFETY: installing the default handler for SIGPIPE is async-signal-safe
    // and is called once at startup before any output is produced.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

async fn run(cli: Cli) -> Result<()> {
    let workspace_root = run::resolve_workspace_root(cli.workspace_root)?;

    match cli.command {
        command @ Commands::Run { .. } => run_command(&workspace_root, command).await,
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
            let options = LogsOptions {
                tasks: &tasks,
                packages: &packages,
                top_level,
                time_taken,
                failed,
                show_inputs,
                show_outputs,
                show_cache_nonce,
                files: &files,
            };
            logs::execute_logs(&workspace_root, &options).await
        }
        Commands::Check => {
            // Check mode: a worker `Reject` during resolution is a hard error
            // (surfaced from prepare_workspace); a `Prune` is informational.
            let prepared =
                run::prepare_workspace(&workspace_root, ResolveMode::Check, None).await?;
            prepared.worker_manager.shutdown().await;
            run::report_pruned_tasks(&prepared.pruned);

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

            let env_conflicts = env_conflict::detect_env_conflicts(
                &prepared.env,
                &prepared.workers,
                &prepared.pipeline,
            );
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
    output: OutputMode,
    continue_on_failure: bool,
    thresholds: ThresholdInputs,
    max_weight_cli: Option<String>,
    since: Option<String>,
}

struct ThresholdInputs {
    usage_cli: Option<String>,
    free_cli: Option<String>,
}

fn command_run_args(command: Commands) -> RunArgs {
    match command {
        Commands::Run {
            tasks,
            packages,
            top_level,
            dry_run,
            output,
            mem_usage_threshold,
            max_weight,
            mem_free_threshold,
            since,
            continue_on_failure,
        } => RunArgs {
            tasks,
            packages,
            top_level,
            dry_run,
            output,
            continue_on_failure,
            thresholds: ThresholdInputs {
                usage_cli: mem_usage_threshold,
                free_cli: mem_free_threshold,
            },
            max_weight_cli: max_weight,
            since,
        },
        Commands::Logs { .. } | Commands::Check => unreachable!("checked by caller"),
    }
}

async fn run_command(workspace_root: &std::path::Path, command: Commands) -> Result<()> {
    let args = command_run_args(command);
    if args.tasks.is_empty() {
        return Err(miette::miette!("no tasks specified for run command"));
    }

    let selection = run::TaskSelection {
        requested_tasks: &args.tasks,
        packages: &args.packages,
        top_level: args.top_level,
        since: args.since.as_deref(),
    };
    let memory_pressure = resolve_memory_pressure_config(args.thresholds)?;
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
            output: args.output,
            continue_on_failure: args.continue_on_failure,
            memory_pressure,
            max_weight_override,
        })
        .await
    }
}

fn resolve_memory_pressure_config(
    thresholds: ThresholdInputs,
) -> Result<run::MemoryPressureConfig> {
    Ok(run::MemoryPressureConfig {
        usage: resolve_threshold_spec(
            thresholds.usage_cli.as_deref(),
            "LUCHTA_MEM_USAGE_THRESHOLD",
            "mem-usage-threshold",
        )?,
        free: resolve_threshold_spec(
            thresholds.free_cli.as_deref(),
            "LUCHTA_MEM_FREE_THRESHOLD",
            "mem-free-threshold",
        )?,
    })
}
/// Precedence: CLI flag > env var. Returns `None` if neither is set.
/// Returns an error if the value is invalid.
fn resolve_threshold_spec(
    cli_value: Option<&str>,
    env_var: &str,
    flag_name: &str,
) -> Result<Option<crate::memory_pressure::ThresholdSpec>, miette::Report> {
    use crate::memory_pressure::{parse_threshold, ThresholdParseError};

    let raw = cli_value
        .map(|s| s.to_string())
        .or_else(|| std::env::var(env_var).ok().filter(|s| !s.is_empty()));

    match raw {
        None => Ok(None),
        Some(value) => parse_threshold(&value).map(Some).map_err(|e| match e {
            ThresholdParseError::Empty => {
                let source = if cli_value.is_some() {
                    format!("--{flag_name}")
                } else {
                    env_var.to_string()
                };
                miette::miette!("threshold value for {source} cannot be empty")
            }
            ThresholdParseError::InvalidNumber => {
                miette::miette!(
                    "Invalid --{} value '{}': must be a non-negative number or percentage",
                    flag_name,
                    value
                )
            }
            ThresholdParseError::UnknownUnit { unit } => {
                miette::miette!(
                    "Invalid --{} value '{}': unknown unit '{}'. \
                             Use: % (percent), B, K/KiB/KB, M/MiB/MB, G/GiB/GB",
                    flag_name,
                    value,
                    unit
                )
            }
            ThresholdParseError::Overflow => {
                miette::miette!("Invalid --{} value '{}': value too large", flag_name, value)
            }
        }),
    }
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

    #[tokio::test]
    async fn run_command_errors_when_no_tasks_specified() {
        let cli = Cli {
            workspace_root: None,
            command: Commands::Run {
                tasks: Vec::new(),
                packages: Vec::new(),
                top_level: false,
                dry_run: true,
                output: OutputMode::Default,
                mem_usage_threshold: None,
                max_weight: None,
                mem_free_threshold: None,
                since: None,
                continue_on_failure: false,
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
