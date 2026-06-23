//! Environment variable conflict detection for `luchta check`.
//!
//! A conflict is defined as an `EnvSpec` with BOTH `value.is_some()` AND `default.is_some()`
//! (i.e., `set` + `setDefault` on the same variable) within a single scope's env map.
//! Distinct modes across different scopes are NOT conflicts (that's intended precedence).

use std::collections::{BTreeMap, HashMap};

use luchta_types::{EnvSpec, TaskDefinition, TaskName, WorkerDefinition};

/// Represents a single env conflict detected during check validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvConflict {
    /// The environment variable name that has conflicting modes.
    pub var_name: String,
    /// Human-readable scope label: "global", "worker '<name>'", or "task '<taskname>'".
    pub scope_label: String,
}

impl EnvConflict {
    /// Creates a conflict report for a global env variable.
    pub fn global(var_name: impl Into<String>) -> Self {
        Self {
            var_name: var_name.into(),
            scope_label: "global".to_owned(),
        }
    }

    /// Creates a conflict report for a worker env variable.
    pub fn worker(var_name: impl Into<String>, worker_name: &str) -> Self {
        Self {
            var_name: var_name.into(),
            scope_label: format!("worker '{}'", worker_name),
        }
    }

    /// Creates a conflict report for a task env variable.
    pub fn task(var_name: impl Into<String>, task_name: &TaskName) -> Self {
        Self {
            var_name: var_name.into(),
            scope_label: format!("task '{}'", task_name),
        }
    }

    /// Formats this conflict as a miette diagnostic report.
    pub fn to_diagnostic(&self) -> miette::Report {
        miette::miette!(
            "env variable '{}' in {} sets both `value` and `default` (set + setDefault conflict)",
            self.var_name,
            self.scope_label
        )
    }
}

/// Scans all scopes for env conflicts and returns a list of conflicts found.
///
/// Scans in deterministic order: global, then workers sorted by name, then tasks sorted by name.
/// Within each scope, variables are iterated in BTreeMap order (already sorted).
pub fn detect_env_conflicts(
    global_env: &BTreeMap<String, EnvSpec>,
    workers: &HashMap<String, WorkerDefinition>,
    pipeline: &HashMap<TaskName, TaskDefinition>,
) -> Vec<EnvConflict> {
    let mut conflicts = Vec::new();

    // 1. Scan global env
    for (var_name, spec) in global_env {
        if has_conflict(spec) {
            conflicts.push(EnvConflict::global(var_name));
        }
    }

    // 2. Scan workers in sorted order
    let mut worker_names: Vec<_> = workers.keys().collect();
    worker_names.sort();
    for worker_name in worker_names {
        let worker = &workers[worker_name];
        for (var_name, spec) in &worker.env {
            if has_conflict(spec) {
                conflicts.push(EnvConflict::worker(var_name, worker_name));
            }
        }
    }

    // 3. Scan tasks in sorted order (by name string)
    let mut task_names: Vec<_> = pipeline.keys().collect();
    task_names.sort_by_key(|name| name.as_str());
    for task_name in task_names {
        let task = &pipeline[task_name];
        for (var_name, spec) in &task.env {
            if has_conflict(spec) {
                conflicts.push(EnvConflict::task(var_name, task_name));
            }
        }
    }

    conflicts
}

/// Returns true if an EnvSpec has both value and default set (conflict condition).
fn has_conflict(spec: &EnvSpec) -> bool {
    spec.value.is_some() && spec.default.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_conflicting_spec() -> EnvSpec {
        EnvSpec {
            value: Some("explicit".to_owned()),
            default: Some("fallback".to_owned()),
            input: true,
        }
    }

    fn make_value_only_spec() -> EnvSpec {
        EnvSpec {
            value: Some("explicit".to_owned()),
            default: None,
            input: true,
        }
    }

    fn make_default_only_spec() -> EnvSpec {
        EnvSpec {
            value: None,
            default: Some("fallback".to_owned()),
            input: true,
        }
    }

    fn make_empty_spec() -> EnvSpec {
        EnvSpec {
            value: None,
            default: None,
            input: true,
        }
    }

    fn worker_with_conflict(var_name: &str) -> WorkerDefinition {
        let mut env = BTreeMap::new();
        env.insert(var_name.to_owned(), make_conflicting_spec());
        WorkerDefinition {
            command: "echo".to_owned(),
            depends_on: vec![],
            env,
            cache: None,
        }
    }

    fn task_with_conflict(var_name: &str) -> TaskDefinition {
        let mut env = BTreeMap::new();
        env.insert(var_name.to_owned(), make_conflicting_spec());
        TaskDefinition {
            depends_on: vec![],
            weight: 1,
            command: None,
            worker: None,
            cache: None,
            inputs: vec![],
            outputs: vec![],
            env,
        }
    }

    #[test]
    fn detects_conflict_in_global_env() {
        let mut global_env = BTreeMap::new();
        global_env.insert("VAR1".to_owned(), make_conflicting_spec());
        global_env.insert("VAR2".to_owned(), make_value_only_spec());

        let conflicts = detect_env_conflicts(&global_env, &HashMap::new(), &HashMap::new());

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].var_name, "VAR1");
        assert_eq!(conflicts[0].scope_label, "global");
    }

    #[test]
    fn detects_conflict_in_worker_env() {
        let mut workers = HashMap::new();
        let mut worker_env = BTreeMap::new();
        worker_env.insert("WORKER_VAR".to_owned(), make_conflicting_spec());
        workers.insert(
            "my-worker".to_owned(),
            WorkerDefinition {
                command: "echo".to_owned(),
                depends_on: vec![],
                env: worker_env,
                cache: None,
            },
        );

        let conflicts = detect_env_conflicts(&BTreeMap::new(), &workers, &HashMap::new());

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].var_name, "WORKER_VAR");
        assert_eq!(conflicts[0].scope_label, "worker 'my-worker'");
    }

    #[test]
    fn detects_conflict_in_task_env() {
        let mut pipeline = HashMap::new();
        let mut task_env = BTreeMap::new();
        task_env.insert("TASK_VAR".to_owned(), make_conflicting_spec());
        pipeline.insert(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![],
                weight: 1,
                command: Some("echo build".to_owned()),
                worker: Some("default".to_owned()),
                cache: None,
                inputs: vec![],
                outputs: vec![],
                env: task_env,
            },
        );

        let conflicts = detect_env_conflicts(&BTreeMap::new(), &HashMap::new(), &pipeline);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].var_name, "TASK_VAR");
        assert_eq!(conflicts[0].scope_label, "task 'build'");
    }

    #[test]
    fn no_conflicts_when_value_only() {
        let mut global_env = BTreeMap::new();
        global_env.insert("VAR".to_owned(), make_value_only_spec());

        let conflicts = detect_env_conflicts(&global_env, &HashMap::new(), &HashMap::new());

        assert!(conflicts.is_empty());
    }

    #[test]
    fn no_conflicts_when_default_only() {
        let mut global_env = BTreeMap::new();
        global_env.insert("VAR".to_owned(), make_default_only_spec());

        let conflicts = detect_env_conflicts(&global_env, &HashMap::new(), &HashMap::new());

        assert!(conflicts.is_empty());
    }

    #[test]
    fn no_conflicts_when_empty() {
        let mut global_env = BTreeMap::new();
        global_env.insert("VAR".to_owned(), make_empty_spec());

        let conflicts = detect_env_conflicts(&global_env, &HashMap::new(), &HashMap::new());

        assert!(conflicts.is_empty());
    }

    #[test]
    fn detects_multiple_conflicts_in_deterministic_order() {
        // Global: Z_VAR, A_VAR (BTreeMap sorts to A_VAR, Z_VAR)
        let mut global_env = BTreeMap::new();
        global_env.insert("Z_GLOBAL".to_owned(), make_conflicting_spec());
        global_env.insert("A_GLOBAL".to_owned(), make_conflicting_spec());

        // Workers: "worker-b", "worker-a" (sorted to worker-a, worker-b)
        let mut workers = HashMap::new();
        workers.insert("worker-a".to_owned(), worker_with_conflict("A_WORKER"));
        workers.insert("worker-b".to_owned(), worker_with_conflict("B_WORKER"));

        // Tasks: "test", "build" (sorted to build, test)
        let mut pipeline = HashMap::new();
        pipeline.insert(TaskName::from("build"), task_with_conflict("BUILD_VAR"));
        pipeline.insert(TaskName::from("test"), task_with_conflict("TEST_VAR"));

        let conflicts = detect_env_conflicts(&global_env, &workers, &pipeline);

        // Expected order:
        // 1. Global (sorted by var name): A_GLOBAL, Z_GLOBAL
        // 2. Workers (sorted by worker name): worker-a/A_WORKER, worker-b/B_WORKER
        // 3. Tasks (sorted by task name): build/BUILD_VAR, test/TEST_VAR
        assert_eq!(conflicts.len(), 6);
        assert_eq!(conflicts[0], EnvConflict::global("A_GLOBAL"));
        assert_eq!(conflicts[1], EnvConflict::global("Z_GLOBAL"));
        assert_eq!(conflicts[2], EnvConflict::worker("A_WORKER", "worker-a"));
        assert_eq!(conflicts[3], EnvConflict::worker("B_WORKER", "worker-b"));
        assert_eq!(
            conflicts[4],
            EnvConflict::task("BUILD_VAR", &TaskName::from("build"))
        );
        assert_eq!(
            conflicts[5],
            EnvConflict::task("TEST_VAR", &TaskName::from("test"))
        );
    }

    #[test]
    fn distinct_modes_across_scopes_not_flagged() {
        // Same var name, different scopes, each with only one mode set - NOT a conflict
        let mut global_env = BTreeMap::new();
        global_env.insert("SHARED_VAR".to_owned(), make_value_only_spec());

        let mut workers = HashMap::new();
        let mut worker_env = BTreeMap::new();
        worker_env.insert("SHARED_VAR".to_owned(), make_default_only_spec());
        workers.insert(
            "my-worker".to_owned(),
            WorkerDefinition {
                command: "echo".to_owned(),
                depends_on: vec![],
                env: worker_env,
                cache: None,
            },
        );

        let mut pipeline = HashMap::new();
        let mut task_env = BTreeMap::new();
        task_env.insert("SHARED_VAR".to_owned(), make_empty_spec());
        pipeline.insert(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![],
                weight: 1,
                command: None,
                worker: None,
                cache: None,
                inputs: vec![],
                outputs: vec![],
                env: task_env,
            },
        );

        let conflicts = detect_env_conflicts(&global_env, &workers, &pipeline);

        // No conflicts - each scope has only one mode set per var
        assert!(conflicts.is_empty());
    }

    #[test]
    fn to_diagnostic_formats_correctly() {
        let conflict = EnvConflict::task("MY_VAR", &TaskName::from("build"));
        let report = conflict.to_diagnostic();

        let message = report.to_string();
        assert!(message.contains("MY_VAR"));
        assert!(message.contains("task 'build'"));
        assert!(message.contains("value"));
        assert!(message.contains("default"));
        assert!(message.contains("set + setDefault conflict"));
    }
}
