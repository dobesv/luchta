use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Deserializer};

use crate::{DependsOn, EnvSpec, TaskDefinition};

#[derive(Debug, Clone, PartialEq, Eq)]
/// Worker command definition shared across crates.
pub struct WorkerDefinition {
    pub command: String,
    pub depends_on: Vec<DependsOn>,
    pub env: BTreeMap<String, EnvSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WorkerDefinitionRepr {
    Command(String),
    Object {
        command: String,
        #[serde(default, rename = "dependsOn", alias = "depends_on")]
        depends_on: Vec<DependsOn>,
        #[serde(default)]
        env: BTreeMap<String, EnvSpec>,
    },
}

impl<'de> Deserialize<'de> for WorkerDefinition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match WorkerDefinitionRepr::deserialize(deserializer)? {
            WorkerDefinitionRepr::Command(command) => Ok(Self {
                command,
                depends_on: Vec::new(),
                env: BTreeMap::new(),
            }),
            WorkerDefinitionRepr::Object {
                command,
                depends_on,
                env,
            } => Ok(Self {
                command,
                depends_on,
                env,
            }),
        }
    }
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
    #[serde(default)]
    pub tasks: HashMap<String, TaskDefinition>,
    /// Worker definitions keyed by worker name.
    #[serde(default)]
    pub workers: HashMap<String, WorkerDefinition>,
    /// Environment variables provided to all tasks, keyed by variable name.
    #[serde(default)]
    pub env: BTreeMap<String, EnvSpec>,
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
        let max_weight = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);
        Self { max_weight }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{ConcurrencyConfig, LuchtaConfig};
    use crate::{
        DependsOn, EnvSpec, PackageName, TaskDefinition, TaskId, TaskName, WorkerDefinition,
    };

    #[test]
    fn deserializes_luchta_toml_tasks_with_defaults_and_commands() {
        let sample = r#"
            [concurrency]
            max_weight = 8

            [tasks.build]
            depends_on = ["^build"]
            weight = 3

            [tasks.test]
            depends_on = ["build", "ui#build"]
            command = "vitest run"

            [tasks.lint]

            [tasks.bundle]
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
                ..Default::default()
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
                ..Default::default()
            })
        );

        assert_eq!(
            config.tasks.get("lint"),
            Some(&TaskDefinition {
                depends_on: Vec::new(),
                weight: 1,
                command: None,
                worker: None,
                ..Default::default()
            })
        );

        assert_eq!(
            config.tasks.get("bundle"),
            Some(&TaskDefinition {
                depends_on: vec![DependsOn::TransitiveUpstream(TaskName::from("build"))],
                weight: 1,
                command: None,
                worker: None,
                ..Default::default()
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
                depends_on: vec![],
                env: BTreeMap::new(),
            })
        );
    }

    #[test]
    fn deserializes_worker_object_with_depends_on() {
        let sample = r##"
            [workers.babel]
            command = "luchta-babel-worker"
            dependsOn = ["luchta-workers#build", "#prep"]
        "##;

        let config: LuchtaConfig = toml::from_str(sample).expect("config should deserialize");

        assert_eq!(
            config.workers.get("babel"),
            Some(&WorkerDefinition {
                command: "luchta-babel-worker".to_owned(),
                depends_on: vec![
                    DependsOn::Specific(TaskId::new(
                        PackageName::from("luchta-workers"),
                        TaskName::from("build"),
                    )),
                    DependsOn::Root(TaskName::from("prep")),
                ],
                env: BTreeMap::new(),
            })
        );
    }

    #[test]
    fn deserializes_worker_object_with_snake_case_depends_on() {
        let sample = r#"
            [workers.babel]
            command = "luchta-babel-worker"
            depends_on = ["^build"]
        "#;

        let config: LuchtaConfig = toml::from_str(sample).expect("config should deserialize");

        assert_eq!(
            config.workers.get("babel"),
            Some(&WorkerDefinition {
                command: "luchta-babel-worker".to_owned(),
                depends_on: vec![DependsOn::DirectUpstream(TaskName::from("build"))],
                env: BTreeMap::new(),
            })
        );
    }

    #[test]
    fn deserializes_worker_bare_string_definition() {
        let sample = r#"
            [workers]
            babel = "luchta-babel-worker"
        "#;

        let config: LuchtaConfig = toml::from_str(sample).expect("config should deserialize");

        assert_eq!(
            config.workers.get("babel"),
            Some(&WorkerDefinition {
                command: "luchta-babel-worker".to_owned(),
                depends_on: vec![],
                env: BTreeMap::new(),
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
        assert_eq!(
            toml::from_str::<TaskDefinition>("depends_on = [\"#audit-licenses\"]")
                .expect("root task")
                .depends_on,
            vec![DependsOn::Root(TaskName::from("audit-licenses"))]
        );
    }

    #[test]
    fn deserializes_global_env_worker_env_and_task_env() {
        let sample = r#"
            [env]
            NODE_ENV = { value = "production" }
            API_KEY = {}

            [workers.babel]
            command = "luchta-babel-worker"
            [workers.babel.env]
            BABEL_ENV = { value = "development" }

            [tasks.build]
            command = "npm run build"
            [tasks.build.env]
            BUILD_TARGET = { value = "es2020" }
            OVERRIDE_VAR = { value = "worker-override", input = false }
        "#;

        let config: LuchtaConfig = toml::from_str(sample).expect("config should deserialize");

        // Check global env
        assert_eq!(config.env.len(), 2);
        assert_eq!(
            config.env.get("NODE_ENV"),
            Some(&EnvSpec {
                value: Some("production".to_owned()),
                default: None,
                input: true,
            })
        );
        assert_eq!(
            config.env.get("API_KEY"),
            Some(&EnvSpec {
                value: None,
                default: None,
                input: true,
            })
        );

        // Check worker env
        let babel_worker = config.workers.get("babel").expect("babel worker");
        assert_eq!(babel_worker.env.len(), 1);
        assert_eq!(
            babel_worker.env.get("BABEL_ENV"),
            Some(&EnvSpec {
                value: Some("development".to_owned()),
                default: None,
                input: true,
            })
        );

        // Check task env
        let build_task = config.tasks.get("build").expect("build task");
        assert_eq!(build_task.env.len(), 2);
        assert_eq!(
            build_task.env.get("BUILD_TARGET"),
            Some(&EnvSpec {
                value: Some("es2020".to_owned()),
                default: None,
                input: true,
            })
        );
        assert_eq!(
            build_task.env.get("OVERRIDE_VAR"),
            Some(&EnvSpec {
                value: Some("worker-override".to_owned()),
                default: None,
                input: false,
            })
        );
    }
}
