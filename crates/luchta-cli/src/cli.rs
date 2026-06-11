use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "luchta")]
#[command(about = "Rust monorepo build orchestration tool")]
pub struct Cli {
    #[arg(long, value_name = "PATH", global = true)]
    pub workspace_root: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Run {
        tasks: Vec<String>,

        /// Print the tasks in the order they would run (grouped into parallel
        /// waves) without executing anything.
        #[arg(long)]
        dry_run: bool,
    },
    Check,
}
