//! Dependency output-hash resolution across command-less connector tasks.
//!
//! A task's cache key folds in the output hashes of the tasks it depends on. A
//! connector — no worker and no non-blank command, see
//! [`TaskDefinition::counts_in_progress`] — never runs, so it has no outputs of
//! its own to hash. Left out of the map, it breaks the chain: a task whose
//! `dependsOn` reaches real build outputs only through a connector keeps hitting
//! the cache after those outputs change.
//!
//! A connector instead contributes a *synthetic* hash: a digest over the
//! resolved output hashes of its own dependencies, recursively. That makes the
//! value a Merkle root over the counted tasks reachable through the connector,
//! so any upstream output change reaches the dependent's key through any depth
//! of connectors — while the stored map stays one entry per declared
//! dependency, rather than the whole transitive closure.

use std::collections::{BTreeMap, HashMap};

use luchta_engine::TaskGraph;
use luchta_types::{TaskDefinition, TaskId};

/// Domain separator for the connector digest.
///
/// Distinct from the shared cache's `combined_dep_outputs_hash` domain so a
/// connector's synthetic hash can never collide with a real task's combined
/// dependency hash.
const CONNECTOR_DIGEST_DOMAIN: &[u8] = b"luchta.connector.dep-outputs.v1";
const CONNECTOR_DIGEST_SEPARATOR: u8 = 0x1f;

/// Resolves the dependency output hashes to fold into `task_id`'s cache key.
///
/// `lookup` supplies the output hash of a task that actually runs; returning
/// `None` drops that dependency from the map, matching the caller's own rule
/// for an unavailable hash (not yet run, no cache record, prior failure).
/// Connectors are never passed to `lookup` — they resolve to a digest over
/// their own dependencies instead.
pub(crate) fn resolve_dependency_outputs(
    task_id: &TaskId,
    task_graph: &TaskGraph,
    lookup: &dyn Fn(&TaskId) -> Option<[u8; 32]>,
) -> BTreeMap<String, [u8; 32]> {
    let mut connector_digests = HashMap::new();
    resolve(task_id, task_graph, lookup, &mut connector_digests)
}

fn resolve(
    task_id: &TaskId,
    task_graph: &TaskGraph,
    lookup: &dyn Fn(&TaskId) -> Option<[u8; 32]>,
    connector_digests: &mut HashMap<TaskId, [u8; 32]>,
) -> BTreeMap<String, [u8; 32]> {
    task_graph
        .dependencies_of(task_id)
        .into_iter()
        .filter_map(|dependency| {
            let hash = dependency_hash(&dependency.id, task_graph, lookup, connector_digests)?;
            Some((dependency.id.to_string(), hash))
        })
        .collect()
}

fn dependency_hash(
    dependency_id: &TaskId,
    task_graph: &TaskGraph,
    lookup: &dyn Fn(&TaskId) -> Option<[u8; 32]>,
    connector_digests: &mut HashMap<TaskId, [u8; 32]>,
) -> Option<[u8; 32]> {
    if !is_connector(dependency_id, task_graph) {
        return lookup(dependency_id);
    }

    // The task graph is acyclic, so the recursion terminates. Memoizing keeps a
    // diamond of connectors from re-walking the same subgraph: every task the
    // walk reaches has already finished, so its hash is final.
    if let Some(digest) = connector_digests.get(dependency_id) {
        return Some(*digest);
    }
    let nested = resolve(dependency_id, task_graph, lookup, connector_digests);
    let digest = connector_digest(&nested);
    connector_digests.insert(dependency_id.clone(), digest);
    Some(digest)
}

fn is_connector(task_id: &TaskId, task_graph: &TaskGraph) -> bool {
    task_graph
        .task_definition(task_id)
        .is_some_and(|definition| !TaskDefinition::counts_in_progress(definition))
}

/// Digest over a connector's own resolved dependency output hashes.
///
/// Order-stable: `dep_outputs` is a `BTreeMap`, so iteration follows task id.
fn connector_digest(dep_outputs: &BTreeMap<String, [u8; 32]>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CONNECTOR_DIGEST_DOMAIN);

    for (task_id, outputs_hash) in dep_outputs {
        hasher.update(task_id.as_bytes());
        hasher.update(&[CONNECTOR_DIGEST_SEPARATOR]);
        hasher.update(outputs_hash);
        hasher.update(&[CONNECTOR_DIGEST_SEPARATOR]);
    }

    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use luchta_types::{DependsOn, TaskName};
    use luchta_workspace::{PackageGraph, PackageNode};

    use super::*;

    const BUILD_HASH: [u8; 32] = [1; 32];
    const CHANGED_BUILD_HASH: [u8; 32] = [2; 32];
    const OTHER_HASH: [u8; 32] = [3; 32];

    /// Single-package graph over the given task definitions.
    fn task_graph(tasks: Vec<(TaskName, TaskDefinition)>) -> TaskGraph {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let package_dir = temp_dir.path().join("packages/app");
        std::fs::create_dir_all(&package_dir).expect("create package dir");
        std::fs::write(
            package_dir.join("package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .expect("write package manifest");

        let package_graph = PackageGraph::build(vec![PackageNode::new("app".into(), package_dir)])
            .expect("build package graph");

        TaskGraph::build(&package_graph, &HashMap::from_iter(tasks)).expect("build task graph")
    }

    fn counted(name: &str, depends_on: &[&str]) -> (TaskName, TaskDefinition) {
        let (name, mut definition) = connector(name, depends_on);
        definition.worker = Some("shell".to_owned());
        (name, definition)
    }

    /// No worker and no command — an ordering-only connector.
    fn connector(name: &str, depends_on: &[&str]) -> (TaskName, TaskDefinition) {
        (
            TaskName::from(name),
            TaskDefinition {
                depends_on: depends_on
                    .iter()
                    .map(|dependency| DependsOn::SamePackage(TaskName::from(*dependency)))
                    .collect(),
                ..TaskDefinition::default()
            },
        )
    }

    fn resolve_with(graph: &TaskGraph, hashes: &[(&str, [u8; 32])]) -> BTreeMap<String, [u8; 32]> {
        let hashes: HashMap<TaskId, [u8; 32]> = hashes
            .iter()
            .map(|(task, hash)| (TaskId::new("app", *task), *hash))
            .collect();
        resolve_dependency_outputs(&TaskId::new("app", "test"), graph, &|task_id| {
            hashes.get(task_id).copied()
        })
    }

    #[test]
    fn counted_dependency_contributes_its_own_output_hash() {
        let graph = task_graph(vec![counted("build", &[]), counted("test", &["build"])]);

        let resolved = resolve_with(&graph, &[("build", BUILD_HASH)]);

        assert_eq!(
            resolved,
            BTreeMap::from([("app#build".to_owned(), BUILD_HASH)])
        );
    }

    /// `app#test` -> `app#meta` (connector) -> `app#build`.
    fn single_connector_graph() -> TaskGraph {
        task_graph(vec![
            counted("build", &[]),
            connector("meta", &["build"]),
            counted("test", &["meta"]),
        ])
    }

    #[test]
    fn connector_dependency_contributes_a_hash_of_its_own() {
        let resolved = resolve_with(&single_connector_graph(), &[("build", BUILD_HASH)]);

        assert_eq!(
            resolved.keys().collect::<Vec<_>>(),
            vec!["app#meta"],
            "the connector, not the task behind it, is the recorded dependency"
        );
    }

    /// The whole point of the digest: it is a function of what is behind the
    /// connector, and of nothing else.
    #[test]
    fn connector_hash_tracks_the_output_behind_it() {
        let graph = single_connector_graph();

        assert_eq!(
            resolve_with(&graph, &[("build", BUILD_HASH)]),
            resolve_with(&graph, &[("build", BUILD_HASH)]),
            "an unchanged output must leave the connector hash alone"
        );
        assert_ne!(
            resolve_with(&graph, &[("build", BUILD_HASH)]),
            resolve_with(&graph, &[("build", CHANGED_BUILD_HASH)]),
            "a changed output must reach the connector hash"
        );
    }

    #[test]
    fn nested_connectors_propagate_an_upstream_change() {
        let graph = task_graph(vec![
            counted("build", &[]),
            connector("meta-inner", &["build"]),
            connector("meta-outer", &["meta-inner"]),
            counted("test", &["meta-outer"]),
        ]);

        assert_ne!(
            resolve_with(&graph, &[("build", BUILD_HASH)]),
            resolve_with(&graph, &[("build", CHANGED_BUILD_HASH)]),
        );
    }

    #[test]
    fn connector_hash_covers_every_task_behind_it() {
        let graph = task_graph(vec![
            counted("build", &[]),
            counted("codegen", &[]),
            connector("meta", &["build", "codegen"]),
            counted("test", &["meta"]),
        ]);

        assert_ne!(
            resolve_with(&graph, &[("build", BUILD_HASH), ("codegen", OTHER_HASH)]),
            resolve_with(
                &graph,
                &[("build", BUILD_HASH), ("codegen", CHANGED_BUILD_HASH)],
            ),
            "a sibling behind the same connector must reach the hash too"
        );
    }

    #[test]
    fn a_connector_reached_twice_resolves_to_one_hash() {
        let graph = task_graph(vec![
            counted("build", &[]),
            connector("meta", &["build"]),
            connector("meta-left", &["meta"]),
            connector("meta-right", &["meta"]),
            counted("test", &["meta-left", "meta-right"]),
        ]);

        let resolved = resolve_with(&graph, &[("build", BUILD_HASH)]);

        let left = resolved.get("app#meta-left").expect("left connector hash");
        assert_eq!(
            Some(left),
            resolved.get("app#meta-right"),
            "both connectors wrap the same subgraph"
        );
    }

    #[test]
    fn a_task_with_no_recorded_hash_drops_out() {
        let graph = task_graph(vec![counted("build", &[]), counted("test", &["build"])]);

        let resolved = resolve_with(&graph, &[]);

        assert!(resolved.is_empty());
    }
}
