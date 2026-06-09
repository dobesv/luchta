# Luchta

Luchta is a Rust-based alternative to Microsoft's Lage build system, specifically designed for JavaScript/TypeScript (yarn) monorepos. The project is named after Luchta, the Irish god of woodwork, reflecting its role in crafting and assembling complex software projects.

**Status:** Early-stage / Work-in-Progress (WIP).

## Overview

Luchta optimizes monorepo workflows by:
- Discovering yarn workspace packages.
- Building a **Package Graph** for dependency topology.
- Constructing a **Task Graph** (e.g., `ui#build`) for granular execution.
- Executing tasks in topological order with **weight-based concurrency** to manage resources like RAM.

## Crate Layout

The project is organized into a multi-crate Cargo workspace under `crates/`:

- `luchta-types`: Shared types such as `PackageName`, `TaskId`, and `TaskDefinition`.
- `luchta-lockfiles`: `Lockfile` trait abstraction and Yarn v1 implementation.
- `luchta-workspace`: Workspace discovery and Package Graph construction.
- `luchta-engine`: Task Graph construction and the weighted task executor.
- `luchta-cli`: Entry point, `clap` CLI, and executable config script loading.

## Development

### Building and Testing

To build the entire workspace:
```bash
cargo build
```

To run all tests:
```bash
cargo test
```

To build and run the CLI:
```bash
cargo build -p luchta-cli
./target/debug/luchta --help
```

## Usage Sketch

Luchta is configured via an executable script at the workspace root matching `luchta-config.*` (e.g., `.ts`, `.js`, `.sh`, `.py`). 

The script **must** have a shebang line and print its configuration to `stdout` as a JSON object with `camelCase` fields. Luchta executes the script directly and parses this JSON to load the pipeline definition.

Example `luchta-config.ts`:
```typescript
#!/usr/bin/env node
console.log(JSON.stringify({ 
  pipeline: { 
    build: { 
      dependsOn: ["^build"], 
      weight: 2 
    } 
  }, 
  concurrency: { 
    maxWeight: 10 
  } 
}));
```

```bash
# Run the build task for all relevant packages
luchta run build
```

### `dependsOn` Syntax
Luchta supports flexible dependency definitions between tasks:
- `^task`: Direct upstream packages' task.
- `^^task`: Transitive upstream packages' task.
- `task`: Same-package task.
- `pkg#task`: Specific package and task.

## Roadmap

- **Phase 1 (Current):** Multi-crate workspace skeleton and CI setup.
- **Phase 2:** Foundation libraries (workspace discovery, lockfile parsing, graph construction, weighted parallel execution).
- **Future:** Caching (blake3 hashing) and advanced features.
