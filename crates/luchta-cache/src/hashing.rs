use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

use luchta_types::{CacheSharing, DependsOn, EnvSpec, TaskDefinition};
use serde::Serialize;

use crate::serialization::bincode_config;

pub fn blake3_file(path: &Path) -> crate::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(*hasher.finalize().as_bytes())
}

#[must_use]
pub fn task_spec_hash(task_def: &TaskDefinition, nonce: Option<&str>) -> [u8; 32] {
    let spec = TaskSpecHashInput {
        command: task_def.command.as_deref(),
        worker: task_def.worker.as_deref(),
        weight: task_def.weight,
        depends_on: &task_def.depends_on,
        cache_enabled: task_def.cache_enabled(),
        inputs: &task_def.inputs,
        outputs: &task_def.outputs,
        nonce,
        sharing: task_def
            .cache
            .as_ref()
            .map(|c| c.sharing)
            .unwrap_or_default(),
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
    // Cache nonce belongs in task_spec_hash, not env_hash: unlike env, this is
    // cache-control with no opt-out semantics, so any supplied value must always
    // invalidate task-spec identity. See docs/hash-boundary-task-spec-vs-separate
    // for prior art on why env stays tracked separately by env_hash. Adding this
    // Option also changes bincode layout even for None because Option serializes a
    // discriminant byte, so on-disk hashes invalidate once on upgrade; accepted by
    // plan decision.
    nonce: Option<&'a str>,
    // Cache sharing policy belongs in task_spec_hash for the same reason as nonce:
    // it is cache-control that affects which cache tiers (local vs remote) a task
    // may use, and changing the policy must invalidate task-spec identity. Changing
    // this enum value also changes bincode layout, so on-disk hashes invalidate once
    // on upgrade; accepted by plan decision. See plan luchta-cache-sharing / issue #103.
    sharing: CacheSharing,
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
    use super::{blake3_file, env_hash, pkg_dep_hash, task_spec_hash};
    use std::{collections::BTreeMap, fs};

    use luchta_types::{
        CacheConfig, CacheSharing, DependsOn, EnvSpec, PackageName, TaskDefinition, TaskId,
        TaskName,
    };
    use tempfile::TempDir;

    #[test]
    fn blake3_file_matches_in_memory_hash() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("artifact.bin");
        let bytes = (0..25_000).map(|n| (n % 251) as u8).collect::<Vec<_>>();
        fs::write(&path, &bytes).unwrap();

        assert_eq!(
            blake3_file(&path).unwrap(),
            *blake3::hash(&bytes).as_bytes()
        );
    }

    #[test]
    fn task_spec_hash_changes_when_command_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task, None);
        task.command = Some("pnpm run test".to_owned());

        assert_ne!(baseline, task_spec_hash(&task, None));
    }

    #[test]
    fn task_spec_hash_changes_when_weight_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task, None);
        task.weight += 1;

        assert_ne!(baseline, task_spec_hash(&task, None));
    }

    #[test]
    fn task_spec_hash_changes_when_worker_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task, None);
        task.worker = Some("shell".to_owned());

        assert_ne!(baseline, task_spec_hash(&task, None));
    }

    #[test]
    fn task_spec_hash_changes_when_depends_on_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task, None);
        task.depends_on
            .push(DependsOn::Root(TaskName::from("lint")));

        assert_ne!(baseline, task_spec_hash(&task, None));
    }

    #[test]
    fn task_spec_hash_changes_when_cache_enabled_toggles() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task, None);
        task.cache = None;

        assert_ne!(baseline, task_spec_hash(&task, None));
    }

    #[test]
    fn task_spec_hash_ignores_significant_env_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task, None);
        task.env.get_mut("NODE_ENV").expect("NODE_ENV").value = Some("prod".to_owned());

        assert_eq!(baseline, task_spec_hash(&task, None));
    }

    #[test]
    fn task_spec_hash_ignores_input_false_env_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task, None);
        task.env.get_mut("API_TOKEN").expect("API_TOKEN").value = Some("secret".to_owned());

        assert_eq!(baseline, task_spec_hash(&task, None));
    }

    #[test]
    fn task_spec_hash_changes_when_inputs_change() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task, None);
        task.inputs.push("src/extra.ts".to_owned());

        assert_ne!(baseline, task_spec_hash(&task, None));
    }

    /// Regression guard for lage issue #839
    /// (<https://github.com/microsoft/lage/issues/839>): "Hashes should be
    /// different (but are the same) when inputs differ by filename" for
    /// non-existing files.
    ///
    /// In lage the target hash was derived only from the *resolved* file set, so
    /// two targets whose inputs named different but non-existent files
    /// (`file1.txt` vs `file2.txt`) collided to the same hash and the cache
    /// served stale results. Luchta folds the declared input *patterns*
    /// themselves into `task_spec_hash`, so distinct input filenames yield
    /// distinct hashes regardless of whether the files exist on disk.
    #[test]
    fn task_spec_hash_differs_for_distinct_missing_input_filenames() {
        let mut a = sample_task_definition();
        a.inputs = vec!["file1.txt".to_owned()];

        let mut b = sample_task_definition();
        b.inputs = vec!["file2.txt".to_owned()];

        // Neither file exists; the only difference is the input filename.
        assert_ne!(
            task_spec_hash(&a, None),
            task_spec_hash(&b, None),
            "tasks with different (non-existent) input filenames must hash differently"
        );
    }

    #[test]
    fn task_spec_hash_changes_when_outputs_change() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task, None);
        task.outputs.push("coverage/**".to_owned());

        assert_ne!(baseline, task_spec_hash(&task, None));
    }

    #[test]
    fn task_spec_hash_without_nonce_is_deterministic() {
        let task = sample_task_definition();
        let first = task_spec_hash(&task, None);
        let second = task_spec_hash(&task, None);

        assert_eq!(first, second);
    }

    #[test]
    fn task_spec_hash_without_nonce_matches_pinned_regression_value() {
        let task = sample_task_definition();

        // Pinned after cacheNonce feature landed. Value changed once at feature
        // introduction because bincode encodes Option with discriminant byte even
        // for None, so future changes here should be deliberate.
        // Updated again after cacheSharing feature: adding `sharing` to TaskSpecHashInput
        // changes bincode layout (enum discriminant byte), causing one-time invalidation.
        assert_eq!(
            task_spec_hash(&task, None),
            [
                156, 137, 202, 68, 166, 105, 242, 120, 72, 35, 223, 54, 124, 94, 58, 58, 171, 210,
                216, 144, 153, 241, 170, 61, 144, 74, 49, 1, 100, 127, 227, 99,
            ]
        );
    }

    #[test]
    fn task_spec_hash_changes_when_nonce_added() {
        let task = sample_task_definition();

        assert_ne!(
            task_spec_hash(&task, Some("x")),
            task_spec_hash(&task, None)
        );
    }

    #[test]
    fn task_spec_hash_changes_when_nonce_changes() {
        let task = sample_task_definition();

        assert_ne!(
            task_spec_hash(&task, Some("a")),
            task_spec_hash(&task, Some("b"))
        );
    }

    #[test]
    fn task_spec_hash_distinguishes_scope_specific_and_combined_nonces() {
        let task = sample_task_definition();
        let env_only = Some("env=env-only");
        let global_only = Some("global=global-only");
        let worker_only = Some("worker=worker-only");
        let task_only = Some("task=task-only");
        let combined = Some("env=env-only&global=global-only&worker=worker-only&task=task-only");

        let hashes = [
            task_spec_hash(&task, env_only),
            task_spec_hash(&task, global_only),
            task_spec_hash(&task, worker_only),
            task_spec_hash(&task, task_only),
            task_spec_hash(&task, combined),
        ];

        for (left_index, left_hash) in hashes.iter().enumerate() {
            for right_hash in hashes.iter().skip(left_index + 1) {
                assert_ne!(left_hash, right_hash);
            }
        }
    }

    #[test]
    fn task_spec_hash_changes_when_any_nonce_source_changes() {
        let task = sample_task_definition();
        let baseline = task_spec_hash(
            &task,
            Some("env=env-a&global=global-a&worker=worker-a&task=task-a"),
        );

        assert_ne!(
            baseline,
            task_spec_hash(
                &task,
                Some("env=env-b&global=global-a&worker=worker-a&task=task-a"),
            )
        );
        assert_ne!(
            baseline,
            task_spec_hash(
                &task,
                Some("env=env-a&global=global-b&worker=worker-a&task=task-a"),
            )
        );
        assert_ne!(
            baseline,
            task_spec_hash(
                &task,
                Some("env=env-a&global=global-a&worker=worker-b&task=task-a"),
            )
        );
        assert_ne!(
            baseline,
            task_spec_hash(
                &task,
                Some("env=env-a&global=global-a&worker=worker-a&task=task-b"),
            )
        );
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
            description: None,
            worker: Some("node".to_owned()),
            cache: Some(CacheConfig::default()),
            inputs: vec!["src/**/*.ts".to_owned(), "package.json".to_owned()],
            outputs: vec!["dist/**".to_owned()],
            dependencies: vec!["**/*".to_string()],
            env,
        }
    }

    #[test]
    fn task_spec_hash_changes_when_sharing_changes() {
        let mut task = sample_task_definition();
        let baseline = task_spec_hash(&task, None);

        // Change sharing from default (Remote) to None
        task.cache = Some(CacheConfig {
            sharing: CacheSharing::None,
            ..CacheConfig::default()
        });

        assert_ne!(baseline, task_spec_hash(&task, None));
    }

    #[test]
    fn task_spec_hash_is_stable_for_identical_sharing() {
        let task = sample_task_definition();
        let first = task_spec_hash(&task, None);
        let second = task_spec_hash(&task, None);

        assert_eq!(first, second);
    }

    #[test]
    fn task_spec_hash_distinguishes_all_sharing_variants() {
        let base_task = sample_task_definition();

        let hash_none = task_spec_hash(
            &TaskDefinition {
                cache: Some(CacheConfig {
                    sharing: CacheSharing::None,
                    ..CacheConfig::default()
                }),
                ..base_task.clone()
            },
            None,
        );

        let hash_local = task_spec_hash(
            &TaskDefinition {
                cache: Some(CacheConfig {
                    sharing: CacheSharing::Local,
                    ..CacheConfig::default()
                }),
                ..base_task.clone()
            },
            None,
        );

        let hash_remote = task_spec_hash(
            &TaskDefinition {
                cache: Some(CacheConfig {
                    sharing: CacheSharing::Remote,
                    ..CacheConfig::default()
                }),
                ..base_task.clone()
            },
            None,
        );

        // All three variants should produce distinct hashes
        assert_ne!(hash_none, hash_local);
        assert_ne!(hash_local, hash_remote);
        assert_ne!(hash_none, hash_remote);
    }
}
