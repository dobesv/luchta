# Repo-root-relative diagnostic paths Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the ast-grep, oxlint, and oxfmt workers report diagnostic file paths (SARIF URIs and plain-text finding lines) relative to the repository root, so they are clickable from the repo root and correct for CI uploads.

**Architecture:** The engine pins every resident worker process's OS working directory to the workspace root at spawn. Each worker reads that root once via `std::env::current_dir()` in its `main`, threads it as an explicit `repo_root` parameter into its scan/lint/format code, and relativizes output paths against it — while continuing to use the per-task `req.cwd` (package dir) for config discovery and file collection. A shared `luchta-worker` module provides the path helper and a unified SARIF builder.

**Tech Stack:** Rust, tokio, serde/serde_json, oxc crates, ast-grep crates.

## Global Constraints

- Diagnostic output paths (SARIF `artifactLocation.uri`, plain-text finding lines, oxfmt reformat/error lines) MUST be repo-root-relative with forward slashes.
- A path not under the repo root falls back to its absolute normalized path — locations are never dropped.
- Protocol `inputs`/`outputs` and `declared_inputs` remain package-relative — do NOT change them.
- `req.cwd` remains the base for config discovery and file collection — do NOT repurpose it.
- No new `WorkerRequest` protocol field.
- `luchta-oxlint-worker` and `luchta-oxfmt-worker` build with the default `oxc` feature; run their tests with plain `cargo test -p <crate>` (oxc is on by default). `luchta-ast-grep-worker` has no feature gate.

---

### Task 1: Shared `repo_relative` path helper in luchta-worker

**Files:**
- Create: `crates/luchta-worker/src/paths.rs`
- Modify: `crates/luchta-worker/src/lib.rs:1` (add `pub mod paths;`)

**Interfaces:**
- Produces:
  - `pub fn repo_relative(path: &std::path::Path, root: &std::path::Path) -> String`
  - `pub fn normalize_forward_slashes(path: &std::path::Path) -> String`

- [ ] **Step 1: Write the failing tests**

Create `crates/luchta-worker/src/paths.rs`:

```rust
use std::path::Path;

/// Render `path` relative to `root` with forward slashes. If `path` is not under
/// `root`, fall back to the (normalized) full path so the location stays usable.
pub fn repo_relative(path: &Path, root: &Path) -> String {
    normalize_forward_slashes(path.strip_prefix(root).unwrap_or(path))
}

/// Normalize path separators to `/` for stable, portable output.
pub fn normalize_forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{normalize_forward_slashes, repo_relative};

    #[test]
    fn repo_relative_strips_root_prefix() {
        assert_eq!(
            repo_relative(Path::new("/repo/packages/app/src/foo.ts"), Path::new("/repo")),
            "packages/app/src/foo.ts"
        );
    }

    #[test]
    fn repo_relative_handles_root_equal_to_parent() {
        assert_eq!(
            repo_relative(Path::new("/repo/src/foo.ts"), Path::new("/repo")),
            "src/foo.ts"
        );
    }

    #[test]
    fn repo_relative_falls_back_to_full_path_when_outside_root() {
        assert_eq!(
            repo_relative(Path::new("/other/src/foo.ts"), Path::new("/repo")),
            "/other/src/foo.ts"
        );
    }

    #[test]
    fn normalize_forward_slashes_replaces_backslashes() {
        assert_eq!(
            normalize_forward_slashes(Path::new("packages\\app\\src\\foo.ts")),
            "packages/app/src/foo.ts"
        );
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/luchta-worker/src/lib.rs`, add after line 1 (`pub mod parallel;`):

```rust
pub mod paths;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p luchta-worker paths::`
Expected: PASS (4 tests)

- [ ] **Step 4: Commit**

```bash
git add crates/luchta-worker/src/paths.rs crates/luchta-worker/src/lib.rs
git commit -m "feat(worker): add repo_relative path helper"
```

---

### Task 2: Shared SARIF builder in luchta-worker

**Files:**
- Create: `crates/luchta-worker/src/sarif.rs`
- Modify: `crates/luchta-worker/src/lib.rs` (add `pub mod sarif;`)

**Interfaces:**
- Produces:
  - `pub enum SarifLevel { Error, Warning, Note, None }`
  - `pub struct SarifFinding { pub rule_id: String, pub level: SarifLevel, pub message: String, pub uri: String, pub start_line: usize, pub start_column: usize, pub end_line: Option<usize>, pub end_column: Option<usize> }`
  - `pub fn build_sarif(tool_name: &str, findings: &[SarifFinding]) -> Result<String, String>`
- Note: `end_line`/`end_column` are `Option` and omitted from JSON when `None`, so oxlint (no end positions) and ast-grep (with end positions) both keep their current output shape.

- [ ] **Step 1: Write the failing tests**

Create `crates/luchta-worker/src/sarif.rs`:

```rust
use serde::Serialize;

const SARIF_VERSION: &str = "2.1.0";
const SARIF_SCHEMA: &str = "https://json.schemastore.org/sarif-2.1.0.json";

/// SARIF result severity levels used by luchta's diagnostic workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SarifLevel {
    Error,
    Warning,
    Note,
    None,
}

impl SarifLevel {
    fn as_str(self) -> &'static str {
        match self {
            SarifLevel::Error => "error",
            SarifLevel::Warning => "warning",
            SarifLevel::Note => "note",
            SarifLevel::None => "none",
        }
    }
}

/// One diagnostic to render as a SARIF result. `end_line`/`end_column` are
/// optional; when `None` they are omitted from the region object.
#[derive(Debug, Clone)]
pub struct SarifFinding {
    pub rule_id: String,
    pub level: SarifLevel,
    pub message: String,
    pub uri: String,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: Option<usize>,
    pub end_column: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifLog<'a> {
    version: &'a str,
    #[serde(rename = "$schema")]
    schema: &'a str,
    runs: Vec<SarifRun<'a>>,
}

#[derive(Serialize)]
struct SarifRun<'a> {
    tool: SarifTool<'a>,
    results: Vec<SarifResult>,
}

#[derive(Serialize)]
struct SarifTool<'a> {
    driver: SarifDriver<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifDriver<'a> {
    name: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifResult {
    rule_id: String,
    level: &'static str,
    message: SarifMessage,
    locations: Vec<SarifLocation>,
}

#[derive(Serialize)]
struct SarifMessage {
    text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifLocation {
    physical_location: SarifPhysicalLocation,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifPhysicalLocation {
    artifact_location: SarifArtifactLocation,
    region: SarifRegion,
}

#[derive(Serialize)]
struct SarifArtifactLocation {
    uri: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifRegion {
    start_line: usize,
    start_column: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_column: Option<usize>,
}

/// Serialize `findings` as a SARIF 2.1.0 log with a single run whose driver
/// name is `tool_name`.
pub fn build_sarif(tool_name: &str, findings: &[SarifFinding]) -> Result<String, String> {
    let results = findings
        .iter()
        .map(|finding| SarifResult {
            rule_id: finding.rule_id.clone(),
            level: finding.level.as_str(),
            message: SarifMessage {
                text: finding.message.clone(),
            },
            locations: vec![SarifLocation {
                physical_location: SarifPhysicalLocation {
                    artifact_location: SarifArtifactLocation {
                        uri: finding.uri.clone(),
                    },
                    region: SarifRegion {
                        start_line: finding.start_line,
                        start_column: finding.start_column,
                        end_line: finding.end_line,
                        end_column: finding.end_column,
                    },
                },
            }],
        })
        .collect();

    serde_json::to_string_pretty(&SarifLog {
        version: SARIF_VERSION,
        schema: SARIF_SCHEMA,
        runs: vec![SarifRun {
            tool: SarifTool {
                driver: SarifDriver { name: tool_name },
            },
            results,
        }],
    })
    .map_err(|error| format!("failed to serialize SARIF: {error}"))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{build_sarif, SarifFinding, SarifLevel};

    fn finding(end: Option<(usize, usize)>) -> SarifFinding {
        SarifFinding {
            rule_id: "no-console".to_owned(),
            level: SarifLevel::Error,
            message: "no console".to_owned(),
            uri: "packages/app/src/index.ts".to_owned(),
            start_line: 1,
            start_column: 1,
            end_line: end.map(|(line, _)| line),
            end_column: end.map(|(_, col)| col),
        }
    }

    #[test]
    fn empty_findings_produce_valid_sarif_with_driver_name() {
        let sarif = build_sarif("oxlint", &[]).expect("sarif");
        let json: Value = serde_json::from_str(&sarif).expect("json");
        assert_eq!(json["version"], "2.1.0");
        assert_eq!(json["runs"][0]["tool"]["driver"]["name"], "oxlint");
        assert_eq!(json["runs"][0]["results"], Value::Array(vec![]));
    }

    #[test]
    fn finding_uri_and_level_are_rendered() {
        let sarif = build_sarif("ast-grep", &[finding(Some((1, 12)))]).expect("sarif");
        let json: Value = serde_json::from_str(&sarif).expect("json");
        let result = &json["runs"][0]["results"][0];
        assert_eq!(result["ruleId"], "no-console");
        assert_eq!(result["level"], "error");
        assert_eq!(
            result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "packages/app/src/index.ts"
        );
        let region = &result["locations"][0]["physicalLocation"]["region"];
        assert_eq!(region["endLine"], 1);
        assert_eq!(region["endColumn"], 12);
    }

    #[test]
    fn end_positions_are_omitted_when_absent() {
        let sarif = build_sarif("oxlint", &[finding(None)]).expect("sarif");
        let json: Value = serde_json::from_str(&sarif).expect("json");
        let region = &json["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"];
        assert!(region.get("endLine").is_none());
        assert!(region.get("endColumn").is_none());
        assert_eq!(region["startLine"], 1);
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/luchta-worker/src/lib.rs`, add near the other `pub mod` lines (after `pub mod paths;`):

```rust
pub mod sarif;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p luchta-worker sarif::`
Expected: PASS (3 tests)

- [ ] **Step 4: Commit**

```bash
git add crates/luchta-worker/src/sarif.rs crates/luchta-worker/src/lib.rs
git commit -m "feat(worker): add shared SARIF builder"
```

---

### Task 3: Pin worker process cwd to the workspace root

**Files:**
- Modify: `crates/luchta-engine/src/worker/io_tasks.rs:391-401` (`worker_command`)
- Modify: `crates/luchta-engine/src/worker/spawn.rs` (`SpawnAttempt`, `spawn_worker_process`, `spawn_worker_child`)
- Modify: `crates/luchta-engine/src/worker/manager.rs` (add `workspace_root` field + `with_workspace_root`, pass to spawn — both the `#[cfg(unix)]` impl near line 69 and the non-unix stub near line 503)
- Modify: `crates/luchta-cli/src/run.rs:151` (set workspace root on the manager)

**Interfaces:**
- Consumes: `luchta_worker` unchanged.
- Produces:
  - `worker_command(command_line: &str, workspace_root: &std::path::Path) -> tokio::process::Command`
  - `spawn_worker_process(worker: &str, command_line: &str, workspace_root: &std::path::Path) -> Result<Child, WorkerError>`
  - `WorkerManager::with_workspace_root(self, root: std::path::PathBuf) -> Self`
- Behavior: an empty `workspace_root` (default) leaves the child's cwd unset (preserves prior behavior for tests / non-configured callers).

- [ ] **Step 1: Write the failing test for `worker_command`**

In `crates/luchta-engine/src/worker/io_tasks.rs`, add to the existing `#[cfg(test)] mod tests` (near line 403):

```rust
#[test]
fn worker_command_sets_current_dir_when_root_provided() {
    let command = super::worker_command("echo hi", std::path::Path::new("/repo/root"));
    assert_eq!(
        command.as_std().get_current_dir(),
        Some(std::path::Path::new("/repo/root"))
    );
}

#[test]
fn worker_command_leaves_current_dir_unset_for_empty_root() {
    let command = super::worker_command("echo hi", std::path::Path::new(""));
    assert_eq!(command.as_std().get_current_dir(), None);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p luchta-engine worker_command_sets_current_dir`
Expected: FAIL to compile (`worker_command` takes 1 argument, not 2).

- [ ] **Step 3: Update `worker_command`**

In `crates/luchta-engine/src/worker/io_tasks.rs`, replace `worker_command` (lines 391-401):

```rust
pub(crate) fn worker_command(
    command_line: &str,
    workspace_root: &std::path::Path,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(command_line)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // Pin the resident worker's cwd to the workspace root so diagnostic paths
    // relativize consistently regardless of luchta's invocation directory.
    if !workspace_root.as_os_str().is_empty() {
        command.current_dir(workspace_root);
    }
    command
}
```

- [ ] **Step 4: Thread `workspace_root` through spawn.rs**

In `crates/luchta-engine/src/worker/spawn.rs`:

Change `SpawnAttempt` (lines 10-13):

```rust
struct SpawnAttempt<'a> {
    worker: &'a str,
    command_line: &'a str,
    workspace_root: &'a std::path::Path,
}
```

Change `spawn_worker_process` (lines 15-22):

```rust
pub(crate) async fn spawn_worker_process(
    worker: &str,
    command_line: &str,
    workspace_root: &std::path::Path,
) -> Result<Child, WorkerError> {
    let attempt = SpawnAttempt {
        worker,
        command_line,
        workspace_root,
    };
    let mut last_error = None;
```

Change `spawn_worker_child` (lines 74-76):

```rust
fn spawn_worker_child(attempt: &SpawnAttempt<'_>) -> io::Result<Child> {
    worker_command(attempt.command_line, attempt.workspace_root).spawn()
}
```

- [ ] **Step 5: Add the field + builder + spawn call in manager.rs**

In `crates/luchta-engine/src/worker/manager.rs`, add to the `#[cfg(unix)]` struct (after `prefix_width: usize,` at line ~76):

```rust
    workspace_root: std::path::PathBuf,
```

In `with_shutdown_timeout` (the `#[cfg(unix)]` impl, in the `Self { ... }` literal after `prefix_width: 0,`):

```rust
            workspace_root: std::path::PathBuf::new(),
```

Add the builder next to `with_prefix_width` (after line ~100):

```rust
    pub fn with_workspace_root(mut self, root: std::path::PathBuf) -> Self {
        self.workspace_root = root;
        self
    }
```

Update the spawn call at line 370:

```rust
        let mut child =
            spawn_worker_process(worker_name, &definition.command, &self.workspace_root).await?;
```

Mirror the field + builder on the non-unix stub struct/impl (near lines 503-532) so the crate compiles on non-unix — add `workspace_root: std::path::PathBuf` to the struct, initialize it in its `with_shutdown_timeout`, add the same `with_workspace_root` builder, and reference it in the existing `let _ = (...)` no-op line to avoid dead-code warnings.

- [ ] **Step 6: Set the workspace root in run.rs**

In `crates/luchta-cli/src/run.rs:151`, replace:

```rust
    let worker_manager = Arc::new(WorkerManager::new(config.workers.clone()));
```

with:

```rust
    let worker_manager = Arc::new(
        WorkerManager::new(config.workers.clone())
            .with_workspace_root(workspace_root.to_path_buf()),
    );
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p luchta-engine worker_command_ ` then `cargo build -p luchta-cli`
Expected: the two `worker_command_*` tests PASS; `luchta-cli` builds.

- [ ] **Step 8: Commit**

```bash
git add crates/luchta-engine/src/worker/io_tasks.rs crates/luchta-engine/src/worker/spawn.rs crates/luchta-engine/src/worker/manager.rs crates/luchta-cli/src/run.rs
git commit -m "feat(engine): pin worker process cwd to workspace root"
```

---

### Task 4: ast-grep — repo-root-relative URIs + shared SARIF

**Files:**
- Modify: `crates/luchta-ast-grep-worker/src/lint.rs` (`ScanContext` gains `repo_root`; `relative_uri` uses it; `scan_files`/`scan_files_async` gain a `repo_root` param)
- Modify: `crates/luchta-ast-grep-worker/src/sarif.rs` (delegate to `luchta_worker::sarif`)
- Modify: `crates/luchta-ast-grep-worker/src/main.rs` (compute repo root, pass into `scan_files_async`)

**Interfaces:**
- Consumes: `luchta_worker::paths::repo_relative`, `luchta_worker::sarif::{build_sarif, SarifFinding, SarifLevel}`, `crate::lint::Finding`.
- Produces: `scan_files_async(cwd: &Path, repo_root: &Path, config: &DiscoveredConfig, files: Vec<PathBuf>, fix: bool)` and `build_sarif(findings: &[Finding]) -> Result<String, String>` (unchanged signature, now backed by the shared builder).

- [ ] **Step 1: Update the ast-grep sarif test to expect a repo-root path**

In `crates/luchta-ast-grep-worker/src/sarif.rs` tests, the `single_finding_produces_expected_shape` test uses `relative_uri: "src/index.ts"`. Change that literal to `"packages/app/src/index.ts"` and the assertion at the bottom to match:

```rust
            relative_uri: "packages/app/src/index.ts".to_owned(),
```
```rust
        assert_eq!(
            result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "packages/app/src/index.ts"
        );
```

- [ ] **Step 2: Replace ast-grep's sarif.rs body with a delegation to the shared builder**

Replace the non-test portion of `crates/luchta-ast-grep-worker/src/sarif.rs` (lines 1-125) with:

```rust
use luchta_worker::sarif::{build_sarif as build_shared_sarif, SarifFinding, SarifLevel};

use crate::lint::Finding;

pub fn build_sarif(findings: &[Finding]) -> Result<String, String> {
    let entries: Vec<SarifFinding> = findings
        .iter()
        .map(|finding| SarifFinding {
            rule_id: if finding.rule_id.is_empty() {
                "ast-grep-rule".to_owned()
            } else {
                finding.rule_id.clone()
            },
            level: map_level(&finding.severity),
            message: finding.message.clone(),
            uri: finding.relative_uri.clone(),
            start_line: finding.start_line,
            start_column: finding.start_column,
            end_line: Some(finding.end_line),
            end_column: Some(finding.end_column),
        })
        .collect();
    build_shared_sarif("ast-grep", &entries)
}

fn map_level(sev: &ast_grep_config::Severity) -> SarifLevel {
    match sev {
        ast_grep_config::Severity::Error => SarifLevel::Error,
        ast_grep_config::Severity::Warning => SarifLevel::Warning,
        ast_grep_config::Severity::Info => SarifLevel::Note,
        ast_grep_config::Severity::Hint => SarifLevel::Note,
        ast_grep_config::Severity::Off => SarifLevel::None,
    }
}
```

Keep the existing `#[cfg(test)] mod tests` block (with the edits from Step 1). Confirm `luchta-worker` is a dependency of `luchta-ast-grep-worker` (it is — `Cargo.toml`).

- [ ] **Step 3: Add `repo_root` to `ScanContext` and use it in `relative_uri`**

In `crates/luchta-ast-grep-worker/src/lint.rs`, change `ScanContext` (lines 28-33):

```rust
#[derive(Clone, Copy)]
struct ScanContext<'a> {
    cwd: &'a Path,
    repo_root: &'a Path,
    config_dir: &'a Path,
    language_globs: &'a [LanguageGlobEntry],
}
```

Change `relative_uri` (lines 40-42) to relativize against the repo root:

```rust
    fn relative_uri(&self, file: &Path) -> String {
        luchta_worker::paths::repo_relative(file, self.repo_root)
    }
```

(`selection_path` at lines 36-38 stays as-is — it strips `config_dir` for rule selection, not output.)

- [ ] **Step 4: Thread `repo_root` through `scan_files` and `scan_files_async`**

In the `#[cfg(test)] scan_files` (lines 71-88), add the param and field:

```rust
#[cfg(test)]
pub fn scan_files(
    cwd: &Path,
    repo_root: &Path,
    config: &DiscoveredConfig,
    files: Vec<PathBuf>,
    fix: bool,
) -> Result<ScanResult, String> {
    let rules = load_rules(&config.rule_files)?;
    if rules.is_empty() {
        eprintln!("warning: ast-grep rule set is empty; skipping scan");
        return Ok(ScanResult::default());
    }
    let context = ScanContext {
        cwd,
        repo_root,
        config_dir: &config.config_dir,
        language_globs: &config.language_globs,
    };
    scan_files_with_rules(context, rules, files, fix)
}
```

In `scan_files_async` (lines 413-435), add the param, clone it into the closure, and set the field:

```rust
pub async fn scan_files_async(
    cwd: &Path,
    repo_root: &Path,
    config: &DiscoveredConfig,
    files: Vec<PathBuf>,
    fix: bool,
) -> Result<ScanResult, String> {
    let cwd = cwd.to_path_buf();
    let repo_root = repo_root.to_path_buf();
    let config = config.clone();
    tokio::task::spawn_blocking(move || {
        let rules = load_rules(&config.rule_files)?;
        if rules.is_empty() {
            return Ok(ScanResult::default());
        }
        let context = ScanContext {
            cwd: &cwd,
            repo_root: &repo_root,
            config_dir: &config.config_dir,
            language_globs: &config.language_globs,
        };
        scan_files_with_rules(context, rules, files, fix)
    })
    .await
    .map_err(|error| format!("ast-grep worker join error: {error}"))?
}
```

- [ ] **Step 5: Update `main.rs` to compute + pass the repo root**

In `crates/luchta-ast-grep-worker/src/main.rs`, at the run handler where `scan_files_async(&run.cwd, &run.config, run.files, run.opts.fix)` is called (around line 108), compute the repo root from the process cwd (set by the engine to the workspace root) and pass it:

```rust
                let repo_root = std::env::current_dir().unwrap_or_else(|_| run.cwd.clone());
                match scan_files_async(&run.cwd, &repo_root, &run.config, run.files, run.opts.fix).await {
```

- [ ] **Step 6: Fix any in-crate test call sites of `scan_files`/`scan_files_async`**

Search and update every caller in `crates/luchta-ast-grep-worker/src/**` tests to pass a `repo_root`. For tests that previously expected package-relative output, pass the package `cwd` as `repo_root` to keep them green unless the test's intent is cross-package (then pass the true root and update the expected path).

Run: `grep -rn "scan_files(" crates/luchta-ast-grep-worker/src` and `grep -rn "scan_files_async(" crates/luchta-ast-grep-worker/src` to find them; add the argument.

- [ ] **Step 7: Add a test proving sub-package files are repo-root-relative**

In `crates/luchta-ast-grep-worker/src/lint.rs` tests, add a test that runs a scan with `cwd = <root>/packages/app` and `repo_root = <root>`, then asserts a finding's `relative_uri` starts with `packages/app/`. Model it on the existing scan fixture helpers in that test module (`write_basic_rule_fixture` etc.), placing the source file under `packages/app/` and passing both paths to `scan_files`.

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p luchta-ast-grep-worker`
Expected: PASS (including the new sub-package test and updated sarif test).

- [ ] **Step 9: Commit**

```bash
git add crates/luchta-ast-grep-worker/src
git commit -m "feat(ast-grep): report repo-root-relative diagnostic paths"
```

---

### Task 5: oxlint — repo-root-relative URIs + shared SARIF

**Files:**
- Modify: `crates/luchta-oxlint-worker/src/lint.rs` (`wrap_error` gains `cwd`/`repo_root`; `lint_files`/`lint_files_blocking` gain `repo_root`)
- Modify: `crates/luchta-oxlint-worker/src/sarif.rs` (delegate to `luchta_worker::sarif`)
- Modify: `crates/luchta-oxlint-worker/src/main.rs` (compute repo root, pass into `lint_files`)

**Interfaces:**
- Consumes: `luchta_worker::paths::repo_relative`, `luchta_worker::sarif::{build_sarif, SarifFinding, SarifLevel}`, `crate::lint::WrappedDiagnostic`.
- Produces: `lint_files(cwd: &Path, repo_root: &Path, store: ConfigStore, files: Vec<PathBuf>, opts: OxlintOpts)` and `build_sarif(findings: &[WrappedDiagnostic]) -> Result<String, String>` (unchanged signature).

- [ ] **Step 1: Replace oxlint's sarif.rs body with a delegation to the shared builder**

Replace the non-test portion of `crates/luchta-oxlint-worker/src/sarif.rs` (lines 1-120) with:

```rust
#![cfg(feature = "oxc")]

use luchta_worker::sarif::{build_sarif as build_shared_sarif, SarifFinding, SarifLevel};

use crate::lint::WrappedDiagnostic;

pub fn build_sarif(findings: &[WrappedDiagnostic]) -> Result<String, String> {
    let entries: Vec<SarifFinding> = findings
        .iter()
        .map(|finding| SarifFinding {
            rule_id: finding
                .rule_id
                .clone()
                .unwrap_or_else(|| "oxlint-diagnostic".to_owned()),
            level: map_level(&finding.severity),
            message: finding.message.clone(),
            uri: finding.relative_uri.clone(),
            start_line: finding.start_line,
            start_column: finding.start_column,
            end_line: None,
            end_column: None,
        })
        .collect();
    build_shared_sarif("oxlint", &entries)
}

fn map_level(severity: &oxc_diagnostics::Severity) -> SarifLevel {
    match severity {
        oxc_diagnostics::Severity::Error => SarifLevel::Error,
        oxc_diagnostics::Severity::Warning => SarifLevel::Warning,
        oxc_diagnostics::Severity::Advice => SarifLevel::Note,
    }
}
```

Preserve any existing `#[cfg(test)] mod tests` in that file (update URI literals to a `packages/...`-style path if a test asserts the emitted URI).

- [ ] **Step 2: Add `repo_root` to `wrap_error` and relativize the filename**

In `crates/luchta-oxlint-worker/src/lint.rs`, `wrap_error` currently builds `relative_uri: info.filename.replace('\\', "/")` (line 151). oxc renders `info.filename` relative to the lint cwd (= `req.cwd`). Convert it to repo-root-relative:

Change the signature (line 136) and the `relative_uri` assignment (line 151):

```rust
pub fn wrap_error(error: &Error, cwd: &Path, repo_root: &Path) -> WrappedDiagnostic {
```
```rust
        relative_uri: luchta_worker::paths::repo_relative(&cwd.join(&info.filename), repo_root),
```

(`Path` is already imported in this file — see line 5.)

- [ ] **Step 3: Thread `repo_root` into `lint_files` / `lint_files_blocking` and the wrap_error call**

Change `lint_files` (line 34) and `lint_files_blocking` (line 63) to take `repo_root: &Path` / `repo_root: PathBuf`, clone it across the `spawn_blocking` boundary like `cwd`, and pass `&cwd, &repo_root` into `wrap_error` at the mapping site (line 121: `.map(|error| wrap_error(&error))`):

```rust
        .map(|error| wrap_error(&error, &cwd, &repo_root))
```

Update `lint_files`:

```rust
pub async fn lint_files(
    cwd: &Path,
    repo_root: &Path,
    store: ConfigStore,
    files: Vec<PathBuf>,
    opts: OxlintOpts,
) -> Result<LintRunResult, String> {
    let cwd = cwd.to_path_buf();
    let repo_root = repo_root.to_path_buf();
    tokio::task::spawn_blocking(move || lint_files_blocking(cwd, repo_root, store, files, opts))
        .await
        .map_err(|error| format!("oxlint worker join error: {error}"))?
}
```

Update `lint_files_blocking`'s signature to accept `repo_root: PathBuf` (add the parameter after `cwd: PathBuf`), and thread `&repo_root` to the `wrap_error` mapping.

- [ ] **Step 4: Update `main.rs` to compute + pass the repo root**

In `crates/luchta-oxlint-worker/src/main.rs`, at the `lint_files(cwd, loaded.store, files, opts)` call (line 189), compute and pass the repo root:

```rust
            let repo_root = std::env::current_dir().unwrap_or_else(|_| cwd.to_path_buf());
            let results = match lint_files(cwd, &repo_root, loaded.store, files, opts).await {
```

- [ ] **Step 5: Fix in-crate test call sites**

Run `grep -rn "wrap_error(\|lint_files(\|lint_files_blocking(" crates/luchta-oxlint-worker/src` and update each test caller with a `repo_root` argument. For tests asserting package-relative URIs, pass `cwd` as `repo_root` to preserve intent; for a new cross-package assertion, pass the true root.

- [ ] **Step 6: Add a test proving sub-package findings are repo-root-relative**

Add a `lint.rs` test that lints a file under `<root>/packages/app` with `cwd = <root>/packages/app`, `repo_root = <root>`, and asserts the resulting `relative_uri` starts with `packages/app/`. Reuse the crate's existing lint test fixtures/helpers.

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p luchta-oxlint-worker`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/luchta-oxlint-worker/src
git commit -m "feat(oxlint): report repo-root-relative diagnostic paths"
```

---

### Task 6: oxfmt — repo-root-relative reformat + error lines

**Files:**
- Modify: `crates/luchta-oxfmt-worker/src/format.rs` (`format_path`/`format_diagnostic` take `repo_root`; `relative_display` removed or repurposed)
- Modify: `crates/luchta-oxfmt-worker/src/worker.rs` (`format_file` takes `repo_root`; compute it in `run_in_process`)

**Interfaces:**
- Consumes: `luchta_worker::paths::repo_relative`.
- Produces: `format_path(path: &Path, repo_root: &Path, source: &str, options: &JsFormatOptions) -> Result<FormatResult, String>`; `format_file(path: &Path, repo_root: &Path, loaded_config, opts) -> FileOutcome`.

- [ ] **Step 1: Update `format.rs` error diagnostics to relativize**

In `crates/luchta-oxfmt-worker/src/format.rs`, change `format_diagnostic` (lines 88-90) to take a repo root and relativize:

```rust
fn format_diagnostic(path: &Path, repo_root: &Path, message: &str) -> String {
    format!(
        "{}: {message}",
        luchta_worker::paths::repo_relative(path, repo_root)
    )
}
```

Add `repo_root: &Path` to `format_path` (line 21) and pass it to the two `format_diagnostic` calls (lines 61, 63):

```rust
pub fn format_path(
    path: &Path,
    repo_root: &Path,
    source: &str,
    options: &JsFormatOptions,
) -> Result<FormatResult, String> {
```
```rust
    .map_err(|error| format_diagnostic(path, repo_root, &error.to_string()))?
    .print()
    .map_err(|error| format_diagnostic(path, repo_root, &error.to_string()))?
```

Also update `SourceType::from_path` error (lines 48-53) to use the relative form for consistency:

```rust
    let source_type = SourceType::from_path(path).map_err(|error| {
        format!(
            "failed to determine source type for {}: {error}",
            luchta_worker::paths::repo_relative(path, repo_root)
        )
    })?;
```

Remove `relative_display` (lines 92-94) since `repo_relative` replaces it; keep `normalize_path` only if still referenced (it is used by the `relative_display_normalizes_separators` test — update that test to call `repo_relative`/`normalize_forward_slashes` from `luchta_worker::paths`, or delete it and rely on Task 1's coverage). Update the format.rs tests that call `format_path(path, ...)` to pass a `repo_root` argument (pass `Path::new("")` so error messages equal the given path — those tests assert `result.formatted`, not error text).

- [ ] **Step 2: Update `format_file` to take + use `repo_root`**

In `crates/luchta-oxfmt-worker/src/worker.rs`, change `format_file` (lines 265-272) to take `repo_root` instead of `cwd`, and build `relative` from it:

```rust
#[cfg(feature = "oxc")]
fn format_file(
    path: &Path,
    repo_root: &Path,
    loaded_config: &crate::config::LoadedConfig,
    opts: OxfmtOpts,
) -> FileOutcome {
    let relative = luchta_worker::paths::repo_relative(path, repo_root);
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            return FileOutcome::ReadError(format!("failed to read {relative}: {error}"));
        }
    };
    let options = loaded_config.options_for(path);
    let result = match format_path(path, repo_root, &source, &options) {
        Ok(result) => result,
        Err(error) => return FileOutcome::FormatError(error),
    };
```

Update the `use` at the top of `worker.rs` (line 23 imports `format_path, relative_display`) to drop `relative_display`:

```rust
use crate::format::format_path;
```

Update the `WriteError` message (worker.rs:293-295) similarly if it embeds an absolute path (relativize with `repo_relative(path, repo_root)`); check the `write_text_file` error string and make it repo-root-relative.

- [ ] **Step 3: Compute `repo_root` in `run_in_process` and pass it to the parallel map**

In `crates/luchta-oxfmt-worker/src/worker.rs` `run_in_process`, after `let cwd = PathBuf::from(cwd);` (line 99), add:

```rust
            let repo_root = std::env::current_dir().unwrap_or_else(|_| cwd.clone());
```

At the `process_items_in_parallel(... |path| format_file(path, &cwd, &loaded_config, opts))` call (line 179), clone `repo_root` into the closure scope and pass it:

```rust
                let repo_root = repo_root.clone();
                move || {
                    process_items_in_parallel(
                        /* existing args */
                        files,
                        /* existing worker count */,
                        |path| format_file(path, &repo_root, &loaded_config, opts),
                    )
                }
```

(Match the exact existing closure/argument shape at lines 172-183; the only change is `&cwd` → `&repo_root` and cloning `repo_root` alongside the existing `cwd` clone.)

- [ ] **Step 4: Add a test proving sub-package reformat lines are repo-root-relative**

Add a `worker.rs` (or `format.rs`) test that calls `format_file` with `path = <root>/packages/app/src/x.ts` (unformatted content) and `repo_root = <root>`, then asserts the `FileOutcome::WouldReformat { relative }` (with `opts.check = true`) has `relative == "packages/app/src/x.ts"`. Use a `tempfile::TempDir` and the crate's existing config/opts constructors.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p luchta-oxfmt-worker`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/luchta-oxfmt-worker/src
git commit -m "feat(oxfmt): report repo-root-relative reformat and error paths"
```

---

### Task 7: CLI rendering integration check

**Files:**
- Modify: `crates/luchta-cli/src/format.rs` (add one test to the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `format_sarif_pretty` (existing) which renders `uri:line:col` verbatim from `artifactLocation.uri`.

- [ ] **Step 1: Add the test**

In `crates/luchta-cli/src/format.rs` tests, add a test that feeds a SARIF log whose `artifactLocation.uri` is a repo-root-relative path from a sub-package and asserts the rendered line preserves it (documents the end-to-end contract that workers now emit repo-root-relative URIs and the CLI shows them clickably from the repo root):

```rust
#[test]
fn format_sarif_pretty_preserves_repo_root_relative_subpackage_path() {
    let sarif_json = r#"{
        "version": "2.1.0",
        "runs": [{
            "tool": { "driver": { "name": "oxlint" } },
            "results": [{
                "ruleId": "no-console",
                "level": "error",
                "message": { "text": "Unexpected console" },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": "packages/app/src/index.ts" },
                        "region": { "startLine": 4, "startColumn": 3 }
                    }
                }]
            }]
        }]
    }"#;
    let sarif: serde_sarif::sarif::Sarif = serde_json::from_str(sarif_json).unwrap();
    let formatted = format_sarif_pretty(&sarif, Stream::Stdout);
    assert!(
        formatted.contains("packages/app/src/index.ts:4:3: error: Unexpected console [no-console]"),
        "unexpected render: {formatted}"
    );
}
```

- [ ] **Step 2: Run the test to verify it passes**

Run: `cargo test -p luchta-cli format_sarif_pretty_preserves_repo_root_relative_subpackage_path`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/luchta-cli/src/format.rs
git commit -m "test(cli): assert repo-root-relative SARIF paths render verbatim"
```

---

### Task 8: Full verification

- [ ] **Step 1: Build and test the whole workspace**

Run: `cargo test --workspace`
Expected: PASS. If a worker's existing test asserted a package-relative diagnostic path that is now repo-root-relative, update the expectation (the task cwd's package prefix now appears).

- [ ] **Step 2: Lint/format gate (match repo conventions)**

Run: `cargo clippy --workspace --all-targets` and `cargo fmt --all --check`
Expected: clean. Fix any findings.

- [ ] **Step 3: Manual smoke check (optional but recommended)**

From a checkout with a multi-package workspace, run a lint task that produces a finding in a sub-package and confirm the printed `path:line:col` is `packages/<pkg>/...` and clickable from the repo root.

- [ ] **Step 4: Final commit if any fixups were needed**

```bash
git add -A
git commit -m "chore: fixups for repo-root-relative diagnostic paths"
```

---

## Self-Review notes

- **Spec coverage:** engine cwd pin (Task 3), shared helpers in luchta-worker (Tasks 1-2), ast-grep/oxlint/oxfmt relativization + shared SARIF (Tasks 4-6), oxfmt absolute→relative error fix (Task 6), fallback-to-absolute (Task 1 `repo_relative`), CLI render contract (Task 7), inputs/outputs unchanged (not touched). All spec sections map to a task.
- **Type consistency:** `repo_relative(&Path, &Path) -> String`, `build_sarif(&str, &[SarifFinding]) -> Result<String,String>`, and `SarifFinding`/`SarifLevel` are used identically across Tasks 4-6. Worker-local `build_sarif(&[Finding|WrappedDiagnostic])` signatures are preserved so their call sites in each `main.rs` need no change beyond the repo-root threading already covered.
- **Known follow-the-code steps:** Tasks 4-6 include `grep` steps to find and fix in-crate test call sites, because those callers are numerous and mechanical (add one argument each).
