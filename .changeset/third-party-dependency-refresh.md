---
luchta: minor
---

# Update third-party dependencies

`swc_core` moves to 80.0.0, `thiserror` to 2.0.20, `dirs` to 7.0.0, `toml` to
1.1.6, `petgraph` to 0.8.3, `yarn-lock-parser` to 0.14.0, `sysinfo` to 0.39.6,
`notify` to 8.2 (with `notify-debouncer-full` to 0.7), `gix` to 0.87.1, and
`zstd` to 0.14. A golden-byte fixture now pins the on-disk cache encoding so a
future dependency bump that changes it on-disk will fail a test instead of
silently invalidating caches.

Two dependencies that were on the table did not move, deliberately:
`bincode` stays at 2.0.1 — the would-be 3.0 release is a tombstone whose
entire body is `compile_error!`, published only to mark the crate
unmaintained, so there is nothing to upgrade to and no cache-format change.
`libc` stays on 0.2, and `notify`/`notify-debouncer-full` stayed on their
stable lines rather than the 9.0/0.8 prereleases.

**MSRV moves to 1.96.0, up from 1.94.0 on the last release.** This branch
raised it twice along the way — first to 1.95.0 to satisfy
`oxc_sourcemap`/`oxc_resolver`/`oxc-browserslist`, then to 1.96.0 for the
`apps_v1.83.0` oxc bump — but 1.95.0 never shipped as a release, so from a
user's perspective it's one jump: 1.94.0 to 1.96.0. `rustup update stable`
covers it.

**gix 0.87 changes how a global `diff.ignoreSubmodules` git config setting
interacts with a submodule's own `.gitmodules` ignore setting.** gix 0.73
consulted only the submodule's own setting; 0.87 lets a global
`diff.ignoreSubmodules = all` override it, matching upstream git's documented
behaviour — gix was fixing a divergence, not introducing one. Practical
effect: if you have that global git config set and submodules in your
workspace, `luchta --since` may now report fewer changed paths than it did
before. This is rare in practice, but if a `--since` result looks smaller
than expected after upgrading, check for that config first.

**yarn-lock-parser 0.14 is stricter about `dependenciesMeta` and
`peerDependenciesMeta` values.** A sub-value other than `"true"` or `"false"`
is now a hard parse failure; 0.11 silently dropped the offending line
instead. Failing loudly on a malformed lockfile is a correctness improvement
over silently mis-parsing it, but it does mean a lockfile that happened to
carry such a value would previously parse quietly and will now error.
Luchta's own fixtures don't exercise this shape, but real user lockfiles are
what get parsed in practice, so it's worth knowing about if `luchta` starts
refusing a lockfile that worked before.
