---
title: "Yarn Berry lockfile parser with serde_norway and format auto-detection"
date: 2026-06-09
category: integration-issues
problem_type: integration_issue
component: luchta-lockfiles/yarn-berry
root_cause: "Yarn Berry lockfiles use strict YAML format incompatible with Yarn v1 parser"
resolution_type: code_fix
severity: medium
tags:
  - yarn
  - yaml
  - serde
  - lockfile
  - dependency-resolution
plan_ref: yarn-berry-lockfile
---

## Problem

Luchta's lockfile parser only supported Yarn v1's custom lockfile syntax. Yarn Berry (v2/v3/v4) uses a completely different strict YAML format with a `__metadata` block and protocol-prefixed descriptors. Users with modern Yarn installations could not parse their lockfiles.

## Symptoms

1. **Parse failures on Berry lockfiles**: `yarn-lock-parser` crate (v0.11.0) throws parse errors on Yarn Berry lockfiles — it only handles v1 syntax.
2. **Missing `__metadata` block**: Berry lockfiles start with `__metadata:` containing version and cacheKey, which v1 parser doesn't recognize.
3. **Quoted comma-joined keys**: Berry entries use `"chalk@npm:^4.1.2, chalk@npm:^4.0.0":` syntax not present in v1.
4. **Protocol prefixes**: Berry uses `name@npm:range`, `name@workspace:*`, `name@patch:...` format incompatible with v1's `name@range`.

## Investigation Steps

1. **YAML crate evaluation**: Both `serde_yaml` (0.9.34+deprecated) and `serde_yml` are deprecated/unmaintained. Searched crates.io for "YAML data format for Serde" — found `serde_norway` (0.9.42) as the actively maintained fork.

2. **Berry format analysis**: Examined real Yarn Berry lockfiles. Key structure:
   - Top-level `__metadata` with integer `version` (4=v2, 6=v3, 8=v4) and `cacheKey`
   - Entry keys are quoted YAML strings, comma-joined: `"pkg@npm:^1.0, pkg@npm:^2.0":`
   - Dependencies written as `chalk: "npm:^4.1.2"` with protocol prefix
   - Entry fields: version, resolution, dependencies, peerDependencies, checksum, etc.

3. **Descriptor parsing**: Entry keys must split on `", "` (comma-space) to index each descriptor separately. Splitting on just `,` would break on scoped packages like `@babel/core`.

4. **Resolution approach design**: `resolve_package(name, version)` needs to handle both bare ranges (default to `npm:`) and protocol-prefixed ranges (use as-is).

## Root Cause

Yarn Berry lockfiles are valid YAML documents, unlike Yarn v1's custom non-YAML syntax. The `yarn-lock-parser` crate only implements v1 parsing. No Rust crate existed for Berry format. Additionally, the YAML ecosystem has shifting maintenance: `serde_yaml` deprecated, `serde_yml` deprecated, requiring discovery of the maintained `serde_norway` fork.

## Solution

Added `yarn_berry.rs` module implementing `YarnBerryLockfile` struct with:

**1. Auto-detection in `parse_lockfile()`:**

```rust
pub fn parse_lockfile(content: &str) -> Result<Box<dyn Lockfile>, LockfileError> {
    if content.contains("__metadata:") {
        YarnBerryLockfile::parse(content).map(|lf| Box::new(lf) as Box<dyn Lockfile>)
    } else {
        Yarn1Lockfile::parse(content).map(|lf| Box::new(lf) as Box<dyn Lockfile>)
    }
}
```

**2. YAML parsing with `serde_norway`:**

```rust
use serde::Deserialize;

pub fn parse(content: &str) -> Result<Self, LockfileError> {
    let document = serde_norway::from_str::<serde_norway::Mapping>(content)
        .map_err(|error| LockfileError::Parse(error.to_string()))?;
    
    let metadata_value = document
        .get(serde_norway::Value::String("__metadata".to_string()))
        .ok_or_else(|| LockfileError::Parse("missing __metadata block".to_string()))?;
    
    let _: YarnBerryMetadata = serde_norway::from_value(metadata_value.clone())
        .map_err(|error| LockfileError::Parse(error.to_string()))?;
    // ...
}
```

**3. Tolerant entry deserialization:**

```rust
#[derive(Debug, Clone, Deserialize)]
struct YarnBerryRawEntry {
    version: String,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
}
// Ignores unknown fields (resolution, checksum, languageName, etc.)
```

**4. Descriptor key splitting:**

```rust
for descriptor in key.split(", ") {
    entries_by_descriptor.insert(descriptor.to_string(), normalized_entry.clone());
}
```

**5. Resolution with protocol handling:**

```rust
fn candidate_keys(name: &str, version: &str) -> Vec<String> {
    let mut candidates = Vec::with_capacity(2);
    if version.contains(':') {
        candidates.push(format!("{name}@{version}"));  // Already has protocol
    } else {
        candidates.push(format!("{name}@npm:{version}"));  // Default npm:
        candidates.push(format!("{name}@{version}"));  // Fallback
    }
    candidates
}
```

**Dependencies added:**
```toml
serde_norway = { workspace = true }  # Use maintained YAML crate
```

## Why This Works

1. **`serde_norway` is the maintained fork**: The original `serde_yaml` maintainer archived the project. `serde_norway` (0.9.42+) is actively maintained and API-compatible for basic use cases.

2. **Tolerant deserialization**: `#[serde(default)]` on dependencies and ignoring unknown fields allows parsing only what we need without breaking on new Berry versions adding fields.

3. **Protocol prefix preservation**: `all_dependencies()` returns raw `npm:`-prefixed ranges so values round-trip through `resolve_package()` without transformation.

4. **Comma-space split**: The `", "` delimiter is canonical in Berry lockfiles. Splitting on `,` alone would break scoped packages (`@babel/core`) or version ranges containing commas.

## Prevention Strategies

**Test Cases:**
- Parse realistic Berry fixture with metadata, multiple descriptors per entry
- Auto-detection: Berry fixture routes to Berry parser, v1 fixture routes to v1 parser
- Protocol handling: bare range defaults to `npm:`, prefixed range used as-is
- Error cases: malformed YAML, missing `__metadata`, entry without version

**Best Practices:**
- Use `serde_norway` for YAML in new projects — `serde_yaml` and `serde_yml` are deprecated
- Split descriptor keys on `", "` (comma-space), never just `,`
- Preserve protocol prefixes in dependency ranges for round-tripping
- Implement tolerant deserialization with `#[serde(default)]` to future-proof against format additions
- Use substring detection (`__metadata:`) for format auto-detection when formats have distinct markers

**Code Review Checklist:**
- [ ] YAML crate is `serde_norway` (not deprecated alternatives)?
- [ ] Entry deserialization ignores unknown fields?
- [ ] Descriptor splitting uses `", "` delimiter?
- [ ] Resolution handles both bare and protocol-prefixed ranges?
- [ ] Dependency values preserve protocol prefixes for round-tripping?
- [ ] Auto-detection uses distinct Berry marker?

## Gotchas

1. **`serde_yaml`/`serde_yml` deprecated**: Both show deprecation warnings. Must use `serde_norway` for maintained YAML support.

2. **Comma-space delimiter**: Berry joins descriptors with `", "` (comma-space). Splitting on `,` breaks on scoped packages like `@babel/core@npm:^7.0.0, @babel/core@npm:^7.22.0`.

3. **Workspace protocol variations**: Berry uses `workspace:*`, `workspace:.`, `workspace:packages/foo`. Parser handles prefix but does not normalize variations.

4. **Metadata version not validated**: Parser reads version (4=v2, 6=v3, 8=v4) but doesn't reject unsupported versions. Forward-compatible by design.

5. **Auto-detection heuristic**: `__metadata:` substring check could misroute Yarn v1 content containing that literal. Berry format requires the marker, making false positives unlikely but not impossible.

## Related Issues

- **GitHub:** [dobesv/luchta#11](https://github.com/dobesv/luchta/issues/11) — Support the latest yarn berry lockfile format
- **Plan:** `yarn-berry-lockfile`
