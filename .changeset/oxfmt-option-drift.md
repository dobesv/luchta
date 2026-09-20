---
"luchta": patch
---

Honor `experimentalOperatorPosition` and `jsdoc` in `.oxfmtrc.json`

The oxfmt worker now respects these options from `.oxfmtrc.json`:
- `experimentalOperatorPosition`: maps to `JsFormatOptions.operator_position`. Values: `"start"` or `"end"` (default: `"end"`).
- `jsdoc`: maps to `JsFormatOptions.jsdoc`. Accepts a boolean or object with 11 configuration fields including `commentLineStrategy` and `lineWrappingStyle`.

Previously, valid oxfmt options like `insertFinalNewline` emitted misleading "unsupported" warnings. Now:
- Options that luchta recognizes but doesn't apply (`insertFinalNewline`, `embeddedLanguageFormatting`, `sortTailwindcss` when truthy) emit an accurate notice: `recognized by bundled oxfmt but not applied by luchta's in-process formatter`.
- Options that only affect non-JS/TS file types (`proseWrap`, `svelte`, `vueIndentScriptAndStyle`, `sortPackageJson`) are completely silent.
- Genuinely unknown options still emit the existing "unsupported" warning.

Added a bidirectional schema-drift guardrail test that verifies luchta's option classification matches the bundled oxfmt 0.68.0 configuration schema. This test will fail if future oxc upgrades add or remove options without updating luchta's classification.

Fixes #366.
