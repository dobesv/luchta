---
title: "Oxfmt config option gaps and per-file override resolution"
date: 2026-07-12
category: "integration-issues"
problem_type: integration_issue
component: "luchta-oxfmt-worker"
root_cause: "Hand-maintained serde config struct silently drops unknown keys; override inheritance clobbers inherited values"
resolution_type: code_fix
severity: high
tags:
  - oxfmt
  - prettier-compat
  - config-override
  - eager-validation
  - glob-matching
  - serde-silent-drop
  - sort-imports
plan_ref: "oxfmt-opts-gaps"
last_updated: 2026-07-17
---

# Problem

`luchta-oxfmt-worker` mapped `.oxfmtrc.json` (Prettier-style config) to `oxc_formatter::JsFormatOptions` but silently dropped several scalar/enum options including `arrowParens` (subject of GitHub issue #211). Additionally, adding `overrides` support introduced a panic bug: override options were not validated at load time, causing `.expect()` to panic later when formatting matching files.

# Symptoms

- `arrowParens: "avoid"` in `.oxfmtrc.json` had no effect — arrow functions always had parentheses
- Other Prettier-compatible options (`quoteProps`, `singleAttributePerLine`, `objectWrap`, `htmlWhitespaceSensitivity`, `embeddedLanguageFormatting`) were silently ignored
- Config with invalid override values (e.g., `printWidth: 0`) panicked at format time instead of returning a config error
- No test coverage for invalid override values or precedence semantics

# Investigation Steps

1. Reviewed `.oxfmtrc` parsing in `config.rs`. Found options listed in "Still ignored" comment block.
2. Checked upstream `oxc_formatter` (`crates/oxc_formatter/src/options.rs`, rev 415fe1e) — all target fields already exist on `JsFormatOptions`.
3. Referenced `apps/oxfmt/src/core/options/to_oxc_formatter.rs` for canonical Prettier→enum mappings.
4. Identified panic path: `build_override_matchers` cloned override configs without validating, but ` LoadedConfig::options_for` called `apply_oxfmtrc_to_options(...).expect("override options already validated")`.
5. Traced `LineWidth::try_from(0)` — returns `Err`, confirming invalid values cause runtime panic.
6. Clippy flagged 6-element tuple return type as `type_complexity` — factored into `ParsedConfig` struct.

# Root Cause

**Silent option dropping**: Worker code parsed options but never wired them to `JsFormatOptions`. Upstream oxc_formatter already supported all fields.

**Panic on invalid override values**: Deferred application pattern stored unvalidated `OxfmtRc` in `OverrideMatcher`. The assertion `.expect("override options already validated")` was a latent panic because validation never happened. Invalid config values (e.g., `printWidth: 0` from `LineWidth::try_from`) surfaced at format time instead of load time.

# Solution

## 1. Added 6 previously-ignored option mappings

```rust
// arrowParens: "avoid" -> ArrowParentheses::AsNeeded, "always" -> Always
options.arrow_parentheses = match arrow_parens {
    ArrowParens::Avoid => ArrowParentheses::AsNeeded,
    ArrowParens::Always => ArrowParentheses::Always,
};

// quoteProps: "as-needed"/"consistent"/"preserve" -> QuoteProperties::*
options.quote_properties = match quote_props {
    QuoteProps::AsNeeded => QuoteProperties::AsNeeded,
    QuoteProps::Consistent => QuoteProperties::Consistent,
    QuoteProps::Preserve => QuoteProperties::Preserve,
};

// singleAttributePerLine: true -> AttributePosition::Multiline
options.attribute_position = if single_attribute_per_line {
    AttributePosition::Multiline
} else {
    AttributePosition::Auto
};

// objectWrap: "preserve" -> Expand::Auto, "collapse" -> Expand::Never
options.expand = match object_wrap {
    ObjectWrap::Preserve => Expand::Auto,
    ObjectWrap::Collapse => Expand::Never,
};

// htmlWhitespaceSensitivity: "ignore" -> html_whitespace_sensitivity_ignore=true
options.html_whitespace_sensitivity_ignore =
    matches!(html_whitespace_sensitivity, HtmlWhitespaceSensitivity::Ignore);

// embeddedLanguageFormatting: "auto"/"off" -> EmbeddedLanguageFormatting::*
options.embedded_language_formatting = match embedded_language_formatting {
    EmbeddedLanguageFormattingOption::Auto => EmbeddedLanguageFormatting::Auto,
    EmbeddedLanguageFormattingOption::Off => EmbeddedLanguageFormatting::Off,
};
```

## 2. Per-file override resolution

Refactored option application into shared function:

```rust
fn apply_oxfmtrc_to_options(config: &OxfmtRc, options: &mut JsFormatOptions) -> Result<(), String>
```

`LoadedConfig::options_for(&Path)` clones base options and applies matching overrides:

```rust
pub fn options_for(&self, path: &Path) -> JsFormatOptions {
    let mut options = self.options.clone();
    let relative_path = path.strip_prefix(&self.config_root).unwrap_or(path);
    for override_matcher in &self.overrides {
        if override_matcher.matcher.matched_path_or_any_parents(&relative_path, false).is_ignore() {
            apply_oxfmtrc_to_options(&override_matcher.options, &mut options)
                .expect("override options already validated");
        }
    }
    options
}
```

Glob matching uses `ignore::gitignore::GitignoreBuilder` anchored at config directory. Pattern `*.json` matches at any depth (gitignore semantics).

## 3. Eager validation of override options

```rust
fn build_override_matchers(overrides: &[Override], root: &Path) -> Result<Vec<OverrideMatcher>, String> {
    let mut matchers = Vec::new();
    for override_config in overrides {
        // Validate eagerly so invalid values surface at load time
        let mut scratch = JsFormatOptions::new();
        apply_oxfmtrc_to_options(&override_config.options, &mut scratch)
            .map_err(|error| format!("invalid override options: {error}"))?;

        // Only store validated configs
        matchers.push(OverrideMatcher { matcher, options: override_config.options.clone() });
    }
    Ok(matchers)
}
```

## 4. Worker loop integration

Single-line change in `main.rs`:

```rust
// Before: single shared options
let options_for_blocking = format_options.clone();

// After: per-file resolution
let options_for_blocking = loaded_config.options_for(&path);
```

# Why This Works

**Shared applier ensures consistency**: `apply_oxfmtrc_to_options` called for base config, override validation, and per-file resolution. Single source of truth for all option mappings.

**Eager validation eliminates panic path**: Invalid override values fail during `discover_config()`, returning clean error to user. The `.expect()` in `options_for` is now valid assertion since only configs that passed validation are stored.

**Gitignore semantics for glob matching**: `ignore::GitignoreBuilder` already used for `ignorePatterns`; reusing for override `files` patterns ensures consistent behavior. Pattern `*.json` matches at all depths without explicit `**/` prefix.

**Last-match-wins semantics**: Overrides applied in array order; later matching entries overwrite earlier ones. Matches Prettier/ESLint standard behavior.

**`excludeFiles` opt-out**: Each override accepts an optional `excludeFiles` (string or array) compiled into a second `Gitignore` matcher. An override is skipped for a file when its `files` matcher matches but its `excludeFiles` matcher also matches — mirroring Prettier's `overrides[].excludeFiles`. Absent `excludeFiles` preserves prior behavior.

# Prevention Strategies

**Test Cases:**
- Invalid override values return error at load time (`invalid_override_options_return_error_at_load_time`)
- Multiple matching overrides apply in order (`overrides_apply_in_order_last_match_wins`)
- Deep nested paths match correctly (`overrides_match_deeply_nested_paths`)
- Non-matching files fall back to base options
- `excludeFiles` skips the override for excluded paths (`overrides_respect_exclude_files`)
- Per-file resolution reflected in formatted output (`format_path_uses_oxfmtrc_overrides_per_file`)

**Code Review Checklist:**
- [ ] All `OxfmtRc` fields mapped to `JsFormatOptions`?
- [ ] Override options validated before `.expect()` assertion?
- [ ] Glob patterns tested at multiple depths?
- [ ] Reference upstream oxc_formatter for canonical enum mappings

**When adding deferred config:**
- Validate eagerly at load time, or
- Propagate errors at application time instead of `.expect()`
- Never store unvalidated config that later code assumes is valid

**Reference upstream for mappings:**
- `oxc_formatter/src/options.rs` for available `JsFormatOptions` fields
- `apps/oxfmt/src/core/options/to_oxc_formatter.rs` for canonical Prettier→enum mappings

---

# Addendum: sortImports and Systematic Unknown-Key Detection (2026-07-17)

GitHub issue #242 added `sortImports` support and systematic unknown-key warnings.

## Problem: Serde Silent Drop Pattern

`OxfmtRc` in `config.rs` is a hand-maintained subset of prettier-compatible options. Without `#[serde(deny_unknown_fields)]`, serde silently drops unknown keys — `sortImports`, `sortTailwindcss`, `jsdoc`, experimental options all vanished without warning.

## Solution: Warning-Based Unknown Key Strategy

Chose warnings over hard-errors for forward-compat. A newer or shared `.oxfmtrc` degrades gracefully (warn + ignore) rather than hard-failing formatting for the whole repo.

Implementation in `config.rs`:
- `KNOWN_FORMAT_OPTION_KEYS` — single source of truth for valid formatter options
- `KNOWN_TOP_LEVEL_ONLY_KEYS` — `["overrides", "ignorePatterns", "$schema"]`
- `KNOWN_OVERRIDE_ENTRY_KEYS` — `["files", "excludeFiles", "options"]`
- `collect_unknown_options()` — scans raw JSON before serde deserialization
- Notices surfaced via `LoadedConfig.unsupported_option_notices` (NOT `warnings` —
  see "Two Diagnostic Channels" below). These are informational and never affect
  task resolution.

```rust
let unknown = collect_unknown_options(&json)?;
for key in unknown.top_level {
    unsupported_option_notices.push(format!(
        "warning: unsupported .oxfmtrc option `{key}` in {}; ignoring",
        path.display()
    ));
}
```

Unsupported experimental options (`experimentalOperatorPosition`, `experimentalTernaries`) are ignored and surfaced as unsupported-key notices instead of hard-failing. They're absent from `KNOWN_FORMAT_OPTION_KEYS`, so they flow through the same generic notice path.

## Override Scanner Subtlety

Override entry shape is `{files, excludeFiles, options}`. Formatter options live under nested `options`, not at entry root. The scanner must:
1. Warn on formatter keys misplaced at override root (serde silently ignores them)
2. Recurse into `overrides[].options` for typos there — reusing the shared
   `unknown_format_option_keys` helper, which ALSO descends into a nested
   `sortImports` / `experimentalSortImports` object (so typos like
   `sortImports.custmGroups` inside an override are surfaced too).

```rust
fn unknown_override_entry_keys(entry: &Value) -> Vec<String> {
    // Unknown at root (e.g. "singleQuote" at root instead of under "options")
    let mut unknown: Vec<String> = object
        .keys()
        .filter(|key| !is_known(KNOWN_OVERRIDE_ENTRY_KEYS, key))
        .cloned()
        .collect();
    // Unknown inside nested options — including nested sortImports sub-keys,
    // via the same helper used for the top-level scan.
    if let Some(options) = object.get("options").and_then(Value::as_object) {
        unknown.extend(unknown_format_option_keys(options));
    }
    unknown
}
```

## Override Inheritance Bug

`apply_oxfmtrc_to_options` is used for both base config and per-override merge. Unconditional assignment of `options.sort_imports = resolve(...)` meant an override omitting `sortImports` overwrote inherited top-level value with `None` — silently disabling sorting.

Fix: guard with presence check:

```rust
// config.rs:243-245
if config.sort_imports.is_some() {
    options.sort_imports = sort_imports::resolve_sort_imports(config.sort_imports.clone())?;
}
```

Explicit `sortImports: false` still resolves to `None` (intended disable). Pattern matches every scalar option's `if let Some(...)` guard.

## Where to Find the Upstream Schema

`apps/oxfmt` is a binary crate (not a publishable library), so luchta cannot depend on it. But these are authoritative sources to MIRROR:

- `apps/oxfmt/src/core/oxfmtrc.rs` — `FormatConfig`, `SortImportsUserConfig`, `SortImportsConfig` serde schema
- `apps/oxfmt/src/core/options/to_oxc_formatter.rs::to_sort_imports` — canonical mapping
- Vendored at `~/.cargo/git/checkouts/oxc-*/<rev>/` — rev pinned in workspace root Cargo.toml

`oxc_formatter::JsFormatOptions` supports: `sort_imports: Option<SortImportsOptions>`, `sort_tailwindcss`, `jsdoc`, `experimental_operator_position`, `experimental_ternaries`.

## CodeScene Refactoring Notes

Faithfully mirroring upstream's group/customGroups parsing produces high cyclomatic complexity. Decompose into `map_*`/`apply_*` helpers. Caveats:

- `if let Some(x) { options.y = match x {...} }` triggers "Bumpy Road" (nesting depth 2) — extract inner match into free `map_*` fn
- Over-extracting tiny helpers into one file triggers "Low Cohesion" — move cohesive cluster into its own module (`sort_imports.rs`)

Worker-level E2E tests belong in `tests/protocol.rs` (spawns real binary) rather than inflating the unit-test module's LCOM4 responsibility count.

## Verification Gotchas

- `SortImportsOptions::validate()` rejects `partitionByNewline: true` + `newlinesBetween: true` together
- `ignore` crate accepts `"["` as gitignore pattern (no error) but rejects `"\\"` (dangling escape)
- `ImportSelector` has no `Value` variant — catch-all selector string is `"import"`

## Related Issues

- **GitHub:** [#242](https://github.com/dobesv/luchta/issues/242) — Honor sortImports + systematically surface unmapped options
- **GitHub:** [#211](https://github.com/dobesv/luchta/issues/211) — Support configuration overrides in oxfmt worker
- **Related Solution:** [logic-errors/ignore-pattern-worker-file-selection-2026-07-11.md](../logic-errors/ignore-pattern-worker-file-selection-2026-07-11.md) — Config-anchored pattern matching using same `ignore::Gitignore` crate
- **Related Solution:** [integration-issues/oxc-worker-in-process-integration-2026-07-08.md](../integration-issues/oxc-worker-in-process-integration-2026-07-08.md) — Original `.oxfmtrc` config discovery implementation

---

# POST-MERGE REGRESSION: Warnings Pruned Tasks Silently (2026-07-17)

## Regression

After #242 merged, `luchta run oxfmt` reported `note: task 'oxfmt' was pruned from every package during resolution; nothing to run` for any `.oxfmtrc` containing an unrecognized key (e.g. `plugins`).

**Root cause:** `OxfmtWorker::resolve_task` (`crates/luchta-oxfmt-worker/src/worker.rs`) treated ANY `config.warnings` — and any `discover_config`/`collect_formattable_files` error — as `ResolveResult::reject`. In run mode, the engine downgrades `Reject` to `Prune` (see `PruneOutcome::Rejected` in luchta-engine). The CLI then reports "nothing to run" (`luchta-cli/src/run.rs` `report_unmatched_request`).

**Trigger:** #242 added "unsupported .oxfmtrc key" notices into `config.warnings`. Any config with an unknown key pruned the task. Symptom appeared intermittent ("worked once, pruned later") but was actually config-content dependent.

## Design Principle

Config errors must FAIL at execution, not PRUNE at resolve. Pruning hides problems as "nothing to run" — the opposite of useful. The resolve phase's job is computing file inputs and detecting the legitimate "no source files" case.

**Fix:** `resolve_task` now returns `Modify` (keep the task) on ANY config discovery/collection error or warning. Only `Prune`s when genuinely no JS/TS source files exist. `run_in_process` re-discovers config at execution, emits diagnostics to stderr, and returns exit_code 1 on real parse errors.

**Rule:** A worker's resolve verdict should never turn a config/tooling problem into a silent prune.

## Two Diagnostic Channels

`LoadedConfig` now separates:
- `warnings` — genuine problems (unparseable ignore pattern)
- `unsupported_option_notices` — unknown-key notices

Both informational at execution via `LoadedConfig::diagnostics()`; neither affects resolution.

## Unknown-Key Scanning Must Recurse and Cover Aliases

The scanner must descend into nested objects (`overrides[].options`, and the `sortImports` object) AND check alias spellings (`sortImports` + `experimentalSortImports`). Typos nested inside `sortImports` or its alias are silently dropped otherwise — recreating the exact bug class in the new feature surface.

See `KNOWN_SORT_IMPORTS_KEYS` / `SORT_IMPORTS_KEYS` / `unknown_sort_imports_keys` in `config.rs`.

## Bound the FILE Read

`load_config_from_path` originally capped only the comment-stripped output; the raw `read_to_string` was unbounded (OOM vector).

**Fix:** `file.take(MAX_CONFIG_BYTES).read_to_string(...)` bounds the input itself, not just derived buffers.

## Testing Resolve-Phase Behavior

Worker binary handles `resolveTask` JSONL message (`WorkerMessage::ResolveTask`, tag `resolveTask`) and responds `{"type":"resolved","result":{"decision":"modify"|"prune"|...}}`. Integration tests can drive this directly (`tests/protocol.rs` `resolve_decision` helper) to assert the decision — stronger than unit-testing the pub(crate) fn.
