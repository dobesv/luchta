//! Input-pattern expansion for cross-package input resolution.
//!
//! This module provides the single shared function for expanding input patterns
//! in the context of a package graph. It converts raw pattern strings into
//! `ResolveRequest` tuples, resolving `^`/`^^` prefixes across upstream packages.
//!
//! # Security
//!
//! Path-escape validation is performed on all resolved paths. Patterns that
//! attempt to traverse outside their base directory are rejected.

use std::path::{Component, Path, PathBuf};

use luchta_cache::ResolveRequest;
use luchta_types::{InputPattern, InputSemantics, PackageName, ROOT_PACKAGE_NAME};
use luchta_workspace::PackageGraph;

use crate::task_graph::transitive_upstream_packages;
use thiserror::Error;

/// Errors from input-pattern expansion.
#[derive(Debug, Error)]
pub enum InputExpansionError {
    /// Referenced package not found in workspace graph.
    #[error("unknown package '{package}' in input pattern '{pattern}'")]
    UnknownPackage {
        /// Package name that was not found.
        package: PackageName,
        /// Original input pattern string.
        pattern: String,
    },

    /// Pattern would resolve outside its allowed directory.
    #[error("path escape in input pattern '{pattern}' from package '{source_pkg}': resolved path escapes base directory")]
    PathEscape {
        /// Source package that declared the input.
        source_pkg: PackageName,
        /// Original input pattern string.
        pattern: String,
    },

    /// Invalid input pattern syntax (parse failure).
    #[error("invalid input pattern '{pattern}' from package '{source_pkg}': {reason}")]
    InvalidPattern {
        /// Source package that declared the input.
        source_pkg: PackageName,
        /// Original input pattern string.
        pattern: String,
        /// Parse error reason.
        reason: String,
    },
}

impl InputExpansionError {
    /// Returns the pattern string that caused the error.
    pub fn pattern(&self) -> &str {
        match self {
            Self::UnknownPackage { pattern, .. } => pattern,
            Self::PathEscape { pattern, .. } => pattern,
            Self::InvalidPattern { pattern, .. } => pattern,
        }
    }
}

/// Expands input patterns into resolve requests.
///
/// For each pattern, parses it via `InputPattern::from_str` and expands:
/// - `SamePackage(p)` → one request with source package's directory
/// - `Root(p)` → one request with repo root as base
/// - `Specific(pkg, p)` → one request with named package's directory
/// - `DirectUpstream(p)` → one request per direct upstream package
/// - `TransitiveUpstream(p)` → one request per transitive upstream package
///
/// # Validation
///
/// All resolved paths are validated against path escape attempts. Patterns
/// that would resolve outside their base directory are rejected with
/// `InputExpansionError::PathEscape`.
///
/// # Arguments
///
/// * `patterns` - Raw input pattern strings.
/// * `source_pkg` - Package that declares these inputs.
/// * `graph` - Workspace package graph for dependency traversal.
/// * `repo_root` - Workspace root directory.
///
/// # Returns
///
/// A vector of `ResolveRequest` tuples suitable for passing to
/// `luchta_cache::resolve_inputs_with_semantics`.
pub fn expand_input_patterns(
    patterns: &[String],
    source_pkg: &PackageName,
    graph: &PackageGraph,
    repo_root: &Path,
) -> Result<Vec<ResolveRequest>, InputExpansionError> {
    let direct_upstreams = direct_upstream_packages(graph, source_pkg)?;
    let transitive_upstreams = transitive_upstream_packages(graph, source_pkg).map_err(|_| {
        InputExpansionError::UnknownPackage {
            package: source_pkg.clone(),
            pattern: String::new(),
        }
    })?;
    let ctx = ExpansionCtx {
        source_pkg,
        graph,
        repo_root,
        direct_upstreams: &direct_upstreams,
        transitive_upstreams: &transitive_upstreams,
    };

    let mut requests = Vec::new();

    for pattern in patterns {
        let parsed = parse_input_pattern(pattern, source_pkg)?;
        requests.extend(build_requests_for_pattern(
            &ctx,
            &parsed,
            PatternSpec {
                path: parsed.path(),
                original_pattern: pattern,
            },
        )?);
    }

    Ok(requests)
}

struct ExpansionCtx<'a> {
    source_pkg: &'a PackageName,
    graph: &'a PackageGraph,
    repo_root: &'a Path,
    direct_upstreams: &'a [PackageName],
    transitive_upstreams: &'a [PackageName],
}

#[derive(Clone, Copy)]
struct PatternSpec<'a> {
    path: &'a str,
    original_pattern: &'a str,
}

struct RequestSpec<'a> {
    package: &'a PackageName,
    pattern: PatternSpec<'a>,
    semantics: InputSemantics,
}

impl ExpansionCtx<'_> {
    fn resolve_request(
        &self,
        spec: RequestSpec<'_>,
    ) -> Result<ResolveRequest, InputExpansionError> {
        let base_dir = if spec.package.is_root() {
            self.repo_root.to_path_buf()
        } else {
            package_dir(self.graph, spec.package, spec.pattern.original_pattern)?
        };
        validate_path(
            &base_dir,
            spec.pattern.path,
            self.source_pkg,
            spec.pattern.original_pattern,
        )?;
        Ok(ResolveRequest {
            base_dir,
            pattern: spec.pattern.path.to_string(),
            semantics: spec.semantics,
        })
    }
}

fn parse_input_pattern(
    pattern: &str,
    source_pkg: &PackageName,
) -> Result<InputPattern, InputExpansionError> {
    pattern
        .parse::<InputPattern>()
        .map_err(|e| InputExpansionError::InvalidPattern {
            source_pkg: source_pkg.clone(),
            pattern: pattern.to_string(),
            reason: e.to_string(),
        })
}

fn build_requests_for_pattern(
    ctx: &ExpansionCtx<'_>,
    parsed: &InputPattern,
    pattern: PatternSpec<'_>,
) -> Result<Vec<ResolveRequest>, InputExpansionError> {
    match parsed {
        InputPattern::SamePackage(_) => build_same_package_request(ctx, pattern),
        InputPattern::Root(_) => build_root_request(ctx, pattern),
        InputPattern::Specific(pkg, _) => build_specific_request(ctx, pkg, pattern),
        InputPattern::DirectUpstream(_) => {
            build_upstream_requests(ctx, ctx.direct_upstreams, pattern)
        }
        InputPattern::TransitiveUpstream(_) => {
            build_upstream_requests(ctx, ctx.transitive_upstreams, pattern)
        }
    }
}

fn build_same_package_request(
    ctx: &ExpansionCtx<'_>,
    pattern: PatternSpec<'_>,
) -> Result<Vec<ResolveRequest>, InputExpansionError> {
    build_specific_request(ctx, ctx.source_pkg, pattern)
}

fn build_root_request(
    ctx: &ExpansionCtx<'_>,
    pattern: PatternSpec<'_>,
) -> Result<Vec<ResolveRequest>, InputExpansionError> {
    Ok(vec![ctx.resolve_request(RequestSpec {
        package: &PackageName::from(ROOT_PACKAGE_NAME),
        pattern,
        semantics: InputPattern::Root(pattern.path.to_string()).semantics(),
    })?])
}

fn build_specific_request(
    ctx: &ExpansionCtx<'_>,
    pkg: &PackageName,
    pattern: PatternSpec<'_>,
) -> Result<Vec<ResolveRequest>, InputExpansionError> {
    let semantics = if pkg == ctx.source_pkg {
        InputPattern::SamePackage(pattern.path.to_string()).semantics()
    } else {
        InputPattern::Specific(pkg.clone(), pattern.path.to_string()).semantics()
    };
    Ok(vec![ctx.resolve_request(RequestSpec {
        package: pkg,
        pattern,
        semantics,
    })?])
}

fn build_upstream_requests(
    ctx: &ExpansionCtx<'_>,
    upstreams: &[PackageName],
    pattern: PatternSpec<'_>,
) -> Result<Vec<ResolveRequest>, InputExpansionError> {
    upstreams
        .iter()
        .map(|upstream_pkg| {
            ctx.resolve_request(RequestSpec {
                package: upstream_pkg,
                pattern,
                semantics: InputSemantics::Wildcard,
            })
        })
        .collect()
}

/// Gets the directory path for a package.
fn package_dir(
    graph: &PackageGraph,
    pkg_name: &PackageName,
    pattern: &str,
) -> Result<PathBuf, InputExpansionError> {
    // Special case for synthetic root package: use repo root (caller's context)
    // The root package is not in the graph, so we can't find it there.
    // This is handled by passing repo_root directly for SamePackage patterns on root tasks.
    // For Specific patterns referencing the root package, we need special handling.
    if pkg_name.is_root() {
        // Root package isn't in the graph - caller should handle it via repo_root
        // by passing repo_root as base_dir. If we reach here via package_dir lookup,
        // return an error indicating the package wasn't found.
        return Err(InputExpansionError::UnknownPackage {
            package: pkg_name.clone(),
            pattern: pattern.to_string(),
        });
    }

    // Try to find the package in topological order
    let nodes = graph
        .topological_order()
        .map_err(|_| InputExpansionError::UnknownPackage {
            package: pkg_name.clone(),
            pattern: pattern.to_string(),
        })?;

    nodes
        .iter()
        .find(|node| &node.name == pkg_name)
        .map(|node| node.path.clone())
        .ok_or_else(|| InputExpansionError::UnknownPackage {
            package: pkg_name.clone(),
            pattern: pattern.to_string(),
        })
}

/// Validates that a pattern doesn't escape its base directory.
///
/// Performs lexical normalization without requiring filesystem access.
/// The pattern may contain glob characters, but we only validate the
/// literal prefix up to the first glob metacharacter.
fn validate_path(
    base_dir: &Path,
    pattern: &str,
    source_pkg: &PackageName,
    original_pattern: &str,
) -> Result<(), InputExpansionError> {
    // Extract the literal prefix (before any glob metacharacters)
    let literal_prefix = extract_literal_prefix(pattern);

    // Join and normalize lexically
    let resolved = lexical_normalize(&base_dir.join(literal_prefix));

    // Check if resolved path stays within base_dir
    if !path_starts_with(&resolved, base_dir) {
        return Err(InputExpansionError::PathEscape {
            source_pkg: source_pkg.clone(),
            pattern: original_pattern.to_string(),
        });
    }

    Ok(())
}

/// Extracts the literal prefix from a pattern (up to first glob metachar).
fn extract_literal_prefix(pattern: &str) -> &str {
    // Find the first glob metacharacter
    for (i, ch) in pattern.char_indices() {
        if matches!(ch, '*' | '?' | '[' | '{') {
            return &pattern[..i];
        }
    }
    pattern
}

/// Lexically normalizes a path, collapsing `.` and `..` components.
///
/// This does NOT require the path to exist on disk.
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

/// Checks if `path` starts with `prefix` (i.e., is within or equal to prefix).
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

/// Gets the direct upstream packages for a package.
fn direct_upstream_packages(
    graph: &PackageGraph,
    pkg_name: &PackageName,
) -> Result<Vec<PackageName>, InputExpansionError> {
    // Root package has no upstream dependencies
    if pkg_name.is_root() {
        return Ok(Vec::new());
    }

    let deps =
        graph
            .dependencies_of(pkg_name)
            .map_err(|_| InputExpansionError::UnknownPackage {
                package: pkg_name.clone(),
                pattern: String::new(),
            })?;

    Ok(deps.into_iter().map(|node| node.name.clone()).collect())
}

/// Gets the transitive upstream packages for a package (BFS traversal).
/// Reuses the same algorithm as `task_graph.rs::transitive_upstream_packages`.
#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};
    use tempfile::tempdir;

    use luchta_workspace::PackageNode;

    fn make_graph() -> PackageGraph {
        make_graph_with_deps(vec![
            ("frontend", &[]),
            ("shared", &[]),
            ("utils", &[]),
            ("common", &[]),
        ])
    }

    fn make_graph_with_deps(packages: Vec<(&str, &[&str])>) -> PackageGraph {
        let temp_dir = tempdir().expect("create temp dir");

        for (name, deps) in &packages {
            let pkg_dir = temp_dir.path().join(name);
            fs::create_dir_all(&pkg_dir).expect("create package dir");

            // Build dependencies JSON with "workspace:*" protocol for internal deps
            let deps_json = if deps.is_empty() {
                "{}".to_string()
            } else {
                let entries: Vec<String> = deps
                    .iter()
                    .map(|d| format!(r#""{}": "workspace:*""#, d))
                    .collect();
                format!("{{ {} }}", entries.join(", "))
            };

            let content = format!(
                r#"{{"name": "{}", "scripts": {{}}, "dependencies": {}}}"#,
                name, deps_json
            );
            fs::write(pkg_dir.join("package.json"), content).expect("write package.json");
        }

        let nodes: Vec<PackageNode> = packages
            .iter()
            .map(|(name, _deps)| {
                PackageNode::new(PackageName::from(*name), temp_dir.path().join(name))
            })
            .collect();

        PackageGraph::build(nodes)
            .expect("build graph")
            .with_root_package(PackageName::from("//root"))
    }

    #[test]
    fn same_package_resolves_to_own_dir() {
        let (graph, repo_root, source_pkg) = test_context("frontend");

        let patterns = vec!["src/**/*.ts".to_string()];
        let requests = expand_input_patterns(&patterns, &source_pkg, &graph, &repo_root).unwrap();

        assert_single_request(
            &requests,
            "frontend",
            "src/**/*.ts",
            InputSemantics::Wildcard,
        );
    }

    #[test]
    fn literal_pattern_is_literal_semantics() {
        let (graph, repo_root, source_pkg) = test_context("frontend");

        let patterns = vec!["package.json".to_string()];
        let requests = expand_input_patterns(&patterns, &source_pkg, &graph, &repo_root).unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].semantics, InputSemantics::Literal);
    }

    #[test]
    fn root_pattern_resolves_to_repo_root() {
        let (graph, repo_root, source_pkg) = test_context("frontend");

        let patterns = vec!["#config/base.json".to_string()];
        let requests = expand_input_patterns(&patterns, &source_pkg, &graph, &repo_root).unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].base_dir, PathBuf::from("/repo"));
        assert_eq!(requests[0].pattern, "config/base.json");
    }

    #[test]
    fn specific_pattern_resolves_to_named_pkg() {
        let (graph, repo_root, source_pkg) = test_context("frontend");

        let patterns = vec!["shared#src/index.ts".to_string()];
        let requests = expand_input_patterns(&patterns, &source_pkg, &graph, &repo_root).unwrap();

        assert_eq!(requests.len(), 1);
        assert!(requests[0].base_dir.ends_with("shared"));
        assert_eq!(requests[0].pattern, "src/index.ts");
    }

    #[test]
    fn unknown_package_returns_error() {
        let (graph, repo_root, source_pkg) = test_context("frontend");

        let patterns = vec!["@unknown/pkg#file.txt".to_string()];
        let result = expand_input_patterns(&patterns, &source_pkg, &graph, &repo_root);

        assert!(matches!(
            result,
            Err(InputExpansionError::UnknownPackage { .. })
        ));
    }

    fn assert_single_request(
        requests: &[ResolveRequest],
        expected_base_dir_suffix: &str,
        expected_pattern: &str,
        expected_semantics: InputSemantics,
    ) {
        let request = requests
            .first()
            .expect("expected exactly one resolve request");
        assert_eq!(requests.len(), 1);
        assert_request_matches(
            request,
            expected_base_dir_suffix,
            expected_pattern,
            expected_semantics,
        );
    }

    fn assert_request_matches(
        request: &ResolveRequest,
        expected_base_dir_suffix: &str,
        expected_pattern: &str,
        expected_semantics: InputSemantics,
    ) {
        assert!(request.base_dir.ends_with(expected_base_dir_suffix));
        assert_eq!(request.pattern, expected_pattern);
        assert_eq!(request.semantics, expected_semantics);
    }

    /// Helper to create a standard test context (graph, repo root, source package).
    fn test_context(pkg: &str) -> (PackageGraph, PathBuf, PackageName) {
        (make_graph(), PathBuf::from("/repo"), PackageName::from(pkg))
    }

    #[test]
    fn path_escape_patterns_rejected() {
        // Table-driven test for all patterns that should be rejected due to path escape
        let cases: &[&str] = &[
            "../other/file.txt",       // same-package escape
            "#../etc/passwd",          // root-pattern escape
            "shared#../../etc/passwd", // specific-package escape
            "../other/*.ts",           // literal-prefix escape (glob suffix)
        ];

        for pattern in cases {
            let (graph, repo_root, source_pkg) = test_context("frontend");
            let result =
                expand_input_patterns(&[pattern.to_string()], &source_pkg, &graph, &repo_root);
            assert!(
                matches!(result, Err(InputExpansionError::PathEscape { .. })),
                "pattern {:?} should be rejected as path escape",
                pattern
            );
        }
    }

    #[test]
    fn valid_subdirectory_allowed() {
        let (graph, repo_root, source_pkg) = test_context("frontend");

        // "src/../dist/file.txt" normalizes to "dist/file.txt" which is valid
        let patterns = vec!["src/../dist/file.txt".to_string()];
        let requests = expand_input_patterns(&patterns, &source_pkg, &graph, &repo_root).unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].pattern, "src/../dist/file.txt");
    }

    #[test]
    fn caret_always_wildcard_even_literal_looking() {
        let graph = make_graph_with_deps(vec![("lib", &[]), ("app", &["lib"])]);
        let repo_root = PathBuf::from("/repo");
        let source_pkg = PackageName::from("app");

        // ^config.json looks literal but ^ forces Wildcard
        let patterns = vec!["^config.json".to_string()];
        let requests = expand_input_patterns(&patterns, &source_pkg, &graph, &repo_root).unwrap();

        assert_eq!(requests.len(), 1);
        assert!(requests[0].base_dir.ends_with("lib"));
        assert_eq!(requests[0].semantics, InputSemantics::Wildcard);
    }

    #[test]
    fn lexical_normalize_collapses_dots() {
        assert_eq!(
            lexical_normalize(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(
            lexical_normalize(Path::new("/a/./b")),
            PathBuf::from("/a/b")
        );
        assert_eq!(
            lexical_normalize(Path::new("/a/b/c/../../d")),
            PathBuf::from("/a/d")
        );
    }

    #[test]
    fn lexical_normalize_preserves_root() {
        // Can't go above root
        let result = lexical_normalize(Path::new("/../etc"));
        assert!(result.starts_with("/"));
    }

    #[test]
    fn extract_literal_prefix_stops_at_glob() {
        // "src/**/*.ts" - the first * is at index 4, so prefix is "src/"
        assert_eq!(extract_literal_prefix("src/**/*.ts"), "src/");
        assert_eq!(extract_literal_prefix("file.txt"), "file.txt");
        assert_eq!(extract_literal_prefix("src/file?.ts"), "src/file");
        // When glob char is after a path separator, include the separator
        assert_eq!(extract_literal_prefix("src/[abc].ts"), "src/");
        assert_eq!(extract_literal_prefix("src/{a,b}.ts"), "src/");
    }

    #[test]
    fn direct_upstream_fan_out_to_exact_count() {
        let graph = make_graph_with_deps(vec![("c", &[]), ("b", &["c"]), ("a", &["b"])]);
        let repo_root = PathBuf::from("/repo");
        let source_pkg = PackageName::from("a");

        let requests =
            expand_input_patterns(&["^*.ts".to_string()], &source_pkg, &graph, &repo_root)
                .expect("expand direct upstream pattern");

        assert_eq!(requests.len(), 1);
        assert!(requests[0].base_dir.ends_with("b"));
        assert!(!requests[0].base_dir.ends_with("c"));
        assert_eq!(requests[0].pattern, "*.ts");
        assert_eq!(requests[0].semantics, InputSemantics::Wildcard);
    }

    #[test]
    fn transitive_upstream_fan_out_to_all_upstreams() {
        let graph = make_graph_with_deps(vec![("c", &[]), ("b", &["c"]), ("a", &["b"])]);
        let repo_root = PathBuf::from("/repo");
        let source_pkg = PackageName::from("a");

        let requests =
            expand_input_patterns(&["^^*.ts".to_string()], &source_pkg, &graph, &repo_root)
                .expect("expand transitive upstream pattern");

        assert_eq!(requests.len(), 2);
        let base_dirs: std::collections::HashSet<_> = requests
            .iter()
            .map(|request| request.base_dir.clone())
            .collect();
        assert_eq!(base_dirs.len(), 2);
        assert!(base_dirs.iter().any(|path| path.ends_with("b")));
        assert!(base_dirs.iter().any(|path| path.ends_with("c")));
        assert!(requests
            .iter()
            .all(|request| request.pattern == "*.ts"
                && request.semantics == InputSemantics::Wildcard));
    }

    #[test]
    fn transitive_upstream_includes_indirect() {
        // Build multi-level: A → B → C → D
        // A's ^^glob should include B, C, D (all transitive)
        let graph = make_graph_with_deps(vec![
            ("d", &[]),
            ("c", &["d"]),
            ("b", &["c"]),
            ("a", &["b"]),
        ]);
        let repo_root = PathBuf::from("/repo");
        let source_pkg = PackageName::from("a");

        let patterns = vec!["^^src/**/*.ts".to_string()];
        let requests = expand_input_patterns(&patterns, &source_pkg, &graph, &repo_root).unwrap();

        // Should get exactly 3 requests (B, C, D)
        assert_eq!(requests.len(), 3);

        // All should be Wildcard
        for req in &requests {
            assert_eq!(req.semantics, InputSemantics::Wildcard);
            assert_eq!(req.pattern, "src/**/*.ts");
        }

        // Verify all base_dirs
        let base_endings: Vec<_> = requests
            .iter()
            .map(|r| {
                r.base_dir.ends_with("b") || r.base_dir.ends_with("c") || r.base_dir.ends_with("d")
            })
            .collect();
        assert_eq!(base_endings.iter().filter(|&&x| x).count(), 3);
    }

    #[test]
    fn malformed_pattern_is_handled() {
        // Test that patterns are validated through InputPattern parsing
        // The "@" pattern alone should either succeed as valid or fail with an error
        // depending on InputPattern semantics - either outcome is acceptable
        let graph = make_graph_with_deps(vec![("a", &[])]);
        let repo_root = PathBuf::from("/repo");
        let source_pkg = PackageName::from("a");

        let _result = expand_input_patterns(&["@".to_string()], &source_pkg, &graph, &repo_root);
        // Test passes - we just verify the function doesn't panic
    }
}
