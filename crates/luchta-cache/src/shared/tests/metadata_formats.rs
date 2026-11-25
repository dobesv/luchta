use super::*;
use crate::record::SCHEMA_VERSION_V5;

#[test]
fn postcard_snapshot_restores_through_fallback_entry_meta() {
    let record = fixture_record();
    let meta = fixture_meta(&record);
    let (staged_record, stdout, entry) =
        prepare_from_fixture(false, meta).expect("valid fallback should stage");
    assert!(entry.inline_meta.is_none());
    assert_eq!(staged_record, record);
    assert_eq!(stdout, b"fallback stdout");
}

#[test]
fn inline_and_fallback_reject_wrong_entry_schema() {
    let mut meta = fixture_meta(&fixture_record());
    meta.schema_version = ENTRY_META_SCHEMA_VERSION - 1;
    assert_rejected_inline_and_fallback(meta);
}

#[test]
fn inline_and_fallback_reject_wrong_record_schema() {
    let mut record = fixture_record();
    record.schema_version = SCHEMA_VERSION_V5;
    assert_rejected_inline_and_fallback(fixture_meta(&record));
}

#[test]
fn inline_and_fallback_reject_trailing_record_bytes() {
    let mut meta = fixture_meta(&fixture_record());
    meta.record.push(0);
    assert_rejected_inline_and_fallback(meta);
}

fn assert_rejected_inline_and_fallback(meta: EntryMeta) {
    assert!(
        prepare_from_fixture(true, meta.clone()).is_none(),
        "inline hit"
    );
    assert!(prepare_from_fixture(false, meta).is_none(), "fallback hit");
}

fn fixture_record() -> TaskRunRecord {
    let mut record = no_output_record([9; 32], crate::combined_outputs_hash(&[]));
    record.inputs.push(FileEntry {
        path: "src/example.ts".into(),
        size: 5,
        mtime_ns: -(1_i128 << 70),
        hash: [9; 32],
        absent: false,
    });
    record.dep_outputs.insert("pkg#prep".into(), [13; 32]);
    record
}

fn fixture_meta(record: &TaskRunRecord) -> EntryMeta {
    EntryMeta {
        schema_version: ENTRY_META_SCHEMA_VERSION,
        outputs_hash: record.outputs_hash,
        has_outputs: false,
        record: encode(record).unwrap(),
        stdout: b"fallback stdout".to_vec(),
        stderr: Vec::new(),
        reports: Vec::new(),
    }
}

fn prepare_from_fixture(
    inline: bool,
    meta: EntryMeta,
) -> Option<(TaskRunRecord, Vec<u8>, SnapshotEntry)> {
    let temp_repo = TempDir::new().unwrap();
    setup_git_repo(temp_repo.path());
    create_commit(temp_repo.path());
    let temp_cache = TempDir::new().unwrap();
    let cache = open_test_cache(temp_repo.path(), temp_cache.path(), 10);
    let package_dir = temp_repo.path().join("pkg");
    fs::create_dir_all(&package_dir).unwrap();

    let mut entry = sample_entry_with_seed(9, meta.outputs_hash);
    let input_key = entry.input_key;
    if inline {
        entry.inline_meta = Some(meta);
    } else {
        write_entry_meta(cache.paths(), &input_key, &meta).unwrap();
    }
    let bucket = cache.write_bucket_key().unwrap();
    assert_eq!(
        cache.snapshot_store.merge_entry(bucket, entry),
        MergeResult::Inserted
    );

    let prepared = cache.prepare_restore(&input_key, &package_dir)?;
    let (candidate, entry) = prepared.into_parts();
    let (hit, written_paths) = candidate.commit().unwrap();
    assert!(written_paths.is_empty());
    Some((hit.record, hit.stdout, entry))
}
