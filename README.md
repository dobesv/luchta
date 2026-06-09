# Luchta

Luchta is a Rust-based alternative to Microsoft's Lage build system, specifically designed for JavaScript/TypeScript (yarn) monorepos. The project is named after Luchta, the Irish god of woodwork, reflecting its role in crafting and assembling complex software projects.

**Status:** Early-stage / Work-in-Progress (WIP).

## Overview

Luchta optimizes monorepo workflows by:
- Discovering yarn workspace packages.
- Building a **Package Graph** for dependency topology.
- Constructing a **Task Graph** (e.g., `ui#build`) for granular execution.
- Executing tasks in topological order with **weight-based concurrency** to manage resources like RAM.

## Crate Layout

The project is organized into a multi-crate Cargo workspace under `crates/`:

- `luchta-types`: Shared types such as `PackageName`, `TaskId`, and `TaskDefinition`.
- `luchta-lockfiles`: `Lockfile` trait abstraction and Yarn v1 implementation.
- `luchta-workspace`: Workspace discovery and Package Graph construction.
- `luchta-engine`: Task Graph construction and the weighted task executor.
- `luchta-cli`: Entry point, `clap` CLI, and executable config script loading.

## Development

### Building and Testing

To build the entire workspace:
```bash
cargo build --workspace
```

Tests run via [cargo-nextest](https://nexte.st/). Install it once with
`cargo install cargo-nextest --locked`, then:
```bash
cargo nextest run --workspace
```

It is recommended to run the suite **5 times** to catch flaky tests before
opening a PR:
```bash
cargo nextest run --workspace --stress-count=5
```

To build and run the CLI:
```bash
cargo build -p luchta-cli
./target/debug/luchta --help
```

### Verification

Before committing, run the full pipeline (see `AGENTS.md` for details):

```bash
cargo build --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --stress-count=5
cs delta $(git merge-base HEAD origin/main)   # CodeScene — must be all green
```

The CodeScene `cs delta` check **must be all green** (no new code-health
problems) for a change to be considered done.

### Releasing

Releases are managed by [knope](https://knope.tech/) and driven by changeset
files in `.changeset/`. Add a changeset for every user-visible change:

```markdown
---
luchta: minor
---
Brief description of the change.
```

The front-matter key is always `luchta`, and the bump level is one of `patch`,
`minor`, or `major`. To cut a release, run the **Prepare Release** GitHub
Action (or `knope release` locally); knope bumps the version, updates
`CHANGELOG.md`, and pushes a `luchta/v<version>` tag. The tag push triggers the
**Release** workflow, which cross-builds the `luchta` binary for Linux, macOS,
and Windows and attaches the archives to the GitHub Release. The Release
workflow can also be run on demand (`workflow_dispatch`) to build binaries
without cutting a version.

## Usage Sketch

Luchta is configured via an executable script at the workspace root matching `luchta-config.*` (e.g., `.ts`, `.js`, `.sh`, `.py`). 

The script **must** have a shebang line and print its configuration to `stdout` as a JSON object with `camelCase` fields. Luchta executes the script directly and parses this JSON to load the pipeline definition.

Example `luchta-config.ts`:
```typescript
#!/usr/bin/env node
console.log(JSON.stringify({ 
  pipeline: { 
    build: { 
      dependsOn: ["^build"], 
      weight: 2 
    } 
  }, 
  concurrency: { 
    maxWeight: 10 
  } 
}));
```

```bash
# Run the build task for all relevant packages
luchta run build
```

### `dependsOn` Syntax
Luchta supports flexible dependency definitions between tasks:
- `^task`: Direct upstream packages' task.
- `^^task`: Transitive upstream packages' task.
- `task`: Same-package task.
- `pkg#task`: Specific package and task.

## Roadmap

- **Phase 1 (Current):** Multi-crate workspace skeleton, CI, and release tooling (nextest, knope changesets, GitHub release workflows).
- **Phase 2:** Foundation libraries (workspace discovery, lockfile parsing, graph construction, weighted parallel execution).
- **Future:** Caching (blake3 hashing) and advanced features.
