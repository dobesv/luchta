---
luchta: patch
---
Stop logging `failed to read snapshot shard ... No such file or directory`
warnings when a snapshot shard is missing. A missing shard is a benign,
expected condition (e.g. pruned or not yet synced from a remote cache), so it
is now skipped silently. Other shard read errors are still reported.
