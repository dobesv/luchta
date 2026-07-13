---
title: "Oxfmt config option gaps and per-file override resolution"
date: 2026-07-12
category: "integration-issues"
problem_type: integration_issue
component: "luchta-oxfmt-worker"
root_cause: "Config mapping code silently dropped Prettier-compatible options; override options not validated at load time causing runtime panics"
resolution_type: code_fix
severity: high
tags:
  - oxfmt
  - prettier-compat
  - config-override
  - eager-validation
  - glob-matching
plan_ref: "oxfmt-opts-gaps"
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

# Related Issues

- **GitHub:** [#211](https://github.com/dobesv/luchta/issues/211) — Support configuration overrides in oxfmt worker
- **Related Solution:** [logic-errors/ignore-pattern-worker-file-selection-2026-07-11.md](../logic-errors/ignore-pattern-worker-file-selection-2026-07-11.md) — Config-anchored pattern matching using same `ignore::Gitignore` crate
- **Related Solution:** [integration-issues/oxc-worker-in-process-integration-2026-07-08.md](../integration-issues/oxc-worker-in-process-integration-2026-07-08.md) — Original `.oxfmtrc` config discovery implementation
