use std::path::Path;

/// Render `path` relative to `root` with forward slashes. If `path` is not under
/// `root`, fall back to the (normalized) full path so the location stays usable.
pub fn repo_relative(path: &Path, root: &Path) -> String {
    normalize_forward_slashes(path.strip_prefix(root).unwrap_or(path))
}

/// Normalize path separators to `/` for stable, portable output.
pub fn normalize_forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{normalize_forward_slashes, repo_relative};

    #[test]
    fn repo_relative_strips_root_prefix() {
        assert_eq!(
            repo_relative(
                Path::new("/repo/packages/app/src/foo.ts"),
                Path::new("/repo")
            ),
            "packages/app/src/foo.ts"
        );
    }

    #[test]
    fn repo_relative_handles_root_equal_to_parent() {
        assert_eq!(
            repo_relative(Path::new("/repo/src/foo.ts"), Path::new("/repo")),
            "src/foo.ts"
        );
    }

    #[test]
    fn repo_relative_falls_back_to_full_path_when_outside_root() {
        assert_eq!(
            repo_relative(Path::new("/other/src/foo.ts"), Path::new("/repo")),
            "/other/src/foo.ts"
        );
    }

    #[test]
    fn normalize_forward_slashes_replaces_backslashes() {
        assert_eq!(
            normalize_forward_slashes(Path::new("packages\\app\\src\\foo.ts")),
            "packages/app/src/foo.ts"
        );
    }
}
