---
luchta: minor
---
# Add `--since` and `--packages` flags to `luchta list`

Adds task filtering and package listing to `luchta list`:

- `--since <GIT_REF>` filters listed tasks to those affected by git changes since the target ref, including changed packages and their transitive dependents (matching `luchta run --since`).
- `--packages` lists the unique packages that own selected tasks rather than individual tasks. Plain text output lists sorted package names one per line. JSON output (`--json`) returns an array of `{package, path}` objects with workspace-relative paths. Only packages that own at least one matching task are listed.

Combining both options (`luchta list --packages --since <ref>`) outputs affected packages as a drop-in replacement for `lage affected --since <ref>` (#330):

```
luchta list --packages --since "$MERGE_BASE_SHA"
```
