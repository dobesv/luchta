use std::collections::HashMap;

use serde::Deserialize;

use crate::TaskDefinition;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
/// Worker command definition shared across crates.
pub struct WorkerDefinition {
    pub command: String,
}

/// Canonical config schema shared across crates.
///
/// Loaded by `luchta-cli` from an executable `luchta-config.*` script that prints
/// this structure as JSON (camelCase fields). Snake_case is also accepted via serde aliases.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LuchtaConfig {
    /// Weighted concurrency settings for executor.
    #[serde(default)]
    pub concurrency: ConcurrencyConfig,
    /// Task definitions keyed by task name.
    #[serde(default, alias = "pipeline")]
    pub tasks: HashMap<String, TaskDefinition>,
    /// Worker definitions keyed by worker name.
    #[serde(default)]
    pub workers: HashMap<String, WorkerDefinition>,
}

/// Scheduler concurrency settings from `[concurrency]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ConcurrencyConfig {
    /// Global maximum cumulative task weight allowed to run at once.
    #[serde(rename = "maxWeight", alias = "max_weight")]
    pub max_weight: u32,
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self { max_weight: 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConcurrencyConfig, LuchtaConfig};
    use crate::{DependsOn, PackageName, TaskDefinition, TaskId, TaskName, WorkerDefinition};

    #[test]
    fn deserializes_luchta_toml_pipeline_with_defaults_and_commands() {
        let sample = r#"
            [concurrency]
            max_weight = 8

            [pipeline.build]
            depends_on = ["^build"]
            weight = 3

            [pipeline.test]
            depends_on = ["build", "ui#build"]
            command = "vitest run"

            [pipeline.lint]

            [pipeline.bundle]
            depends_on = ["^^build"]
        "#;

        let config: LuchtaConfig = toml::from_str(sample).expect("config should deserialize");

        assert_eq!(config.concurrency, ConcurrencyConfig { max_weight: 8 });

        assert_eq!(
            config.tasks.get("build"),
            Some(&TaskDefinition {
                depends_on: vec![DependsOn::DirectUpstream(TaskName::from("build"))],
                weight: 3,
                command: None,
                worker: None,
            })
        );

        assert_eq!(
            config.tasks.get("test"),
            Some(&TaskDefinition {
                depends_on: vec![
                    DependsOn::SamePackage(TaskName::from("build")),
                    DependsOn::Specific(TaskId::new(
                        PackageName::from("ui"),
                        TaskName::from("build")
                    )),
                ],
                weight: 1,
                command: Some("vitest run".to_owned()),
                worker: None,
            })
        );

        assert_eq!(
            config.tasks.get("lint"),
            Some(&TaskDefinition {
                depends_on: Vec::new(),
                weight: 1,
                command: None,
                worker: None,
            })
        );

        assert_eq!(
            config.tasks.get("bundle"),
            Some(&TaskDefinition {
                depends_on: vec![DependsOn::TransitiveUpstream(TaskName::from("build"))],
                weight: 1,
                command: None,
                worker: None,
            })
        );
    }

    #[test]
    fn deserializes_config_with_tasks_key() {
        let sample = r#"
            [tasks.build]
            weight = 2
        "#;

        let config: LuchtaConfig = toml::from_str(sample).expect("config should deserialize");

        assert_eq!(config.tasks["build"].weight, 2);
    }

    #[test]
    fn deserializes_config_with_pipeline_alias() {
        let sample = r#"
            [pipeline.build]
            weight = 2
        "#;

        let config: LuchtaConfig = toml::from_str(sample).expect("config should deserialize");

        assert_eq!(config.tasks["build"].weight, 2);
    }

    #[test]
    fn deserializes_workers_map() {
        let sample = r#"
            [workers.jest-worker]
            command = "node worker.js"
        "#;

        let config: LuchtaConfig = toml::from_str(sample).expect("config should deserialize");

        assert_eq!(
            config.workers.get("jest-worker"),
            Some(&WorkerDefinition {
                command: "node worker.js".to_owned(),
            })
        );
    }

    #[test]
    fn deserializes_task_worker_field() {
        let sample = r#"
            [tasks.test]
            worker = "jest-worker"
        "#;

        let config: LuchtaConfig = toml::from_str(sample).expect("config should deserialize");

        assert_eq!(config.tasks["test"].worker.as_deref(), Some("jest-worker"));
    }

    #[test]
    fn deserializes_depends_on_from_plain_strings() {
        assert_eq!(
            toml::from_str::<TaskDefinition>("depends_on = [\"^build\"]")
                .expect("direct upstream")
                .depends_on,
            vec![DependsOn::DirectUpstream(TaskName::from("build"))]
        );
        assert_eq!(
            toml::from_str::<TaskDefinition>("depends_on = [\"^^build\"]")
                .expect("transitive upstream")
                .depends_on,
            vec![DependsOn::TransitiveUpstream(TaskName::from("build"))]
        );
        assert_eq!(
            toml::from_str::<TaskDefinition>("depends_on = [\"test\"]")
                .expect("same package")
                .depends_on,
            vec![DependsOn::SamePackage(TaskName::from("test"))]
        );
        assert_eq!(
            toml::from_str::<TaskDefinition>("depends_on = [\"ui#build\"]")
                .expect("specific task")
                .depends_on,
            vec![DependsOn::Specific(TaskId::new(
                PackageName::from("ui"),
                TaskName::from("build"),
            ))]
        );
    }
}
