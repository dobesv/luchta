---
luchta: patch
---
Fix `cargo xtask build-worker` losing the worker binary when given a relative
`--out-dir` (as the release workflow does): the Go build runs inside
`vendor/tsgo`, so a relative output path resolved there and was then deleted by
the post-build submodule cleanup, leaving no `luchta-tsc-worker` for the release
archive step. The output directory is now resolved to an absolute path before
building, and the release workflow verifies the binary exists right after the
build step.
