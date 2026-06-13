use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use gix::bstr::ByteSlice;
use globset::{Glob, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;

use crate::{CacheError, FileEntry, Result};

const COMBINED_OUTPUTS_HASH_DOMAIN: &[u8] = b"luchta-cache:combined-outputs:v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    Literal,
    Glob,
}

#[must_use]
pub fn classify_pattern(pattern: &str) -> PatternKind {
    if pattern.contains(['*', '?', '[', '{']) {
        PatternKind::Glob
    } else {
        PatternKind::Literal
    }
}

pub fn resolve_inputs(base_dir: &Path, patterns: &[String]) -> Result<Vec<FileEntry>> {
    let base_prefix = worktree_relative_base_dir(base_dir)?;
    let package_paths = GitTrackedInputLister::new(base_prefix).list(base_dir)?;
    resolve_with_candidates(base_dir, patterns, package_paths, &StdFs)
}

pub fn resolve_outputs(base_dir: &Path, patterns: &[String]) -> Result<Vec<FileEntry>> {
    resolve_with(base_dir, patterns, &FilesystemLister, &StdFs)
}

fn resolve_with_candidates(
    base_dir: &Path,
    patterns: &[String],
    candidates: Vec<PathBuf>,
    file_reader: &dyn FileReader,
) -> Result<Vec<FileEntry>> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let literal_paths = patterns
        .iter()
        .filter(|pattern| classify_pattern(pattern) == PatternKind::Literal)
        .map(PathBuf::from)
        .collect::<Vec<_>>();

    let glob_patterns = patterns
        .iter()
        .filter(|pattern| classify_pattern(pattern) == PatternKind::Glob)
        .cloned()
        .collect::<Vec<_>>();

    let mut resolved_paths = BTreeSet::new();
    resolved_paths.extend(literal_paths.iter().cloned());

    if !glob_patterns.is_empty() {
        let globset = build_globset(&glob_patterns)?;
        for candidate in candidates {
            if globset.is_match(candidate.as_path()) {
                resolved_paths.insert(candidate);
            }
        }
    }

    resolved_paths
        .into_iter()
        .map(|path| file_entry_from_path(base_dir, path, file_reader))
        .collect()
}

#[must_use]
pub fn combined_outputs_hash(entries: &[FileEntry]) -> [u8; 32] {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|left, right| left.path.cmp(&right.path));

    let mut hasher = blake3::Hasher::new();
    hasher.update(COMBINED_OUTPUTS_HASH_DOMAIN);
    hasher.update(&(sorted.len() as u64).to_le_bytes());

    for entry in sorted {
        let path = entry.path.as_bytes();
        hasher.update(&(path.len() as u64).to_le_bytes());
        hasher.update(path);
        hasher.update(&[u8::from(entry.absent)]);
        hasher.update(&entry.hash);
    }

    *hasher.finalize().as_bytes()
}

fn resolve_with(
    base_dir: &Path,
    patterns: &[String],
    candidate_lister: &dyn CandidateLister,
    file_reader: &dyn FileReader,
) -> Result<Vec<FileEntry>> {
    let candidates = candidate_lister.list(base_dir)?;
    resolve_with_candidates(base_dir, patterns, candidates, file_reader)
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).map_err(CacheError::InvalidGlob)?);
    }
    builder.build().map_err(CacheError::BuildGlobSet)
}

fn file_entry_from_path(
    base_dir: &Path,
    relative_path: PathBuf,
    file_reader: &dyn FileReader,
) -> Result<FileEntry> {
    let absolute_path = base_dir.join(&relative_path);
    if !absolute_path.exists() {
        return Ok(FileEntry::absent(normalize_path(&relative_path)));
    }

    let metadata = fs::metadata(&absolute_path)?;
    let hash = file_reader.blake3_file(&absolute_path)?;
    Ok(FileEntry {
        path: normalize_path(&relative_path),
        size: metadata.len(),
        mtime_ns: modified_time_ns(&metadata)?,
        hash,
        absent: false,
    })
}

fn worktree_relative_base_dir(base_dir: &Path) -> Result<PathBuf> {
    let repo = gix::discover(base_dir).map_err(|err| {
        CacheError::Git(format!(
            "failed to open git repo at {}: {err}",
            base_dir.display()
        ))
    })?;
    let work_dir = repo.workdir().ok_or_else(|| {
        CacheError::Git(format!(
            "repository at {} has no worktree for input resolution",
            base_dir.display()
        ))
    })?;
    let relative = base_dir
        .strip_prefix(work_dir)
        .map_err(|err| CacheError::StripBaseDir(err.to_string()))?;
    Ok(relative.to_path_buf())
}

fn modified_time_ns(metadata: &fs::Metadata) -> Result<i128> {
    let modified = metadata.modified()?;
    let duration = modified
        .duration_since(UNIX_EPOCH)
        .map_err(|err| CacheError::InvalidMtime(err.to_string()))?;
    Ok(i128::from(duration.as_secs()) * 1_000_000_000 + i128::from(duration.subsec_nanos()))
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

trait CandidateLister {
    fn list(&self, base_dir: &Path) -> Result<Vec<PathBuf>>;
}

struct GitTrackedInputLister {
    base_prefix: PathBuf,
}

impl GitTrackedInputLister {
    fn new(base_prefix: PathBuf) -> Self {
        Self { base_prefix }
    }

    fn to_package_relative_path(&self, repo_relative_path: &Path) -> Option<PathBuf> {
        if self.base_prefix.as_os_str().is_empty() {
            return Some(repo_relative_path.to_path_buf());
        }
        repo_relative_path
            .strip_prefix(&self.base_prefix)
            .ok()
            .map(Path::to_path_buf)
    }

    fn worktree_relative_path<'a>(&self, worktree_root: &Path, path: &'a Path) -> Result<&'a Path> {
        path.strip_prefix(worktree_root)
            .map_err(|err| CacheError::StripBaseDir(err.to_string()))
    }
}

impl CandidateLister for GitTrackedInputLister {
    fn list(&self, base_dir: &Path) -> Result<Vec<PathBuf>> {
        let repo = gix::discover(base_dir).map_err(|err| {
            CacheError::Git(format!(
                "failed to open git repo at {}: {err}",
                base_dir.display()
            ))
        })?;
        let worktree = repo.worktree().ok_or_else(|| {
            CacheError::Git(format!(
                "repository at {} has no worktree for input resolution",
                base_dir.display()
            ))
        })?;
        let worktree_root = worktree.base().to_path_buf();
        let git_dir = repo.git_dir().to_path_buf();
        let mut excludes = worktree.excludes(None).map_err(|err| {
            CacheError::Git(format!(
                "failed to initialize git ignore stack at {}: {err}",
                base_dir.display()
            ))
        })?;

        let mut paths = BTreeSet::new();
        let mut walker = WalkDir::new(base_dir).into_iter();
        while let Some(entry) = walker.next() {
            let entry = entry?;
            let path = entry.path();

            if path == git_dir || path.starts_with(&git_dir) {
                if entry.file_type().is_dir() {
                    walker.skip_current_dir();
                }
                continue;
            }

            if path != base_dir {
                let worktree_relative_path = self.worktree_relative_path(&worktree_root, path)?;
                let repo_relative_path = normalize_path(worktree_relative_path);
                let is_excluded = excludes
                    .at_entry(repo_relative_path.as_bytes().as_bstr(), None)
                    .map_err(CacheError::Io)?
                    .is_excluded();
                if is_excluded {
                    if entry.file_type().is_dir() {
                        walker.skip_current_dir();
                    }
                    continue;
                }
            }

            if !entry.file_type().is_file() {
                continue;
            }

            let worktree_relative_path = self.worktree_relative_path(&worktree_root, path)?;
            let Some(package_relative_path) = self.to_package_relative_path(worktree_relative_path)
            else {
                continue;
            };
            paths.insert(package_relative_path);
        }

        Ok(paths.into_iter().collect())
    }
}

struct FilesystemLister;

impl CandidateLister for FilesystemLister {
    fn list(&self, base_dir: &Path) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for entry in WalkDir::new(base_dir) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(base_dir)
                .map_err(|err| CacheError::StripBaseDir(err.to_string()))?;
            paths.push(relative.to_path_buf());
        }
        paths.sort();
        Ok(paths)
    }
}

trait FileReader {
    fn blake3_file(&self, path: &Path) -> Result<[u8; 32]>;
}

struct StdFs;

impl FileReader for StdFs {
    fn blake3_file(&self, path: &Path) -> Result<[u8; 32]> {
        let mut file = fs::File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(*hasher.finalize().as_bytes())
    }
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

    use super::{
        classify_pattern, combined_outputs_hash, resolve_inputs, resolve_outputs, PatternKind,
    };
    use crate::FileEntry;

    #[test]
    fn literal_missing_vs_literal_present_produce_different_entries_and_hashes() {
        let repo = TestRepo::new();
        let patterns = vec!["dist/output.js".to_string()];

        let missing = resolve_outputs(repo.path(), &patterns).unwrap();
        repo.write_file("dist/output.js", "console.log('hi');");
        let present = resolve_outputs(repo.path(), &patterns).unwrap();

        assert_eq!(missing, vec![FileEntry::absent("dist/output.js")]);
        assert_eq!(present.len(), 1);
        assert!(!present[0].absent);
        assert_ne!(missing, present);
        assert_ne!(
            combined_outputs_hash(&missing),
            combined_outputs_hash(&present)
        );
    }

    #[test]
    fn adding_untracked_non_ignored_matching_input_file_changes_resolved_set() {
        let repo = TestRepo::new();
        repo.write_file("src/one.ts", "export const one = 1;\n");
        repo.git_add_and_commit_all();

        let patterns = vec!["src/**/*.ts".to_string()];
        let before = resolve_inputs(repo.path(), &patterns).unwrap();

        repo.write_file("src/two.ts", "export const two = 2;\n");
        let after = resolve_inputs(repo.path(), &patterns).unwrap();

        assert_eq!(
            before
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/one.ts"]
        );
        assert_eq!(
            after
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/one.ts", "src/two.ts"]
        );
        assert_ne!(before, after);
    }

    #[test]
    fn gitignored_file_excluded_from_inputs_but_included_in_outputs() {
        let repo = TestRepo::new();
        repo.write_file(".gitignore", "ignored/\n");
        repo.write_file("ignored/out.txt", "ignored");
        repo.write_file("tracked/in.txt", "tracked");
        repo.git_add_and_commit_all();

        let patterns = vec!["**/*.txt".to_string()];
        let inputs = resolve_inputs(repo.path(), &patterns).unwrap();
        let outputs = resolve_outputs(repo.path(), &patterns).unwrap();

        assert_eq!(
            inputs
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["tracked/in.txt"]
        );
        assert_eq!(
            outputs
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["ignored/out.txt", "tracked/in.txt"]
        );
    }

    #[test]
    fn new_gitignored_matching_input_file_is_excluded() {
        let repo = TestRepo::new();
        repo.write_file(".gitignore", "ignored/\n");
        repo.write_file("src/one.ts", "export const one = 1;\n");
        repo.git_add_and_commit_all();

        let patterns = vec!["**/*.ts".to_string()];
        let before = resolve_inputs(repo.path(), &patterns).unwrap();

        repo.write_file("ignored/two.ts", "export const two = 2;\n");
        let after = resolve_inputs(repo.path(), &patterns).unwrap();

        assert_eq!(
            before
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/one.ts"]
        );
        assert_eq!(after, before);
    }

    #[test]
    fn ordering_is_deterministic_regardless_of_pattern_order() {
        let repo = TestRepo::new();
        repo.write_file("b.txt", "b");
        repo.write_file("a.txt", "a");
        repo.git_add_and_commit_all();

        let first =
            resolve_inputs(repo.path(), &["b.txt".to_string(), "a.txt".to_string()]).unwrap();
        let second =
            resolve_inputs(repo.path(), &["a.txt".to_string(), "b.txt".to_string()]).unwrap();

        let expected = vec!["a.txt", "b.txt"];
        assert_eq!(
            first
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            second
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(first, second);
    }

    #[test]
    fn glob_resolution_is_stable_across_repeated_calls() {
        let repo = TestRepo::new();
        repo.write_file("src/one.txt", "one\n");
        repo.write_file("src/nested/two.txt", "two\n");
        repo.git_add_and_commit_all();

        let patterns = vec!["src/**/*.txt".to_string()];
        let first = resolve_inputs(repo.path(), &patterns).unwrap();
        let second = resolve_inputs(repo.path(), &patterns).unwrap();

        assert_eq!(
            first
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/nested/two.txt", "src/one.txt"]
        );
        assert_eq!(first, second);
    }

    #[test]
    fn glob_resolution_from_package_dir_uses_package_relative_paths() {
        let repo = TestRepo::new();
        repo.write_file("packages/app/src/seed.txt", "seed\n");
        repo.git_add_and_commit_all();

        let patterns = vec!["src/**/*.txt".to_string()];
        let resolved = resolve_inputs(&repo.path().join("packages/app"), &patterns).unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].path, "src/seed.txt");
    }

    #[test]
    fn empty_pattern_set_and_absent_entry_have_distinct_hashes() {
        let repo = TestRepo::new();
        let empty = resolve_outputs(repo.path(), &[]).unwrap();
        let absent = vec![FileEntry::absent("dist/output.js")];

        assert!(empty.is_empty());
        assert_ne!(
            combined_outputs_hash(&empty),
            combined_outputs_hash(&absent)
        );
    }

    #[test]
    fn classify_pattern_detects_wildcards() {
        assert_eq!(classify_pattern("src/main.ts"), PatternKind::Literal);
        for pattern in ["src/*.ts", "src/?.ts", "src/[ab].ts", "src/{a,b}.ts"] {
            assert_eq!(classify_pattern(pattern), PatternKind::Glob);
        }
    }

    struct TestRepo {
        root: TempDir,
    }

    impl TestRepo {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            git(root.path(), ["init"]);
            git(root.path(), ["config", "user.name", "Luchta Tests"]);
            git(root.path(), ["config", "user.email", "luchta@example.com"]);
            Self { root }
        }

        fn path(&self) -> &Path {
            self.root.path()
        }

        fn write_file(&self, relative: &str, contents: &str) {
            let path = self.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }

        fn git_add(&self, paths: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) {
            let status = std::process::Command::new("git")
                .arg("add")
                .args(paths)
                .current_dir(self.path())
                .status()
                .unwrap();
            assert!(status.success());
        }

        fn git_add_and_commit_all(&self) {
            static COUNTER: AtomicU64 = AtomicU64::new(1);
            self.git_add(["."]);
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
