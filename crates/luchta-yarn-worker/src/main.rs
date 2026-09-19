use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

use luchta_worker::tokenize::tokenize_command;
use luchta_worker::{
    run_worker_main, shell_single_quote, version_requested, JobSpec, ResolveResult, ResolveTask,
    TaskModification, Worker, WorkerRequest, WorkerResponse,
};
use luchta_yarn_env::YarnEnvComputer;

/// Runs yarn scripts. In direct mode (the default) the script body runs under
/// `bash` with the environment yarn would have injected, computed from the PnP
/// manifest in the worker's working directory. With `--no-direct` every task
/// is wrapped as `yarn workspace <pkg> <command>` as before.
struct YarnWorker {
    direct: Option<Mutex<YarnEnvComputer>>,
}

impl YarnWorker {
    fn direct(project_root: PathBuf) -> Self {
        Self {
            direct: Some(Mutex::new(YarnEnvComputer::new(project_root))),
        }
    }

    fn via_yarn() -> Self {
        Self { direct: None }
    }
}

fn direct_disabled(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--no-direct")
}

/// First token is the yarn script name, the rest are extra args yarn would
/// append to the script body.
fn split_command(command: &str) -> (String, Vec<String>) {
    let mut tokens = tokenize_command(command).into_iter();
    let script = tokens.next().unwrap_or_default();
    (script, tokens.collect())
}

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

    fn build_job(&self, req: &WorkerRequest) -> Result<JobSpec, String> {
        // `workspace` is only a presence check here: both the root ("") and a
        // named workspace are located through `req.cwd`, which the engine
        // already sets to the package directory.
        let (Some(computer), Some(_)) = (&self.direct, req.workspace.as_deref()) else {
            return Ok(JobSpec::shell(self.build_command(req), req.env.clone()));
        };
        let workspace_dir = PathBuf::from(req.cwd.as_deref().unwrap_or("."));
        let (script, extra_args) = split_command(&req.command);
        let job = computer
            .lock()
            .map_err(|_| "direct yarn execution state is poisoned; restart the worker".to_owned())?
            .direct_job(&workspace_dir, &script, &extra_args, &req.env)
            .map_err(|error| error.to_string())?;
        Ok(JobSpec {
            program: job.program,
            args: job.args,
            env: job.env,
        })
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
    let args: Vec<String> = std::env::args().collect();
    if version_requested(&args, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")) {
        return;
    }
    let worker = if direct_disabled(&args) {
        YarnWorker::via_yarn()
    } else {
        let project_root = std::env::current_dir().unwrap_or_else(|error| {
            eprintln!("luchta-yarn-worker: cannot determine working directory: {error}");
            std::process::exit(1);
        });
        YarnWorker::direct(project_root)
    };
    run_worker_main(worker).await;
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

    /// Asserts `decision` accepted the task, modifying it to declare exactly
    /// `inputs`. Shared by the resolve tests below, which only differ in the
    /// request that produced the decision and the inputs it should carry.
    fn assert_modifies_with_inputs(decision: luchta_worker::ResolveDecision, inputs: &[&str]) {
        assert_eq!(
            decision,
            luchta_worker::ResolveDecision::Modify(TaskModification {
                inputs: Some(inputs.iter().map(|input| (*input).to_owned()).collect()),
                ..TaskModification::default()
            })
        );
    }

    #[test]
    fn resolve_accepts_task_whose_name_is_a_declared_script() {
        let result = YarnWorker::via_yarn().resolve_task(&resolve_request(
            "build",
            "",
            &["build", "test"],
            None,
        ));
        assert_modifies_with_inputs(result.decision, &["package.json"]);
    }

    #[test]
    fn resolve_accepts_command_with_extra_args() {
        let result = YarnWorker::via_yarn().resolve_task(&resolve_request(
            "args",
            "args --flag 'two words'",
            &["args"],
            None,
        ));
        assert_modifies_with_inputs(result.decision, &["package.json"]);
    }

    #[test]
    fn resolve_prunes_task_whose_name_is_absent_from_scripts() {
        let result =
            YarnWorker::via_yarn().resolve_task(&resolve_request("build", "", &["test"], None));
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
        let accepted = YarnWorker::via_yarn().resolve_task(&resolve_request(
            "start",
            "serve",
            &["serve"],
            None,
        ));
        assert_modifies_with_inputs(accepted.decision, &["package.json"]);

        let pruned = YarnWorker::via_yarn().resolve_task(&resolve_request(
            "serve",
            "missing",
            &["serve"],
            None,
        ));
        assert!(matches!(
            pruned.decision,
            luchta_worker::ResolveDecision::Prune { .. }
        ));
    }

    #[test]
    fn resolve_prunes_when_package_declares_no_scripts() {
        let result = YarnWorker::via_yarn().resolve_task(&resolve_request("build", "", &[], None));
        assert!(matches!(
            result.decision,
            luchta_worker::ResolveDecision::Prune { .. }
        ));
    }

    #[test]
    fn resolve_returns_declared_inputs_plus_package_json() {
        let result = YarnWorker::via_yarn().resolve_task(&resolve_request(
            "build",
            "",
            &["build", "test"],
            Some(&["src/**"]),
        ));
        assert_modifies_with_inputs(result.decision, &["package.json", "src/**"]);
    }

    #[test]
    fn build_command_keeps_raw_command_when_workspace_missing() {
        assert_eq!(
            YarnWorker::via_yarn().build_command(&WorkerRequest::new("job", "echo hello")),
            "echo hello"
        );
    }

    #[test]
    fn build_command_prefixes_root_workspace_with_yarn() {
        assert_eq!(
            YarnWorker::via_yarn().build_command(
                &WorkerRequest::new("job", "install --mode=skip-build").with_workspace("")
            ),
            "yarn install --mode=skip-build"
        );
    }

    #[test]
    fn build_command_prefixes_named_workspace_with_yarn_workspace() {
        assert_eq!(
            YarnWorker::via_yarn()
                .build_command(&WorkerRequest::new("job", "build --flag").with_workspace("a")),
            "yarn workspace 'a' build --flag"
        );
    }

    #[test]
    fn done_response_emits_only_outputs() {
        let response = YarnWorker::via_yarn().done_response(
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
            YarnWorker::via_yarn()
                .build_command(&WorkerRequest::new("job", "build").with_workspace("my pkg")),
            "yarn workspace 'my pkg' build"
        );
    }

    #[test]
    fn build_command_quotes_workspace_names_with_single_quotes() {
        assert_eq!(
            YarnWorker::via_yarn()
                .build_command(&WorkerRequest::new("job", "build").with_workspace("a'b")),
            r"yarn workspace 'a'\''b' build"
        );
    }

    #[test]
    fn no_direct_flag_is_recognized() {
        assert!(super::direct_disabled(&[
            "luchta-yarn-worker".to_owned(),
            "--no-direct".to_owned()
        ]));
        assert!(!super::direct_disabled(&["luchta-yarn-worker".to_owned()]));
    }

    #[test]
    fn split_command_separates_script_and_args() {
        assert_eq!(
            super::split_command("test --coverage 'a b'"),
            (
                "test".to_owned(),
                vec!["--coverage".to_owned(), "a b".to_owned()]
            )
        );
        assert_eq!(super::split_command("build"), ("build".to_owned(), vec![]));
    }

    #[test]
    fn build_job_without_workspace_keeps_raw_shell_path() {
        let worker = YarnWorker::direct(std::env::temp_dir());
        let job = worker
            .build_job(&WorkerRequest::new("job", "echo hello"))
            .unwrap();
        assert_eq!(job.program, "sh");
        assert_eq!(job.args, vec!["-c".to_owned(), "echo hello".to_owned()]);
    }

    #[test]
    fn build_job_via_yarn_when_direct_disabled() {
        let worker = YarnWorker::via_yarn();
        let job = worker
            .build_job(&WorkerRequest::new("job", "build").with_workspace("a"))
            .unwrap();
        assert_eq!(
            job.args,
            vec!["-c".to_owned(), "yarn workspace 'a' build".to_owned()]
        );
    }

    /// Builds a direct-mode worker over a manifest-less temp project root and
    /// returns the `build_job` error for a `build` request scoped to
    /// `workspace`, run from `cwd`.
    fn direct_build_job_error(
        workspace: &str,
        cwd: impl FnOnce(&std::path::Path) -> String,
    ) -> String {
        let temp = tempfile::tempdir().unwrap();
        let worker = YarnWorker::direct(temp.path().to_path_buf());
        let cwd = cwd(temp.path());
        worker
            .build_job(
                &WorkerRequest::new("job", "build")
                    .with_workspace(workspace)
                    .with_cwd(cwd),
            )
            .unwrap_err()
    }

    #[test]
    fn build_job_direct_reports_missing_manifest() {
        let error = direct_build_job_error("a", |_root| "packages/a".to_owned());
        assert!(error.contains(".pnp.cjs"), "{error}");
        assert!(error.contains("--no-direct"), "{error}");
    }

    #[test]
    fn build_job_direct_handles_root_workspace() {
        let error = direct_build_job_error("", |root| root.to_string_lossy().into_owned());
        assert!(error.contains(".pnp.cjs"), "{error}");
    }
}
