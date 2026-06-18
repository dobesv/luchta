//! Shared-cache output scope classification.
//!
//! Shared cache only stores outputs that remain inside package directory.
//! Outputs crossing into another package are excluded from shared cache but are
//! still valid for local cache. Outputs escaping repository root are a hard
//! security error and must be propagated by caller.

use std::path::{Component, Path, PathBuf};

/// Classification for resolved task outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputScope {
    /// All outputs stay inside package directory.
    InPackage,
    /// At least one output leaves package directory but stays inside repo root.
    CrossPackage,
    /// Conceptual escape class. `classify_outputs` signals this via `Err` so
    /// callers cannot silently ignore repo-root escape.
    Escape,
}

/// Errors from output scope validation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScopeError {
    #[error(
        "output path escapes repository root: repo_root={repo_root:?}, package_dir={package_dir:?}, output={output:?}, normalized_output={normalized_output:?}"
    )]
    PathEscape {
        repo_root: PathBuf,
        package_dir: PathBuf,
        output: PathBuf,
        normalized_output: PathBuf,
    },
}

/// Classifies resolved output paths for shared-cache eligibility.
///
/// Relative outputs are interpreted relative to `package_dir`, matching how
/// resolved outputs are produced elsewhere in cache code.
///
/// Escape is returned as `Err(ScopeError::PathEscape)` rather than
/// `Ok(OutputScope::Escape)` so repo-root escape is a hard-fail at call sites.
pub fn classify_outputs(
    repo_root: &Path,
    package_dir: &Path,
    resolved_outputs: &[PathBuf],
) -> Result<OutputScope, ScopeError> {
    let normalized_repo_root = lexical_normalize(repo_root);
    let normalized_package_dir = lexical_normalize(package_dir);
    let mut scope = OutputScope::InPackage;

    for output in resolved_outputs {
        let normalized_output =
            lexical_normalize(&resolve_output_path(&normalized_package_dir, output));

        if !path_starts_with(&normalized_output, &normalized_repo_root) {
            return Err(ScopeError::PathEscape {
                repo_root: normalized_repo_root.clone(),
                package_dir: normalized_package_dir.clone(),
                output: output.clone(),
                normalized_output,
            });
        }

        if !path_starts_with(&normalized_output, &normalized_package_dir) {
            scope = OutputScope::CrossPackage;
        }
    }

    Ok(scope)
}

fn resolve_output_path(package_dir: &Path, output: &Path) -> PathBuf {
    if output.is_absolute() {
        output.to_path_buf()
    } else {
        package_dir.join(output)
    }
}

// Mirrors `luchta-engine/src/input_expansion.rs` exactly. Duplicated here to
// avoid circular dependency: `luchta-engine` already depends on `luchta-cache`.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                components.clear();
                components.push(Component::Prefix(prefix));
            }
            Component::RootDir => {
                components.clear();
                components.push(Component::RootDir);
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                if matches!(components.last(), Some(Component::Normal(_))) {
                    components.pop();
                    continue;
                }
                components.push(Component::ParentDir);
            }
            Component::Normal(_) => components.push(component),
        }
    }

    components.iter().collect()
}

// Mirrors `luchta-engine/src/input_expansion.rs` exactly. Duplicated here to
// avoid circular dependency: `luchta-engine` already depends on `luchta-cache`.
fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    let path_components: Vec<_> = path.components().collect();
    let prefix_components: Vec<_> = prefix.components().collect();

    if prefix_components.len() > path_components.len() {
        return false;
    }

    path_components
        .iter()
        .take(prefix_components.len())
        .eq(prefix_components.iter())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_outputs_in_package_when_all_outputs_stay_under_package_dir() {
        let repo_root = Path::new("/repo");
        let package_dir = Path::new("/repo/packages/pkg-a");
        let outputs = vec![
            PathBuf::from("dist/out.txt"),
            PathBuf::from("./nested/./artifact.bin"),
            PathBuf::from("sub/../logs/run.log"),
        ];

        let scope = classify_outputs(repo_root, package_dir, &outputs).expect("in-package outputs");

        assert_eq!(scope, OutputScope::InPackage);
    }

    #[test]
    fn classify_outputs_cross_package_for_sibling_package_output() {
        let repo_root = Path::new("/repo");
        let package_dir = Path::new("/repo/packages/pkg-a");
        let outputs = vec![
            PathBuf::from("dist/out.txt"),
            PathBuf::from("../pkg-b/generated.txt"),
        ];

        let scope = classify_outputs(repo_root, package_dir, &outputs)
            .expect("cross-package stays in repo");

        assert_eq!(scope, OutputScope::CrossPackage);
    }

    #[test]
    fn classify_outputs_errors_when_output_escapes_repo_root() {
        let repo_root = Path::new("/repo");
        let package_dir = Path::new("/repo/packages/pkg-a");
        let outputs = vec![PathBuf::from("../../../etc/passwd")];

        let error =
            classify_outputs(repo_root, package_dir, &outputs).expect_err("escape must fail");

        assert_eq!(
            error,
            ScopeError::PathEscape {
                repo_root: PathBuf::from("/repo"),
                package_dir: PathBuf::from("/repo/packages/pkg-a"),
                output: PathBuf::from("../../../etc/passwd"),
                normalized_output: PathBuf::from("/etc/passwd"),
            }
        );
    }

    #[test]
    fn classify_outputs_accepts_parent_segments_that_stay_inside_package_dir() {
        let repo_root = Path::new("/repo");
        let package_dir = Path::new("/repo/packages/pkg-a");
        let outputs = vec![
            PathBuf::from("build/sub/../out.txt"),
            PathBuf::from("./cache/./result.json"),
        ];

        let scope = classify_outputs(repo_root, package_dir, &outputs)
            .expect("parent segments stay inside");

        assert_eq!(scope, OutputScope::InPackage);
    }

    #[test]
    fn classify_outputs_handles_absolute_and_trailing_dot_paths() {
        let repo_root = Path::new("/repo/.");
        let package_dir = Path::new("/repo/packages/pkg-a/.");
        let outputs = vec![
            PathBuf::from("/repo/packages/pkg-a/out/./artifact.txt"),
            PathBuf::from("dist/./"),
        ];

        let scope = classify_outputs(repo_root, package_dir, &outputs)
            .expect("absolute and trailing dot paths");

        assert_eq!(scope, OutputScope::InPackage);
    }
}
