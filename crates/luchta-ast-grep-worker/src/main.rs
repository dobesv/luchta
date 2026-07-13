mod config;
mod lint;
mod sarif;

use std::path::Path;

use ast_grep_config::Severity;
use luchta_worker::{
    run_worker_main, InProcessOutcome, JobContext, ResolveResult, ResolveTask, TaskModification,
    Worker, WorkerRequest,
};

use crate::config::{collect_source_files, discover_config, DiscoveredConfig};
use crate::lint::scan_files_async;
use crate::sarif::build_sarif;

struct AstGrepWorker;

impl Worker for AstGrepWorker {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult {
        let Some(cwd) = req.cwd.as_deref() else {
            return ResolveResult::reject("ast-grep worker requires cwd");
        };

        let cwd = Path::new(cwd);
        let config = match discover_config(cwd) {
            Ok(Some(config)) => config,
            Ok(None) => {
                return ResolveResult::prune(Some(
                    "no sgconfig.yml found; skipping ast-grep".to_owned(),
                ))
            }
            Err(error) => return ResolveResult::reject(error),
        };
        if config.rule_files.is_empty() {
            return ResolveResult::prune(Some(
                "sgconfig.yml found but no rule files; skipping ast-grep".to_owned(),
            ));
        }

        ResolveResult::modify(TaskModification {
            inputs: Some(declared_inputs(cwd, &config)),
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
                    .emit_stderr("ast-grep worker requires cwd".to_owned())
                    .await;
                return InProcessOutcome::Done {
                    exit_code: 1,
                    outputs: None,
                };
            };

            let cwd = Path::new(cwd);
            let config = match discover_config(cwd) {
                Ok(Some(config)) => config,
                Ok(None) => {
                    let _ = ctx
                        .emit_stderr("no sgconfig.yml found; skipping ast-grep".to_owned())
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 0,
                        outputs: None,
                    };
                }
                Err(error) => {
                    let _ = ctx.emit_stderr(error).await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            };

            if config.rule_files.is_empty() {
                if let Err(error) = ctx
                    .emit_stderr(
                        "sgconfig.yml found but no rule files; skipping ast-grep".to_owned(),
                    )
                    .await
                {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit ast-grep log: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
                return InProcessOutcome::Done {
                    exit_code: 0,
                    outputs: None,
                };
            }

            for warning in &config.warnings {
                eprintln!("{warning}");
                if let Err(error) = ctx.emit_stderr(warning.clone()).await {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit ast-grep warning: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            }

            let files = match collect_source_files(cwd, &config.config_dir, &config.language_globs)
            {
                Ok(files) => files,
                Err(error) => {
                    let _ = ctx.emit_stderr(error).await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            };
            if files.is_empty() {
                return InProcessOutcome::Done {
                    exit_code: 0,
                    outputs: None,
                };
            }

            let findings = match scan_files_async(cwd, &config, files).await {
                Ok(findings) => findings,
                Err(error) => {
                    let _ = ctx.emit_stderr(error).await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            };

            let visible_findings = findings
                .into_iter()
                .filter(|finding| !matches!(finding.severity, Severity::Off))
                .collect::<Vec<_>>();

            for finding in &visible_findings {
                let line = format!(
                    "{}:{}:{}: {} [{}] {}",
                    finding.relative_uri,
                    finding.start_line,
                    finding.start_column,
                    severity_label(&finding.severity),
                    if finding.rule_id.is_empty() {
                        "ast-grep-rule"
                    } else {
                        &finding.rule_id
                    },
                    finding.message
                );
                if let Err(error) = ctx.emit_stdout(line).await {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit ast-grep log: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            }

            if !visible_findings.is_empty() {
                let sarif = match build_sarif(&visible_findings) {
                    Ok(sarif) => sarif,
                    Err(error) => {
                        let _ = ctx.emit_stderr(error).await;
                        return InProcessOutcome::Done {
                            exit_code: 1,
                            outputs: None,
                        };
                    }
                };
                if let Err(error) = ctx
                    .emit_report("ast-grep.sarif", "application/sarif+json", sarif)
                    .await
                {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit ast-grep SARIF report: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            }

            InProcessOutcome::Done {
                exit_code: if visible_findings.is_empty() { 0 } else { 1 },
                outputs: None,
            }
        }
    }
}

fn declared_inputs(cwd: &Path, config: &DiscoveredConfig) -> Vec<String> {
    let mut inputs = vec![
        "**/*.yml".to_owned(),
        "**/*.yaml".to_owned(),
        ".gitignore".to_owned(),
    ];

    if let Some(relative_config) = relative_or_parent_path(cwd, &config.config_path) {
        inputs.push(relative_config);
    } else {
        inputs.push("sgconfig.yml".to_owned());
    }

    for rule_file in &config.rule_files {
        if let Some(relative_rule) = relative_or_parent_path(cwd, rule_file) {
            inputs.push(relative_rule);
        }
    }

    // Worker walks whole task cwd with ignore/gitignore semantics, not only src/**.
    // Declare broad tree inputs so any lintable file under cwd invalidates cache, while
    // still relying on .gitignore and worker-side language filtering to avoid absurd churn.
    inputs.push("**/*".to_owned());

    inputs.sort();
    inputs.dedup();
    inputs
}

fn relative_or_parent_path(cwd: &Path, path: &Path) -> Option<String> {
    if let Ok(relative) = path.strip_prefix(cwd) {
        return Some(normalize_path(relative));
    }

    let cwd_parts = path_components(cwd);
    let path_parts = path_components(path);
    let common_len = cwd_parts
        .iter()
        .zip(&path_parts)
        .take_while(|(left, right)| left == right)
        .count();
    if common_len == 0 {
        return None;
    }

    let mut relative_parts = Vec::new();
    for _ in common_len..cwd_parts.len() {
        relative_parts.push("..".to_owned());
    }
    for component in &path_parts[common_len..] {
        relative_parts.push(component.clone());
    }
    Some(relative_parts.join("/"))
}

fn path_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn severity_label(severity: &Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
        Severity::Hint => "hint",
        Severity::Off => "off",
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    run_worker_main(AstGrepWorker).await;
}

#[cfg(test)]
mod tests {
    use std::fs;

    use std::path::PathBuf;

    use luchta_worker::{ResolveDecision, ResolveMode, ResolveTask, TaskModification, Worker};

    use super::{declared_inputs, relative_or_parent_path, AstGrepWorker};
    use crate::config::discover_config;

    fn resolve_task(cwd: Option<String>) -> ResolveTask {
        ResolveTask {
            id: "resolve".to_owned(),
            name: "lint".to_owned(),
            command: "lint".to_owned(),
            package: "pkg".to_owned(),
            cwd,
            scripts: vec![],
            inputs: vec![],
            mode: ResolveMode::Run,
        }
    }

    #[test]
    fn resolve_prunes_when_no_sgconfig() {
        let temp = tempfile::tempdir().expect("tempdir");

        let result =
            AstGrepWorker.resolve_task(&resolve_task(Some(temp.path().display().to_string())));

        assert_eq!(
            result.decision,
            ResolveDecision::Prune {
                reason: Some("no sgconfig.yml found; skipping ast-grep".to_owned())
            }
        );
    }

    #[test]
    fn resolve_accepts_with_rules() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("rules")).expect("rules");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        fs::write(temp.path().join("rules/no-console.yml"), "id: no-console\n").expect("rule");

        let config = discover_config(temp.path())
            .expect("discover")
            .expect("config present");
        let result =
            AstGrepWorker.resolve_task(&resolve_task(Some(temp.path().display().to_string())));

        assert_eq!(
            result.decision,
            ResolveDecision::Modify(TaskModification {
                inputs: Some(declared_inputs(temp.path(), &config)),
                ..TaskModification::default()
            })
        );
    }

    #[test]
    fn declared_inputs_include_parent_config_and_full_tree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pkg = temp.path().join("packages/app");
        fs::create_dir_all(pkg.join("rules")).expect("rules");
        fs::write(
            temp.path().join("sgconfig.yml"),
            "ruleDirs:\n  - packages/app/rules\n",
        )
        .expect("config");
        fs::write(pkg.join("rules/no-console.yml"), "id: no-console\n").expect("rule");

        let config = discover_config(&pkg)
            .expect("discover")
            .expect("config present");
        let inputs = declared_inputs(&pkg, &config);

        assert!(inputs.contains(&"../../sgconfig.yml".to_owned()));
        assert!(inputs.contains(&"rules/no-console.yml".to_owned()));
        assert!(inputs.contains(&"**/*".to_owned()));
    }

    #[test]
    fn declared_inputs_include_rule_files_outside_task_cwd() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pkg = temp.path().join("packages/app");
        fs::create_dir_all(&pkg).expect("pkg");
        fs::create_dir_all(temp.path().join("rules")).expect("rules");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        fs::write(temp.path().join("rules/shared.yml"), "id: shared\n").expect("rule");

        let config = discover_config(&pkg)
            .expect("discover")
            .expect("config present");
        let inputs = declared_inputs(&pkg, &config);

        assert!(inputs.contains(&"../../sgconfig.yml".to_owned()));
        assert!(inputs.contains(&"../../rules/shared.yml".to_owned()));
    }

    #[test]
    fn relative_or_parent_path_reaches_ancestor_config() {
        let cwd = PathBuf::from("/repo/packages/app");
        let config = PathBuf::from("/repo/sgconfig.yml");

        assert_eq!(
            relative_or_parent_path(&cwd, &config),
            Some("../../sgconfig.yml".to_owned())
        );
    }
}
