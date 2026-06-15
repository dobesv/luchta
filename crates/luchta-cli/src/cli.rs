use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// How much progress output `luchta run` prints.
///
/// Only two modes exist in v1. JSONL and color output are explicit future work
/// and intentionally absent here.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum OutputMode {
    /// Periodic wave-bucketed progress (every 5s, only when the run exceeds 5s)
    /// plus a final summary line.
    #[default]
    Default,
    /// Only the final summary line; no periodic progress.
    Summary,
}

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

        /// Match the given task names as top-level (workspace-root) tasks
        /// instead of package tasks.
        #[arg(short = 'T', long = "top-level")]
        top_level: bool,

        /// Print the tasks in the order they would run (grouped into parallel
        /// waves) without executing anything.
        #[arg(long)]
        dry_run: bool,

        /// Control how much progress output is printed.
        #[arg(long, value_enum, default_value_t = OutputMode::Default)]
        output: OutputMode,
    },
    Check,
}
