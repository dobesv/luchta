use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::record::TaskRunRecord;
use crate::shared::atomic_write;
use crate::{CacheError, Result};

const META_FILE_NAME: &str = "meta.bincode";
const STDOUT_FILE_NAME: &str = "stdout.log";
const STDERR_FILE_NAME: &str = "stderr.log";
const TMP_SUFFIX: &str = ".tmp";
pub const CACHE_DIR_ENV: &str = "LUCHTA_CACHE_DIR";
pub const LUCHTA_DIR_NAME: &str = ".luchta";
pub const CACHE_DIR_NAME: &str = "cache";
pub const GITIGNORE_FILE_NAME: &str = ".gitignore";
pub const GITIGNORE_CONTENTS: &str = "*\n";
const TMP_FILE_MAX_AGE: Duration = Duration::from_secs(60 * 60);

fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard().with_fixed_int_encoding()
}

#[derive(Debug, Clone)]
pub struct Cache {
    cache_dir: PathBuf,
}

pub struct RunArtifacts<'a> {
    pub record: &'a TaskRunRecord,
    pub stdout: &'a [u8],
    pub stderr: &'a [u8],
}

impl Cache {
    pub fn open(cache_dir: &Path) -> Result<Self> {
        fs::create_dir_all(cache_dir)?;
        clean_tmp_files(cache_dir)?;
        ensure_luchta_gitignore(cache_dir)?;

        Ok(Self {
            cache_dir: cache_dir.to_path_buf(),
        })
    }

    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn read(&self, task_id: &str) -> Option<TaskRunRecord> {
        let path = self.record_path(task_id);
        let bytes = fs::read(path).ok()?;
        bincode::serde::decode_from_slice(&bytes, bincode_config())
            .map(|(record, _consumed)| record)
            .ok()
    }

    pub fn write(&self, task_id: &str, artifacts: RunArtifacts<'_>) -> Result<()> {
        let task_dir = self.task_dir(task_id);
        fs::create_dir_all(&task_dir)?;

        let metadata = bincode::serde::encode_to_vec(artifacts.record, bincode_config())
            .map_err(CacheError::SerializeRecord)?;

        atomic_write(&self.stdout_path(task_id), artifacts.stdout)?;
        atomic_write(&self.stderr_path(task_id), artifacts.stderr)?;
        atomic_write(&self.record_path(task_id), &metadata)?;

        Ok(())
    }

    #[must_use]
    pub fn stdout_path(&self, task_id: &str) -> PathBuf {
        self.task_dir(task_id).join(STDOUT_FILE_NAME)
    }

    #[must_use]
    pub fn stderr_path(&self, task_id: &str) -> PathBuf {
        self.task_dir(task_id).join(STDERR_FILE_NAME)
    }

    fn record_path(&self, task_id: &str) -> PathBuf {
        self.task_dir(task_id).join(META_FILE_NAME)
    }

    fn task_dir(&self, task_id: &str) -> PathBuf {
        self.cache_dir.join(task_cache_key(task_id))
    }
}

#[must_use]
pub fn resolve_cache_dir(workspace_root: &Path) -> PathBuf {
    match env::var_os(CACHE_DIR_ENV) {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => workspace_root.join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME),
    }
}

#[must_use]
pub fn task_cache_key(task_id: &str) -> String {
    blake3::hash(task_id.as_bytes()).to_hex().to_string()
}

fn ensure_luchta_gitignore(cache_dir: &Path) -> Result<()> {
    if !is_default_cache_layout(cache_dir) {
        return Ok(());
    }

    let Some(luchta_dir) = cache_dir.parent() else {
        return Ok(());
    };

    fs::create_dir_all(luchta_dir)?;
    let gitignore_path = luchta_dir.join(GITIGNORE_FILE_NAME);

    match fs::read_to_string(&gitignore_path) {
        Ok(existing) if existing == GITIGNORE_CONTENTS => Ok(()),
        Ok(_) => {
            fs::write(gitignore_path, GITIGNORE_CONTENTS)?;
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::write(gitignore_path, GITIGNORE_CONTENTS)?;
            Ok(())
        }
        Err(err) => Err(CacheError::Io(err)),
    }
}

fn is_default_cache_layout(cache_dir: &Path) -> bool {
    cache_dir
        .file_name()
        .is_some_and(|name| name == CACHE_DIR_NAME)
        && cache_dir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == LUCHTA_DIR_NAME)
}

fn clean_tmp_files(cache_dir: &Path) -> Result<()> {
    clean_tmp_files_older_than(cache_dir, TMP_FILE_MAX_AGE, SystemTime::now())
}

fn clean_tmp_files_older_than(cache_dir: &Path, max_age: Duration, now: SystemTime) -> Result<()> {
    if !cache_dir.exists() {
        return Ok(());
    }

    let entries = match fs::read_dir(cache_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };

    for entry in entries.filter_map(|entry| entry.ok()) {
        let is_dir = entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        clean_tmp_files_in_task_dir(&entry.path(), max_age, now);
    }

    Ok(())
}

fn clean_tmp_files_in_task_dir(task_dir: &Path, max_age: Duration, now: SystemTime) {
    let Ok(children) = fs::read_dir(task_dir) else {
        return;
    };
    for child in children.filter_map(|child| child.ok()) {
        let _ = remove_stale_tmp_file_if_present(&child, max_age, now);
    }
}

fn remove_stale_tmp_file_if_present(
    entry: &fs::DirEntry,
    max_age: Duration,
    now: SystemTime,
) -> Result<()> {
    if !is_tmp_file_entry(entry)? || !is_stale_tmp_file(entry, max_age, now)? {
        return Ok(());
    }

    match fs::remove_file(entry.path()) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(CacheError::Io(err)),
    }
}

fn is_stale_tmp_file(entry: &fs::DirEntry, max_age: Duration, now: SystemTime) -> Result<bool> {
    let modified = entry.metadata()?.modified()?;
    match now.duration_since(modified) {
        Ok(age) => Ok(age > max_age),
        Err(_) => Ok(false),
    }
}

fn is_tmp_file_entry(entry: &fs::DirEntry) -> Result<bool> {
    Ok(entry.file_type()?.is_file() && is_tmp_file(&entry.path()))
}

fn is_tmp_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(TMP_SUFFIX))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::fs::{File, FileTimes};
    use std::time::{Duration, SystemTime};

    use tempfile::tempdir;

    use super::*;
    use crate::record::{FileEntry, SCHEMA_VERSION_V1};

    #[test]
    fn write_then_read_round_trip_identical() {
        let temp_dir = tempdir().unwrap();
        let workspace_root = temp_dir.path();
        let cache =
            Cache::open(&workspace_root.join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();
        let record = sample_record();

        cache
            .write(
                "pkg#build",
                RunArtifacts {
                    record: &record,
                    stdout: b"stdout bytes",
                    stderr: b"stderr bytes",
                },
            )
            .unwrap();

        assert_eq!(cache.read("pkg#build"), Some(record));
        assert_eq!(
            fs::read(cache.stdout_path("pkg#build")).unwrap(),
            b"stdout bytes"
        );
        assert_eq!(
            fs::read(cache.stderr_path("pkg#build")).unwrap(),
            b"stderr bytes"
        );
    }

    #[test]
    fn write_persists_stdout_and_stderr_before_meta_marker() {
        let temp_dir = tempdir().unwrap();
        let cache =
            Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();

        cache
            .write(
                "pkg#build",
                RunArtifacts {
                    record: &sample_record(),
                    stdout: b"stdout bytes",
                    stderr: b"stderr bytes",
                },
            )
            .unwrap();

        let task_dir = cache.task_dir("pkg#build");
        assert!(task_dir.join(META_FILE_NAME).is_file());
        assert!(cache.stdout_path("pkg#build").is_file());
        assert!(cache.stderr_path("pkg#build").is_file());
    }

    #[test]
    fn corrupt_meta_returns_none() {
        let temp_dir = tempdir().unwrap();
        let cache =
            Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();
        let task_dir = cache.task_dir("pkg#build");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join(META_FILE_NAME), b"not-bincode").unwrap();

        assert_eq!(cache.read("pkg#build"), None);
    }

    #[test]
    fn missing_meta_returns_none() {
        let temp_dir = tempdir().unwrap();
        let cache =
            Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();

        assert_eq!(cache.read("pkg#missing"), None);
    }

    #[test]
    fn open_keeps_fresh_tmp_files() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME);
        let cache = Cache::open(&cache_dir).unwrap();
        let record = sample_record();

        cache
            .write(
                "pkg#build",
                RunArtifacts {
                    record: &record,
                    stdout: b"out",
                    stderr: b"err",
                },
            )
            .unwrap();

        let task_dir = cache.task_dir("pkg#build");
        fs::write(task_dir.join("meta.bincode.tmp"), b"fresh").unwrap();
        fs::write(task_dir.join("stdout.log.tmp"), b"fresh").unwrap();
        fs::write(task_dir.join("stderr.log.tmp"), b"fresh").unwrap();

        let reopened = Cache::open(&cache_dir).unwrap();
        let mut files = fs::read_dir(reopened.task_dir("pkg#build"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        files.sort();

        assert_eq!(
            files,
            vec![
                META_FILE_NAME.to_string(),
                "meta.bincode.tmp".to_string(),
                STDERR_FILE_NAME.to_string(),
                "stderr.log.tmp".to_string(),
                STDOUT_FILE_NAME.to_string(),
                "stdout.log.tmp".to_string(),
            ]
        );
    }

    #[test]
    fn clean_tmp_files_older_than_removes_only_stale_tmp_files() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME);
        let cache = Cache::open(&cache_dir).unwrap();
        let task_dir = cache.task_dir("pkg#build");
        fs::create_dir_all(&task_dir).unwrap();

        let max_age = Duration::from_secs(60);
        let stale = task_dir.join("meta.bincode.tmp");
        let fresh = task_dir.join("stdout.log.tmp");
        let regular = task_dir.join(STDERR_FILE_NAME);
        fs::write(&stale, b"stale").unwrap();
        fs::write(&fresh, b"fresh").unwrap();
        fs::write(&regular, b"keep").unwrap();

        let past = SystemTime::now() - (max_age * 2);
        File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(FileTimes::new().set_modified(past))
            .unwrap();

        clean_tmp_files_older_than(&cache_dir, max_age, SystemTime::now()).unwrap();

        assert!(!stale.exists());
        assert!(fresh.exists());
        assert!(regular.exists());
    }

    #[test]
    fn open_skips_parent_gitignore_for_custom_cache_dir() {
        let temp_dir = tempdir().unwrap();
        let custom_cache_dir = temp_dir.path().join("custom-cache");

        Cache::open(&custom_cache_dir).unwrap();

        assert!(!temp_dir.path().join(GITIGNORE_FILE_NAME).exists());
    }

    #[test]
    fn resolve_cache_dir_uses_env_override() {
        let temp_dir = tempdir().unwrap();
        let override_dir = temp_dir.path().join("custom-cache");
        unsafe {
            env::set_var(CACHE_DIR_ENV, &override_dir);
        }

        let resolved = resolve_cache_dir(temp_dir.path());

        unsafe {
            env::remove_var(CACHE_DIR_ENV);
        }
        assert_eq!(resolved, override_dir);
    }

    #[test]
    fn open_creates_gitignore_idempotently() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME);

        Cache::open(&cache_dir).unwrap();
        Cache::open(&cache_dir).unwrap();

        let gitignore_path = temp_dir
            .path()
            .join(LUCHTA_DIR_NAME)
            .join(GITIGNORE_FILE_NAME);
        assert_eq!(
            fs::read_to_string(gitignore_path).unwrap(),
            GITIGNORE_CONTENTS
        );
    }

    #[test]
    fn clean_tmp_files_ignores_missing_task_dir() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME);
        let task_dir = cache_dir.join("task");
        fs::create_dir_all(&task_dir).unwrap();
        fs::remove_dir_all(&task_dir).unwrap();

        clean_tmp_files_older_than(&cache_dir, TMP_FILE_MAX_AGE, SystemTime::now()).unwrap();
    }

    #[test]
    fn file_entry_absent_and_present_are_distinct() {
        let absent = FileEntry::absent("dist/app.js");
        let present = FileEntry {
            path: "dist/app.js".to_string(),
            size: 0,
            mtime_ns: 0,
            hash: [0; 32],
            absent: false,
        };

        assert_ne!(absent, present);
        assert!(absent.absent);
        assert!(!present.absent);
    }

    fn sample_record() -> TaskRunRecord {
        TaskRunRecord {
            schema_version: SCHEMA_VERSION_V1,
            task_spec_hash: [1; 32],
            input_patterns: vec!["src/**/*.ts".to_string()],
            inputs: vec![FileEntry {
                path: "src/main.ts".to_string(),
                size: 42,
                mtime_ns: 111,
                hash: [2; 32],
                absent: false,
            }],
            output_patterns: vec!["dist/**/*.js".to_string()],
            outputs: vec![FileEntry {
                path: "dist/main.js".to_string(),
                size: 84,
                mtime_ns: 222,
                hash: [3; 32],
                absent: false,
            }],
            detected_input_patterns: true,
            detected_output_patterns: true,
            outputs_hash: [4; 32],
            env_hash: [5; 32],
            pkg_dep_hash: [6; 32],
            dep_outputs: BTreeMap::from([("dep#build".to_string(), [7; 32])]),
            exit_status: 0,
            succeeded: true,
            start_unix_ms: 1_000,
            end_unix_ms: 2_000,
        }
    }
}
