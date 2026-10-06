---
luchta: patch
---
Preserve final newlines in oxlint suppression files so repeated `--fix` runs no longer produce newline-only diffs. Keep automatic suppression pruning and end surviving files written by `--fix`, `--suppress-all`, or `--prune-suppressions` with exactly one LF.
