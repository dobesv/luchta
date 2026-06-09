mod cli;
mod config;
mod run;

use clap::Parser;
use cli::{Cli, Commands};
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
            // Stub: validate config + graph construction
            let config_path = workspace_root.join("luchta.toml");
            let _config = config::load_config(&config_path)?;
            println!("Configuration valid");
            Ok(())
        }
    }
}
