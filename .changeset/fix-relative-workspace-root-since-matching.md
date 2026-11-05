---
"luchta": patch
---

# Fix `--since` silently skipping tasks when `--workspace-root` is relative

`run::resolve_workspace_root` now canonicalizes an explicit `--workspace-root` before any consumer uses it. With a relative root (e.g. `--workspace-root ../ws` from a sibling directory), gix returned an absolute repository base for changed paths while task-input requests kept relative bases, so `PathRequestMatcher` failed its `strip_prefix` and tasks whose declared `inputs` matched the change were silently filtered out of `run --since` and `list --since`. Config-script spawning and workspace discovery also share the now-absolute base. A `--workspace-root` that does not exist keeps its previous error behavior.

Canonicalization uses `dunce`, which on Windows avoids the `\\?\` verbatim prefix that `std::fs::canonicalize` produces and that gix paths never carry, so absolute roots on Windows keep working. A symlinked `--workspace-root` now resolves to its canonical path, so errors and worker working directories show the real location rather than the symlink.
