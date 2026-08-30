use super::*;

#[derive(serde::Serialize)]
struct LegacySnapshotFixture {
    schema_version: u32,
    entries: BTreeMap<String, LegacySnapshotEntryFixture>,
}

#[derive(serde::Serialize)]
struct LegacySnapshotEntryFixture {
    task_id: String,
    input_key: [u8; 32],
    outputs_hash: [u8; 32],
    task_spec_hash: [u8; 32],
    env_hash: [u8; 32],
    pkg_dep_hash: [u8; 32],
    duration_ms: u64,
    output_bytes: u64,
    cached_at_unix_ms: u64,
    tool_version: Option<String>,
}

#[test]
fn snapshot_restores_through_fallback_entry_meta() {
    let temp_repo = TempDir::new().unwrap();
    setup_git_repo(temp_repo.path());
    create_commit(temp_repo.path());
    let temp_cache = TempDir::new().unwrap();
    let cache = open_test_cache(temp_repo.path(), temp_cache.path(), 10);
    let package_dir = temp_repo.path().join("pkg");
    fs::create_dir_all(&package_dir).unwrap();

    let input_key = derive_input_key([9; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
    let outputs_hash = crate::resolve::combined_outputs_hash(&[]);
    let mut record = sample_record(true, 1);
    record.output_patterns.clear();
    record.outputs.clear();
    record.outputs_hash = outputs_hash;
    let meta = EntryMeta {
        schema_version: ENTRY_META_SCHEMA_VERSION,
        outputs_hash,
        has_outputs: false,
        record: bincode::serde::encode_to_vec(&record, bincode_config()).unwrap(),
        stdout: b"legacy stdout".to_vec(),
        stderr: Vec::new(),
        reports: Vec::new(),
    };
    write_entry_meta(cache.paths(), &input_key, &meta).unwrap();

    let legacy = LegacySnapshotFixture {
        schema_version: 2,
        entries: BTreeMap::from([(
            input_key_hex(input_key),
            LegacySnapshotEntryFixture {
                task_id: "pkg#lint".to_owned(),
                input_key,
                outputs_hash,
                task_spec_hash: record.task_spec_hash,
                env_hash: record.env_hash,
                pkg_dep_hash: record.pkg_dep_hash,
                duration_ms: 1,
                output_bytes: 0,
                cached_at_unix_ms: record.end_unix_ms,
                tool_version: None,
            },
        )]),
    };
    let raw =
        bincode::serde::encode_to_vec(&legacy, crate::shared::snapshot::snapshot_bincode_config())
            .unwrap();
    let encoded = zstd::encode_all(raw.as_slice(), 3).unwrap();
    let bucket = cache.write_bucket_key().unwrap();
    let snapshot_dir = cache.paths().snapshots_dir.join(bucket);
    fs::create_dir_all(&snapshot_dir).unwrap();
    fs::write(snapshot_dir.join("legacy.bincode"), encoded).unwrap();

    let prepared = cache
        .prepare_restore(&input_key, &package_dir)
        .expect("v2 fallback restore should stage");
    let (candidate, entry) = prepared.into_parts();
    assert!(!entry.duration_trusted);
    assert!(entry.inline_meta.is_none());
    assert_eq!(candidate.stdout, b"legacy stdout");
    candidate.discard().unwrap();
}
