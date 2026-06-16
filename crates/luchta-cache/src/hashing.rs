use std::collections::BTreeMap;

use luchta_types::{DependsOn, EnvSpec, TaskDefinition};
use serde::Serialize;

#[must_use]
pub fn task_spec_hash(task_def: &TaskDefinition) -> [u8; 32] {
    let spec = TaskSpecHashInput {
        command: task_def.command.as_deref(),
        worker: task_def.worker.as_deref(),
        weight: task_def.weight,
        depends_on: &task_def.depends_on,
        cache_enabled: task_def.cache_enabled(),
        inputs: &task_def.inputs,
        outputs: &task_def.outputs,
    };
    let bytes = bincode::serde::encode_to_vec(spec, bincode_config())
        .expect("task spec canonical bincode serialization should succeed");

    *blake3::hash(&bytes).as_bytes()
}

#[must_use]
pub fn env_hash<F>(env: &BTreeMap<String, EnvSpec>, mut resolver: F) -> [u8; 32]
where
    F: FnMut(&str) -> Option<String>,
{
    let pairs = env
        .iter()
        .filter(|(_name, spec)| spec.input)
        .map(|(name, spec)| SignificantEnvEntry {
            name: name.clone(),
            value: match spec.resolve_env_value(name, || resolver(name)) {
                Some(value) => ResolvedEnvValue::Present(value),
                None => ResolvedEnvValue::Absent,
            },
        })
        .collect::<Vec<_>>();

    let bytes = bincode::serde::encode_to_vec(&pairs, bincode_config())
        .expect("significant environment canonical bincode serialization should succeed");

    *blake3::hash(&bytes).as_bytes()
}

#[must_use]
pub fn pkg_dep_hash(pairs: &[(String, String)]) -> [u8; 32] {
    let mut canonical_pairs = pairs.to_vec();
    canonical_pairs.sort_unstable();
    canonical_pairs.dedup();

    let bytes = bincode::serde::encode_to_vec(&canonical_pairs, bincode_config())
        .expect("package dependency canonical bincode serialization should succeed");

    *blake3::hash(&bytes).as_bytes()
}

fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard().with_fixed_int_encoding()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct TaskSpecHashInput<'a> {
    command: Option<&'a str>,
    worker: Option<&'a str>,
    weight: u32,
    depends_on: &'a [DependsOn],
    cache_enabled: bool,
    // Declared input/output patterns are part of the task spec: changing a
    // pattern must invalidate the cache even if the resolved file set happens
    // to be unchanged. `env` is deliberately excluded here — it is tracked by
    // `env_hash`, which honors the `input: false` opt-out.
    inputs: &'a [String],
    outputs: &'a [String],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SignificantEnvEntry {
    name: String,
    value: ResolvedEnvValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
enum ResolvedEnvValue {
    Present(String),
    Absent,
}

#[cfg(test)]
mod tests {
    use super::{env_hash, pkg_dep_hash, task_spec_hash};
    use std::collections::BTreeMap;

    use luchta_types::{
        CacheConfig, DependsOn, EnvSpec, PackageName, TaskDefinition, TaskId, TaskName,
    };

    #[test]
    fn task_spec_hash_changes_when_command_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task);
        task.command = Some("pnpm run test".to_owned());

        assert_ne!(baseline, task_spec_hash(&task));
    }

    #[test]
    fn task_spec_hash_changes_when_weight_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task);
        task.weight += 1;

        assert_ne!(baseline, task_spec_hash(&task));
    }

    #[test]
    fn task_spec_hash_changes_when_worker_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task);
        task.worker = Some("shell".to_owned());

        assert_ne!(baseline, task_spec_hash(&task));
    }

    #[test]
    fn task_spec_hash_changes_when_depends_on_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task);
        task.depends_on
            .push(DependsOn::Root(TaskName::from("lint")));

        assert_ne!(baseline, task_spec_hash(&task));
    }

    #[test]
    fn task_spec_hash_changes_when_cache_enabled_toggles() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task);
        task.cache = None;

        assert_ne!(baseline, task_spec_hash(&task));
    }

    #[test]
    fn task_spec_hash_ignores_significant_env_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task);
        task.env.get_mut("NODE_ENV").expect("NODE_ENV").value = Some("prod".to_owned());

        assert_eq!(baseline, task_spec_hash(&task));
    }

    #[test]
    fn task_spec_hash_ignores_input_false_env_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task);
        task.env.get_mut("API_TOKEN").expect("API_TOKEN").value = Some("secret".to_owned());

        assert_eq!(baseline, task_spec_hash(&task));
    }

    #[test]
    fn task_spec_hash_changes_when_inputs_change() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task);
        task.inputs.push("src/extra.ts".to_owned());

        assert_ne!(baseline, task_spec_hash(&task));
    }

    #[test]
    fn task_spec_hash_changes_when_outputs_change() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task);
        task.outputs.push("coverage/**".to_owned());

        assert_ne!(baseline, task_spec_hash(&task));
    }

    #[test]
    fn task_spec_hash_is_deterministic_for_identical_input() {
        let task = sample_task_definition();

        assert_eq!(task_spec_hash(&task), task_spec_hash(&task));
    }

    #[test]
    fn env_hash_changes_when_significant_value_changes() {
        let mut env = BTreeMap::new();
        env.insert(
            "NODE_ENV".to_owned(),
            EnvSpec {
                value: Some("test".to_owned()),
                default: None,
                input: true,
            },
        );

        let baseline = env_hash(&env, |_| None);
        env.insert(
            "NODE_ENV".to_owned(),
            EnvSpec {
                value: Some("production".to_owned()),
                default: None,
                input: true,
            },
        );

        assert_ne!(baseline, env_hash(&env, |_| None));
    }

    #[test]
    fn env_hash_excludes_input_false_entries() {
        let mut env = BTreeMap::new();
        env.insert(
            "NODE_ENV".to_owned(),
            EnvSpec {
                value: Some("test".to_owned()),
                default: None,
                input: true,
            },
        );
        let baseline = env_hash(&env, |_| None);

        env.insert(
            "SECRET".to_owned(),
            EnvSpec {
                value: Some("abc123".to_owned()),
                default: None,
                input: false,
            },
        );
        let with_input_false = env_hash(&env, |_| Some("ambient-secret".to_owned()));

        env.insert(
            "SECRET".to_owned(),
            EnvSpec {
                value: Some("changed".to_owned()),
                default: None,
                input: false,
            },
        );
        let changed_input_false = env_hash(&env, |_| None);

        env.remove("SECRET");
        let removed_input_false = env_hash(&env, |_| None);

        assert_eq!(baseline, with_input_false);
        assert_eq!(baseline, changed_input_false);
        assert_eq!(baseline, removed_input_false);
    }

    #[test]
    fn env_hash_changes_when_inherited_ambient_value_changes() {
        let env = BTreeMap::from([(
            "HOME".to_owned(),
            EnvSpec {
                value: None,
                default: None,
                input: true,
            },
        )]);

        let first = env_hash(&env, |_| Some("/tmp/one".to_owned()));
        let second = env_hash(&env, |_| Some("/tmp/two".to_owned()));

        assert_ne!(first, second);
    }

    #[test]
    fn env_hash_changes_when_default_fallback_changes() {
        let first = BTreeMap::from([(
            "CACHE_KEY".to_owned(),
            EnvSpec {
                value: None,
                default: Some("alpha".to_owned()),
                input: true,
            },
        )]);
        let second = BTreeMap::from([(
            "CACHE_KEY".to_owned(),
            EnvSpec {
                value: None,
                default: Some("beta".to_owned()),
                input: true,
            },
        )]);

        assert_ne!(env_hash(&first, |_| None), env_hash(&second, |_| None));
    }

    #[test]
    fn env_hash_ignores_whitelist_only_ambient_changes() {
        let env = BTreeMap::new();

        assert_eq!(
            env_hash(&env, |_| Some("/path/one".to_owned())),
            env_hash(&env, |_| Some("/path/two".to_owned()))
        );
    }

    #[test]
    fn env_hash_matches_execution_resolution_for_default_fallback() {
        let spec = EnvSpec {
            value: None,
            default: Some("fallback".to_owned()),
            input: true,
        };
        let env = BTreeMap::from([("DEFAULTED".to_owned(), spec.clone())]);

        let resolved = spec.resolve_env_value("DEFAULTED", || None);
        assert_eq!(resolved, Some("fallback".to_owned()));

        let hashed = env_hash(&env, |_| None);
        let expected = env_hash(
            &BTreeMap::from([(
                "DEFAULTED".to_owned(),
                EnvSpec {
                    value: Some("fallback".to_owned()),
                    default: None,
                    input: true,
                },
            )]),
            |_| None,
        );

        assert_eq!(hashed, expected);
    }

    #[test]
    fn env_hash_distinguishes_present_from_absent() {
        let mut env = BTreeMap::new();
        env.insert(
            "OPTIONAL_FLAG".to_owned(),
            EnvSpec {
                value: None,
                default: None,
                input: true,
            },
        );

        let absent = env_hash(&env, |_| None);
        let present_empty = env_hash(&env, |_| Some(String::new()));

        assert_ne!(absent, present_empty);
    }

    #[test]
    fn env_hash_is_deterministic_for_identical_input() {
        let mut env = BTreeMap::new();
        env.insert(
            "HOME".to_owned(),
            EnvSpec {
                value: None,
                default: None,
                input: true,
            },
        );

        let first = env_hash(&env, |name| {
            assert_eq!(name, "HOME");
            Some("/tmp/home".to_owned())
        });
        let second = env_hash(&env, |name| {
            assert_eq!(name, "HOME");
            Some("/tmp/home".to_owned())
        });

        assert_eq!(first, second);
    }

    #[test]
    fn pkg_dep_hash_changes_when_version_changes() {
        let baseline = pkg_dep_hash(&[("react".to_owned(), "18.2.0".to_owned())]);
        let changed = pkg_dep_hash(&[("react".to_owned(), "19.0.0".to_owned())]);

        assert_ne!(baseline, changed);
    }

    #[test]
    fn pkg_dep_hash_is_deterministic_for_identical_input() {
        let pairs = vec![
            ("react".to_owned(), "18.2.0".to_owned()),
            ("vite".to_owned(), "5.4.0".to_owned()),
        ];

        assert_eq!(pkg_dep_hash(&pairs), pkg_dep_hash(&pairs));
    }

    #[test]
    fn pkg_dep_hash_normalizes_unsorted_and_duplicate_input() {
        let unsorted = vec![
            ("vite".to_owned(), "5.4.0".to_owned()),
            ("react".to_owned(), "18.2.0".to_owned()),
            ("react".to_owned(), "18.2.0".to_owned()),
        ];
        let sorted = vec![
            ("react".to_owned(), "18.2.0".to_owned()),
            ("vite".to_owned(), "5.4.0".to_owned()),
        ];

        assert_eq!(pkg_dep_hash(&unsorted), pkg_dep_hash(&sorted));
    }

    fn sample_task_definition() -> TaskDefinition {
        let mut env = BTreeMap::new();
        env.insert(
            "NODE_ENV".to_owned(),
            EnvSpec {
                value: Some("test".to_owned()),
                default: None,
                input: true,
            },
        );
        env.insert(
            "API_TOKEN".to_owned(),
            EnvSpec {
                value: None,
                default: None,
                input: false,
            },
        );

        TaskDefinition {
            depends_on: vec![
                DependsOn::DirectUpstream(TaskName::from("build")),
                DependsOn::Specific(TaskId::new(PackageName::from("ui"), TaskName::from("lint"))),
            ],
            weight: 2,
            command: Some("pnpm run build".to_owned()),
            worker: Some("node".to_owned()),
            cache: Some(CacheConfig::default()),
            inputs: vec!["src/**/*.ts".to_owned(), "package.json".to_owned()],
            outputs: vec!["dist/**".to_owned()],
            env,
        }
    }
}
