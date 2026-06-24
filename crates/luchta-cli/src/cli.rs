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

        /// Match package NAMEs (not paths); supports glob wildcards. Repeat to target multiple packages.
        #[arg(short = 'p', long = "package")]
        packages: Vec<String>,

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

        /// Pause NEW task dispatch when process-tree RSS exceeds this threshold.
        ///
        /// Accepts percentages like "50%" or absolute values like "4GiB",
        /// "512MiB", "2GB", or bare bytes. Flag overrides
        /// `LUCHTA_MEM_USAGE_THRESHOLD`; otherwise defaults to 50% of total
        /// system memory. In-flight tasks continue until completion.
        #[arg(long, value_name = "BYTES_OR_PERCENT")]
        mem_usage_threshold: Option<String>,

        /// Override maximum cumulative task weight allowed to run at once.
        ///
        /// Flag overrides `LUCHTA_MAX_WEIGHT`; otherwise uses config
        /// `concurrency.maxWeight`, falling back to available parallelism.
        #[arg(long, value_name = "WEIGHT")]
        max_weight: Option<String>,

        /// Pause NEW task dispatch when system available memory drops below this threshold.
        ///
        /// Accepts percentages like "12.5%" or absolute values like "1GiB",
        /// "512MiB", "500MB", or bare bytes. Flag overrides
        /// `LUCHTA_MEM_FREE_THRESHOLD`; otherwise defaults to 1/16 of total
        /// system memory. In-flight tasks continue until completion.
        #[arg(long, value_name = "BYTES_OR_PERCENT")]
        mem_free_threshold: Option<String>,

        /// Only run tasks for packages changed since this git ref (plus their dependents).
        #[arg(long, value_name = "GIT_REF")]
        since: Option<String>,

        /// Continue running independent tasks after a task fails (only transitive dependents are
        /// skipped); exit non-zero if any task failed.
        #[arg(long = "continue")]
        continue_on_failure: bool,
    },
    /// View cached logs and metadata for previously executed tasks.
    Logs {
        /// Task names to match; supports glob wildcards.
        tasks: Vec<String>,

        /// Match package NAMEs (not paths); supports glob wildcards. Repeat to target multiple packages.
        #[arg(short = 'p', long = "package")]
        packages: Vec<String>,

        /// Match the given task names as top-level (workspace-root) tasks
        /// instead of package tasks.
        #[arg(short = 'T', long = "top-level")]
        top_level: bool,

        /// Filter to tasks that took at least this many milliseconds.
        #[arg(long = "time-taken", value_name = "MS")]
        time_taken: Option<u64>,

        /// Filter to tasks that failed (succeeded == false).
        #[arg(long)]
        failed: bool,

        /// Show the stored effective input patterns (globs, marked detected or
        /// declared) plus input file metadata (path, size, mtime, hash) for each task.
        #[arg(long = "show-inputs")]
        show_inputs: bool,

        /// Show the stored effective output patterns (globs, marked detected or
        /// declared) plus output file metadata (path, size, mtime, hash) for each task.
        #[arg(long = "show-outputs")]
        show_outputs: bool,

        /// Show the persisted cache nonce per task.
        #[arg(long = "show-cache-nonce")]
        show_cache_nonce: bool,

        /// Exact names of attached report files to extract verbatim. Repeat to target multiple files.
        #[arg(long = "file", value_name = "NAME")]
        files: Vec<String>,
    },
    Check,
}
