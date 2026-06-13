mod cache_ctx;
mod cli;
mod config;
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
        Commands::Run { tasks, dry_run } => {
            if tasks.is_empty() {
                return Err(miette::miette!("no tasks specified for run command"));
            }
            if dry_run {
                run::dry_run_tasks(&workspace_root, &tasks).await
            } else {
                run::run_tasks(&workspace_root, &tasks).await
            }
        }
        Commands::Check => {
            // Check mode: a worker `Reject` during resolution is a hard error
            // (surfaced from prepare_workspace); a `Prune` is informational.
            let prepared = run::prepare_workspace(&workspace_root, ResolveMode::Check).await?;
            prepared.worker_manager.shutdown().await;
            run::report_pruned_tasks(&prepared.pruned);
            let worker_names = prepared.workers.keys().cloned().collect();

            match TaskGraph::validate_tasks_with_pruned(
                &prepared.package_graph,
                &prepared.pipeline,
                &worker_names,
                &prepared.pruned_ids,
            ) {
                Ok(()) => {
                    println!("Configuration valid");
                    Ok(())
                }
                Err(DependencyValidationError::InvalidTasks { diagnostics }) => {
                    let diagnostics = diagnostics
                        .into_iter()
                        .map(task_validation_diagnostic_report)
                        .collect::<Vec<_>>();
                    Err(CheckValidationError { diagnostics }.into())
                }
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
