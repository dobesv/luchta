# Repo-root-relative diagnostic paths (#223)

## Problem

The ast-grep, oxlint, and oxfmt workers report file paths in their diagnostics
(SARIF `artifactLocation.uri` and plain-text finding lines) **relative to the
per-task package directory**, not the repository root. When luchta aggregates
every package's output at the repo level — under only a `pkg#task` banner
(`format.rs`) and printing `uri:line:col` verbatim — those package-relative paths
are ambiguous (two packages can both have `src/foo.ts`) and are not clickable
from the repo root where luchta runs. They also misplace locations when uploaded
to GitHub code scanning, which resolves URIs against the repo root.

This is inconsistent with the TypeScript reference implementation
(`app-luchta/packages/platform/luchta`), whose `eslintWorker` and `depcheckWorker`
deliberately emit **repo-root-relative** SARIF URIs via `findRepoRoot` +
`relativizeSarifUris`.

### Verified mechanism (why paths are package-relative today)

- `WorkerRequest.cwd` is set to the absolute **package directory**
  (`task_graph.rs:29` — `package.path`). Root-package tasks get cwd = workspace
  root; sub-package tasks get cwd = the package dir.
- All three workers relativize diagnostics against `req.cwd`:
  - ast-grep: `file.strip_prefix(self.cwd)` (`lint.rs:40`).
  - oxlint: oxc renders `Info.filename` relative to `LintServiceOptions::new(cwd)`
    (`lint.rs:86,151`).
  - oxfmt: `relative_display(cwd, path)` = `strip_prefix(cwd)` (`format.rs:92`);
    error diagnostics use the **absolute** path (`format.rs:88`).
- `req.cwd` must stay the package dir: workers walk *up* from it to find configs
  (`.oxlintrc`, `sgconfig`) and walk *down* from it to collect source files.
  Repurposing it to the repo root would lint the entire repo per task.
- The worker **process's** OS working directory is not set by luchta
  (`worker_command`, `io_tasks.rs:391` — no `current_dir`), so it inherits
  luchta's invocation directory. `resolve_workspace_root` (`run.rs:111`) returns
  the `--workspace-root` flag or the invocation cwd; luchta never chdirs and never
  walks up. So the worker process cwd equals the repo root only incidentally.
- The authoritative repo root is `workspace_root`, which workers never receive.

## Goal

All worker-emitted **diagnostic** paths (SARIF URIs and plain-text finding lines)
are repo-root-relative with forward slashes, matching the TS reference and making
them clickable from the repo root and correct for CI uploads. Protocol
`inputs`/`outputs` remain package-relative (correct for the task graph, matches
the reference's `relativizeOutputs`).

## Design

### 1. Engine — pin the worker process cwd to the workspace root

Make the "workers run at the repo root" assumption true by construction.

- Add `workspace_root: PathBuf` to `WorkerManager` via a builder method
  (`with_workspace_root`), populated in `prepare_workspace` (`run.rs`) where it is
  already in hand.
- Thread it through the spawn path: `SpawnAttempt` → `worker_command`
  (`io_tasks.rs:391`), calling `.current_dir(&workspace_root)` on the
  `tokio::process::Command`.
- Applies to resident worker processes used by both the resolve and run phases.

This is safe: the oxc workers operate entirely on absolute paths derived from
`req.cwd` (`ensure_within_root` canonicalizes absolute paths; oxfmt config
resolution uses absolute paths from config discovery), and generic shell-command
tasks set their child `current_dir(req.cwd)` explicitly (`spawn_child`), so they
are unaffected by the parent worker's cwd.

### 2. luchta-worker — shared helpers

All three worker crates already depend on `luchta-worker` and on
`serde`/`serde_json`, so shared code lives there.

- `repo_relative(path: &Path, root: &Path) -> String`: strip the `root` prefix and
  normalize to forward slashes. If `path` is not under `root`, fall back to the
  absolute normalized path (still clickable). Never drops the location.
- Move the duplicated `SarifLog` / `build_sarif` structs out of
  `luchta-ast-grep-worker/src/sarif.rs` and `luchta-oxlint-worker/src/sarif.rs`
  into a single `luchta-worker` module parameterized by tool driver name
  (`"oxlint"`, `"ast-grep"`). oxfmt uses only `repo_relative` (it emits no SARIF).

### 3. Workers — relativize output against the repo root

Each worker resolves the repo root once per run via `std::env::current_dir()`
(now guaranteed to equal the workspace root) and uses it for **output paths
only**; config discovery and file collection continue to use `req.cwd`.

- **ast-grep**: `ScanContext::relative_uri` strips the repo root instead of
  `self.cwd` (`lint.rs:40`). The separate `selection_path` (strips `config_dir`,
  used for rule selection, not output) is unchanged.
- **oxlint**: post-process oxc's `Info.filename` in `wrap_error` (`lint.rs:151`):
  `repo_relative(&req.cwd.join(&info.filename), &root)`. `LintServiceOptions` cwd
  stays `req.cwd` so oxc's config/ignore resolution is unchanged.
- **oxfmt**: the `would reformat:` / `reformatted:` lines (`worker.rs:216,228`)
  use the repo root as the base; `format_diagnostic` (`format.rs:88`) switches
  from the absolute path to a repo-root-relative path.
- Protocol `inputs`/`outputs` and `declared_inputs` (`main.rs:296`) remain
  package-relative — unchanged.

### 4. Error handling

A path outside the repo root falls back to the absolute normalized path (rare;
e.g. symlinked checkouts or files outside the workspace). Locations are never
silently dropped.

## Testing

- Unit tests for `repo_relative`: file inside root, nested package path, path
  outside root (absolute fallback), backslash normalization.
- Engine test: a spawned worker process observes `current_dir == workspace_root`.
- Update existing worker tests that assert package-relative URIs (e.g.
  `src/foo.ts`) to expect repo-root-relative paths (e.g. `packages/x/src/foo.ts`)
  when the task cwd is a sub-package.
- CLI fixture test: a sub-package SARIF finding renders as
  `packages/x/...:line:col` through `format_sarif_pretty`.

## Non-goals

- No change to protocol `inputs`/`outputs` semantics.
- No new `WorkerRequest` protocol field (the engine sets the process cwd instead).
- No changes to non-diagnostic workers (yarn, transform, bash, etc.).
