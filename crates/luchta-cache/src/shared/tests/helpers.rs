use super::*;

pub(super) fn open_test_cache(repo: &Path, cache_dir: &Path, day_window: usize) -> SharedCache {
    SharedCache::open_with_cache_dir(repo, 1_000_000, day_window, Some(cache_dir)).unwrap()
}

pub(super) fn no_output_record(task_spec_hash: [u8; 32], outputs_hash: [u8; 32]) -> TaskRunRecord {
    let mut record = sample_record(true, 200);
    record.output_patterns.clear();
    record.outputs.clear();
    record.outputs_hash = outputs_hash;
    record.task_spec_hash = task_spec_hash;
    record
}

pub(super) struct NoOutputStore<'a> {
    pub(super) cache: &'a SharedCache,
    pub(super) repo: &'a Path,
    pub(super) package: &'a Path,
    pub(super) task_id: &'a str,
    pub(super) input_key: &'a [u8; 32],
    pub(super) record: &'a TaskRunRecord,
    pub(super) stdout: &'a [u8],
}

pub(super) fn store_no_output(input: NoOutputStore<'_>) -> StoreOutcome {
    input
        .cache
        .store(
            input.task_id,
            input.input_key,
            &input.record.outputs_hash,
            input.package,
            &[],
            input.record,
            input.stdout,
            b"",
            &[],
            input.repo,
        )
        .unwrap()
}

pub(super) fn input_hash_with_content(hash: [u8; 32]) -> [u8; 32] {
    crate::resolve::combined_inputs_hash(&[FileEntry {
        path: "src/main.ts".to_string(),
        size: 10,
        mtime_ns: 0,
        hash,
        absent: false,
    }])
}

pub(super) fn assert_inline_stdout(snapshot: &Snapshot, input_key: [u8; 32], expected: &[u8]) {
    assert_eq!(
        snapshot.entries[&input_key_hex(input_key)]
            .inline_meta
            .as_ref()
            .expect("entry should carry inline metadata")
            .stdout,
        expected
    );
}

pub(super) fn assert_round_trip_hit(
    hit: &RestoredHit,
    written_paths: &[PathBuf],
    restore_dir: &Path,
) {
    assert_eq!(
        (
            hit.outputs_hash,
            hit.stdout.as_slice(),
            hit.stderr.as_slice(),
            hit.record.succeeded,
        ),
        ([7; 32], &b"stdout output"[..], &b"stderr output"[..], true,)
    );
    assert_eq!(written_paths, [restore_dir.join("dist/main.js")]);
}
