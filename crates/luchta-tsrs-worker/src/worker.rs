use std::path::{Path, PathBuf};

use luchta_worker::{
    InProcessOutcome, JobContext, ResolveResult, ResolveTask, Worker, WorkerRequest,
};

use crate::compile::{compile_package, CompileResult};
use crate::{inputs, sarif};

pub struct TsrsWorker {
    repo_root: PathBuf,
}

impl TsrsWorker {
    /// Capture the launch directory, matching the Go worker's Serve entry point.
    pub fn new(repo_root: PathBuf) -> Self {
        Self { repo_root }
    }
}

impl Worker for TsrsWorker {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult {
        inputs::resolve_task(req)
    }

    fn build_command(&self, _req: &WorkerRequest) -> String {
        // Never fall back to spawning the task's command.
        String::new()
    }

    async fn run_in_process(&self, req: &WorkerRequest, ctx: &JobContext) -> InProcessOutcome {
        let Some(cwd) = req.cwd.as_deref().filter(|cwd| !cwd.is_empty()) else {
            let _ = ctx.emit_stderr("luchta-tsrs-worker requires cwd").await;
            return InProcessOutcome::Done {
                exit_code: 1,
                outputs: None,
            };
        };

        // ResolveMode only affects resolution. Like Go, execution always checks
        // and emits according to the package's configs, ignoring command/env.
        let result = compile_package(Path::new(cwd), &self.repo_root).await;
        finish_compile(result, ctx).await
    }
}

pub(crate) async fn finish_compile(
    mut result: CompileResult,
    ctx: &JobContext,
) -> InProcessOutcome {
    for line in result.stderr {
        let _ = ctx.emit_stderr(line).await;
    }
    if !result.findings.is_empty() {
        if let Err(error) = emit_sarif(&result.findings, ctx).await {
            let _ = ctx.emit_stderr(error).await;
            result.exit_code = 1;
        }
    }
    InProcessOutcome::Done {
        exit_code: result.exit_code,
        outputs: (!result.outputs.is_empty()).then_some(result.outputs),
    }
}

async fn emit_sarif(
    findings: &[luchta_worker::sarif::SarifFinding],
    ctx: &JobContext,
) -> Result<(), String> {
    let report = sarif::build_sarif(findings)?;
    ctx.emit_report(sarif::REPORT_FILENAME, sarif::MIME_TYPE, report)
        .await
        .map_err(|error| format!("failed to emit TypeScript SARIF report: {error}"))
}

#[cfg(test)]
mod tests;
