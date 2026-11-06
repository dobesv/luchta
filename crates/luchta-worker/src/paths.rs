use std::path::Path;

/// Render `path` relative to `root` with forward slashes. If `path` is not under
/// `root`, fall back to the (normalized) full path so the location stays usable.
pub fn repo_relative(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) => normalize_forward_slashes(relative),
        Err(_) => {
            canonical_repo_relative(path, root).unwrap_or_else(|| normalize_forward_slashes(path))
        }
    }
}

// macOS may spell the same directory as either /var/... or /private/var/....
// Only touch the filesystem when the fast lexical comparison fails.
fn canonical_repo_relative(path: &Path, root: &Path) -> Option<String> {
    let path = path.canonicalize().ok()?;
    let root = root.canonicalize().ok()?;
    let relative = path.strip_prefix(root).ok()?;
    Some(normalize_forward_slashes(relative))
}

/// Normalize path separators to `/` for stable, portable output.
pub fn normalize_forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{normalize_forward_slashes, repo_relative};

    #[cfg(unix)]
    #[test]
    fn repo_relative_handles_symlink_aliases_for_file_or_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&root).expect("create repo");
        std::fs::write(root.join("file.ts"), "export {};\n").expect("write source");
        std::os::unix::fs::symlink(&root, &alias).expect("symlink repo");

        assert_eq!(repo_relative(&root.join("file.ts"), &alias), "file.ts");
        assert_eq!(repo_relative(&alias.join("file.ts"), &root), "file.ts");
    }

    #[cfg(unix)]
    #[test]
    fn canonical_fallback_preserves_outside_file_spelling() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        let outside = temp.path().join("outside.ts");
        std::fs::create_dir(&root).expect("create repo");
        std::fs::write(&outside, "export {};\n").expect("write outside source");
        assert_eq!(repo_relative(&outside, &root), outside.to_string_lossy());
    }

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
