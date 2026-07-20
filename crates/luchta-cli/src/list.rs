//! List command implementation.
//!
//! Prints configured task definitions for matched tasks, omitting default-valued
//! fields in human output and optionally emitting full JSON records.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use luchta_engine::{PrunedTask, ResolveMode, TaskGraph};
use luchta_types::{CacheConfig, DependsOn, EnvSpec, TaskDefinition, TaskId};
use miette::{IntoDiagnostic, Result};
use serde::Serialize;

use crate::format::package_and_task_display;
use crate::run::{
    build_globset, collect_matched_package_names, collect_requested_subgraph, prepare_workspace,
    CollectSubgraphRequest, TaskSelection,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ListedTask {
    pub task_id: String,
    pub package: String,
    pub task: String,
    #[serde(flatten)]
    pub definition: TaskDefinition,
}

/// Execute `luchta list`.
pub async fn execute_list(
    workspace_root: &Path,
    tasks: Vec<String>,
    packages: Vec<String>,
    top_level: bool,
    json: bool,
) -> Result<()> {
    let prepared = prepare_workspace(workspace_root, ResolveMode::Run, None).await?;
    prepared.worker_manager.shutdown().await;

    let selection = TaskSelection {
        requested_tasks: &tasks,
        packages: &packages,
        top_level,
        since: None,
    };
    let selected_ids = select_task_ids(&prepared.task_graph, &selection, &prepared.pruned)?;

    let mut sorted_ids: Vec<TaskId> = selected_ids.into_iter().collect();
    sorted_ids.sort_by_key(|id| id.to_string());

    if json {
        let listed: Vec<ListedTask> = sorted_ids
            .into_iter()
            .filter_map(|task_id| {
                let definition = prepared.task_graph.task_definition(&task_id)?.clone();
                let (package, task) = package_and_task_display(&task_id);
                Some(ListedTask {
                    task_id: task_id.to_string(),
                    package: package.to_string(),
                    task: task.to_string(),
                    definition,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&listed).into_diagnostic()?
        );
        return Ok(());
    }

    for task_id in sorted_ids {
        let Some(task_def) = prepared.task_graph.task_definition(&task_id) else {
            continue;
        };

        println!("{task_id}");
        for (key, value) in format_non_default_fields(task_def) {
            println!("  {key}: {value}");
        }
    }

    Ok(())
}

fn select_task_ids(
    task_graph: &TaskGraph,
    selection: &TaskSelection<'_>,
    pruned: &[PrunedTask],
) -> Result<HashSet<TaskId>> {
    if selection_uses_default_scope(selection) {
        return Ok(task_graph.nodes().map(|node| node.id.clone()).collect());
    }

    if selection_requests_only_top_level(selection) {
        return Ok(task_graph
            .nodes()
            .filter(|node| node.id.is_root())
            .map(|node| node.id.clone())
            .collect());
    }

    if selection_requests_packages_without_tasks(selection) {
        let package_globs = build_globset(selection.packages)?;
        let available_nodes: Vec<_> = task_graph.nodes().collect();
        let matched_package_names =
            collect_matched_package_names(&available_nodes, selection.packages, &package_globs);

        if matched_package_names.is_empty() {
            return Err(miette::miette!(
                "No packages matched: [{}]. -p matches package names, not paths.",
                selection.packages.join(", ")
            ));
        }

        return Ok(available_nodes
            .into_iter()
            .filter(|node| !node.id.is_root())
            .filter(|node| matched_package_names.contains(&node.id.package))
            .map(|node| node.id.clone())
            .collect());
    }

    collect_requested_subgraph(CollectSubgraphRequest {
        task_graph,
        selection,
        pruned,
        since_affected: None,
        expand_dependencies: false,
    })
}

fn selection_uses_default_scope(selection: &TaskSelection<'_>) -> bool {
    selection.requested_tasks.is_empty() && selection.packages.is_empty() && !selection.top_level
}

fn selection_requests_only_top_level(selection: &TaskSelection<'_>) -> bool {
    selection.top_level && selection.requested_tasks.is_empty() && selection.packages.is_empty()
}

fn selection_requests_packages_without_tasks(selection: &TaskSelection<'_>) -> bool {
    selection.requested_tasks.is_empty() && !selection.packages.is_empty() && !selection.top_level
}

fn format_non_default_fields(task_def: &TaskDefinition) -> Vec<(String, String)> {
    let mut fields = Vec::new();

    if let Some(description) = &task_def.description {
        fields.push(("description".to_string(), description.clone()));
    }
    if let Some(command) = &task_def.command {
        fields.push(("command".to_string(), command.clone()));
    }
    if let Some(worker) = &task_def.worker {
        fields.push(("worker".to_string(), worker.clone()));
    }
    if task_def.weight != 1 {
        fields.push(("weight".to_string(), task_def.weight.to_string()));
    }
    if !task_def.depends_on.is_empty() {
        fields.push((
            "depends_on".to_string(),
            format_depends_on(&task_def.depends_on),
        ));
    }
    if !task_def.inputs.is_empty() {
        fields.push(("inputs".to_string(), format_string_list(&task_def.inputs)));
    }
    if !task_def.outputs.is_empty() {
        fields.push(("outputs".to_string(), format_string_list(&task_def.outputs)));
    }
    if task_def.dependencies != ["**/*"] {
        fields.push((
            "dependencies".to_string(),
            format_string_list(&task_def.dependencies),
        ));
    }
    if !task_def.env.is_empty() {
        fields.push(("env".to_string(), format_env_map(&task_def.env)));
    }
    if let Some(cache) = &task_def.cache {
        fields.push(("cache".to_string(), format_cache_config(cache)));
    }

    fields
}

fn format_depends_on(depends_on: &[DependsOn]) -> String {
    let parts: Vec<String> = depends_on.iter().map(ToString::to_string).collect();
    format!("[{}]", parts.join(", "))
}

fn format_string_list(values: &[String]) -> String {
    format!("[{}]", values.join(", "))
}

fn format_env_map(env: &BTreeMap<String, EnvSpec>) -> String {
    let parts: Vec<String> = env
        .iter()
        .map(|(key, spec)| format!("{key}: {}", format_env_spec(spec)))
        .collect();
    format!("{{{}}}", parts.join(", "))
}

fn format_env_spec(spec: &EnvSpec) -> String {
    let mut fields = Vec::new();

    if let Some(value) = &spec.value {
        fields.push(format!("value={value}"));
    }
    if let Some(default) = &spec.default {
        fields.push(format!("default={default}"));
    }
    if !spec.input {
        fields.push("input=false".to_string());
    }

    if fields.is_empty() {
        "{}".to_string()
    } else {
        format!("{{{}}}", fields.join(", "))
    }
}

fn format_cache_config(cache: &CacheConfig) -> String {
    match &cache.cache_nonce {
        Some(cache_nonce) => format!("{{cache_nonce: {cache_nonce}}}"),
        None => "{}".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use luchta_types::{CacheConfig, DependsOn, EnvSpec, TaskDefinition, TaskId, TaskName};

    use super::format_non_default_fields;

    #[test]
    fn format_non_default_fields_omits_defaults() {
        assert!(format_non_default_fields(&TaskDefinition::default()).is_empty());
    }

    #[test]
    fn format_non_default_fields_with_partial_non_defaults_returns_only_expected_fields_in_order() {
        let task_def = TaskDefinition {
            description: Some("small desc".to_string()),
            weight: 3,
            inputs: vec!["a.txt".to_string()],
            ..TaskDefinition::default()
        };

        assert_eq!(
            format_non_default_fields(&task_def),
            vec![
                ("description".to_string(), "small desc".to_string()),
                ("weight".to_string(), "3".to_string()),
                ("inputs".to_string(), "[a.txt]".to_string()),
            ]
        );
    }

    #[test]
    fn format_non_default_fields_orders_and_formats_values() {
        let mut env = BTreeMap::new();
        env.insert(
            "FOO".to_string(),
            EnvSpec {
                value: Some("bar".to_string()),
                default: Some("baz".to_string()),
                input: false,
            },
        );

        let task_def = TaskDefinition {
            description: Some("desc".to_string()),
            command: Some("build:prod".to_string()),
            worker: Some("node".to_string()),
            weight: 2,
            depends_on: vec![
                DependsOn::SamePackage(TaskName("lint".to_string())),
                DependsOn::Specific(TaskId::new("pkg", "build")),
            ],
            cache: Some(CacheConfig {
                cache_nonce: Some("abc".to_string()),
            }),
            inputs: vec!["src/**/*".to_string()],
            outputs: vec!["dist/**/*".to_string()],
            dependencies: vec!["left-pad".to_string(), "react".to_string()],
            env,
        };

        assert_eq!(
            format_non_default_fields(&task_def),
            vec![
                ("description".to_string(), "desc".to_string()),
                ("command".to_string(), "build:prod".to_string()),
                ("worker".to_string(), "node".to_string()),
                ("weight".to_string(), "2".to_string()),
                ("depends_on".to_string(), "[lint, pkg#build]".to_string()),
                ("inputs".to_string(), "[src/**/*]".to_string()),
                ("outputs".to_string(), "[dist/**/*]".to_string()),
                ("dependencies".to_string(), "[left-pad, react]".to_string(),),
                (
                    "env".to_string(),
                    "{FOO: {value=bar, default=baz, input=false}}".to_string(),
                ),
                ("cache".to_string(), "{cache_nonce: abc}".to_string()),
            ]
        );
    }
}
