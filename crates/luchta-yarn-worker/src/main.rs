use std::collections::BTreeSet;

use luchta_worker::{
    run_worker_main, shell_single_quote, ResolveResult, ResolveTask, TaskModification, Worker,
    WorkerRequest, WorkerResponse,
};

struct YarnWorker;

fn resolved_inputs_with_package_json(inputs: Option<&[String]>) -> Vec<String> {
    let mut detected = BTreeSet::from(["package.json".to_owned()]);
    if let Some(inputs) = inputs {
        detected.extend(inputs.iter().cloned());
    }
    detected.into_iter().collect()
}

impl Worker for YarnWorker {
    fn done_response(&self, req: &WorkerRequest, exit_code: i32) -> WorkerResponse {
        WorkerResponse::done_with_outputs(req.id.clone(), exit_code, req.outputs.clone())
    }

    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult {
        let script = req.resolved_script_name();
        if req.scripts.iter().any(|candidate| candidate == script) {
            ResolveResult::modify(TaskModification {
                inputs: Some(resolved_inputs_with_package_json(Some(
                    req.inputs.as_slice(),
                ))),
                ..TaskModification::default()
            })
        } else {
            ResolveResult::prune(Some(format!(
                "script `{script}` not found in package `{}`",
                req.package
            )))
        }
    }

    fn build_command(&self, req: &WorkerRequest) -> String {
        match req.workspace.as_deref() {
            None => req.command.clone(),
            Some("") => format!("yarn {}", req.command),
            Some(workspace) => format!(
                "yarn workspace {} {}",
                shell_single_quote(workspace),
                req.command
            ),
        }
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
    run_worker_main(YarnWorker).await;
}

#[cfg(test)]
mod tests {
    use luchta_worker::{
        shell_single_quote, ResolveMode, ResolveTask, TaskModification, WorkerRequest,
        WorkerResponse,
    };

    use super::{Worker, YarnWorker};

    fn resolve_request(
        name: &str,
        command: &str,
        scripts: &[&str],
        inputs: Option<&[&str]>,
    ) -> ResolveTask {
        ResolveTask {
            id: format!("@repo/app#{name}"),
            name: name.to_owned(),
            command: command.to_owned(),
            package: "@repo/app".to_owned(),
            cwd: Some("packages/app".to_owned()),
            scripts: scripts.iter().map(|script| script.to_string()).collect(),
            inputs: inputs
                .map(|patterns| {
                    patterns
                        .iter()
                        .map(|pattern| (*pattern).to_owned())
                        .collect()
                })
                .unwrap_or_default(),
            mode: ResolveMode::Run,
        }
    }

    #[test]
    fn resolve_accepts_task_whose_name_is_a_declared_script() {
        let result =
            YarnWorker.resolve_task(&resolve_request("build", "", &["build", "test"], None));
        assert_eq!(
            result.decision,
            luchta_worker::ResolveDecision::Modify(TaskModification {
                inputs: Some(vec!["package.json".to_owned()]),
                ..TaskModification::default()
            })
        );
    }

    #[test]
    fn resolve_prunes_task_whose_name_is_absent_from_scripts() {
        let result = YarnWorker.resolve_task(&resolve_request("build", "", &["test"], None));
        match result.decision {
            luchta_worker::ResolveDecision::Prune { reason } => {
                let reason = reason.expect("prune carries a reason");
                assert!(reason.contains("build"), "reason: {reason}");
                assert!(reason.contains("@repo/app"), "reason: {reason}");
            }
            other => panic!("expected Prune, got {other:?}"),
        }
    }

    #[test]
    fn resolve_uses_explicit_command_as_script_name() {
        let accepted =
            YarnWorker.resolve_task(&resolve_request("start", "serve", &["serve"], None));
        assert_eq!(
            accepted.decision,
            luchta_worker::ResolveDecision::Modify(TaskModification {
                inputs: Some(vec!["package.json".to_owned()]),
                ..TaskModification::default()
            })
        );

        let pruned =
            YarnWorker.resolve_task(&resolve_request("serve", "missing", &["serve"], None));
        assert!(matches!(
            pruned.decision,
            luchta_worker::ResolveDecision::Prune { .. }
        ));
    }

    #[test]
    fn resolve_prunes_when_package_declares_no_scripts() {
        let result = YarnWorker.resolve_task(&resolve_request("build", "", &[], None));
        assert!(matches!(
            result.decision,
            luchta_worker::ResolveDecision::Prune { .. }
        ));
    }

    #[test]
    fn resolve_returns_declared_inputs_plus_package_json() {
        let result = YarnWorker.resolve_task(&resolve_request(
            "build",
            "",
            &["build", "test"],
            Some(&["src/**"]),
        ));
        assert_eq!(
            result.decision,
            luchta_worker::ResolveDecision::Modify(TaskModification {
                inputs: Some(vec!["package.json".to_owned(), "src/**".to_owned()]),
                ..TaskModification::default()
            })
        );
    }

    #[test]
    fn build_command_keeps_raw_command_when_workspace_missing() {
        assert_eq!(
            YarnWorker.build_command(&WorkerRequest::new("job", "echo hello")),
            "echo hello"
        );
    }

    #[test]
    fn build_command_prefixes_root_workspace_with_yarn() {
        assert_eq!(
            YarnWorker.build_command(
                &WorkerRequest::new("job", "install --mode=skip-build").with_workspace("")
            ),
            "yarn install --mode=skip-build"
        );
    }

    #[test]
    fn build_command_prefixes_named_workspace_with_yarn_workspace() {
        assert_eq!(
            YarnWorker
                .build_command(&WorkerRequest::new("job", "build --flag").with_workspace("a")),
            "yarn workspace 'a' build --flag"
        );
    }

    #[test]
    fn done_response_emits_only_outputs() {
        let response = YarnWorker.done_response(
            &WorkerRequest::new("job", "build")
                .with_inputs(["src/**/*.ts"])
                .with_outputs(["dist/**"]),
            0,
        );

        assert_eq!(
            response,
            WorkerResponse::done_with_outputs("job", 0, Some(vec!["dist/**".to_owned()]),)
        );
    }

    #[test]
    fn resolved_inputs_dedupes_package_json() {
        assert_eq!(
            super::resolved_inputs_with_package_json(Some(&[
                "src/**/*.ts".to_owned(),
                "package.json".to_owned(),
            ])),
            vec!["package.json".to_owned(), "src/**/*.ts".to_owned()]
        );
    }

    #[test]
    fn shell_single_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_single_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn build_command_quotes_workspace_names_with_spaces() {
        assert_eq!(
            YarnWorker.build_command(&WorkerRequest::new("job", "build").with_workspace("my pkg")),
            "yarn workspace 'my pkg' build"
        );
    }

    #[test]
    fn build_command_quotes_workspace_names_with_single_quotes() {
        assert_eq!(
            YarnWorker.build_command(&WorkerRequest::new("job", "build").with_workspace("a'b")),
            r"yarn workspace 'a'\''b' build"
        );
    }
}
