mod cli;
mod config;
mod run;

use clap::Parser;
use cli::{Cli, Commands};
use luchta_engine::{
    DependencyValidationError, TaskGraph, TaskValidationDiagnostic, TaskValidationReason,
};
use miette::Result;

#[tokio::main]
async fn main() {
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

async fn run(cli: Cli) -> Result<()> {
    let workspace_root = run::resolve_workspace_root(cli.workspace_root)?;

    match cli.command {
        Commands::Run { tasks } => {
            if tasks.is_empty() {
                return Err(miette::miette!("no tasks specified for run command"));
            }
            run::run_tasks(&workspace_root, &tasks).await
        }
        Commands::Check => {
            let prepared = run::prepare_workspace(&workspace_root).await?;
            let worker_names = prepared.workers.keys().cloned().collect();

            match TaskGraph::validate_tasks(
                &prepared.package_graph,
                &prepared.pipeline,
                &worker_names,
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
