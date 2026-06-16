mod cache_ctx;
mod cli;
mod config;
mod env_conflict;
mod env_merge;
mod progress;
mod rss;
mod run;

use clap::Parser;
use cli::{Cli, Commands};
use luchta_engine::{
    DependencyValidationError, ResolveMode, TaskGraph, TaskValidationDiagnostic,
    TaskValidationReason,
};
use miette::Result;

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
        Err(err) => {
            eprintln!("{:?}", err);
            1
        }
    };
    std::process::exit(exit_code);
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
        Commands::Run {
            tasks,
            packages,
            top_level,
            dry_run,
            output,
        } => {
            if tasks.is_empty() {
                return Err(miette::miette!("no tasks specified for run command"));
            }
            let selection = run::TaskSelection {
                requested_tasks: &tasks,
                packages: &packages,
                top_level,
            };
            if dry_run {
                run::dry_run_tasks(&workspace_root, &selection).await
            } else {
                run::run_tasks(&workspace_root, &selection, output).await
            }
        }
        Commands::Check => {
            // Check mode: a worker `Reject` during resolution is a hard error
            // (surfaced from prepare_workspace); a `Prune` is informational.
            let prepared = run::prepare_workspace(&workspace_root, ResolveMode::Check).await?;
            prepared.worker_manager.shutdown().await;
            run::report_pruned_tasks(&prepared.pruned);

            // Collect dependency validation diagnostics
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

            // Collect env conflict diagnostics
            let env_conflicts = env_conflict::detect_env_conflicts(
                &prepared.env,
                &prepared.workers,
                &prepared.pipeline,
            );
            let env_diagnostics: Vec<_> = env_conflicts
                .into_iter()
                .map(|conflict| conflict.to_diagnostic())
                .collect();

            // Combine diagnostics; report error if any exist
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::OutputMode;
    use std::collections::BTreeMap;

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

    #[test]
    fn env_conflict_detects_task_conflict() {
        // Test the detect_env_conflicts function directly with a task conflict
        use luchta_types::{EnvSpec, TaskDefinition, TaskName};

        let mut pipeline = std::collections::HashMap::new();
        let mut task_env = BTreeMap::new();
        task_env.insert(
            "CONFLICT_VAR".to_owned(),
            EnvSpec {
                value: Some("explicit".to_owned()),
                default: Some("fallback".to_owned()),
                input: true,
            },
        );
        pipeline.insert(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![],
                weight: 1,
                command: None,
                worker: None,
                cache: None,
                inputs: vec![],
                outputs: vec![],
                env: task_env,
            },
        );

        let conflicts = env_conflict::detect_env_conflicts(
            &BTreeMap::new(),
            &std::collections::HashMap::new(),
            &pipeline,
        );

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].var_name, "CONFLICT_VAR");
        assert_eq!(conflicts[0].scope_label, "task 'build'");
    }

    #[test]
    fn env_conflict_detects_global_conflict() {
        // Test the detect_env_conflicts function directly with a global conflict
        use luchta_types::EnvSpec;

        let mut global_env = BTreeMap::new();
        global_env.insert(
            "GLOBAL_VAR".to_owned(),
            EnvSpec {
                value: Some("explicit".to_owned()),
                default: Some("fallback".to_owned()),
                input: true,
            },
        );

        let conflicts = env_conflict::detect_env_conflicts(
            &global_env,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].var_name, "GLOBAL_VAR");
        assert_eq!(conflicts[0].scope_label, "global");
    }

    #[test]
    fn env_conflict_detects_worker_conflict() {
        // Test the detect_env_conflicts function directly with a worker conflict
        use luchta_types::{EnvSpec, WorkerDefinition};

        let mut workers = std::collections::HashMap::new();
        let mut worker_env = BTreeMap::new();
        worker_env.insert(
            "WORKER_VAR".to_owned(),
            EnvSpec {
                value: Some("explicit".to_owned()),
                default: Some("fallback".to_owned()),
                input: true,
            },
        );
        workers.insert(
            "my-worker".to_owned(),
            WorkerDefinition {
                command: "echo".to_owned(),
                depends_on: vec![],
                env: worker_env,
            },
        );

        let conflicts = env_conflict::detect_env_conflicts(
            &BTreeMap::new(),
            &workers,
            &std::collections::HashMap::new(),
        );

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].var_name, "WORKER_VAR");
        assert_eq!(conflicts[0].scope_label, "worker 'my-worker'");
    }
}
