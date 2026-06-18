//! Shared cache path resolution.
//!
//! The shared cache is distinct from the local project cache. It stores
//! build artifacts that can be shared across multiple projects and users.
//!
//! # Location
//! - Default: `$XDG_CACHE_HOME/luchta` (typically `~/.cache/luchta` on Linux/macOS)
//! - Override: `LUCHTA_SHARED_CACHE_DIR` environment variable
//!
//! Note: The shared cache uses a different env var than the local cache
//! (`LUCHTA_CACHE_DIR`) to allow independent configuration.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Environment variable for overriding the shared cache directory.
pub const SHARED_CACHE_DIR_ENV: &str = "LUCHTA_SHARED_CACHE_DIR";

/// Subdirectory name for blob storage.
pub const BLOBS_DIR_NAME: &str = "blobs";

/// Subdirectory name for snapshot storage.
pub const SNAPSHOTS_DIR_NAME: &str = "snapshots";

/// Default application directory name within the cache.
const APP_DIR_NAME: &str = "luchta";

/// Paths for the shared cache directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedCachePaths {
    /// Root directory of the shared cache.
    pub root: PathBuf,
    /// Directory for storing blobs (content-addressed).
    pub blobs_dir: PathBuf,
    /// Directory for storing snapshots.
    pub snapshots_dir: PathBuf,
}

/// Resolve the shared cache root directory from explicit inputs.
///
/// This is a pure helper for testing; production code should use
/// [`resolve_shared_cache_dir`] which reads from the environment.
///
/// # Arguments
/// * `env_override` - Value of `LUCHTA_SHARED_CACHE_DIR` env var, if set.
///   Empty string is treated as `None`.
/// * `platform_cache_dir` - Result of `dirs::cache_dir()`, if available.
///
/// # Resolution order
/// 1. `env_override` if `Some(non_empty)`
/// 2. `platform_cache_dir` joined with `luchta` if `Some`
/// 3. Fallback: `/tmp/luchta-cache`
pub(crate) fn resolve_shared_cache_dir_from(
    env_override: Option<String>,
    platform_cache_dir: Option<PathBuf>,
) -> PathBuf {
    // Check for environment override (empty string treated as None)
    if let Some(path) = env_override {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }

    // Use standard cache directory
    if let Some(cache_dir) = platform_cache_dir {
        return cache_dir.join(APP_DIR_NAME);
    }

    // Fallback: use /tmp/luchta-cache if system cache dir unavailable.
    // This is a stable path that works on Unix systems.
    // For Windows this path may not be ideal, but dirs::cache_dir() should
    // always succeed on Windows.
    PathBuf::from("/tmp/luchta-cache")
}

/// Resolve the shared cache root directory.
///
/// Resolution order:
/// 1. `LUCHTA_SHARED_CACHE_DIR` environment variable (if set and non-empty)
/// 2. `dirs::cache_dir()` joined with `luchta`
/// 3. Fallback: `/tmp/luchta-cache` (used when system cache dir unavailable)
///
/// This function never panics. If no cache directory can be determined,
/// it falls back to a sensible temp directory.
pub fn resolve_shared_cache_dir() -> PathBuf {
    let env_override = env::var(SHARED_CACHE_DIR_ENV).ok();
    // Treat empty string as None
    let env_override = env_override.filter(|s| !s.is_empty());
    resolve_shared_cache_dir_from(env_override, dirs::cache_dir())
}

/// Open and initialize the shared cache paths.
///
/// Creates the cache directory structure if it doesn't exist:
/// - `root/` - main cache directory
/// - `root/blobs/` - blob storage
/// - `root/snapshots/` - snapshot storage
///
/// # Errors
///
/// Returns an error if directory creation fails due to permissions or
/// filesystem issues.
pub fn open_shared_paths(root: &Path) -> io::Result<SharedCachePaths> {
    let blobs_dir = root.join(BLOBS_DIR_NAME);
    let snapshots_dir = root.join(SNAPSHOTS_DIR_NAME);

    // Create all directories (mkdir -p)
    fs::create_dir_all(root)?;
    fs::create_dir_all(&blobs_dir)?;
    fs::create_dir_all(&snapshots_dir)?;

    Ok(SharedCachePaths {
        root: root.to_path_buf(),
        blobs_dir,
        snapshots_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Tests for the pure helper function - no env mutation needed

    #[test]
    fn resolve_from_env_override_non_empty_used() {
        let expected = PathBuf::from("/custom/cache/path");
        let actual = resolve_shared_cache_dir_from(Some(expected.to_string_lossy().into()), None);
        assert_eq!(actual, expected);
    }

    #[test]
    fn resolve_from_env_override_empty_string_ignored() {
        // Empty string treated as None -> fall back to platform_cache_dir
        let cache_dir = PathBuf::from("/home/user/.cache");
        let actual = resolve_shared_cache_dir_from(Some(String::new()), Some(cache_dir.clone()));
        assert_eq!(actual, cache_dir.join("luchta"));
    }

    #[test]
    fn resolve_from_env_override_none_uses_cache_dir() {
        let cache_dir = PathBuf::from("/home/user/.cache");
        let actual = resolve_shared_cache_dir_from(None, Some(cache_dir.clone()));
        assert_eq!(actual, cache_dir.join("luchta"));
    }

    #[test]
    fn resolve_from_no_env_no_cache_dir_falls_back() {
        let actual = resolve_shared_cache_dir_from(None, None);
        assert_eq!(actual, PathBuf::from("/tmp/luchta-cache"));
    }

    #[test]
    fn open_shared_paths_creates_directories() {
        let temp_dir = tempdir().expect("failed to create temp dir");
        let root = temp_dir.path().join("test-cache");

        // Directories should not exist yet
        assert!(!root.exists());

        let paths = open_shared_paths(&root).expect("failed to open paths");

        // Verify paths
        assert_eq!(paths.root, root);
        assert_eq!(paths.blobs_dir, root.join(BLOBS_DIR_NAME));
        assert_eq!(paths.snapshots_dir, root.join(SNAPSHOTS_DIR_NAME));

        // Verify directories were created
        assert!(root.exists());
        assert!(paths.blobs_dir.exists());
        assert!(paths.snapshots_dir.exists());
    }

    #[test]
    fn open_shared_paths_idempotent() {
        let temp_dir = tempdir().expect("failed to create temp dir");
        let root = temp_dir.path().join("test-cache-idempotent");

        // Create twice
        let paths1 = open_shared_paths(&root).expect("first open failed");
        let paths2 = open_shared_paths(&root).expect("second open failed");

        // Should be equivalent
        assert_eq!(paths1.root, paths2.root);
        assert_eq!(paths1.blobs_dir, paths2.blobs_dir);
        assert_eq!(paths1.snapshots_dir, paths2.snapshots_dir);
    }
}
