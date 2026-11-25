# Pre-Postcard cache fixtures

Captured at `905b03841dc430c30696530e2a0730e77b3ee3c0`, before changing any storage writer. A temporary nextest test called the repository's writers and copied their bytes. The generator was then removed; the tests use these files verbatim.

| File | Baseline writer and contents | SHA-256 |
| --- | --- | --- |
| `meta.bincode` | `Cache::write` for `pkg#build`, schema 5, fixed-int bincode 2.0.1. Output `dist/main.js` has `mtime_ns = -(1_i128 << 70)`; dependency map contains `pkg#prep`. | `7c4a628f714db731b7835784141aa701f614942c995817ff7aae4ef78b9b1ee3` |
| `entry.bin` | `write_entry_meta` at input key `[3; 32]`, schema 1, fixed-int bincode inside zstd. Contains the record above, distinct stdout/stderr, and outputs hash `[7; 32]`. | `b6df6a3926f47f6686bfbbfac3c697daffd9f6c7d032f32bbed0dadbe330e8d0` |
| `shard.bincode` | `SnapshotStore::merge_entry_with_outcome` under `20261125-00`, schema 4, varint bincode inside zstd. Contains the entry above inline. | `582381571e89304f350cbe49cad0eee885b28e00f11cc685f96b476983e95e17` |
| `embedded-meta.tar.zst` | Baseline `write_blob_with_meta` at outputs hash `[7; 32]`. Contains `dist/main.js` (`v1`), embedded logs, `.luchta-meta/meta.bincode` with the record above, and `.luchta-meta/reports/meta.postcard` (`formerly valid report`). The old report validator accepted that basename. | `8c0cb47326eb6bbbcbdfde8598b31e597f9a129a5a92af4e6902e3c48c62145a` |

The test paths may rename shard stems to check same-stem isolation; the stored bytes don't change.
