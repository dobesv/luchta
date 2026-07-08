#![cfg(feature = "oxc")]

use std::path::PathBuf;

use oxc_diagnostics::Error;
use oxc_linter::OxlintSuppressionFileAction;

pub const SUPPRESSIONS_FILENAME: &str = "oxlint-suppressions.json";

#[derive(Debug)]
pub struct FinalizeResult {
    pub action: OxlintSuppressionFileAction,
    pub diagnostics: Vec<Error>,
    pub suppressions_path: PathBuf,
}

impl FinalizeResult {
    pub fn has_unused_suppressions(&self) -> bool {
        matches!(
            self.action,
            OxlintSuppressionFileAction::HasUnprunedSuppressions
        )
    }
}

pub fn suppression_exit_code(result: &FinalizeResult) -> Option<i32> {
    if result.has_unused_suppressions() {
        Some(1)
    } else {
        None
    }
}

pub fn suppression_log_lines(result: &FinalizeResult) -> Vec<String> {
    let mut lines = Vec::new();
    match &result.action {
        OxlintSuppressionFileAction::Created => {
            lines.push(format!("wrote {}", result.suppressions_path.display()));
        }
        OxlintSuppressionFileAction::Updated => {
            lines.push(format!("updated {}", result.suppressions_path.display()));
        }
        OxlintSuppressionFileAction::HasUnprunedSuppressions => {
            lines.push(
                "unused suppressions detected; rerun with OXLINT_OPTS=--suppress-all or --prune-suppressions"
                    .to_owned(),
            );
        }
        OxlintSuppressionFileAction::Malformed(error)
        | OxlintSuppressionFileAction::UnableToPerformFsOperation(error) => {
            lines.push(error.to_string());
        }
        OxlintSuppressionFileAction::None | OxlintSuppressionFileAction::Exists => {}
    }
    lines.extend(result.diagnostics.iter().map(ToString::to_string));
    lines
}
