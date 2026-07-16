---
luchta: patch
---

Fix the release pipeline silently shipping incomplete archives. The set of released binaries is now derived from a single source of truth (`cargo xtask list-release-bins`) instead of five hand-maintained lists that could drift out of sync. The release workflow builds `--workspace --bins` and packages/installs whatever that command reports, a CI drift guard fails the build if a shipped binary lacks a smoke-test probe, and the installers discover binaries from the archive rather than a hardcoded list. This restores the previously-missing `luchta-ast-grep-worker` and `luchta-worker-watcher` binaries to release archives.
