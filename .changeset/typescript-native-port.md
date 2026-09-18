---
luchta: minor
---
# Move the vendored TypeScript compiler off the archived typescript-go

`luchta-tsc-worker` is built from a vendored, patched copy of the TypeScript
compiler. That vendor tracked `microsoft/typescript-go`, which is now
**archived** — it was staging ground for the TypeScript 7.0 native port, and
that port is complete. `vendor/typescript` now tracks `microsoft/TypeScript`
instead, pinned to a recent commit on its `main` branch.

Yarn PnP module resolution — previously a from-scratch reimplementation
carried entirely as our own patch — now comes from upstream pull request
[microsoft/TypeScript#63919](https://github.com/microsoft/TypeScript/pull/63919),
vendored as a mechanically regenerated patch applied ahead of our own small
one. **This does not mean PnP support has landed upstream.** The PR is open
and unmerged, blocked on the TypeScript team accepting its tracking issue
(open since March 2025); this change only means our patch now tracks
upstream's implementation of PnP resolution instead of reimplementing it
ourselves, which meaningfully shrinks what we maintain.

PnP resolution was re-verified end to end against a real Yarn 3 workspace —
bare imports resolving into zip-resident packages, declaration files read out
of `.yarn/cache/*.zip`, and per-package dependency isolation all behaving
correctly, confirmed with a before/after control (removing `.pnp.cjs` and
watching the same imports fail). Yarn 4's cache layout was not exercised, and
every PnP compile in that verification used `noEmit`, so declaration emit
against a zip-resident package remains unverified.
