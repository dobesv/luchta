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
cs delta origin/HEAD   # CodeScene — must be all green
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
 * - `"#task"`    a specific top-level task
 */
type DependsOn = string;

interface EnvSpec {
  /** Explicit value for the variable. Pins the value and is cache-relevant. */
  value?: string;
  /** Fallback value if the variable is unset in the ambient environment. Cache-relevant. */
  default?: string;
  /** Whether the variable should be included in the build cache hash. Defaults to true. */
  input?: boolean;
}

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
  /** Environment variables for this task. Overrides worker and global env. */
  env?: Record<string, EnvSpec>;
}

interface WorkerDefinition {
  /** Command that launches the long-lived worker process. */
  command: string;
  /** Environment variables for all tasks running on this worker. Overrides global env. */
  env?: Record<string, EnvSpec>;
}

interface LuchtaConfig {
  /** Global environment variables for all tasks. */
  env?: Record<string, EnvSpec>;
  /** Pipeline task definitions, keyed by task name (or pkg#task, #task). */
  tasks?: Record<string, TaskDefinition>;
  /** Stay-resident worker definitions, keyed by worker name (Unix only). */
  workers?: Record<string, WorkerDefinition>;
  /** Scheduler limits. */
  concurrency?: {
    /** Maximum cumulative task weight allowed to run at once. Overridden by --max-weight / LUCHTA_MAX_WEIGHT. */
    maxWeight: number;
  };
}

const config = {
  env: {
    NODE_ENV: { value: "production" }
  },
  tasks: {
    build: {
      dependsOn: ["^build"],
      weight: 2,
      env: {
        BUILD_TYPE: { value: "full" }
      }
    },
    "#prep": {
      command: "echo 'Top-level prep'"
    },
    "web#test": {
      dependsOn: ["build", "#prep"],
      worker: "yarn",
      env: {
        CI: { input: false } // Passed to task but doesn't affect cache hash
      }
    },
    test: {
      dependsOn: ["build"],
      worker: "yarn"
    }
  },
  workers: {
    yarn: {
      command: "luchta-yarn-worker",
      env: {
        YARN_CACHE_FOLDER: { default: "./.yarn-cache" }
      }
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
- `inputs`: relative input paths/globs. Literal paths and glob matches are hashed from git-tracked files, so `.gitignore` is respected. See [Input Pattern Prefixes](#input-pattern-prefixes).
- `outputs`: relative output paths/globs. These are checked on disk, so missing/deleted outputs invalidate cache entries even if ignored by git.
- `env`: environment variables for the task. See [Environment Variables](#environment-variables) for details on scopes and resolution modes.

### Input Pattern Prefixes

`inputs` and worker-reported `detected_inputs` support package/root prefixes in addition to bare package-relative paths:

| Prefix | Resolves against | Semantics |
| --- | --- | --- |
| `#path` | repo root | literal → absent if missing; glob → wildcard |
| `@scope/pkg#path` / `pkg#path` | named package | literal → absent if missing; glob → wildcard |
| `^path` | direct upstream packages | always wildcard; never errors on no match |
| `^^path` | transitive upstream packages | always wildcard; never errors on no match |
| bare `path` | own package | literal → absent if missing; glob → wildcard |

Notes:
- `^` and `^^` are wildcard-only even when the suffix looks like a literal path.
- Inter-package `outputs` are not supported; prefixes apply to cache inputs only.
- Cross-package inputs obey the target package's `.gitignore` / git-tracked file view because resolution happens relative to each target base directory.
- Missing named packages or path escapes fail hard.

### Task Key Formats

The `tasks` map defines how tasks are applied across the workspace:

- `task` (e.g., `build`): Default definition for all non-top-level packages. Does **not** apply to the workspace root.
- `pkg#task` (e.g., `web#build`): Specific definition for package `pkg`.
- `#task` (e.g., `#build`): A top-level task that runs at the workspace root. Only `#`-prefixed keys run at the top level.

### Running Tasks

- `luchta run build`: Runs package `build` tasks. Top-level tasks are never included.
- `luchta run -T build` (or `--top-level`): Runs the top-level `#build` task.
- `luchta run -p <PATTERN> build`: Selects tasks by package **name** (not path). Supports glob wildcards (e.g. `@repo/*`, `pkg-*`). Repeatable.
- `luchta run --since <GIT_REF> build`: Restricts goal tasks to packages changed since `GIT_REF`, plus their transitive dependents.
- `luchta run 'test*'`: Task arguments also support glob wildcards (e.g. `test:*`, `build*`).
- `luchta run -T -p app build`: Runs both `@repo/app#build` and the top-level `#build` task (`-T` is additive to `-p`).

Luchta uses a **Goal-not-filter** selection model. Filters select the entry-point goals you want to reach; transitive prerequisites of those goals always run, even if they live in packages or have task names that do not match the filter. Luchta ensures everything needed for your targets is built.

`--since <GIT_REF>` checks for package-folder changes from committed history (`GIT_REF..HEAD`), staged changes, unstaged changes, and untracked files that are not gitignored. The affected set is `changed packages ∪ transitive dependents`, then normal dependency expansion still runs prerequisites needed by those goals. If no packages are affected, `luchta run` exits 0 immediately and prints that nothing will run — **unless** top-level mode (`-T`) is requested. Top-level `-T` / `#task` goals bypass both the since filter and that early exit, so they still run regardless of whether the affected set is empty or non-empty.

Additional targeting rules:
- **AND Logic**: Filters across dimensions are combined, including `--since` (e.g. `-p pkg --since main build` matches goals where package name matches `pkg`, task name matches `build`, and package is in affected set).
- **Mandatory Tasks**: At least one task argument is required; `luchta run -p pkg` is an error.
- **Error Reporting**: If no matches are found, Luchta provides a clear error distinguishing between "no packages matched the pattern" and "no tasks matched within the selected packages".

#### Memory-pressure backpressure

`luchta run` can pause dispatching **new** tasks when memory pressure is high. In-flight tasks keep running to completion.

- `--mem-usage-threshold <BYTES_OR_PERCENT>` / `LUCHTA_MEM_USAGE_THRESHOLD`
  - Pauses new task dispatch while summed process-tree RSS is greater than threshold.
  - Accepts percentages like `50%` or absolute values like `4GiB`, `512MiB`, `2GB`, or bare bytes.
  - Default: `50%` of total system memory.
- `--mem-free-threshold <BYTES_OR_PERCENT>` / `LUCHTA_MEM_FREE_THRESHOLD`
  - Pauses new task dispatch while system available memory is less than threshold.
  - Accepts percentages like `12.5%` or absolute values like `1GiB`, `512MiB`, `500MB`, or bare bytes.
  - Default: `1/16` of total system memory.

Precedence: flag > env var > default.

Behavior: luchta pauses dispatching **NEW** tasks while process-tree RSS exceeds `--mem-usage-threshold` **or** system available memory drops below `--mem-free-threshold`. In-flight tasks run to completion. There is no timeout or auto-abort while paused; use Ctrl-C to abort.

Status line: while paused, periodic progress output appends `⚠️ mem usage high` and/or `⚠️ system free memory low`.

#### Concurrency weight override

- `--max-weight <WEIGHT>` / `LUCHTA_MAX_WEIGHT`
  - Overrides the global maximum cumulative task weight allowed to run at once.
  - Accepts a positive integer. `0` or empty values are rejected.
  - Default: `concurrency.maxWeight` from config, or available parallelism.

Precedence: flag > env var > config `concurrency.maxWeight` > default.

### `dependsOn` Syntax

Luchta supports flexible dependency definitions:

- `^task`: Direct upstream packages' task.
- `^^task`: Transitive upstream packages' task.
- `task`: Same-scope task. Inside a package task, targets the same package; inside a `#task`, targets the top-level.
- `pkg#task`: Specific package and task.
- `#task`: Specific top-level (workspace root) task.


### Environment Variables

Environment variables can be declared at three scopes, with the following precedence:
**Task > Worker > Global**. A variable defined in a more specific scope overrides the same variable name from a broader scope.

Each variable in an `env` map follows one of four modes based on the fields provided:

| Mode | Configuration | Description | Cache-Relevant? |
| --- | --- | --- | --- |
| **Set** | `value: "..."` | Use the exact provided value. | Yes |
| **Inherit** | *(neither `value` nor `default`)* | Inherit from the ambient environment of the `luchta` process. | Yes |
| **Set Default** | `default: "..."` | Use ambient environment if present, otherwise fall back to the default. | Yes |
| **Cache Ignore** | `input: false` | Inherit from ambient environment, but exclude from the build cache hash. | No |

**Notes:**
- An empty string (`value: ""`) counts as a present value and does not fall through to a default.
- `luchta check` will report an error if both `value` and `default` are set for the same variable in a single scope.
- The build cache hash uses the **effective** resolved value (including the `default` fallback).

#### Strict Mode & Passthrough Whitelist

Luchta executes task subprocesses in a **strict environment**. The ambient environment is cleared, and only the following are injected:
1. Resolved variables declared in your `luchta-config`.
2. A built-in **passthrough whitelist** of essential variables.

Variables in the passthrough whitelist are provided to the subprocess but **do not affect the build cache hash**, ensuring that caches remain portable across different machines.

**Passthrough Whitelist:**
`PATH`, `PATHEXT`, `LD_LIBRARY_PATH`, `DYLD_FALLBACK_LIBRARY_PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `USERPROFILE`, `APPDATA`, `PROGRAMDATA`, `SystemRoot`, `SYSTEMDRIVE`, `WINDIR`, `ProgramFiles`, `ProgramFiles(x86)`, `TMPDIR`, `TMP`, `TEMP`, `TERM`, `COLORTERM`, `FORCE_COLOR`, `NO_COLOR`, `LANG`, `LC_ALL`, `TZ`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, `CI`, `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`, `http_proxy`, `https_proxy`, `no_proxy`.

Declared variables always override whitelist variables on name collision.
### Workers
For tools with heavy startup costs (Yarn PnP, Babel, ESLint, Jest), Luchta can
route tasks to **stay-resident worker processes** instead of spawning a fresh
process per task. Workers are lazily spawned on first use and reused across
jobs, then shut down cleanly when the run completes.

Workers are defined in the top-level `workers` map, keyed by name. They can be
defined as a bare string (command only) or an object (command + dependencies):

```typescript
workers: {
  // Bare string form: command only
  bash: "luchta-bash-worker",
  // Object form: command and optional dependencies
  yarn: {
    command: "luchta-yarn-worker",
    dependsOn: ["#prep"]
  }
}
```

Then point a task at a worker with its `worker` field. Luchta ships several
standard worker binaries and a set of composable filters.

#### Worker `dependsOn`
Workers can declare their own dependencies in the configuration.
`workers.<name>.dependsOn` uses the same syntax as task `dependsOn` (see below).
These dependencies are automatically appended (engine-side) to every task that
uses that worker.

Injected worker dependencies are:
- Deduped against existing task dependencies.
- Persistent even if the worker's `resolve` protocol message tries to modify
  task dependencies.
- Tolerant of pointing at pruned or missing tasks.

#### Standard Worker Binaries
- **luchta-yarn-worker** runs each task through Yarn so that Yarn-injected
  environment variables (`PATH`, `NODE_OPTIONS`, …) are available. For
  yarn-worker tasks, the task's `command` becomes the Yarn subcommand
  (defaulting to the task name) and is invoked as `yarn workspace <pkg> <command>`
  for package tasks, or `yarn <command>` at the workspace root.
  Worker-reported detected inputs/outputs replace declared cache patterns for
  next run decisions; yarn worker always adds `package.json` to detected inputs
  so script changes invalidate cache entries.
- **luchta-bash-worker** runs arbitrary commands via `sh -c`, useful for
  tasks that don't need Yarn workspace wrapping.

#### Wrapper & Filter Workers
Luchta provides a set of composable wrapper workers that can be chained using
`--` to add laziness or conditional pruning to any worker. Each wrapper spawns
the next stage in the chain as a child process and forwards the JSONL protocol.
Composition works from left to right; the rightmost stage is the real worker.
Pruning is silent.

- **luchta-lazy-worker -- <delegate...>**
  Answers `resolve` with `Accept` immediately without starting the delegate.
  Spawns the delegate only on the first `Run` request and reuses it thereafter.
  Useful for deferring expensive worker startup until a task actually runs.
- **luchta-file-exists-filter <glob>... -- <delegate...>**
  During `resolve`, prunes the task unless at least one of the provided file
  globs matches a file within the task's directory (OR semantics).
- **luchta-yarn-filter [--script NAME]... [--dependency NAME]... -- <delegate...>**
  Prunes tasks based on `package.json` content. All conditions must be met (AND):
  - Default: Prune unless a script matching the task name exists.
  - `--script NAME`: Prune unless the specified script name(s) exist.
  - `--dependency NAME`: Prune unless the specified package(s) are present in
    `dependencies` or `devDependencies`. If only `--dependency` is used, the
    default script check is skipped.
- **luchta-command-filter <predicate cmd...> -- <delegate...>**
  Runs the provided predicate command in the task's directory during `resolve`.
  If the command exits with code 0, the task is kept; otherwise, it is pruned.
  Predicate output is kept off the protocol stdout.

**Example: A complex worker chain**
This example only runs the Babel worker if `package.json` has a `babel`
dependency, a `babel.config.*` file exists, and the worker startup is deferred
until needed:

```typescript
workers: {
  babel: {
    command: "luchta-yarn-filter -- luchta-file-exists-filter 'babel.config.*' -- luchta-command-filter jq -e '.dependencies.babel' package.json -- luchta-lazy-worker -- yarn workspace luchta-workers luchta-babel-worker"
  }
}
```

> **Note:** Stay-resident workers and filters are supported on Unix only.


### Build Cache
Luchta build cache is **opt-in** per task via `cache: {}`. Cached task skips only when prior run succeeded and all cache inputs still match: task spec, significant env, package dependency versions from `yarn.lock`, dependency-task output hashes, declared or worker-detected inputs, and outputs.

- Default cache dir: `<workspace>/.luchta/cache`
- Override: `LUCHTA_CACHE_DIR=/abs/path`
- Inputs use git-tracked listing, so `.gitignore` is honored for globs and literals.
- Input prefixes may target repo root (`#...`), named packages (`pkg#...`, `@scope/pkg#...`), direct upstream packages (`^...`), or transitive upstream packages (`^^...`).
- `^` / `^^` inputs are wildcard-only and never error on zero matches; missing literals become `absent` entries only for bare / `#` / `pkg#` forms.
- Outputs are checked directly on disk, so missing output reruns task.
- Worker-detected inputs/outputs replace declared patterns for later cache checks.
- Inter-package outputs are not supported.
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



### Shared Build Cache

The shared build cache is a cross-worktree, cross-clone cache that restores task **outputs** and logs from prior builds. While the standard [Build Cache](#build-cache) is local to a single workspace, the shared cache allows developers and CI to reuse results across different checkouts of the same repository.

#### Concept
- **Commit-Keyed:** Results are indexed by git commit hash.
- **Content-Addressed Blobs:** Build outputs are compressed and stored in a deduped blob store.
- **Read Window:** On cache lookup, Luchta consults the last 20 commits (configurable) to find a match.
- **Remote Synchronization:** Opt-in synchronization with S3 or other object stores via `rclone`.

#### Layout
By default, the cache is stored at `~/.cache/luchta` (on Linux/macOS):
- `blobs/<outputs_hash>.tar.zst` — Content-addressed compressed output archives.
- `snapshots/<commit>/<shard_id>.bincode` — Metadata snapshots, stored as append-only content-addressed shards.

#### Configuration (Environment Variables)
The shared cache is **OPT-IN** and is configured exclusively via environment variables:

- `LUCHTA_SHARED_CACHE` — Configuration mode:
    - `off` (default) — Disabled.
    - `local`, `1`, `true`, `on` — Local-only shared cache.
    - `rclone:<spec>` — Enable remote-sync via rclone, where `<spec>` is an rclone Fs base that points at a bucket and (recommended) a prefix, e.g. `rclone:my-s3:my-bucket/luchta-cache`.
- `LUCHTA_SHARED_CACHE_DIR` — Override the cache root directory.
- `LUCHTA_SHARED_CACHE_SYNC_TIMEOUT` — Maximum seconds for the initial remote sync. Default: `30`.
- `LUCHTA_SHARED_CACHE_GC_DAYS` — Retention period for local cache entries. Default: `14`.
- `LUCHTA_SHARED_CACHE_MAX_OUTPUT_MB` — Maximum size for a single task's output to be cached. Default: `250`.
- `LUCHTA_SHARED_CACHE_HISTORY` — Number of recent commits to check for snapshots. Default: `20`.

Invalid numeric values will trigger a warning and fall back to their defaults.

#### Remote Synchronization (S3/rclone)
Luchta can synchronize the shared cache with a remote object store (like S3, GCS, or Azure) using [rclone](https://rclone.org/).

1. **Setup:** Run `rclone config` to create and name a remote (e.g., `my-s3`).
2. **Enable:** Set `LUCHTA_SHARED_CACHE=rclone:<remote-name>:<bucket>/<prefix>`.
   - Example: `rclone:my-s3:my-bucket/luchta-cache`.
   - Luchta appends `blobs/` and `snapshots/` beneath this base, so a dedicated
     bucket or prefix is recommended.
   - For S3 (and other bucket-based backends) you **must** include the bucket
     name — pointing at the bare remote root (`rclone:my-s3`) is not a valid
     write target.
3. **Credentials:** Luchta does not handle credentials directly. It uses the `rclone` binary on your `PATH` and relies on your `rclone.conf` or `RCLONE_*` environment variables.

**Resilience & Performance:**
- **Build Safety:** Remote cache problems (timeouts or rclone errors) never fail a build. If an error occurs, Luchta issues a warning, disables the remote cache for the rest of the run, and continues using only the local cache.
- **No CAS Required:** Snapshots are stored as append-only content-addressed shards, eliminating the need for complex "Compare-and-Swap" operations on the remote store.
- **Garbage Collection:** Remote GC is not managed by Luchta. Use S3 bucket lifecycle rules or similar object store features to expire old objects.

#### Cacheability
A task is eligible for the shared cache if all the following are true:
- The task succeeded.
- It took at least 100ms to run.
- Its total output size is within the `LUCHTA_SHARED_CACHE_MAX_OUTPUT_MB` limit.
- All its outputs are contained within its own package directory (outputs escaping the repository root are a hard error).
- The working tree is "clean" (bare `<commit>` key) or "dirty" (staged or unstaged changes to tracked files; ignored files don't count). Both clean and dirty entries are reusable (content-validated on restore), though dirty entries are kept out of any future remote sync.

#### Maintenance
Luchta automatically performs throttled garbage collection of old local cache entries and blobs (those older than `LUCHTA_SHARED_CACHE_GC_DAYS`). The cache is read-tolerant; if a blob is missing due to GC or other reasons, it is treated as a cache miss.

#### Stats
Shared cache hits are shown in the build summary: `📦 shared: <n>`.

## Roadmap

- **Phase 1 (Current):** Multi-crate workspace skeleton, CI, and release tooling (nextest, knope changesets, GitHub release workflows).
- **Phase 2:** Foundation libraries (workspace discovery, lockfile parsing, graph construction, weighted parallel execution).
- **Phase 3 (Current):** Opt-in build change-detection cache (blake3 hashing, local and shared) — see "Build cache" and "Shared Build Cache" above.
- **Future:** Cross-process build locking.
