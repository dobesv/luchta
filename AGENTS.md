# Agent Guidelines for Luchta

This document provides conventions and guidelines for AI coding agents working on the Luchta project.

## Project Tracking
- This project uses **GitHub Issues** as its primary tracker.

## Project Structure
Luchta is a Cargo workspace with the following crate layout:
- `crates/luchta-types`: Core data structures and types.
- `crates/luchta-lockfiles`: Lockfile parsing and abstraction.
- `crates/luchta-workspace`: Workspace and package discovery.
- `crates/luchta-engine`: Graph logic and execution engine.
- `crates/luchta-cli`: CLI interface and configuration.
- `crates/luchta-cache`: Filesystem-backed build cache, hashing, and skip logic (`thiserror`, filesystem records, no embedded DB).
- `crates/luchta-yarn-env`: Computes the environment Yarn Berry injects when running a script (bin shims, `NODE_OPTIONS`, `npm_*`) from the PnP manifest via the `pnp` crate; `luchta-yarn-worker` uses it for direct execution. Its `test-fixture` feature exposes `PnpFixture`, a synthetic PnP project builder for other crates' tests.
- `xtask`: Project automation crate (standard Rust `xtask` pattern), run via the `cargo xtask` alias.
  `xtask/rustqual` pins the rustqual version `cargo xtask quality` builds; it is
  its own workspace, not a member of this one. `rustqual.toml` holds its settings.

## Key Architectural Decisions
Agents must respect these fundamental design choices:
- **Runtime:** Uses `tokio` for async I/O-bound process spawning. **Rayon is explicitly excluded** from Luchta's own code. Dependencies may run their own rayon pools internally (tsrs's type checkers do); that's fine, don't "fix" it.
- **Concurrency Model:** Implements **weight-based concurrency** using `tokio::sync::Semaphore::acquire_many_owned(weight)`.
- **Graph Logic:** Uses `petgraph::DiGraph` and `petgraph::algo::toposort` for cycle detection.
- **Dual-Graph Separation:** Maintains separate **Package Graph** (package topology) and **Task Graph** (task execution units).
- **Lockfile Abstraction:** All lockfile interactions must go through the `Lockfile` trait.
- **Error Handling:**
    - Use `thiserror` for library crates (`luchta-types`, `luchta-lockfiles`, `luchta-workspace`, `luchta-engine`).
    - Use `miette` only in the `luchta-cli` for user-facing diagnostics.
- **Configuration:** Primary configuration is an executable `luchta-config.*` script at the workspace root that prints a JSON configuration object to `stdout`.

## Validation Commands
You MUST run the full verification pipeline before committing. Do not skip any
step or you WILL miss problems.

```bash
cargo build --workspace                                  # Compile the whole workspace
cargo fmt --all                                          # Auto-format (rustup uses rust-toolchain.toml — matches CI)
cargo clippy --workspace --all-targets -- -D warnings    # Lint — treat warnings as errors
cargo nextest run --workspace                            # Run all tests via nextest
cargo nextest run --workspace --stress-count=5           # Repeat 5x to catch flaky tests
cargo xtask quality                                      # Fail on rustqual findings the change adds or makes worse
cargo xtask install                                      # Install all workspace binary crates locally
cargo xtask build-worker --target <triple>                  # Build the Go-based TypeScript worker
```

**Don't add rustqual findings or make existing ones worse.** A failing
`cargo xtask quality` is a blocker: fix the flagged code, don't merge around it.

**Do not ignore clippy warnings.** Treat every warning as an error; CI runs
`cargo clippy -- -D warnings`.

If `cargo nextest` is not installed: `cargo install cargo-nextest --locked`
(or `cargo binstall cargo-nextest`). The `--stress-count=5` run repeats the
suite five times — flaky tests that pass once but fail intermittently surface
here.

The process-spawning end-to-end tests (`luchta-worker-watcher::e2e`,
`luchta-cli`'s `worker_integration` and `driver_e2e_tests`) spawn real
processes and wait on filesystem events against a roughly 10s deadline. Under
full CPU-count concurrency that contention can push one past the deadline and
fail as a bare timeout. They run in the throttled `watch-e2e` test group
(`max-threads = 4`) defined in `.config/nextest.toml`, so the rest of the
suite keeps full parallelism. If you hit a bare timeout there, re-run the test
in isolation (it finishes in well under a second): a clean isolated pass points
to load-dependent flakiness (contention, or a concurrency/timing bug that only
surfaces under parallelism) rather than logic that's broken every run, so
reproduce under full-workspace stress before blaming — or clearing — a code
change. Raising `fs.inotify.max_user_instances` above the common default of 128
is a reasonable dev-machine tweak but is not the cause of these timeouts.

**`cargo nextest run --workspace` is the canonical test command.** Do not use
plain `cargo test` — it runs the whole suite in one process with shared
threads, so tests that mutate process-global state (cwd, real environment
variables, temp dirs coupled to cwd) race and fail nondeterministically.
Nextest runs each test in its own process, isolating that state. Such tests
call `require_nextest()` (from the `luchta-test-support` crate) as their first
line; when run under `cargo test` they panic with guidance instead of failing
spuriously. When adding a test that mutates process-global state, call
`require_nextest()` first and add `luchta-test-support` as a `[dev-dependency]`.

Tests that need a real Yarn PnP project (anything exercising `luchta-yarn-worker`
without `--no-direct`) build one with `luchta_yarn_env::test_fixture::PnpFixture`
(dev-dependency on `luchta-yarn-env` with the `test-fixture` feature) instead of
hand-writing `package.json` files; `FixtureOptions` switches between inline and
split manifests and toggles the ESM loader. `luchta run` prints a task's stdout
only when the task fails, so to assert what a script printed, give the task
`cache: {}` and read it back with `luchta logs` (see
`crates/luchta-cli/tests/yarn_direct_e2e.rs`).

Nextest runs every test through the repository's hermetic environment wrapper
in `.config/nextest.toml`. Host environment variables are removed unless they
are runtime essentials, Cargo/nextest metadata, rustup's toolchain selection
(`RUSTUP_TOOLCHAIN`, `RUSTUP_HOME`), Cargo's cache location (`CARGO_HOME`),
dynamic-loader or coverage settings, or the explicit `LUCHTA_TEST_RCLONE` opt-in.
Keeping `CARGO_HOME` ensures offline metadata uses the outer build's cache.
The rustup variables matter because some CLI tests build worker binaries with
`escargot`: without them the `rustc`
proxy obeys a `rust-toolchain.toml` shipped inside a dependency's registry
source (the `pnp` crate pins an older release) and the build fails on CI. When a test suite needs
a new ambient customization, add its exact variable to both wrapper scripts'
allowlists and cover that behavior with a test; do not allow all `LUCHTA_*`
variables through.

Integration tests must not look up binaries at runtime by a hyphenated name.
The Unix wrapper runs under `/bin/sh`, which is dash on Ubuntu, and dash drops
any environment variable whose name contains `-`, so a runtime
`CARGO_BIN_EXE_luchta-bash-worker` never reaches the test. assert_cmd's runtime
`cargo_bin()` then falls back to searching beside the test executable, and
since Rust 1.100 changed Cargo's build-dir layout that search fails too. For a
binary in the test's own package, use the compile-time
`env!("CARGO_BIN_EXE_<name>")`; for another package's binary, build it with
`escargot` (see `yarn_worker_bin()` in `crates/luchta-cli/tests/common/mod.rs`).
To reproduce the CI layout on an older toolchain, set
`CARGO_BUILD_BUILD_DIR` to a directory outside the target dir.

### Code quality gate

`cargo xtask quality` runs [rustqual](https://github.com/SaschaOnTour/rustqual)
on the working tree, uncommitted and untracked files included, and on its merge
base with `origin/HEAD` (or `--base <rev>`), and fails on every finding the
change added or made worse. The workspace's existing findings (about 1,650
when the gate was added) are not a to-do list, and the gate ignores them.
Findings are matched by file, category and function rather than by line, and
follow a renamed file. "Worse" means a bigger number on a finding both sides
have: a longer function, higher complexity, deeper nesting, more parameters or
more `unwrap`s. A file already over the length limit may grow; only crossing
the limit counts. Apart from
fetching rustqual's crates for its first build, it needs no network access and
no license, so it runs inside the sandbox.

rustqual is pinned in `xtask/rustqual/Cargo.toml` and its lockfile, so nothing
needs installing; the first run builds it, which takes about a minute. Its
settings live in `rustqual.toml`, and both sides of the comparison use the
working tree's copy, so changing a setting never fails the gate by itself.
`cargo xtask rustqual <path> --verbose` runs the same binary with the same
settings to show what is behind a finding, and `--suggestions` adds refactoring
hints for logic-plus-calls (`iosp`) violations.

rustqual's IOSP rule wants each function either to contain logic (`if`,
`match`, loops, arithmetic) or to call other functions of this workspace, not
both. It resolves calls by name, so a method call such as `.len()` or `.find()`
counts when any function in the workspace shares that name. Closures and
iterator chains don't count as logic, so a loop that calls helpers can usually
become an iterator chain, or the logic can move into a helper of its own.

Fix what the gate reports. When a finding is deliberate, suppress it with a
comment on the line above the item: `// qual:allow(iosp) — <reason>` for a
logic-plus-calls violation, and `// qual:allow(<dimension>, <target>) reason:
"<reason>"` for anything else, where the target names the kind of finding and a
metric target pins its ceiling (`max_function_lines=80`).
`cargo xtask rustqual --explain allow` lists the targets. `// qual:api` and
`// qual:test_helper` mark a function used only from outside its crate or only
from tests, which rustqual otherwise reports as dead code. Trait impls aren't
counted against the parameter limit, so an upstream trait's wide signature
(`tsrs_module::Resolver`) needs no suppression.

rustqual treats only an exact `#[cfg(test)]` as test code. A module gated with
`#[cfg(all(test, unix))]` or `#[cfg(all(test, feature = "oxc"))]` is analysed
as production code, so its helpers' `unwrap`s, magic numbers and test-only
calls are reported. Write that gate as two attributes, `#[cfg(test)]` and
`#[cfg(unix)]`, which compile the same.

The `Code quality` CI job runs the gate on pull requests and annotates the diff
with what it finds. It is advisory: `.mergify.yml` doesn't require it.

## Conventions
- **No `target/`:** Never commit `target/` directories.
- **Error Types:** Library errors should be clear and descriptive using `thiserror`.
- **Async Traits:** Use native stable `async fn` in traits where possible (refer to `luchta-engine` for specific patterns).
- **Worker reports**: Workers can attach reports via the `report` JSONL message. These are stored verbatim in the task cache and can be retrieved raw via `luchta logs --file <NAME>`. Native pretty-printing in `luchta logs` supports `application/sarif+json` and `application/vnd.ctrf+json` (dispatch by MIME type).
- **Workspace walks must tolerate vanished entries:** Directory walks over user workspaces/package trees (`ignore` in the workers, `walkdir` in `luchta-cache`) skip `NotFound` errors for entries below the walk root (depth > 0) and still fail on a missing root or any other error. Transient dirs such as shared-cache restore staging, plus dangling symlinks under `follow_links(true)`, vanish mid-walk and must not abort the walk (#950). Follow the `is_skippable_walk_error` / `is_skippable_walkdir_error` helper pattern in the existing walkers and add a dangling-symlink test.

## Changeset Files

When making a user-visible change, add a changeset file under `.changeset/`. The
YAML front matter specifies the version bump level, which Knope maps to
`CHANGELOG.md` sections:
- `patch` → **Fixes**
- `minor` → **Features**
- `major` → **Breaking Changes**

The key **must** be `luchta` — the whole workspace shares one version
(`version.workspace = true`), so individual crate names are not valid keys and
will cause `knope release` to error.

### Examples

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
> Use a single `#` for headers. Knope re-levels them automatically. Do **not**
> use `####` as it produces mis-leveled output.

The filename should be a short kebab-case slug, e.g.
`.changeset/add-blake3-caching.md`.

## Releasing

Releases are cut by [knope](https://knope.tech/) (config in `knope.toml`),
driven entirely from changeset files. The flow:

1. Land changes on `main`, each with a changeset describing the bump.
2. Trigger the **Prepare Release** GitHub Action (Actions -> Prepare Release ->
   Run workflow), or run `knope release` locally. This bumps the version in
   `Cargo.toml`, aggregates changesets into `CHANGELOG.md`, refreshes
   `Cargo.lock`, commits, and pushes a `luchta/v<version>` tag.
3. The tag push triggers the **Release** workflow
   (`.github/workflows/release.yaml`), which cross-builds platform binaries and
   uploads them to the GitHub Release.

To build a release on demand without cutting a version, run the **Release**
workflow manually (`workflow_dispatch`) — it builds the binaries and uploads
them as workflow artifacts instead of publishing a release.

## Dependency Updates & Auto-merge

- **Renovate** (`renovate.json`) opens PRs for dependency and GitHub Actions
  updates but never merges them; its own `automerge` stays off. With it on,
  Renovate merged #1017 two minutes after rebasing it, before CI had started on
  the new commit, after Mergify had already dequeued that PR for a failing macOS
  check. Merging is left to Mergify, which waits for the CI checks.
- OXC Git dependencies and the allocator patch must share one revision. The
  `oxc toolchain` Renovate group includes a custom regex manager because the
  Cargo manager skips `[patch.crates-io]`; keep its matcher aligned with the
  allocator entry in `Cargo.toml`.
- The tsrs crates follow the same pin discipline with one extra moving part:
  all tsrs deps share one upstream git rev, and `tsrs_core` is redirected to
  the dobesv/tsrs fork through a `[patch]` entry in the root `Cargo.toml`.
  Renovate groups tsrs bumps, but neither Renovate nor Mergify merges them
  automatically. Before merging, rebase the fork onto the new upstream rev and
  update the `[patch]` rev. The pin guard doesn't check that the fork is based
  on the upstream rev. The `xtask` test `tsrs_git_dependencies_share_one_revision`
  checks both rev sets.
- Renovate also bumps the rustqual pin in `xtask/rustqual/Cargo.toml`, and
  Mergify merges that PR like any other. The `xtask` test
  `a_revision_compared_with_itself_passes` builds the pinned rustqual and
  parses its report, so a release that breaks either fails `Test`. A release
  that only adds rules makes the next `cargo xtask quality` report nothing new,
  because both sides of the comparison run the same binary.
- **Mergify** (`.mergify.yml`) runs a squash merge queue named `main`. It
  auto-queues every Renovate PR (any update type, majors included), except
  `renovate/tsrs-toolchain`, and any PR labeled `automerge`. A batch only
  merges once every CI check is green. The `Check` job runs as a per-OS matrix
  (ubuntu/windows/macos, and also runs clippy), producing `Check (<os>)`
  check-runs; alongside `Test` and `Format` (the job names in
  `.github/workflows/ci.yml`). If you rename or add a CI job, update the
  `merge_conditions` in `.mergify.yml` to match.
