---
luchta: patch
---
# Select packages whose resolved dependency closures changed under `--since`

`luchta run --since <ref>` and `luchta list --since <ref>` now detect lockfile-only dependency updates. A package counts as changed when `yarn.lock` changes the resolved version, resolution, or checksum of any package in its transitive external dependency closure (including devDependencies), compared between the base ref's tree and the working tree. Transitive workspace dependents are added as usual. If `yarn.lock` is unreadable or unparseable at the base ref or in the working tree, all non-root packages are selected.

Task cache keys now also factor in lockfile checksum and resolution identity (such as edits under `.yarn/patches`), so expect a one-time cache miss after upgrading.

Fixes #939.
