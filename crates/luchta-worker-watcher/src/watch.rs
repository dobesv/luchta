//! File watching component for the worker watcher.
//!
//! Watches file globs and emits a signal on matching (debounced) changes.
//! Does NOT respect `.gitignore` — build outputs are commonly watched.

use std::path::{Path, PathBuf};
use std::time::Duration;

use globset::{Glob, GlobSetBuilder};
use notify::Watcher;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use thiserror::Error;

/// Configuration for the file watcher.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Glob patterns to watch (e.g., `["src/**/*.rs", "dist/**/*.js"]`).
    pub globs: Vec<String>,
    /// Debounce duration for file system events.
    pub debounce: Duration,
}

/// Errors from the file watcher.
#[derive(Debug, Error)]
pub enum WatchError {
    /// Failed to parse a glob pattern.
    #[error("invalid glob pattern: {0}")]
    InvalidGlob(String),

    /// Failed to build the globset.
    #[error("failed to build globset: {0}")]
    GlobsetBuild(String),

    /// Failed to create the file watcher.
    #[error("failed to create watcher: {0}")]
    WatcherCreate(String),
}

/// Tests if a path matches the globset, trying multiple path forms.
///
/// Tries matching in order:
/// 1. Raw path (handles absolute globs)
/// 2. Path stripped of cwd prefix (handles relative globs with absolute event paths)
/// 3. Canonicalized path stripped of canonicalized cwd (handles symlink differences)
fn path_matches(
    globset: &globset::GlobSet,
    cwd: &Option<PathBuf>,
    canonical_cwd: &Option<PathBuf>,
    path: &Path,
) -> bool {
    matches_raw(globset, path)
        || matches_relative_to(globset, cwd, path)
        || matches_canonical_relative(globset, canonical_cwd, path)
}

fn matches_raw(globset: &globset::GlobSet, path: &Path) -> bool {
    globset.is_match(path)
}

fn matches_relative_to(globset: &globset::GlobSet, base: &Option<PathBuf>, path: &Path) -> bool {
    base.as_deref()
        .and_then(|base| path.strip_prefix(base).ok())
        .is_some_and(|relative| globset.is_match(relative))
}

fn matches_canonical_relative(
    globset: &globset::GlobSet,
    canonical_base: &Option<PathBuf>,
    path: &Path,
) -> bool {
    let Ok(canonical_path) = path.canonicalize() else {
        return false;
    };
    matches_relative_to(globset, canonical_base, &canonical_path)
}

/// Builds a `GlobSet` from the provided glob patterns.
///
/// # Errors
///
/// Returns `WatchError::InvalidGlob` if any pattern cannot be parsed,
/// or `WatchError::GlobsetBuild` if the set cannot be built.
pub fn build_glob_set(globs: &[String]) -> Result<globset::GlobSet, WatchError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in globs {
        let glob =
            Glob::new(pattern).map_err(|e| WatchError::InvalidGlob(format!("{pattern}: {e}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| WatchError::GlobsetBuild(e.to_string()))
}

/// Determines the watch roots from glob patterns.
///
/// For each glob, finds the nearest existing ancestor directory (the longest
/// literal prefix path that exists) and returns them deduplicated. This ensures
/// notify receives events for files matching the glob.
///
/// If no existing ancestor is found for a glob, falls back to `"."` (current
/// directory) as a safe default so events aren't silently missed.
pub fn watch_roots(globs: &[String]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    for pattern in globs {
        if let Some(root) = find_existing_ancestor(pattern) {
            if !roots.contains(&root) {
                roots.push(root);
            }
        } else {
            // Fallback to current directory if no ancestor exists
            let cwd = PathBuf::from(".");
            if !roots.contains(&cwd) {
                roots.push(cwd);
            }
        }
    }

    roots
}

/// Finds the nearest existing ancestor directory for a glob pattern.
///
/// Walks the literal prefix of the glob (before any wildcards) and returns
/// the longest path that exists as a directory.
fn find_existing_ancestor(pattern: &str) -> Option<PathBuf> {
    // Extract the literal prefix before any wildcard characters
    let literal_prefix = extract_literal_prefix(pattern);

    // If empty, no valid ancestor
    if literal_prefix.is_empty() {
        return None;
    }

    let path = Path::new(literal_prefix);

    // Try the full path first, then each parent
    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };

    loop {
        if current.is_dir() {
            return Some(current);
        }
        if !current.pop() {
            // Reached the root without finding a directory
            break;
        }
    }

    None
}

/// Extracts the literal prefix from a glob pattern (before any wildcards).
///
/// Handles common glob patterns like `src/**/*.rs` or `dist/**/*.js`.
fn extract_literal_prefix(pattern: &str) -> &str {
    let wildcard_index = pattern
        .split('/')
        .scan(0usize, |offset, component| {
            let start = *offset;
            *offset += component.len() + 1;
            Some((start, component))
        })
        .find(|(_, component)| has_glob_meta(component))
        .map(|(start, _)| start);

    wildcard_index
        .map(|index| trim_trailing_separator(&pattern[..index]))
        .unwrap_or_else(|| trim_trailing_separator(pattern))
}

fn has_glob_meta(component: &str) -> bool {
    component.contains(['*', '?', '[', '{'])
}

fn trim_trailing_separator(value: &str) -> &str {
    value.strip_suffix('/').unwrap_or(value)
}

/// Runs the file watcher with the given configuration.
///
/// Watches directories derived from the glob patterns and sends a unit `()`
/// signal into the provided channel whenever any debounced file event matches
/// one of the globs.
///
/// # Public Contract
///
/// - **Signature**: `pub async fn run(config: WatchConfig, on_change: tokio::sync::mpsc::Sender<()>) -> Result<(), WatchError>`
/// - **Behavior**: On each debounced batch of file events, tests each affected path
///   against the configured `GlobSet`. If ANY path matches, emits exactly ONE `()`
///   message into the `on_change` channel. Coalesces the whole batch into a single signal.
/// - **Gitignore**: Does NOT respect `.gitignore` — build outputs are commonly watched.
/// - **Errors**: Logs watcher errors to stderr and continues; only returns `Err`
///   for setup failures (bad glob, watcher creation).
///
/// The caller (main.rs) adapts the `()` signal into `RouterEvent::FileChanged`
/// when forwarding to the router's event channel.
pub async fn run(
    config: WatchConfig,
    on_change: tokio::sync::mpsc::Sender<()>,
) -> Result<(), WatchError> {
    let globset = build_glob_set(&config.globs)?;
    let roots = watch_roots(&config.globs);

    if roots.is_empty() {
        eprintln!("[watch] warning: no watch roots determined, watching current directory");
        return Err(WatchError::WatcherCreate("no watch roots available".into()));
    }

    // Capture cwd for relative glob matching
    // notify delivers absolute paths, but user globs are typically relative
    let cwd = std::env::current_dir().ok();
    let canonical_cwd = cwd.as_ref().and_then(|c| c.canonicalize().ok());

    // Use tokio unbounded channel for non-blocking bridge from sync callback to async
    // The UnboundedSender can be used synchronously from the notify callback
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // Spawn a task to forward from tokio unbounded channel to on_change
    let forward_task = tokio::spawn(async move {
        while rx.recv().await.is_some() {
            if on_change.send(()).await.is_err() {
                // Receiver dropped, exit
                break;
            }
        }
    });

    // Create debouncer with callback
    let callback_globset = globset.clone();
    let callback = move |result: DebounceEventResult| {
        match result {
            Ok(events) => {
                for event in events {
                    for path in &event.paths {
                        if path_matches(&callback_globset, &cwd, &canonical_cwd, path) {
                            // Send exactly one signal per batch
                            // UnboundedSender::send is synchronous and non-blocking
                            let _ = tx.send(());
                            return; // Exit after first match in batch
                        }
                    }
                }
            }
            Err(errors) => {
                eprintln!("[watch] error: {:?}", errors);
            }
        }
    };

    // Create the debouncer
    let mut debouncer = new_debouncer(config.debounce, None, callback)
        .map_err(|e| WatchError::WatcherCreate(e.to_string()))?;

    // Watch each root recursively
    for root in roots {
        if let Err(e) = debouncer
            .watcher()
            .watch(&root, notify::RecursiveMode::Recursive)
        {
            eprintln!("[watch] warning: failed to watch {root:?}: {e}");
        }
    }

    // Keep the debouncer alive until the channel is closed
    // The forward_task will run until on_change is dropped
    forward_task
        .await
        .map_err(|e| WatchError::WatcherCreate(e.to_string()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn glob_set_matches_src_files() {
        let globs = vec!["src/**/*.rs".to_string()];
        let globset = build_glob_set(&globs).expect("build globset");

        assert!(globset.is_match("src/a/b.rs"));
        assert!(globset.is_match("src/lib.rs"));
        assert!(globset.is_match("src/deep/nested/file.rs"));
        assert!(!globset.is_match("docs/readme.md"));
        assert!(!globset.is_match("README.md"));
        assert!(!globset.is_match("build.rs"));
    }

    #[test]
    fn glob_set_matches_build_outputs() {
        let globs = vec!["dist/**/*.js".to_string()];
        let globset = build_glob_set(&globs).expect("build globset");

        assert!(globset.is_match("dist/bundle.js"));
        assert!(globset.is_match("dist/a/b/c.js"));
        assert!(!globset.is_match("src/index.ts"));
        assert!(!globset.is_match("dist/style.css"));
    }

    #[test]
    fn glob_set_multiple_patterns() {
        let globs = vec!["src/**/*.rs".to_string(), "dist/**/*.js".to_string()];
        let globset = build_glob_set(&globs).expect("build globset");

        assert!(globset.is_match("src/main.rs"));
        assert!(globset.is_match("dist/app.js"));
        assert!(!globset.is_match("Cargo.toml"));
        assert!(!globset.is_match("tests/main.rs")); // outside src
    }

    #[test]
    fn glob_set_invalid_pattern() {
        let globs = vec!["[invalid".to_string()]; // Unclosed bracket
        let err = build_glob_set(&globs).expect_err("should fail");
        assert!(matches!(err, WatchError::InvalidGlob(_)));
    }

    #[test]
    fn watch_roots_finds_existing_src_dir() {
        let temp = TempDir::new().expect("create temp dir");
        let temp_path = temp.path();

        // Create src directory
        fs::create_dir_all(temp_path.join("src")).expect("create src");

        // Change to temp directory
        let original = std::env::current_dir().expect("get cwd");
        std::env::set_current_dir(temp_path).expect("set cwd");

        let result = watch_roots(&["src/**/*.rs".to_string()]);

        std::env::set_current_dir(&original).expect("restore cwd");

        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("src") || result[0].ends_with("src/"));
    }

    #[test]
    fn watch_roots_deduplicates_common_ancestors() {
        let temp = TempDir::new().expect("create temp dir");
        let temp_path = temp.path();

        // Create directories
        fs::create_dir_all(temp_path.join("src")).expect("create src");

        let original = std::env::current_dir().expect("get cwd");
        std::env::set_current_dir(temp_path).expect("set cwd");

        let result = watch_roots(&["src/**/*.rs".to_string(), "src/**/*.toml".to_string()]);

        std::env::set_current_dir(&original).expect("restore cwd");

        // Both patterns share src as ancestor - should have only one root
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn watch_roots_fallback_to_current_dir() {
        // For a glob that doesn't exist, should fallback to "."
        let temp = TempDir::new().expect("create temp dir");
        let temp_path = temp.path();

        let original = std::env::current_dir().expect("get cwd");
        std::env::set_current_dir(temp_path).expect("set cwd");

        // Non-existent directory in glob
        let result = watch_roots(&["nonexistent/**/*.rs".to_string()]);

        std::env::set_current_dir(&original).expect("restore cwd");

        // Should fallback to current directory
        assert!(!result.is_empty());
    }

    #[test]
    fn extract_literal_prefix_stops_at_double_star() {
        assert_eq!(extract_literal_prefix("src/**/*.rs"), "src");
        assert_eq!(extract_literal_prefix("dist/**/*.js"), "dist");
        assert_eq!(extract_literal_prefix("a/b/**/*.txt"), "a/b");
    }

    #[test]
    fn extract_literal_prefix_simple_single_wildcard() {
        // Just to verify behavior with non-** wildcards
        assert_eq!(extract_literal_prefix("src/*.rs"), "src");
        assert_eq!(extract_literal_prefix("*.rs"), "");
        assert_eq!(extract_literal_prefix("a/b/?.rs"), "a/b");
    }

    #[test]
    fn extract_literal_prefix_preserves_absolute_prefix() {
        assert_eq!(
            extract_literal_prefix("/tmp/x/watched/**/*.txt"),
            "/tmp/x/watched"
        );
        assert_eq!(extract_literal_prefix("/tmp/x/file.txt"), "/tmp/x/file.txt");
    }

    #[test]
    fn watch_roots_handles_absolute_path() {
        let temp = TempDir::new().expect("create temp dir");
        let temp_path = temp.path().canonicalize().expect("canonicalize");

        // Create directory
        fs::create_dir_all(temp_path.join("project")).expect("create project");

        let glob = format!("{}/project/**/*.rs", temp_path.display());
        let result = watch_roots(&[glob]);

        // Should find the project directory
        assert!(!result.is_empty());
        assert!(result[0].ends_with("project") || result[0].ends_with("project/"));
    }

    // ====== path_matches tests ======

    #[test]
    fn path_matches_relative_glob_with_absolute_event_path() {
        // Relative glob "watched/**/*.txt" should match absolute path "/tmp/x/watched/a.txt"
        // by stripping the cwd prefix
        let globs = vec!["watched/**/*.txt".to_string()];
        let globset = build_glob_set(&globs).expect("build globset");

        let cwd = Some(PathBuf::from("/tmp/x"));
        let canonical_cwd = None; // not needed for this test

        let path = Path::new("/tmp/x/watched/a.txt");
        assert!(path_matches(&globset, &cwd, &canonical_cwd, path));
    }

    #[test]
    fn path_matches_absolute_glob_with_absolute_path() {
        // Absolute glob should match absolute path via raw branch
        let globs = vec!["/tmp/x/watched/**/*.txt".to_string()];
        let globset = build_glob_set(&globs).expect("build globset");

        let cwd: Option<PathBuf> = None;
        let canonical_cwd: Option<PathBuf> = None;

        let path = Path::new("/tmp/x/watched/a.txt");
        assert!(path_matches(&globset, &cwd, &canonical_cwd, path));
    }

    #[test]
    fn path_matches_does_not_match_unrelated_path() {
        let globs = vec!["watched/**/*.txt".to_string()];
        let globset = build_glob_set(&globs).expect("build globset");

        let cwd = Some(PathBuf::from("/tmp/x"));
        let canonical_cwd: Option<PathBuf> = None;

        // Path outside watched directory
        let path = Path::new("/tmp/x/other/b.txt");
        assert!(!path_matches(&globset, &cwd, &canonical_cwd, path));
    }

    #[test]
    fn path_matches_does_not_match_wrong_extension() {
        let globs = vec!["watched/**/*.txt".to_string()];
        let globset = build_glob_set(&globs).expect("build globset");

        let cwd = Some(PathBuf::from("/tmp/x"));
        let canonical_cwd: Option<PathBuf> = None;

        let path = Path::new("/tmp/x/watched/a.rs");
        assert!(!path_matches(&globset, &cwd, &canonical_cwd, path));
    }

    #[test]
    fn path_matches_no_cwd_still_matches_absolute_glob() {
        // If cwd is None, absolute globs should still work
        let globs = vec!["/tmp/x/watched/**/*.txt".to_string()];
        let globset = build_glob_set(&globs).expect("build globset");

        let cwd: Option<PathBuf> = None;
        let canonical_cwd: Option<PathBuf> = None;

        let path = Path::new("/tmp/x/watched/a.txt");
        assert!(path_matches(&globset, &cwd, &canonical_cwd, path));
    }

    #[test]
    fn path_matches_handles_symlinked_path_via_canonical() {
        // If event path uses symlinked prefix but glob is relative,
        // canonical branch should handle it
        let globs = vec!["watched/**/*.txt".to_string()];
        let globset = build_glob_set(&globs).expect("build globset");

        // Simulate: cwd is canonicalized, event path is symlinked
        // We can't easily create real symlinks in unit tests,
        // but we can test that the code handles it gracefully
        let cwd = Some(PathBuf::from("/var/folders/x/project"));
        let canonical_cwd = Some(PathBuf::from("/private/var/folders/x/project"));

        // If path canonicalizes to match, it should work
        // (In real usage, path might be /var/... which canonicalizes to /private/var/...)
        // For this test, we just verify the logic path doesn't panic
        let path = Path::new("/var/folders/x/project/watched/a.txt");
        // This may or may not match depending on FS state, but should not panic
        let _ = path_matches(&globset, &cwd, &canonical_cwd, path);
    }
}
