#![cfg(feature = "oxc")]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

use rustc_hash::FxHashMap;

use oxc_diagnostics::{reporter::Info, DiagnosticService, Error, Severity};
use oxc_linter::{
    ConfigStore, DisableDirectives, FixKind, Fixer, LintOptions, LintService, LintServiceOptions,
    Linter, Message, OsFileSystem, OxlintSuppressionFileAction, RuntimeFileSystem,
    SuppressionManager, TsGoLintState,
};
use oxc_span::SourceType;

use crate::opts::OxlintOpts;
use crate::suppressions::{FinalizeResult, SUPPRESSIONS_FILENAME};

#[derive(Clone, Debug)]
pub struct WrappedDiagnostic {
    pub severity: Severity,
    pub rule_id: Option<String>,
    pub message: String,
    pub start_line: usize,
    pub start_column: usize,
    pub relative_uri: String,
}

#[derive(Debug)]
pub struct LintFileResult {
    pub path: PathBuf,
    pub active_messages: Vec<Message>,
}

#[derive(Debug)]
pub struct LintRunResult {
    pub files: Vec<LintFileResult>,
    pub finalize: FinalizeResult,
    pub warnings: Vec<String>,
}

pub async fn lint_files(
    cwd: &Path,
    store: ConfigStore,
    files: Vec<PathBuf>,
    opts: OxlintOpts,
) -> Result<LintRunResult, String> {
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || lint_files_blocking(cwd, store, files, opts))
        .await
        .map_err(|error| format!("oxlint worker join error: {error}"))?
}

pub fn type_aware_flags(store: &ConfigStore, opts: &OxlintOpts) -> (bool, bool) {
    let type_check = opts.type_check_only || opts.type_check || store.type_check_enabled();
    let type_aware =
        type_check || opts.type_check_only || opts.type_aware || store.type_aware_enabled();
    (type_aware, type_check)
}

pub fn initial_suppression_action(cwd: &Path, opts: &OxlintOpts) -> OxlintSuppressionFileAction {
    SuppressionManager::load(
        cwd,
        SUPPRESSIONS_FILENAME,
        opts.suppress_all,
        opts.suppression_prune_mode(),
    )
    .file_action
}

fn lint_files_blocking(
    cwd: PathBuf,
    store: ConfigStore,
    files: Vec<PathBuf>,
    opts: OxlintOpts,
) -> Result<LintRunResult, String> {
    let (type_aware, type_check) = type_aware_flags(&store, &opts);
    let type_aware_linter = if type_aware {
        TsGoLintState::try_new(&cwd, store.clone(), FixKind::None)
            .ok()
            .map(|state| {
                state
                    .with_silent(true)
                    .with_type_check(type_check)
                    .with_timings(false)
            })
    } else {
        None
    };

    let linter = Linter::new(LintOptions::default(), store, None);
    let options = LintServiceOptions::new(cwd.clone().into_boxed_path());
    let service = LintService::new(linter, options);
    let os_fs: &(dyn RuntimeFileSystem + Sync + Send) = &OsFileSystem;

    let mut warnings = Vec::new();
    if type_aware && type_aware_linter.is_none() {
        warnings.push(
            "type-aware linting requested but tsgolint unavailable; run `npm i -D oxlint-tsgolint`. Continuing without type-aware rules.".to_owned(),
        );
    }

    let mut manager = SuppressionManager::load(
        &cwd,
        SUPPRESSIONS_FILENAME,
        opts.suppress_all,
        opts.suppression_prune_mode(),
    );
    let diff = manager.build_diff();
    let mut results = Vec::new();

    for file in files {
        let paths = vec![Arc::<OsStr>::from(file.as_os_str())];
        let mut raw_messages = service.run_source(os_fs, paths.clone());
        if let Some(type_aware_linter) = &type_aware_linter {
            let directives: Arc<Mutex<FxHashMap<PathBuf, DisableDirectives>>> =
                Arc::new(Mutex::new(FxHashMap::default()));
            match type_aware_linter.lint_source(&paths, os_fs, directives) {
                Ok(tsgo_messages) => raw_messages.extend(tsgo_messages),
                Err(error) => warnings.push(format!("type-aware lint failed: {error}")),
            }
        }
        let source = std::fs::read_to_string(&file)
            .map_err(|error| format!("failed to read {}: {error}", file.display()))?;
        let source_type = SourceType::from_path(&file).map_err(|error| {
            format!(
                "failed to detect source type for {}: {error}",
                file.display()
            )
        })?;

        let mut active_messages = diff.collect_file(&file, &cwd, raw_messages);
        if opts.fix {
            let fixed = Fixer::new(&source, active_messages, Some(source_type)).fix();
            if fixed.fixed {
                std::fs::write(&file, fixed.fixed_code.as_ref()).map_err(|error| {
                    format!("failed to write fixed {}: {error}", file.display())
                })?;
            }
            active_messages = fixed.messages;
        }

        if active_messages.is_empty() {
            diff.collect_empty_file(&file, &cwd);
        }

        results.push(LintFileResult {
            path: file,
            active_messages,
        });
    }

    let (tx_error, rx_error) = mpsc::channel::<Vec<Error>>();
    manager
        .finalize(diff, &tx_error, &cwd)
        .map_err(|error| error.to_string())?;
    drop(tx_error);
    let diagnostics = rx_error.try_iter().flatten().collect();

    Ok(LintRunResult {
        files: results,
        finalize: FinalizeResult {
            action: manager.file_action.clone(),
            diagnostics,
            suppressions_path: cwd.join(SUPPRESSIONS_FILENAME),
        },
        warnings,
    })
}

pub fn wrap_message(
    cwd: &Path,
    source_path: &Path,
    message: &Message,
) -> Result<WrappedDiagnostic, String> {
    let source = std::fs::read_to_string(source_path)
        .map_err(|error| format!("failed to read {}: {error}", source_path.display()))?;
    let wrapped: Vec<Error> =
        DiagnosticService::wrap_diagnostics(cwd, source_path, &source, vec![message.error.clone()]);
    let error = wrapped.first().ok_or_else(|| {
        format!(
            "oxlint produced no wrapped diagnostic for {}",
            source_path.display()
        )
    })?;
    let info = Info::new(error);
    let severity = error.severity().unwrap_or(info.severity);
    let diagnostic_message = if info.message.is_empty() {
        error.to_string()
    } else {
        info.message.clone()
    };

    Ok(WrappedDiagnostic {
        severity,
        rule_id: info.rule_id.clone(),
        message: diagnostic_message,
        start_line: info.start.line.max(1),
        start_column: info.start.column.max(1),
        relative_uri: source_path
            .strip_prefix(cwd)
            .unwrap_or(source_path)
            .to_string_lossy()
            .replace('\\', "/"),
    })
}

pub fn has_error(findings: &[WrappedDiagnostic]) -> bool {
    findings
        .iter()
        .any(|finding| matches!(finding.severity, Severity::Error))
}
