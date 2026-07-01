---
luchta: patch
---
Speed up build startup by resolving tasks concurrently. During task-graph
construction luchta asks each task's worker whether to run, modify, or prune it;
these independent worker round-trips now run concurrently (bounded by available
parallelism) instead of one at a time. On a large workspace this cuts
`luchta run` startup from ~9s to ~3s, so a fully-cached build drops from ~15s to
~11s (#154).
