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
- `xtask`: Project automation crate (standard Rust `xtask` pattern), run via the `cargo xtask` alias.

## Key Architectural Decisions
Agents must respect these fundamental design choices:
- **Runtime:** Uses `tokio` for async I/O-bound process spawning. **Rayon is explicitly excluded.**
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
cs delta $(git merge-base HEAD origin/main)              # CodeScene quality analysis of branch changes
cargo xtask install                                      # Install all workspace binary crates locally
```

**CodeScene must be all green.** `cs delta` must report no new code-health
problems (no degrading functions, no new code smells) before the work is
considered done. A red or degrading CodeScene result is a blocker — fix the
flagged code, do not merge around it.

**Do not ignore clippy warnings.** Treat every warning as an error; CI runs
`cargo clippy -- -D warnings`.

If `cargo nextest` is not installed: `cargo install cargo-nextest --locked`
(or `cargo binstall cargo-nextest`). The `--stress-count=5` run repeats the
suite five times — flaky tests that pass once but fail intermittently surface
here.

## Conventions
- **No `target/`:** Never commit `target/` directories.
- **Error Types:** Library errors should be clear and descriptive using `thiserror`.
- **Async Traits:** Use native stable `async fn` in traits where possible (refer to `luchta-engine` for specific patterns).

## Changeset Files

When making a user-visible change, add a changeset file under `.changeset/`:

```markdown
---
luchta: minor
---
Brief description of the change.
```

The YAML front matter specifies the version bump: `patch`, `minor`, or `major`.
The key on the left **must** be `luchta` — the whole workspace shares one
version (`version.workspace = true`), so individual crate names are not valid
keys and will cause `knope release` to error.

The filename should be a short kebab-case slug of the change, e.g.
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
  updates. Minor/patch/digest/pin updates (including non-major Actions bumps)
  are flagged for automerge; major updates require manual review.
- **Mergify** (`.mergify.yml`) runs a squash merge queue named `main`. It
  auto-queues passing Renovate PRs and any PR labeled `automerge`. A batch only
  merges once every CI check is green: `Check`, `Test`, `Clippy`, `Format`
  (the job names in `.github/workflows/ci.yml`). If you rename or add a CI job,
  update the `merge_conditions` in `.mergify.yml` to match.
