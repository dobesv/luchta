use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::record::{TaskRunRecord, SCHEMA_VERSION_V3};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportInput {
    pub filename: String,
    pub mime_type: String,
    pub content: String,
}

pub struct RunArtifacts<'a> {
    pub record: &'a TaskRunRecord,
    pub stdout: &'a [u8],
    pub stderr: &'a [u8],
    pub reports: &'a [ReportInput],
}

#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    pub fn open(cache_dir: &Path) -> Result<Self> {
        fs::create_dir_all(cache_dir)?;
        clean_tmp_files_older_than(cache_dir, TMP_FILE_MAX_AGE, SystemTime::now())?;
        ensure_luchta_gitignore(cache_dir)?;
        Ok(Self {
            root: cache_dir.to_path_buf(),
        })
    }

    #[must_use]
    pub fn read(&self, task_id: &str) -> Option<TaskRunRecord> {
        let bytes = fs::read(self.task_dir(task_id).join(META_FILE_NAME)).ok()?;
        let (record, _): (TaskRunRecord, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode_config()).ok()?;
        // Only accept V3 records. Older versions -> clean cache miss.
        (record.schema_version == SCHEMA_VERSION_V3).then_some(record)
    }

    pub fn write(&self, task_id: &str, artifacts: RunArtifacts<'_>) -> Result<()> {
        let task_dir = self.task_dir(task_id);
        fs::create_dir_all(&task_dir)?;

        for report in artifacts.reports {
            if !is_valid_report_filename(&report.filename) {
                return Err(CacheError::InputExpansion(format!(
                    "invalid cached report filename: {}",
                    report.filename
                )));
            }

            atomic_write(&task_dir.join(&report.filename), report.content.as_bytes())?;
        }

        let encoded = bincode::serde::encode_to_vec(artifacts.record, bincode_config())
            .map_err(CacheError::SerializeRecord)?;
        atomic_write(&task_dir.join(STDOUT_FILE_NAME), artifacts.stdout)?;
        atomic_write(&task_dir.join(STDERR_FILE_NAME), artifacts.stderr)?;
        atomic_write(&task_dir.join(META_FILE_NAME), &encoded)?;
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

    #[must_use]
    pub fn report_path(&self, task_id: &str, filename: &str) -> PathBuf {
        self.task_dir(task_id).join(filename)
    }

    #[must_use]
    pub fn read_report(&self, task_id: &str, filename: &str) -> Option<Vec<u8>> {
        if !is_valid_report_filename(filename) {
            return None;
        }
        fs::read(self.report_path(task_id, filename)).ok()
    }

    #[must_use]
    pub fn task_dir(&self, task_id: &str) -> PathBuf {
        self.root.join(task_cache_key(task_id))
    }
}

pub(crate) fn is_valid_report_filename(filename: &str) -> bool {
    !filename.is_empty()
        && !Path::new(filename).is_absolute()
        && !filename.contains('/')
        && !filename.contains('\\')
        && !filename.contains("..")
        && filename != META_FILE_NAME
        && filename != STDOUT_FILE_NAME
        && filename != STDERR_FILE_NAME
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

fn clean_tmp_files_older_than(
    cache_dir: &Path,
    max_age: Duration,
    now: SystemTime,
) -> io::Result<()> {
    let read_dir = match fs::read_dir(cache_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    for task_entry in read_dir {
        let task_entry = task_entry?;
        if !task_entry.file_type()?.is_dir() {
            continue;
        }

        let task_dir = task_entry.path();
        let files = match fs::read_dir(&task_dir) {
            Ok(files) => files,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };

        for file in files {
            let file = file?;
            if !file.file_type()?.is_file() {
                continue;
            }
            let name = file.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(TMP_SUFFIX) {
                continue;
            }

            let modified = file.metadata()?.modified()?;
            let age = match now.duration_since(modified) {
                Ok(age) => age,
                Err(_) => Duration::ZERO,
            };
            if age > max_age {
                let _ = fs::remove_file(file.path());
            }
        }
    }

    Ok(())
}

#[must_use]
pub fn resolve_cache_dir(workspace_root: &Path) -> PathBuf {
    if let Ok(path) = env::var(CACHE_DIR_ENV) {
        return PathBuf::from(path);
    }
    workspace_root.join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)
}

#[must_use]
pub fn task_cache_key(task_id: &str) -> String {
    use blake3::Hasher;
    let mut hasher = Hasher::new();
    hasher.update(task_id.as_bytes());
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::{self, File};

    use std::fs::FileTimes;
    use tempfile::tempdir;

    use super::*;
    use crate::record::{
        FileEntry, ReportMeta, SCHEMA_VERSION_V1, SCHEMA_VERSION_V2, SCHEMA_VERSION_V3,
    };

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
                    reports: &[],
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
                    stdout: b"out",
                    stderr: b"err",
                    reports: &[],
                },
            )
            .unwrap();

        let task_dir = cache.task_dir("pkg#build");
        assert!(task_dir.join(META_FILE_NAME).is_file());
        assert!(cache.stdout_path("pkg#build").is_file());
        assert!(cache.stderr_path("pkg#build").is_file());
    }

    #[test]
    fn write_then_read_round_trip_preserves_reports_and_contents() {
        let temp_dir = tempdir().unwrap();
        let cache =
            Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();
        let record = sample_record_with_reports();
        let reports = sample_reports();

        cache
            .write(
                "pkg#build",
                RunArtifacts {
                    record: &record,
                    stdout: b"stdout bytes",
                    stderr: b"stderr bytes",
                    reports: &reports,
                },
            )
            .unwrap();

        assert_eq!(cache.read("pkg#build"), Some(record.clone()));
        assert_eq!(
            fs::read_to_string(cache.task_dir("pkg#build").join("summary.md")).unwrap(),
            "# summary\nsecond line\n"
        );
        assert_eq!(
            fs::read_to_string(cache.task_dir("pkg#build").join("report.json")).unwrap(),
            "{\"ok\":true}\n"
        );
        assert_ne!(
            fs::metadata(cache.task_dir("pkg#build").join("summary.md"))
                .unwrap()
                .len(),
            0
        );
        assert_ne!(
            fs::metadata(cache.task_dir("pkg#build").join("report.json"))
                .unwrap()
                .len(),
            0
        );
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
    fn v1_meta_returns_none_instead_of_panicking() {
        assert_previous_schema_record_is_cache_miss("pkg#build", sample_v1_record());
    }

    #[test]
    fn v2_meta_returns_none_instead_of_panicking() {
        assert_previous_schema_record_is_cache_miss("pkg#build", sample_v2_record());
    }

    #[test]
    fn read_report_returns_bytes_for_cached_report() {
        let temp_dir = tempdir().unwrap();
        let cache =
            Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();
        let record = sample_record_with_reports();
        let reports = sample_reports();

        cache
            .write(
                "pkg#build",
                RunArtifacts {
                    record: &record,
                    stdout: b"out",
                    stderr: b"err",
                    reports: &reports,
                },
            )
            .unwrap();

        assert_eq!(
            cache.read_report("pkg#build", "summary.md").unwrap(),
            b"# summary\nsecond line\n"
        );
        assert_eq!(cache.read_report("pkg#build", "missing.md"), None);
    }

    #[test]
    fn read_report_rejects_unsafe_filename() {
        let temp_dir = tempdir().unwrap();
        let cache =
            Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();

        assert_eq!(cache.read_report("pkg#build", "../escape.txt"), None);
        assert_eq!(
            cache.report_path("pkg#build", "report.json"),
            cache.task_dir("pkg#build").join("report.json")
        );
    }

    #[test]
    fn missing_meta_returns_none() {
        let temp_dir = tempdir().unwrap();
        let cache =
            Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();

        assert_eq!(cache.read("pkg#missing"), None);
    }

    #[test]
    fn invalid_report_filename_rejected() {
        let temp_dir = tempdir().unwrap();
        let cache =
            Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();
        let record = sample_record_with_reports();
        let reports = vec![ReportInput {
            filename: "../escape.txt".to_string(),
            mime_type: "text/plain".to_string(),
            content: "x".to_string(),
        }];

        let error = cache
            .write(
                "pkg#build",
                RunArtifacts {
                    record: &record,
                    stdout: b"out",
                    stderr: b"err",
                    reports: &reports,
                },
            )
            .unwrap_err();

        assert!(matches!(error, CacheError::InputExpansion(_)));
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
                    reports: &[],
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
    fn open_removes_stale_tmp_files() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME);
        let task_dir = cache_dir.join("task");
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
    fn open_writes_gitignore_once() {
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

    #[test]
    fn read_returns_none_for_previous_schema_record() {
        assert_previous_schema_record_is_cache_miss("pkg#build", sample_v1_record());
    }

    #[test]
    fn read_returns_none_for_truncated_or_garbage_meta() {
        let temp_dir = tempdir().unwrap();
        let cache =
            Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();
        let task_dir = cache.root.join(task_cache_key("pkg#build"));
        fs::create_dir_all(&task_dir).unwrap();

        fs::write(task_dir.join(META_FILE_NAME), b"not bincode at all").unwrap();
        assert_eq!(cache.read("pkg#build"), None);

        let bytes = bincode::serde::encode_to_vec(sample_record(), bincode_config()).unwrap();
        fs::write(task_dir.join(META_FILE_NAME), &bytes[..bytes.len() / 2]).unwrap();
        assert_eq!(cache.read("pkg#build"), None);
    }

    fn assert_previous_schema_record_is_cache_miss<T>(task_id: &str, previous_record: T)
    where
        T: serde::Serialize,
    {
        let temp_dir = tempdir().unwrap();
        let cache =
            Cache::open(&temp_dir.path().join(LUCHTA_DIR_NAME).join(CACHE_DIR_NAME)).unwrap();
        let task_dir = cache.root.join(task_cache_key(task_id));
        fs::create_dir_all(&task_dir).unwrap();

        let bytes = bincode::serde::encode_to_vec(&previous_record, bincode_config()).unwrap();
        fs::write(task_dir.join(META_FILE_NAME), bytes).unwrap();

        assert_eq!(cache.read(task_id), None);
    }

    fn sample_record() -> TaskRunRecord {
        TaskRunRecord {
            schema_version: SCHEMA_VERSION_V3,
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
            reports: vec![],
            cache_nonce: None,
        }
    }

    fn sample_reports() -> Vec<ReportInput> {
        vec![
            ReportInput {
                filename: "summary.md".to_string(),
                mime_type: "text/markdown".to_string(),
                content: "# summary\nsecond line\n".to_string(),
            },
            ReportInput {
                filename: "report.json".to_string(),
                mime_type: "application/json".to_string(),
                content: "{\"ok\":true}\n".to_string(),
            },
        ]
    }

    fn sample_record_with_reports() -> TaskRunRecord {
        TaskRunRecord {
            reports: vec![
                ReportMeta {
                    filename: "summary.md".to_string(),
                    mime_type: "text/markdown".to_string(),
                },
                ReportMeta {
                    filename: "report.json".to_string(),
                    mime_type: "application/json".to_string(),
                },
            ],
            ..sample_record()
        }
    }

    #[derive(serde::Serialize)]
    struct V1TaskRunRecord {
        schema_version: u32,
        task_spec_hash: [u8; 32],
        input_patterns: Vec<String>,
        inputs: Vec<FileEntry>,
        output_patterns: Vec<String>,
        outputs: Vec<FileEntry>,
        detected_input_patterns: bool,
        detected_output_patterns: bool,
        outputs_hash: [u8; 32],
        env_hash: [u8; 32],
        pkg_dep_hash: [u8; 32],
        dep_outputs: BTreeMap<String, [u8; 32]>,
        exit_status: i32,
        succeeded: bool,
        start_unix_ms: u64,
        end_unix_ms: u64,
    }

    #[derive(serde::Serialize)]
    struct V2TaskRunRecord {
        schema_version: u32,
        task_spec_hash: [u8; 32],
        input_patterns: Vec<String>,
        inputs: Vec<FileEntry>,
        output_patterns: Vec<String>,
        outputs: Vec<FileEntry>,
        detected_input_patterns: bool,
        detected_output_patterns: bool,
        outputs_hash: [u8; 32],
        env_hash: [u8; 32],
        pkg_dep_hash: [u8; 32],
        dep_outputs: BTreeMap<String, [u8; 32]>,
        exit_status: i32,
        succeeded: bool,
        start_unix_ms: u64,
        end_unix_ms: u64,
        reports: Vec<ReportMeta>,
    }

    fn sample_inputs() -> Vec<FileEntry> {
        vec![FileEntry {
            path: "src/main.ts".to_string(),
            size: 42,
            mtime_ns: 111,
            hash: [12; 32],
            absent: false,
        }]
    }

    fn sample_outputs() -> Vec<FileEntry> {
        vec![FileEntry {
            path: "dist/main.js".to_string(),
            size: 84,
            mtime_ns: 222,
            hash: [13; 32],
            absent: false,
        }]
    }

    fn sample_dep_outputs() -> BTreeMap<String, [u8; 32]> {
        BTreeMap::from([("dep#build".to_string(), [17; 32])])
    }

    fn sample_reports_meta() -> Vec<ReportMeta> {
        vec![ReportMeta {
            filename: "summary.md".to_string(),
            mime_type: "text/markdown".to_string(),
        }]
    }

    fn sample_v2_record() -> V2TaskRunRecord {
        V2TaskRunRecord {
            schema_version: SCHEMA_VERSION_V2,
            task_spec_hash: [11; 32],
            input_patterns: vec!["src/**/*.ts".to_string()],
            inputs: sample_inputs(),
            output_patterns: vec!["dist/**/*.js".to_string()],
            outputs: sample_outputs(),
            detected_input_patterns: true,
            detected_output_patterns: true,
            outputs_hash: [14; 32],
            env_hash: [15; 32],
            pkg_dep_hash: [16; 32],
            dep_outputs: sample_dep_outputs(),
            exit_status: 0,
            succeeded: true,
            start_unix_ms: 1_000,
            end_unix_ms: 2_000,
            reports: sample_reports_meta(),
        }
    }

    fn sample_v1_record() -> V1TaskRunRecord {
        V1TaskRunRecord {
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
