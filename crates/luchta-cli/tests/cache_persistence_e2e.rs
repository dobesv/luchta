use std::fs;

use assert_fs::prelude::*;

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
    assert!(cache_dir.join("meta.bincode").exists());
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
