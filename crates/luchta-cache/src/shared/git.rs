use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitKey {
    Clean(String),
    Dirty(String),
    Unavailable,
}

pub fn resolve_commit_key(repo_root: &Path) -> CommitKey {
    let repo = match gix::discover(repo_root) {
        Ok(repo) => repo,
        Err(_) => return CommitKey::Unavailable,
    };

    let head = match repo.head() {
        Ok(head) => head,
        Err(_) => return CommitKey::Unavailable,
    };

    if head.is_detached() || head.is_unborn() {
        return CommitKey::Unavailable;
    }

    let commit = match head.id() {
        Some(id) => id.to_string(),
        None => return CommitKey::Unavailable,
    };

    match repo.is_dirty() {
        Ok(true) => CommitKey::Dirty(format!("{commit}-dirty")),
        Ok(false) => CommitKey::Clean(commit),
        Err(_) => CommitKey::Unavailable,
    }
}

/// Ordered newest-first list of snapshot keys to consult on read.
pub fn candidate_commit_keys(repo_root: &Path, history_len: usize) -> Vec<String> {
    if history_len == 0 {
        return Vec::new();
    }

    let repo = match gix::discover(repo_root) {
        Ok(repo) => repo,
        Err(_) => return Vec::new(),
    };

    let head = match repo.head() {
        Ok(head) => head,
        Err(_) => return Vec::new(),
    };

    if head.is_unborn() {
        return Vec::new();
    }

    let mut next_commit = match head.id() {
        Some(id) => id.detach(),
        None => return Vec::new(),
    };

    let mut keys = Vec::with_capacity(history_len.saturating_mul(2));
    let commit_graph = match repo.commit_graph_if_enabled() {
        Ok(graph) => graph,
        Err(_) => return Vec::new(),
    };
    let mut graph: gix::revwalk::Graph<'_, '_, ()> = repo.revision_graph(commit_graph.as_ref());

    for _ in 0..history_len {
        let commit = match graph.lookup(&next_commit) {
            Ok(commit) => commit,
            Err(_) => return Vec::new(),
        };

        let hash = next_commit.to_string();
        keys.push(hash.clone());
        keys.push(format!("{hash}-dirty"));

        let mut parent_ids = commit.iter_parents();
        let first_parent = match parent_ids.next() {
            Some(Ok(parent_id)) => parent_id,
            Some(Err(_)) => return Vec::new(),
            None => break,
        };

        next_commit = first_parent;
    }

    keys
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use tempfile::TempDir;

    use super::{candidate_commit_keys, resolve_commit_key, CommitKey};

    #[test]
    fn clean_repo_returns_clean_commit_key() {
        let repo = TestRepo::new();
        repo.write_file("tracked.txt", "hello\n");
        repo.git_add_and_commit_all();

        let expected = repo.head_commit();
        assert_eq!(resolve_commit_key(repo.path()), CommitKey::Clean(expected));
    }

    #[test]
    fn modified_tracked_file_returns_dirty_commit_key() {
        let repo = TestRepo::new();
        repo.write_file("tracked.txt", "hello\n");
        repo.git_add_and_commit_all();

        let expected = repo.head_commit();
        repo.write_file("tracked.txt", "changed\n");

        assert_eq!(
            resolve_commit_key(repo.path()),
            CommitKey::Dirty(format!("{expected}-dirty"))
        );
    }

    #[test]
    fn ignored_file_only_keeps_repo_clean() {
        let repo = TestRepo::new();
        repo.write_file("tracked.txt", "hello\n");
        repo.write_file(".gitignore", "ignored.log\n");
        repo.git_add_and_commit_all();

        let expected = repo.head_commit();
        repo.write_file("ignored.log", "noise\n");

        assert_eq!(resolve_commit_key(repo.path()), CommitKey::Clean(expected));
    }

    #[test]
    fn candidate_commit_keys_returns_newest_first_bare_then_dirty_pairs() {
        let repo = TestRepo::new();
        let commits = repo.create_linear_history(4);

        assert_eq!(
            candidate_commit_keys(repo.path(), 3),
            expected_keys(&commits[..3])
        );
    }

    #[test]
    fn candidate_commit_keys_returns_short_history_when_history_len_exceeds_reachable_commits() {
        let repo = TestRepo::new();
        let commits = repo.create_linear_history(2);

        assert_eq!(
            candidate_commit_keys(repo.path(), 5),
            expected_keys(&commits)
        );
    }

    #[test]
    fn candidate_commit_keys_returns_empty_for_non_git_dir() {
        let dir = TempDir::new().unwrap();
        assert!(candidate_commit_keys(dir.path(), 5).is_empty());
    }

    #[test]
    fn candidate_commit_keys_returns_empty_for_unborn_head() {
        let repo = TestRepo::new();
        assert!(candidate_commit_keys(repo.path(), 5).is_empty());
    }

    #[test]
    fn non_git_dir_is_unavailable() {
        let dir = TempDir::new().unwrap();
        assert_eq!(resolve_commit_key(dir.path()), CommitKey::Unavailable);
    }

    fn expected_keys(commits: &[String]) -> Vec<String> {
        commits
            .iter()
            .flat_map(|commit| [commit.clone(), format!("{commit}-dirty")])
            .collect()
    }

    struct TestRepo {
        root: TempDir,
    }

    impl TestRepo {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            git(root.path(), ["init"]);
            git(root.path(), ["config", "user.name", "Luchta Tests"]);
            git(root.path(), ["config", "user.email", "luchta@example.com"]);
            Self { root }
        }

        fn path(&self) -> &Path {
            self.root.path()
        }

        fn create_linear_history(&self, count: usize) -> Vec<String> {
            let mut commits = Vec::with_capacity(count);
            for index in 0..count {
                self.write_file("tracked.txt", &format!("commit-{index}\n"));
                self.git_add_and_commit_all();
                commits.push(self.head_commit());
            }
            commits.reverse();
            commits
        }

        fn write_file(&self, relative: &str, contents: &str) {
            let path = self.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }

        fn git_add_and_commit_all(&self) {
            static COUNTER: AtomicU64 = AtomicU64::new(1);
            git(self.path(), ["add", "."]);
            let message = format!(
                "commit-{}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            git(self.path(), ["commit", "-m", &message]);
        }

        fn head_commit(&self) -> String {
            match resolve_commit_key(self.path()) {
                CommitKey::Clean(commit) => commit,
                other => panic!("expected clean commit key, got {other:?}"),
            }
        }
    }

    fn git(repo: &Path, args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
