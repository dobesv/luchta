---
title: "In-process autofix mode for linting workers (oxlint, oxfmt, ast-grep)"
date: 2026-07-14
category: integration-issues
problem_type: integration_issue
component: luchta-oxlint-worker, luchta-oxfmt-worker, luchta-ast-grep-worker, luchta-worker
root_cause: "in-process library APIs require different fix-mode wiring than CLI shells; rule scoping must be consistent between fix and scan paths; overlapping edits corrupt output if not conflict-detected"
resolution_type: code_fix
severity: high
tags:
  - autofix
  - in-process-worker
  - oxlint
  - oxfmt
  - ast-grep
  - overlapping-edits
  - rule-scoping
  - severity-filtering
  - quote-aware-tokenizer
  - codescene-test-structure
plan_ref: oxc-fix-mode
---

## Problem

Adding `--fix` (autofix) mode to in-process linting workers required library-driven fix application (not shelling out to CLI), consistent rule scoping between fix and scan paths, handling overlapping edits safely, and shared command-string parsing—all while satisfying CodeScene code-quality thresholds.

## Symptoms

- oxlint worker hardcoded `FixKind::None`; needed `FixKind::SafeFix` when `--fix` passed.
- The ast-grep fix path applied rules to out-of-scope files and disabled (`severity: off`) rules because fix loop iterated raw loaded rules instead of scoped collection.
- Overlapping/same-start edits from multiple ast-grep rules corrupted output or silently dropped edits (BTreeMap keyed by start position).
- Each worker had its own ad-hoc command tokenizer; inconsistent parsing behavior across workers.
- Near-identical test bodies tripped CodeScene "Code Duplication"; assertion helpers with many `&str` params tripped "Excess Arguments" / "String-Heavy Arguments".

## Investigation Steps

1. Traced oxlint fix path: `OxlintOpts.fix` must map to `FixKind::SafeFix` (NOT `FixKind::Safe`—the actual variant name in pinned oxc version).
2. Traced ast-grep fix path: `apply_fixes_to_file` iterated `rules: &[RuleConfig]` directly, bypassing `collection.get_rule_from_lang(Path, lang)` used by scan path.
3. Verified `Severity::Off` filter present in scan path (`matches!(rule.severity, Severity::Off) { continue; }`) but missing in fix candidate collection.
4. Tested naive BTreeMap edit collection: edits at same start position silently overwrite; overlapping ranges cause `replace_range` corruption.
5. Noted `RuleConfig` is NOT `Clone`—cannot build second `RuleCollection` from `&[RuleConfig]` for fix path.
6. Hoisted tokenizer to `luchta-worker/src/tokenize.rs`; verified dep cycle avoided (worker → types; workers already depend on worker).
7. Reviewed CodeScene test-quality rules: extracted table-driven tests and struct-based assertion helpers.

## Root Cause

1. **API matching**: oxlint's `FixKind` enum variant is `SafeFix` not `Safe` in the pinned oxc version.
2. **Rule scoping inconsistency**: ast-grep fix path used raw rules list; scan path used `RuleCollection::get_rule_from_lang(path, lang)` for file/language scoping.
3. **Severity bypass**: Fix path did not skip `Severity::Off` rules before building fixers.
4. **Overlap corruption**: BTreeMap keyed by start position overwrites same-start edits; overlapping ranges splice incorrectly.
5. **Concrete type required**: `RuleConfig` not `Clone` → share single `RuleCollection` between fix and scan, preventing second collection.
6. **Tokenizer duplication**: Shared parsing logic needed centralization to avoid drift.

## Solution

### 1. oxlint: Wire `--fix` to `FixKind::SafeFix`

```rust
// crates/luchta-oxlint-worker/src/lint.rs:70-74
let fix_kind = if opts.fix {
    FixKind::SafeFix
} else {
    FixKind::None
};

// Passed to TsGoLintState::try_new, Linter::with_fix, and LintRunner::with_fix_kind
```

### 2. oxfmt: `--fix` Alias for Write Mode

```rust
// crates/luchta-oxfmt-worker/src/opts.rs:20-23
// --check wins when both present; --fix is explicit alias for default write mode
let has_check = tokens.iter().any(|token| token == "--check");
Self { check: has_check }
```

### 3. ast-grep: Single RuleCollection with Consistent Scoping

```rust
// crates/luchta-ast-grep-worker/src/lint.rs:96-103
let collection = RuleCollection::try_new(rules)?;
if fix {
    apply_fixes(context, &collection, &files)?;
}
scan_files_with_collection(context, &collection, files)

// crates/luchta-ast-grep-worker/src/lint.rs:185-186
// apply_fixes_to_file uses SAME scoped selection as scan_file:
let applicable_rules = collection.get_rule_from_lang(Path::new(&selection_path), lang);
```

### 4. ast-grep: Skip Severity::Off Before Building Fixers

```rust
// crates/luchta-ast-grep-worker/src/lint.rs:217-220
for rule in applicable_rules {
    if matches!(rule.severity, Severity::Off) {
        continue;  // matches scan behavior
    }
    let fixers = rule.get_fixer()?;
    // ... collect edits
}
```

### 5. ast-grep: Overlap Detection with Right-to-Left Application

```rust
// crates/luchta-ast-grep-worker/src/lint.rs:241-272
fn select_non_overlapping_edits(file: &Path, mut edits: Vec<FileEdit>) -> (Vec<FileEdit>, Vec<String>) {
    // Sort by position, then descending deleted_length (longer first), then rule_id for determinism
    edits.sort_by(|left, right| {
        left.position.cmp(&right.position)
            .then_with(|| right.deleted_length.cmp(&left.deleted_length))
            .then_with(|| left.rule_id.cmp(&right.rule_id))
    });
    
    let mut accepted = Vec::new();
    let mut warnings = Vec::new();
    let mut last_end = 0usize;
    for edit in edits {
        let start = edit.position;
        let end = edit.position + edit.deleted_length;
        if !accepted.is_empty() && start < last_end {
            warnings.push(format!(
                "warning: skipped conflicting fix from rule {} for {} at byte range {}..{}",
                edit.rule_id, file.display(), start, end
            ));
            continue;  // skip overlapping edit
        }
        last_end = end;
        accepted.push(edit);
    }
    (accepted, warnings)
}

// crates/luchta-ast-grep-worker/src/lint.rs:274-280
fn apply_edits(mut source: Vec<u8>, edits: Vec<FileEdit>) -> Result<String, String> {
    // Right-to-left to preserve byte offsets
    for edit in edits.into_iter().rev() {
        let end = edit.position + edit.deleted_length;
        source.splice(edit.position..end, edit.inserted_text);
    }
    String::from_utf8(source).map_err(|error| format!("failed to decode rewritten source: {error}"))
}
```

### 6. Shared Quote-Aware Tokenizer

```rust
// crates/luchta-worker/src/tokenize.rs
pub fn tokenize_command(raw: &str) -> Vec<String> {
    // Handles: --config '/path/with spaces' and --config="/path"
    // Preserves empty quoted tokens so --config "" --fix doesn't misparse
}
```

All workers now use consistent parsing via `luchta_worker::tokenize::tokenize_command`.

### 7. CodeScene-Compliant Test Structure

```rust
// Table-driven instead of near-identical bodies:
#[test]
fn fix_mode_behavior_matrix() {
    #[derive(Clone)]
    struct Case {
        name: &'static str,
        fix: bool,
        severity: Severity,
        expected_rewrite: bool,
        expected_findings: usize,
    }
    let cases = &[
        Case { name: "fix_on_severity_error", fix: true, severity: Severity::Error, expected_rewrite: true, expected_findings: 0 },
        Case { name: "fix_off_severity_error", fix: false, severity: Severity::Error, expected_rewrite: false, expected_findings: 1 },
        Case { name: "fix_on_severity_off", fix: true, severity: Severity::Off, expected_rewrite: false, expected_findings: 0 },
    ];
    for case in cases { /* single assert helper call */ }
}

// Struct-based assertion helper:
struct ExpectedFinding {
    rule_id: &'static str,
    line: usize,
    message_contains: &'static str,
}
impl ExpectedFinding {
    fn assert_matches(&self, findings: &[Finding]) { /* ... */ }
}
```

## Why This Works

1. **Single RuleCollection**: Both fix and scan paths use same scoping logic; no two-collection inconsistency possible.
2. **Severity::Off filter**: Disabled rules skipped in both paths; user expectation of "disabled = no side effects" satisfied.
3. **Overlap detection**: Greedy interval selection + warning emission prevents silent dropped edits and output corruption.
4. **Right-to-left splice**: Preserves byte offsets after each edit.
5. **UTF-8 validation**: `String::from_utf8` catches any corruption from misaligned offsets.
6. **Shared tokenizer**: Consistent quote handling across all workers; `tokenize_command` is pure function with tests.

## Prevention Strategies

**Test Cases:**
- Fix-on rewrites file; fix-off leaves untouched and reports findings.
- `severity: off` rule neither rewrites nor reports.
- Path-scoped rule (`files: ['**/*.tsx']`) does not apply fix to out-of-scope file.
- Overlapping edits at same start position: exactly one applied, warning emitted.
- Quote-parsing: `--config '/path/with spaces'` produces single path token.
- Empty quoted value: `--config "" --fix` parses config=None, fix=true.

**Code Review Checklist:**
- [ ] Fix path uses SAME `RuleCollection::get_rule_from_lang` call as scan path.
- [ ] `Severity::Off` filtered before fixer construction.
- [ ] Overlap detection handles same-start and intersecting ranges.
- [ ] Edits applied right-to-left to preserve byte offsets.
- [ ] UTF-8 validation after edit application.
- [ ] Table-driven tests for similar test scenarios (no CodeScene duplication).
- [ ] Assertion helper structs with named fields (no string-heavy params).

## Related Issues

- **GitHub:** [#226](https://github.com/dobesv/luchta/issues/226) — Support `--fix` mode for linting workers
- **Related Solution:** [integration-oxlint-config-option-phase-merge-2026-07-13.md](./integration-oxlint-config-option-phase-merge-2026-07-13.md) — Shared tokenizer introduced for config option parsing
- **Related Solution:** [ast-grep-worker-in-process-integration-2026-07-11.md](./ast-grep-worker-in-process-integration-2026-07-11.md) — ast-grep API probe-validation methodology
- **Related Solution:** [codescene-quality-score-refactoring-2026-06-09.md](../workflow-issues/codescene-quality-score-refactoring-2026-06-09.md) — CodeScene test-structure remediation patterns
