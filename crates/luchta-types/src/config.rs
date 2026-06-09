use std::{collections::HashMap, path::PathBuf};

use serde::Deserialize;

use crate::TaskDefinition;

/// Canonical `luchta.toml` schema shared across crates.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LuchtaConfig {
    /// Weighted concurrency settings for executor.
    #[serde(default)]
    pub concurrency: ConcurrencyConfig,
    /// Task pipeline keyed by task name.
    #[serde(default)]
    pub pipeline: HashMap<String, TaskDefinition>,
}

/// Scheduler concurrency settings from `[concurrency]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ConcurrencyConfig {
    /// Global maximum cumulative task weight allowed to run at once.
    pub max_weight: u32,
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self { max_weight: 1 }
    }
}

/// Concrete execution settings after command resolution.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct ExecutionSpec {
    /// Command line to execute for task.
    pub command: String,
    /// Working directory override relative to workspace root when set.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables merged into spawned process environment.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::{ConcurrencyConfig, LuchtaConfig};
    use crate::{DependsOn, PackageName, TaskDefinition, TaskId, TaskName};

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
            config.pipeline.get("build"),
            Some(&TaskDefinition {
                depends_on: vec![DependsOn::DirectUpstream(TaskName::from("build"))],
                weight: 3,
                command: None,
            })
        );

        assert_eq!(
            config.pipeline.get("test"),
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
            })
        );

        assert_eq!(
            config.pipeline.get("lint"),
            Some(&TaskDefinition {
                depends_on: Vec::new(),
                weight: 1,
                command: None,
            })
        );

        assert_eq!(
            config.pipeline.get("bundle"),
            Some(&TaskDefinition {
                depends_on: vec![DependsOn::TransitiveUpstream(TaskName::from("build"))],
                weight: 1,
                command: None,
            })
        );
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
