---
luchta: minor
---
Add generic `report` worker message and normalized SARIF/CTRF display in `luchta logs` (#27).

- Workers can now attach reports via a new `report` protocol message. Content is written verbatim to the task's cache directory.
- `luchta logs` pretty-prints reports with `application/sarif+json` or `application/vnd.ctrf+json` MIME types. SARIF output includes IDE-clickable links; CTRF provides a test result summary.
- New repeatable `--file <NAME>` flag for `luchta logs` provides raw byte-exact passthrough of named report files.
- **Cache Migration**: Task cache metadata schema bumped to v2. The first run after upgrading will regenerate the task cache.
