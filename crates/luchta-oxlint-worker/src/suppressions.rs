#![cfg(feature = "oxc")]

use std::path::{Path, PathBuf};

use oxc_diagnostics::Error;
use oxc_linter::OxlintSuppressionFileAction;
use serde_json::Value;

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

fn is_empty_suppressions_value(value: &Value) -> bool {
    match value {
        Value::Object(entries) => entries.values().all(is_empty_suppressions_value),
        _ => false,
    }
}

pub fn remove_empty_suppressions_file(
    path: &Path,
    action: OxlintSuppressionFileAction,
) -> Result<OxlintSuppressionFileAction, String> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(action),
        Err(error) => {
            return Err(format!(
                "failed to read suppressions file {}: {error}",
                path.display()
            ));
        }
    };

    let parsed: Value = serde_json::from_str(&contents).map_err(|error| {
        format!(
            "failed to parse suppressions file {}: {error}",
            path.display()
        )
    })?;

    if !is_empty_suppressions_value(&parsed) {
        return Ok(action);
    }

    match std::fs::remove_file(path) {
        Ok(()) => Ok(OxlintSuppressionFileAction::None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(OxlintSuppressionFileAction::None)
        }
        Err(error) => Err(format!(
            "failed to remove empty suppressions file {}: {error}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use assert_fs::TempDir;
    use oxc_linter::OxlintSuppressionFileAction;

    use super::remove_empty_suppressions_file;

    #[test]
    fn removes_empty_object_file_and_clears_action() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("oxlint-suppressions.json");
        std::fs::write(&path, "{\n  \n}\n").expect("write suppressions");

        let action = remove_empty_suppressions_file(&path, OxlintSuppressionFileAction::Created)
            .expect("remove empty suppressions file");

        assert!(matches!(action, OxlintSuppressionFileAction::None));
        assert!(!path.exists());
    }

    #[test]
    fn removes_nested_empty_object_file_and_clears_action() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("oxlint-suppressions.json");
        std::fs::write(
            &path,
            "{\n  \"a\": {},\n  \"nested\": {\n    \"b\": {}\n  }\n}\n",
        )
        .expect("write suppressions");

        let action = remove_empty_suppressions_file(&path, OxlintSuppressionFileAction::Updated)
            .expect("remove nested empty suppressions file");

        assert!(matches!(action, OxlintSuppressionFileAction::None));
        assert!(!path.exists());
    }

    #[test]
    fn missing_file_keeps_action_unchanged() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("oxlint-suppressions.json");

        let action = remove_empty_suppressions_file(&path, OxlintSuppressionFileAction::Exists)
            .expect("missing suppressions file should not error");

        assert!(matches!(action, OxlintSuppressionFileAction::Exists));
        assert!(!path.exists());
    }

    #[test]
    fn malformed_json_returns_error_and_keeps_file() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("oxlint-suppressions.json");
        std::fs::write(&path, "{ not json").expect("write malformed suppressions");

        let error = remove_empty_suppressions_file(&path, OxlintSuppressionFileAction::Created)
            .expect_err("malformed json should error");

        assert!(error.contains("failed to parse suppressions file"));
        assert!(path.exists());
    }

    #[test]
    fn keeps_non_empty_file_and_action() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("oxlint-suppressions.json");
        std::fs::write(
            &path,
            "{\n  \"src/index.js\": {\n    \"no-debugger\": {\n      \"count\": 1\n    }\n  }\n}\n",
        )
        .expect("write suppressions");

        let action = remove_empty_suppressions_file(&path, OxlintSuppressionFileAction::Updated)
            .expect("keep non-empty suppressions file");

        assert!(matches!(action, OxlintSuppressionFileAction::Updated));
        assert!(path.exists());
    }
}
