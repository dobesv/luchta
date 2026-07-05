---
title: "Move worker-provided file inputs to resolve phase"
date: 2026-07-05
category: "logic-errors"
problem_type: logic_error
component: "worker-protocol"
root_cause: "input hash snapshot taken post-execution enabled TOCTOU race; declared-only pre-snapshot in PR #178 was insufficient"
resolution_type: code_fix
severity: high
tags:
  - cache
  - worker
  - input
  - snapshot
  - toctou
plan_ref: "luchta-resolve-phase-inputs"
---

# Problem

Luchta's build cache used to resolve input file hashes for worker-driven tasks AFTER execution, using the inputs reported in the `Done` message. This created a Time-of-Check to Time-of-Use (TOCTOU) race: if an input file was modified during task execution (H1 → H2), the task would produce output based on H1 but the engine would record the H2 hash in the cache. Subsequent runs would see H2 and skip the rebuild, serving stale H1-derived outputs.

A previous attempt (PR #178) tried to fix this by snapshotting *declared* (static configuration) inputs before execution. However, this was incorrect because workers often narrow or expand the input set dynamically (e.g., by parsing `tsconfig.json` or `package.json`). Snapshotting only the declared patterns missed the actual files the worker intended to track.

# Solution

The worker protocol was updated to move input reporting from the run phase to the resolve phase. This allows the engine to capture an authoritative "pre-execution" snapshot of exactly what the worker intends to use as inputs.

## 1. Protocol Changes

- **`TaskModification` (Resolve Phase)**: Added `inputs: Option<Vec<String>>`. If provided, these patterns **fully replace** the task's declared inputs.
- **`Done` (Run Phase)**: Removed the `inputs` field. `Done` now only carries `exit_code` and `outputs`.

## 2. Replace Semantics

Worker-provided resolve inputs use **REPLACE** semantics. The worker is responsible for providing the complete set of patterns to track, including any declared patterns it still wants to include. This ensures the engine has a definitive list before dispatching the task.

## 3. Engine Implementation

- During `TaskGraph::build_resolved`, the engine applies `TaskModification.inputs` to the task definition before the task is dispatched.
- The existing pre-execution snapshot mechanism (`resolve_pre_execution_inputs`) now captures the hashes of the worker-provided input set.
- Post-run, a strict stability check re-hashes the SAME worker-provided inputs. If a mismatch is detected (indicating a concurrent edit), the cache write is skipped and the task remains dirty.

## 4. Worker Updates

All built-in workers were updated to report inputs during the resolve phase:
- **Yarn (Rust)**: Adds `package.json` to the resolve inputs.
- **tsgo (Go)**: Parses `tsconfig.json` at resolve time to determine source inputs.
- **TypeScript Platform Workers**: Babel, ESLint, GraphQL Codegen, Depcheck, Storybook, etc., now calculate their input sets during the `ResolveTask` request.

# Verification

Regression tests were added in `crates/luchta-cli/tests/worker_resolve_inputs_regression.rs` to verify:
- **TOCTOU concurrent-edit**: Modifying an input file mid-run causes the cache write to be skipped and triggers a rerun on the next pass.
- **Input narrowing**: Worker-provided inputs correctly replace declared patterns.
- **Escape-path validation**: Worker-provided inputs are validated against repository boundaries.

All workspace tests passed: `cargo nextest run --workspace --stress-count=5`.

# Links

- **GitHub Issue**: [#157](https://github.com/dobesv/luchta/issues/157)
- **Superseded PR**: [#178](https://github.com/dobesv/luchta/pull/178) (Reverted by [#179](https://github.com/dobesv/luchta/pull/179))
