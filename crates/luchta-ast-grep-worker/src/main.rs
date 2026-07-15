mod config;
mod lint;
mod sarif;

use std::path::Path;

use ast_grep_config::Severity;
use luchta_worker::{
    run_worker_main, tokenize::tokenize_command, version_requested, InProcessOutcome, JobContext,
    ResolveResult, ResolveTask, TaskModification, Worker, WorkerRequest,
};

use crate::config::{collect_source_files, discover_config, DiscoveredConfig};
use crate::lint::scan_files_async;
use crate::sarif::build_sarif;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AstGrepOpts {
    fix: bool,
}

impl AstGrepOpts {
    fn from_request(req: &WorkerRequest) -> Self {
        let mut tokens = tokenize_command(&req.command);
        if let Some(raw) = req.env.get("AST_GREP_OPTS") {
            tokens.extend(tokenize_command(raw));
        }
        Self::parse_tokens(&tokens)
    }

    fn parse_tokens(tokens: &[String]) -> Self {
        Self {
            fix: tokens.iter().any(|token| token == "--fix"),
        }
    }
}

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
            inputs: Some(resolve_inputs(cwd, &config, &req.inputs)),
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

            let run = match prepare_run(Path::new(cwd), req) {
                Ok(run) => run,
                Err(failure) => return failure.into_outcome(ctx).await,
            };

            if let Err(outcome) = emit_warnings(ctx, &run.config.warnings).await {
                return outcome;
            }
            for warning in &run.config.warnings {
                eprintln!("{warning}");
            }
            if run.files.is_empty() {
                return InProcessOutcome::Done {
                    exit_code: 0,
                    outputs: None,
                };
            }

            let repo_root = std::env::current_dir().unwrap_or_else(|_| run.cwd.clone());
            let scan_result =
                match scan_files_async(&run.cwd, &repo_root, &run.config, run.files, run.opts.fix)
                    .await
                {
                    Ok(findings) => findings,
                    Err(error) => {
                        let _ = ctx.emit_stderr(error).await;
                        return InProcessOutcome::Done {
                            exit_code: 1,
                            outputs: None,
                        };
                    }
                };

            if let Err(outcome) = emit_warnings(ctx, &scan_result.warnings).await {
                return outcome;
            }
            if let Err(outcome) = emit_fixed_files(ctx, &scan_result.fixed_files).await {
                return outcome;
            }
            let visible_findings = visible_findings(scan_result.findings);
            if let Err(outcome) = emit_findings(ctx, &visible_findings).await {
                return outcome;
            }
            if let Err(outcome) = emit_sarif_if_needed(ctx, &visible_findings).await {
                return outcome;
            }

            InProcessOutcome::Done {
                exit_code: if visible_findings.is_empty() { 0 } else { 1 },
                outputs: None,
            }
        }
    }
}

struct PreparedRun {
    cwd: std::path::PathBuf,
    opts: AstGrepOpts,
    config: DiscoveredConfig,
    files: Vec<std::path::PathBuf>,
}

fn prepare_run(cwd: &Path, req: &WorkerRequest) -> Result<PreparedRun, PrepareFailure> {
    let opts = AstGrepOpts::from_request(req);
    let config = match discover_config(cwd) {
        Ok(Some(config)) => config,
        Ok(None) => {
            return Err(PrepareFailure::new(
                0,
                "no sgconfig.yml found; skipping ast-grep".to_owned(),
            ));
        }
        Err(error) => return Err(PrepareFailure::new(1, error)),
    };
    if config.rule_files.is_empty() {
        return Err(PrepareFailure::new(
            0,
            "sgconfig.yml found but no rule files; skipping ast-grep".to_owned(),
        ));
    }
    let files = match collect_source_files(cwd, &config.config_dir, &config.language_globs) {
        Ok(files) => files,
        Err(error) => return Err(PrepareFailure::new(1, error)),
    };

    Ok(PreparedRun {
        cwd: cwd.to_path_buf(),
        opts,
        config,
        files,
    })
}

/// A `prepare_run` failure carrying the diagnostic to surface on stderr before
/// the worker completes.
struct PrepareFailure {
    exit_code: i32,
    line: String,
}

impl PrepareFailure {
    fn new(exit_code: i32, line: String) -> Self {
        Self { exit_code, line }
    }

    async fn into_outcome(self, ctx: &JobContext) -> InProcessOutcome {
        let _ = ctx.emit_stderr(self.line).await;
        InProcessOutcome::Done {
            exit_code: self.exit_code,
            outputs: None,
        }
    }
}

async fn emit_warnings(ctx: &JobContext, warnings: &[String]) -> Result<(), InProcessOutcome> {
    for warning in warnings {
        if let Err(error) = ctx.emit_stderr(warning.clone()).await {
            return Err(
                emit_failure(ctx, format!("failed to emit ast-grep warning: {error}")).await,
            );
        }
    }
    Ok(())
}

async fn emit_fixed_files(
    ctx: &JobContext,
    fixed_files: &[String],
) -> Result<(), InProcessOutcome> {
    for fixed_file in fixed_files {
        if let Err(error) = ctx.emit_stdout(format!("fixed: {fixed_file}")).await {
            return Err(emit_failure(ctx, format!("failed to emit ast-grep log: {error}")).await);
        }
    }
    Ok(())
}

fn visible_findings(findings: Vec<crate::lint::Finding>) -> Vec<crate::lint::Finding> {
    findings
        .into_iter()
        .filter(|finding| !matches!(finding.severity, Severity::Off))
        .collect()
}

async fn emit_findings(
    ctx: &JobContext,
    findings: &[crate::lint::Finding],
) -> Result<(), InProcessOutcome> {
    for finding in findings {
        let line = format_finding_line(finding);
        if let Err(error) = ctx.emit_stdout(line).await {
            return Err(emit_failure(ctx, format!("failed to emit ast-grep log: {error}")).await);
        }
    }
    Ok(())
}

fn format_finding_line(finding: &crate::lint::Finding) -> String {
    format!(
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
    )
}

async fn emit_sarif_if_needed(
    ctx: &JobContext,
    findings: &[crate::lint::Finding],
) -> Result<(), InProcessOutcome> {
    if findings.is_empty() {
        return Ok(());
    }
    let sarif = match build_sarif(findings) {
        Ok(sarif) => sarif,
        Err(error) => return Err(emit_failure(ctx, error).await),
    };
    if let Err(error) = ctx
        .emit_report("ast-grep.sarif", "application/sarif+json", sarif)
        .await
    {
        return Err(emit_failure(
            ctx,
            format!("failed to emit ast-grep SARIF report: {error}"),
        )
        .await);
    }
    Ok(())
}

async fn emit_failure(ctx: &JobContext, message: String) -> InProcessOutcome {
    let _ = ctx.emit_stderr(message).await;
    InProcessOutcome::Done {
        exit_code: 1,
        outputs: None,
    }
}

/// Declare only package-relative worker-owned inputs.
///
/// Shared root `sgconfig.yml` and rule dirs discovered via ancestor walk are intentionally
/// omitted here when they live outside task `cwd`; worker must never synthesize `../` inputs for
/// per-package tasks.
fn declared_inputs(cwd: &Path, config: &DiscoveredConfig) -> Vec<String> {
    let mut inputs = vec![
        "package.json".to_owned(),
        "**/*".to_owned(),
        ".gitignore".to_owned(),
    ];

    if let Some(relative_config) = relative_within_cwd(cwd, &config.config_path) {
        inputs.push(relative_config);
    }

    for rule_file in &config.rule_files {
        if let Some(relative_rule) = relative_within_cwd(cwd, rule_file) {
            inputs.push(relative_rule);
        }
    }

    inputs.sort();
    inputs.dedup();
    inputs
}

/// Engine applies worker input modifications as replacement, not merge.
/// Preserve consumer-declared repo-root `#...` inputs so shared root `sgconfig.yml` and rule-dir
/// cache invalidation survives resolve. Keep filter narrow: package-relative user inputs are
/// already subsumed by worker-owned `**/*` coverage.
fn resolve_inputs(cwd: &Path, config: &DiscoveredConfig, user_inputs: &[String]) -> Vec<String> {
    let mut inputs = declared_inputs(cwd, config);
    inputs.extend(
        user_inputs
            .iter()
            .filter(|input| input.starts_with('#'))
            .cloned(),
    );
    inputs.sort();
    inputs.dedup();
    inputs
}

fn relative_within_cwd(cwd: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(cwd).ok().map(normalize_path)
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
    if version_requested(
        &std::env::args().collect::<Vec<_>>(),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ) {
        return;
    }

    run_worker_main(AstGrepWorker).await;
}

#[cfg(test)]
mod tests {
    use std::fs;

    use luchta_worker::{
        ResolveDecision, ResolveMode, ResolveTask, TaskModification, Worker, WorkerRequest,
    };

    use super::{declared_inputs, resolve_inputs, AstGrepOpts, AstGrepWorker};
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

    fn resolve_task_with_inputs(cwd: Option<String>, inputs: Vec<&str>) -> ResolveTask {
        let mut req = resolve_task(cwd);
        req.inputs = inputs.into_iter().map(str::to_owned).collect();
        req
    }

    #[test]
    fn opts_parse_fix_from_command_and_env() {
        let req = WorkerRequest::new("lint", "lint --fix");
        assert_eq!(AstGrepOpts::from_request(&req), AstGrepOpts { fix: true });

        let mut env = std::collections::HashMap::new();
        env.insert("AST_GREP_OPTS".to_owned(), "--fix".to_owned());
        let req = WorkerRequest::new("lint", "lint").with_env(env);
        assert_eq!(AstGrepOpts::from_request(&req), AstGrepOpts { fix: true });
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
                inputs: Some(resolve_inputs(temp.path(), &config, &[])),
                ..TaskModification::default()
            })
        );
    }

    #[test]
    fn resolve_inputs_preserve_repo_root_hash_inputs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pkg = temp.path().join("packages/app");
        fs::create_dir_all(&pkg).expect("pkg");
        fs::create_dir_all(temp.path().join("rules")).expect("rules");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        fs::write(temp.path().join("rules/shared.yml"), "id: shared\n").expect("rule");

        let config = discover_config(&pkg)
            .expect("discover")
            .expect("config present");
        let inputs = resolve_inputs(
            &pkg,
            &config,
            &[
                "#sgconfig.yml".to_owned(),
                "#etc/ast-grep/rules/**/*.yml".to_owned(),
                "src/**".to_owned(),
            ],
        );

        assert!(inputs.iter().all(|input| !input.starts_with("../")));
        assert!(inputs.contains(&"package.json".to_owned()));
        assert!(inputs.contains(&"**/*".to_owned()));
        assert!(inputs.contains(&".gitignore".to_owned()));
        assert!(inputs.contains(&"#sgconfig.yml".to_owned()));
        assert!(inputs.contains(&"#etc/ast-grep/rules/**/*.yml".to_owned()));
        assert!(!inputs.contains(&"src/**".to_owned()));
    }

    #[test]
    fn resolve_task_preserves_repo_root_hash_inputs() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("rules")).expect("rules");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        fs::write(temp.path().join("rules/no-console.yml"), "id: no-console\n").expect("rule");

        let result = AstGrepWorker.resolve_task(&resolve_task_with_inputs(
            Some(temp.path().display().to_string()),
            vec!["#sgconfig.yml", "#etc/ast-grep/rules/**/*.yml", "src/**"],
        ));

        let ResolveDecision::Modify(modification) = result.decision else {
            panic!("expected modify decision");
        };
        let inputs = modification.inputs.expect("inputs");

        assert!(inputs.iter().all(|input| !input.starts_with("../")));
        assert!(inputs.contains(&"package.json".to_owned()));
        assert!(inputs.contains(&"**/*".to_owned()));
        assert!(inputs.contains(&".gitignore".to_owned()));
        assert!(inputs.contains(&"sgconfig.yml".to_owned()));
        assert!(inputs.contains(&"rules/no-console.yml".to_owned()));
        assert!(inputs.contains(&"#sgconfig.yml".to_owned()));
        assert!(inputs.contains(&"#etc/ast-grep/rules/**/*.yml".to_owned()));
        assert!(!inputs.contains(&"src/**".to_owned()));
    }

    #[test]
    fn declared_inputs_omit_ancestor_config_and_rule_files() {
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

        assert!(inputs.iter().all(|input| !input.starts_with("../")));
        assert!(inputs.contains(&"package.json".to_owned()));
        assert!(inputs.contains(&"**/*".to_owned()));
        assert!(inputs.contains(&".gitignore".to_owned()));
        assert!(!inputs.contains(&"sgconfig.yml".to_owned()));
        assert!(!inputs.contains(&"../../sgconfig.yml".to_owned()));
        assert!(!inputs.contains(&"../../rules/shared.yml".to_owned()));
    }

    #[test]
    fn declared_inputs_include_config_and_rules_within_task_cwd() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("rules/nested")).expect("rules");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        fs::write(temp.path().join("rules/no-console.yml"), "id: no-console\n").expect("rule");
        fs::write(
            temp.path().join("rules/nested/no-debug.yaml"),
            "id: no-debug\n",
        )
        .expect("rule");

        let config = discover_config(temp.path())
            .expect("discover")
            .expect("config present");
        let inputs = declared_inputs(temp.path(), &config);

        assert!(inputs.iter().all(|input| !input.starts_with("../")));
        assert!(inputs.contains(&"package.json".to_owned()));
        assert!(inputs.contains(&"**/*".to_owned()));
        assert!(inputs.contains(&".gitignore".to_owned()));
        assert!(inputs.contains(&"sgconfig.yml".to_owned()));
        assert!(inputs.contains(&"rules/no-console.yml".to_owned()));
        assert!(inputs.contains(&"rules/nested/no-debug.yaml".to_owned()));
    }
}
