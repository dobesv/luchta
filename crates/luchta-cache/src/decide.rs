use std::{collections::BTreeMap, path::Path};

use crate::{FileDelta, FileEntry, RunReason, TaskRunRecord};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Skip,
    SharedHit,
    Run,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionResult {
    pub action: Decision,
    pub reason: RunReason,
}

pub const DECIDE_FILES_DIFF_LIMIT: usize = 50;

pub trait FileStateResolver {
    fn resolve_inputs(&self, patterns: &[String]) -> crate::Result<Vec<FileEntry>>;
    fn resolve_outputs(&self, patterns: &[String]) -> crate::Result<Vec<FileEntry>>;
    fn blake3_file(&self, path: &Path) -> crate::Result<[u8; 32]>;
}

pub struct CurrentState<'a> {
    pub task_spec_hash: [u8; 32],
    pub env_hash: [u8; 32],
    pub pkg_dep_hash: [u8; 32],
    pub dep_outputs: BTreeMap<String, [u8; 32]>,
    pub cache_nonce: Option<&'a str>,
    pub declared_input_patterns: &'a [String],
    pub declared_output_patterns: &'a [String],
    pub resolver: &'a dyn FileStateResolver,
}

/// Decision precedence. First mismatch wins and returns `Decision::Run` with reason:
/// 1. NoPriorRecord
/// 2. PriorFailed
/// 3. NonceChanged
/// 4. TaskSpecMismatch
/// 5. DepOutputMismatch
/// 6. PkgDepMismatch
/// 7. EnvMismatch
/// 8. OutputChanged
/// 9. InputChanged
/// 10. CacheHit
pub fn decide(prior: Option<&TaskRunRecord>, current: &CurrentState<'_>) -> DecisionResult {
    let Some(prior) = prior else {
        return DecisionResult {
            action: Decision::Run,
            reason: RunReason::NoPriorRecord,
        };
    };

    if !prior.succeeded {
        return DecisionResult {
            action: Decision::Run,
            reason: RunReason::PriorFailed,
        };
    }

    if prior.cache_nonce.as_deref() != current.cache_nonce {
        return DecisionResult {
            action: Decision::Run,
            reason: RunReason::NonceChanged,
        };
    }

    if prior.task_spec_hash != current.task_spec_hash {
        return DecisionResult {
            action: Decision::Run,
            reason: RunReason::TaskSpecMismatch,
        };
    }

    let changed_dep_tasks = changed_dep_tasks(prior, current);
    if !changed_dep_tasks.is_empty() {
        return DecisionResult {
            action: Decision::Run,
            reason: RunReason::DepOutputMismatch {
                tasks: changed_dep_tasks,
            },
        };
    }

    if prior.pkg_dep_hash != current.pkg_dep_hash {
        return DecisionResult {
            action: Decision::Run,
            reason: RunReason::PkgDepMismatch,
        };
    }

    if prior.env_hash != current.env_hash {
        return DecisionResult {
            action: Decision::Run,
            reason: RunReason::EnvMismatch,
        };
    }

    check_patterns_unchanged(prior, current)
}

/// Check if task definition patterns and inputs/outputs are unchanged.
fn check_patterns_unchanged(prior: &TaskRunRecord, current: &CurrentState<'_>) -> DecisionResult {
    let input_patterns = effective_input_patterns(prior, current);
    let output_patterns = effective_output_patterns(prior, current);

    if prior.detected_output_patterns && prior.output_patterns != output_patterns {
        return DecisionResult {
            action: Decision::Run,
            reason: output_pattern_mismatch_reason(prior, current),
        };
    }

    if let Some(reason) = change_reason(
        current.resolver,
        &output_patterns,
        &prior.outputs,
        FileEntryKind::Outputs,
    ) {
        return DecisionResult {
            action: Decision::Run,
            reason,
        };
    }

    if let Some(reason) = change_reason(
        current.resolver,
        &input_patterns,
        &prior.inputs,
        FileEntryKind::Inputs,
    ) {
        return DecisionResult {
            action: Decision::Run,
            reason,
        };
    }

    DecisionResult {
        action: Decision::Skip,
        reason: RunReason::CacheHit,
    }
}

/// Special decision path for shared cache restore eligibility.
///
/// Purpose: Determine whether a shared cache candidate can restore outputs into
/// current tree state. Differs from full `decide()` in one critical way:
///
/// - Full `decide()` validates current outputs match prior outputs. That is correct
///   for local skip decisions, because local cache hit means outputs already exist.
/// - Shared restore needs opposite behavior: outputs may be absent or stale in tree,
///   and restore should still be allowed if all *inputs and cacheability facts* match.
///
/// Returns `true` iff record is safe to restore from shared cache.
///
/// Rules:
/// 1. Same cacheability checks as normal skip path: prior succeeded, task spec/env/
///    package deps/dependency outputs unchanged.
/// 2. Same effective input pattern resolution semantics as normal skip path.
/// 3. Inputs must still match current tree state.
/// 4. Outputs are NOT compared at all.
///
/// This allows cases like:
/// - Full `decide()` would return `Run` because outputs are absent → legitimate
///   shared restore candidate.
/// - Shared snapshot from another clone/commit can hydrate outputs safely when
///   inputs and dependency outputs still match.
#[must_use]
pub fn decide_shared_restore(record: &TaskRunRecord, current: &CurrentState<'_>) -> bool {
    if !cacheable_prior(record, current) {
        return false;
    }

    let input_patterns = effective_input_patterns(record, current);

    patterns_unchanged(
        &record.inputs,
        &input_patterns,
        current.resolver,
        FileEntryKind::Inputs,
    )
}

fn cacheable_prior(prior: &TaskRunRecord, current: &CurrentState<'_>) -> bool {
    prior.succeeded
        && prior.cache_nonce.as_deref() == current.cache_nonce
        && prior.task_spec_hash == current.task_spec_hash
        && prior.env_hash == current.env_hash
        && prior.pkg_dep_hash == current.pkg_dep_hash
        && dependency_outputs_unchanged(prior, current)
}

fn effective_input_patterns(prior: &TaskRunRecord, current: &CurrentState<'_>) -> Vec<String> {
    if prior.detected_input_patterns {
        prior.input_patterns.clone()
    } else {
        current.declared_input_patterns.to_vec()
    }
}

fn effective_output_patterns(prior: &TaskRunRecord, current: &CurrentState<'_>) -> Vec<String> {
    if prior.detected_output_patterns {
        prior.output_patterns.clone()
    } else {
        current.declared_output_patterns.to_vec()
    }
}

fn dependency_outputs_unchanged(prior: &TaskRunRecord, current: &CurrentState<'_>) -> bool {
    prior.dep_outputs == current.dep_outputs
}

fn changed_dep_tasks(prior: &TaskRunRecord, current: &CurrentState<'_>) -> Vec<String> {
    let prior_outputs = &prior.dep_outputs;
    let current_outputs = &current.dep_outputs;

    let mut changed: Vec<String> = prior_outputs
        .iter()
        .filter(|(task, prior_hash)| current_outputs.get(&**task) != Some(prior_hash))
        .map(|(task, _)| task.clone())
        .collect();

    for task in current_outputs.keys() {
        if !prior_outputs.contains_key(task) {
            changed.push(task.clone());
        }
    }

    changed
}

#[derive(Clone, Copy)]
enum FileEntryKind {
    Inputs,
    Outputs,
}

fn patterns_unchanged(
    prior_entries: &[FileEntry],
    patterns: &[String],
    resolver: &dyn FileStateResolver,
    kind: FileEntryKind,
) -> bool {
    let resolved_entries = match kind {
        FileEntryKind::Inputs => resolver.resolve_inputs(patterns),
        FileEntryKind::Outputs => resolver.resolve_outputs(patterns),
    };
    let Ok(resolved_entries) = resolved_entries else {
        return false;
    };
    !files_changed(prior_entries, &resolved_entries)
}

fn output_pattern_mismatch_reason(prior: &TaskRunRecord, current: &CurrentState<'_>) -> RunReason {
    let (changed, truncated, change_count) = files_diff(
        &prior.outputs,
        &resolve_or_empty(
            current.resolver,
            current.declared_output_patterns,
            FileEntryKind::Outputs,
        ),
        DECIDE_FILES_DIFF_LIMIT,
    );
    RunReason::OutputChanged {
        changed,
        truncated,
        change_count,
    }
}

fn change_reason(
    resolver: &dyn FileStateResolver,
    patterns: &[String],
    prior_entries: &[FileEntry],
    kind: FileEntryKind,
) -> Option<RunReason> {
    let resolved_entries = match kind {
        FileEntryKind::Inputs => resolver.resolve_inputs(patterns),
        FileEntryKind::Outputs => resolver.resolve_outputs(patterns),
    };
    let Ok(resolved_entries) = resolved_entries else {
        return Some(match kind {
            FileEntryKind::Inputs => RunReason::InputChanged {
                changed: Vec::new(),
                truncated: false,
                change_count: 0,
            },
            FileEntryKind::Outputs => RunReason::OutputChanged {
                changed: Vec::new(),
                truncated: false,
                change_count: 0,
            },
        });
    };
    if !files_changed(prior_entries, &resolved_entries) {
        return None;
    }
    let (changed, truncated, change_count) =
        files_diff(prior_entries, &resolved_entries, DECIDE_FILES_DIFF_LIMIT);
    Some(match kind {
        FileEntryKind::Inputs => RunReason::InputChanged {
            changed,
            truncated,
            change_count,
        },
        FileEntryKind::Outputs => RunReason::OutputChanged {
            changed,
            truncated,
            change_count,
        },
    })
}

fn resolve_or_empty(
    resolver: &dyn FileStateResolver,
    patterns: &[String],
    kind: FileEntryKind,
) -> Vec<FileEntry> {
    let resolved = match kind {
        FileEntryKind::Inputs => resolver.resolve_inputs(patterns),
        FileEntryKind::Outputs => resolver.resolve_outputs(patterns),
    };
    resolved.unwrap_or_default()
}

fn files_changed(prior_entries: &[FileEntry], current_entries: &[FileEntry]) -> bool {
    files_diff(prior_entries, current_entries, 0).2 > 0
}

pub fn files_diff(
    prior_entries: &[FileEntry],
    current_entries: &[FileEntry],
    limit: usize,
) -> (Vec<FileDelta>, bool, u32) {
    let prior_by_path = prior_entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let current_by_path = current_entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();

    let mut changed = Vec::new();
    let mut change_count = 0_u32;
    let mut seen_paths = std::collections::BTreeSet::new();

    for path in prior_by_path.keys().chain(current_by_path.keys()) {
        let path = *path;
        if !seen_paths.insert(path) {
            continue;
        }
        let prior = prior_by_path.get(path).copied();
        let current = current_by_path.get(path).copied();
        if !path_changed(prior, current) {
            continue;
        }
        change_count = change_count.saturating_add(1);
        if changed.len() < limit {
            changed.push(FileDelta {
                path: path.to_owned(),
                prior_hash: prior.map_or([0; 32], |entry| entry.hash),
                current_hash: current.map_or([0; 32], |entry| entry.hash),
                prior_absent: prior.map(|entry| entry.absent).unwrap_or(true),
                current_absent: current.map(|entry| entry.absent).unwrap_or(true),
            });
        }
    }

    (changed, change_count as usize > limit, change_count)
}

fn path_changed(prior: Option<&FileEntry>, current: Option<&FileEntry>) -> bool {
    match (prior, current) {
        (Some(prior), Some(current)) => file_entry_changed(prior, current),
        (Some(_), None) | (None, Some(_)) => true,
        (None, None) => false,
    }
}

fn file_entry_changed(prior: &FileEntry, current: &FileEntry) -> bool {
    if file_identity_changed(prior, current) {
        return true;
    }
    if prior.absent {
        return false;
    }
    prior.hash != current.hash
}

fn file_identity_changed(prior: &FileEntry, current: &FileEntry) -> bool {
    prior.path != current.path || prior.absent != current.absent
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap, path::Path};

    use crate::{CacheError, FileDelta, FileEntry, RunReason, TaskRunRecord, SCHEMA_VERSION_V4};

    use super::{
        decide, files_diff, CurrentState, Decision, DecisionResult, FileStateResolver,
        DECIDE_FILES_DIFF_LIMIT,
    };

    #[test]
    fn unchanged_everything_skips_with_cache_hit_reason() {
        assert_matching_decision(
            sample_record(),
            |_| {},
            expected_run_decision(RunReason::CacheHit),
        );
    }

    #[test]
    fn missing_prior_runs_with_no_prior_reason() {
        let prior = sample_record();
        let resolver = matching_resolver(&prior);
        let current = current_state(&prior, &resolver);

        assert_eq!(
            decide(None, &current),
            expected_run_decision(RunReason::NoPriorRecord)
        );
    }

    #[test]
    fn failed_prior_runs_with_prior_failed_reason() {
        assert_decision_with_match(
            sample_record(),
            |prior| prior.succeeded = false,
            DecisionResult {
                action: Decision::Run,
                reason: RunReason::PriorFailed,
            },
        );
    }

    #[test]
    fn changed_nonce_runs_with_nonce_changed_reason() {
        assert_matching_decision(
            sample_record(),
            |current| current.cache_nonce = Some("nonce-v2"),
            expected_run_decision(RunReason::NonceChanged),
        );
    }

    #[test]
    fn changed_task_spec_runs_with_reason() {
        assert_mutated_state_runs(
            |state| state.task_spec_hash = [9; 32],
            RunReason::TaskSpecMismatch,
        );
    }

    #[test]
    fn changed_env_runs_with_reason() {
        assert_mutated_state_runs(|state| state.env_hash = [9; 32], RunReason::EnvMismatch);
    }

    #[test]
    fn changed_pkg_dep_runs_with_reason() {
        assert_mutated_state_runs(
            |state| state.pkg_dep_hash = [9; 32],
            RunReason::PkgDepMismatch,
        );
    }

    #[test]
    fn changed_dep_outputs_runs_with_task_names_reason() {
        let prior = sample_record();
        let resolver = matching_resolver(&prior);
        let mut current = current_state(&prior, &resolver);
        current.dep_outputs.insert("dep#build".to_owned(), [9; 32]);
        current.dep_outputs.insert("new#lint".to_owned(), [7; 32]);

        assert_eq!(
            decide(Some(&prior), &current),
            DecisionResult {
                action: Decision::Run,
                reason: RunReason::DepOutputMismatch {
                    tasks: vec!["dep#build".to_owned(), "new#lint".to_owned()],
                },
            }
        );
    }

    #[test]
    fn changed_output_pattern_runs_with_output_changed_reason() {
        let prior = sample_record();
        let current_patterns = vec!["dist/other.js".to_owned()];
        let current_outputs = vec![sample_present_file("dist/other.js", [2; 32])];
        let outputs_by_patterns = BTreeMap::from([(current_patterns.clone(), current_outputs)]);
        let resolver = FixtureResolver::with_patterns(
            prior.inputs.clone(),
            prior.outputs.clone(),
            BTreeMap::new(),
            outputs_by_patterns,
        );
        let mut current = current_state(&prior, &resolver);
        current.declared_output_patterns = &current_patterns;

        assert_changed_reason(
            decide(Some(&prior), &current),
            ExpectedChange {
                changed: vec![
                    FileDelta {
                        path: "dist/app.js".to_owned(),
                        prior_hash: [2; 32],
                        current_hash: [0; 32],
                        prior_absent: false,
                        current_absent: true,
                    },
                    FileDelta {
                        path: "dist/other.js".to_owned(),
                        prior_hash: [0; 32],
                        current_hash: [2; 32],
                        prior_absent: true,
                        current_absent: false,
                    },
                ],
                truncated: false,
                change_count: 2,
                kind: ChangeKind::Output,
            },
        );
        assert_eq!(resolver.output_patterns_calls(), vec![current_patterns]);
    }

    #[test]
    fn changed_output_contents_runs_with_output_changed_reason() {
        assert_file_change_runs(FileChangeCase {
            target: ChangeTarget::Outputs,
            mutate: |entry| entry.size += 1,
            new_hash: [9; 32],
        });
    }

    #[test]
    fn changed_input_contents_runs_with_input_changed_reason() {
        assert_file_change_runs(FileChangeCase {
            target: ChangeTarget::Inputs,
            mutate: |entry| entry.mtime_ns += 1,
            new_hash: [9; 32],
        });
    }

    #[test]
    fn files_diff_truncates_and_detects_hash_absent_and_path_changes() {
        let prior = vec![
            sample_present_file("a", [1; 32]),
            sample_present_file("b", [2; 32]),
            FileEntry::absent("c"),
        ];
        let current = vec![
            sample_present_file("a", [9; 32]),
            FileEntry::absent("b"),
            sample_present_file("d", [4; 32]),
        ];

        let (changed, truncated, total) = files_diff(&prior, &current, 2);

        assert!(truncated);
        assert_eq!(total, 4);
        assert_eq!(
            changed,
            vec![
                FileDelta {
                    path: "a".to_owned(),
                    prior_hash: [1; 32],
                    current_hash: [9; 32],
                    prior_absent: false,
                    current_absent: false,
                },
                FileDelta {
                    path: "b".to_owned(),
                    prior_hash: [2; 32],
                    current_hash: [0; 32],
                    prior_absent: false,
                    current_absent: true,
                },
            ]
        );
    }

    #[test]
    fn files_diff_with_zero_limit_reports_total_without_payload() {
        let prior = vec![sample_present_file("a", [1; 32])];
        let current = vec![sample_present_file("a", [9; 32])];

        let (changed, truncated, total) = files_diff(&prior, &current, 0);

        assert!(changed.is_empty());
        assert!(truncated);
        assert_eq!(total, 1);
    }

    #[test]
    fn files_diff_same_qualified_cross_package_paths_report_no_change() {
        let prior = vec![
            sample_present_file("pkg-a/src/schema.graphql", [1; 32]),
            sample_present_file("pkg-b/src/schema.graphql", [2; 32]),
        ];
        let current = vec![
            sample_present_file("pkg-b/src/schema.graphql", [2; 32]),
            sample_present_file("pkg-a/src/schema.graphql", [1; 32]),
        ];

        let (changed, truncated, total) = files_diff(&prior, &current, 10);

        assert!(changed.is_empty());
        assert!(!truncated);
        assert_eq!(total, 0);
        assert!(!super::files_changed(&prior, &current));
    }

    #[test]
    fn uses_detected_output_patterns_when_present() {
        let mut prior = sample_record();
        prior.detected_output_patterns = true;
        prior.output_patterns = vec!["dist/generated.js".to_owned()];
        let matched_output = sample_present_file("dist/generated.js", [7; 32]);
        prior.outputs = vec![matched_output.clone()];
        let outputs_by_patterns = BTreeMap::from([(
            vec!["dist/generated.js".to_owned()],
            vec![matched_output.clone()],
        )]);
        let resolver = FixtureResolver::with_patterns(
            prior.inputs.clone(),
            vec![matched_output],
            BTreeMap::new(),
            outputs_by_patterns,
        );
        let current_declared_outputs = vec!["dist/declared.js".to_owned()];

        assert_output_pattern_resolution(
            prior,
            Some(current_declared_outputs),
            resolver,
            OutputPatternExpectation {
                decision: Decision::Skip,
                output_pattern_calls: vec![vec!["dist/generated.js".to_owned()]],
            },
        );
    }

    #[test]
    fn declared_outputs_ignore_current_pattern_drift_until_worker_detects_outputs() {
        let prior = sample_record();
        let outputs_by_patterns =
            BTreeMap::from([(vec!["dist/declared.js".to_owned()], prior.outputs.clone())]);
        let resolver = FixtureResolver::with_patterns(
            prior.inputs.clone(),
            prior.outputs.clone(),
            BTreeMap::new(),
            outputs_by_patterns,
        );
        let current_declared_outputs = vec!["dist/declared.js".to_owned()];

        assert_output_pattern_resolution(
            prior,
            Some(current_declared_outputs),
            resolver,
            OutputPatternExpectation {
                decision: Decision::Skip,
                output_pattern_calls: vec![vec!["dist/declared.js".to_owned()]],
            },
        );
    }

    #[test]
    fn removed_output_path_runs() {
        let prior = sample_record();
        let resolver = FixtureResolver::new(prior.inputs.clone(), Vec::new());
        let current = current_state(&prior, &resolver);

        assert_changed_reason(
            decide(Some(&prior), &current),
            ExpectedChange {
                changed: vec![FileDelta {
                    path: "dist/app.js".to_owned(),
                    prior_hash: [2; 32],
                    current_hash: [0; 32],
                    prior_absent: false,
                    current_absent: true,
                }],
                truncated: false,
                change_count: 1,
                kind: ChangeKind::Output,
            },
        );
    }

    #[test]
    fn changed_output_path_runs() {
        assert_file_change_runs(FileChangeCase {
            target: ChangeTarget::Outputs,
            mutate: |entry| entry.path = "dist/other.js".to_owned(),
            new_hash: [9; 32],
        });
    }

    #[test]
    fn changed_input_path_runs() {
        assert_file_change_runs(FileChangeCase {
            target: ChangeTarget::Inputs,
            mutate: |entry| entry.path = "src/other.ts".to_owned(),
            new_hash: [9; 32],
        });
    }

    #[test]
    fn changed_input_metadata_with_different_content_runs_when_resolved_hash_differs() {
        assert_file_change_runs(FileChangeCase {
            target: ChangeTarget::Inputs,
            mutate: |entry| entry.mtime_ns += 1,
            new_hash: [9; 32],
        });
    }

    #[test]
    fn changed_output_metadata_with_different_content_runs_when_resolved_hash_differs() {
        assert_file_change_runs(FileChangeCase {
            target: ChangeTarget::Outputs,
            mutate: |entry| entry.size += 1,
            new_hash: [9; 32],
        });
    }

    enum ChangeKind {
        Input,
        Output,
    }

    fn assert_file_change_runs(case: FileChangeCase) {
        let prior = sample_record();
        let resolver = resolver_for_file_change(case);
        let current = current_state(&prior, &resolver);

        let expected = expected_diff(case);
        let expected_change_count = expected.len() as u32;
        let result = decide(Some(&prior), &current);
        assert_eq!(result.action, Decision::Run);
        match case.target {
            ChangeTarget::Inputs => {
                assert_changed_reason(
                    result,
                    ExpectedChange {
                        changed: expected,
                        truncated: false,
                        change_count: expected_change_count,
                        kind: ChangeKind::Input,
                    },
                );
            }
            ChangeTarget::Outputs => {
                assert_changed_reason(
                    result,
                    ExpectedChange {
                        changed: expected,
                        truncated: false,
                        change_count: expected_change_count,
                        kind: ChangeKind::Output,
                    },
                );
            }
        }
        assert_eq!(resolver.hash_calls(), Vec::<Option<String>>::new());
    }

    fn expected_diff(case: FileChangeCase) -> Vec<FileDelta> {
        let prior = sample_record();
        let prior_entry = match case.target {
            ChangeTarget::Inputs => prior.inputs[0].clone(),
            ChangeTarget::Outputs => prior.outputs[0].clone(),
        };
        let mut current_entry = prior_entry.clone();
        (case.mutate)(&mut current_entry);
        current_entry.hash = case.new_hash;
        if current_entry.path != prior_entry.path {
            vec![
                FileDelta {
                    path: prior_entry.path.clone(),
                    prior_hash: prior_entry.hash,
                    current_hash: [0; 32],
                    prior_absent: prior_entry.absent,
                    current_absent: true,
                },
                FileDelta {
                    path: current_entry.path,
                    prior_hash: [0; 32],
                    current_hash: case.new_hash,
                    prior_absent: true,
                    current_absent: current_entry.absent,
                },
            ]
        } else {
            vec![FileDelta {
                path: prior_entry.path,
                prior_hash: prior_entry.hash,
                current_hash: current_entry.hash,
                prior_absent: prior_entry.absent,
                current_absent: current_entry.absent,
            }]
        }
    }

    fn resolver_for_file_change(case: FileChangeCase) -> FixtureResolver {
        let prior = sample_record();
        let mut changed_inputs = prior.inputs.clone();
        let mut changed_outputs = prior.outputs.clone();
        let entries = match case.target {
            ChangeTarget::Inputs => &mut changed_inputs,
            ChangeTarget::Outputs => &mut changed_outputs,
        };
        (case.mutate)(&mut entries[0]);
        entries[0].hash = case.new_hash;
        FixtureResolver::new(changed_inputs, changed_outputs)
    }

    fn assert_mutated_state_runs(
        mutate: impl FnOnce(&mut CurrentState<'_>),
        expected_reason: RunReason,
    ) {
        let prior = sample_record();
        let resolver = FixtureResolver::new(prior.inputs.clone(), prior.outputs.clone());
        let mut current = current_state(&prior, &resolver);
        mutate(&mut current);

        assert_eq!(
            decide(Some(&prior), &current),
            DecisionResult {
                action: Decision::Run,
                reason: expected_reason,
            }
        );
    }

    /// Expected outcomes for output pattern resolution tests.
    struct OutputPatternExpectation {
        decision: Decision,
        output_pattern_calls: Vec<Vec<String>>,
    }

    fn assert_output_pattern_resolution(
        prior: TaskRunRecord,
        declared_output_patterns: Option<Vec<String>>,
        resolver: FixtureResolver,
        expected: OutputPatternExpectation,
    ) {
        let mut current = current_state(&prior, &resolver);
        if let Some(patterns) = declared_output_patterns.as_deref() {
            current.declared_output_patterns = patterns;
        }

        assert_eq!(decide(Some(&prior), &current).action, expected.decision);
        assert_eq!(
            resolver.output_patterns_calls(),
            expected.output_pattern_calls
        );
        assert_eq!(resolver.hash_calls(), Vec::<Option<String>>::new());
    }

    fn expected_run_decision(reason: RunReason) -> DecisionResult {
        let action = match reason {
            RunReason::CacheHit | RunReason::SharedCacheHit => Decision::Skip,
            _ => Decision::Run,
        };
        DecisionResult { action, reason }
    }

    struct ExpectedChange {
        changed: Vec<FileDelta>,
        truncated: bool,
        change_count: u32,
        kind: ChangeKind,
    }

    fn assert_changed_reason(result: DecisionResult, expected: ExpectedChange) {
        let ExpectedChange {
            changed,
            truncated,
            change_count,
            kind,
        } = expected;
        let expected_reason = match kind {
            ChangeKind::Input => RunReason::InputChanged {
                changed,
                truncated,
                change_count,
            },
            ChangeKind::Output => RunReason::OutputChanged {
                changed,
                truncated,
                change_count,
            },
        };
        assert_run_reason(result, expected_reason);
    }

    fn assert_run_reason(result: DecisionResult, expected_reason: RunReason) {
        assert_eq!(result.action, Decision::Run);
        assert_eq!(result.reason, expected_reason);
    }

    fn current_state<'a>(
        prior: &'a TaskRunRecord,
        resolver: &'a dyn FileStateResolver,
    ) -> CurrentState<'a> {
        CurrentState {
            task_spec_hash: prior.task_spec_hash,
            env_hash: prior.env_hash,
            pkg_dep_hash: prior.pkg_dep_hash,
            dep_outputs: prior.dep_outputs.clone(),
            cache_nonce: prior.cache_nonce.as_deref(),
            declared_input_patterns: &prior.input_patterns,
            declared_output_patterns: &prior.output_patterns,
            resolver,
        }
    }

    fn sample_record() -> TaskRunRecord {
        TaskRunRecord {
            schema_version: SCHEMA_VERSION_V4,
            task_spec_hash: [1; 32],
            input_patterns: vec!["src/**/*.ts".to_owned()],
            inputs: vec![sample_present_file("src/main.ts", [3; 32])],
            output_patterns: vec!["dist/app.js".to_owned()],
            outputs: vec![sample_present_file("dist/app.js", [2; 32])],
            detected_input_patterns: false,
            detected_output_patterns: false,
            outputs_hash: [4; 32],
            env_hash: [5; 32],
            pkg_dep_hash: [6; 32],
            dep_outputs: BTreeMap::from([("dep#build".to_owned(), [7; 32])]),
            exit_status: 0,
            succeeded: true,
            start_unix_ms: 1,
            end_unix_ms: 2,
            reports: Vec::new(),
            cache_nonce: Some("nonce-v1".to_owned()),
            run_reason: None,
        }
    }

    #[test]
    fn decide_skip_with_qualified_cross_package_inputs() {
        let mut prior = sample_record();
        prior.input_patterns = vec![
            "pkg-a/src/schema.graphql".to_owned(),
            "pkg-b/src/schema.graphql".to_owned(),
        ];
        prior.inputs = vec![
            sample_present_file("pkg-a/src/schema.graphql", [1; 32]),
            sample_present_file("pkg-b/src/schema.graphql", [2; 32]),
        ];

        let resolver = FixtureResolver::new(prior.inputs.clone(), prior.outputs.clone());
        let current = current_state(&prior, &resolver);

        assert_eq!(
            decide(Some(&prior), &current),
            DecisionResult {
                action: Decision::Skip,
                reason: RunReason::CacheHit,
            }
        );
    }

    fn sample_present_file(path: &str, hash: [u8; 32]) -> FileEntry {
        FileEntry {
            path: path.to_owned(),
            size: 1,
            mtime_ns: 1,
            hash,
            absent: false,
        }
    }

    #[derive(Clone, Copy)]
    enum ChangeTarget {
        Inputs,
        Outputs,
    }

    #[derive(Clone, Copy)]
    struct FileChangeCase {
        target: ChangeTarget,
        mutate: fn(&mut FileEntry),
        new_hash: [u8; 32],
    }

    /// Creates a resolver with matching inputs/outputs for cache-hit scenarios.
    fn matching_resolver(record: &TaskRunRecord) -> FixtureResolver {
        FixtureResolver::new(record.inputs.clone(), record.outputs.clone())
    }

    /// Helper for testing decide() results with matching state.
    fn assert_decision_with_match(
        prior: TaskRunRecord,
        prior_mut: impl FnOnce(&mut TaskRunRecord),
        expected: DecisionResult,
    ) {
        let mut prior = prior;
        prior_mut(&mut prior);
        let resolver = matching_resolver(&prior);
        let current = current_state(&prior, &resolver);
        assert_eq!(decide(Some(&prior), &current), expected);
    }

    fn assert_matching_decision(
        prior: TaskRunRecord,
        mutate_current: impl FnOnce(&mut CurrentState<'_>),
        expected: DecisionResult,
    ) {
        let resolver = matching_resolver(&prior);
        let mut current = current_state(&prior, &resolver);
        mutate_current(&mut current);
        assert_eq!(decide(Some(&prior), &current), expected);
    }

    struct FixtureResolver {
        default_inputs: Vec<FileEntry>,
        default_outputs: Vec<FileEntry>,
        inputs_by_patterns: BTreeMap<Vec<String>, Vec<FileEntry>>,
        outputs_by_patterns: BTreeMap<Vec<String>, Vec<FileEntry>>,
        input_pattern_calls: RefCell<Vec<Vec<String>>>,
        output_pattern_calls: RefCell<Vec<Vec<String>>>,
        hash_calls: RefCell<Vec<Option<String>>>,
    }

    impl FixtureResolver {
        fn new(inputs: Vec<FileEntry>, outputs: Vec<FileEntry>) -> Self {
            Self::with_patterns(inputs, outputs, BTreeMap::new(), BTreeMap::new())
        }

        fn with_patterns(
            inputs: Vec<FileEntry>,
            outputs: Vec<FileEntry>,
            inputs_by_patterns: BTreeMap<Vec<String>, Vec<FileEntry>>,
            outputs_by_patterns: BTreeMap<Vec<String>, Vec<FileEntry>>,
        ) -> Self {
            Self {
                default_inputs: inputs,
                default_outputs: outputs,
                inputs_by_patterns,
                outputs_by_patterns,
                input_pattern_calls: RefCell::new(Vec::new()),
                output_pattern_calls: RefCell::new(Vec::new()),
                hash_calls: RefCell::new(Vec::new()),
            }
        }

        fn output_patterns_calls(&self) -> Vec<Vec<String>> {
            self.output_pattern_calls.borrow().clone()
        }

        fn hash_calls(&self) -> Vec<Option<String>> {
            self.hash_calls.borrow().clone()
        }
    }

    impl FileStateResolver for FixtureResolver {
        fn resolve_inputs(&self, patterns: &[String]) -> crate::Result<Vec<FileEntry>> {
            self.input_pattern_calls
                .borrow_mut()
                .push(patterns.to_vec());
            Ok(self
                .inputs_by_patterns
                .get(patterns)
                .cloned()
                .unwrap_or_else(|| self.default_inputs.clone()))
        }

        fn resolve_outputs(&self, patterns: &[String]) -> crate::Result<Vec<FileEntry>> {
            self.output_pattern_calls
                .borrow_mut()
                .push(patterns.to_vec());
            Ok(self
                .outputs_by_patterns
                .get(patterns)
                .cloned()
                .unwrap_or_else(|| self.default_outputs.clone()))
        }

        fn blake3_file(&self, path: &Path) -> crate::Result<[u8; 32]> {
            self.hash_calls
                .borrow_mut()
                .push(path.to_str().map(str::to_owned));
            Err(CacheError::InputExpansion(
                "hashing should not be needed".to_owned(),
            ))
        }
    }

    #[test]
    fn decide_resolver_error_forces_run() {
        struct ErrorResolver;

        impl FileStateResolver for ErrorResolver {
            fn resolve_inputs(&self, _: &[String]) -> crate::Result<Vec<FileEntry>> {
                Err(CacheError::InputExpansion(
                    "input resolver failed".to_owned(),
                ))
            }

            fn resolve_outputs(&self, _: &[String]) -> crate::Result<Vec<FileEntry>> {
                Err(CacheError::InputExpansion(
                    "output resolver failed".to_owned(),
                ))
            }

            fn blake3_file(&self, path: &Path) -> crate::Result<[u8; 32]> {
                Err(CacheError::InputExpansion(format!(
                    "hashing should not be needed: {}",
                    path.display()
                )))
            }
        }

        let prior = sample_record();
        let resolver = ErrorResolver;
        let current = current_state(&prior, &resolver);

        let result = decide(Some(&prior), &current);
        assert_eq!(
            result.action,
            Decision::Run,
            "resolver errors must force rerun instead of skip"
        );
        assert_eq!(
            result.reason,
            RunReason::OutputChanged {
                changed: Vec::new(),
                truncated: false,
                change_count: 0,
            },
            "output resolver errors should force rerun via output-changed reason because outputs are checked first"
        );
    }

    // === Tests for decide_shared_restore ===

    #[test]
    fn shared_restore_allows_absent_outputs_when_inputs_match() {
        let prior = sample_record();
        let resolver = FixtureResolver::new(prior.inputs.clone(), vec![]);
        let current = current_state(&prior, &resolver);

        assert!(
            super::decide_shared_restore(&prior, &current),
            "should allow shared restore when outputs are absent but inputs match"
        );
        assert_eq!(
            decide(Some(&prior), &current).action,
            Decision::Run,
            "sanity check: full decide() returns Run when outputs absent"
        );
    }

    #[test]
    fn shared_restore_input_mismatch_returns_false() {
        let prior = sample_record();
        let mut changed_inputs = prior.inputs.clone();
        changed_inputs[0].hash = [9; 32];
        let resolver = FixtureResolver::new(changed_inputs, vec![]);
        let current = current_state(&prior, &resolver);

        assert!(
            !super::decide_shared_restore(&prior, &current),
            "should reject restore when inputs differ"
        );
    }

    #[test]
    fn shared_restore_task_spec_mismatch_returns_false() {
        let prior = sample_record();
        let resolver = FixtureResolver::new(prior.inputs.clone(), vec![]);
        let mut current = current_state(&prior, &resolver);
        current.task_spec_hash = [9; 32];

        assert!(
            !super::decide_shared_restore(&prior, &current),
            "should reject restore when task_spec_hash differs"
        );
    }

    #[test]
    fn shared_restore_env_mismatch_returns_false() {
        let prior = sample_record();
        let resolver = FixtureResolver::new(prior.inputs.clone(), vec![]);
        let mut current = current_state(&prior, &resolver);
        current.env_hash = [9; 32];

        assert!(
            !super::decide_shared_restore(&prior, &current),
            "should reject restore when env_hash differs"
        );
    }

    #[test]
    fn shared_restore_pkg_dep_mismatch_returns_false() {
        let prior = sample_record();
        let resolver = FixtureResolver::new(prior.inputs.clone(), vec![]);
        let mut current = current_state(&prior, &resolver);
        current.pkg_dep_hash = [9; 32];

        assert!(
            !super::decide_shared_restore(&prior, &current),
            "should reject restore when pkg_dep_hash differs"
        );
    }

    #[test]
    fn shared_restore_dep_outputs_mismatch_returns_false() {
        let prior = sample_record();
        let resolver = FixtureResolver::new(prior.inputs.clone(), vec![]);
        let mut current = current_state(&prior, &resolver);
        current.dep_outputs.insert("dep#build".to_owned(), [9; 32]);

        assert!(
            !super::decide_shared_restore(&prior, &current),
            "should reject restore when dep_outputs differs"
        );
    }

    #[test]
    fn shared_restore_failed_prior_returns_false() {
        assert_shared_restore_rejects(
            sample_record(),
            |prior| prior.succeeded = false,
            |_| {},
            "should reject restore when prior failed",
        );
    }

    #[test]
    fn shared_restore_outputs_present_also_returns_true() {
        let prior = sample_record();
        let resolver = matching_resolver(&prior);
        let current = current_state(&prior, &resolver);

        assert!(
            super::decide_shared_restore(&prior, &current),
            "should allow restore when inputs match and outputs present"
        );
    }

    #[test]
    fn shared_restore_nonce_mismatch_returns_false() {
        let mut prior = sample_record();
        prior.cache_nonce = Some("nonce-v2".to_owned());
        assert_shared_restore_rejects(
            prior,
            |_| {},
            |current| current.cache_nonce = Some("nonce-v1"),
            "should reject restore when nonce differs",
        );
    }

    /// Helper for testing that decide_shared_restore rejects a condition.
    fn assert_shared_restore_rejects(
        prior: TaskRunRecord,
        prior_mut: impl FnOnce(&mut TaskRunRecord),
        current_mut: impl FnOnce(&mut CurrentState<'_>),
        message: &str,
    ) {
        let mut prior = prior;
        prior_mut(&mut prior);
        let resolver = matching_resolver(&prior);
        let mut current = current_state(&prior, &resolver);
        current_mut(&mut current);
        assert!(
            !super::decide_shared_restore(&prior, &current),
            "{}",
            message
        );
    }

    #[test]
    fn decide_files_diff_limit_constant_is_50() {
        assert_eq!(DECIDE_FILES_DIFF_LIMIT, 50);
    }
}
