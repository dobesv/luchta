---
luchta: patch
---

# Move the vendored TypeScript worker to the `ts7-release` branch tip

`vendor/tsgo` moves from the `typescript/v7.0.2` tag to `9dd50136`, the current
tip of `microsoft/typescript-go`'s `ts7-release` branch. `patches/tsgo.patch`,
which carries Luchta's Yarn PnP resolution support and the `luchta-tsc-worker`
itself, applies unmodified.

Nothing about the worker's behaviour changes. The three commits in this range
touch only build tooling and npm packaging — no Go source at all — so type
checking, emit, diagnostics and PnP resolution are byte-for-byte what they were.
Upgrading is safe and gains you nothing visible; it exists to keep the submodule
on the branch tip rather than a tag several commits behind it.

Worth knowing for anyone maintaining the patch: `microsoft/typescript-go` is now
archived, and this branch will not move again. TypeScript 7 development has
continued in `microsoft/TypeScript`. Any future update is a port to that
repository, not another submodule bump.
