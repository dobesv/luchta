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

Project automation lives in the `xtask/` crate (the standard Rust `xtask`
pattern), invoked via the `cargo xtask` alias.

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

### Project Automation (`xtask`)

Repetitive project tasks live in the `xtask` crate, run through the
`cargo xtask` alias. To install all workspace binary crates in one step:
```bash
cargo xtask install
```
This discovers every workspace member with a binary target via `cargo
metadata` and runs `cargo install --path` for each, so it stays correct as
crates are added.

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

/**
 * A dependency reference for a task. One of:
 * - `"^task"`   direct upstream packages' task
 * - `"^^task"`  transitive upstream packages' task
 * - `"task"`    same-package task
 * - `"pkg#task"` a specific package's task
 */
type DependsOn = string;

interface TaskDefinition {
  /** Tasks that must finish before this one runs. */
  dependsOn?: DependsOn[];
  /** Relative cost for the weighted scheduler. Defaults to 1. */
  weight?: number;
  /**
   * Explicit command line. When omitted, the matching `scripts` entry from
   * the package's `package.json` is used. For tasks routed to a `worker`,
   * this is passed to the worker (e.g. the Yarn subcommand) and defaults to
   * the task name.
   */
  command?: string;
  /** Name of a worker (from `workers`) that should execute this task. */
  worker?: string;
}

interface WorkerDefinition {
  /** Command that launches the long-lived worker process. */
  command: string;
}

interface LuchtaConfig {
  /** Pipeline task definitions, keyed by task name. */
  tasks?: Record<string, TaskDefinition>;
  /** Stay-resident worker definitions, keyed by worker name (Unix only). */
  workers?: Record<string, WorkerDefinition>;
  /** Scheduler limits. */
  concurrency?: {
    /** Maximum cumulative task weight allowed to run at once. */
    maxWeight: number;
  };
}

const config = {
  tasks: {
    build: {
      dependsOn: ["^build"],
      weight: 2
    },
    test: {
      dependsOn: ["build"],
      worker: "yarn"
    }
  },
  workers: {
    yarn: {
      command: "luchta-yarn-worker"
    }
  },
  concurrency: {
    maxWeight: 10
  }
} satisfies LuchtaConfig;

console.log(JSON.stringify(config));
```

The top-level `tasks` map defines the pipeline. Each task may set:
- `dependsOn`: dependency list (see syntax below).
- `weight`: relative cost for the weighted scheduler (defaults to `1`).
- `command`: explicit command line. When omitted, the matching `scripts` entry
  from the package's `package.json` is used.
- `worker`: name of a long-lived worker (from the `workers` map) that should
  execute this task. The named worker **must** be defined or the run fails.
- `cache`: opt-in build cache. Provide an object (`cache: {}`) to enable change-detection skips for successful prior runs; omit the field to disable. (Reserved for future per-task cache options.)
- `inputs`: relative input paths/globs. Literal paths and glob matches are hashed from git-tracked files, so `.gitignore` is respected.
- `outputs`: relative output paths/globs. These are checked on disk, so missing/deleted outputs invalidate cache entries even if ignored by git.
- `env`: environment variables passed to task. `value` pins explicit value, omitted `value` inherits from current `luchta` process environment, and `input: false` keeps variable available to task while excluding it from cache hash.

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
- `#task`: Root workspace (monorepo top-level) task.

### Workers
For tools with heavy startup costs (Yarn PnP, Babel, ESLint, Jest), Luchta can
route tasks to **stay-resident worker processes** instead of spawning a fresh
process per task. Workers are lazily spawned on first use and reused across
jobs, then shut down cleanly when the run completes.

Define workers in the top-level `workers` map, keyed by name, each with a
`command` that launches the long-lived worker process:
```typescript
workers: {
  yarn: { command: "luchta-yarn-worker" },
  bash: { command: "luchta-bash-worker" }
}
```
Then point a task at a worker with its `worker` field. Luchta ships the
`luchta-yarn-worker` and `luchta-bash-worker` binaries:

- **luchta-yarn-worker** runs each task through Yarn so that Yarn-injected
  environment variables (`PATH`, `NODE_OPTIONS`, …) are available. For
  yarn-worker tasks, the task's `command` becomes the Yarn subcommand
  (defaulting to the task name) and is invoked as `yarn workspace <pkg> <command>`
  for package tasks, or `yarn <command>` at the workspace root.
  Worker-reported detected inputs/outputs replace declared cache patterns for next run decisions; yarn worker always adds `package.json` to detected inputs so script changes invalidate cache entries.
- **luchta-bash-worker** runs arbitrary commands via `sh -c`, useful for
  tasks that don't need Yarn workspace wrapping.

> **Note:** Stay-resident workers are supported on Unix only.

### Build Cache
Luchta build cache is **opt-in** per task via `cache: {}`. Cached task skips only when prior run succeeded and all cache inputs still match: task spec, significant env, package dependency versions from `yarn.lock`, dependency-task output hashes, declared or worker-detected inputs, and outputs.

- Default cache dir: `<workspace>/.luchta/cache`
- Override: `LUCHTA_CACHE_DIR=/abs/path`
- Inputs use git-tracked listing, so `.gitignore` is honored for globs and literals.
- Outputs are checked directly on disk, so missing output reruns task.
- Worker-detected inputs/outputs replace declared patterns for later cache checks.
- Logs are stored in cache records; only FAILED-task logs are printed by default.

Example:
```typescript
build: {
  worker: "yarn",
  cache: {},
  inputs: ["src/**/*.ts", "package.json"],
  outputs: ["dist/**"],
  env: {
    NODE_ENV: { value: "production" },
    CI_JOB_ID: { input: false }
  }
}
```


## Roadmap

- **Phase 1 (Current):** Multi-crate workspace skeleton, CI, and release tooling (nextest, knope changesets, GitHub release workflows).
- **Phase 2:** Foundation libraries (workspace discovery, lockfile parsing, graph construction, weighted parallel execution).
- **Phase 3 (Current):** Opt-in build change-detection cache (blake3 hashing, filesystem-backed) — see "Build cache" above.
- **Future:** Cross-process build locking and remote cache.
