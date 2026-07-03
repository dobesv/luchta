use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use globset::GlobSet;
use luchta_cache::{blake3_file, TaskRunRecord};
use luchta_types::{PackageName, TaskId};
use luchta_workspace::PackageNode;
use miette::Result;

use crate::run::build_globset;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputFingerprint {
    pub mtime_ns: i128,
    pub size: u64,
    pub hash: [u8; 32],
}

#[derive(Debug, Clone)]
pub(crate) struct TaskWatchState {
    pub package: PackageName,
    pub package_dir: PathBuf,
    pub input_globset: GlobSet,
    /// Output globs for this task. Used to exclude a task's own produced/restored
    /// outputs from triggering rebuilds (notably for the undeclared-input
    /// fallback), so restoring cache outputs can never re-trigger a build (#161).
    pub output_globset: GlobSet,
    pub inputs: HashMap<PathBuf, InputFingerprint>,
}

pub(crate) type TaskWatchRegistry = Arc<Mutex<HashMap<TaskId, TaskWatchState>>>;

pub(crate) fn empty_task_watch_registry() -> TaskWatchRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

pub(crate) fn retain_task_watch_registry_task_ids(
    registry: &TaskWatchRegistry,
    live_task_ids: &HashSet<TaskId>,
) {
    registry
        .lock()
        .expect("task watch registry mutex poisoned")
        .retain(|task_id, _| live_task_ids.contains(task_id));
}

/// Register a task's inputs from `record`, resolving the package directory from
/// the workspace `packages`. No-op for the root package or when the package is
/// not found (nothing to watch in those cases).
pub(crate) fn register_task_watch_state_from_packages(
    registry: &TaskWatchRegistry,
    task_id: &TaskId,
    packages: &[PackageNode],
    record: &TaskRunRecord,
) -> Result<()> {
    let Some(package_dir) = package_dir_for(packages, &task_id.package) else {
        return Ok(());
    };
    register_task_watch_state(
        registry,
        task_id,
        task_id.package.clone(),
        package_dir,
        record,
    )
}

fn package_dir_for(packages: &[PackageNode], package: &PackageName) -> Option<PathBuf> {
    packages
        .iter()
        .find(|node| &node.name == package)
        .map(|node| node.path.clone())
}

/// Record (or refresh) a task's watch state from its cache record: compile its
/// input and output glob sets and snapshot each resolved input's fingerprint
/// (absolute path → mtime/size/hash). Called whenever a task is dispatched
/// (Run/Skip/SharedHit) so the watcher always has up-to-date input knowledge.
pub(crate) fn register_task_watch_state(
    registry: &TaskWatchRegistry,
    task_id: &TaskId,
    package: PackageName,
    package_dir: PathBuf,
    record: &TaskRunRecord,
) -> Result<()> {
    let input_globset = build_globset(&record.input_patterns)?;
    let output_globset = build_globset(&record.output_patterns)?;
    let inputs = record
        .inputs
        .iter()
        .filter(|entry| !entry.absent)
        .map(|entry| {
            (
                package_dir.join(&entry.path),
                InputFingerprint {
                    mtime_ns: entry.mtime_ns,
                    size: entry.size,
                    hash: entry.hash,
                },
            )
        })
        .collect();

    registry
        .lock()
        .expect("task watch registry mutex poisoned")
        .insert(
            task_id.clone(),
            TaskWatchState {
                package,
                package_dir,
                input_globset,
                output_globset,
                inputs,
            },
        );
    Ok(())
}

/// Given a batch of changed ABSOLUTE paths reported by the filesystem watcher,
/// return the set of packages whose tasks should rebuild.
///
/// A path triggers a task's package only when it is a real change to one of that
/// task's inputs (verified by size+mtime, then content hash), or when it is a
/// NEW file matching one of the task's input glob patterns. Anything else — cache
/// outputs, restore staging dirs, unrelated files — is ignored. Fingerprints for
/// dirtied inputs are refreshed immediately so a subsequent edit re-dirties.
pub(crate) fn dirty_packages_for_changes(
    registry: &TaskWatchRegistry,
    changed: &HashSet<PathBuf>,
) -> HashSet<PackageName> {
    // Do all filesystem I/O (stat + hash) up front, WITHOUT holding the registry
    // lock, so concurrent task registration is never blocked on disk latency.
    let probes: HashMap<&PathBuf, PathProbe> = changed
        .iter()
        .map(|path| (path, probe_path(path)))
        .collect();

    let mut dirty_packages = HashSet::new();
    let mut states = registry.lock().expect("task watch registry mutex poisoned");

    for (path, probe) in &probes {
        for state in states.values_mut() {
            if task_dirtied_by_path(state, path, probe) {
                dirty_packages.insert(state.package.clone());
            }
        }
    }

    dirty_packages
}

/// Result of stat-ing (and, if needed, hashing) a changed path once, off-lock.
enum PathProbe {
    /// The file exists with this fingerprint.
    Present(InputFingerprint),
    /// The file no longer exists (a genuine deletion).
    Deleted,
    /// The file could not be read for a transient reason (e.g. permission, EAGAIN)
    /// — NOT a confirmed deletion, so we must not infer a change from absence.
    Unknown,
}

/// Stat and (lazily) hash a path once, outside the registry lock.
fn probe_path(path: &Path) -> PathProbe {
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return PathProbe::Deleted,
        Err(_) => return PathProbe::Unknown,
    };
    if !metadata.is_file() {
        // A directory (or other non-file) event carries no input content.
        return PathProbe::Unknown;
    }
    let Some(mtime_ns) = modified_time_ns(&metadata) else {
        return PathProbe::Unknown;
    };
    match blake3_file(path) {
        Ok(hash) => PathProbe::Present(InputFingerprint {
            mtime_ns,
            size: metadata.len(),
            hash,
        }),
        Err(_) => PathProbe::Unknown,
    }
}

/// Decide whether `path` dirties this task, updating the stored fingerprint when
/// it does. Returns true only on a genuine input change or a new matching file.
fn task_dirtied_by_path(state: &mut TaskWatchState, path: &Path, probe: &PathProbe) -> bool {
    if let Some(prior) = state.inputs.get(path) {
        return match probe {
            PathProbe::Present(current) => {
                // The content hash is already computed in `probe_path`, so decide
                // purely on it. This catches same-size edits even when mtime is
                // coarse or preserved (editors, `cp -p`, low-resolution FS mtimes).
                let changed = current.hash != prior.hash;
                // Refresh the fingerprint either way so a touch-only event doesn't
                // re-trigger and a real change re-dirties on the next edit.
                state.inputs.insert(path.to_path_buf(), current.clone());
                changed
            }
            // Confirmed deletion of a known input → real change.
            PathProbe::Deleted => {
                state.inputs.remove(path);
                true
            }
            // Transient read failure: do not infer a change; leave state untouched.
            PathProbe::Unknown => false,
        };
    }

    // Not a known input: consider it only if it lives inside this task's package.
    let Ok(relative) = path.strip_prefix(&state.package_dir) else {
        return false;
    };
    let normalized = normalize_glob_path(relative);
    if normalized.is_empty() {
        return false;
    }

    // Never let a task's OWN outputs trigger a rebuild — this is what would
    // otherwise sustain the #161 loop when cache restore writes outputs into the
    // package directory. Applies to both the fallback and the new-input path.
    if state.output_globset.is_match(&normalized) {
        return false;
    }

    // Fallback for tasks that declare NO inputs (no input globs and no resolved
    // inputs yet): we cannot know what they depend on, so any non-output change
    // inside the package conservatively dirties them. Preserves responsiveness
    // and the "no lost changes" guarantee for undeclared-input tasks.
    if state.input_globset.is_empty() && state.inputs.is_empty() {
        return true;
    }

    // Otherwise treat it as a NEW file only if it matches an input glob.
    if state.input_globset.is_match(&normalized) {
        if let PathProbe::Present(current) = probe {
            state.inputs.insert(path.to_path_buf(), current.clone());
        }
        return true;
    }

    false
}

/// Nanoseconds since the Unix epoch for a file's mtime, matching the cache's
/// `FileEntry.mtime_ns` computation.
fn modified_time_ns(metadata: &fs::Metadata) -> Option<i128> {
    let duration = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(i128::from(duration.as_secs()) * 1_000_000_000 + i128::from(duration.subsec_nanos()))
}

fn normalize_glob_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use globset::{Glob, GlobSetBuilder};
    use luchta_cache::FileEntry;
    use std::fs;
    use std::io::Write;

    fn globset(patterns: &[&str]) -> GlobSet {
        let mut builder = GlobSetBuilder::new();
        for p in patterns {
            builder.add(Glob::new(p).expect("valid glob"));
        }
        builder.build().expect("build globset")
    }

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        let mut f = fs::File::create(path).expect("create file");
        f.write_all(contents).expect("write file");
        f.sync_all().ok();
    }

    fn fingerprint_of(path: &Path) -> InputFingerprint {
        match probe_path(path) {
            PathProbe::Present(fp) => fp,
            _ => panic!("expected present fingerprint for {}", path.display()),
        }
    }

    fn registry_with(state: TaskWatchState, task: &str) -> TaskWatchRegistry {
        let reg = empty_task_watch_registry();
        reg.lock().unwrap().insert(TaskId::new("pkg", task), state);
        reg
    }

    fn state_for(
        package_dir: &Path,
        inputs: HashMap<PathBuf, InputFingerprint>,
        globs: &[&str],
    ) -> TaskWatchState {
        state_with_outputs(package_dir, inputs, globs, &[])
    }

    fn state_with_outputs(
        package_dir: &Path,
        inputs: HashMap<PathBuf, InputFingerprint>,
        input_globs: &[&str],
        output_globs: &[&str],
    ) -> TaskWatchState {
        TaskWatchState {
            package: PackageName::from("pkg"),
            package_dir: package_dir.to_path_buf(),
            input_globset: globset(input_globs),
            output_globset: globset(output_globs),
            inputs,
        }
    }

    #[test]
    fn retain_task_watch_registry_task_ids_prunes_removed_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path();
        let reg = empty_task_watch_registry();
        let kept = TaskId::new("pkg", "build");
        let removed = TaskId::new("pkg", "test");
        reg.lock().unwrap().insert(
            kept.clone(),
            state_for(pkg, HashMap::new(), &["src/**/*.ts"]),
        );
        reg.lock().unwrap().insert(
            removed.clone(),
            state_for(pkg, HashMap::new(), &["tests/**/*.ts"]),
        );

        retain_task_watch_registry_task_ids(&reg, &HashSet::from([kept.clone()]));

        let states = reg.lock().unwrap();
        assert!(states.contains_key(&kept));
        assert!(!states.contains_key(&removed));
    }

    #[test]
    fn unchanged_input_is_not_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path();
        let input = pkg.join("src/lib.ts");
        write_file(&input, b"hello");
        let inputs = HashMap::from([(input.clone(), fingerprint_of(&input))]);
        let reg = registry_with(state_for(pkg, inputs, &["src/**/*.ts"]), "build");

        let dirty = dirty_packages_for_changes(&reg, &HashSet::from([input]));
        assert!(dirty.is_empty(), "unchanged input must not dirty");
    }

    #[test]
    fn changed_content_dirties_and_touch_only_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path();
        let input = pkg.join("src/lib.ts");
        write_file(&input, b"hello");
        let prior = fingerprint_of(&input);
        let inputs = HashMap::from([(input.clone(), prior.clone())]);
        let reg = registry_with(state_for(pkg, inputs, &["src/**/*.ts"]), "build");

        // Real content change -> dirty.
        write_file(&input, b"changed contents!");
        let dirty = dirty_packages_for_changes(&reg, &HashSet::from([input.clone()]));
        assert_eq!(dirty, HashSet::from([PackageName::from("pkg")]));

        // Touch-only: same size + same hash but bumped mtime -> NOT dirty.
        let same = b"same size string";
        write_file(&input, same);
        let cur = fingerprint_of(&input);
        // Force a differing mtime by rewriting the stored fingerprint to an older mtime
        // while keeping size+hash identical to the on-disk file.
        {
            let mut states = reg.lock().unwrap();
            let st = states.get_mut(&TaskId::new("pkg", "build")).unwrap();
            st.inputs.insert(
                input.clone(),
                InputFingerprint {
                    mtime_ns: cur.mtime_ns - 1,
                    size: cur.size,
                    hash: cur.hash,
                },
            );
        }
        let dirty = dirty_packages_for_changes(&reg, &HashSet::from([input]));
        assert!(dirty.is_empty(), "touch-only (same hash) must not dirty");
    }

    #[test]
    fn new_file_matching_input_glob_dirties() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path();
        // No known inputs yet, but a glob is registered.
        let reg = registry_with(state_for(pkg, HashMap::new(), &["src/**/*.ts"]), "build");
        let new_file = pkg.join("src/new.ts");
        write_file(&new_file, b"export const x = 1;");

        let dirty = dirty_packages_for_changes(&reg, &HashSet::from([new_file]));
        assert_eq!(dirty, HashSet::from([PackageName::from("pkg")]));
    }

    #[test]
    fn output_path_not_matching_input_glob_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path();
        let reg = registry_with(state_for(pkg, HashMap::new(), &["src/**/*.ts"]), "build");
        // A restored build output / staging path — not an input.
        let output = pkg.join("dist/index.js");
        write_file(&output, b"compiled");
        let staging = pkg.join("blob-restore-meta-abc/dist/index.js");
        write_file(&staging, b"restored");

        let dirty = dirty_packages_for_changes(&reg, &HashSet::from([output, staging]));
        assert!(
            dirty.is_empty(),
            "non-input paths must not dirty (loop-break)"
        );
    }

    #[test]
    fn registration_from_record_populates_inputs_and_globs() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path();
        let input = pkg.join("src/a.ts");
        write_file(&input, b"a");
        let fp = fingerprint_of(&input);

        let record = TaskRunRecord {
            schema_version: luchta_cache::SCHEMA_VERSION_V4,
            task_spec_hash: [0; 32],
            input_patterns: vec!["src/**/*.ts".to_string()],
            inputs: vec![FileEntry {
                path: "src/a.ts".to_string(),
                size: fp.size,
                mtime_ns: fp.mtime_ns,
                hash: fp.hash,
                absent: false,
            }],
            output_patterns: vec!["dist/**".to_string()],
            outputs: vec![],
            detected_input_patterns: true,
            detected_output_patterns: true,
            outputs_hash: [0; 32],
            env_hash: [0; 32],
            pkg_dep_hash: [0; 32],
            dep_outputs: std::collections::BTreeMap::new(),
            exit_status: 0,
            succeeded: true,
            start_unix_ms: 0,
            end_unix_ms: 1,
            reports: vec![],
            cache_nonce: None,
            run_reason: None,
        };

        let reg = empty_task_watch_registry();
        register_task_watch_state(
            &reg,
            &TaskId::new("pkg", "build"),
            PackageName::from("pkg"),
            pkg.to_path_buf(),
            &record,
        )
        .expect("register");

        // Registered input, unchanged -> not dirty.
        assert!(dirty_packages_for_changes(&reg, &HashSet::from([input.clone()])).is_empty());
        // New file under the registered glob -> dirty.
        let new_file = pkg.join("src/b.ts");
        write_file(&new_file, b"b");
        assert_eq!(
            dirty_packages_for_changes(&reg, &HashSet::from([new_file])),
            HashSet::from([PackageName::from("pkg")])
        );
    }

    #[test]
    fn second_distinct_change_re_dirties() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path();
        let input = pkg.join("src/lib.ts");
        write_file(&input, b"v1");
        let inputs = HashMap::from([(input.clone(), fingerprint_of(&input))]);
        let reg = registry_with(state_for(pkg, inputs, &["src/**/*.ts"]), "build");

        write_file(&input, b"v2 longer");
        assert_eq!(
            dirty_packages_for_changes(&reg, &HashSet::from([input.clone()])),
            HashSet::from([PackageName::from("pkg")])
        );
        // Fingerprint was refreshed; a second distinct edit dirties again.
        write_file(&input, b"v3 even longer!");
        assert_eq!(
            dirty_packages_for_changes(&reg, &HashSet::from([input])),
            HashSet::from([PackageName::from("pkg")])
        );
    }

    #[test]
    fn deleted_known_input_dirties() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path();
        let input = pkg.join("src/lib.ts");
        write_file(&input, b"hello");
        let inputs = HashMap::from([(input.clone(), fingerprint_of(&input))]);
        let reg = registry_with(state_for(pkg, inputs, &["src/**/*.ts"]), "build");

        fs::remove_file(&input).expect("delete input");
        let dirty = dirty_packages_for_changes(&reg, &HashSet::from([input.clone()]));
        assert_eq!(dirty, HashSet::from([PackageName::from("pkg")]));
        // The deleted path is dropped from the registry.
        let states = reg.lock().unwrap();
        assert!(!states
            .get(&TaskId::new("pkg", "build"))
            .unwrap()
            .inputs
            .contains_key(&input));
    }

    #[test]
    fn undeclared_input_task_dirties_on_any_non_output_change() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path();
        // No input globs, no known inputs, but output globs declared.
        let reg = registry_with(
            state_with_outputs(pkg, HashMap::new(), &[], &["dist/**"]),
            "build",
        );

        // A source-like change with no declared inputs -> conservatively dirty.
        let src = pkg.join("src/main.ts");
        write_file(&src, b"code");
        assert_eq!(
            dirty_packages_for_changes(&reg, &HashSet::from([src])),
            HashSet::from([PackageName::from("pkg")])
        );

        // But a change to this task's OWN output must NOT dirty it (loop-break for
        // undeclared-input tasks whose restore writes outputs into the package dir).
        let out = pkg.join("dist/index.js");
        write_file(&out, b"restored");
        assert!(
            dirty_packages_for_changes(&reg, &HashSet::from([out])).is_empty(),
            "output change must not dirty an undeclared-input task"
        );
    }

    #[test]
    fn output_matching_input_glob_is_not_dirtied() {
        // Task whose input glob would ALSO match its output (e.g. **/*.js), but the
        // output globs take precedence, preventing a restore-output loop.
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path();
        let reg = registry_with(
            state_with_outputs(pkg, HashMap::new(), &["**/*.js"], &["dist/**"]),
            "build",
        );
        let restored_output = pkg.join("dist/index.js");
        write_file(&restored_output, b"restored");

        assert!(
            dirty_packages_for_changes(&reg, &HashSet::from([restored_output])).is_empty(),
            "a path matching output globs must never dirty, even if it matches input globs"
        );
    }

    #[test]
    fn transient_unreadable_known_input_does_not_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path();
        let input = pkg.join("src/lib.ts");
        write_file(&input, b"hello");
        let inputs = HashMap::from([(input.clone(), fingerprint_of(&input))]);
        let reg = registry_with(state_for(pkg, inputs, &["src/**/*.ts"]), "build");

        // Directly exercise the probe/decision split: an Unknown probe (transient
        // read failure, NOT a deletion) must not be treated as a change.
        {
            let mut states = reg.lock().unwrap();
            let st = states.get_mut(&TaskId::new("pkg", "build")).unwrap();
            assert!(!task_dirtied_by_path(st, &input, &PathProbe::Unknown));
            // State is untouched: the input is still tracked.
            assert!(st.inputs.contains_key(&input));
        }
    }
}
