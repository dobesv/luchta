//! Gitignore-aware debounced file watcher for watch mode.
//!
//! Strategy: enumerate non-ignored directories with `ignore::WalkBuilder`, then place
//! one `notify` watch per directory using `RecursiveMode::NonRecursive`. This keeps
//! inotify usage frugal on Linux because ignored trees like `node_modules/`, `target/`,
//! `.git/`, and `.luchta/` are never watched in first place. Sync debouncer callback
//! forwards raw debounced events into tokio channel; intermediary task owned by
//! `WatcherHandle` filters changed paths and registers newly created source directories
//! with more non-recursive watches. Live propagation from a freshly added watch is not
//! E2E-tested here because sandbox filesystem timing is flaky; unit tests cover created
//! directory selection and de-dup logic instead.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;
use notify::event::CreateKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use notify_debouncer_full::{
    new_debouncer, DebounceEventResult, DebouncedEvent, Debouncer, FileIdMap,
};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const DEFAULT_CHANNEL_CAPACITY: usize = 32;
const DEFAULT_DEBOUNCE_MS: u64 = 150;
const IGNORED_DIR_NAMES: &[&str] = &["target", "node_modules", ".git", ".luchta"];

type SharedDebouncer = Arc<Mutex<Debouncer<RecommendedWatcher, FileIdMap>>>;

/// Keeps debouncer and bridge task alive. Dropping handle aborts bridge task and
/// releases this handle's debouncer reference; OS-watch teardown happens when last
/// debouncer reference is dropped, not necessarily synchronously with `drop`.
pub struct WatcherHandle {
    #[allow(dead_code)]
    debouncer: Option<SharedDebouncer>,
    bridge_task: JoinHandle<()>,
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.bridge_task.abort();
    }
}

#[cfg(test)]
impl WatcherHandle {
    /// Test-only no-op handle that does nothing when dropped.
    pub fn noop() -> Self {
        // Spawn a dummy task that never completes, will be aborted on drop.
        let handle = tokio::spawn(std::future::pending::<()>());
        Self {
            debouncer: None,
            bridge_task: handle,
        }
    }
}

#[derive(Debug, Error)]
pub enum WatcherError {
    #[error("failed to canonicalize workspace root '{path}': {source}")]
    CanonicalizeWorkspaceRoot {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to build ignore matcher for '{path}': {source}")]
    BuildIgnoreMatcher {
        path: PathBuf,
        source: ignore::Error,
    },
    #[error("failed to create watcher: {0}")]
    CreateDebouncer(#[source] notify::Error),
    #[error("failed to walk watch directories under '{path}': {source}")]
    WalkDirectories {
        path: PathBuf,
        source: ignore::Error,
    },
    #[error("failed to watch '{path}': {source}")]
    WatchPath {
        path: PathBuf,
        #[source]
        source: notify::Error,
    },
    #[error("watch state lock poisoned")]
    WatchStatePoisoned,
}

pub fn spawn_watcher(
    workspace_root: &Path,
    debounce_ms: u64,
) -> Result<(WatcherHandle, mpsc::Receiver<HashSet<PathBuf>>), WatcherError> {
    let workspace_root = workspace_root.canonicalize().map_err(|source| {
        WatcherError::CanonicalizeWorkspaceRoot {
            path: workspace_root.to_path_buf(),
            source,
        }
    })?;
    let ignore_filter = Arc::new(IgnoreFilter::new(&workspace_root)?);
    let initial_dirs = discover_watch_dirs(&workspace_root, &ignore_filter)?;
    let (raw_tx, raw_rx) = mpsc::channel(DEFAULT_CHANNEL_CAPACITY);
    let (batch_tx, batch_rx) = mpsc::channel(DEFAULT_CHANNEL_CAPACITY);

    let debouncer = new_debouncer(
        Duration::from_millis(resolve_debounce_ms(debounce_ms)),
        None,
        move |result: DebounceEventResult| {
            let Ok(events) = result else {
                return;
            };
            let _ = raw_tx.blocking_send(events);
        },
    )
    .map_err(WatcherError::CreateDebouncer)?;
    let debouncer = Arc::new(Mutex::new(debouncer));

    {
        let mut guard = debouncer
            .lock()
            .map_err(|_| WatcherError::WatchStatePoisoned)?;
        watch_directories(guard.watcher(), initial_dirs.iter().cloned())?;
    }

    let bridge_task = spawn_bridge_task(
        Arc::clone(&debouncer),
        Arc::clone(&ignore_filter),
        raw_rx,
        batch_tx,
    );

    Ok((
        WatcherHandle {
            debouncer: Some(debouncer),
            bridge_task,
        },
        batch_rx,
    ))
}

fn spawn_bridge_task(
    debouncer: SharedDebouncer,
    ignore_filter: Arc<IgnoreFilter>,
    mut raw_rx: mpsc::Receiver<Vec<DebouncedEvent>>,
    batch_tx: mpsc::Sender<HashSet<PathBuf>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut watched_dirs = HashSet::new();
        while let Some(events) = raw_rx.recv().await {
            let created_dirs = created_directories(&ignore_filter, events.iter());
            if let Ok(mut guard) = debouncer.lock() {
                let new_dirs = pending_watch_dirs(&ignore_filter, &mut watched_dirs, created_dirs);
                let _ = watch_directories(guard.watcher(), new_dirs.into_iter());
            }

            let changed_paths = collect_changed_paths(&ignore_filter, events);
            if changed_paths.is_empty() {
                continue;
            }

            if batch_tx.send(changed_paths).await.is_err() {
                break;
            }
        }
    })
}

fn resolve_debounce_ms(debounce_ms: u64) -> u64 {
    if debounce_ms == 0 {
        DEFAULT_DEBOUNCE_MS
    } else {
        debounce_ms
    }
}

fn watch_directories(
    watcher: &mut RecommendedWatcher,
    directories: impl Iterator<Item = PathBuf>,
) -> Result<(), WatcherError> {
    for path in directories {
        watcher
            .watch(&path, RecursiveMode::NonRecursive)
            .map_err(|source| WatcherError::WatchPath { path, source })?;
    }
    Ok(())
}

fn collect_changed_paths(
    ignore_filter: &IgnoreFilter,
    events: Vec<DebouncedEvent>,
) -> HashSet<PathBuf> {
    events
        .into_iter()
        .flat_map(|event| event.paths.clone().into_iter())
        .filter_map(normalize_absolute_path)
        .filter(|path| !ignore_filter.should_ignore(path))
        .collect()
}

fn normalize_absolute_path(path: PathBuf) -> Option<PathBuf> {
    if path.is_absolute() {
        return Some(path);
    }

    std::fs::canonicalize(&path).ok()
}

fn is_ignored_by_name(workspace_root: &Path, path: &Path) -> bool {
    path.strip_prefix(workspace_root)
        .unwrap_or(path)
        .components()
        .any(|component| {
            let name = component.as_os_str().to_string_lossy();
            IGNORED_DIR_NAMES.iter().any(|ignored| name == *ignored)
        })
}

struct IgnoreFilter {
    workspace_root: PathBuf,
    gitignore: Gitignore,
}

impl IgnoreFilter {
    fn new(workspace_root: &Path) -> Result<Self, WatcherError> {
        let mut builder = GitignoreBuilder::new(workspace_root);
        builder.add(workspace_root.join(".gitignore"));
        builder
            .add_line(None, ".git/")
            .map_err(|source| WatcherError::BuildIgnoreMatcher {
                path: workspace_root.to_path_buf(),
                source,
            })?;
        builder
            .add_line(None, "target/")
            .map_err(|source| WatcherError::BuildIgnoreMatcher {
                path: workspace_root.to_path_buf(),
                source,
            })?;
        builder.add_line(None, "node_modules/").map_err(|source| {
            WatcherError::BuildIgnoreMatcher {
                path: workspace_root.to_path_buf(),
                source,
            }
        })?;
        builder
            .add_line(None, ".luchta/")
            .map_err(|source| WatcherError::BuildIgnoreMatcher {
                path: workspace_root.to_path_buf(),
                source,
            })?;
        let gitignore = builder
            .build()
            .map_err(|source| WatcherError::BuildIgnoreMatcher {
                path: workspace_root.to_path_buf(),
                source,
            })?;
        Ok(Self {
            workspace_root: workspace_root.to_path_buf(),
            gitignore,
        })
    }

    fn should_ignore(&self, absolute_path: &Path) -> bool {
        if is_ignored_by_name(&self.workspace_root, absolute_path) {
            return true;
        }

        let is_dir = absolute_path.is_dir();
        self.gitignore.matched(absolute_path, is_dir).is_ignore()
    }

    fn should_watch_dir(&self, absolute_path: &Path) -> bool {
        absolute_path.starts_with(&self.workspace_root)
            && absolute_path.is_dir()
            && !self.should_ignore(absolute_path)
    }
}

fn discover_watch_dirs(
    workspace_root: &Path,
    ignore_filter: &IgnoreFilter,
) -> Result<HashSet<PathBuf>, WatcherError> {
    let mut dirs = HashSet::new();
    let mut walker = WalkBuilder::new(workspace_root);
    walker.hidden(false);
    walker.filter_entry({
        let root = workspace_root.to_path_buf();
        move |entry| {
            entry
                .path()
                .strip_prefix(&root)
                .ok()
                .map(|relative| !is_ignored_by_name(Path::new(""), relative))
                .unwrap_or(true)
        }
    });

    for entry in walker.build() {
        let entry = entry.map_err(|source| WatcherError::WalkDirectories {
            path: workspace_root.to_path_buf(),
            source,
        })?;
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let path = entry.into_path();
            if ignore_filter.should_watch_dir(&path) {
                dirs.insert(path);
            }
        }
    }
    Ok(dirs)
}

fn created_directories<'a>(
    ignore_filter: &IgnoreFilter,
    events: impl Iterator<Item = &'a DebouncedEvent>,
) -> HashSet<PathBuf> {
    events
        .filter(|event| {
            matches!(
                event.kind,
                EventKind::Create(CreateKind::Any | CreateKind::Folder)
            )
        })
        .flat_map(|event| event.paths.iter())
        .filter_map(|path| normalize_absolute_path(path.clone()))
        .filter(|path| ignore_filter.should_watch_dir(path))
        .collect()
}

fn pending_watch_dirs(
    ignore_filter: &IgnoreFilter,
    watched_dirs: &mut HashSet<PathBuf>,
    created_dirs: HashSet<PathBuf>,
) -> Vec<PathBuf> {
    created_dirs
        .into_iter()
        .filter(|path| ignore_filter.should_watch_dir(path) && watched_dirs.insert(path.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        created_directories, discover_watch_dirs, pending_watch_dirs, spawn_watcher,
        DebouncedEvent, IgnoreFilter, DEFAULT_DEBOUNCE_MS,
    };
    use notify::event::CreateKind;
    use notify::{Event, EventKind};
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    const RECEIVE_TIMEOUT: Duration = Duration::from_secs(3);
    const QUIET_TIMEOUT: Duration = Duration::from_millis(700);

    #[tokio::test]
    async fn spawn_watcher_emits_absolute_changed_file_path() {
        let temp = tempdir().expect("create tempdir");
        let root = temp.path();
        let (handle, mut rx) = spawn_watcher(root, DEFAULT_DEBOUNCE_MS).expect("spawn watcher");

        let file_path = root.join("src.txt");
        fs::write(&file_path, "hello").expect("write file");

        let batch = receive_batch_containing(&mut rx, &file_path).await;
        assert!(batch.contains(&canonical(&file_path)));

        drop(handle);
    }

    #[tokio::test]
    async fn spawn_watcher_filters_ignored_node_modules_paths() {
        let temp = tempdir().expect("create tempdir");
        let root = temp.path();
        let (handle, mut rx) = spawn_watcher(root, DEFAULT_DEBOUNCE_MS).expect("spawn watcher");

        let ignored_dir = root.join("node_modules/pkg");
        fs::create_dir_all(&ignored_dir).expect("create ignored dir");
        let ignored_file = ignored_dir.join("index.js");
        fs::write(&ignored_file, "module.exports = 1;\n").expect("write ignored file");

        assert_no_batch_containing(&mut rx, &ignored_file).await;

        drop(handle);
    }

    #[test]
    fn discover_watch_dirs_skips_ignored_trees() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        fs::create_dir_all(root.join("src/nested")).expect("create src dir");
        fs::create_dir_all(root.join("target/debug")).expect("create target dir");
        fs::create_dir_all(root.join("node_modules/pkg")).expect("create node_modules dir");
        let ignore_filter = IgnoreFilter::new(&root).expect("build ignore filter");

        let dirs = discover_watch_dirs(&root, &ignore_filter).expect("discover dirs");
        assert!(dirs.contains(&root));
        assert!(dirs.contains(&root.join("src")));
        assert!(dirs.contains(&root.join("src/nested")));
        assert!(!dirs.contains(&root.join("target")));
        assert!(!dirs.contains(&root.join("target/debug")));
        assert!(!dirs.contains(&root.join("node_modules")));
        assert!(!dirs.contains(&root.join("node_modules/pkg")));
    }

    #[test]
    fn created_directories_skips_ignored_dirs() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let ignored_dir = root.join("target/generated");
        let watched_dir = root.join("src/generated");
        fs::create_dir_all(&ignored_dir).expect("create ignored dir");
        fs::create_dir_all(&watched_dir).expect("create watched dir");
        let ignore_filter = IgnoreFilter::new(&root).expect("build ignore filter");

        let events = [
            debounced_create_dir_event(&ignored_dir),
            debounced_create_dir_event(&watched_dir),
        ];

        let created = created_directories(&ignore_filter, events.iter());
        let expected: HashSet<_> = [watched_dir].into_iter().collect();
        assert_eq!(created, expected);
    }

    #[test]
    fn workspace_ancestor_named_like_ignored_dir_does_not_hide_workspace_paths() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let workspace_root = root.join("target/workspace");
        let source_dir = workspace_root.join("src");
        let source_file = source_dir.join("main.ts");
        fs::create_dir_all(&source_dir).expect("create source dir");
        fs::write(&source_file, "export const value = 1;\n").expect("write source file");
        let ignore_filter = IgnoreFilter::new(&workspace_root).expect("build ignore filter");

        assert!(!ignore_filter.should_ignore(&source_file));
        assert!(ignore_filter.should_watch_dir(&workspace_root));
        assert!(ignore_filter.should_watch_dir(&source_dir));
    }

    #[test]
    fn pending_watch_dirs_filters_duplicates_and_ignored_dirs() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let already_watched = root.join("src/already");
        let new_watched = root.join("src/new");
        let ignored_dir = root.join("target/generated");
        fs::create_dir_all(&already_watched).expect("create already watched dir");
        fs::create_dir_all(&new_watched).expect("create new watched dir");
        fs::create_dir_all(&ignored_dir).expect("create ignored dir");
        let ignore_filter = IgnoreFilter::new(&root).expect("build ignore filter");
        let created = [already_watched.clone(), new_watched.clone(), ignored_dir]
            .into_iter()
            .collect();
        let mut watched_dirs = HashSet::from([already_watched.clone()]);

        let pending = pending_watch_dirs(&ignore_filter, &mut watched_dirs, created);
        let expected: HashSet<_> = [new_watched.clone()].into_iter().collect();
        assert_eq!(pending.into_iter().collect::<HashSet<_>>(), expected);
        assert!(watched_dirs.contains(&already_watched));
        assert!(watched_dirs.contains(&new_watched));
        assert_eq!(watched_dirs.len(), 2);
        assert!(ignore_filter.should_ignore(&root.join("target/generated")));
    }

    async fn receive_batch_containing(
        rx: &mut mpsc::Receiver<HashSet<PathBuf>>,
        expected_path: &Path,
    ) -> HashSet<PathBuf> {
        let expected_path = canonical(expected_path);
        timeout(RECEIVE_TIMEOUT, async {
            loop {
                let batch = rx.recv().await.expect("watcher channel open");
                if batch.contains(&expected_path) {
                    return batch;
                }
            }
        })
        .await
        .expect("timed out waiting for watcher event")
    }

    async fn assert_no_batch_containing(
        rx: &mut mpsc::Receiver<HashSet<PathBuf>>,
        ignored_path: &Path,
    ) {
        let ignored_path = canonical(ignored_path);
        let result = timeout(QUIET_TIMEOUT, async {
            while let Some(batch) = rx.recv().await {
                assert!(
                    !batch.contains(&ignored_path),
                    "received ignored path batch: {batch:?}"
                );
            }
        })
        .await;
        assert!(
            result.is_err(),
            "watcher produced unexpected channel closure"
        );
    }

    fn debounced_create_dir_event(path: &Path) -> DebouncedEvent {
        DebouncedEvent {
            event: Event {
                kind: EventKind::Create(CreateKind::Folder),
                paths: vec![path.to_path_buf()],
                attrs: Default::default(),
            },
            time: std::time::Instant::now(),
        }
    }

    fn canonical(path: &Path) -> PathBuf {
        path.canonicalize().expect("canonicalize path")
    }
}
