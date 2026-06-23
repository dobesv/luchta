use std::{collections::BTreeMap, path::Path};

use crate::{FileEntry, TaskRunRecord};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Skip,
    SharedHit,
    Run,
}

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
    pub declared_input_patterns: &'a [String],
    pub declared_output_patterns: &'a [String],
    pub resolver: &'a dyn FileStateResolver,
}

pub fn decide(prior: Option<&TaskRunRecord>, current: &CurrentState<'_>) -> Decision {
    let Some(prior) = prior else {
        return Decision::Run;
    };

    if !cacheable_prior(prior, current) {
        return Decision::Run;
    }

    let input_patterns = effective_input_patterns(prior, current);
    let output_patterns = effective_output_patterns(prior, current);

    if !declared_outputs_match_prior(prior, &output_patterns) {
        return Decision::Run;
    }
    if !patterns_unchanged(
        &prior.inputs,
        &input_patterns,
        current.resolver,
        FileEntryKind::Inputs,
    ) {
        return Decision::Run;
    }
    if !patterns_unchanged(
        &prior.outputs,
        &output_patterns,
        current.resolver,
        FileEntryKind::Outputs,
    ) {
        return Decision::Run;
    }

    Decision::Skip
}

/// Validates a shared-cache candidate for RESTORE: like `decide` but does NOT
/// require the current-tree OUTPUTS to be present/match (the restore will
/// provide them from the content-addressed blob).
///
/// This is the correct validation for a shared restore because:
/// - On a shared restore, outputs DON'T EXIST in the work tree yet (we're
///   ABOUT to restore them from the blob).
/// - Full `decide()` would return `Run` because outputs are absent → legitimate
///   shared hits get REJECTED.
/// - Output integrity is inherent: the blob is content-addressed by
///   `outputs_hash`, so restored outputs are exactly what was stored.
///
/// Returns `true` if the candidate is content-valid to restore.
pub fn decide_shared_restore(record: &TaskRunRecord, current: &CurrentState<'_>) -> bool {
    // 1. Check cacheable_prior (succeeded, task_spec_hash, env_hash, pkg_dep_hash, dep_outputs)
    if !cacheable_prior(record, current) {
        return false;
    }

    // 2. Resolve and check INPUTS only (not outputs)
    let input_patterns = effective_input_patterns(record, current);
    if !patterns_unchanged(
        &record.inputs,
        &input_patterns,
        current.resolver,
        FileEntryKind::Inputs,
    ) {
        return false;
    }

    // 3. Skip output validation — outputs will be restored from content-addressed blob
    true
}

fn cacheable_prior(prior: &TaskRunRecord, current: &CurrentState<'_>) -> bool {
    prior.succeeded
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

fn declared_outputs_match_prior(prior: &TaskRunRecord, output_patterns: &[String]) -> bool {
    if prior.detected_output_patterns {
        prior.output_patterns == output_patterns
    } else {
        true
    }
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

fn files_changed(prior_entries: &[FileEntry], current_entries: &[FileEntry]) -> bool {
    if prior_entries.len() != current_entries.len() {
        return true;
    }

    let prior_by_path = prior_entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let current_by_path = current_entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();

    if prior_by_path.len() != prior_entries.len() || current_by_path.len() != current_entries.len()
    {
        return true;
    }

    for (path, prior) in &prior_by_path {
        let Some(current) = current_by_path.get(path) else {
            return true;
        };
        if file_entry_changed(prior, current) {
            return true;
        }
    }

    false
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

    use crate::{CacheError, FileEntry, TaskRunRecord, SCHEMA_VERSION_V3};

    use super::{decide, CurrentState, Decision, FileStateResolver};

    #[test]
    fn unchanged_everything_skips() {
        let prior = sample_record();
        let resolver = FixtureResolver::new(prior.inputs.clone(), prior.outputs.clone());
        let current = current_state(&prior, &resolver);

        assert_eq!(decide(Some(&prior), &current), Decision::Skip);
    }

    #[test]
    fn missing_prior_runs() {
        let prior = sample_record();
        let resolver = FixtureResolver::new(prior.inputs.clone(), prior.outputs.clone());
        let current = current_state(&prior, &resolver);

        assert_eq!(decide(None, &current), Decision::Run);
    }

    #[test]
    fn failed_prior_runs() {
        let mut prior = sample_record();
        prior.succeeded = false;
        let resolver = FixtureResolver::new(prior.inputs.clone(), prior.outputs.clone());
        let current = current_state(&prior, &resolver);

        // A previously-failed task must always rerun, even when every hash and
        // resolved file set is unchanged (decide rule 1).
        assert_eq!(decide(Some(&prior), &current), Decision::Run);
    }

    #[test]
    fn changed_task_spec_runs() {
        assert_mutated_state_runs(|state| state.task_spec_hash = [9; 32]);
    }

    #[test]
    fn changed_env_runs() {
        assert_mutated_state_runs(|state| state.env_hash = [9; 32]);
    }

    #[test]
    fn changed_pkg_dep_runs() {
        assert_mutated_state_runs(|state| state.pkg_dep_hash = [9; 32]);
    }

    #[test]
    fn changed_dependency_outputs_run() {
        assert_mutated_state_runs(|state| {
            state.dep_outputs.insert("dep#build".to_owned(), [8; 32]);
        });
    }

    #[test]
    fn changed_input_patterns_run() {
        let mut prior = sample_record();
        prior.detected_input_patterns = true;
        let resolver = FixtureResolver::new(prior.inputs.clone(), prior.outputs.clone());
        let changed_patterns = ["src/**/*.tsx".to_owned()];
        let mut current = current_state(&prior, &resolver);
        current.declared_input_patterns = &changed_patterns;

        assert_eq!(decide(Some(&prior), &current), Decision::Skip);
        assert_eq!(
            resolver.input_patterns_calls(),
            vec![vec!["src/**/*.ts".to_owned()]]
        );
    }

    #[test]
    fn changed_declared_input_patterns_run_when_not_detected() {
        let prior = sample_record();
        let changed_patterns = ["src/**/*.tsx".to_owned()];
        let resolver = FixtureResolver::with_patterns(
            prior.inputs.clone(),
            prior.outputs.clone(),
            BTreeMap::from([(
                changed_patterns.to_vec(),
                vec![file_entry("src/main.tsx", 11, 110, [8; 32])],
            )]),
            BTreeMap::new(),
        );
        let mut current = current_state(&prior, &resolver);
        current.declared_input_patterns = &changed_patterns;

        assert_eq!(decide(Some(&prior), &current), Decision::Run);
        assert_eq!(
            resolver.input_patterns_calls(),
            vec![changed_patterns.to_vec()]
        );
    }

    #[test]
    fn detected_input_patterns_do_not_fall_back_to_declared_inputs() {
        let mut prior = sample_record();
        prior.detected_input_patterns = true;
        prior.input_patterns = vec!["package.json".to_owned()];
        prior.inputs = vec![file_entry("package.json", 10, 100, [2; 32])];
        let resolver = FixtureResolver::with_patterns(
            prior.inputs.clone(),
            prior.outputs.clone(),
            BTreeMap::from([
                (
                    vec!["package.json".to_owned()],
                    vec![file_entry("package.json", 10, 100, [2; 32])],
                ),
                (
                    vec!["src/**/*.tsx".to_owned()],
                    vec![file_entry("src/main.tsx", 10, 100, [9; 32])],
                ),
            ]),
            BTreeMap::new(),
        );
        let changed_patterns = ["src/**/*.tsx".to_owned()];
        let mut current = current_state(&prior, &resolver);
        current.declared_input_patterns = &changed_patterns;

        assert_eq!(decide(Some(&prior), &current), Decision::Skip);
        assert_eq!(
            resolver.input_patterns_calls(),
            vec![vec!["package.json".to_owned()]]
        );
    }

    #[test]
    fn changed_output_patterns_run_when_detected() {
        let mut prior = sample_record();
        prior.detected_output_patterns = true;
        // detected_output_patterns == true means the EFFECTIVE output patterns are the
        // prior detected ones ("dist/**/*.js"). Re-resolving those now yields a
        // changed output set, which must force a rerun.
        let resolver = FixtureResolver::with_patterns(
            prior.inputs.clone(),
            prior.outputs.clone(),
            BTreeMap::new(),
            BTreeMap::from([(
                vec!["dist/**/*.js".to_owned()],
                vec![file_entry("dist/app.js", 20, 200, [9; 32])],
            )]),
        );
        let changed_patterns = ["build/out.txt".to_owned()];
        let mut current = current_state(&prior, &resolver);
        current.declared_output_patterns = &changed_patterns;

        assert_eq!(decide(Some(&prior), &current), Decision::Run);
        assert_eq!(
            resolver.output_patterns_calls(),
            vec![vec!["dist/**/*.js".to_owned()]]
        );
    }

    #[test]
    fn unchanged_outputs_with_detected_patterns_skip() {
        let mut prior = sample_record();
        prior.detected_output_patterns = true;
        let patterns = prior.output_patterns.clone();

        assert_output_pattern_resolution(
            prior,
            BTreeMap::from([(patterns.clone(), sample_record().outputs)]),
            None,
            Decision::Skip,
            vec![patterns],
        );
    }

    #[test]
    fn undetected_output_pattern_changes_still_skip_with_same_resolved_outputs() {
        let prior = sample_record();

        assert_output_pattern_resolution(
            prior,
            BTreeMap::from([(vec!["build/out.txt".to_owned()], sample_record().outputs)]),
            Some(vec!["build/out.txt".to_owned()]),
            Decision::Skip,
            vec![vec!["build/out.txt".to_owned()]],
        );
    }

    #[test]
    fn changed_outputs_with_undetected_patterns_run_when_resolved_set_changes() {
        let prior = sample_record();

        assert_output_pattern_resolution(
            prior,
            BTreeMap::from([(
                vec!["build/out.txt".to_owned()],
                vec![file_entry("build/out.txt", 20, 200, [3; 32])],
            )]),
            Some(vec!["build/out.txt".to_owned()]),
            Decision::Run,
            vec![vec!["build/out.txt".to_owned()]],
        );
    }

    #[test]
    fn missing_output_in_resolved_set_runs() {
        let prior = sample_record();
        let missing = vec![FileEntry::absent("dist/app.js")];
        let resolver = FixtureResolver::new(prior.inputs.clone(), missing);
        let current = current_state(&prior, &resolver);

        assert_eq!(decide(Some(&prior), &current), Decision::Run);
    }

    #[test]
    fn changed_output_metadata_with_same_resolved_hash_skips() {
        let prior = sample_record();
        let mut changed_outputs = prior.outputs.clone();
        changed_outputs[0].mtime_ns += 1;
        let resolver = FixtureResolver::new(prior.inputs.clone(), changed_outputs);
        let current = current_state(&prior, &resolver);

        assert_eq!(decide(Some(&prior), &current), Decision::Skip);
        assert_eq!(resolver.hash_calls(), Vec::<Option<String>>::new());
    }

    #[test]
    fn changed_output_hash_with_same_metadata_runs() {
        let prior = sample_record();
        let mut changed_outputs = prior.outputs.clone();
        changed_outputs[0].hash = [9; 32];
        let resolver = FixtureResolver::new(prior.inputs.clone(), changed_outputs);
        let current = current_state(&prior, &resolver);

        assert_eq!(decide(Some(&prior), &current), Decision::Run);
        assert_eq!(resolver.hash_calls(), Vec::<Option<String>>::new());
    }

    #[test]
    fn changed_absent_flag_runs() {
        assert_file_change_runs(FileChangeCase {
            target: ChangeTarget::Outputs,
            mutate: |entry| entry.absent = true,
            new_hash: [9; 32],
        });
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

    fn assert_file_change_runs(case: FileChangeCase) {
        let prior = sample_record();
        let resolver = resolver_for_file_change(case);
        let current = current_state(&prior, &resolver);

        assert_eq!(decide(Some(&prior), &current), Decision::Run);
        assert_eq!(resolver.hash_calls(), Vec::<Option<String>>::new());
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

    fn assert_mutated_state_runs(mutate: impl FnOnce(&mut CurrentState<'_>)) {
        let prior = sample_record();
        let resolver = FixtureResolver::new(prior.inputs.clone(), prior.outputs.clone());
        let mut current = current_state(&prior, &resolver);
        mutate(&mut current);

        assert_eq!(decide(Some(&prior), &current), Decision::Run);
    }

    fn assert_output_pattern_resolution(
        prior: TaskRunRecord,
        outputs_by_patterns: BTreeMap<Vec<String>, Vec<FileEntry>>,
        declared_output_patterns: Option<Vec<String>>,
        expected_decision: Decision,
        expected_output_pattern_calls: Vec<Vec<String>>,
    ) {
        let resolver = FixtureResolver::with_patterns(
            prior.inputs.clone(),
            prior.outputs.clone(),
            BTreeMap::new(),
            outputs_by_patterns,
        );
        let changed_patterns = declared_output_patterns;
        let mut current = current_state(&prior, &resolver);
        if let Some(patterns) = changed_patterns.as_deref() {
            current.declared_output_patterns = patterns;
        }

        assert_eq!(decide(Some(&prior), &current), expected_decision);
        assert_eq!(
            resolver.output_patterns_calls(),
            expected_output_pattern_calls
        );
        assert_eq!(resolver.hash_calls(), Vec::<Option<String>>::new());
    }
    fn current_state<'a>(
        prior: &'a TaskRunRecord,
        resolver: &'a FixtureResolver,
    ) -> CurrentState<'a> {
        CurrentState {
            task_spec_hash: prior.task_spec_hash,
            env_hash: prior.env_hash,
            pkg_dep_hash: prior.pkg_dep_hash,
            dep_outputs: prior.dep_outputs.clone(),
            declared_input_patterns: &prior.input_patterns,
            declared_output_patterns: &prior.output_patterns,
            resolver,
        }
    }

    fn sample_record() -> TaskRunRecord {
        TaskRunRecord {
            schema_version: SCHEMA_VERSION_V3,
            task_spec_hash: [1; 32],
            input_patterns: vec!["src/**/*.ts".to_owned()],
            inputs: vec![file_entry("src/main.ts", 10, 100, [2; 32])],
            output_patterns: vec!["dist/**/*.js".to_owned()],
            outputs: vec![file_entry("dist/app.js", 20, 200, [3; 32])],
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
            reports: vec![],
            cache_nonce: None,
        }
    }

    fn file_entry(path: &str, size: u64, mtime_ns: i128, hash: [u8; 32]) -> FileEntry {
        FileEntry {
            path: path.to_owned(),
            size,
            mtime_ns,
            hash,
            absent: false,
        }
    }

    #[derive(Clone)]
    struct FixtureResolver {
        default_inputs: Vec<FileEntry>,
        default_outputs: Vec<FileEntry>,
        inputs_by_patterns: BTreeMap<Vec<String>, Vec<FileEntry>>,
        outputs_by_patterns: BTreeMap<Vec<String>, Vec<FileEntry>>,
        hash_overrides: BTreeMap<String, [u8; 32]>,
        hash_calls: RefCell<Vec<String>>,
        input_patterns_calls: RefCell<Vec<Vec<String>>>,
        output_patterns_calls: RefCell<Vec<Vec<String>>>,
    }
    impl FixtureResolver {
        fn new(default_inputs: Vec<FileEntry>, default_outputs: Vec<FileEntry>) -> Self {
            Self::with_patterns(
                default_inputs,
                default_outputs,
                BTreeMap::new(),
                BTreeMap::new(),
            )
        }

        fn with_patterns(
            default_inputs: Vec<FileEntry>,
            default_outputs: Vec<FileEntry>,
            inputs_by_patterns: BTreeMap<Vec<String>, Vec<FileEntry>>,
            outputs_by_patterns: BTreeMap<Vec<String>, Vec<FileEntry>>,
        ) -> Self {
            Self {
                default_inputs,
                default_outputs,
                inputs_by_patterns,
                outputs_by_patterns,
                hash_overrides: BTreeMap::new(),
                hash_calls: RefCell::new(Vec::new()),
                input_patterns_calls: RefCell::new(Vec::new()),
                output_patterns_calls: RefCell::new(Vec::new()),
            }
        }

        fn hash_calls(&self) -> Vec<Option<String>> {
            self.hash_calls.borrow().iter().cloned().map(Some).collect()
        }

        fn input_patterns_calls(&self) -> Vec<Vec<String>> {
            self.input_patterns_calls.borrow().clone()
        }

        fn output_patterns_calls(&self) -> Vec<Vec<String>> {
            self.output_patterns_calls.borrow().clone()
        }

        fn resolve_entries(
            by_patterns: &BTreeMap<Vec<String>, Vec<FileEntry>>,
            defaults: &[FileEntry],
            patterns: &[String],
        ) -> Vec<FileEntry> {
            by_patterns
                .get(patterns)
                .cloned()
                .unwrap_or_else(|| defaults.to_vec())
        }
    }

    impl FileStateResolver for FixtureResolver {
        fn resolve_inputs(&self, patterns: &[String]) -> crate::Result<Vec<FileEntry>> {
            self.input_patterns_calls
                .borrow_mut()
                .push(patterns.to_vec());
            Ok(Self::resolve_entries(
                &self.inputs_by_patterns,
                &self.default_inputs,
                patterns,
            ))
        }

        fn resolve_outputs(&self, patterns: &[String]) -> crate::Result<Vec<FileEntry>> {
            self.output_patterns_calls
                .borrow_mut()
                .push(patterns.to_vec());
            Ok(Self::resolve_entries(
                &self.outputs_by_patterns,
                &self.default_outputs,
                patterns,
            ))
        }

        fn blake3_file(&self, path: &Path) -> crate::Result<[u8; 32]> {
            let normalized = path.to_string_lossy().replace('\\', "/");
            self.hash_calls.borrow_mut().push(normalized.clone());
            if let Some(hash) = self.hash_overrides.get(&normalized) {
                return Ok(*hash);
            }
            self.default_inputs
                .iter()
                .chain(self.default_outputs.iter())
                .find(|entry| entry.path == normalized)
                .map(|entry| entry.hash)
                .ok_or_else(|| {
                    CacheError::Git(format!("unknown file requested for hash: {normalized}"))
                })
        }
    }

    struct FileChangeCase {
        target: ChangeTarget,
        mutate: fn(&mut FileEntry),
        new_hash: [u8; 32],
    }

    enum ChangeTarget {
        Inputs,
        Outputs,
    }

    // === Tests for decide_shared_restore ===

    #[test]
    fn shared_restore_inputs_match_outputs_absent_returns_true() {
        // KEY CASE: inputs match, outputs ABSENT in current tree → should return true
        // This is the case full decide() got wrong (it would return Run because outputs absent)
        let prior = sample_record();
        // Empty outputs = outputs don't exist in current tree
        let resolver = FixtureResolver::new(prior.inputs.clone(), vec![]);
        let current = current_state(&prior, &resolver);

        assert!(
            super::decide_shared_restore(&prior, &current),
            "should allow restore when inputs match and outputs absent"
        );
        // Verify: full decide() would return Run because outputs don't match
        assert_eq!(
            decide(Some(&prior), &current),
            Decision::Run,
            "sanity check: full decide() returns Run when outputs absent"
        );
    }

    #[test]
    fn shared_restore_input_content_differs_returns_false() {
        // Input content hash differs → should reject restore
        let prior = sample_record();
        let mut changed_inputs = prior.inputs.clone();
        changed_inputs[0].hash = [9; 32]; // Different content hash
        let resolver = FixtureResolver::new(changed_inputs, vec![]);
        let current = current_state(&prior, &resolver);

        assert!(
            !super::decide_shared_restore(&prior, &current),
            "should reject restore when input content differs"
        );
    }

    #[test]
    fn shared_restore_task_spec_mismatch_returns_false() {
        // task_spec_hash mismatch → should reject
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
        // env_hash mismatch → should reject
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
        // pkg_dep_hash mismatch → should reject
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
        // dep_outputs mismatch → should reject
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
        // Prior task failed → should reject
        let mut prior = sample_record();
        prior.succeeded = false;
        let resolver = FixtureResolver::new(prior.inputs.clone(), vec![]);
        let current = current_state(&prior, &resolver);

        assert!(
            !super::decide_shared_restore(&prior, &current),
            "should reject restore when prior failed"
        );
    }

    #[test]
    fn shared_restore_outputs_present_also_returns_true() {
        // Outputs already present and matching → should also allow restore
        // (This handles same-commit restore case where outputs exist)
        let prior = sample_record();
        let resolver = FixtureResolver::new(prior.inputs.clone(), prior.outputs.clone());
        let current = current_state(&prior, &resolver);

        assert!(
            super::decide_shared_restore(&prior, &current),
            "should allow restore when inputs match and outputs present"
        );
    }
}
