---
luchta: minor
---

# Replace bincode with Postcard in the build cache

Swap the unmaintained bincode serializer for Postcard on every cache surface: local run records, shared-cache entry metadata, snapshot index shards, and the cache-key encodings themselves. The bincode 3.x line does not compile against this workspace, so the Renovate-proposed upgrade (PR #1094) is superseded by this change.

This is a one-time cache reset. Storage identities changed with the codec — run records are `meta.postcard` (schema 6), fallback metadata is `entries/<input_key>.postcard` (schema 2), snapshot shards are `<shard_id>.postcard` (schema 5) — and every cache key was re-encoded, so the first build after upgrading misses all prior entries and rebuilds from scratch. No migration runs and nothing needs deleting by hand: old `meta.bincode` records, `entries/<input_key>.bin` objects, `*.bincode` shards and `*.merged` sidecars are never read, never crash a build, and are left in place. Cache GC only manages current-format files, so those legacy files stay on disk until you remove them yourself (or, remotely, your object-store lifecycle rules expire them by age).
