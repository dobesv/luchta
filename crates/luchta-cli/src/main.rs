mod cli;
mod config;

use clap::Parser;
use cli::{Cli, Commands};
use miette::Result;

fn main() -> Result<()> {
    run(Cli::parse())
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Run { .. } | Commands::Check => {
            let _ = cli.workspace_root;
            println!("not yet implemented");
        }
    }

    Ok(())
}
