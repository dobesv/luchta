use std::collections::BTreeMap;

use luchta_types::EnvSpec;

pub(crate) fn merge_env(
    global: &BTreeMap<String, EnvSpec>,
    worker: Option<&BTreeMap<String, EnvSpec>>,
    task: &BTreeMap<String, EnvSpec>,
) -> BTreeMap<String, EnvSpec> {
    let mut merged = global.clone();

    if let Some(worker) = worker {
        merged.extend(worker.clone());
    }

    merged.extend(task.clone());
    merged
}

#[cfg(test)]
mod tests {
    use super::merge_env;
    use luchta_types::EnvSpec;
    use std::collections::BTreeMap;

    fn spec(value: &str) -> EnvSpec {
        EnvSpec {
            value: Some(value.to_string()),
            default: None,
            input: true,
        }
    }

    #[test]
    fn merge_env_keeps_global_only_var() {
        let global = BTreeMap::from([(String::from("GLOBAL"), spec("global"))]);

        let merged = merge_env(&global, None, &BTreeMap::new());

        assert_eq!(merged.get("GLOBAL"), Some(&spec("global")));
    }

    #[test]
    fn merge_env_worker_overrides_global() {
        let global = BTreeMap::from([(String::from("SHARED"), spec("global"))]);
        let worker = BTreeMap::from([(String::from("SHARED"), spec("worker"))]);

        let merged = merge_env(&global, Some(&worker), &BTreeMap::new());

        assert_eq!(merged.get("SHARED"), Some(&spec("worker")));
    }

    #[test]
    fn merge_env_task_overrides_worker() {
        let global = BTreeMap::from([(String::from("SHARED"), spec("global"))]);
        let worker = BTreeMap::from([(String::from("SHARED"), spec("worker"))]);
        let task = BTreeMap::from([(String::from("SHARED"), spec("task"))]);

        let merged = merge_env(&global, Some(&worker), &task);

        assert_eq!(merged.get("SHARED"), Some(&spec("task")));
    }

    #[test]
    fn merge_env_unions_disjoint_scopes() {
        let global = BTreeMap::from([(String::from("GLOBAL"), spec("global"))]);
        let worker = BTreeMap::from([(String::from("WORKER"), spec("worker"))]);
        let task = BTreeMap::from([(String::from("TASK"), spec("task"))]);

        let merged = merge_env(&global, Some(&worker), &task);

        assert_eq!(merged.get("GLOBAL"), Some(&spec("global")));
        assert_eq!(merged.get("WORKER"), Some(&spec("worker")));
        assert_eq!(merged.get("TASK"), Some(&spec("task")));
    }

    #[test]
    fn merge_env_task_overrides_global_without_worker() {
        let global = BTreeMap::from([(String::from("SHARED"), spec("global"))]);
        let task = BTreeMap::from([(String::from("SHARED"), spec("task"))]);

        let merged = merge_env(&global, None, &task);

        assert_eq!(merged.get("SHARED"), Some(&spec("task")));
    }
}
