---
luchta: minor
---
oxlint worker: support a `--config <path>` option in the task command (with
quote-aware parsing for paths containing spaces) to select an explicit oxlint
config file, and apply `// eslint-disable` comments to type-aware (tsgolint)
diagnostics so suppressed errors are no longer reported. Fixes #219 and #221.
