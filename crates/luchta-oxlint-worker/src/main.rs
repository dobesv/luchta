#[cfg(feature = "oxc")]
mod config;
#[cfg(feature = "oxc")]
mod lint;
#[cfg(feature = "oxc")]
mod opts;
#[cfg(feature = "oxc")]
mod sarif;
#[cfg(feature = "oxc")]
mod suppressions;

#[cfg(feature = "oxc")]
use std::path::Path;

#[cfg(feature = "oxc")]
use luchta_worker::{
    run_worker_main, InProcessOutcome, JobContext, ResolveResult, ResolveTask, TaskModification,
    Worker, WorkerRequest,
};

#[cfg(feature = "oxc")]
use crate::config::{collect_target_files, discover_config};
#[cfg(feature = "oxc")]
use crate::lint::{has_error, initial_suppression_action, lint_files, wrap_message};
#[cfg(feature = "oxc")]
use crate::opts::OxlintOpts;
#[cfg(feature = "oxc")]
use crate::sarif::build_sarif;
#[cfg(feature = "oxc")]
use crate::suppressions::{
    suppression_exit_code, suppression_log_lines, FinalizeResult, SUPPRESSIONS_FILENAME,
};

#[cfg(feature = "oxc")]
struct OxlintWorker;

#[cfg(feature = "oxc")]
impl Worker for OxlintWorker {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult {
        let inputs = default_inputs();
        let Some(cwd) = req.cwd.as_deref() else {
            return ResolveResult::modify(TaskModification {
                inputs: Some(inputs),
                ..TaskModification::default()
            });
        };

        let cwd = Path::new(cwd);
        // ResolveTask carries command but no env, so resolve can only parse command opts.
        // Run phase merges command with OXLINT_OPTS for backward compatibility.
        let opts = OxlintOpts::from_command(&req.command);
        let loaded = match discover_config(cwd, opts.config.as_deref()) {
            Ok(loaded) => loaded,
            Err(error) => return ResolveResult::reject(error),
        };
        let (files, warnings) =
            match collect_target_files(cwd, &loaded.ignore_patterns, &loaded.ignore_base) {
                Ok(result) => result,
                Err(error) => return ResolveResult::reject(error),
            };
        if !warnings.is_empty() {
            return ResolveResult::reject(warnings.join("; "));
        }

        if files.is_empty() {
            return ResolveResult::prune(Some("no JS/TS source files found for oxlint".to_owned()));
        }

        let action = initial_suppression_action(cwd, &opts);
        let action_logs = suppression_log_lines(&FinalizeResult {
            action,
            diagnostics: Vec::new(),
            suppressions_path: cwd.join(SUPPRESSIONS_FILENAME),
        });
        if !action_logs.is_empty() {
            return ResolveResult::reject(action_logs.join("; "));
        }

        ResolveResult::modify(TaskModification {
            inputs: Some(inputs),
            ..TaskModification::default()
        })
    }

    fn build_command(&self, _req: &WorkerRequest) -> String {
        String::new()
    }

    #[allow(clippy::manual_async_fn)]
    fn run_in_process(
        &self,
        req: &WorkerRequest,
        ctx: &JobContext,
    ) -> impl std::future::Future<Output = InProcessOutcome> + Send {
        async move {
            let Some(cwd) = req.cwd.as_deref() else {
                let _ = ctx
                    .emit_stderr("oxlint worker requires cwd".to_owned())
                    .await;
                return InProcessOutcome::Done {
                    exit_code: 1,
                    outputs: None,
                };
            };

            let cwd = Path::new(cwd);
            let opts = OxlintOpts::from_request(req);
            let loaded = match discover_config(cwd, opts.config.as_deref()) {
                Ok(loaded) => loaded,
                Err(error) => {
                    let _ = ctx.emit_stderr(error).await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            };

            if let Some(path) = &loaded.root_config_path {
                if let Err(error) = ctx
                    .emit_stdout(format!("oxlint config: {}", path.display()))
                    .await
                {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit oxlint log: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            }
            if loaded.saw_only_unsupported_ts_config {
                if let Err(error) = ctx
                    .emit_stderr("TS oxlint config unsupported (JSON/JSONC only)".to_owned())
                    .await
                {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit oxlint log: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            }

            let (files, warnings) =
                match collect_target_files(cwd, &loaded.ignore_patterns, &loaded.ignore_base) {
                    Ok(result) => result,
                    Err(error) => {
                        let _ = ctx.emit_stderr(error).await;
                        return InProcessOutcome::Done {
                            exit_code: 1,
                            outputs: None,
                        };
                    }
                };
            for warning in &loaded.warnings {
                if let Err(error) = ctx.emit_stderr(warning.clone()).await {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit oxlint log: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            }
            for warning in warnings {
                if let Err(error) = ctx.emit_stderr(warning).await {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit oxlint log: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            }

            if files.is_empty() {
                return InProcessOutcome::Done {
                    exit_code: 0,
                    outputs: None,
                };
            }

            let results = match lint_files(cwd, loaded.store, files, opts).await {
                Ok(results) => results,
                Err(error) => {
                    let _ = ctx.emit_stderr(error).await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            };

            for warning in &results.warnings {
                if let Err(error) = ctx.emit_stderr(warning.clone()).await {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit oxlint log: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            }

            let mut findings = Vec::new();
            for result in &results.files {
                for message in &result.active_messages {
                    let wrapped = match wrap_message(cwd, &result.path, message) {
                        Ok(wrapped) => wrapped,
                        Err(error) => {
                            let _ = ctx.emit_stderr(error).await;
                            return InProcessOutcome::Done {
                                exit_code: 1,
                                outputs: None,
                            };
                        }
                    };
                    let line = format!(
                        "{}:{}:{}: {} [{}] {}",
                        wrapped.relative_uri,
                        wrapped.start_line,
                        wrapped.start_column,
                        severity_label(wrapped.severity),
                        wrapped
                            .rule_id
                            .clone()
                            .unwrap_or_else(|| "unknown".to_owned()),
                        wrapped.message
                    );
                    if let Err(error) = ctx.emit_stdout(line).await {
                        let _ = ctx
                            .emit_stderr(format!("failed to emit oxlint log: {error}"))
                            .await;
                        return InProcessOutcome::Done {
                            exit_code: 1,
                            outputs: None,
                        };
                    }
                    findings.push(wrapped);
                }
            }

            for line in suppression_log_lines(&results.finalize) {
                if let Err(error) = ctx.emit_stderr(line).await {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit oxlint log: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            }

            let exit_code = if has_error(&findings) { 1 } else { 0 };
            if !findings.is_empty() {
                let report = match build_sarif(&findings) {
                    Ok(report) => report,
                    Err(error) => {
                        let _ = ctx.emit_stderr(error).await;
                        return InProcessOutcome::Done {
                            exit_code: 1,
                            outputs: None,
                        };
                    }
                };

                if let Err(error) = ctx
                    .emit_report("oxlint.sarif", "application/sarif+json", report)
                    .await
                {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit oxlint SARIF report: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            }

            let final_exit_code = suppression_exit_code(&results.finalize).unwrap_or(exit_code);
            InProcessOutcome::Done {
                exit_code: final_exit_code,
                outputs: None,
            }
        }
    }
}

#[cfg(feature = "oxc")]
fn default_inputs() -> Vec<String> {
    vec![
        "package.json".to_owned(),
        "src/**".to_owned(),
        ".oxlintrc.json".to_owned(),
        ".oxlintrc.jsonc".to_owned(),
        ".gitignore".to_owned(),
        ".ignore".to_owned(),
        ".oxlintignore".to_owned(),
        SUPPRESSIONS_FILENAME.to_owned(),
    ]
}

#[cfg(feature = "oxc")]
fn severity_label(severity: oxc_diagnostics::Severity) -> &'static str {
    match severity {
        oxc_diagnostics::Severity::Error => "error",
        oxc_diagnostics::Severity::Warning => "warning",
        oxc_diagnostics::Severity::Advice => "advice",
    }
}

#[cfg(feature = "oxc")]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    run_worker_main(OxlintWorker).await;
}

#[cfg(not(feature = "oxc"))]
fn main() {
    eprintln!("this binary was built without the 'oxc' feature; the oxlint worker is unavailable");
    std::process::exit(1);
}
