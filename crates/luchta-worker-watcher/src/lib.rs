//! `luchta-worker-watcher` is a binary middleware for the luchta JSONL-over-stdin/stdout
//! worker protocol. It wraps a delegate worker command, watches file globs, and gracefully
//! hot-swaps the delegate when matching changes occur.
//!
//! # Hot-swap and Drain Model
//!
//! Upon detecting a file change, the watcher:
//! 1. Spawns a **new generation** of the delegate worker.
//! 2. Routes all **new inbound work** to this current generation.
//! 3. Allows **prior generations** to drain their in-flight operations.
//! 4. Shuts down old generations once idle (via a ladder: stdin EOF → wait → SIGTERM → wait → SIGKILL).
//!
//! Multiple draining generations can coexist independently; no generation is forced to terminate
//! while it is still processing work.
//!
//! # Usage
//!
//! ```text
//! luchta-worker-watcher --watch <glob>... [--debounce-ms <n>] -- <delegate cmd> [args...]
//! ```
//!
//! # Flags
//!
//! * `--watch <glob>` (repeatable, at least one required): File globs to watch for changes.
//! * `--debounce-ms <n>` (default 300): Debounce window in milliseconds for coalescing rapid file events.
//! * `--`: Separates watcher flags from the delegate command. Everything following `--` is treated
//!   as the command and arguments for the delegate worker.
//!
//! # Critical Contracts
//!
//! 1. **Resolve/Run Affinity is Unsupported**: The watcher routes all new inbound work to the
//!    latest generation and does not pin tasks to specific processes. If a worker requires
//!    `resolve` and `run` to occur on the same process, it is incompatible with this watcher.
//!    This crate is designed to operate behind `luchta-lazy-worker` (where it primarily
//!    receives `Run` messages).
//! 2. **Globs do not respect `.gitignore`**: Watched globs are matched directly against the
//!    filesystem. `.gitignore` files are intentionally ignored, as it is common to watch
//!    build artifacts or other ignored paths.

pub mod cli;
pub mod generation;
pub mod router;
pub mod watch;
