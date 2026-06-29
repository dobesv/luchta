---
title: "Compress at the content-addressed serialization chokepoint with hash-before-compress and magic-byte passthrough"
date: 2026-06-29
category: logic-errors
problem_type: logic_error
component: luchta-cache/shared-snapshot
root_cause: "content-addressed ID must derive from uncompressed bytes; naive compression before hashing breaks addressing"
resolution_type: code_fix
severity: high
tags:
  - compression
  - content-addressing
  - zstd
  - backward-compat
  - cache
  - blake3
plan_ref: luchta-144-compress-snapshot-metadata
---
## Problem

Adding compression to a content-addressed cache requires care: the identifier (`shard_id = blake3(encoded_bytes)`) must remain stable. Compress-then-hash changes all IDs, breaking deduplication and remote addressing. Additionally, pre-existing uncompressed local caches must degrade gracefully, not panic.

## Symptoms

**Content-address identity drift:**
- `shard_id` computed from compressed bytes differs from legacy uncompressed hashes
- Remote cache entries become unreachable (different IDs)
- Shared cache hit rate drops to zero after compression rollout

**Legacy local cache failures:**
- Raw bincode files fail to decode after compression rollout
- Cache misses where hits were expected
- Potential panics if decode errors propagate

## Investigation Steps

1. **Identified serialization chokepoint**: `snapshot.rs` has a single bincode encode path in `write_consolidated_shard`. This is where compression must be applied.

2. **Traced transport assumptions**: `remote.rs` and `rclone.rs` copy raw bytes verbatim — no transform on push/pull. Compressing at the serialization point automatically compresses remote uploads.

3. **Analyzed hash-before-compress invariant**: Existing `shard_id` derives from `blake3::hash(&encoded)` where `encoded` is uncompressed bincode. Compression must happen AFTER this line.

4. **Designed graceful read-path**: Need to detect compressed vs raw bytes. zstd frames start with magic bytes `[0x28, 0xB5, 0x2F, 0xFD]`. If present, decompress; else passthrough.

5. **Checked existing conventions**: `blob.rs` already uses `zstd` at level 3. Matched this constant (`SNAPSHOT_ZSTD_LEVEL = 3`) for consistency.

## Root Cause

The content-addressed `shard_id` is a BLAKE3 hash of the **uncompressed** bincode-encoded snapshot. If compression is applied before hashing, the ID changes for identical logical content, breaking:
- Remote cache addressing (different IDs → miss)
- Local-to-remote deduplication (same content → different IDs)
- Cache entry reuse after rollback (compressed ID → uncompressed lookup)

Additionally, existing uncompressed local cache files would fail to decode if the read path assumed compression, causing cache-layer errors instead of graceful misses.

## Solution

### Write path: hash-before-compress

```rust
// Encode to bincode first
let encoded = bincode::serde::encode_to_vec(consolidated, bincode_config())
    .expect("snapshot serialization should succeed");

// Hash UNCOMPRESSED bytes for shard_id
let shard_id = blake3::hash(&encoded).to_hex().to_string();

// Compress AFTER hashing
let on_disk = compress_snapshot_bytes(&encoded)?;

// Write compressed bytes
atomic_write(&shard_path, &on_disk)?;
```

### Read path: magic-byte auto-detection

```rust
const ZSTD_FRAME_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

fn decompress_snapshot_bytes(bytes: &[u8]) -> io::Result<Vec<u8>> {
    if bytes.starts_with(&ZSTD_FRAME_MAGIC) {
        zstd::decode_all(bytes)  // Compressed: decompress
    } else {
        Ok(bytes.to_vec())        // Raw: passthrough
    }
}
```

### Integration with decode path

Map decompression errors to existing decode-error handling (graceful cache miss):

```rust
fn decode_snapshot(bytes: &[u8], commit_key: &str) -> Result<Snapshot, DecodeError> {
    let raw = decompress_snapshot_bytes(bytes)
        .map_err(|err| DecodeError::OtherString(err.to_string()))?;
    bincode::serde::decode_from_slice(&raw, bincode_config())
    // ... schema_version check unchanged
}
```

### Byte-blind transport (zero changes)

`remote.rs` and `rclone.rs` remain untouched. They upload/download whatever bytes exist on disk:
- Compressed local file → compressed remote upload
- Pull writes compressed bytes → decompress on load

## Why This Works

1. **Hash-before-compress preserves IDs**: The `shard_id` derives from uncompressed content, so existing remote entries remain addressable and deduplication works identically.

2. **Magic-byte detection enables graceful transition**: Pre-existing uncompressed shards load via passthrough. New compressed shards decompress. Mixed states work.

3. **Decode-error mapping preserves "corrupt = cache miss" convention**: The existing pattern (`load_merged_snapshot_from_shards` catches `DecodeError`, logs warning, continues) applies unchanged. Decompression failure → `DecodeError::OtherString` → skip shard → partial load.

4. **Byte-blind transport avoids transport-layer changes**: The chokepoint is the only place needing modification. Remote sync gets compression "for free" because it copies raw bytes.

5. **Matched existing compression level**: Using zstd level 3 matches `blob.rs` convention, keeping compression behavior consistent across the codebase.

## Prevention Strategies

**Test cases:**
- Round-trip: write compressed → read → verify content matches
- Raw passthrough: write uncompressed bincode → read → verify loads correctly
- Shard ID invariant: verify `shard_id` equals `blake3(decompressed_bytes)`
- Corrupt zstd payload: bytes starting with magic + junk → verify error (cache miss, no panic)
- Remote round-trip: simulate push → pull → verify entries intact

**Code review checklist:**
- [ ] Content-addressed hash computed BEFORE compression?
- [ ] Read path has magic-byte detection for backward compat?
- [ ] Decompression errors mapped to decode-error path (graceful miss)?
- [ ] Transport layer untouched (byte-blind)?
- [ ] Compression level matches project convention?

**Best practices:**
- In content-addressed systems, ALWAYS compute the address from canonical (uncompressed) form
- Use magic-byte detection for format transitions to avoid flag days
- Keep "decode-fail = cache miss" pattern for all decode layers (schema, compression, format)

## Related Issues

- **GitHub Issue:** [#144](https://github.com/dobesv/luchta/issues/144) — Compress remote cache files
- **Related Solution:** [s3-remote-cache-via-rclone-rcd-2026-06-19.md](../integration-issues/s3-remote-cache-via-rclone-rcd-2026-06-19.md) — Append-only content-addressed shards design
- **Related Solution:** [worker-reports-schema-migration-2026-06-21.md](../logic-errors/worker-reports-schema-migration-2026-06-21.md) — Decode-fail = cache miss convention
- **Related Solution:** [yarn-berry-lockfile-parser-2026-06-09.md](../integration-issues/yarn-berry-lockfile-parser-2026-06-09.md) — Magic-byte auto-detection precedent
