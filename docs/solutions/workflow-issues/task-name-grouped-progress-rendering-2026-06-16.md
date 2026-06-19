---
title: "Task-name-grouped running-task list with CodeScene-whack-a-mole test patterns"
date: 2026-06-16
category: workflow-issues
problem_type: workflow_issue
component: luchta-cli/progress-reporter
root_cause: "Scope-based grouping was misaligned with task-centric mental model; test refactor triggered CodeScene rule chain"
resolution_type: code_fix
severity: medium
tags:
  - progress-reporting
  - task-grouping
  - display-formatting
  - codescene
  - test-refactoring
  - chainable-asserter
plan_ref: issue-68-console-output
---

## Problem

Running-task list grouped by npm scope (`@acme/web`, `@acme/api`) when users expected task-centric grouping (e.g., `{web,api}:lint`). Status line used verbose text without skip/wave counts. Test refactoring triggered CodeScene rule cascade: Large Method → method extraction → String-Heavy Function Arguments → Large Assertion Blocks.

## Symptoms

- Running list output: `@acme/{web,api}:build, web:{test,tsc}` — scope-first, task-second
- User confusion: tasks with same name scattered across groups
- `cs delta origin/HEAD` red with multiple CodeScene violations after naive test helper extraction
- Status line: `2/5 done · 1 skipped · 1 running · 12s` — verbose, no wave indicator

## Investigation Steps

Traced `render_running_task_groups()` through scope-based path. Found `render_scoped_tasks()` and `render_mixed_scope_tasks()` grouping by `package_scope()` result.

Read plan note 0e7d2d72: spec ambiguity where example showed `e:babel` but algorithm said TaskId Display. Resolution: algorithm authoritative; single leftover uses `e#babel` (TaskId Display contract).

Analyzed CodeScene violations on test file:
1. Large Method (test function too long)
2. Extracted helper functions with 4+ `&str` params → String-Heavy Function Arguments
3. Helpers called from few tests → Low Cohesion (LCOM4)
4. Duplicated setup across tests → Code Duplication
5. Nested match in loops → Bumpy-Road

## Root Cause

**Grouping**: Scope-based grouping served monorepo convention but misaligned with task-centric workflow. Users think "all lint tasks" not "all @acme packages".

**CodeScene whack-a-mole**: Naive extraction of assertion helpers traded one smell for another:
- `Large Method` → split test → `Code Duplication` (same setup in both halves)
- Helper with multiple `&str` params → `String-Heavy Function Arguments`
- Helper called from few tests → `Low Cohesion`
- Loop with nested match → `Bumpy-Road`

## Solution

### 1. Two-Pass Task-Name Grouping Algorithm

Replaced scope-based grouping with task-name-first:

```rust
fn render_running_task_groups(shown: &[&TaskId]) -> String {
    let (mut rendered, consumed) = group_by_shared_task_name(shown);
    rendered.extend(group_remaining_by_package(shown, &consumed));
    rendered.join(", ")
}
```

**Pass 1: Shared task names**

```rust
fn group_by_shared_task_name(shown: &[&TaskId]) -> (Vec<String>, Vec<bool>) {
    let mut tasks_by_name: BTreeMap<&str, Vec<(usize, &TaskId)>> = BTreeMap::new();
    for (index, task) in shown.iter().copied().enumerate() {
        tasks_by_name
            .entry(task.task.as_ref())
            .or_default()
            .push((index, task));
    }

    let mut consumed = vec![false; shown.len()];
    let mut rendered = Vec::new();
    for (task_name, tasks) in tasks_by_name {
        let packages = shared_task_name_packages(&tasks);
        if packages.len() < 2 {
            continue;  // Single package → pass 2
        }

        rendered.push(format!("{}:{}", format_package_set(&packages), task_name));
        mark_consumed(&mut consumed, &tasks);
    }

    (rendered, consumed)
}
```

Only groups where 2+ packages share task name. Root package excluded (masked via `!task.package.is_root()`).

**Pass 2: Remaining by package**

```rust
fn group_remaining_by_package(shown: &[&TaskId], consumed: &[bool]) -> Vec<String> {
    let mut tasks_by_package: BTreeMap<&str, Vec<&TaskId>> = BTreeMap::new();
    for (index, task) in shown.iter().copied().enumerate() {
        if consumed[index] { continue; }
        tasks_by_package
            .entry(task.package.as_str())
            .or_default()
            .push(task);
    }

    tasks_by_package
        .into_values()
        .map(render_package_group)
        .collect()
}

fn render_package_group(mut tasks: Vec<&TaskId>) -> String {
    tasks.sort_by_key(|task| task.task.to_string());
    if tasks.len() == 1 {
        return tasks[0].to_string();  // TaskId Display: `pkg#task` or `#task`
    }

    let names = tasks.iter().map(|task| task.task.to_string()).collect::<Vec<_>>().join(",");

    // Root sentinel masked: `#{...}` not `//root:{...}`
    if tasks[0].package.is_root() {
        format!("#{{{names}}}")
    } else {
        format!("{}:{{{names}}}", tasks[0].package.as_str())
    }
}
```

**Common scope factoring**

```rust
fn format_package_set(packages: &BTreeSet<&str>) -> String {
    if let Some(scope) = common_scope(packages) {
        let inner = packages.iter()
            .map(|p| p.trim_start_matches(scope).trim_start_matches('/'))
            .collect::<Vec<_>>()
            .join(",");
        format!("{scope}/{{{inner}}}")
    } else {
        format!("{{{}}}", packages.iter().copied().collect::<Vec<_>>().join(","))
    }
}

fn common_scope<'a>(packages: &BTreeSet<&'a str>) -> Option<&'a str> {
    let mut scopes = packages.iter().map(|p| scope_of(p));
    let first = scopes.next().flatten()?;
    scopes.all(|s| s == Some(first)).then_some(first)
}

fn scope_of(package: &str) -> Option<&str> {
    if !package.starts_with('@') { return None; }
    package.rsplit_once('/').map(|(scope, _)| scope)
}
```

Result: `{web,api,cli}:lint, platform:{test,build}, app#custom`.

### 2. Emoji Status/Done Line

```rust
// Status line (render_progress)
let done_or_skipped = done + skipped;
let pending = total_tasks.saturating_sub(done_or_skipped + running_count);
let waves_done = self.wave_total.iter().enumerate()
    .filter(|(i, total)| {
        **total > 0 &&
        self.wave_done[*i].load(Ordering::SeqCst) +
        self.wave_skipped[*i].load(Ordering::SeqCst) == **total
    })
    .count();

let mut parts = vec![
    format!("✔ {}/{}", done_or_skipped, total_tasks),
    format!("⏭️ {}", skipped),
];
if pending > 0 { parts.push(format!("⌛ {}", pending)); }
if running_count > 0 {
    parts.push(format!("🏃{}", running_count));
    parts.push(format!("({})", render_running_task_list(running)));
}
parts.extend([
    format!("⏱️ {}s", elapsed),
    format!("🐏 {}", rss_formatted),
    format!("🌊 {} / {}", waves_done, self.total_waves),
]);
parts.join(" ")
```

Omit `⌛`/`🏃` when 0. Done numerator = done + skipped (equals total at completion).

### 3. Chainable Asserter Pattern for Tests

Instead of multiple free helper functions with `&str` params:

```rust
// WRONG: String-Heavy, triggers CodeScene
fn assert_done_line(stdout: &str, done: usize, total: usize, skipped: usize) { ... }
fn assert_no_wave_progress(stdout: &str, stderr: &str) { ... }
fn assert_success(stdout: &str, stderr: &str, status: ExitStatus) { ... }
```

Use single struct with chainable methods:

```rust
struct DoneLine {
    done: usize,
    total: usize,
    skipped: usize,
    waves: usize,
}

struct ProgressOutput {
    label: String,
    stdout: String,
    stderr: String,
}

impl ProgressOutput {
    fn new(label: &str, output: &std::process::Output) -> Self { ... }

    fn assert_success(&self, status: ExitStatus) -> &Self {
        assert!(status.success(), "{} should succeed, stderr: {}", self.label, self.stderr);
        self
    }

    fn assert_done_line(&self, expected: DoneLine) -> &Self {
        let token = format!("✔ {}/{} ⏭️ {}", expected.done, expected.total, expected.skipped);
        assert!(self.stdout.contains(&token), "{} missing '{}', got: {}", self.label, token, self.stdout);
        self
    }

    fn assert_no_wave_progress(&self) -> &Self {
        for stream in [&self.stdout, &self.stderr] {
            assert!(!stream.contains("Wave "), "{} should not emit wave progress", self.label);
        }
        self
    }

    fn assert_no_per_task_spam(&self) -> &Self { ... }
}
```

**Shared setup helper with internal assertion**

```rust
fn run_build(
    temp: &assert_fs::TempDir,
    worker_body: &str,
    tasks_json: &str,
    summary_mode: bool,
    label: &str,
    extra_env: &[(&str, &str)],
) -> ProgressOutput {
    // ... setup ...
    let output = cmd.output().expect("run command");
    let progress = ProgressOutput::new(label, &output);
    progress.assert_success(output.status);  // Internal assertion
    progress
}
```

Tests collapse to:

```rust
let out = run_build(&temp, worker_body, tasks, true, "summary mode", &[]);
out.assert_done_line(DoneLine { done: 2, total: 2, skipped: 0, waves: 2 })
    .assert_no_wave_progress()
    .assert_no_per_task_spam();
```

### 4. Flatten Bumpy-Road Patterns

```rust
// WRONG: Bumpy-Road (nested match in loop)
for warning in warnings {
    match warning {
        PressureReason::UsageHigh => { ... }
        PressureReason::FreeLow => { ... }
    }
}

// BETTER: Iterator chain
fn pressure_suffix(warnings: &[PressureReason]) -> String {
    let has_usage = warnings.iter().any(|w| matches!(w, PressureReason::UsageHigh));
    let has_free = warnings.iter().any(|w| matches!(w, PressureReason::FreeLow));

    let mut suffix = String::new();
    if has_usage { suffix.push_str(" ⚠️ mem usage high"); }
    if has_free { suffix.push_str(" ⚠️ system free memory low"); }
    suffix
}
```

## Why This Works

**Task-name grouping**: Matches mental model. Users think "run all lint" not "run @acme packages". BTreeMap ensures deterministic output.

**Root sentinel masking**: `//root` is internal implementation detail. TaskId Display contract (`#task`) exposes user-facing syntax. Never leak sentinel.

**Common scope factoring**: `@acme/{web,api}:lint` is shorter than `{@acme/web,@acme/api}:lint`. Preserves mental model when all packages share scope.

**Chainable asserter**: Single struct parameter dodges String-Heavy Function Arguments. `&Self` returns enable fluent chaining. Setup helper with internal assertion eliminates duplication.

**Iterator chains**: Replace loop+match with `any()`, `filter()`, `map()` — avoids Bumpy-Road violation.

## Prevention Strategies

**Test Refactoring (CodeScene interactions):**

- [ ] Use struct-based asserters, not multi-`&str` helpers
- [ ] Shared setup helper includes internal success assertion
- [ ] Flatten nested match-in-loop to iterator chains
- [ ] When splitting Large Method creates Code Duplication, prefer shared setup over test duplication
- [ ] Residual duplication than can't be eliminated without re-merging scenarios: flag for human review

**Progress Rendering:**

- [ ] Group by task-name first (cross-package), then by package (remaining)
- [ ] Mask root sentinel: use TaskId Display (`#task`) not package name (`//root`)
- [ ] Factor out common npm scope when all packages share it
- [ ] BTreeMap for deterministic ordering

**Display Contract:**

- [ ] TaskId Display renders `pkg#task`; root renders `#task`
- [ ] Grouping code uses `to_string()` for single leftovers
- [ ] Never expose `//root` sentinel in output

## Related Issues

- **GitHub:** [#68](https://github.com/dobesv/luchta/issues/68) — Console output improvements
- **Related Solution:** [wave-bucketed-progress-reporter-2026-06-13.md](wave-bucketed-progress-reporter-2026-06-13.md) — Original ProgressReporter design
- **Related Solution:** [codescene-quality-score-refactoring-2026-06-09.md](codescene-quality-score-refactoring-2026-06-09.md) — General CodeScene remediation
- **Related Solution:** [cli-package-targeting-codescene-whack-a-mole-2026-06-15.md](../logic-errors/cli-package-targeting-codescene-whack-a-mole-2026-06-15.md) — CodeScene rule chain on CLI targeting
