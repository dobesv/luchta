#![cfg(feature = "oxc")]

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};

use oxc_diagnostics::{reporter::Info, Error, Severity};
use oxc_linter::{
    ConfigStore, FixKind, LintOptions, LintRunner, LintServiceOptions, Linter,
    OxlintSuppressionFileAction, SuppressionManager, TsGoLintState,
};

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
pub struct LintRunResult {
    pub findings: Vec<WrappedDiagnostic>,
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
    let fix_kind = FixKind::None;
    let tsgolint_available = TsGoLintState::try_new(&cwd, store.clone(), fix_kind).is_ok();
    let effective_type_aware = type_aware && tsgolint_available;

    let mut warnings = Vec::new();
    if type_aware && !tsgolint_available {
        warnings.push(
            "type-aware linting requested but tsgolint unavailable; run `npm i -D oxlint-tsgolint`. Continuing without type-aware rules.".to_owned(),
        );
    }

    let linter = Linter::new(LintOptions::default(), store, None).with_fix(fix_kind);
    let options = LintServiceOptions::new(cwd.clone().into_boxed_path());
    let lint_runner = LintRunner::builder(options, linter)
        .with_type_aware(effective_type_aware)
        .with_type_check(type_check)
        .with_silent(false)
        .with_fix_kind(fix_kind)
        .with_timings(false)
        .build()
        .map_err(|error| format!("failed to initialize oxlint runner: {error}"))?;

    let mut manager = SuppressionManager::load(
        &cwd,
        SUPPRESSIONS_FILENAME,
        opts.suppress_all,
        opts.suppression_prune_mode(),
    );
    let diff = manager.build_diff();
    let (tx_error, rx_error) = mpsc::channel::<Vec<Error>>();
    let paths: Vec<Arc<OsStr>> = files
        .iter()
        .map(|path| Arc::<OsStr>::from(path.as_os_str()))
        .collect();
    // `lint_files` consumes and returns the runner (`Result<Self, String>`); we
    // only need it for its diagnostics side effects, so discard the returned value.
    let _consumed_runner = lint_runner
        .lint_files::<false>(&paths, tx_error.clone(), &diff, None)
        .map_err(|error| format!("lint execution failed: {error}"))?;
    manager
        .finalize(diff, &tx_error, &cwd)
        .map_err(|error| error.to_string())?;
    drop(tx_error);

    let mut findings: Vec<WrappedDiagnostic> = rx_error
        .try_iter()
        .flatten()
        .map(|error| wrap_error(&error))
        .collect();
    findings.sort_by(compare_findings);

    Ok(LintRunResult {
        findings,
        finalize: FinalizeResult {
            action: manager.file_action.clone(),
            diagnostics: Vec::new(),
            suppressions_path: cwd.join(SUPPRESSIONS_FILENAME),
        },
        warnings,
    })
}

pub fn wrap_error(error: &Error) -> WrappedDiagnostic {
    let info = Info::new(error);
    let severity = error.severity().unwrap_or(info.severity);
    let diagnostic_message = if info.message.is_empty() {
        error.to_string()
    } else {
        info.message.clone()
    };

    WrappedDiagnostic {
        severity,
        rule_id: info.rule_id.clone(),
        message: diagnostic_message,
        start_line: info.start.line.max(1),
        start_column: info.start.column.max(1),
        relative_uri: info.filename.replace('\\', "/"),
    }
}

fn compare_findings(left: &WrappedDiagnostic, right: &WrappedDiagnostic) -> Ordering {
    left.relative_uri
        .cmp(&right.relative_uri)
        .then_with(|| left.start_line.cmp(&right.start_line))
        .then_with(|| left.start_column.cmp(&right.start_column))
        .then_with(|| left.rule_id.cmp(&right.rule_id))
        .then_with(|| left.message.cmp(&right.message))
}

pub fn has_error(findings: &[WrappedDiagnostic]) -> bool {
    findings
        .iter()
        .any(|finding| matches!(finding.severity, Severity::Error))
}
