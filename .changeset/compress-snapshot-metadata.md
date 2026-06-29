---
luchta: minor
---
Compress shared snapshot cache metadata (#144). Snapshot index shards (`.bincode`) and their `.merged` sidecars are now zstd-compressed at rest, shrinking the shared cache footprint and remote transfer size. Shard IDs are unchanged (still BLAKE3 of the uncompressed bincode); pre-existing uncompressed local caches are auto-detected and degrade to a cache miss rather than erroring.
