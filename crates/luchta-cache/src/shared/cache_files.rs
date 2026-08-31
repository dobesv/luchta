//! Immutable advisory cache-file state blobs.
//!
//! These archives are intentionally separate from normal output blobs. They
//! warm a task that is already going to execute and never contribute to cache
//! hit decisions or output hashes.

use std::io;
use std::path::{Path, PathBuf};

use super::blob::{restore_file_blob_staged_with_size_cap, write_file_blob, StagedRestore};
use super::snapshot::{input_key_hex, select_cache_file_observation, CacheFileObservation};
use super::{
    classify_outputs, hex_hash, min_store_duration_ms, BlobReadResultWithMeta, BlobWriteResult,
    OutputScope, ScopeError, SharedCache, SharedCachePaths, StoreOutcome,
};
use crate::record::TaskRunRecord;

#[derive(Debug)]
pub struct PreparedCacheFiles {
    staged: StagedRestore,
}

impl PreparedCacheFiles {
    pub fn commit(self) -> io::Result<Vec<PathBuf>> {
        self.staged.commit()
    }

    pub fn discard(self) -> io::Result<()> {
        self.staged.discard()
    }

    #[must_use]
    pub fn relative_paths(&self) -> Vec<PathBuf> {
        self.staged.relative_file_paths().unwrap_or_default()
    }

    /// Whether a package-relative path points into this restore's private
    /// staging directory rather than pre-existing local cache state.
    #[must_use]
    pub fn is_staged_path(&self, path: &Path) -> bool {
        self.staged.contains_package_relative_path(path)
    }
}

#[derive(Debug)]
pub enum CacheFileReadResult {
    Staged(PreparedCacheFiles),
    Missing,
    Corrupt,
}

/// Files captured into one immutable advisory-state blob.
#[derive(Debug, Clone, Copy)]
pub struct CacheFileBlobSource<'a> {
    pub package_dir: &'a Path,
    pub relative_paths: &'a [PathBuf],
    pub size_cap_bytes: u64,
}

/// Destination and validation policy for one advisory-state restore.
#[derive(Debug, Clone, Copy)]
pub struct CacheFileRestoreTarget<'a> {
    pub package_dir: &'a Path,
    pub patterns: &'a [String],
    pub size_cap_bytes: u64,
}

/// Selection and staging inputs for one advisory cache-file restore.
#[derive(Debug, Clone, Copy)]
pub struct CacheFileRestoreRequest<'a> {
    pub scope_hash: &'a [u8; 32],
    pub upstream_outputs_hash: [u8; 32],
    pub package_dir: &'a Path,
    pub patterns: &'a [String],
}

/// Capture inputs for one advisory cache-file observation.
#[derive(Debug, Clone, Copy)]
pub struct CacheFileStoreRequest<'a> {
    pub scope_hash: [u8; 32],
    pub upstream_outputs_hash: [u8; 32],
    pub package_dir: &'a Path,
    pub relative_paths: &'a [PathBuf],
    pub state_hash: Option<[u8; 32]>,
    pub state_bytes: u64,
    pub record: &'a TaskRunRecord,
    pub repo_root: &'a Path,
}

#[must_use]
pub fn cache_file_blob_path(paths: &SharedCachePaths, state_hash: &[u8; 32]) -> PathBuf {
    paths
        .cache_files_dir
        .join(format!("{}.tar.zst", hex_hash(*state_hash)))
}

pub fn write_cache_file_blob(
    paths: &SharedCachePaths,
    state_hash: &[u8; 32],
    source: CacheFileBlobSource<'_>,
) -> io::Result<BlobWriteResult> {
    write_file_blob(
        &cache_file_blob_path(paths, state_hash),
        source.package_dir,
        source.relative_paths,
        source.size_cap_bytes,
    )
}

/// Stage one cache-file blob and validate both its content address and every
/// archive path against the task's current declaration before exposing it to
/// the package tree.
pub fn stage_cache_file_blob(
    paths: &SharedCachePaths,
    state_hash: &[u8; 32],
    target: CacheFileRestoreTarget<'_>,
) -> io::Result<CacheFileReadResult> {
    let staged = match restore_file_blob_staged_with_size_cap(
        &cache_file_blob_path(paths, state_hash),
        target.package_dir,
        Some(target.size_cap_bytes),
    )? {
        BlobReadResultWithMeta::Restored(staged) => staged,
        BlobReadResultWithMeta::Missing => return Ok(CacheFileReadResult::Missing),
        BlobReadResultWithMeta::Corrupt => return Ok(CacheFileReadResult::Corrupt),
    };

    let relative_paths = match staged.relative_file_paths() {
        Ok(paths) => paths,
        Err(_) => {
            let _ = staged.discard();
            return Ok(CacheFileReadResult::Corrupt);
        }
    };
    if !staged_cache_file_blob_is_valid(&staged, &relative_paths, state_hash, target.patterns)? {
        let _ = staged.discard();
        return Ok(CacheFileReadResult::Corrupt);
    }

    Ok(CacheFileReadResult::Staged(PreparedCacheFiles { staged }))
}

fn staged_cache_file_blob_is_valid(
    staged: &StagedRestore,
    relative_paths: &[PathBuf],
    state_hash: &[u8; 32],
    patterns: &[String],
) -> io::Result<bool> {
    if relative_paths.is_empty() {
        return Ok(false);
    }
    if relative_paths
        .iter()
        .any(|path| !path_matches_patterns(path, patterns))
    {
        return Ok(false);
    }
    Ok(staged.cache_files_hash()? == *state_hash)
}

fn path_matches_patterns(path: &Path, patterns: &[String]) -> bool {
    let included = patterns
        .iter()
        .filter(|pattern| !luchta_glob::is_negated(pattern))
        .any(|pattern| pattern_matches_path(pattern, path));
    included
        && !patterns
            .iter()
            .filter(|pattern| luchta_glob::is_negated(pattern))
            .any(|pattern| {
                let (_, body) = luchta_glob::split_negation(pattern);
                pattern_matches_path(body, path)
            })
}

fn pattern_matches_path(pattern: &str, path: &Path) -> bool {
    if luchta_types::classify_pattern(pattern) == luchta_types::InputSemantics::Literal {
        return path == Path::new(&luchta_glob::unescape_literal(pattern));
    }
    luchta_glob::build_path_glob(pattern)
        .map(|glob| glob.compile_matcher().is_match(path))
        .unwrap_or(false)
}

impl SharedCache {
    #[cfg(unix)]
    fn enqueue_cache_file_blob(&self, state_hash: [u8; 32]) {
        let Some(remote) = &self.remote else {
            return;
        };
        if remote.is_disabled() {
            return;
        }
        remote.enqueue_cache_file_blob(super::remote::OwnedCacheFileBlob {
            paths: std::sync::Arc::clone(&self.paths),
            state_hash,
        });
    }

    /// Select and stage at most one advisory cache-file candidate. Missing,
    /// corrupt, or unavailable selected blobs run cold; no fallback candidate
    /// is fetched, keeping remote work bounded.
    pub fn prepare_cache_files(
        &self,
        request: CacheFileRestoreRequest<'_>,
    ) -> Option<PreparedCacheFiles> {
        #[cfg(unix)]
        self.pull_remote_snapshots_for_restore();
        let candidates = self
            .get_or_build_index()
            .cache_files
            .get(&input_key_hex(*request.scope_hash))?;
        let observation =
            select_cache_file_observation(candidates, request.upstream_outputs_hash)?.clone();
        if observation.state_bytes > self.size_cap_bytes {
            return None;
        }
        let state_hash = observation.state_blob_hash?;

        if !cache_file_blob_path(&self.paths, &state_hash).is_file() {
            #[cfg(unix)]
            if let Some(remote) = self.remote.as_ref() {
                if let Err(error) = remote.pull_cache_file_blob(&self.paths, &state_hash) {
                    eprintln!(
                        "warn: shared cache-file blob restore failed for state_hash={}: {error}",
                        hex_hash(state_hash)
                    );
                }
            }
        }

        match stage_cache_file_blob(
            &self.paths,
            &state_hash,
            CacheFileRestoreTarget {
                package_dir: request.package_dir,
                patterns: request.patterns,
                size_cap_bytes: self.size_cap_bytes,
            },
        ) {
            Ok(CacheFileReadResult::Staged(staged)) => Some(staged),
            Ok(CacheFileReadResult::Missing | CacheFileReadResult::Corrupt) => None,
            Err(error) => {
                eprintln!(
                    "debug: shared cache-file staging failed for state_hash={}: {error}",
                    hex_hash(state_hash)
                );
                None
            }
        }
    }

    /// Capture advisory cache files independently of the normal output cache.
    /// Eligibility uses the same success, duration, and size thresholds, but a
    /// rejection here never changes whether normal outputs can be stored.
    pub fn store_cache_files_with_execution_duration(
        &self,
        request: CacheFileStoreRequest<'_>,
        execution_duration_ms: u64,
    ) -> io::Result<StoreOutcome> {
        if self.write_bucket_key.is_none() {
            return Ok(StoreOutcome::Disabled);
        }
        if !request.record.succeeded {
            return Ok(StoreOutcome::SkippedNotSucceeded);
        }
        if execution_duration_ms < min_store_duration_ms() {
            return Ok(StoreOutcome::SkippedTooFast {
                duration_ms: execution_duration_ms,
            });
        }
        match classify_outputs(
            request.repo_root,
            request.package_dir,
            request.relative_paths,
        ) {
            Ok(OutputScope::InPackage) => {}
            Ok(OutputScope::CrossPackage) => return Ok(StoreOutcome::SkippedCrossPackage),
            Err(ScopeError::PathEscape { .. }) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cache-file path escapes repository root",
                ))
            }
        }
        if request.state_bytes > self.size_cap_bytes {
            return Ok(StoreOutcome::SkippedTooLarge {
                bytes: request.state_bytes,
            });
        }

        if let Some(state_hash) = request.state_hash {
            match write_cache_file_blob(
                &self.paths,
                &state_hash,
                CacheFileBlobSource {
                    package_dir: request.package_dir,
                    relative_paths: request.relative_paths,
                    size_cap_bytes: self.size_cap_bytes,
                },
            )? {
                BlobWriteResult::Written | BlobWriteResult::AlreadyExists => {
                    #[cfg(unix)]
                    self.enqueue_cache_file_blob(state_hash);
                }
                BlobWriteResult::SkippedTooLarge { bytes } => {
                    return Ok(StoreOutcome::SkippedTooLarge { bytes })
                }
                BlobWriteResult::NoOutputs => return Ok(StoreOutcome::Disabled),
            }
        }

        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record_cache_file(CacheFileObservation {
                scope_hash: request.scope_hash,
                upstream_outputs_hash: request.upstream_outputs_hash,
                state_blob_hash: request.state_hash,
                state_bytes: request.state_bytes,
                cached_at_unix_ms: request.record.end_unix_ms,
            });
        Ok(StoreOutcome::Stored)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::shared::open_shared_paths;
    use crate::{combined_cache_files_hash, resolve_outputs};

    #[test]
    fn multi_file_archive_honors_exclusions_and_commits_together() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir_all(source.join("cache/tmp")).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("cache/a.bin"), b"a").unwrap();
        fs::write(source.join("cache/b.bin"), b"b").unwrap();
        fs::write(source.join("cache/tmp/ignored.bin"), b"ignored").unwrap();
        let patterns = vec!["cache/**".to_owned(), "!cache/tmp/**".to_owned()];
        let entries = resolve_outputs(&source, &patterns).unwrap();
        let state_hash = combined_cache_files_hash(&entries);
        let paths = open_shared_paths(&temp.path().join("shared")).unwrap();
        let relative_paths = entries
            .iter()
            .map(|entry| PathBuf::from(&entry.path))
            .collect::<Vec<_>>();

        assert_eq!(
            write_cache_file_blob(
                &paths,
                &state_hash,
                CacheFileBlobSource {
                    package_dir: &source,
                    relative_paths: &relative_paths,
                    size_cap_bytes: 1_000,
                },
            )
            .unwrap(),
            BlobWriteResult::Written
        );
        let CacheFileReadResult::Staged(staged) = stage_cache_file_blob(
            &paths,
            &state_hash,
            CacheFileRestoreTarget {
                package_dir: &destination,
                patterns: &patterns,
                size_cap_bytes: 1_000,
            },
        )
        .unwrap() else {
            panic!("cache files should stage");
        };
        assert_eq!(
            staged.relative_paths(),
            [PathBuf::from("cache/a.bin"), PathBuf::from("cache/b.bin")]
        );
        assert!(!destination.join("cache").exists());
        staged.commit().unwrap();
        assert_eq!(fs::read(destination.join("cache/a.bin")).unwrap(), b"a");
        assert_eq!(fs::read(destination.join("cache/b.bin")).unwrap(), b"b");
        assert!(!destination.join("cache/tmp/ignored.bin").exists());
    }

    #[test]
    fn undeclared_or_content_mismatched_archive_is_rejected_before_commit() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("other.bin"), b"bad").unwrap();
        let entries = resolve_outputs(&source, &["other.bin".to_owned()]).unwrap();
        let state_hash = combined_cache_files_hash(&entries);
        let paths = open_shared_paths(&temp.path().join("shared")).unwrap();
        write_cache_file_blob(
            &paths,
            &state_hash,
            CacheFileBlobSource {
                package_dir: &source,
                relative_paths: &[PathBuf::from("other.bin")],
                size_cap_bytes: 1_000,
            },
        )
        .unwrap();

        assert!(matches!(
            stage_cache_file_blob(
                &paths,
                &state_hash,
                CacheFileRestoreTarget {
                    package_dir: &destination,
                    patterns: &["expected.bin".to_owned()],
                    size_cap_bytes: 1_000,
                },
            )
            .unwrap(),
            CacheFileReadResult::Corrupt
        ));
        assert!(!destination.join("other.bin").exists());

        assert!(matches!(
            stage_cache_file_blob(
                &paths,
                &[9; 32],
                CacheFileRestoreTarget {
                    package_dir: &destination,
                    patterns: &["other.bin".to_owned()],
                    size_cap_bytes: 1_000,
                },
            )
            .unwrap(),
            CacheFileReadResult::Missing
        ));
    }

    #[test]
    fn corrupt_and_oversized_cache_file_blobs_fail_open() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("cache.bin"), b"12345").unwrap();
        let paths = open_shared_paths(&temp.path().join("shared")).unwrap();

        assert_eq!(
            write_cache_file_blob(
                &paths,
                &[1; 32],
                CacheFileBlobSource {
                    package_dir: &source,
                    relative_paths: &[PathBuf::from("cache.bin")],
                    size_cap_bytes: 4,
                },
            )
            .unwrap(),
            BlobWriteResult::SkippedTooLarge { bytes: 5 }
        );

        let entries = resolve_outputs(&source, &["cache.bin".to_owned()]).unwrap();
        let state_hash = combined_cache_files_hash(&entries);
        assert_eq!(
            write_cache_file_blob(
                &paths,
                &state_hash,
                CacheFileBlobSource {
                    package_dir: &source,
                    relative_paths: &[PathBuf::from("cache.bin")],
                    size_cap_bytes: 10,
                },
            )
            .unwrap(),
            BlobWriteResult::Written
        );
        assert!(matches!(
            stage_cache_file_blob(
                &paths,
                &state_hash,
                CacheFileRestoreTarget {
                    package_dir: &destination,
                    patterns: &["cache.bin".to_owned()],
                    size_cap_bytes: 4,
                },
            )
            .unwrap(),
            CacheFileReadResult::Corrupt
        ));

        fs::write(cache_file_blob_path(&paths, &[2; 32]), b"not an archive").unwrap();
        assert!(matches!(
            stage_cache_file_blob(
                &paths,
                &[2; 32],
                CacheFileRestoreTarget {
                    package_dir: &destination,
                    patterns: &["cache.bin".to_owned()],
                    size_cap_bytes: 10,
                },
            )
            .unwrap(),
            CacheFileReadResult::Corrupt
        ));
        assert!(fs::read_dir(&destination).unwrap().next().is_none());
    }
}
