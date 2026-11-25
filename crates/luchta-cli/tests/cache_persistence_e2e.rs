use std::fs;

use assert_fs::prelude::*;
use luchta_cache::{Cache, RunReason, SCHEMA_VERSION_V6};

mod common;

use common::{init_git, write_basic_package, write_counter_task_config, write_root_workspace};

struct CachePersistenceWorkspace<'a> {
    task_json: &'a str,
    extra_files: &'a [(&'a str, &'a str)],
}

impl CachePersistenceWorkspace<'_> {
    fn build(self, temp: &assert_fs::TempDir) {
        write_root_workspace(temp);
        write_counter_task_config(temp, self.task_json);
        write_basic_package(temp, "pkgbuild");
        for (path, contents) in self.extra_files {
            temp.child(path).write_str(contents).unwrap();
        }
        init_git(temp);
    }
}

fn task_cache_dir(workspace_root: &std::path::Path, task_id: &str) -> std::path::PathBuf {
    workspace_root
        .join(".luchta")
        .join("cache")
        .join(blake3::hash(task_id.as_bytes()).to_hex().as_str())
}

/// Genuine pre-Postcard record bytes captured at 905b038 before the codec
/// switch (see crates/luchta-cache/tests/fixtures/legacy-bincode/README.md).
const LEGACY_META_RECORD: &[u8] =
    include_bytes!("../../luchta-cache/tests/fixtures/legacy-bincode/meta.bincode");

/// Test: a genuine legacy `meta.bincode` record at the exact path the build
/// reads is a miss, not a crash and not a skip.
///
/// The task executes despite the legacy record, writes the new
/// `meta.postcard` next to it, and the second run skips on the new record
/// while the legacy file stays in place byte-identical.
#[test]
fn legacy_bincode_local_record_is_a_miss_not_a_skip() {
    let temp = assert_fs::TempDir::new().unwrap();
    CachePersistenceWorkspace {
        task_json: r#""app#pkgbuild":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt"}"#,
        extra_files: &[("packages/app/src.txt", "one\n")],
    }
    .build(&temp);

    let cache_dir = task_cache_dir(temp.path(), "app#pkgbuild");
    fs::create_dir_all(&cache_dir).unwrap();
    let legacy_record = cache_dir.join("meta.bincode");
    fs::write(&legacy_record, LEGACY_META_RECORD).unwrap();

    let first = common::run_luchta(&temp, "pkgbuild").success();
    let first_stdout = String::from_utf8_lossy(&first.get_output().stdout).into_owned();
    assert!(
        first_stdout.contains("✔ 1") && !first_stdout.contains("⏩"),
        "legacy record must produce a miss: task executes, stdout was:\n{first_stdout}"
    );
    temp.child("packages/app/counter.txt").assert("1\n");
    assert!(
        cache_dir.join("meta.postcard").is_file(),
        "execution must write the new-format record next to the legacy one"
    );

    let second = common::run_luchta(&temp, "pkgbuild").success();
    let second_stdout = String::from_utf8_lossy(&second.get_output().stdout).into_owned();
    assert!(
        second_stdout.contains("⏩ 1"),
        "second run must skip on the new record, stdout was:\n{second_stdout}"
    );
    temp.child("packages/app/counter.txt").assert("1\n");

    assert_eq!(
        fs::read(&legacy_record).unwrap(),
        LEGACY_META_RECORD,
        "legacy record must stay in place byte-identical"
    );
}

#[test]
fn non_cacheable_task_persists_local_run_record_but_still_reruns() {
    let temp = assert_fs::TempDir::new().unwrap();
    CachePersistenceWorkspace {
        task_json: r#""app#pkgbuild":{"worker":"shell","inputs":["src.txt"],"outputs":["counter.txt"],"command":"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt; echo run-$count > stdout.txt; echo err-$count > stderr.txt"}"#,
        extra_files: &[("packages/app/src.txt", "one\n")],
    }
    .build(&temp);

    common::run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("1\n");
    temp.child("packages/app/stdout.txt").assert("run-1\n");
    temp.child("packages/app/stderr.txt").assert("err-1\n");

    let cache_dir = task_cache_dir(temp.path(), "app#pkgbuild");
    assert!(cache_dir.join("meta.postcard").is_file());
    let cache = Cache::open(&temp.path().join(".luchta/cache")).unwrap();
    let record = cache
        .read("app#pkgbuild")
        .expect("execution record should be readable");
    assert_eq!(record.schema_version, SCHEMA_VERSION_V6);
    assert_ne!(record.run_reason, Some(RunReason::SharedCacheHit));
    assert!(record.succeeded);
    assert!(cache_dir.join("stdout.log").exists());
    assert!(cache_dir.join("stderr.log").exists());
    assert_eq!(
        fs::read_to_string(cache_dir.join("stdout.log")).unwrap(),
        ""
    );
    assert_eq!(
        fs::read_to_string(cache_dir.join("stderr.log")).unwrap(),
        ""
    );

    common::run_luchta(&temp, "pkgbuild").success();
    temp.child("packages/app/counter.txt").assert("2\n");
    temp.child("packages/app/stdout.txt").assert("run-2\n");
    temp.child("packages/app/stderr.txt").assert("err-2\n");
    assert_eq!(
        fs::read_to_string(cache_dir.join("stdout.log")).unwrap(),
        ""
    );
    assert_eq!(
        fs::read_to_string(cache_dir.join("stderr.log")).unwrap(),
        ""
    );
}
