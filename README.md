# Luchta

Luchta is a Rust-based alternative to Microsoft's Lage build system, specifically designed for JavaScript/TypeScript (yarn) monorepos. The project is named after Luchta, the Irish god of woodwork, reflecting its role in crafting and assembling complex software projects.

**Status:** Early-stage / Work-in-Progress (WIP).

## Overview

Luchta optimizes monorepo workflows by:
- Discovering yarn workspace packages.
- Building a **Package Graph** for dependency topology.
- Constructing a **Task Graph** (e.g., `ui#build`) for granular execution.
- Executing tasks in topological order with **weight-based concurrency** to manage resources like RAM.

## Installation

### ASDF

You can use `asdf` to install luchta binaries:

```
asdf plugin add luchta https://github.com/dobesv/asdf-plugins.git
asdf set luchta latest
asdf install
```

### Install script

For a fast, automated installation, use the standalone installer scripts. They detect your OS and architecture, download the latest release, and extract all available binaries to `~/.luchta/bin` (on Unix) or `%USERPROFILE%\.luchta\bin` (on Windows).

**Unix / macOS (recommended: download, inspect, run):**
```bash
curl -fsSLO https://raw.githubusercontent.com/dobesv/luchta/main/scripts/install.sh
less install.sh   # review
bash install.sh
```

**Windows PowerShell (recommended: download, inspect, run):**
```powershell
Invoke-WebRequest https://raw.githubusercontent.com/dobesv/luchta/main/scripts/install.ps1 -OutFile install.ps1
notepad .\install.ps1   # review
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

**Convenience option (pin to a release tag):**

Replace `<version>` with a released version whose tag includes these installer
scripts (available from the first release cut after they land, e.g. `0.1.14`):
```bash
curl -fsSL https://raw.githubusercontent.com/dobesv/luchta/luchta/v<version>/scripts/install.sh | bash
```

```powershell
irm https://raw.githubusercontent.com/dobesv/luchta/luchta/v<version>/scripts/install.ps1 | iex
```

**Security note:** Piping a script straight into a shell executes whatever bytes are served at that URL. For safest use, download the script, inspect it, and run it locally. For reproducible automation, replace `main` with a pinned release tag that includes these scripts (e.g. `luchta/v0.1.14`).

After installation, add the install directory to your `PATH` if the script tells you it is missing.

Luchta discovers workers (like `luchta-tsc-worker`, `luchta-yarn-worker`, etc.) via your `PATH`. Because the installer bundles all workers in the same directory as the `luchta` binary, adding that directory to your `PATH` ensures all workers are automatically resolved without additional configuration.

### Manual download

If you prefer to install manually, download the appropriate archive for your platform from the [GitHub Releases](https://github.com/dobesv/luchta/releases) page.

1. Extract the archive (e.g., `luchta-v<version>-x86_64-unknown-linux-musl.tar.gz`).
2. Move the extracted binaries to a directory of your choice.
3. Add that directory to your `PATH`.

**Note:** The archive contains the `luchta` binary along with all standard worker binaries. They should be kept together in the same directory to ensure they are correctly resolved at runtime.

### From source

To build and install Luchta from source, you will need the Rust toolchain and Go 1.26+ (for the TypeScript worker).

```bash
# Ensure submodules are initialized
git submodule update --init
# Build and install all binaries to your cargo bin directory
cargo xtask install
```

See the [Project Automation (xtask)](#project-automation-xtask) section for more details on building the Go-based workers.

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
cargo xtask install          # Install all workspace binary crates (including the Go worker)
cargo xtask build-worker     # Build the TypeScript Go worker standalone (requires Go 1.26+)
```
This discovers every workspace member with a binary target via `cargo
metadata` and runs `cargo install --path` for each, so it stays correct as
crates are added. `install` also builds the Go worker for the host and
places `luchta-tsc-worker` in the cargo bin directory alongside the Rust
binaries, so it requires Go 1.26+ and an initialized `vendor/tsgo`
submodule (`git submodule update --init`).

#### Building the TypeScript Worker

The TypeScript worker (`luchta-tsc-worker`) is written in Go and is built using `xtask`.

1. **Prerequisites:** Install [Go 1.26+](https://go.dev/doc/install) and ensure git submodules are initialized:
   ```bash
   git submodule update --init
   ```
2. **Build:**
   ```bash
   cargo xtask build-worker --target <rust-triple>
   ```
   Optional: `--out-dir <dir>` overrides the default output directory.
3. **Output:** The binary is placed at `target/<triple>/release/luchta-tsc-worker` (or `.exe` on Windows).

#### Patch Maintenance

The worker uses a vendored `vendor/tsgo` (git submodule) pinned to the upstream `microsoft/typescript-go` merge-base `e578159b7ae473127056a65748d7b3a4daa9a93f`. Changes are applied via `patches/tsgo.patch` (the diff against the fork `dobesv/typescript-go` at `9ed9a7d054c8dd0655bce2e4c3248a14da7d8772`).

**Regenerating the Patch:**
To update the patch from a scratch clone containing both remotes (`upstream=microsoft/typescript-go`, `fork=dobesv/typescript-go`):
```bash
git diff --no-color --binary e578159b7ae473127056a65748d7b3a4daa9a93f..9ed9a7d054c8dd0655bce2e4c3248a14da7d8772 \
  -- . ':!node_modules' ':!docs/superpowers/**' ':!testdata/fixtures/pnp/*.cjs' > patches/tsgo.patch
```

**Important:**
- The repository uses `core.autocrlf=input`. `.gitattributes` marks `patches/tsgo.patch -text` to ensure CRLF line endings survive checkout. Maintainers MUST preserve this attribute.
- A scheduled workflow (`patch-drift.yaml`) monitors the patch and opens a maintenance issue if it can no longer be applied.

### Verification

`cargo nextest run --workspace` is canonical test command for this workspace. Some tests call `require_nextest()` because they touch process-global state like cwd or real environment variables and rely on nextest's per-test process isolation. Plain `cargo test` will make those tests panic with guidance instead of failing nondeterministically.

Nextest also wraps each test in a hermetic environment. Ambient Luchta settings
such as `LUCHTA_SHARED_CACHE` are removed so a developer's shell or CI runner
cannot silently change test behavior. The wrapper preserves only process
essentials, Cargo/nextest metadata and binary paths, dynamic-loader and coverage
settings, and the intentional `LUCHTA_TEST_RCLONE` opt-in used by the remote
cache suite.

Before committing, run full pipeline (see `AGENTS.md` for details):

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
files in `.changeset/`. Add a changeset for every user-visible change.

The front-matter key is always `luchta` — the whole workspace shares one
version (`version.workspace = true`), so individual crate names are not valid
keys and will cause `knope release` to error. The bump level determines the
section in `CHANGELOG.md`:
- `patch` → **Fixes**
- `minor` → **Features**
- `major` → **Breaking Changes**

#### Format

**Simple:**
```markdown
---
luchta: patch
---
Fix oxfmt output truncation when buffer is full.
```

**Multi-line:**
```markdown
---
luchta: minor
---
# Support for custom build targets

Allow users to specify `--target` in the configuration file.
```

> [!IMPORTANT]
> When using a header, use a single `#`. Knope automatically re-levels it for
> the changelog. Do not use `####`.

To cut a release, run the **Prepare Release** GitHub Action (or `knope release`
locally); knope bumps the version, updates `CHANGELOG.md`, and pushes a
`luchta/v<version>` tag. The tag push triggers the **Release** workflow, which
cross-builds platform binaries for Linux, macOS, and Windows and attaches the
archives to the GitHub Release. The Release workflow can also be run on demand
(`workflow_dispatch`) to build binaries without cutting a version.

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

interface CacheConfig {
  /** Optional nonce; change to force-bust this scope's cache. */
  nonce?: string;
}

interface TaskDefinition {
  /** Tasks that must finish before this one runs. */
  dependsOn?: DependsOn[];
  /**
   * Optional filter for external package dependencies (yarn.lock).
   * Reuses the Input Pattern grammar (^, ^^, pkg#, #, globs).
   * Default: ["**/*"] (conservative).
   */
  dependencies?: string[];
  /** Opt-in build cache configuration. */
  cache?: CacheConfig;
  /** Relative input paths/globs. */
  inputs?: string[];
  /** Relative output paths/globs. */
  outputs?: string[];
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
  /** Optional cache configuration for all tasks on this worker. */
  cache?: CacheConfig;
  /** Environment variables for all tasks running on this worker. Overrides global env. */
  env?: Record<string, EnvSpec>;
}

interface LuchtaConfig {
  /** Global environment variables for all tasks. */
  env?: Record<string, EnvSpec>;
  /** Global cache configuration for all tasks. */
  cache?: CacheConfig;
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
  cache: { nonce: "v1" },
  tasks: {
    build: {
      dependsOn: ["^build"],
      cache: { nonce: "v1" },
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
      cache: { nonce: "v1" },
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
- `cache`: opt-in build cache. Provide an object (e.g. `cache: {}`) to enable change-detection skips for successful prior runs; omit the field to disable. Set the `nonce` field (e.g. `cache: { nonce: "v1" }`) to force-bust this task's cache. See [Cache Nonce](#cache-nonce-force-busting-stale-cache) for details.
- `inputs`: relative input paths/globs, including `!` exclusions. Glob patterns are resolved against the git-tracked file listing, so `.gitignore` is respected; literal (non-glob) paths are hashed directly and are included even when git-ignored. See [Input Pattern Prefixes](#input-pattern-prefixes) and [Glob Syntax](#glob-syntax).
- `outputs`: relative output paths/globs, including `!` exclusions. These are checked on disk, so missing/deleted outputs invalidate cache entries even if ignored by git. See [Glob Syntax](#glob-syntax).
- `dependencies`: optional filter for external package dependencies (from `yarn.lock`). Reuses the [Input Pattern Prefixes](#input-pattern-prefixes) grammar (`^`, `^^`, `pkg#`, `#`, globs). Its globs match dependency **names**, so they follow [Name globs](#name-globs-are-different).
    - **Default:** `["**/*"]` (conservative; includes all package dependencies).
    - **Semantic difference:** Patterns select which package dependencies' **resolved versions** (and their full transitive closures) feed the task's cache hash — they do NOT select files.
    - **Interpretation:** The filter selects "roots" from the package's immediate dependencies; each matched root contributes its FULL transitive closure to the hash. Narrowing the filter reduces cache invalidation (fewer roots → fewer version changes bust the cache).
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
| `!path` | every base above | global exclusion filter; takes no prefix |

Notes:
- `^` and `^^` are wildcard-only even when the suffix looks like a literal path.
- A `!` negation is not a prefix form: it filters everything the other patterns resolved, from every base dir. See [Negation](#negation).
- Inter-package `outputs` are not supported; prefixes apply to cache inputs only.
- Cross-package glob inputs obey the target package's `.gitignore` / git-tracked file view because resolution happens relative to each target base directory (literal paths are still taken as-is).
- Missing named packages or path escapes fail hard.

### Glob Syntax

Luchta has two families of glob. **Path globs** match files: `inputs`, `outputs`, `cacheFiles`, the `workspaces` globs in the root `package.json`, watch patterns, and `luchta-file-exists-filter`. **Name globs** match package and task names: `-p`, task arguments, and the `dependencies` filter. They share a grammar, but differ in how `*` treats `/` and in whether `!` negates. Path globs are described first; see [Name globs](#name-globs-are-different) for the differences.

Path globs are compiled by [`globset`](https://docs.rs/globset) with `literal_separator` enabled, matching what `.gitignore`, Turborepo, and lage all do. Patterns are matched against paths relative to a base directory (the package directory, or the repo root for `#`-prefixed patterns), always written with `/` separators.

| Pattern | Matches |
| --- | --- |
| `*` | zero or more characters **within one directory level** |
| `?` | exactly one character, never `/` |
| `**` | zero or more directories, as a leading `**/`, a trailing `/**`, a middle `/**/`, or the whole pattern `**` |
| `{a,b}` | `a` or `b`, where each branch is itself a glob |
| `[ab]`, `[a-z]` | one character from the set |
| `[!ab]` | one character not in the set |
| `!pattern` | **excludes** everything matching `pattern` (see below) |
| `[*]`, `\*` | the literal metacharacter |

Recursion is explicit: `src/*.ts` matches `src/a.ts` but not `src/deep/a.ts`, and you need `src/**/*.ts` for the latter. Patterns are anchored at the base directory, so a bare `*.ts` matches only top-level files — unlike `.gitignore`, where a slash-free pattern matches at any depth.

#### Negation

A pattern starting with `!` removes files instead of adding them:

```js
inputs: ["src/**", "!src/**/*.test.ts"],
outputs: ["dist/**", "!dist/**/*.map"],
```

Rules:

- **Negations always win, and order does not matter.** A file is selected when at least one normal pattern matches it and no negation does. This differs from `.gitignore`, where the last matching line wins and a later rule can re-include something excluded earlier.
- **A negation is a global filter across every base directory.** In a task's `inputs`, one `!**/*.test.ts` applies to files resolved from your own package, from `#` root patterns, and from every `^` / `^^` upstream package, each matched relative to its own base. It does not fan out into one exclusion per package.
- **Negations carry no package prefix.** `!shared#**` is not "exclude from the `shared` package" — the `shared#` is just part of the pattern text. Write the path shape you want to exclude.
- **Negation applies to literal paths too.** Listing `src/secret.ts` and `!src/secret.ts` together yields nothing.
- **A negation alone selects nothing.** There is no implicit "everything" to subtract from.
- To match a file whose name really starts with `!`, escape it: `\!important.txt`.

#### Details worth knowing

- **Alternates are comma-separated, not pipe-separated.** `{a,b}` works; `{a|b}` is not an alternate at all, it matches the literal text `a|b`. An empty branch (`{a,}`) is dropped rather than matching the empty string. Nesting braces is not supported by globset — avoid it even where it appears to work.
- **Dotfiles are not special.** `*` matches `.env`, unlike most shells.
- **Matching is case-sensitive**, on every platform.
- **A misplaced double-star is not an error.** `a**b` is accepted and behaves like `a*b`. An unclosed `{` or `[` *is* a hard error and fails the run.
- **Escaping works the same on every platform.** `\*`, `\!`, and the character-class form `[*]` all work on Windows too, because luchta forces globset's `backslash_escape` on for path globs. Name globs keep the default, where backslash escapes are Unix-only.

Whether a pattern counts as a glob at all is decided by a plain scan for `*`, `?`, `[`, or `{`. That distinction drives `.gitignore` handling for `inputs`: globs resolve against the git-tracked file listing, literals are hashed as given. See [Build Cache](#build-cache).

#### Name globs are different

`-p` package filters, task-name arguments, and the `dependencies` filter match package and task **names**, not paths, and are compiled with globset's *default* options. The pattern table above still applies — `{a,b}`, `[ab]`, case sensitivity, and the `{a|b}` trap are all the same — but three things differ:

- **`*` and `?` cross `/`.** That is deliberate: `@scope/pkg` contains a slash, so `-p '*'` still matches scoped packages and `-p '@repo/*'` matches every package in a scope. The "recursion is explicit" rule above is about path globs only.
- **`!` is not negation.** `-p '!@repo/app'` is not an error — it compiles to a pattern matching a package literally *named* `!@repo/app`, so it silently selects nothing. To narrow a run, list the packages you want.
- **Backslash escapes are Unix-only.** Name globs do not force `backslash_escape`, so globset disables it on Windows where `\` is a path separator. Use the character-class form (`[*]`) if a pattern must be portable.

`**` rarely adds anything to a name glob, since `*` already crosses `/`: `**/*` and `*` select the same names. It differs only beside a slash, where it makes that slash optional — `**/pkg` matches a bare `pkg` while `*/pkg` does not.

#### Two exceptions

- `.oxfmtrc` patterns read by the oxfmt worker (`ignorePatterns`, and `files` / `excludeFiles` inside `overrides`) use full **gitignore** semantics via the `ignore` crate: `!` negates with last-match-wins, a leading `/` anchors to the directory holding the config file, a trailing `/` matches directories only, and a slash-free pattern matches at any depth.
- The ast-grep worker's `languageGlobs` keep globset's defaults, so `*.vue` there matches at any depth, mirroring ast-grep upstream.

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
- `luchta run 'test*'`: Task arguments also support glob wildcards (e.g. `test:*`, `build*`). Package and task patterns match **names**, so they follow [Name globs](#name-globs-are-different), not the path-glob rules used by `inputs` and `outputs`.
- `luchta run -T -p app build`: Runs both `@repo/app#build` and the top-level `#build` task (`-T` is additive to `-p`).
- `luchta run --continue build`: Keep building after a failure — independent tasks still run; only the failed task's transitive dependents are skipped. Exits non-zero if anything failed.

Luchta uses a **Goal-not-filter** selection model. Filters select the entry-point goals you want to reach; transitive prerequisites of those goals always run, even if they live in packages or have task names that do not match the filter. Luchta ensures everything needed for your targets is built.

`--since <GIT_REF>` checks for package-folder changes from committed history (`GIT_REF..HEAD`), staged changes, unstaged changes, and untracked files that are not gitignored. The affected set is `changed packages ∪ transitive dependents`, then normal dependency expansion still runs prerequisites needed by those goals. If no packages are affected, `luchta run` exits 0 immediately and prints that nothing will run — **unless** top-level mode (`-T`) is requested. Top-level `-T` / `#task` goals bypass both the since filter and that early exit, so they still run regardless of whether the affected set is empty or non-empty.

Additional targeting rules:
- **AND Logic**: Filters across dimensions are combined, including `--since` (e.g. `-p pkg --since main build` matches goals where package name matches `pkg`, task name matches `build`, and package is in affected set).
- **Mandatory Tasks**: At least one task argument is required; `luchta run -p pkg` is an error.
- **Error Reporting**: If no matches are found, Luchta provides a clear error distinguishing between "no packages matched the pattern" and "no tasks matched within the selected packages".

### Awaiting Task Readiness

Use `luchta await build` as a passive barrier when another `luchta run` or
`luchta watch` process is responsible for building. It snapshots task selection
and configuration at startup, then checks the selected targets and their full
dependency subgraph once per second. Package globs, `-T` / `--top-level`,
`--workspace-root`, and implicit package selection work the same way as for
`run`:

```bash
luchta await build
luchta await -p '@repo/*' build test
luchta await -T build
```

`await` never executes a task and never restores outputs from the shared cache.
It succeeds only after local run records and filesystem outputs show that the
whole selected subgraph is current. It waits for an active build cycle to
release the repository build lock before inspecting state, then releases the
lock between polls so a watch process can build. Ctrl-C cancels either the lock
wait or the one-second delay cleanly and exits with status 0.

`await` exits non-zero instead of waiting if a selected task is invalid or its
cache state cannot be inspected.

Unlike `run`, `await` cannot make progress by itself. Unlike `watch`, it does not
monitor files or start rebuilds. Unlike `run --dry-run`, it checks actual local
readiness instead of printing an execution plan. There is no timeout: if no
matching builder is running, or builds keep failing, it waits indefinitely.

#### Progress output

With the default output mode, an interactive terminal with terminal control
support (`TERM` is not `dumb`) gets one live status line that refreshes ten times
per second. Task output, warnings, and diagnostics clear that line before they
are printed, and progress resumes below them. Long running-task lists adapt to
the available terminal width, keeping complete task names that fit and
summarizing the remainder. Compact lists omit an npm scope shared by their
visible tasks and factor a shared package prefix at a word boundary when that
does not increase the width. The unparenthesized running-task list is always the
final status segment, after elapsed time, memory usage, wave progress, and any
warnings. When stderr is redirected or piped, or `TERM=dumb`, Luchta retains
append-only status records every five seconds so logs remain readable and
deterministic. Running-task groups are ordered by their oldest member, members
are ordered from oldest to newest, and tasks running longer than five seconds
include their elapsed time in the same parentheses as any worker progress.
`--output summary` suppresses progress in either environment and prints only
the final summary.

#### Failed Task Output

When a task fails during `luchta run`, its output is replayed to the console wrapped in a clear header and footer block.

To prevent extremely large logs from flooding the terminal, `luchta run` truncates output that exceeds 100 lines. It preserves the first 30 lines and the last 70 lines, inserting a placeholder that points to the exact `luchta logs` command needed to view the full output.

```text
──▶ app#build
...
(first 30 lines)
...
… 150 lines hidden — run `luchta logs -p app build` for full output
...
(last 70 lines)
...
──◀ app#build (1200ms)
```

#### Stop-on-failure behavior

By default, `luchta run` uses an aggressive fast-stop strategy. On the first task failure:
1. New task dispatch stops immediately.
2. In-flight workers are terminated via SIGTERM, followed by SIGKILL after a 1-second grace period.
3. The process exits promptly with a non-zero code.

Use the `--continue` flag to keep building independent tasks after a failure. In this mode, only the failed task's transitive dependents are skipped. The run still exits non-zero if any failures occurred.

Failed tasks are displayed in the status line and final summary as `× <count> (<names>)`. The final summary (showing run, skipped, and failed counts) is printed on both success and failure.

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

#### Cache Nonce override

- `LUCHTA_CACHE_NONCE`
  - An independent global nonce that is read once per run and busts ALL task caches.
  - Combines with (does not override) any nonces defined in the configuration files.
  - Use this to quickly force-bust the entire workspace cache from a CI script or local shell.


#### Disable cache

- `--no-cache` / `LUCHTA_NO_CACHE`
  - Disables task skipping and shared cache interaction for `run` and `watch`.
  - Every task always runs; local skip logic is bypassed and the shared cache is neither read nor written.
  - Local workspace cache metadata is still written after each task, so subsequent normal runs can skip unchanged tasks as usual.
  - Provides a simpler, explicit alternative to the `LUCHTA_CACHE_NONCE` workaround for forcing a fresh execution.
  - The environment variable accepts `1`, `true`, or `on` (case-insensitive).

### Viewing Logs

By default, `luchta run` suppresses the output of successful tasks to keep the console clean. You can view the full stdout, stderr, and execution metadata for any previously run task using the `luchta logs` command.

All executed tasks—even those that are not opt-in for caching—persist their run records and logs locally.

#### Examples

- `luchta logs`: View logs for all tasks from the most recent runs.
- `luchta logs build`: View logs for all tasks named `build`.
- `luchta logs -p '@scope/*' build`: View logs for `build` tasks in packages matching `@scope/*`.
- `luchta logs --failed`: View logs only for tasks that failed in their last run.
- `luchta logs --show-outputs`: Include metadata for all task outputs.

#### Logs CLI Options

| Flag | Description |
|---|---|
| `tasks` (positional) | Task names to match; supports glob wildcards (e.g. `b*`). |
| `-p, --package <PKG>` | Match package name globs (not paths). Repeatable. |
| `-T, --top-level` | Match tasks defined at the workspace root instead of package tasks. |
| `--time-taken <MS>` | Filter to tasks that took at least this many milliseconds. |
| `--failed` | Filter to tasks that failed (`succeeded == false`). |
| `--show-inputs` | Show the stored effective input patterns (globs, marked `detected` or `declared`) plus input file metadata (path, size, mtime, hash) for each task. |
| `--show-outputs` | Show the stored effective output patterns (globs, marked `detected` or `declared`) plus output file metadata for each task. |
| `--show-cache-nonce` | Show the resolved nonce string persisted for the task. |
| `--file <NAME>` | Raw byte-exact passthrough of named report files (repeatable). |

`luchta logs` always displays the full, non-truncated output for every matching task.

#### Attached Reports

By default, `luchta logs` surfaces all reports attached by workers after stdout/stderr. If a report's MIME type has a native renderer, it is pretty-printed; otherwise, it is dumped verbatim.

Native MIME renderers:
- `application/sarif+json`: SARIF format. Prints IDE-clickable `[LEVEL] message --> path:line:col` lines.
- `application/vnd.ctrf+json`: CTRF format. Prints a pass/fail/skip summary plus details for each failed test.

Dispatch is based on **MIME type only**, ignoring filename/extension. Pretty-printing automatically disables coloring when piped or when `NO_COLOR` is set.

To retrieve the raw, unformatted content of specific reports (e.g., for mechanical consumers like `reviewdog`), use the `--file` flag:
```bash
luchta logs build --file sarif.json
```
The `--file` flag uses union task selection: a task is included if it has at least one of the named files. If no tasks match any of the requested files, the command exits with a non-zero error code.

### CI Integration with reviewdog

Luchta workers can attach diagnostic reports in [SARIF format](https://sarifweb.azurewebsites.net/) (`application/sarif+json`). You can use [reviewdog](https://github.com/reviewdog/reviewdog) in CI pipelines to parse these SARIF reports and post lint or static analysis findings directly to pull requests as inline comments or check runs.

While individual raw reports can be retrieved using `luchta logs --file <NAME>`, Luchta stores all execution records and attached reports on disk under `.luchta/`. In CI workflows, you can search `.luchta` directly for `*.sarif` files, aggregate them across tasks, and submit them to reviewdog.

#### Aggregating SARIF Reports and Submitting to GitHub

When running tasks across multiple workspace packages, each worker emits its own SARIF report. The script below uses `jq` to concatenate the `runs` array from every `.sarif` file found under `.luchta/` into a single SARIF 2.1.0 document (filtering out empty runs), then sends it to reviewdog.

```bash
# Map these placeholders to your CI provider's own variables.
# (e.g. in GitHub Actions: PULL_NUMBER -> github.event.number,
#  BUILD_SHA -> github.event.pull_request.head.sha, etc.)
export CI_PULL_REQUEST="${PULL_NUMBER}"
export CI_REPO_OWNER="${REPO_OWNER}"
export CI_REPO_NAME="${REPO_NAME}"
export CI_COMMIT="${BUILD_SHA}"
export CI_BRANCH="${BUILD_BRANCH}"
export REVIEWDOG_GITHUB_API_TOKEN="${GITHUB_TOKEN}"
export REVIEWDOG_SKIP_DOGHOUSE=true

# Merge all SARIF reports luchta wrote (concatenate every file's `runs` array)
# into a single SARIF 2.1.0 report, then submit via reviewdog under one check name.
COMBINED_SARIF="${ARTIFACTS:-.}/lint.sarif"
if [ -d .luchta ] && find .luchta -name '*.sarif' -print0 \
     | xargs -0 jq -s '{version: "2.1.0", "$schema": "https://json.schemastore.org/sarif-2.1.0.json", runs: [(.[].runs // [])[] | select((.results // []) | length > 0)]}' \
    > "$COMBINED_SARIF" ; then
  reviewdog <"$COMBINED_SARIF" -f sarif -reporter=github-pr-check -name reviewdog/lint-check -filter-mode=nofilter || echo "Warning: failed to submit reviewdog report via github-pr-check"
  reviewdog <"$COMBINED_SARIF" -f sarif -reporter=github-pr-review -name reviewdog/lint-review -filter-mode=file || echo "Warning: failed to submit reviewdog report via github-pr-review"
else
  echo "Warning: failed to create aggregated SARIF report, skipping reviewdog submission"
fi
```

#### Key Parameters & Reporter Modes

- **Environment variables:**
  - `CI_PULL_REQUEST`, `CI_REPO_OWNER`, `CI_REPO_NAME`, `CI_COMMIT`, `CI_BRANCH`: Supply repository and pull request context to reviewdog.
  - `REVIEWDOG_GITHUB_API_TOKEN`: GitHub token with permission to post PR reviews and check runs (`pull-requests: write` / `checks: write`).
  - `REVIEWDOG_SKIP_DOGHOUSE=true`: Directs reviewdog to post directly to the GitHub API without contacting Doghouse servers.
- **SARIF aggregation:** Multiple workers each produce separate reports. The `jq` command merges all `.sarif` files under `.luchta/` into a single SARIF 2.1.0 document and excludes runs that contain no results.
- **Reporter modes:**
  - `-reporter=github-pr-check`: Submits all findings as a single GitHub Check Run (`-name reviewdog/lint-check`). Using `-filter-mode=nofilter` ensures findings outside changed lines are still reported in the check summary.
  - `-reporter=github-pr-review`: Posts inline pull request comments (`-name reviewdog/lint-review`). Using `-filter-mode=file` restricts inline comments to files modified in the pull request.

#### GitLab Support

reviewdog also supports GitLab Merge Requests. Reuse the same SARIF aggregation step (`$COMBINED_SARIF` above) and swap in a GitLab token and reporter. Pass `REVIEWDOG_GITLAB_API_TOKEN` (a Personal or Project Access Token) and use a GitLab reporter — `gitlab-mr-discussion` (inline MR comments) or `gitlab-mr-commit` (commit discussion) — alongside the standard GitLab CI environment variables (e.g. `CI_MERGE_REQUEST_IID` and `CI_PROJECT_PATH`), which GitLab CI sets automatically:

```bash
export REVIEWDOG_GITLAB_API_TOKEN="${GITLAB_TOKEN}"

reviewdog <"$COMBINED_SARIF" -f sarif -reporter=gitlab-mr-discussion -name reviewdog/lint-review -filter-mode=file || echo "Warning: failed to submit reviewdog report to GitLab"
```

### Explaining Task Execution (`why`)

To understand why a task ran in the past or why it would run/skip now, use the `luchta why` command. This is useful for debugging unexpected cache misses or confirming which files triggered a rebuild.

For each matched task, `luchta why` reports three facts:

1.  **Pruning:** Whether the task was excluded from the current run (e.g., filtered out via `--package` or not in the requested subgraph). Pruned tasks receive no further analysis.
2.  **Last Run:** Reports `last ran: {reason}` based on the `run_reason` persisted in the task's cache record. This explains why the task last actually executed (e.g., "input changed", "no prior run", "dependency output changed"). If no prior record exists or it was created before schema V4, it shows `not recorded`.
3.  **Current Decision:** Reports `now: {status}`—a live assessment of what would happen if you ran it now: `would run: {reason}` if it would execute, or `up to date (local cache hit)` / `up to date (shared cache hit)` if it would skip. This is computed fresh without executing the task.

#### Examples

- `luchta why build`: Explain the status of all `build` tasks.
- `luchta why -p app build`: Explain only the `@repo/app#build` task.
- `luchta why -p app build --show-inputs`: Show which specific input files changed compared to the last cached run.

#### `why` CLI Options

The `why` command mirrors the selection flags of `luchta logs`.

| Flag | Description |
|---|---|
| `tasks` (positional) | Task names to match; supports glob wildcards. |
| `-p, --package <PKG>` | Match package name globs (not paths). Repeatable. |
| `-T, --top-level` | Match tasks defined at the workspace root instead of package tasks. |
| `--show-inputs` | Show indented per-file detail for changed inputs. |
| `--show-outputs` | Show indented per-file detail for changed outputs. |

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

#### Worker `dependsOn` and `dependencies`
Workers can declare their own dependencies in the configuration.
`workers.<name>.dependsOn` uses the same syntax as task `dependsOn` (see below).
These dependencies are automatically appended (engine-side) to every task that
uses that worker.

Injected worker dependencies are:
- Deduped against existing task dependencies.
- Persistent even if the worker's `resolve` protocol message tries to modify
  task dependencies.
- Tolerant of pointing at pruned or missing tasks.

**Worker Overrides:** A worker's `Modify` decision (during the `resolve` protocol phase) may include `dependsOn` or `dependencies` (raw pattern strings) which **fully replaces** the task's static definition for that run. This mirrors how workers can override other task fields like `command` or `weight`. Omitting a field in the `Modify` decision leaves the static filter unchanged.

#### Worker progress

The engine negotiates transient worker-level progress by adding `"progress": true`
to a `run` request when a progress-aware execution sink is attached. Workers must
remain silent when the field is absent or false, which keeps newer bundled workers
compatible with older engines.

While a negotiated job is running, a worker may emit absolute snapshots:

```json
{"type":"progress","id":"pkg#task","completed":5,"skipped":1,"running":2,"pending":8}
```

- Every message replaces the previous snapshot; counters are never deltas.
- Counters are non-negative. Omitted counters deserialize as zero.
- `completed` counts all terminal items, including `skipped`; `skipped` is a
  displayed subset for intentionally bypassed inputs.
- An all-zero snapshot clears the task annotation.
- Progress is live display telemetry only. It is not cached, added to reports or
  task results, or used to determine the exit status.
- Progress responses are intermediate. Protocol proxies and watchers forward
  them without completing or removing the in-flight `run` request.

Periodic status lines retain their existing deterministic grouping while adding
snapshots to the individual members, for example:
`{auth(✔ 5 ⏩ 1 ⌛ 2 🏃 1),main(⌛ 2 🏃 1)}#test` or
`auth#{lint(✔ 50 ⌛ 100 🏃 16),test(⌛ 2)}`. Zero counters are omitted.

#### Worker reports

Workers can attach report files (e.g., test results or linting findings) to a task using the `report` message in the JSONL protocol:

```json
{"type":"report","id":"task-id","filename":"report.json","mimeType":"application/sarif+json","content":"..."}
```

- **content**: Must be UTF-8 text. The engine writes this verbatim to the task's cache directory (`.luchta/cache/<hash>/<filename>`) alongside `stdout.log` and `stderr.log`.
- **filename**: Must be a safe, plain basename. Filenames containing path separators (`/`, `\`), reserved names (`stdout.log`, `stderr.log`, `meta.bincode`), or relative path segments (`.`, `..`) are rejected with a warning.
- **mimeType**: Used by `luchta logs` to determine how to display the report. Natively supported MIME types: `application/sarif+json`, `application/vnd.ctrf+json`. Unknown MIMEs are shown verbatim. Dispatch is by MIME, not filename.
- **Duplicate filenames**: If multiple `report` messages use the same filename within one task, the last message wins.

Reports are recorded in the task metadata and can be viewed via `luchta logs`.

#### Standard Worker Binaries

Standard worker binaries are resolved via `PATH`. They ship inside each release archive alongside the `luchta` binary. Add the extraction directory to your `PATH` so Luchta can locate them.

- **luchta-tsc-worker** is a high-performance TypeScript/tsc worker built from an in-tree vendored and patched [typescript-go](https://github.com/microsoft/typescript-go).
- **luchta-ast-grep-worker** scans source files in-process using the custom rules in `sgconfig.yml`. Inline `ast-grep-ignore` comments have the same next-line, same-line, file-level, and rule-specific suppression semantics as the ast-grep CLI, and suppressed matches are also excluded from `--fix`.
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

#### oxc Workers
Luchta bundles three in-process workers built on the oxc toolchain (git-pinned to rev `415fe1e7`). All share the same limitations and upgrade cadence.

**Shared limitations:**
- Unix-only as resident workers: the engine only runs these as resident workers on Unix. Binaries ship on all platforms but Windows usage requires spawning per-task.
- Upgrade cadence: all `oxc_*` crates move together to one main rev. Bumping requires re-verifying APIs since oxc main churns.

---

- **luchta-oxlint-worker** lints JavaScript/TypeScript files using `oxc_linter` and emits a SARIF report. Configure it in your `luchta-config.*` script:

  ```typescript
  workers: {
    oxlint: {
      command: "luchta-oxlint-worker",
      env: { OXLINT_OPTS: "--fix" }   // optional
    }
  }
  ```

  **Options via `OXLINT_OPTS`:**
  - `--fix` — Autofix in place (same as oxlint CLI).
  - `--suppress-all` — Write `oxlint-suppressions.json` for all active violations.
  - `--prune-suppressions` — Remove stale suppression entries.
  - `--quiet` — Suppress stdout output.

  **Suppressions:** The worker reads/writes `oxlint-suppressions.json` in the task's working directory. The file format is byte-compatible with the oxlint CLI and IDE integrations.

  **SARIF report:** After linting, the worker emits `oxlint.sarif` (`application/sarif+json`). Retrieve it with:
  ```
  luchta logs --file oxlint.sarif
  ```

  **Config discovery:** Finds `.oxlintrc.json` or `.oxlintrc.jsonc` by walking ancestor directories from the task's `cwd`. JavaScript/TypeScript config (`oxlint.config.ts`) is not supported.

  **Type-aware linting:** Supported via the external `oxlint-tsgolint` binary.
  - **Enable:** Set `options.typeAware: true` (and `typeCheck`) in `.oxlintrc`, or use `OXLINT_OPTS="--type-aware --type-check"`.
  - **Prerequisite:** The `oxlint-tsgolint` binary must be installed (e.g. `npm i -D oxlint-tsgolint`). It is a user-installed runtime dependency, not shipped by Luchta.
  - **Graceful Fallback:** If the binary is missing when requested, the worker logs a warning and continues with regular non-type-aware linting.
  - Findings are merged into the same SARIF report and exit code.

---

- **luchta-oxc-transform-worker** transpiles TypeScript/JavaScript (babel replacement). It transforms `src/**` to `dist/<envName>/**/*.js` and reports outputs for caching.

  ```typescript
  workers: {
    "oxc-transform": {
      command: "luchta-oxc-transform-worker"
    }
  }
  ```

  **Environment resolution:** The output directory `dist/<envName>` is derived from the task id: `build:<env>` → `<env>`, else `js`.

  **Behavior:**
  - Transpiles `src/**` → `dist/<envName>/**/*.js`.
  - Reports all output files for cache tracking.
  - Removes stale outputs on re-run (files no longer produced are deleted).

  **Source maps:** Supported. The worker emits a `<name>.js.map` next to each transpiled `<name>.js` and appends a `//# sourceMappingURL=` comment. The `.map` files are included in the worker's reported outputs for cache tracking.

---

- **luchta-swc-transform-worker** transpiles TypeScript/JavaScript via SWC (babel/swc-cli replacement). It transforms `src/**` to `dist/js/**/*.js` and reports outputs for caching.

  ```typescript
  workers: {
    "swc-transform": {
      command: "luchta-swc-transform-worker"
    }
  }
  ```

  With no flags it honors `.swcrc` when present (crawls up from each source file, SWC-CLI parity), otherwise built-in defaults (TS + JSX strip, target es2022, sourcemaps on) and writes to `dist/js/`. For deterministic built-in `es2022` when you are not using a `.swcrc`, pass `--no-swcrc`. Coexists with `oxc-transform` as an alternative.

  **SWC-CLI-style config flags** (set per task via the `command` string) let you drive SWC programmatically instead of via `.swcrc` — useful for multi-env (browser/node) builds:

  ```typescript
  workers: {
    "swc-transform": { command: "luchta-swc-transform-worker" }
  }
  // then per build task, e.g.:
  //   command: "--no-swcrc --env-name node --out-dir dist/node --config-file swc.config.json
  //             -C jsc.transform.react.runtime=automatic -C module.type=commonjs"
  ```

  - `--no-swcrc` — disable `.swcrc` discovery (use flags/defaults only).
  - `--config-file <path>` — read a package-relative config file (`.swcrc` format); combine with `--no-swcrc`. Use `--config-file '#<path>'` for a workspace-root-relative shared config, tracked precisely as a cache input.
  - `-C, --config <key=value>` (repeatable) — set nested SWC config by dotted path, JSON-coerced. E.g. `-C jsc.transform.react.runtime=automatic`, `-C module.type=commonjs|es6`, `-C jsc.target=es2022`, `-C env.mode=entry -C env.coreJs=3.30`. Array-valued config (e.g. WASM plugins) and browserslist `env.targets` are better set via `--config-file`.
  - `-d, --out-dir <dir>` — output directory (relative to cwd), default `dist/js`. Use distinct per-task values (e.g. `dist/browser`, `dist/node`) for multi-env builds.
  - `--env-name <name>` — sets SWC's env name (CLI parity).
  - `--source-maps <true|false>` — external source-map mode; `inline` and `both` are treated as `true`.

  Note: `env` (preset-env) and `jsc.target` are mutually exclusive in SWC; when an `env` block is present the worker omits the default `jsc.target`.

  **Behavior:**
  - Transpiles `src/**` → `<out-dir>/**/*.js` (default `dist/js/`).
  - Reports all output files for cache tracking.
  - Removes stale outputs on re-run (files no longer produced are deleted).
  - Copies non-transformable assets (e.g., `.json`, `.css`) to output directory.

  **Source maps:** Supported. The worker emits a `<name>.js.map` next to each transpiled `<name>.js` and appends a `//# sourceMappingURL=` comment. The `.map` files are included in the worker's reported outputs for cache tracking.

---

- **luchta-oxfmt-worker** formats JavaScript/TypeScript files using oxc's formatter. By default, it formats in place.

  ```typescript
  workers: {
    oxfmt: {
      command: "luchta-oxfmt-worker",
      env: { OXFMT_OPTS: "--check" }   // optional
    }
  }
  ```

  **Options via `OXFMT_OPTS`:**
  - `--check` — Check mode: reports unformatted files and exits nonzero without writing. Without this flag, files are formatted in place.

  **Config discovery:** Finds `.oxfmtrc.json` or `.oxfmtrc.jsonc` by walking up from the task's `cwd`. If no config is found, it uses oxfmt defaults.
  - **Supported fields:** `useTabs`, `tabWidth`, `printWidth`, `endOfLine` (lf|crlf|cr), `singleQuote`, `jsxSingleQuote`, `semi`, `trailingComma` (all|es5|none), `bracketSpacing`, `bracketSameLine`.
  - **Other fields:** All other Prettier/oxfmt fields (overrides, ignore patterns, editorconfig, plugins, arrowParens, etc.) are currently ignored.

#### Wrapper & Filter Workers
Luchta provides a set of composable wrapper workers that can be chained using
`--` to add laziness or conditional pruning to any worker. Each wrapper spawns
the next stage in the chain as a child process and forwards the JSONL protocol.
Composition works from left to right; the rightmost stage is the real worker.
Pruning is silent. Wrapper stages preserve worker protocol multiplexing, so
independent resolve and run requests can remain in flight concurrently through
the entire chain.

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

- **Transitive Lockfile Detection (#89):** Cache hashing and watch-mode invalidation both track the **full transitive closure** of external package dependencies from `yarn.lock`. Any transitive dependency's resolved-version change now busts the cache, even when the direct specifier is unchanged. Lockfile cycles are handled silently. `gather_pkg_dep_pairs` serves as the single source of truth for both cache and watch.
- Default cache dir: `<workspace>/.luchta/cache`
- Override: `LUCHTA_CACHE_DIR=/abs/path`
- Disable: `LUCHTA_NO_CACHE=1` (or `--no-cache`)
- Glob inputs use the git-tracked file listing, so `.gitignore` is honored; literal (non-glob) inputs are hashed directly and are **not** filtered by `.gitignore` (an explicitly declared path is always honored). A pattern counts as a glob if it contains `*`, `?`, `[`, or `{` — see [Glob Syntax](#glob-syntax).
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
  cacheFiles: [".eslintcache"],
  env: {
    NODE_ENV: { value: "production" },
    CI_JOB_ID: { input: false }
  }
}
```

`cacheFiles` declares disposable, performance-only warm state such as ESLint
or Babel caches. It requires `cache: {}` and accepts package-relative
output-style globs and exclusions. Cache files must not overlap inputs or
outputs and cannot cross a package boundary.

Cache files never make a task up to date: they are absent from input, output,
and dependency-output hashes. After local and exact shared-output misses,
Luchta restores at most one recent cache-file state before running the task.
Any matching local cache file wins and suppresses the whole shared restore.
After a successful input-stable run, cache files are captured independently
from normal outputs; an empty result records a tombstone so older warm state is
not resurrected. `--no-cache` disables both restoration and publication.



### Cache Nonce (force-busting stale cache)

The `nonce` knob lets you force-bust stale cache entries. This is useful if a task's inputs were previously under-reported (poisoning the cache) or if you need to ensure a fresh run.

Nonces are available at four scopes and are **additive**:
- **Global:** `cache: { nonce: "..." }` on the top-level `LuchtaConfig`.
- **Worker:** `cache: { nonce: "..." }` on a worker definition. Affects all tasks using that worker.
- **Task:** `cache: { nonce: "..." }` on a task definition.
- **Environment variable:** `LUCHTA_CACHE_NONCE` — an independent global 4th nonce, read once per run. See also [`--no-cache`](#disable-cache).

#### Semantics
- **Combine:** All nonces combine; changing any single one invalidates the affected scope's cache. Empty/absent everywhere has no effect.
- **Stale Entries:** Setting a nonce does NOT delete old cache entries; it changes the hash so a fresh entry is written. The local cache keeps only the most recent entry per task, so reverting a nonce is a fresh cache miss (the task re-runs) rather than restoring the old result; the shared cache may still hold a matching prior candidate.
- **Recovery (GitHub #118):** If a worker under-reports a task's inputs (a worker bug), a cache entry can be "poisoned" with wrong outputs. Fixing the worker does NOT invalidate that entry, because the task spec hash does not include the worker's version/code. To recover, bump the relevant-scope `nonce` (e.g. change `nonce: "v1"` → `"v2"`), set `LUCHTA_CACHE_NONCE`, or use `--no-cache`.
- **Upgrade Note:** Upgrading to the version containing `luchta why` bumps the cache schema to V4. This triggers a one-time cache invalidation and full rebuild on the first run after upgrade, which is expected and harmless.

#### Inspection
Use `luchta logs --show-cache-nonce` to view the resolved nonce string persisted per task (shows `(none)` when no nonce is applied).

### Shared Build Cache

The shared build cache is a cross-worktree, cross-clone cache that restores task **outputs** and logs from prior builds. It can also provide advisory `cacheFiles` to tasks that still need to run. While the standard [Build Cache](#build-cache) is local to a single workspace, the shared cache allows developers and CI to reuse results across different checkouts of the same repository.

#### Concept
- **Computed Keys, Not Discovered:** Shard keys are `<YYYYMMDD>-<shard>`, derived from the UTC wall clock and a fixed shard count — never listed or walked. The previous design indexed by git commit hash, discovered by walking first-parent ancestry from `HEAD`. That never matched across pull requests, because CI builds run on feature branches and ephemeral merge commits that no other build shares (GitHub #277).
- **Input-Keyed Entries:** The cache key (`input_key`) folds in the task spec, environment, package-dependency versions, upstream task outputs, and the resolved content of the task's own inputs. Two branches that change a task's source differently land in distinct entries instead of racing for one shared slot — both stay cached and reusable, and reverting one back to the other's state is a hit, not a miss.
- **Content-Addressed Blobs:** Build outputs are compressed and stored in a deduped blob store, addressed by `outputs_hash`.
- **Read Window:** On cache lookup, Luchta concurrently fetches every shard from the last `LUCHTA_SHARED_CACHE_DAYS` UTC days (default 3) directly — `day_window * 6` key fetches, no object-store listing involved. Task restore preparation also runs concurrently. Only the chosen entry's fallback metadata and output blob are fetched; historical candidates are not downloaded eagerly.
- **Inline Restore Metadata:** Schema-v4 snapshots embed complete output-entry metadata when its compressed encoding is at most 16 KiB and add bounded cache-file observations. Schema-v3 and schema-v2 shards remain readable. Small no-output tasks restore with no per-entry remote request; larger records use `entries/<input_key>.bin` as a fallback.
- **Advisory Cache Files:** Cache-file candidates use a coarse task scope that excludes resolved input contents and upstream outputs. A candidate with matching upstream outputs wins even over a newer mismatch; otherwise newest wins. Only that one immutable blob is fetched, and a missing or corrupt blob runs cold without cascading remote requests.
- **Refresh on Hit:** A cache hit re-inserts its entry into today's shard, and, with remote sync on, re-pushes that shard, so a hot entry keeps getting a fresh day stamp instead of aging out of the read window on a fixed schedule. The re-insert and the push both happen in the end-of-run flush described next, not at hit time. Legacy schema-v2 entries remain readable through fallback metadata but are not refreshed because their recorded duration includes queueing time; they naturally age out.
- **Fail-Open Batched Writes:** Output blobs and oversized metadata are admitted to a nonblocking background worker pool. The index shard is gated behind every accepted artifact job that precedes it, then written once at the end of the cycle for all stores and refreshes. A saturated queue drops optional remote work rather than delaying task completion; a missing or partial remote artifact remains a safe cache miss. Normal draining has one total timeout budget. Interrupt and watch cancellation discard queued work immediately and force-stop rclone.
- **Remote Synchronization:** Opt-in synchronization with S3 or other object stores via `rclone`.

#### Layout
By default, the cache is stored at `~/.cache/luchta` (on Linux/macOS), under four prefixes:
- `blobs/<outputs_hash>.tar.zst` — Content-addressed compressed output archives.
- `cache-files/<state_hash>.tar.zst` — Content-addressed advisory cache-file archives.
- `snapshots/<YYYYMMDD>-<shard>/<shard_id>.bincode` — Schema-v4 metadata index shards, one directory per UTC day and shard number (`00`-`05`), holding append-only content-addressed files. Shards are zstd-compressed at rest; `<shard_id>` is the BLAKE3 hash of the uncompressed bincode bytes. Schema-v2/v3 shards remain readable, and shards from future schemas are preserved rather than consolidated or deleted.
- `entries/<input_key>.bin` — Fallback metadata objects only for entries whose compressed metadata exceeds 16 KiB, plus legacy schema-v2 entries. The key is the hex encoding of `input_key`, itself a BLAKE3 hash of the task spec, environment, package-dependency versions, upstream task outputs, and resolved task inputs.

The date baked into each `snapshots/` directory name makes lifecycle rules straightforward to write: target `snapshots/<date>-*` prefixes for a given cutoff directly, no need to inspect individual object ages. `blobs/`, `cache-files/`, and `entries/` have no date in their keys, so expire those by object age instead — matching `LUCHTA_SHARED_CACHE_GC_DAYS` keeps remote retention roughly in step with local GC.

**Shard count is fixed, not configurable.** Six shards per day (`SHARED_CACHE_SHARD_COUNT`) is a wire-compatibility constant: the read set is exactly `day_window * 6` keys, computed independently on every machine. A machine writing with a higher shard count would put entries in shard numbers a machine reading with a lower count never asks for, and that loss is silent — no error, just a quieter cache. Decreasing the shard count fleet-wide is safe (the old, now-unreachable high-numbered shards just age out via GC); increasing it is not, unless every machine changes at once. That asymmetry is why it isn't exposed as an env var.

**Day window is safely tunable per machine.** Unlike the shard count, `LUCHTA_SHARED_CACHE_DAYS` only changes how far back one machine looks; it can't desynchronize writers from readers. Raise it to widen the lookback for a slow-moving repo, or lower it to cut down on shard fetches per build.

#### One-time cache reset on upgrade

Both the shard key format and the entry key derivation changed in this design. `<YYYYMMDD>-<shard>` replaces the old `<commit>` (and, briefly, `<unix_ms>-<nonce>`) discovery scheme, and `entries/<input_key>.bin` is a prefix that didn't exist before. There is no dual-read path: nothing will ever ask for an old `snapshots/<commit>/` directory or an old-format `entries/` object again. The first build against a cache that predates this change misses every prior entry and rebuilds from scratch; results accumulate under the new keys from that point on. This is deliberate and one-time — acceptable because nothing had shipped yet on the old scheme. The stale objects aren't cleaned up proactively; they age out through the same GC as everything else (`LUCHTA_SHARED_CACHE_GC_DAYS` locally, your S3 lifecycle rules remotely).

#### Configuration (Environment Variables)
The shared cache is **OPT-IN** and is configured exclusively via environment variables:

- `LUCHTA_SHARED_CACHE` — Configuration mode:
    - `off` (default) — Disabled.
    - `local`, `1`, `true`, `on` — Local-only shared cache.
    - `rclone:<spec>` — Enable remote-sync via rclone, where `<spec>` is an rclone Fs base that points at a bucket and (recommended) a prefix, e.g. `rclone:my-s3:my-bucket/luchta-cache`.
- `LUCHTA_SHARED_CACHE_DIR` — Override the cache root directory.
- `LUCHTA_SHARED_CACHE_SYNC_TIMEOUT` — Maximum seconds for each normal remote operation and the total end-of-cycle upload drain. Default: `30`.
- `LUCHTA_SHARED_CACHE_GC_DAYS` — Retention period for local cache entries. Default: `14`.
- `LUCHTA_SHARED_CACHE_MAX_OUTPUT_MB` — Maximum size for a single task's normal output archive or advisory cache-file state; the two limits are applied independently. Default: `250`.
- `LUCHTA_SHARED_CACHE_DAYS` — Number of UTC days of shard history to read. Default: `3`. Deprecated alias `LUCHTA_SHARED_CACHE_HISTORY` (which counted commits, not days) is still read for one release; setting it prints a deprecation warning, and if both are set, `LUCHTA_SHARED_CACHE_DAYS` wins.

Invalid numeric values will trigger a warning and fall back to their defaults.

#### Shared cache tuning
- `LUCHTA_SHARED_CACHE_TIMEOUT_DISABLE_THRESHOLD` — Cumulative timeout threshold before disabling remote sync for the run. Successful requests do not erase earlier timeout evidence. Default: `8`.
- `LUCHTA_SHARED_CACHE_RCLONE_CONCURRENCY` — Maximum logical remote operations and background upload workers. Admission is held through complete rclone jobs, including status polling. Default: `16`.
- `LUCHTA_SHARED_CACHE_RCLONE_SUBMIT_TIMEOUT` — Bounded submit timeout for async rclone jobs. Default: `5s`.
- `LUCHTA_SHARED_CACHE_RCLONE_TRANSFERS` — rclone rcd `--transfers` setting. Default: `4`.
- `LUCHTA_SHARED_CACHE_RCLONE_CHECKERS` — rclone rcd `--checkers` setting. Default: `8`.
- `LUCHTA_SHARED_CACHE_RCLONE_JOB_EXPIRE_DURATION` — rclone rcd `--rc-job-expire-duration`; must exceed execution timeout so finished jobs are not reaped before polling completes. Default: `10m`.
- `LUCHTA_SHARED_CACHE_PUSH_QUEUE_CAPACITY` — Bounded background push queue depth. When full, producers continue immediately and optional remote cache work is dropped with one warning. Default: `256`.
- `LUCHTA_SHARED_CACHE_MIN_DURATION_MS` — Tasks whose measured execution is faster than this are not stored; semaphore wait and cache-decision time are excluded. Trusted schema-v3/v4 entries below the reader's threshold are also skipped. Default: `100`.
- `LUCHTA_SHARED_CACHE_STATS` — Set to `1` to print one concise aggregate diagnostics line at the end of each run or watch cycle. It includes snapshot, inline/fallback restore, blob, byte/latency, queue, upload, and disable-reason counters. Disabled by default.

#### Remote Synchronization (S3/rclone)
Luchta can synchronize the shared cache with a remote object store (like S3, GCS, or Azure) using [rclone](https://rclone.org/).

**Needs a recent rclone.** Luchta drives rclone through a persistent `rclone rcd` daemon listening on a unix socket (`--rc-addr unix://…`), which older builds don't support. Ubuntu 24.04's packaged 1.60.1 is known not to work — the daemon exits immediately on startup. Luchta is developed and tested against 1.74.3; if your distro package is older, install from [rclone.org/downloads](https://rclone.org/downloads/).

This fails safe: when the daemon won't start, Luchta records the error, disables remote sync for the run, and the build continues against the local cache. You get no remote sharing rather than a broken build, so it's worth checking `rclone version` if remote hits never materialize.

1. **Setup:** Run `rclone config` to create and name a remote (e.g., `my-s3`).
2. **Enable:** Set `LUCHTA_SHARED_CACHE=rclone:<remote-name>:<bucket>/<prefix>`.
   - Example: `rclone:my-s3:my-bucket/luchta-cache`.
   - Luchta appends `blobs/`, `cache-files/`, `snapshots/`, and `entries/` beneath this base,
     so a dedicated bucket or prefix is recommended.
   - For S3 (and other bucket-based backends) you **must** include the bucket
     name — pointing at the bare remote root (`rclone:my-s3`) is not a valid
     write target.
3. **Credentials:** Luchta does not handle credentials directly. It uses the `rclone` binary on your `PATH` and relies on your `rclone.conf` or `RCLONE_*` environment variables.

**Resilience & Performance:**
- **Build Safety:** Remote cache problems never fail a build. Health failures disable remote access immediately; isolated timeouts fail open and count toward the configured cumulative threshold. Warnings identify the failed phase, such as snapshot download, metadata fallback, blob restore, upload preflight, artifact upload, or final flush.
- **No CAS Required:** Snapshots are stored as append-only content-addressed shards, eliminating the need for complex "Compare-and-Swap" operations on the remote store.
- **Garbage Collection:** Remote GC is not managed by Luchta. Use S3 bucket lifecycle rules or similar object store features to expire old objects under all four prefixes — `blobs/`, `cache-files/`, `snapshots/`, *and* fallback-only `entries/`. Leaving any immutable-object prefix out lets it grow without bound.
  Set the `entries/` cutoff generously relative to the day window (`LUCHTA_SHARED_CACHE_DAYS`), or use last-access-based expiry if your object store supports it. A hit refreshes the current index shard, not a fallback object's timestamp. If fallback metadata expires while its index remains discoverable, the lookup safely misses and the task re-stores it.

#### Cacheability
A task is eligible for the shared cache if all the following are true:
- The task succeeded.
- Its post-semaphore execution took at least 100ms (or the configured minimum).
- Its total output size is within the `LUCHTA_SHARED_CACHE_MAX_OUTPUT_MB` limit.
- All its outputs are contained within its own package directory (outputs escaping the repository root are a hard error).

The working tree's git status plays no part in eligibility: uncommitted changes are simply reflected in the resolved input hash that makes up part of `input_key`, so a dirty and a clean build of the same task land in distinct, independently cacheable entries rather than one being excluded.

Advisory cache files apply the same success, minimum-duration, package-boundary,
and size checks independently. Normal output storage can succeed when cache
files exceed the limit, and cache-file storage can succeed when normal outputs
exceed it. An eligible empty cache-file set stores a tombstone rather than an
archive.

#### Maintenance
Luchta automatically performs throttled garbage collection of old local cache entries, snapshot shards, output blobs, and cache-file blobs (those older than `LUCHTA_SHARED_CACHE_GC_DAYS`). The cache is read-tolerant; if a blob or entry is missing due to GC or other reasons, it is treated as a cache miss.

#### Stats
Shared cache hits are shown in the build summary: `📥 <n>`. Set `LUCHTA_SHARED_CACHE_STATS=1` for the optional per-cycle diagnostics line; normal and summary output are unchanged otherwise.

### Build Lock

Luchta uses a repo-wide exclusive build lock to ensure only one build runs per repository at a time. This prevents concurrent builds from corrupting the local cache or interfering with each other's outputs.

- **Wait Behavior:** If a second `luchta` process starts while a build is already in progress, it logs `Waiting for concurrent build ...` to stderr and waits indefinitely. You can press `Ctrl+C` to cleanly abort the wait.
- **Watch Mode:** `luchta watch` only holds the lock during an active build pass. It releases the lock while idle (waiting for file changes), allowing other `luchta run` invocations to proceed immediately.
- **Lock File:** The lock is managed via a dedicated 0-byte file at `<cache-dir>/build.lock` (by default `.luchta/cache/build.lock` or `$LUCHTA_CACHE_DIR/build.lock`).
- **Resilience:** The lock is an OS-level advisory file lock. If the process crashes, the OS automatically releases the lock. The lock file itself is intentionally never deleted, as the lock guards the file's identity (inode), not its presence on disk.

## Roadmap

- **Phase 1 (Current):** Multi-crate workspace skeleton, CI, and release tooling (nextest, knope changesets, GitHub release workflows).
- **Phase 2:** Foundation libraries (workspace discovery, lockfile parsing, graph construction, weighted parallel execution).
- **Phase 3 (Current):** Opt-in build change-detection cache (blake3 hashing, local and shared) and cross-process build locking — see "Build cache", "Shared Build Cache", and "Build Lock" above.
