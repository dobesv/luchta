//! Resolve-time worker versions must reach both local and shared cache identity.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::Path,
    process::Command,
};

use luchta_cache::{
    blake3_file, combined_inputs_hash, combined_outputs_hash, decide, env_hash, pkg_dep_hash,
    resolve_inputs, resolve_outputs,
    shared::snapshot::{combined_dep_outputs_hash, derive_input_key},
    task_spec_hash, Cache, CurrentState, Decision, DecisionResult, FileEntry, FileStateResolver,
    RunArtifacts, RunReason, TaskRunRecord, SCHEMA_VERSION_V5,
};
use luchta_engine::{
    PackageResolveInfo, ResolveMode, ResolveResult, ResolveTask, TaskGraph, TaskResolver,
};
use luchta_types::{CacheConfig, PackageName, TaskDefinition, TaskId, TaskName};
use luchta_workspace::{PackageGraph, PackageNode};
use tempfile::TempDir;

const TASK_ID: &str = "app#build";

#[tokio::test]
async fn changed_tool_version_reruns_locally_and_misses_shared_cache() {
    assert_cache_behavior(
        Some("v1"),
        Some("v2"),
        DecisionResult {
            action: Decision::Run,
            reason: RunReason::TaskSpecMismatch,
        },
    )
    .await;
}

#[tokio::test]
async fn unchanged_tool_version_hits_local_and_shared_cache() {
    assert_unchanged_version_is_cached(Some("v1")).await;
}

#[tokio::test]
async fn omitted_tool_version_preserves_legacy_hash_and_cache_hits() {
    let hash = assert_unchanged_version_is_cached(None).await;

    // Pinned using pre-tool_version task_spec_hash (pre-feature baseline spec hash), not the current hasher.
    assert_eq!(
        hash,
        [
            109, 79, 136, 181, 230, 171, 58, 59, 238, 192, 86, 254, 84, 124, 42, 162, 253, 165,
            148, 156, 252, 52, 50, 79, 38, 77, 238, 67, 218, 246, 107, 145,
        ]
    );
}

async fn assert_unchanged_version_is_cached(version: Option<&str>) -> [u8; 32] {
    assert_cache_behavior(
        version,
        version,
        DecisionResult {
            action: Decision::Skip,
            reason: RunReason::CacheHit,
        },
    )
    .await
}

async fn assert_cache_behavior(
    first_version: Option<&str>,
    next_version: Option<&str>,
    expected: DecisionResult,
) -> [u8; 32] {
    let workspace = Workspace::new();
    let (prior, resolved) = workspace.cached_rerun(first_version, next_version).await;
    let current = workspace.current_state(&resolved);

    assert_eq!(decide(Some(&prior.record), &current), expected);
    let cache_hit = expected.action == Decision::Skip;
    assert_eq!(
        prior.record.task_spec_hash == current.task_spec_hash,
        cache_hit,
        "task spec equality must match the expected cache decision"
    );
    assert_eq!(
        prior.input_key == shared_input_key(&current),
        cache_hit,
        "shared input key equality must match the expected cache decision"
    );
    current.task_spec_hash
}

struct VersionedWorker<'a>(Option<&'a str>);

impl TaskResolver for VersionedWorker<'_> {
    async fn resolve(&self, worker: &str, request: ResolveTask) -> Result<ResolveResult, String> {
        assert_eq!(worker, "mock");
        assert_eq!(request.id, TASK_ID);
        assert_eq!(request.command, "compile");
        assert_eq!(request.inputs, ["input.ts"]);

        let mut response = serde_json::json!({"decision": "modify"});
        if let Some(version) = self.0 {
            response["toolVersion"] = version.into();
        }
        // Exercise the wire field name, including legacy workers omitting it.
        serde_json::from_value(response).map_err(|error| error.to_string())
    }
}

struct CachedRun {
    record: TaskRunRecord,
    input_key: [u8; 32],
}

struct Workspace(TempDir);

impl Workspace {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        // Literal inputs still need a repository root for repo-relative file entries.
        let git = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .output()
            .expect("initialize fixture git repo");
        assert!(
            git.status.success(),
            "{}",
            String::from_utf8_lossy(&git.stderr)
        );
        fs::write(dir.path().join("package.json"), r#"{"name":"app"}"#).unwrap();
        fs::write(dir.path().join("input.ts"), "export const value = 1;\n").unwrap();
        fs::write(dir.path().join("output.js"), "export const value = 1;\n").unwrap();
        Self(dir)
    }

    async fn resolve(&self, version: Option<&str>) -> TaskDefinition {
        let packages = vec![PackageNode::new(PackageName::from("app"), self.0.path())];
        let resolve_info = PackageResolveInfo::map_from_packages(&packages);
        let package_graph = PackageGraph::build(packages).unwrap();
        let declared = TaskDefinition {
            worker: Some("mock".to_owned()),
            command: Some("compile".to_owned()),
            cache: Some(CacheConfig::default()),
            inputs: vec!["input.ts".to_owned()],
            outputs: vec!["output.js".to_owned()],
            ..TaskDefinition::default()
        };
        let pipeline = HashMap::from([(TaskName::from("build"), declared.clone())]);
        let (graph, pruned) = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &resolve_info,
            &HashMap::new(),
            &VersionedWorker(version),
            ResolveMode::Run,
        )
        .await
        .unwrap();

        assert!(pruned.is_empty());
        assert_eq!(graph.node_count(), 1);
        let resolved = graph
            .task_definition(&TaskId::new("app", "build"))
            .unwrap()
            .clone();
        assert_eq!(
            resolved,
            TaskDefinition {
                tool_version: version.map(str::to_owned),
                ..declared
            },
            "only the worker's resolved version should change the task"
        );
        resolved
    }

    fn current_state<'a>(&'a self, definition: &'a TaskDefinition) -> CurrentState<'a> {
        CurrentState {
            task_spec_hash: task_spec_hash(definition, None),
            env_hash: env_hash(&definition.env, |_| None),
            pkg_dep_hash: pkg_dep_hash(&[]),
            dep_outputs: BTreeMap::new(),
            cache_nonce: None,
            declared_input_patterns: &definition.inputs,
            declared_output_patterns: &definition.outputs,
            resolver: self,
        }
    }

    fn successful_record(&self, current: &CurrentState<'_>) -> TaskRunRecord {
        let inputs = self
            .resolve_inputs(current.declared_input_patterns, &[])
            .unwrap();
        let outputs = self
            .resolve_outputs(current.declared_output_patterns, &[])
            .unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(outputs.len(), 1);
        assert!(!inputs[0].absent && !outputs[0].absent);

        TaskRunRecord {
            schema_version: SCHEMA_VERSION_V5,
            task_spec_hash: current.task_spec_hash,
            input_patterns: current.declared_input_patterns.to_vec(),
            inputs,
            output_patterns: current.declared_output_patterns.to_vec(),
            outputs_hash: combined_outputs_hash(&outputs),
            outputs,
            detected_input_patterns: false,
            detected_output_patterns: false,
            env_hash: current.env_hash,
            pkg_dep_hash: current.pkg_dep_hash,
            dep_outputs: current.dep_outputs.clone(),
            exit_status: 0,
            succeeded: true,
            start_unix_ms: 1,
            end_unix_ms: 2,
            reports: Vec::new(),
            cache_nonce: None,
            run_reason: Some(RunReason::NoPriorRecord),
        }
    }

    async fn cached_rerun(
        &self,
        first_version: Option<&str>,
        next_version: Option<&str>,
    ) -> (CachedRun, TaskDefinition) {
        let first = self.resolve(first_version).await;
        let current = self.current_state(&first);
        let cache_dir = self.0.path().join(".luchta/cache");
        let cache = Cache::open(&cache_dir).unwrap();
        assert_eq!(
            decide(cache.read(TASK_ID).as_ref(), &current),
            DecisionResult {
                action: Decision::Run,
                reason: RunReason::NoPriorRecord,
            }
        );
        let record = self.successful_record(&current);
        let input_key = shared_input_key(&current);
        cache
            .write(
                TASK_ID,
                RunArtifacts {
                    record: &record,
                    stdout: &[],
                    stderr: &[],
                    reports: &[],
                },
            )
            .unwrap();

        let resolved = self.resolve(next_version).await;
        let persisted = Cache::open(&cache_dir).unwrap().read(TASK_ID).unwrap();
        assert_eq!(persisted, record);
        assert_eq!(
            self.resolve_inputs(&resolved.inputs, &[]).unwrap(),
            persisted.inputs,
            "version changes must not change file inputs"
        );
        assert_eq!(
            self.resolve_outputs(&resolved.outputs, &[]).unwrap(),
            persisted.outputs,
            "outputs stay in place for the local skip decision"
        );
        (
            CachedRun {
                record: persisted,
                input_key,
            },
            resolved,
        )
    }
}

impl FileStateResolver for Workspace {
    fn resolve_inputs(
        &self,
        patterns: &[String],
        _prior_entries: &[FileEntry],
    ) -> luchta_cache::Result<Vec<FileEntry>> {
        resolve_inputs(self.0.path(), patterns)
    }

    fn resolve_outputs(
        &self,
        patterns: &[String],
        _prior_entries: &[FileEntry],
    ) -> luchta_cache::Result<Vec<FileEntry>> {
        resolve_outputs(self.0.path(), patterns)
    }

    fn blake3_file(&self, path: &Path) -> luchta_cache::Result<[u8; 32]> {
        blake3_file(&self.0.path().join(path))
    }
}

fn shared_input_key(current: &CurrentState<'_>) -> [u8; 32] {
    let inputs = current
        .resolver
        .resolve_inputs(current.declared_input_patterns, &[])
        .unwrap();
    derive_input_key(
        current.task_spec_hash,
        current.env_hash,
        current.pkg_dep_hash,
        combined_dep_outputs_hash(&current.dep_outputs),
        combined_inputs_hash(&inputs),
    )
}
