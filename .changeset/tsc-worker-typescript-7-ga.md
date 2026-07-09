---
luchta: minor
---
Update the bundled `luchta-tsc-worker` to TypeScript 7.0 GA. The vendored
`typescript-go` submodule is rebased from the release-candidate merge-base onto
the `typescript/v7.0.2` tag, and our Yarn PnP + worker-integration patch is
re-applied on top. Yarn PnP module resolution is still carried as a local patch
because it has not yet merged upstream (microsoft/typescript-go#460 / #1966).
