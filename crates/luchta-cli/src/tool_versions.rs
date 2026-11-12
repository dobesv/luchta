use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use miette::{bail, IntoDiagnostic, Result, WrapErr};

/// Parses asdf entries, keeping fallback versions in their declared order.
/// Comments and blank lines are ignored; duplicate names use the first entry.
pub(crate) fn parse_tool_versions(content: &str) -> BTreeMap<String, Vec<String>> {
    content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .lines()
        .filter_map(|line| {
            let mut tokens = line
                .split('#')
                .next()
                .unwrap_or_default()
                .split_whitespace();
            let name = tokens.next()?.to_owned();
            Some((name, tokens.map(str::to_owned).collect()))
        })
        .fold(BTreeMap::new(), |mut entries, (name, versions)| {
            entries.entry(name).or_insert(versions);
            entries
        })
}

/// Returns added, removed, or changed names. Version tokens are opaque strings.
pub(crate) fn changed_tools(
    base: &BTreeMap<String, Vec<String>>,
    head: &BTreeMap<String, Vec<String>>,
) -> BTreeSet<String> {
    base.keys()
        .chain(head.keys())
        .filter(|name| base.get(*name) != head.get(*name))
        .cloned()
        .collect()
}

pub(crate) struct ToolVersionChanges {
    pub changed: BTreeSet<String>,
    pub head: BTreeMap<String, Vec<String>>,
}

/// Compares a base tree with the working tree, retaining head entries for warnings.
/// `workspace_relative` is the workspace-root path relative to the git repo root;
/// `workspace_root` is the working-tree workspace directory.
pub(crate) fn tool_version_changes_from_tree(
    base_tree: &gix::Tree<'_>,
    workspace_relative: &Path,
    workspace_root: &Path,
) -> Result<ToolVersionChanges> {
    let base = tool_versions_from_tree(base_tree, workspace_relative)?;
    let head = load_tool_versions(workspace_root)?;
    Ok(ToolVersionChanges {
        changed: changed_tools(&base, &head),
        head,
    })
}

/// Reads the workspace's entries from a git tree. A missing file is an empty map.
pub(crate) fn tool_versions_from_tree(
    tree: &gix::Tree<'_>,
    workspace_relative: &Path,
) -> Result<BTreeMap<String, Vec<String>>> {
    let path = workspace_relative.join(".tool-versions");
    let entry = tree
        .lookup_entry_by_path(&path)
        .into_diagnostic()
        .wrap_err_with(|| format!("Could not look up {} in git tree", path.display()))?;
    let Some(entry) = entry else {
        return Ok(BTreeMap::new());
    };
    if !entry.mode().is_blob() {
        bail!(
            "{} in git tree must be a regular file (symlinks are unsupported)",
            path.display()
        );
    }
    let blob = entry
        .object()
        .into_diagnostic()
        .and_then(|object| object.try_into_blob().into_diagnostic())
        .wrap_err_with(|| format!("Could not read {} from git tree", path.display()))?;
    parse_tool_versions_bytes(&blob.data, &path)
}

/// Reads the workspace's working-tree entries. A missing file is an empty map.
pub(crate) fn load_tool_versions(workspace_root: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let path = workspace_root.join(".tool-versions");
    if !regular_worktree_file_exists(&path)? {
        return Ok(BTreeMap::new());
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(error)
                .into_diagnostic()
                .wrap_err_with(|| format!("Could not read {}", path.display()));
        }
    };
    parse_tool_versions_bytes(&bytes, &path)
}

/// Missing files return false; other non-regular entries are errors. Inspect
/// symlinks themselves so working-tree reads follow the same policy as git trees.
fn regular_worktree_file_exists(path: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .into_diagnostic()
                .wrap_err_with(|| format!("Could not inspect {}", path.display()));
        }
    };
    if !metadata.is_file() {
        bail!(
            "{} must be a regular file (symlinks are unsupported)",
            path.display()
        );
    }
    Ok(true)
}

fn parse_tool_versions_bytes(bytes: &[u8], path: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let content = std::str::from_utf8(bytes)
        .into_diagnostic()
        .wrap_err_with(|| format!("{} is not valid UTF-8", path.display()))?;
    Ok(parse_tool_versions(content))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn ignores_leading_utf8_bom() {
        for content in ["luchta 0.3.7\n", "# comment\nluchta 0.3.7\n", ""] {
            assert_eq!(
                parse_tool_versions(&format!("\u{feff}{content}")),
                parse_tool_versions(content)
            );
        }
    }

    #[test]
    fn parses_comments_blank_lines_and_multiple_versions() {
        let content =
            "\n  # whole-line comment\n\t\nnodejs 22.1 20.9 system # fallback\nrust\t1.98.1#pin\n";
        assert_eq!(
            parse_tool_versions(content),
            BTreeMap::from([
                (
                    "nodejs".into(),
                    vec!["22.1".into(), "20.9".into(), "system".into()]
                ),
                ("rust".into(), vec!["1.98.1".into()]),
            ])
        );
    }

    #[test]
    fn keeps_names_without_versions_and_first_duplicate() {
        let parsed =
            parse_tool_versions("rust 1.97 system\nrust 1.98\nluchta # no version\nluchta 0.3.7\n");
        assert_eq!(parsed["rust"], ["1.97", "system"]);
        assert!(parsed["luchta"].is_empty());
        assert_eq!(parse_tool_versions("rust 1.98\nrust\n")["rust"], ["1.98"]);
    }

    #[test]
    fn accepts_opaque_version_tokens_without_validation() {
        let parsed = parse_tool_versions("custom ref:topic path:../tool system\n");
        assert_eq!(parsed["custom"], ["ref:topic", "path:../tool", "system"]);
    }

    #[test]
    fn detects_added_removed_and_changed_entries() {
        let base = parse_tool_versions("luchta 0.3.6\nrust 1.98\nnodejs 22\n");
        let head = parse_tool_versions("luchta 0.3.7\nrust 1.98\npython 3.13\n");
        assert_eq!(
            changed_tools(&base, &head),
            names(&["luchta", "nodejs", "python"])
        );
    }

    #[test]
    fn detects_fallback_token_and_order_changes() {
        let base = parse_tool_versions("nodejs 22 20\npython 3.13 system\n");
        let head = parse_tool_versions("nodejs 20 22\npython 3.13 3.12\n");
        assert_eq!(changed_tools(&base, &head), names(&["nodejs", "python"]));
    }

    #[test]
    fn empty_version_entry_is_distinct_from_absent_entry() {
        let base = parse_tool_versions("luchta\n");
        assert_eq!(changed_tools(&base, &BTreeMap::new()), names(&["luchta"]));
        assert_eq!(changed_tools(&BTreeMap::new(), &base), names(&["luchta"]));
    }

    #[test]
    fn unchanged_versions_ignore_comments_whitespace_and_entry_order() {
        let base = parse_tool_versions("luchta 0.3.7\nrust 1.98\n");
        let head = parse_tool_versions("# comment\nrust\t1.98\n\n luchta 0.3.7 # pin\n");
        assert!(changed_tools(&base, &head).is_empty());
    }

    #[test]
    fn reads_nested_workspace_instead_of_repo_root() {
        let fixture = TestRepo::new();
        fixture.write_file(".tool-versions", b"luchta root\n");
        fixture.write_file("nested/workspace/.tool-versions", b"luchta 0.3.6\n");
        fixture.git_add_and_commit_all();
        fixture.write_file("nested/workspace/.tool-versions", b"luchta 0.3.7\n");
        let changes = fixture.changes("nested/workspace").unwrap();
        assert_eq!(changes.changed, names(&["luchta"]));
        assert_eq!(changes.head["luchta"], ["0.3.7"]);
    }

    #[test]
    fn unchanged_file_has_no_changed_tools() {
        let fixture = TestRepo::new();
        fixture.write_file(".tool-versions", b"luchta 0.3.7\n");
        fixture.git_add_and_commit_all();
        assert!(fixture.changes("").unwrap().changed.is_empty());
    }

    #[test]
    fn absent_base_file_counts_worktree_entries_as_added() {
        let fixture = TestRepo::new();
        fixture.git_add_and_commit_all();
        fixture.write_file(".tool-versions", b"luchta 0.3.7\n");
        let changes = fixture.changes("").unwrap();
        assert_eq!(changes.changed, names(&["luchta"]));
        assert_eq!(changes.head["luchta"], ["0.3.7"]);
    }

    #[test]
    fn absent_at_both_sides_has_no_changed_tools() {
        let fixture = TestRepo::new();
        fixture.git_add_and_commit_all();
        let changes = fixture.changes("missing/workspace").unwrap();
        assert!(changes.changed.is_empty());
        assert!(changes.head.is_empty());
    }

    #[test]
    fn absent_worktree_file_counts_base_entries_as_removed() {
        let fixture = TestRepo::new();
        fixture.write_file(".tool-versions", b"luchta 0.3.7\n");
        fixture.git_add_and_commit_all();
        fs::remove_file(fixture.path().join(".tool-versions")).unwrap();
        let changes = fixture.changes("").unwrap();
        assert_eq!(changes.changed, names(&["luchta"]));
        assert!(changes.head.is_empty());
    }

    #[test]
    fn non_utf8_base_content_errors_with_repo_relative_path() {
        let fixture = TestRepo::new();
        fixture.write_file("nested/.tool-versions", b"luchta \xff\n");
        fixture.git_add_and_commit_all();
        fixture.write_file("nested/.tool-versions", b"luchta 0.3.7\n");
        let error = fixture.changes("nested").err().unwrap().to_string();
        assert!(
            error.contains(
                &Path::new("nested")
                    .join(".tool-versions")
                    .display()
                    .to_string()
            ),
            "{error}"
        );
        assert!(error.contains("UTF-8"), "{error}");
    }

    #[test]
    fn non_utf8_worktree_content_errors_with_path() {
        let fixture = TestRepo::new();
        fixture.git_add_and_commit_all();
        fixture.write_file("nested/.tool-versions", b"luchta \xff\n");
        let error = fixture.changes("nested").err().unwrap().to_string();
        assert!(
            error.contains(
                &fixture
                    .path()
                    .join("nested/.tool-versions")
                    .display()
                    .to_string()
            ),
            "{error}"
        );
        assert!(error.contains("UTF-8"), "{error}");
    }

    #[test]
    fn worktree_read_errors_are_not_treated_as_absent() {
        let fixture = TestRepo::new();
        fs::create_dir(fixture.path().join(".tool-versions")).unwrap();
        let error = load_tool_versions(fixture.path()).unwrap_err().to_string();
        assert!(error.contains(".tool-versions"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_in_base_tree_errors_instead_of_parsing_link_target() {
        let fixture = TestRepo::new();
        fixture.write_file("nested/pins", b"luchta 0.3.7\n");
        let path = fixture.path().join("nested/.tool-versions");
        std::os::unix::fs::symlink("pins", &path).unwrap();
        fixture.git_add_and_commit_all();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"luchta 0.3.7\n").unwrap();

        let error = fixture.changes("nested").err().unwrap().to_string();
        assert!(error.contains("nested/.tool-versions"), "{error}");
        assert!(error.contains("regular file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn worktree_symlinks_error_even_when_target_is_missing() {
        for target_exists in [false, true] {
            let fixture = TestRepo::new();
            if target_exists {
                fixture.write_file("pins", b"luchta 0.3.7\n");
            }
            let path = fixture.path().join(".tool-versions");
            std::os::unix::fs::symlink("pins", &path).unwrap();
            let error = load_tool_versions(fixture.path()).unwrap_err().to_string();
            assert!(error.contains(&path.display().to_string()), "{error}");
            assert!(error.contains("regular file"), "{error}");
        }
    }

    #[test]
    fn directory_in_base_tree_errors_with_path() {
        let fixture = TestRepo::new();
        fixture.write_file("nested/.tool-versions/pins", b"luchta 0.3.7\n");
        fixture.git_add_and_commit_all();
        let error = fixture.changes("nested").err().unwrap().to_string();
        let path = Path::new("nested").join(".tool-versions");
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert!(error.contains("regular file"), "{error}");
    }

    #[test]
    fn executable_file_in_base_tree_is_a_regular_file() {
        let fixture = TestRepo::new();
        fixture.write_file(".tool-versions", b"luchta 0.3.7\n");
        fixture.git_add_and_commit_all();
        fixture.git(&["update-index", "--chmod=+x", ".tool-versions"]);
        fixture.git(&["commit", "-m", "executable pin file"]);
        assert!(fixture.changes("").unwrap().changed.is_empty());
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    struct TestRepo {
        root: TempDir,
    }

    impl TestRepo {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            let fixture = Self { root };
            fixture.git(&["init"]);
            fixture.git(&["config", "user.name", "Luchta Tests"]);
            fixture.git(&["config", "user.email", "luchta@example.com"]);
            fixture
        }

        fn path(&self) -> &Path {
            self.root.path()
        }

        fn write_file(&self, relative: &str, contents: &[u8]) {
            let path = self.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        fn git_add_and_commit_all(&self) {
            self.git(&["add", "."]);
            self.git(&["commit", "--allow-empty", "-m", "baseline"]);
        }

        fn git(&self, args: &[&str]) {
            let output = Command::new("git")
                .args(args)
                .current_dir(self.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        fn changes(&self, workspace_relative: &str) -> Result<ToolVersionChanges> {
            let repo = gix::open(self.path()).unwrap();
            let tree = repo.head_commit().unwrap().tree().unwrap();
            tool_version_changes_from_tree(
                &tree,
                Path::new(workspace_relative),
                &self.path().join(workspace_relative),
            )
        }
    }
}
