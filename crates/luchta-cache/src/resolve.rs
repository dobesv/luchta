use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};

use gix::bstr::ByteSlice;
use globset::{Glob, GlobSet, GlobSetBuilder};
use luchta_types::{classify_pattern, InputSemantics};
use walkdir::WalkDir;

use crate::{CacheError, FileEntry, Result};

const COMBINED_OUTPUTS_HASH_DOMAIN: &[u8] = b"luchta-cache:combined-outputs:v1";

/// Run-scoped memo of directory listings, shared across every task in a single
/// `luchta run` so each package directory is walked once rather than once per
/// task.
///
/// # Snapshot semantics
///
/// Entries are keyed by `base_dir` and are deliberately **not** invalidated for
/// the lifetime of the cache. This encodes an intentional model: change
/// detection resolves a task's inputs against the state of the git-tracked
/// (non-ignored) files as observed at the start of the build. A build's own
/// task outputs are captured via the separate cache-record write path — which
/// re-resolves freshly, bypassing this cache (see `resolve_cache_inputs` /
/// `resolve_cache_outputs` in the CLI) — so producer/consumer output flow is
/// driven by dependency output hashes, not by re-listing a directory mid-run.
///
/// Because of this, a `ListingCache` MUST be created fresh per run and dropped
/// when the run ends. It must never be a process-lifetime `static`: reusing a
/// listing across separate runs (or across `watch` rebuild cycles) would hide
/// files created or removed between runs.
///
/// Cloning shares the underlying maps (`Arc`), so a single logical cache can be
/// handed to every per-task resolver.
#[derive(Debug, Clone, Default)]
pub struct ListingCache {
    input_candidates: Arc<Mutex<HashMap<PathBuf, Arc<Vec<PathBuf>>>>>,
    output_candidates: Arc<Mutex<HashMap<PathBuf, Arc<Vec<PathBuf>>>>>,
}

impl ListingCache {
    fn get_input_candidates(&self, base_dir: &Path) -> Option<Arc<Vec<PathBuf>>> {
        self.input_candidates.lock().ok()?.get(base_dir).cloned()
    }

    fn insert_input_candidates(&self, base_dir: &Path, candidates: Arc<Vec<PathBuf>>) {
        if let Ok(mut cache) = self.input_candidates.lock() {
            cache.insert(base_dir.to_path_buf(), candidates);
        }
    }

    fn get_output_candidates(&self, base_dir: &Path) -> Option<Arc<Vec<PathBuf>>> {
        self.output_candidates.lock().ok()?.get(base_dir).cloned()
    }

    fn insert_output_candidates(&self, base_dir: &Path, candidates: Arc<Vec<PathBuf>>) {
        if let Ok(mut cache) = self.output_candidates.lock() {
            cache.insert(base_dir.to_path_buf(), candidates);
        }
    }
}

/// Optional inputs that let resolution skip redundant work.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolveOptions<'a> {
    /// The matching entries from the task's prior run record. When a resolved
    /// file's path, size, and `mtime_ns` match its prior entry, the prior
    /// content hash is reused instead of re-hashing the file.
    pub prior_entries: &'a [FileEntry],
    /// Run-scoped directory-listing cache shared across tasks. `None` resolves
    /// against a fresh walk (used by one-shot callers such as `luchta why`).
    pub listing_cache: Option<&'a ListingCache>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveRequest {
    pub base_dir: PathBuf,
    pub pattern: String,
    pub semantics: InputSemantics,
}

pub fn resolve_inputs(base_dir: &Path, patterns: &[String]) -> Result<Vec<FileEntry>> {
    resolve_inputs_with_options(base_dir, patterns, ResolveOptions::default())
}

/// Resolve input `patterns` under `base_dir`, honoring [`ResolveOptions`] for
/// prior-hash reuse and the run-scoped listing cache.
pub fn resolve_inputs_with_options(
    base_dir: &Path,
    patterns: &[String],
    options: ResolveOptions<'_>,
) -> Result<Vec<FileEntry>> {
    let requests = patterns
        .iter()
        .cloned()
        .map(|pattern| ResolveRequest {
            semantics: classify_pattern(&pattern),
            base_dir: base_dir.to_path_buf(),
            pattern,
        })
        .collect::<Vec<_>>();
    resolve_inputs_with_semantics_and_options(&requests, options)
}

pub fn resolve_inputs_with_semantics(requests: &[ResolveRequest]) -> Result<Vec<FileEntry>> {
    resolve_inputs_with_semantics_and_options(requests, ResolveOptions::default())
}

/// Resolve pre-classified input [`ResolveRequest`]s, honoring [`ResolveOptions`].
///
/// Deduplicates candidate listings per `base_dir` within the call (and across
/// the run when a `listing_cache` is supplied), then merges and dedupes the
/// resulting entries.
pub fn resolve_inputs_with_semantics_and_options(
    requests: &[ResolveRequest],
    options: ResolveOptions<'_>,
) -> Result<Vec<FileEntry>> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let mut candidate_cache = HashMap::<PathBuf, Vec<PathBuf>>::new();
    let mut base_dir_prefix_cache = HashMap::<PathBuf, PathBuf>::new();
    let mut merged_entries = Vec::new();
    let mut worktree_roots = Vec::new();
    let prior_by_path = prior_entries_by_path(options.prior_entries);

    for request in requests {
        let base_dir_prefix =
            qualified_base_dir_prefix(&request.base_dir, &mut base_dir_prefix_cache)?.clone();
        // The repo-relative prefix is exactly the tail of `base_dir` below the
        // worktree root, so stripping it yields the worktree root. Entry paths
        // are repo-relative, so this root is what they must be joined to when
        // reconstructing absolute paths for canonical dedup — no path probing.
        let worktree_root = strip_suffix_components(&request.base_dir, &base_dir_prefix);
        if !worktree_roots.contains(&worktree_root) {
            worktree_roots.push(worktree_root);
        }

        let base = ResolvedBase {
            dir: &request.base_dir,
            prefix: &base_dir_prefix,
        };
        merged_entries.extend(resolve_single_request(
            request,
            base,
            &mut candidate_cache,
            &StdFs,
            &prior_by_path,
            options,
        )?);
    }

    Ok(dedupe_and_sort_entries(merged_entries, &worktree_roots))
}

/// A resolved input base directory paired with the repo-relative prefix used to
/// qualify the paths of files discovered beneath it. Bundling the two keeps the
/// pair together as a single abstraction wherever inputs are turned into
/// `FileEntry` values.
#[derive(Clone, Copy)]
struct ResolvedBase<'a> {
    dir: &'a Path,
    prefix: &'a Path,
}

fn prior_entries_by_path(prior_entries: &[FileEntry]) -> HashMap<&str, &FileEntry> {
    prior_entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect()
}

fn resolve_single_request(
    request: &ResolveRequest,
    base: ResolvedBase<'_>,
    candidate_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
    file_reader: &dyn FileReader,
    prior_by_path: &HashMap<&str, &FileEntry>,
    options: ResolveOptions<'_>,
) -> Result<Vec<FileEntry>> {
    match request.semantics {
        InputSemantics::Literal => {
            resolve_literal_request(request, base, file_reader, prior_by_path)
        }
        InputSemantics::Wildcard => resolve_wildcard_request(
            request,
            candidate_cache,
            base,
            file_reader,
            prior_by_path,
            options.listing_cache,
        ),
    }
}

fn resolve_literal_request(
    request: &ResolveRequest,
    base: ResolvedBase<'_>,
    file_reader: &dyn FileReader,
    prior_by_path: &HashMap<&str, &FileEntry>,
) -> Result<Vec<FileEntry>> {
    Ok(vec![file_entry_from_path(
        base,
        PathBuf::from(&request.pattern),
        file_reader,
        prior_by_path,
    )?])
}

fn resolve_wildcard_request(
    request: &ResolveRequest,
    candidate_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
    base: ResolvedBase<'_>,
    file_reader: &dyn FileReader,
    prior_by_path: &HashMap<&str, &FileEntry>,
    listing_cache: Option<&ListingCache>,
) -> Result<Vec<FileEntry>> {
    let candidates = cached_input_candidates(&request.base_dir, candidate_cache, listing_cache)?;
    resolve_wildcard_with_candidates(
        base,
        &request.pattern,
        candidates,
        file_reader,
        prior_by_path,
    )
}

fn cached_input_candidates(
    base_dir: &Path,
    candidate_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
    listing_cache: Option<&ListingCache>,
) -> Result<Vec<PathBuf>> {
    if let Some(candidates) = candidate_cache.get(base_dir) {
        return Ok(candidates.clone());
    }

    if let Some(candidates) = listing_cache.and_then(|cache| cache.get_input_candidates(base_dir)) {
        let candidates = (*candidates).clone();
        candidate_cache.insert(base_dir.to_path_buf(), candidates.clone());
        return Ok(candidates);
    }

    let base_prefix = worktree_relative_base_dir(base_dir)?;
    let candidates = GitTrackedInputLister::new(base_prefix).list(base_dir)?;
    if let Some(cache) = listing_cache {
        cache.insert_input_candidates(base_dir, Arc::new(candidates.clone()));
    }
    candidate_cache.insert(base_dir.to_path_buf(), candidates.clone());
    Ok(candidates)
}

/// Resolve a task's output files.
///
/// Output `FileEntry.path` values stay **package-relative** (an empty base-dir
/// prefix), unlike inputs which are qualified repo-relative. This split is
/// intentional: outputs are always confined to a single task's own package, so
/// they cannot collide across packages within one record the way cross-package
/// inputs can (see issue #138). Keeping outputs package-relative preserves the
/// existing snapshot/restore path semantics.
pub fn resolve_outputs(base_dir: &Path, patterns: &[String]) -> Result<Vec<FileEntry>> {
    resolve_outputs_with_options(base_dir, patterns, ResolveOptions::default())
}

/// [`resolve_outputs`] variant honoring [`ResolveOptions`] for prior-hash reuse
/// and the run-scoped listing cache.
pub fn resolve_outputs_with_options(
    base_dir: &Path,
    patterns: &[String],
    options: ResolveOptions<'_>,
) -> Result<Vec<FileEntry>> {
    resolve_with(base_dir, patterns, &FilesystemLister, &StdFs, options)
}

fn resolve_with_candidates(
    base_dir: &Path,
    patterns: &[String],
    candidates: Vec<PathBuf>,
    file_reader: &dyn FileReader,
    options: ResolveOptions<'_>,
) -> Result<Vec<FileEntry>> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let literal_paths = patterns
        .iter()
        .filter(|pattern| classify_pattern(pattern) == InputSemantics::Literal)
        .map(PathBuf::from)
        .collect::<Vec<_>>();

    let glob_patterns = patterns
        .iter()
        .filter(|pattern| classify_pattern(pattern) == InputSemantics::Wildcard)
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

    let base = ResolvedBase {
        dir: base_dir,
        prefix: Path::new(""),
    };
    let prior_by_path = prior_entries_by_path(options.prior_entries);
    resolve_file_entries(
        base,
        resolved_paths.into_iter().collect(),
        file_reader,
        &prior_by_path,
    )
}

fn resolve_wildcard_with_candidates(
    base: ResolvedBase<'_>,
    pattern: &str,
    candidates: Vec<PathBuf>,
    file_reader: &dyn FileReader,
    prior_by_path: &HashMap<&str, &FileEntry>,
) -> Result<Vec<FileEntry>> {
    let globset = build_globset(&[pattern.to_string()])?;
    let matched = candidates
        .into_iter()
        .filter(|candidate| globset.is_match(candidate.as_path()))
        .collect::<Vec<_>>();
    resolve_file_entries(base, matched, file_reader, prior_by_path)
}

fn resolve_file_entries(
    base: ResolvedBase<'_>,
    paths: Vec<PathBuf>,
    file_reader: &dyn FileReader,
    prior_by_path: &HashMap<&str, &FileEntry>,
) -> Result<Vec<FileEntry>> {
    paths
        .into_iter()
        .map(|path| file_entry_from_path(base, path, file_reader, prior_by_path))
        .collect()
}

fn dedupe_and_sort_entries(entries: Vec<FileEntry>, worktree_roots: &[PathBuf]) -> Vec<FileEntry> {
    let mut seen_present = HashSet::new();
    let mut seen_absent = HashSet::new();
    let mut deduped = Vec::new();

    for entry in entries {
        if entry.absent {
            if seen_absent.insert(entry.path.clone()) {
                deduped.push(entry);
            }
            continue;
        }

        let dedupe_key =
            canonical_dedupe_key(worktree_roots, &entry.path).unwrap_or_else(|| entry.path.clone());
        if seen_present.insert(dedupe_key) {
            deduped.push(entry);
        }
    }

    deduped.sort_by(|left, right| left.path.cmp(&right.path));
    deduped
}

/// Build a stable dedup key for a present entry by resolving its repo-relative
/// path against the known worktree root(s) and canonicalizing. Because entry
/// paths are repo-relative, joining to the worktree root yields the exact
/// absolute path with no ancestor probing, so two qualified paths pointing at
/// the same physical file collapse to one key.
///
/// In the common single-repo workspace there is exactly one worktree root, so
/// the join is unambiguous. If a batch ever spans multiple repositories, the
/// first root whose join canonicalizes wins; that is deterministic for a given
/// `worktree_roots` ordering and only matters in the unusual case of identical
/// repo-relative paths existing in more than one repo.
fn canonical_dedupe_key(worktree_roots: &[PathBuf], entry_path: &str) -> Option<String> {
    let entry_path = Path::new(entry_path);
    worktree_roots
        .iter()
        .map(|root| root.join(entry_path))
        .find_map(|path| fs::canonicalize(path).ok())
        .map(|path| normalize_path(&path))
}

/// Remove the trailing components of `path` that correspond to `suffix`,
/// yielding the directory `suffix` was relative to. When `suffix` is empty,
/// `path` is returned unchanged.
fn strip_suffix_components(path: &Path, suffix: &Path) -> PathBuf {
    let suffix_len = suffix.components().count();
    let mut result = path.to_path_buf();
    for _ in 0..suffix_len {
        if !result.pop() {
            break;
        }
    }
    result
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
    options: ResolveOptions<'_>,
) -> Result<Vec<FileEntry>> {
    let candidates = cached_output_candidates(base_dir, candidate_lister, options.listing_cache)?;
    resolve_with_candidates(base_dir, patterns, candidates, file_reader, options)
}

fn cached_output_candidates(
    base_dir: &Path,
    candidate_lister: &dyn CandidateLister,
    listing_cache: Option<&ListingCache>,
) -> Result<Vec<PathBuf>> {
    if let Some(candidates) = listing_cache.and_then(|cache| cache.get_output_candidates(base_dir))
    {
        return Ok((*candidates).clone());
    }

    let candidates = candidate_lister.list(base_dir)?;
    if let Some(cache) = listing_cache {
        cache.insert_output_candidates(base_dir, Arc::new(candidates.clone()));
    }
    Ok(candidates)
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).map_err(CacheError::InvalidGlob)?);
    }
    builder.build().map_err(CacheError::BuildGlobSet)
}

fn file_entry_from_path(
    base: ResolvedBase<'_>,
    relative_path: PathBuf,
    file_reader: &dyn FileReader,
    prior_by_path: &HashMap<&str, &FileEntry>,
) -> Result<FileEntry> {
    let absolute_path = base.dir.join(&relative_path);
    let qualified_path = qualify_relative_path(base.prefix, &relative_path);
    // Single stat: a missing path (including a broken symlink) yields NotFound,
    // which we treat as absent. Other IO errors propagate. This replaces the
    // former `exists()` + `metadata()` pair, which stat'd every present file twice.
    let metadata = match fs::metadata(&absolute_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileEntry::absent(qualified_path));
        }
        Err(err) => return Err(err.into()),
    };
    let size = metadata.len();
    let mtime_ns = modified_time_ns(&metadata)?;
    let reused = prior_by_path
        .get(qualified_path.as_str())
        .filter(|entry| !entry.absent && entry.size == size && entry.mtime_ns == mtime_ns)
        .map(|entry| entry.hash);
    let hash = match reused {
        Some(h) => h,
        None => file_reader.blake3_file(&absolute_path)?,
    };
    Ok(FileEntry {
        path: qualified_path,
        size,
        mtime_ns,
        hash,
        absent: false,
    })
}

fn qualified_base_dir_prefix<'a>(
    base_dir: &Path,
    base_dir_prefix_cache: &'a mut HashMap<PathBuf, PathBuf>,
) -> Result<&'a PathBuf> {
    use std::collections::hash_map::Entry;

    match base_dir_prefix_cache.entry(base_dir.to_path_buf()) {
        Entry::Occupied(entry) => Ok(entry.into_mut()),
        Entry::Vacant(entry) => {
            let prefix = worktree_relative_base_dir(base_dir)?;
            Ok(entry.insert(prefix))
        }
    }
}

fn qualify_relative_path(base_dir_prefix: &Path, relative_path: &Path) -> String {
    let qualified = if base_dir_prefix.as_os_str().is_empty() {
        relative_path.to_path_buf()
    } else {
        base_dir_prefix.join(relative_path)
    };
    normalize_path(&qualified)
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
        crate::blake3_file(path)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use tempfile::TempDir;

    use luchta_types::{classify_pattern, InputSemantics};

    use super::{
        combined_outputs_hash, dedupe_and_sort_entries, file_entry_from_path,
        prior_entries_by_path, qualified_base_dir_prefix, resolve_file_entries, resolve_inputs,
        resolve_inputs_with_options, resolve_inputs_with_semantics, resolve_literal_request,
        resolve_outputs, resolve_outputs_with_options, resolve_wildcard_with_candidates,
        strip_suffix_components, CandidateLister, FileReader, ListingCache, ResolveOptions,
        ResolveRequest, ResolvedBase, StdFs,
    };
    use crate::FileEntry;
    use crate::Result;

    #[test]
    fn listing_cache_reuses_walks_within_run_and_fresh_cache_rewalks() {
        let repo = TestRepo::new();
        repo.write_file("src/one.ts", "one\n");
        repo.write_file("src/two.ts", "two\n");
        repo.git_add_and_commit_all();

        let requests = [repo.request("src/**/*.ts", InputSemantics::Wildcard)];
        let first_cache = ListingCache::default();
        let counting = CountingCandidateLister::new(vec![
            Path::new("src/one.ts").to_path_buf(),
            Path::new("src/two.ts").to_path_buf(),
        ]);
        let options = ResolveOptions {
            prior_entries: &[],
            listing_cache: Some(&first_cache),
        };

        let first =
            resolve_inputs_with_semantics_with_lister(&requests, &counting, options).unwrap();
        let second =
            resolve_inputs_with_semantics_with_lister(&requests, &counting, options).unwrap();

        assert_eq!(first, second);
        assert_eq!(counting.calls(), 1);

        let second_cache = ListingCache::default();
        let second_lister = CountingCandidateLister::new(vec![
            Path::new("src/one.ts").to_path_buf(),
            Path::new("src/two.ts").to_path_buf(),
        ]);
        let second_options = ResolveOptions {
            prior_entries: &[],
            listing_cache: Some(&second_cache),
        };
        let _ =
            resolve_inputs_with_semantics_with_lister(&requests, &second_lister, second_options)
                .unwrap();
        assert_eq!(second_lister.calls(), 1);
    }

    #[test]
    fn resolve_input_hashes_reuse_prior_hash_when_metadata_matches() {
        let repo = TestRepo::new();
        repo.write_file("src/app.ts", "console.log('same');\n");
        repo.git_add_and_commit_all();

        let initial = resolve_inputs(repo.path(), &["src/app.ts".to_string()]).unwrap();
        let prior = vec![FileEntry {
            path: "src/app.ts".to_string(),
            hash: [9; 32],
            ..initial[0].clone()
        }];
        let resolved = resolve_inputs_with_options(
            repo.path(),
            &["src/app.ts".to_string()],
            ResolveOptions {
                prior_entries: &prior,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(resolved[0].hash, [9; 32]);
        assert_eq!(resolved[0].size, prior[0].size);
        assert_eq!(resolved[0].mtime_ns, prior[0].mtime_ns);
    }

    #[test]
    fn resolve_input_hashes_rehash_when_metadata_changes() {
        let repo = TestRepo::new();
        repo.write_file("src/app.ts", "console.log('same');\n");
        repo.git_add_and_commit_all();

        let initial = resolve_inputs(repo.path(), &["src/app.ts".to_string()]).unwrap();
        let mut prior = initial[0].clone();
        prior.hash = [9; 32];
        prior.size = prior.size.saturating_add(1);

        let resolved = resolve_inputs_with_options(
            repo.path(),
            &["src/app.ts".to_string()],
            ResolveOptions {
                prior_entries: &[prior],
                ..Default::default()
            },
        )
        .unwrap();

        assert_ne!(resolved[0].hash, [9; 32]);
    }

    #[test]
    fn resolve_file_entries_matches_direct_order_and_values() {
        let repo = TestRepo::new();
        repo.write_file("dist/a.js", "a\n");
        repo.write_file("dist/c.js", "c\n");
        repo.write_file("dist/nested/b.js", "b\n");
        let reader = CountingReader::new();
        let base = ResolvedBase {
            dir: repo.path(),
            prefix: Path::new(""),
        };
        let paths = vec![
            Path::new("dist/a.js").to_path_buf(),
            Path::new("dist/c.js").to_path_buf(),
            Path::new("dist/nested/b.js").to_path_buf(),
        ];
        let prior = Vec::new();
        let prior_by_path = prior_entries_by_path(&prior);

        let batched = resolve_file_entries(base, paths.clone(), &reader, &prior_by_path).unwrap();
        let direct = paths
            .into_iter()
            .map(|path| file_entry_from_path(base, path, &reader, &prior_by_path).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(batched, direct);
    }

    #[test]
    fn resolve_input_hashes_rehash_when_only_mtime_changes() {
        // A file whose size is unchanged but mtime_ns differs from the prior
        // record MUST be re-hashed, not served from the prior hash. Guards the
        // stat fast-path against a same-size in-place edit.
        let repo = TestRepo::new();
        repo.write_file("src/app.ts", "console.log('same');\n");
        repo.git_add_and_commit_all();

        let initial = resolve_inputs(repo.path(), &["src/app.ts".to_string()]).unwrap();
        let mut prior = initial[0].clone();
        prior.hash = [9; 32];
        // Same size, different mtime: only the timestamp diverges from disk.
        prior.mtime_ns = prior.mtime_ns.wrapping_add(1);

        let resolved = resolve_inputs_with_options(
            repo.path(),
            &["src/app.ts".to_string()],
            ResolveOptions {
                prior_entries: &[prior.clone()],
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(resolved[0].size, prior.size, "size unchanged on disk");
        assert_ne!(
            resolved[0].hash, [9; 32],
            "mtime mismatch must force a real re-hash, not reuse the prior hash"
        );
    }

    #[test]
    fn listing_cache_reuses_output_walks_within_run_and_isolates_from_inputs() {
        // The output-listing cache must (a) walk each base dir only once per run
        // and (b) not collide with the input-listing cache keyed by the same dir.
        let repo = TestRepo::new();
        repo.write_file("dist/a.js", "a\n");
        repo.write_file("dist/b.js", "b\n");

        let cache = ListingCache::default();

        // Two output resolves against the same base dir share one walk.
        let first = resolve_outputs_with_options(
            repo.path(),
            &["dist/**/*.js".to_string()],
            ResolveOptions {
                prior_entries: &[],
                listing_cache: Some(&cache),
            },
        )
        .unwrap();
        let second = resolve_outputs_with_options(
            repo.path(),
            &["dist/**/*.js".to_string()],
            ResolveOptions {
                prior_entries: &[],
                listing_cache: Some(&cache),
            },
        )
        .unwrap();

        assert_eq!(first, second);
        assert_eq!(first.len(), 2, "both dist files resolved");

        // The output cache is populated for this base dir; the input cache is not
        // (input and output listings are stored in separate maps).
        assert!(
            cache.get_output_candidates(repo.path()).is_some(),
            "output listing cached after resolve_outputs"
        );
        assert!(
            cache.get_input_candidates(repo.path()).is_none(),
            "resolve_outputs must not populate the input-listing cache"
        );
    }

    fn resolve_inputs_with_semantics_with_lister(
        requests: &[ResolveRequest],
        candidate_lister: &dyn CandidateLister,
        options: ResolveOptions<'_>,
    ) -> Result<Vec<FileEntry>> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }

        let mut candidate_cache = HashMap::<PathBuf, Vec<PathBuf>>::new();
        let mut base_dir_prefix_cache = HashMap::<PathBuf, PathBuf>::new();
        let mut merged_entries = Vec::new();
        let mut worktree_roots = Vec::new();
        let prior_by_path = prior_entries_by_path(options.prior_entries);

        for request in requests {
            let base_dir_prefix =
                qualified_base_dir_prefix(&request.base_dir, &mut base_dir_prefix_cache)?.clone();
            let worktree_root = strip_suffix_components(&request.base_dir, &base_dir_prefix);
            if !worktree_roots.contains(&worktree_root) {
                worktree_roots.push(worktree_root);
            }

            let base = ResolvedBase {
                dir: &request.base_dir,
                prefix: &base_dir_prefix,
            };
            merged_entries.extend(resolve_single_request_for_tests(
                request,
                base,
                &mut candidate_cache,
                &StdFs,
                &prior_by_path,
                options,
                candidate_lister,
            )?);
        }

        Ok(dedupe_and_sort_entries(merged_entries, &worktree_roots))
    }

    fn resolve_single_request_for_tests(
        request: &ResolveRequest,
        base: ResolvedBase<'_>,
        candidate_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
        file_reader: &dyn FileReader,
        prior_by_path: &HashMap<&str, &FileEntry>,
        options: ResolveOptions<'_>,
        candidate_lister: &dyn CandidateLister,
    ) -> Result<Vec<FileEntry>> {
        match request.semantics {
            InputSemantics::Literal => {
                resolve_literal_request(request, base, file_reader, prior_by_path)
            }
            InputSemantics::Wildcard => {
                let candidates = cached_input_candidates_for_tests(
                    &request.base_dir,
                    candidate_cache,
                    options.listing_cache,
                    candidate_lister,
                )?;
                resolve_wildcard_with_candidates(
                    base,
                    &request.pattern,
                    candidates,
                    file_reader,
                    prior_by_path,
                )
            }
        }
    }

    fn cached_input_candidates_for_tests(
        base_dir: &Path,
        candidate_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
        listing_cache: Option<&ListingCache>,
        candidate_lister: &dyn CandidateLister,
    ) -> Result<Vec<PathBuf>> {
        if let Some(candidates) = candidate_cache.get(base_dir) {
            return Ok(candidates.clone());
        }
        if let Some(candidates) =
            listing_cache.and_then(|cache| cache.get_input_candidates(base_dir))
        {
            let candidates = (*candidates).clone();
            candidate_cache.insert(base_dir.to_path_buf(), candidates.clone());
            return Ok(candidates);
        }
        let candidates = candidate_lister.list(base_dir)?;
        if let Some(cache) = listing_cache {
            cache.insert_input_candidates(base_dir, Arc::new(candidates.clone()));
        }
        candidate_cache.insert(base_dir.to_path_buf(), candidates.clone());
        Ok(candidates)
    }

    struct CountingCandidateLister {
        calls: Arc<AtomicU64>,
        candidates: Vec<PathBuf>,
    }

    impl CountingCandidateLister {
        fn new(candidates: Vec<PathBuf>) -> Self {
            Self {
                calls: Arc::new(AtomicU64::new(0)),
                candidates,
            }
        }

        fn calls(&self) -> u64 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl CandidateLister for CountingCandidateLister {
        fn list(&self, _base_dir: &Path) -> Result<Vec<PathBuf>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.candidates.clone())
        }
    }

    #[derive(Default)]
    struct CountingReader {
        calls: Arc<AtomicU64>,
    }

    impl CountingReader {
        fn new() -> Self {
            Self::default()
        }
    }

    impl FileReader for CountingReader {
        fn blake3_file(&self, path: &Path) -> crate::Result<[u8; 32]> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            crate::blake3_file(path)
        }
    }

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
        assert_eq!(resolved[0].path, "packages/app/src/seed.txt");
    }

    #[test]
    fn resolve_outputs_remain_package_relative_in_subdirectory() {
        // Outputs intentionally stay package-relative (unlike inputs, which are
        // qualified repo-relative for #138). Resolving from a package subdir
        // must yield `dist/app.js`, not `pkg-a/dist/app.js`.
        let repo = TestRepo::new();
        repo.write_file("pkg-a/dist/app.js", "built\n");
        repo.git_add_and_commit_all();

        let entries =
            resolve_outputs(&repo.path().join("pkg-a"), &["dist/app.js".to_string()]).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "dist/app.js");
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
        assert_eq!(classify_pattern("src/main.ts"), InputSemantics::Literal);
        for pattern in ["src/*.ts", "src/?.ts", "src/[ab].ts", "src/{a,b}.ts"] {
            assert_eq!(classify_pattern(pattern), InputSemantics::Wildcard);
        }
    }

    #[test]
    fn resolve_inputs_with_semantics_literal_missing_returns_absent_entry() {
        let repo = TestRepo::new();
        repo.write_file("present.txt", "present\n");
        repo.git_add_and_commit_all();

        let entries =
            resolve_inputs_with_semantics(&[repo.request("missing.txt", InputSemantics::Literal)])
                .unwrap();

        assert_eq!(entries, vec![FileEntry::absent("missing.txt")]);
    }

    #[test]
    fn resolve_inputs_with_semantics_wildcard_zero_match_returns_empty() {
        let repo = TestRepo::new();
        repo.write_file("present.txt", "present\n");
        repo.git_add_and_commit_all();

        let entries =
            resolve_inputs_with_semantics(&[repo.request("src/**/*.ts", InputSemantics::Wildcard)])
                .unwrap();

        assert!(entries.is_empty());
    }

    #[test]
    fn resolve_inputs_with_semantics_wildcard_literal_looking_missing_returns_empty() {
        let repo = TestRepo::new();
        repo.write_file("present.txt", "present\n");
        repo.git_add_and_commit_all();

        let entries =
            resolve_inputs_with_semantics(&[repo.request("cfg", InputSemantics::Wildcard)])
                .unwrap();

        assert!(entries.is_empty());
    }

    #[test]
    fn resolve_inputs_with_semantics_dedupes_same_canonical_file() {
        let repo = TestRepo::new();
        repo.write_file("shared/file.txt", "shared\n");
        repo.write_file("pkg-a/anchor.txt", "anchor\n");
        repo.write_file("pkg-b/anchor.txt", "anchor\n");
        repo.git_add_and_commit_all();

        let entries = resolve_inputs_with_semantics(&[
            repo.request_from("pkg-a", "../shared/file.txt", InputSemantics::Literal),
            repo.request_from("pkg-b", "../shared/file.txt", InputSemantics::Literal),
        ])
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "pkg-a/../shared/file.txt");
        assert!(!entries[0].absent);
    }

    #[test]
    fn resolve_inputs_with_semantics_sorts_by_relative_path() {
        let repo = TestRepo::new();
        repo.write_file("a.txt", "a\n");
        repo.write_file("b.txt", "b\n");
        repo.git_add_and_commit_all();

        let entries = resolve_inputs_with_semantics(&[
            repo.request("b.txt", InputSemantics::Literal),
            repo.request("a.txt", InputSemantics::Literal),
        ])
        .unwrap();

        assert_eq!(
            entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            vec!["a.txt", "b.txt"]
        );
    }

    #[test]
    fn resolve_inputs_with_semantics_handles_multiple_base_dirs() {
        let repo = TestRepo::new();
        repo.write_file("pkg-a/src/a.txt", "a\n");
        repo.write_file("pkg-b/src/b.txt", "b\n");
        repo.git_add_and_commit_all();

        let entries = resolve_inputs_with_semantics(&[
            repo.request_from("pkg-a", "src/*.txt", InputSemantics::Wildcard),
            repo.request_from("pkg-b", "src/*.txt", InputSemantics::Wildcard),
        ])
        .unwrap();

        assert_eq!(
            entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            vec!["pkg-a/src/a.txt", "pkg-b/src/b.txt"]
        );
    }

    #[test]
    fn resolve_inputs_with_semantics_distinguishes_same_relative_path_across_packages() {
        let repo = TestRepo::new();
        repo.write_file("pkg-a/src/schema.graphql", "type Query { a: String }\n");
        repo.write_file(
            "pkg-b/src/schema.graphql",
            "type Query { field: String, other: Int }\n",
        );
        repo.git_add_and_commit_all();

        let entries = resolve_inputs_with_semantics(&[
            repo.request_from("pkg-a", "src/schema.graphql", InputSemantics::Literal),
            repo.request_from("pkg-b", "src/schema.graphql", InputSemantics::Literal),
        ])
        .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["pkg-a/src/schema.graphql", "pkg-b/src/schema.graphql"]
        );
        assert_ne!(entries[0].path, entries[1].path);
    }

    #[test]
    fn resolve_inputs_with_semantics_dedup_unaffected_by_shadow_directory() {
        // Guards against a dedup heuristic that reconstructs a file's absolute
        // path by probing ancestors: a nested directory mirroring the package
        // name (`pkg-a/pkg-a/...`) must not be mistaken for the real input.
        // Dedup keys off the worktree root join, so the shadow path is ignored.
        let repo = TestRepo::new();
        repo.write_file("pkg-a/src/schema.graphql", "real\n");
        repo.write_file("pkg-a/pkg-a/src/schema.graphql", "shadow\n");
        repo.git_add_and_commit_all();

        let entries = resolve_inputs_with_semantics(&[repo.request_from(
            "pkg-a",
            "src/schema.graphql",
            InputSemantics::Literal,
        )])
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "pkg-a/src/schema.graphql");
        assert!(!entries[0].absent);
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

        /// Create a ResolveRequest from repo root with given pattern and semantics.
        fn request(&self, pattern: &str, semantics: InputSemantics) -> ResolveRequest {
            ResolveRequest {
                base_dir: self.path().to_path_buf(),
                pattern: pattern.to_string(),
                semantics,
            }
        }

        /// Create a ResolveRequest from a subdirectory within the repo.
        fn request_from(
            &self,
            subdir: &str,
            pattern: &str,
            semantics: InputSemantics,
        ) -> ResolveRequest {
            ResolveRequest {
                base_dir: self.path().join(subdir),
                pattern: pattern.to_string(),
                semantics,
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
