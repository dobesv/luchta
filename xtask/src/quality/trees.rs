//! The two trees `cargo xtask quality` analyses. Each is a scratch copy of
//! just the `.rs` files: the base has no checkout to analyse, and rustqual
//! walks every directory it is given, so pointing it at the checkout would
//! also read files git ignores.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use super::git_output;
use crate::{exit_code_label, git_command};

/// The base and head trees, written under a scratch directory. The head is a
/// revision when one is given, otherwise the working tree with uncommitted
/// and untracked files.
pub struct Trees {
    pub base: PathBuf,
    pub head: PathBuf,
}

impl Trees {
    pub fn export(
        root: &Path,
        base: &str,
        head: Option<&str>,
        scratch: &Path,
    ) -> Result<Self, String> {
        let trees = Self {
            base: scratch.join("base"),
            head: scratch.join("head"),
        };
        export_revision(root, base, &trees.base, &scratch.join("base.index"))?;
        match head {
            Some(head) => export_revision(root, head, &trees.head, &scratch.join("head.index"))?,
            None => copy_worktree(root, &trees.head)?,
        }
        Ok(trees)
    }
}

/// Writes `rev`'s `.rs` files under `dest` through the throwaway index
/// `index`, leaving the repository's own index and worktree list alone.
fn export_revision(root: &Path, rev: &str, dest: &Path, index: &Path) -> Result<(), String> {
    let git_with_index = || {
        let mut command = git_command(root);
        command.env("GIT_INDEX_FILE", index);
        command
    };
    let read_tree = git_with_index().args(["read-tree", rev]).output();
    check(read_tree, "git read-tree")?;

    let mut list = git_with_index()
        .args(["ls-files", "-z", "--", "*.rs"])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to run git ls-files: {error}"))?;
    let listed = list
        .stdout
        .take()
        .ok_or_else(|| "git ls-files has no stdout".to_string())?;
    let checkout = git_with_index()
        .args(["checkout-index", "-z", "--stdin"])
        .arg(format!("--prefix={}/", dest.display()))
        .stdin(listed)
        .output();
    let listing = list
        .wait()
        .map_err(|error| format!("failed to wait for git ls-files: {error}"))?;
    check(checkout, "git checkout-index")?;
    if !listing.success() {
        return Err(format!(
            "git ls-files exited with {}",
            exit_code_label(listing.code())
        ));
    }
    Ok(())
}

/// Copies the `.rs` files git tracks, or would track, from the checkout.
fn copy_worktree(root: &Path, dest: &Path) -> Result<(), String> {
    let listed = git_output(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.rs",
        ],
    )?;
    listed
        .split('\0')
        .filter(|file| !file.is_empty())
        .map(|file| (root.join(file), dest.join(file)))
        // A file deleted in the working tree but not yet staged is still listed.
        .filter(|(source, _)| source.is_file())
        .try_for_each(|(source, target)| copy_file(&source, &target))
}

fn copy_file(source: &Path, target: &Path) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    fs::copy(source, target)
        .map(drop)
        .map_err(|error| format!("failed to copy {}: {error}", source.display()))
}

fn check(output: std::io::Result<Output>, label: &str) -> Result<(), String> {
    let output = output.map_err(|error| format!("failed to run {label}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{label} exited with {}: {}",
            exit_code_label(output.status.code()),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The base paths the change renamed, mapped to their new paths. Without
/// `head` the change is the working tree.
pub fn renames(
    root: &Path,
    base: &str,
    head: Option<&str>,
) -> Result<BTreeMap<String, String>, String> {
    let mut args = vec!["diff", "-z", "--name-status", "-M", base];
    args.extend(head);
    Ok(parse_renames(&git_output(root, &args)?))
}

/// Reads `git diff -z --name-status` output, where a rename or copy is
/// followed by two paths and every other status by one.
fn parse_renames(name_status: &str) -> BTreeMap<String, String> {
    let mut fields = name_status.split('\0');
    let mut renames = BTreeMap::new();
    while let Some(status) = fields.next() {
        match status.chars().next() {
            Some('R') => {
                if let (Some(from), Some(to)) = (fields.next(), fields.next()) {
                    renames.insert(from.to_owned(), to.to_owned());
                }
            }
            Some('C') => {
                fields.nth(1);
            }
            Some(_) => {
                fields.next();
            }
            None => {}
        }
    }
    renames
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_root;

    #[test]
    fn renames_are_read_from_name_status_output() {
        let output = "M\0src/kept.rs\0R087\0src/old.rs\0src/new.rs\0C100\0src/a.rs\0src/b.rs\0A\0src/added.rs\0";

        let renames = parse_renames(output);

        assert_eq!(
            renames,
            BTreeMap::from([("src/old.rs".to_owned(), "src/new.rs".to_owned())])
        );
    }

    #[test]
    fn a_revision_exports_only_its_rust_files() {
        let root = repo_root().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let dest = scratch.path().join("tree");

        export_revision(&root, "HEAD", &dest, &scratch.path().join("index")).unwrap();

        assert!(dest.join("xtask/src/main.rs").is_file());
        assert!(!dest.join("xtask/Cargo.toml").exists());
    }

    #[test]
    fn the_working_tree_copy_includes_untracked_rust_files_only() {
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = git_command(repo.path()).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "--quiet"]);
        fs::write(repo.path().join("lib.rs"), "fn tracked() {}\n").unwrap();
        fs::write(repo.path().join("Cargo.toml"), "[package]\n").unwrap();
        git(&["add", "lib.rs", "Cargo.toml"]);
        fs::create_dir(repo.path().join("src")).unwrap();
        fs::write(repo.path().join("src/new.rs"), "fn untracked() {}\n").unwrap();
        let dest = tempfile::tempdir().unwrap();

        copy_worktree(repo.path(), dest.path()).unwrap();

        assert!(dest.path().join("lib.rs").is_file());
        assert!(dest.path().join("src/new.rs").is_file());
        assert!(!dest.path().join("Cargo.toml").exists());
    }
}
