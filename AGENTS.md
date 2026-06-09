# Agent Guidelines for Luchta

This document provides conventions and guidelines for AI coding agents working on the Luchta project.

## Project Tracking
- This project uses **GitHub Issues** as its primary tracker.

## Project Structure
Luchta is a Cargo workspace with the following crate layout:
- `crates/luchta-types`: Core data structures and types.
- `crates/luchta-lockfiles`: Lockfile parsing and abstraction.
- `crates/luchta-workspace`: Workspace and package discovery.
- `crates/luchta-engine`: Graph logic and execution engine.
- `crates/luchta-cli`: CLI interface and configuration.

## Key Architectural Decisions
Agents must respect these fundamental design choices:
- **Runtime:** Uses `tokio` for async I/O-bound process spawning. **Rayon is explicitly excluded.**
- **Concurrency Model:** Implements **weight-based concurrency** using `tokio::sync::Semaphore::acquire_many_owned(weight)`.
- **Graph Logic:** Uses `petgraph::DiGraph` and `petgraph::algo::toposort` for cycle detection.
- **Dual-Graph Separation:** Maintains separate **Package Graph** (package topology) and **Task Graph** (task execution units).
- **Lockfile Abstraction:** All lockfile interactions must go through the `Lockfile` trait.
- **Error Handling:**
    - Use `thiserror` for library crates (`luchta-types`, `luchta-lockfiles`, `luchta-workspace`, `luchta-engine`).
    - Use `miette` only in the `luchta-cli` for user-facing diagnostics.
- **Configuration:** Primary configuration is `luchta.toml` (TOML).

## Validation Commands
Before submitting changes, ensure the following commands pass:
```bash
cargo check --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

## Conventions
- **No `target/`:** Never commit `target/` directories.
- **Error Types:** Library errors should be clear and descriptive using `thiserror`.
- **Async Traits:** Use native stable `async fn` in traits where possible (refer to `luchta-engine` for specific patterns).
