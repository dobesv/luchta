use std::collections::BTreeSet;

use luchta_worker::{
    run_worker_main, shell_single_quote, ResolveResult, ResolveTask, Worker, WorkerRequest,
    WorkerResponse,
};

struct YarnWorker;

fn detected_inputs_with_package_json(inputs: Option<&[String]>) -> Option<Vec<String>> {
    let mut detected = BTreeSet::from(["package.json".to_owned()]);
    if let Some(inputs) = inputs {
        detected.extend(inputs.iter().cloned());
    }
    Some(detected.into_iter().collect())
}

impl Worker for YarnWorker {
    fn done_response(&self, req: &WorkerRequest, exit_code: i32) -> WorkerResponse {
        // Worker-reported inputs replace declared cache inputs for this run, so yarn
        // worker reports full set it wants engine/CLI to persist. Always include
        // package.json so script changes invalidate cache entries.
        WorkerResponse::done_with_io(
            req.id.clone(),
            exit_code,
            detected_inputs_with_package_json(None),
            None,
        )
    }

    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult {
        let script = req.resolved_script_name();
        if req.scripts.iter().any(|candidate| candidate == script) {
            ResolveResult::accept()
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

#[tokio::main]
async fn main() {
    run_worker_main(YarnWorker).await;
}

#[cfg(test)]
mod tests {
    use luchta_worker::{
        shell_single_quote, ResolveDecision, ResolveMode, ResolveTask, WorkerRequest,
        WorkerResponse,
    };

    use super::{Worker, YarnWorker};

    fn resolve_request(name: &str, command: &str, scripts: &[&str]) -> ResolveTask {
        ResolveTask {
            id: format!("@repo/app#{name}"),
            name: name.to_owned(),
            command: command.to_owned(),
            package: "@repo/app".to_owned(),
            cwd: Some("packages/app".to_owned()),
            scripts: scripts.iter().map(|script| script.to_string()).collect(),
            mode: ResolveMode::Run,
        }
    }

    #[test]
    fn resolve_accepts_task_whose_name_is_a_declared_script() {
        let result = YarnWorker.resolve_task(&resolve_request("build", "", &["build", "test"]));
        assert_eq!(result.decision, ResolveDecision::Accept);
    }

    #[test]
    fn resolve_prunes_task_whose_name_is_absent_from_scripts() {
        let result = YarnWorker.resolve_task(&resolve_request("build", "", &["test"]));
        match result.decision {
            ResolveDecision::Prune { reason } => {
                let reason = reason.expect("prune carries a reason");
                assert!(reason.contains("build"), "reason: {reason}");
                assert!(reason.contains("@repo/app"), "reason: {reason}");
            }
            other => panic!("expected Prune, got {other:?}"),
        }
    }

    #[test]
    fn resolve_uses_explicit_command_as_script_name() {
        let accepted = YarnWorker.resolve_task(&resolve_request("start", "serve", &["serve"]));
        assert_eq!(accepted.decision, ResolveDecision::Accept);

        let pruned = YarnWorker.resolve_task(&resolve_request("serve", "missing", &["serve"]));
        assert!(matches!(pruned.decision, ResolveDecision::Prune { .. }));
    }

    #[test]
    fn resolve_prunes_when_package_declares_no_scripts() {
        let result = YarnWorker.resolve_task(&resolve_request("build", "", &[]));
        assert!(matches!(result.decision, ResolveDecision::Prune { .. }));
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
    fn done_response_includes_package_json_input() {
        let response = YarnWorker.done_response(&WorkerRequest::new("job", "build"), 0);

        assert_eq!(
            response,
            WorkerResponse::done_with_io("job", 0, Some(vec!["package.json".to_owned()]), None,)
        );
    }

    #[test]
    fn detected_inputs_dedupes_package_json() {
        assert_eq!(
            super::detected_inputs_with_package_json(Some(&[
                "src/**/*.ts".to_owned(),
                "package.json".to_owned(),
            ])),
            Some(vec!["package.json".to_owned(), "src/**/*.ts".to_owned()])
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
