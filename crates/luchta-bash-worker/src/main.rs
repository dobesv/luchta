//! A worker for Luchta that executes arbitrary commands via `sh -c`.
//!
//! This worker receives a `WorkerRequest` and executes its `command` verbatim
//! using `sh -c`. It ignores the `workspace` hint and uses the `cwd` provided
//! in the request.
//!
//! ## Command Validation
//!
//! The bash worker performs validation on the `command` during the task resolution
//! phase (`ResolveTask`):
//!
//! - **Non-blank commands** are accepted.
//! - **Blank or whitespace-only commands** are handled based on the `ResolveMode`:
//!   - In `ResolveMode::Check`, the task is **rejected** (resulting in a hard error
//!     in `luchta check`).
//!   - In `ResolveMode::Run`, the task is **pruned** (resulting in a no-op/skip
//!     at runtime).
//!
//! ## Example Configuration
//!
//! Register the bash worker in your `luchta` configuration (typically in your
//! `luchta.config.ts` or similar):
//!
//! ```json
//! {
//!   "workers": {
//!     "bash": {
//!       "command": "luchta-bash-worker"
//!     }
//!   },
//!   "tasks": {
//!     "custom-task": {
//!       "worker": "bash",
//!       "command": "echo 'Hello from bash!'"
//!     }
//!   }
//! }
//! ```

use luchta_worker::{
    run_worker_main, ResolveMode, ResolveResult, ResolveTask, Worker, WorkerRequest,
};

struct BashWorker;

impl Worker for BashWorker {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult {
        let blank = req.command.trim().is_empty();
        if !blank {
            ResolveResult::accept()
        } else if req.mode == ResolveMode::Check {
            ResolveResult::reject(format!(
                "task `{}` in package `{}` has a blank command; bash worker requires a command",
                req.name, req.package
            ))
        } else {
            ResolveResult::prune(Some(format!(
                "blank command for task `{}`; skipping",
                req.name
            )))
        }
    }

    fn build_command(&self, req: &WorkerRequest) -> String {
        req.command.clone()
    }
}

// A single-threaded runtime is sufficient: the worker only orchestrates async
// I/O (reads JSONL requests, spawns child processes, streams their output) and
// does no CPU-bound work. The default multi-threaded runtime would spawn one
// worker thread per CPU, and each thread reserves an 8 MB stack. With several
// resident workers running at once that committed memory adds up and, on a
// machine already near its memory commit limit, can push process/thread
// creation into transient `EAGAIN` ("Resource temporarily unavailable")
// failures. current_thread keeps the worker's footprint minimal.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    run_worker_main(BashWorker).await;
}

#[cfg(test)]
mod tests {
    use luchta_worker::{ResolveDecision, ResolveMode, ResolveTask, WorkerRequest};

    use super::{BashWorker, Worker};

    fn resolve_request(name: &str, command: &str, mode: ResolveMode) -> ResolveTask {
        ResolveTask {
            id: format!("@repo/app#{name}"),
            name: name.to_owned(),
            command: command.to_owned(),
            package: "@repo/app".to_owned(),
            cwd: Some("packages/app".to_owned()),
            scripts: vec!["ignored".to_owned()],
            mode,
        }
    }

    #[test]
    fn resolve_accepts_non_blank_command() {
        let result =
            BashWorker.resolve_task(&resolve_request("build", "echo hi", ResolveMode::Check));
        assert_eq!(result.decision, ResolveDecision::Accept);
    }

    #[test]
    fn resolve_rejects_blank_command_in_check_mode() {
        let result = BashWorker.resolve_task(&resolve_request("build", "", ResolveMode::Check));
        match result.decision {
            ResolveDecision::Reject { message } => {
                assert!(message.contains("build"), "message: {message}");
                assert!(message.contains("@repo/app"), "message: {message}");
                assert!(message.contains("blank command"), "message: {message}");
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn resolve_prunes_blank_command_in_run_mode() {
        let result = BashWorker.resolve_task(&resolve_request("build", "", ResolveMode::Run));
        match result.decision {
            ResolveDecision::Prune { reason } => {
                let reason = reason.expect("prune carries reason");
                assert!(reason.contains("build"), "reason: {reason}");
                assert!(reason.contains("skipping"), "reason: {reason}");
            }
            other => panic!("expected Prune, got {other:?}"),
        }
    }

    #[test]
    fn resolve_treats_whitespace_only_command_as_blank() {
        let result = BashWorker.resolve_task(&resolve_request(
            "build",
            " 	
 ",
            ResolveMode::Check,
        ));
        assert!(matches!(result.decision, ResolveDecision::Reject { .. }));
    }

    #[test]
    fn build_command_returns_command_verbatim() {
        assert_eq!(
            BashWorker.build_command(&WorkerRequest::new(
                "job-1",
                "printf '%s' \"$0|$1|$2\" _ alpha 'two words'"
            )),
            "printf '%s' \"$0|$1|$2\" _ alpha 'two words'"
        );
    }
}
