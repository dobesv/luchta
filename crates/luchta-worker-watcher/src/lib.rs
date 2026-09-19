//! `luchta-worker-watcher` is a binary middleware for the luchta JSONL-over-stdin/stdout
//! worker protocol. It wraps a delegate worker command, watches file globs, and restarts
//! the delegate when matching changes occur.
//!
//! # Single-worker restart model
//!
//! There is only ever **one worker process at a time**. Upon detecting a watched file change,
//! the watcher:
//! 1. If the current worker is **idle**, terminates it at once and starts the replacement.
//! 2. If it is **busy**, lets it finish its in-flight tasks and exit while **queueing** any new
//!    work. Once the old worker has drained and exited, the replacement starts and the queued
//!    work is replayed to it.
//!
//! In-flight tasks are never abandoned: the engine treats a `done` with a non-zero exit code as
//! a real task failure (no retry), so failing a task on restart would spuriously break the
//! build. Two worker versions are deliberately never run at once — it is low value (a restart
//! only happens when the worker source is edited or rebuilt) and a correctness hazard. An
//! earlier model let multiple draining generations coexist; it leaked one idle generation per
//! restart, because an idle generation was reaped only on a response or stdout close that never
//! arrived.
//!
//! Two safety valves bound pathological cases: restarts are rate-limited (after a burst within a
//! short window each further restart backs off briefly, so a crash loop or noisy watch cannot
//! spawn workers without bound), and a drain that never completes (a wedged worker) is
//! force-terminated after a timeout so queued work cannot block forever. Set
//! `LUCHTA_WORKER_WATCHER_DEBUG` to log worker lifecycle transitions to stderr.
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
