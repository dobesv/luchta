//! Gitignore-aware debounced file watcher for watch mode.
//!
//! On macOS, one recursive FSEvents root covers the workspace. Registering every nested
//! directory separately is both quadratic in `notify` 8 and unsafe once FSEvents' path
//! budget is exceeded. Other platforms enumerate non-ignored directories and place one
//! non-recursive watch per directory, which keeps inotify usage away from ignored trees
//! such as `node_modules/`, `target/`, `.git/`, and `.luchta/`.
//!
//! Only create/remove/modify events are subscribed (see [`watcher_config`]); file reads by
//! the build's own tools must not look like changes or flood the backend queue.
//!
//! The synchronous debouncer callback forwards events and errors into a bounded Tokio
//! channel. If that channel fills, an overflow latch wakes the bridge and requests a full
//! rescan instead of allowing unbounded memory growth. The bridge task owned by
//! `WatcherHandle` filters changed paths and (where necessary) registers newly created
//! source directories.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard};
use std::time::Duration;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use luchta_workspace::PackageNode;
use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{EventKind, EventKindMask, RecommendedWatcher, RecursiveMode};
#[cfg(target_os = "macos")]
use notify_debouncer_full::NoCache as WatcherCache;
#[cfg(not(target_os = "macos"))]
use notify_debouncer_full::RecommendedCache as WatcherCache;
use notify_debouncer_full::{
    new_debouncer_opt, DebounceEventHandler, DebounceEventResult, DebouncedEvent, Debouncer,
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const DEFAULT_CHANNEL_CAPACITY: usize = 32;
const DEFAULT_DEBOUNCE_MS: u64 = 150;
const WATCHER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const IGNORED_DIR_NAMES: &[&str] = &["target", "node_modules", ".git", ".luchta"];

type SharedDebouncer = Arc<Mutex<Debouncer<RecommendedWatcher, WatcherCache>>>;
type SharedWatchedDirs = Arc<Mutex<HashSet<PathBuf>>>;
type SharedIgnoreFilter = Arc<RwLock<IgnoreFilter>>;

enum RawWatchMessage {
    Events(Vec<DebouncedEvent>),
    Errors(Vec<notify::Error>),
}

enum BridgeInput {
    Message(RawWatchMessage),
    OverflowWake,
    Shutdown,
    Closed,
}

#[derive(Clone)]
struct RawEventForwarder {
    tx: mpsc::Sender<RawWatchMessage>,
    overflowed: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    wake: Arc<Notify>,
}

impl RawEventForwarder {
    fn forward(&self, message: RawWatchMessage) {
        match self.tx.try_send(message) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(RawWatchMessage::Errors(errors))) => {
                let error_batch = backend_error_batch(errors);
                if let Some(failure) = error_batch.failure {
                    if let Ok(mut pending_failure) = self.failure.lock() {
                        pending_failure.get_or_insert(failure);
                    }
                } else {
                    // The detailed recoverable error could not be queued, but the overflow
                    // recovery still guarantees a visible warning and full rescan.
                    self.overflowed.store(true, Ordering::Release);
                }
                self.wake.notify_one();
            }
            Err(mpsc::error::TrySendError::Full(RawWatchMessage::Events(_))) => {
                self.overflowed.store(true, Ordering::Release);
                self.wake.notify_one();
            }
        }
    }
}

struct BridgeTaskParams {
    debouncer: SharedDebouncer,
    ignore_filter: SharedIgnoreFilter,
    watched_dirs: SharedWatchedDirs,
    raw_rx: mpsc::Receiver<RawWatchMessage>,
    raw_overflowed: Arc<AtomicBool>,
    raw_failure: Arc<Mutex<Option<String>>>,
    raw_overflow_wake: Arc<Notify>,
    batch_tx: mpsc::Sender<WatchBatch>,
    shutdown: CancellationToken,
    backend_jobs: BackendJobs,
}

#[derive(Clone)]
struct BridgeEventProcessor {
    debouncer: SharedDebouncer,
    ignore_filter: SharedIgnoreFilter,
    watched_dirs: SharedWatchedDirs,
    backend_jobs: BackendJobs,
}

struct WatcherInitialization {
    debouncer: SharedDebouncer,
    watched_dirs: SharedWatchedDirs,
    ignore_filter: SharedIgnoreFilter,
    raw_rx: mpsc::Receiver<RawWatchMessage>,
    raw_overflowed: Arc<AtomicBool>,
    raw_failure: Arc<Mutex<Option<String>>>,
    raw_overflow_wake: Arc<Notify>,
    batch_tx: mpsc::Sender<WatchBatch>,
    batch_rx: mpsc::Receiver<WatchBatch>,
}

/// Keeps debouncer and bridge task alive. Dropping handle aborts bridge task and
/// releases this handle's debouncer reference; OS-watch teardown happens when last
/// debouncer reference is dropped, not necessarily synchronously with `drop`.
pub struct WatcherHandle {
    debouncer: Option<SharedDebouncer>,
    watched_dirs: SharedWatchedDirs,
    ignore_filter: SharedIgnoreFilter,
    bridge_shutdown: CancellationToken,
    bridge_task: Option<JoinHandle<()>>,
    backend_jobs: BackendJobs,
}

#[derive(Clone, Default)]
struct BackendJobs {
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl BackendJobs {
    fn spawn<T, F>(&self, operation: F) -> Result<oneshot::Receiver<T>, WatcherError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| WatcherError::WatchStatePoisoned)?;
        tasks.retain(|task| !task.is_finished());
        let (result_tx, result_rx) = oneshot::channel();
        tasks.push(tokio::task::spawn_blocking(move || {
            let _ = result_tx.send(operation());
        }));
        Ok(result_rx)
    }

    async fn shutdown(&self) -> bool {
        let mut tasks = match self.take() {
            Some(tasks) => tasks,
            None => return false,
        };
        let deadline = tokio::time::Instant::now() + WATCHER_SHUTDOWN_TIMEOUT;
        while let Some(mut task) = tasks.pop() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || timeout(remaining, &mut task).await.is_err() {
                tasks.push(task);
                supervise_cleanup(tasks);
                return false;
            }
        }
        true
    }

    fn handoff(&self) {
        if let Some(tasks) = self.take() {
            supervise_cleanup(tasks);
        }
    }

    async fn join_all(&self) {
        let Some(tasks) = self.take() else {
            return;
        };
        for task in tasks {
            let _ = task.await;
        }
    }

    fn take(&self) -> Option<Vec<JoinHandle<()>>> {
        self.tasks
            .lock()
            .ok()
            .map(|mut tasks| std::mem::take(&mut *tasks))
    }
}

fn supervise_cleanup(tasks: Vec<JoinHandle<()>>) {
    if tasks.is_empty() {
        return;
    }
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            let _cleanup = runtime.spawn(async move {
                for task in tasks {
                    let _ = task.await;
                }
            });
        }
        Err(_) => {
            for task in tasks {
                task.abort();
            }
        }
    }
}

fn supervise_bridge_cleanup(bridge_task: JoinHandle<()>, backend_jobs: BackendJobs) {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            let _cleanup = runtime.spawn(async move {
                let _ = bridge_task.await;
                backend_jobs.join_all().await;
            });
        }
        Err(_) => {
            bridge_task.abort();
            backend_jobs.handoff();
        }
    }
}

async fn run_backend_job<T, F>(jobs: &BackendJobs, operation: F) -> Result<T, WatcherError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    jobs.spawn(operation)?
        .await
        .map_err(|_| WatcherError::BackendJobStopped)
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.bridge_shutdown.cancel();
        if let Some(bridge_task) = self.bridge_task.take() {
            bridge_task.abort();
            supervise_bridge_cleanup(bridge_task, self.backend_jobs.clone());
        }
    }
}

#[cfg(test)]
impl WatcherHandle {
    /// Test-only no-op handle that does nothing when dropped.
    pub fn noop() -> Self {
        let bridge_shutdown = CancellationToken::new();
        let shutdown = bridge_shutdown.clone();
        let handle = tokio::spawn(async move { shutdown.cancelled().await });
        Self {
            debouncer: None,
            watched_dirs: Arc::new(Mutex::new(HashSet::new())),
            ignore_filter: Arc::new(RwLock::new(
                IgnoreFilter::root_only(&std::env::temp_dir()).expect("build noop ignore filter"),
            )),
            bridge_shutdown,
            bridge_task: Some(handle),
            backend_jobs: BackendJobs::default(),
        }
    }
}

impl WatcherHandle {
    pub(crate) async fn run_tracked_blocking<T, F>(&self, operation: F) -> Result<T, WatcherError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        run_backend_job(&self.backend_jobs, operation).await
    }

    pub async fn reconcile_watch_roots(
        &self,
        workspace_root: &Path,
        _packages: &[PackageNode],
    ) -> Result<(), WatcherError> {
        let workspace_root = workspace_root.to_path_buf();
        let (next_filter, desired_dirs) = run_backend_job(&self.backend_jobs, move || {
            let workspace_root = canonicalize_workspace_root(&workspace_root)?;
            prepare_watch_state(&workspace_root)
        })
        .await??;
        let debouncer = self.debouncer.clone();
        let watched_dirs = Arc::clone(&self.watched_dirs);
        let ignore_filter = Arc::clone(&self.ignore_filter);
        let result = run_backend_job(&self.backend_jobs, move || {
            let reconcile =
                reconcile_shared_watched_dirs(debouncer.as_ref(), &watched_dirs, desired_dirs);
            *ignore_filter
                .write()
                .map_err(|_| WatcherError::WatchStatePoisoned)? = next_filter;
            reconcile
        })
        .await?;
        for warning in result? {
            eprintln!("[watch] warning: {warning}");
        }
        Ok(())
    }

    pub async fn shutdown(mut self) {
        self.bridge_shutdown.cancel();
        let mut bridge_task = self
            .bridge_task
            .take()
            .expect("watcher bridge task should be present until shutdown");
        let bridge_supervised = if timeout(WATCHER_SHUTDOWN_TIMEOUT, &mut bridge_task)
            .await
            .is_err()
        {
            bridge_task.abort();
            supervise_bridge_cleanup(bridge_task, self.backend_jobs.clone());
            eprintln!("[watch] warning: filesystem event bridge did not stop promptly");
            true
        } else {
            false
        };
        if !bridge_supervised && !self.backend_jobs.shutdown().await {
            eprintln!(
                "[watch] warning: filesystem watcher backend work did not stop within {} seconds",
                WATCHER_SHUTDOWN_TIMEOUT.as_secs()
            );
        }
        if let Some(debouncer) = self.debouncer.take() {
            let mut teardown = tokio::task::spawn_blocking(move || drop(debouncer));
            match timeout(WATCHER_SHUTDOWN_TIMEOUT, &mut teardown).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    eprintln!("[watch] warning: filesystem watcher teardown failed: {error}");
                }
                Err(_) => {
                    supervise_cleanup(vec![teardown]);
                    eprintln!(
                        "[watch] warning: filesystem watcher teardown exceeded {} seconds",
                        WATCHER_SHUTDOWN_TIMEOUT.as_secs()
                    );
                }
            }
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
        source: std::io::Error,
    },
    #[error("failed to watch '{path}': {source}")]
    WatchPath {
        path: PathBuf,
        #[source]
        source: notify::Error,
    },
    #[error("watch state lock poisoned")]
    WatchStatePoisoned,
    #[error("watch backend task failed: {0}")]
    BackendTask(#[source] tokio::task::JoinError),
    #[error("watch backend job stopped without returning a result")]
    BackendJobStopped,
    #[error("one or more watch operations failed: {details}")]
    WatchOperations { details: String },
}

impl WatcherError {
    pub(crate) fn is_terminal_recovery_error(&self) -> bool {
        matches!(
            self,
            Self::WatchStatePoisoned | Self::BackendTask(_) | Self::BackendJobStopped
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchBatch {
    pub changed_paths: HashSet<PathBuf>,
    pub structural: bool,
    /// The backend reported that events were lost and the filesystem state must be rescanned.
    pub rescan: bool,
    /// Visible diagnostics emitted by the asynchronous watcher path.
    pub warnings: Vec<String>,
    /// A terminal backend failure after which future events cannot be trusted.
    pub failure: Option<String>,
}

impl WatchBatch {
    fn is_empty(&self) -> bool {
        if !self.changed_paths.is_empty() {
            return false;
        }
        if self.structural {
            return false;
        }
        if self.rescan {
            return false;
        }
        self.warnings.is_empty() && self.failure.is_none()
    }
}

pub async fn spawn_watcher(
    workspace_root: &Path,
    debounce_ms: u64,
) -> Result<(WatcherHandle, mpsc::Receiver<WatchBatch>), WatcherError> {
    let workspace_root = workspace_root.to_path_buf();
    let initialized =
        tokio::task::spawn_blocking(move || initialize_watcher(&workspace_root, debounce_ms))
            .await
            .map_err(WatcherError::BackendTask)??;
    let WatcherInitialization {
        debouncer,
        watched_dirs,
        ignore_filter,
        raw_rx,
        raw_overflowed,
        raw_failure,
        raw_overflow_wake,
        batch_tx,
        batch_rx,
    } = initialized;
    let bridge_shutdown = CancellationToken::new();
    let backend_jobs = BackendJobs::default();
    let bridge_task = spawn_bridge_task(BridgeTaskParams {
        debouncer: Arc::clone(&debouncer),
        ignore_filter: Arc::clone(&ignore_filter),
        watched_dirs: Arc::clone(&watched_dirs),
        raw_rx,
        raw_overflowed,
        raw_failure,
        raw_overflow_wake,
        batch_tx,
        shutdown: bridge_shutdown.clone(),
        backend_jobs: backend_jobs.clone(),
    });

    Ok((
        WatcherHandle {
            debouncer: Some(debouncer),
            watched_dirs,
            ignore_filter,
            bridge_shutdown,
            bridge_task: Some(bridge_task),
            backend_jobs,
        },
        batch_rx,
    ))
}

fn initialize_watcher(
    workspace_root: &Path,
    debounce_ms: u64,
) -> Result<WatcherInitialization, WatcherError> {
    let workspace_root = canonicalize_workspace_root(workspace_root)?;
    let (ignore_filter, initial_dirs) = prepare_watch_state(&workspace_root)?;
    let ignore_filter = Arc::new(RwLock::new(ignore_filter));
    let (raw_tx, raw_rx) = mpsc::channel(DEFAULT_CHANNEL_CAPACITY);
    let raw_overflowed = Arc::new(AtomicBool::new(false));
    let raw_failure = Arc::new(Mutex::new(None));
    let raw_overflow_wake = Arc::new(Notify::new());
    let (batch_tx, batch_rx) = mpsc::channel(DEFAULT_CHANNEL_CAPACITY);

    let forwarder = RawEventForwarder {
        tx: raw_tx,
        overflowed: Arc::clone(&raw_overflowed),
        failure: Arc::clone(&raw_failure),
        wake: Arc::clone(&raw_overflow_wake),
    };
    let debouncer = create_debouncer(
        Duration::from_millis(resolve_debounce_ms(debounce_ms)),
        move |result: DebounceEventResult| {
            let message = match result {
                Ok(events) => RawWatchMessage::Events(events),
                Err(errors) => RawWatchMessage::Errors(errors),
            };
            forwarder.forward(message);
        },
    )
    .map_err(WatcherError::CreateDebouncer)?;
    let debouncer = Arc::new(Mutex::new(debouncer));
    let watched_dirs = Arc::new(Mutex::new(HashSet::new()));

    {
        let mut guard = debouncer
            .lock()
            .map_err(|_| WatcherError::WatchStatePoisoned)?;
        let mut watched_dirs = lock_watched_dirs(&watched_dirs)?;
        watch_directories_strict(&mut guard, &mut watched_dirs, initial_dirs.into_iter())?;
    }

    Ok(WatcherInitialization {
        debouncer,
        watched_dirs,
        ignore_filter,
        raw_rx,
        raw_overflowed,
        raw_failure,
        raw_overflow_wake,
        batch_tx,
        batch_rx,
    })
}

fn canonicalize_workspace_root(workspace_root: &Path) -> Result<PathBuf, WatcherError> {
    workspace_root
        .canonicalize()
        .map_err(|source| WatcherError::CanonicalizeWorkspaceRoot {
            path: workspace_root.to_path_buf(),
            source,
        })
}

fn spawn_bridge_task(params: BridgeTaskParams) -> JoinHandle<()> {
    let BridgeTaskParams {
        debouncer,
        ignore_filter,
        watched_dirs,
        mut raw_rx,
        raw_overflowed,
        raw_failure,
        raw_overflow_wake,
        batch_tx,
        shutdown,
        backend_jobs,
    } = params;
    tokio::spawn(async move {
        let processor = BridgeEventProcessor {
            debouncer,
            ignore_filter,
            watched_dirs,
            backend_jobs,
        };
        loop {
            let input = next_bridge_input(&mut raw_rx, &raw_overflow_wake, &shutdown).await;
            if matches!(input, BridgeInput::Closed | BridgeInput::Shutdown) {
                break;
            }
            let mut batch = tokio::select! {
                batch = process_bridge_input(
                    input,
                    &processor,
                ) => batch,
                () = shutdown.cancelled() => break,
            };
            append_latched_failure(&mut batch, &raw_failure);
            append_overflow_recovery(&mut batch, &raw_overflowed);
            if send_bridge_batch(&batch_tx, batch, &shutdown).await {
                break;
            }
        }
    })
}

fn append_latched_failure(batch: &mut WatchBatch, raw_failure: &Mutex<Option<String>>) {
    let failure = match raw_failure.lock() {
        Ok(mut failure) => failure.take(),
        Err(_) => Some("filesystem watcher failure state lock poisoned".to_string()),
    };
    let Some(failure) = failure else {
        return;
    };
    batch.failure = Some(match batch.failure.take() {
        Some(existing) => format!("{existing}; {failure}"),
        None => failure,
    });
}

async fn next_bridge_input(
    raw_rx: &mut mpsc::Receiver<RawWatchMessage>,
    raw_overflow_wake: &Notify,
    shutdown: &CancellationToken,
) -> BridgeInput {
    tokio::select! {
        message = raw_rx.recv() => message.map_or(BridgeInput::Closed, BridgeInput::Message),
        () = raw_overflow_wake.notified() => BridgeInput::OverflowWake,
        () = shutdown.cancelled() => BridgeInput::Shutdown,
    }
}

async fn process_bridge_input(input: BridgeInput, processor: &BridgeEventProcessor) -> WatchBatch {
    match input {
        BridgeInput::Message(RawWatchMessage::Events(events)) => {
            let debouncer = Arc::clone(&processor.debouncer);
            let ignore_filter = Arc::clone(&processor.ignore_filter);
            let watched_dirs = Arc::clone(&processor.watched_dirs);
            match run_backend_job(&processor.backend_jobs, move || {
                bridge_event_batch(&debouncer, &ignore_filter, &watched_dirs, events)
            })
            .await
            {
                Ok(batch) => batch,
                Err(error) => WatchBatch {
                    failure: Some(format!("watch event processing job failed: {error}")),
                    ..WatchBatch::default()
                },
            }
        }
        BridgeInput::Message(RawWatchMessage::Errors(errors)) => backend_error_batch(errors),
        BridgeInput::OverflowWake | BridgeInput::Shutdown | BridgeInput::Closed => {
            WatchBatch::default()
        }
    }
}

fn append_overflow_recovery(batch: &mut WatchBatch, raw_overflowed: &AtomicBool) {
    if !raw_overflowed.swap(false, Ordering::AcqRel) {
        return;
    }
    batch.structural = true;
    batch.rescan = true;
    batch
        .warnings
        .push("filesystem event queue overflowed; some path events were coalesced".to_string());
}

async fn send_bridge_batch(
    batch_tx: &mpsc::Sender<WatchBatch>,
    batch: WatchBatch,
    shutdown: &CancellationToken,
) -> bool {
    if batch.is_empty() {
        return false;
    }
    let terminal = batch.failure.is_some();
    tokio::select! {
        result = batch_tx.send(batch) => result.is_err() || terminal,
        () = shutdown.cancelled() => true,
    }
}

fn backend_error_batch(errors: Vec<notify::Error>) -> WatchBatch {
    if errors.iter().any(is_terminal_backend_error) {
        return WatchBatch {
            failure: Some(format_backend_errors(errors)),
            ..WatchBatch::default()
        };
    }

    WatchBatch {
        structural: true,
        rescan: true,
        warnings: vec![format!(
            "filesystem watcher backend reported a recoverable error; rescanning: {}",
            format_error_details(errors)
        )],
        ..WatchBatch::default()
    }
}

fn is_terminal_backend_error(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::Generic(_)
        | notify::ErrorKind::MaxFilesWatch
        | notify::ErrorKind::InvalidConfig(_) => true,
        notify::ErrorKind::Io(_)
        | notify::ErrorKind::PathNotFound
        | notify::ErrorKind::WatchNotFound => false,
    }
}

fn format_backend_errors(errors: Vec<notify::Error>) -> String {
    format!(
        "filesystem watcher backend failed: {}",
        format_error_details(errors)
    )
}

fn format_error_details(errors: Vec<notify::Error>) -> String {
    errors
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Backend configuration shared by every watcher this module creates.
///
/// Only content and namespace changes (create/remove/modify) are subscribed. `notify` 9
/// defaults to every event kind, including opens and closes; the build's own tools read
/// config files in watched directories so often that those access events overflow the
/// inotify queue, and the resulting rescan re-runs the tools, which read the files again.
fn watcher_config() -> notify::Config {
    notify::Config::default().with_event_kinds(EventKindMask::CORE)
}

fn create_debouncer(
    timeout: Duration,
    event_handler: impl DebounceEventHandler,
) -> notify::Result<Debouncer<RecommendedWatcher, WatcherCache>> {
    new_debouncer_opt::<_, RecommendedWatcher, WatcherCache>(
        timeout,
        None,
        event_handler,
        WatcherCache::new(),
        watcher_config(),
    )
}

fn bridge_event_batch(
    debouncer: &SharedDebouncer,
    ignore_filter: &SharedIgnoreFilter,
    watched_dirs: &SharedWatchedDirs,
    events: Vec<DebouncedEvent>,
) -> WatchBatch {
    let (created_dirs, mut batch) = match read_ignore_filter(ignore_filter) {
        Ok(ignore_filter) => {
            let created_dirs = created_directories(&ignore_filter, events.iter());
            let batch = collect_watch_batch(&ignore_filter, events);
            (created_dirs, batch)
        }
        Err(error) => {
            return WatchBatch {
                structural: true,
                rescan: true,
                warnings: vec![error.to_string()],
                ..WatchBatch::default()
            };
        }
    };

    batch.warnings.extend(register_created_directories(
        debouncer,
        watched_dirs,
        created_dirs,
    ));
    batch
}

fn resolve_debounce_ms(debounce_ms: u64) -> u64 {
    if debounce_ms == 0 {
        DEFAULT_DEBOUNCE_MS
    } else {
        debounce_ms
    }
}

fn watch_directories_strict(
    watcher: &mut Debouncer<RecommendedWatcher, WatcherCache>,
    watched_dirs: &mut HashSet<PathBuf>,
    directories: impl Iterator<Item = PathBuf>,
) -> Result<(), WatcherError> {
    let mut directories = directories.collect::<Vec<_>>();
    directories.sort();
    for path in directories {
        watcher
            .watch(&path, platform_recursive_mode())
            .map_err(|source| WatcherError::WatchPath {
                path: path.clone(),
                source,
            })?;
        watched_dirs.insert(path);
    }
    Ok(())
}

fn collect_watch_batch(ignore_filter: &IgnoreFilter, events: Vec<DebouncedEvent>) -> WatchBatch {
    let mut batch = WatchBatch::default();
    for event in events {
        // Checked before any kind filter: a backend overflow must always trigger a rescan.
        batch.rescan |= event.need_rescan();
        if is_access_event(&event.kind) {
            continue;
        }
        for path in event
            .paths
            .iter()
            .cloned()
            .filter_map(normalize_absolute_path)
        {
            record_changed_path(&mut batch, ignore_filter, &event.kind, path);
        }
    }
    batch
}

/// Reads (open/close/access) never change file contents, so they are not changes.
fn is_access_event(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Access(_))
}

fn record_changed_path(
    batch: &mut WatchBatch,
    ignore_filter: &IgnoreFilter,
    kind: &EventKind,
    path: PathBuf,
) {
    if is_ignore_file(&path) && ignore_filter.should_process_ignore_file(&path) {
        // Ignore files control which paths are visible to the watcher. Always keep
        // their events, even when a rule happens to ignore the ignore file itself,
        // and rehash the full selection in case previously hidden files appear.
        batch.changed_paths.insert(path);
        batch.structural = true;
        batch.rescan = true;
        return;
    }
    if ignore_filter.should_ignore(&path) {
        return;
    }
    batch.structural |= is_structural_path(kind, &path);
    batch.changed_paths.insert(path);
}

fn is_structural_path(kind: &EventKind, path: &Path) -> bool {
    if is_structural_event_kind(kind) {
        return true;
    }
    path.file_name()
        .map(|name| name.to_string_lossy().starts_with("luchta-config."))
        .unwrap_or(false)
}

fn is_ignore_file(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == ".gitignore")
}

fn is_structural_event_kind(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(CreateKind::Any | CreateKind::Folder)
            | EventKind::Remove(RemoveKind::Any | RemoveKind::Folder)
            | EventKind::Modify(ModifyKind::Name(_))
    )
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

fn is_always_ignored(workspace_root: &Path, path: &Path) -> bool {
    if is_ignored_by_name(workspace_root, path) {
        return true;
    }
    path.strip_prefix(workspace_root)
        .unwrap_or(path)
        .components()
        .any(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .starts_with("blob-restore-")
        })
}

struct IgnoreFilter {
    workspace_root: PathBuf,
    gitignores: Vec<(PathBuf, Gitignore)>,
}

impl IgnoreFilter {
    #[cfg(any(target_os = "macos", test))]
    fn new(workspace_root: &Path) -> Result<Self, WatcherError> {
        Self::scan(workspace_root, false).map(|(filter, _)| filter)
    }

    fn scan(
        workspace_root: &Path,
        collect_watch_dirs: bool,
    ) -> Result<(Self, HashSet<PathBuf>), WatcherError> {
        let mut filter = Self {
            workspace_root: workspace_root.to_path_buf(),
            gitignores: vec![(
                workspace_root.to_path_buf(),
                build_root_ignore_matcher(workspace_root)?,
            )],
        };
        let mut watch_dirs = HashSet::new();
        let mut pending_dirs = vec![workspace_root.to_path_buf()];

        while let Some(directory) = pending_dirs.pop() {
            if filter.should_skip_directory(&directory) {
                continue;
            }
            if collect_watch_dirs {
                watch_dirs.insert(directory.clone());
            }
            if directory != workspace_root {
                filter.add_nested_gitignore(&directory)?;
            }

            pending_dirs.extend(child_directories(&directory)?);
        }
        filter
            .gitignores
            .sort_by_key(|(root, _)| root.components().count());

        Ok((filter, watch_dirs))
    }

    fn should_skip_directory(&self, directory: &Path) -> bool {
        if directory == self.workspace_root {
            return false;
        }
        is_always_ignored(&self.workspace_root, directory)
            || self.matches_ignore_rules(directory, true)
    }

    fn add_nested_gitignore(&mut self, directory: &Path) -> Result<(), WatcherError> {
        let path = directory.join(".gitignore");
        if !path.is_file() {
            return Ok(());
        }
        let mut builder = GitignoreBuilder::new(directory);
        if let Some(source) = builder.add(&path) {
            return Err(WatcherError::BuildIgnoreMatcher { path, source });
        }
        let matcher = builder
            .build()
            .map_err(|source| WatcherError::BuildIgnoreMatcher {
                path: directory.to_path_buf(),
                source,
            })?;
        self.gitignores.push((directory.to_path_buf(), matcher));
        Ok(())
    }

    #[cfg(test)]
    fn root_only(workspace_root: &Path) -> Result<Self, WatcherError> {
        let workspace_root = workspace_root.to_path_buf();
        let root_matcher = build_root_ignore_matcher(&workspace_root)?;

        Ok(Self {
            workspace_root: workspace_root.clone(),
            gitignores: vec![(workspace_root, root_matcher)],
        })
    }

    fn should_ignore(&self, absolute_path: &Path) -> bool {
        if is_always_ignored(&self.workspace_root, absolute_path) {
            return true;
        }

        self.matches_ignore_rules(absolute_path, absolute_path.is_dir())
    }

    fn should_process_ignore_file(&self, absolute_path: &Path) -> bool {
        let Some(parent) = absolute_path.parent() else {
            return false;
        };
        parent.starts_with(&self.workspace_root)
            && !is_always_ignored(&self.workspace_root, parent)
            && !self.matches_ignore_rules(parent, true)
    }

    fn matches_ignore_rules(&self, absolute_path: &Path, is_dir: bool) -> bool {
        let mut ignored = false;
        for (root, matcher) in &self.gitignores {
            if !absolute_path.starts_with(root) {
                continue;
            }
            let matched = matcher.matched_path_or_any_parents(absolute_path, is_dir);
            if !matched.is_none() {
                ignored = matched.is_ignore();
            }
        }
        ignored
    }

    fn should_watch_dir(&self, absolute_path: &Path) -> bool {
        absolute_path.starts_with(&self.workspace_root)
            && absolute_path.is_dir()
            && !self.should_ignore(absolute_path)
    }
}

fn child_directories(directory: &Path) -> Result<Vec<PathBuf>, WatcherError> {
    let Some(entries) = walk_value(std::fs::read_dir(directory), directory)? else {
        return Ok(Vec::new());
    };
    let mut children = Vec::new();
    for entry in entries {
        let Some(entry) = walk_value(entry, directory)? else {
            continue;
        };
        let path = entry.path();
        let Some(file_type) = walk_value(entry.file_type(), &path)? else {
            continue;
        };
        if file_type.is_dir() {
            children.push(path);
        }
    }
    Ok(children)
}

fn walk_value<T>(result: std::io::Result<T>, path: &Path) -> Result<Option<T>, WatcherError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(WatcherError::WalkDirectories {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(target_os = "macos")]
fn prepare_watch_state(
    workspace_root: &Path,
) -> Result<(IgnoreFilter, HashSet<PathBuf>), WatcherError> {
    Ok((
        IgnoreFilter::new(workspace_root)?,
        HashSet::from([workspace_root.to_path_buf()]),
    ))
}

#[cfg(not(target_os = "macos"))]
fn prepare_watch_state(
    workspace_root: &Path,
) -> Result<(IgnoreFilter, HashSet<PathBuf>), WatcherError> {
    IgnoreFilter::scan(workspace_root, true)
}

fn build_root_ignore_matcher(workspace_root: &Path) -> Result<Gitignore, WatcherError> {
    let mut builder = GitignoreBuilder::new(workspace_root);
    let root_ignore = workspace_root.join(".gitignore");
    if root_ignore.exists() {
        if let Some(source) = builder.add(&root_ignore) {
            return Err(WatcherError::BuildIgnoreMatcher {
                path: root_ignore,
                source,
            });
        }
    }
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
    builder
        .add_line(None, "node_modules/")
        .map_err(|source| WatcherError::BuildIgnoreMatcher {
            path: workspace_root.to_path_buf(),
            source,
        })?;
    builder
        .add_line(None, ".luchta/")
        .map_err(|source| WatcherError::BuildIgnoreMatcher {
            path: workspace_root.to_path_buf(),
            source,
        })?;
    // Shared-cache restore stages outputs in tempdirs created inside package
    // dirs via `tempfile::Builder::prefix("blob-restore-")` and
    // `prefix("blob-restore-meta-")` (see luchta-cache blob.rs). Match both.
    builder
        .add_line(None, "blob-restore-*/")
        .map_err(|source| WatcherError::BuildIgnoreMatcher {
            path: workspace_root.to_path_buf(),
            source,
        })?;
    builder
        .build()
        .map_err(|source| WatcherError::BuildIgnoreMatcher {
            path: workspace_root.to_path_buf(),
            source,
        })
}

fn lock_watched_dirs(
    watched_dirs: &SharedWatchedDirs,
) -> Result<MutexGuard<'_, HashSet<PathBuf>>, WatcherError> {
    watched_dirs
        .lock()
        .map_err(|_| WatcherError::WatchStatePoisoned)
}

fn read_ignore_filter(
    ignore_filter: &SharedIgnoreFilter,
) -> Result<RwLockReadGuard<'_, IgnoreFilter>, WatcherError> {
    ignore_filter
        .read()
        .map_err(|_| WatcherError::WatchStatePoisoned)
}

fn reconcile_shared_watched_dirs(
    debouncer: Option<&SharedDebouncer>,
    watched_dirs: &SharedWatchedDirs,
    desired_dirs: HashSet<PathBuf>,
) -> Result<Vec<String>, WatcherError> {
    let Some(debouncer) = debouncer else {
        *lock_watched_dirs(watched_dirs)? = desired_dirs;
        return Ok(Vec::new());
    };

    let mut watcher = debouncer
        .lock()
        .map_err(|_| WatcherError::WatchStatePoisoned)?;
    let mut watched_dirs = lock_watched_dirs(watched_dirs)?;
    reconcile_watched_dirs(&mut watcher, &mut watched_dirs, desired_dirs)
}

fn reconcile_watched_dirs(
    watcher: &mut Debouncer<RecommendedWatcher, WatcherCache>,
    watched_dirs: &mut HashSet<PathBuf>,
    desired_dirs: HashSet<PathBuf>,
) -> Result<Vec<String>, WatcherError> {
    let mut dirs_to_watch: Vec<_> = desired_dirs.difference(watched_dirs).cloned().collect();
    let mut dirs_to_unwatch: Vec<_> = watched_dirs.difference(&desired_dirs).cloned().collect();
    dirs_to_watch.sort();
    dirs_to_unwatch.sort();
    let mut watch_failures = Vec::new();
    let mut warnings = Vec::new();

    for path in dirs_to_watch {
        match watcher.watch(&path, platform_recursive_mode()) {
            Ok(()) => {
                watched_dirs.insert(path);
            }
            Err(source) => {
                watch_failures.push(format!("failed to watch '{}': {source}", path.display()))
            }
        }
    }

    if !watch_failures.is_empty() {
        return Err(WatcherError::WatchOperations {
            details: watch_failures.join("; "),
        });
    }

    for path in dirs_to_unwatch {
        match watcher.unwatch(&path) {
            Ok(()) => {
                watched_dirs.remove(&path);
            }
            Err(source) if matches!(&source.kind, notify::ErrorKind::WatchNotFound) => {
                watched_dirs.remove(&path);
            }
            Err(source) => {
                warnings.push(format!(
                    "failed to unwatch obsolete directory '{}'; continuing with the extra watch: {source}",
                    path.display()
                ));
            }
        }
    }

    Ok(warnings)
}

#[cfg(target_os = "macos")]
fn platform_recursive_mode() -> RecursiveMode {
    RecursiveMode::Recursive
}

#[cfg(not(target_os = "macos"))]
fn platform_recursive_mode() -> RecursiveMode {
    RecursiveMode::NonRecursive
}

#[cfg(test)]
fn discover_watch_dirs(
    workspace_root: &Path,
    _ignore_filter: &IgnoreFilter,
) -> Result<HashSet<PathBuf>, WatcherError> {
    IgnoreFilter::scan(workspace_root, true).map(|(_, dirs)| dirs)
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

#[cfg(any(not(target_os = "macos"), test))]
fn pending_watch_dirs(
    watched_dirs: &HashSet<PathBuf>,
    created_dirs: HashSet<PathBuf>,
) -> Vec<PathBuf> {
    created_dirs
        .into_iter()
        .filter(|path| !watched_dirs.contains(path))
        .collect()
}

#[cfg(target_os = "macos")]
fn register_created_directories(
    _debouncer: &SharedDebouncer,
    _watched_dirs: &SharedWatchedDirs,
    _created_dirs: HashSet<PathBuf>,
) -> Vec<String> {
    Vec::new()
}

#[cfg(not(target_os = "macos"))]
fn register_created_directories(
    debouncer: &SharedDebouncer,
    watched_dirs: &SharedWatchedDirs,
    created_dirs: HashSet<PathBuf>,
) -> Vec<String> {
    let mut watcher = match debouncer.lock() {
        Ok(watcher) => watcher,
        Err(_) => return vec![WatcherError::WatchStatePoisoned.to_string()],
    };
    let mut watched_dirs = match watched_dirs.lock() {
        Ok(watched_dirs) => watched_dirs,
        Err(_) => return vec![WatcherError::WatchStatePoisoned.to_string()],
    };
    let mut new_dirs = pending_watch_dirs(&watched_dirs, created_dirs);
    new_dirs.sort();

    let mut warnings = Vec::new();
    for path in new_dirs {
        match watcher.watch(&path, RecursiveMode::NonRecursive) {
            Ok(()) => {
                watched_dirs.insert(path);
            }
            Err(source) => warnings.push(format!(
                "failed to watch newly created directory '{}': {source}",
                path.display()
            )),
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::{
        backend_error_batch, collect_watch_batch, create_debouncer, created_directories,
        discover_watch_dirs, pending_watch_dirs, reconcile_watched_dirs, spawn_bridge_task,
        spawn_watcher, watcher_config, BridgeTaskParams, DebouncedEvent, IgnoreFilter,
        RawEventForwarder, RawWatchMessage, WatchBatch, WatcherCache, WatcherError,
        DEFAULT_DEBOUNCE_MS,
    };
    use notify::event::{AccessKind, AccessMode, CreateKind, Flag, ModifyKind};
    use notify::{Event, EventKind, EventKindMask, RecommendedWatcher};
    use notify_debouncer_full::Debouncer;
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::sync::{mpsc, Notify};
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    const RECEIVE_TIMEOUT: Duration = Duration::from_secs(3);
    const QUIET_TIMEOUT: Duration = Duration::from_millis(700);

    #[tokio::test]
    async fn raw_queue_overflow_emits_repeatable_rescans_without_lost_wakes() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let debouncer = Arc::new(Mutex::new(
            create_debouncer(Duration::from_millis(DEFAULT_DEBOUNCE_MS), |_| {})
                .expect("create debouncer"),
        ));
        let ignore_filter = Arc::new(RwLock::new(
            IgnoreFilter::new(&root).expect("build ignore filter"),
        ));
        let watched_dirs = Arc::new(Mutex::new(HashSet::new()));
        let (raw_tx, raw_rx) = mpsc::channel(1);
        let raw_overflowed = Arc::new(AtomicBool::new(false));
        let raw_failure = Arc::new(Mutex::new(None));
        let raw_overflow_wake = Arc::new(Notify::new());
        let (batch_tx, mut batch_rx) = mpsc::channel(2);
        let forwarder = RawEventForwarder {
            tx: raw_tx,
            overflowed: Arc::clone(&raw_overflowed),
            failure: Arc::clone(&raw_failure),
            wake: Arc::clone(&raw_overflow_wake),
        };

        enqueue_empty_raw_event(&forwarder);
        enqueue_empty_raw_event(&forwarder);
        enqueue_empty_raw_event(&forwarder);
        let bridge_task = spawn_bridge_task(BridgeTaskParams {
            debouncer,
            ignore_filter,
            watched_dirs,
            raw_rx,
            raw_overflowed: Arc::clone(&raw_overflowed),
            raw_failure: Arc::clone(&raw_failure),
            raw_overflow_wake: Arc::clone(&raw_overflow_wake),
            batch_tx,
            shutdown: CancellationToken::new(),
            backend_jobs: super::BackendJobs::default(),
        });

        assert_overflow_rescan(&mut batch_rx).await;

        // These calls cannot yield between sends, so the second event fills the
        // latch again after the bridge has already consumed the first overflow.
        enqueue_empty_raw_event(&forwarder);
        enqueue_empty_raw_event(&forwarder);
        assert_overflow_rescan(&mut batch_rx).await;

        enqueue_empty_raw_event(&forwarder);
        forwarder.forward(RawWatchMessage::Errors(vec![
            notify::Error::path_not_found().add_path(root.join("removed")),
        ]));
        assert_overflow_rescan(&mut batch_rx).await;

        enqueue_empty_raw_event(&forwarder);
        forwarder.forward(RawWatchMessage::Errors(vec![notify::Error::generic(
            "unable to start FSEvent stream",
        )]));
        let terminal = timeout(RECEIVE_TIMEOUT, batch_rx.recv())
            .await
            .expect("timed out waiting for terminal batch")
            .expect("bridge channel closed before terminal batch");
        assert_eq!(
            terminal.failure.as_deref(),
            Some("filesystem watcher backend failed: unable to start FSEvent stream")
        );

        bridge_task
            .await
            .expect("bridge exits after terminal error");
    }

    #[tokio::test]
    async fn backend_jobs_are_joined_after_their_waiter_is_dropped() {
        let jobs = super::BackendJobs::default();
        let finished = Arc::new(AtomicBool::new(false));
        let job_finished = Arc::clone(&finished);
        let result = jobs
            .spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                job_finished.store(true, Ordering::Release);
            })
            .expect("track backend job");
        drop(result);

        assert!(jobs.shutdown().await);
        assert!(finished.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn backend_job_handoff_keeps_cancelled_work_owned() {
        let jobs = super::BackendJobs::default();
        let finished = Arc::new(AtomicBool::new(false));
        let job_finished = Arc::clone(&finished);
        let result = jobs
            .spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                job_finished.store(true, Ordering::Release);
            })
            .expect("track backend job");
        drop(result);

        jobs.handoff();
        timeout(RECEIVE_TIMEOUT, async {
            while !finished.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup supervisor should retain backend job");
    }

    fn enqueue_empty_raw_event(forwarder: &RawEventForwarder) {
        forwarder.forward(RawWatchMessage::Events(Vec::new()));
    }

    async fn assert_overflow_rescan(batch_rx: &mut mpsc::Receiver<WatchBatch>) {
        let batch = timeout(RECEIVE_TIMEOUT, batch_rx.recv())
            .await
            .expect("timed out waiting for overflow batch")
            .expect("bridge channel closed");
        assert!(
            batch.rescan
                && batch.structural
                && batch.failure.is_none()
                && batch
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("queue overflowed")),
            "unexpected overflow recovery batch: {batch:?}"
        );
    }

    #[test]
    fn asynchronous_backend_errors_are_terminal() {
        let batch = backend_error_batch(vec![notify::Error::generic(
            "unable to start FSEvent stream",
        )]);

        assert_eq!(
            batch.failure.as_deref(),
            Some("filesystem watcher backend failed: unable to start FSEvent stream")
        );
        assert!(!batch.rescan);
        assert!(!batch.structural);
    }

    #[test]
    fn path_specific_backend_errors_warn_and_rescan() {
        let missing = PathBuf::from("/repo/removed");
        let batch = backend_error_batch(vec![notify::Error::path_not_found().add_path(missing)]);

        assert_eq!(
            (
                batch.failure.is_none(),
                batch.rescan,
                batch.structural,
                batch
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("recoverable error")),
            ),
            (true, true, true, true)
        );
    }

    #[test]
    fn poisoned_watcher_state_is_terminal_but_watch_operations_are_retryable() {
        assert_eq!(
            (
                WatcherError::WatchStatePoisoned.is_terminal_recovery_error(),
                WatcherError::WatchOperations {
                    details: "temporary add failure".to_string(),
                }
                .is_terminal_recovery_error(),
            ),
            (true, false)
        );
    }

    #[tokio::test]
    async fn spawn_watcher_emits_absolute_changed_file_path() {
        let temp = tempdir().expect("create tempdir");
        let root = temp.path();
        let (handle, mut rx) = spawn_watcher(root, DEFAULT_DEBOUNCE_MS)
            .await
            .expect("spawn watcher");

        let file_path = root.join("src.txt");
        fs::write(&file_path, "hello").expect("write file");

        let batch = receive_batch_containing(&mut rx, &file_path).await;
        assert!(batch.contains(&canonical(&file_path)));

        drop(handle);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn spawn_watcher_ignores_reads_but_reports_writes() {
        let temp = tempdir().expect("create tempdir");
        let root = temp.path();
        let file_path = root.join("tsconfig.base.json");
        fs::write(&file_path, "{}\n").expect("write file");
        let (handle, mut rx) = spawn_watcher(root, DEFAULT_DEBOUNCE_MS)
            .await
            .expect("spawn watcher");

        for _ in 0..200 {
            fs::read_to_string(&file_path).expect("read file");
        }
        assert_no_batch_containing(&mut rx, &file_path).await;

        fs::write(&file_path, "{\"compilerOptions\":{}}\n").expect("rewrite file");
        let batch = receive_batch_containing(&mut rx, &file_path).await;
        assert!(batch.contains(&canonical(&file_path)));

        drop(handle);
    }

    #[tokio::test]
    async fn spawn_watcher_filters_ignored_node_modules_paths() {
        let temp = tempdir().expect("create tempdir");
        let root = temp.path();
        let (handle, mut rx) = spawn_watcher(root, DEFAULT_DEBOUNCE_MS)
            .await
            .expect("spawn watcher");

        let ignored_dir = root.join("node_modules/pkg");
        fs::create_dir_all(&ignored_dir).expect("create ignored dir");
        let ignored_file = ignored_dir.join("index.js");
        fs::write(&ignored_file, "module.exports = 1;\n").expect("write ignored file");

        assert_no_batch_containing(&mut rx, &ignored_file).await;

        drop(handle);
    }

    #[tokio::test]
    async fn watcher_shutdown_completes_while_output_receiver_is_alive() {
        let temp = tempdir().expect("create tempdir");
        let (handle, _rx) = spawn_watcher(temp.path(), DEFAULT_DEBOUNCE_MS)
            .await
            .expect("spawn watcher");

        timeout(RECEIVE_TIMEOUT, handle.shutdown())
            .await
            .expect("watcher shutdown should complete");
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
    fn directory_scan_tolerates_a_vanished_subtree() {
        let temp = tempdir().expect("create tempdir");
        let vanished = temp.path().join("removed");

        assert_eq!(
            super::child_directories(&vanished).expect("skip vanished directory"),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn watch_discovery_uses_gitignore_but_not_other_implicit_ignore_sources() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let gitignored = root.join("gitignored/nested");
        let dot_ignored = root.join("dot-ignored/nested");
        let git_excluded = root.join("git-excluded/nested");
        fs::create_dir_all(&gitignored).expect("create gitignored dir");
        fs::create_dir_all(&dot_ignored).expect("create dot-ignore dir");
        fs::create_dir_all(&git_excluded).expect("create git-exclude dir");
        fs::create_dir_all(root.join(".git/info")).expect("create git info dir");
        fs::write(root.join(".gitignore"), "gitignored/\n").expect("write gitignore");
        fs::write(root.join(".ignore"), "dot-ignored/\n").expect("write dot-ignore");
        fs::write(root.join(".git/info/exclude"), "git-excluded/\n").expect("write git exclude");

        let filter = IgnoreFilter::new(&root).expect("build ignore filter");
        let dirs = discover_watch_dirs(&root, &filter).expect("discover dirs");

        assert!(!dirs.contains(&root.join("gitignored")));
        assert!(dirs.contains(&dot_ignored));
        assert!(dirs.contains(&git_excluded));
    }

    #[test]
    fn matcher_only_scan_does_not_collect_directory_paths() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        fs::create_dir_all(root.join("src/deep/tree")).expect("create source tree");

        let (_, dirs) = IgnoreFilter::scan(&root, false).expect("scan ignore matchers");

        assert!(dirs.is_empty());
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

    fn assert_collect_watch_batch(
        root: &Path,
        kind: EventKind,
        changed_path: PathBuf,
        expected_structural: bool,
    ) {
        let ignore_filter = IgnoreFilter::new(root).expect("build ignore filter");
        let batch = collect_watch_batch(
            &ignore_filter,
            vec![debounced_event(kind, vec![changed_path.clone()])],
        );
        assert_eq!(batch.structural, expected_structural);
        assert_eq!(batch.changed_paths, HashSet::from([changed_path]));
    }

    #[test]
    fn collect_watch_batch_sets_structural_for_folder_create_and_false_for_file_edit() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let package_dir = root.join("packages/app");
        let source_file = package_dir.join("src/lib.rs");
        let new_dir = package_dir.join("src/new-dir");
        fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source parent");
        fs::write(&source_file, "export const value = 1;\n").expect("write source file");
        fs::create_dir_all(&new_dir).expect("create new dir");

        assert_collect_watch_batch(&root, EventKind::Create(CreateKind::Folder), new_dir, true);
        assert_collect_watch_batch(
            &root,
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
            source_file,
            false,
        );
    }

    #[test]
    fn collect_watch_batch_sets_structural_for_root_config_data_edit() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let config_path = root.join("luchta-config.sh");
        let normal_file = root.join("src/foo.rs");
        fs::create_dir_all(normal_file.parent().expect("normal file parent"))
            .expect("create normal file parent");
        fs::write(&config_path, "#!/bin/sh\necho '{}'\n").expect("write config file");
        fs::write(&normal_file, "export const value = 1;\n").expect("write normal file");

        assert_collect_watch_batch(
            &root,
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
            config_path,
            true,
        );
        assert_collect_watch_batch(
            &root,
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
            normal_file,
            false,
        );
    }

    #[test]
    fn collect_watch_batch_preserves_backend_rescan_signal() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let event = Event::new(EventKind::Other)
            .set_flag(Flag::Rescan)
            .add_path(root.clone());
        let ignore_filter = IgnoreFilter::new(&root).expect("build ignore filter");

        let batch = collect_watch_batch(
            &ignore_filter,
            vec![DebouncedEvent {
                event,
                time: std::time::Instant::now(),
            }],
        );

        assert!(batch.rescan);
        assert_eq!(batch.changed_paths, HashSet::from([root]));
    }

    #[test]
    fn watcher_config_excludes_access_events() {
        let config = watcher_config();
        assert!(!config.event_kinds().intersects(EventKindMask::ALL_ACCESS));
        assert!(config.event_kinds().contains(EventKindMask::CORE));
    }

    #[test]
    fn collect_watch_batch_ignores_access_events() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let read_file = root.join("tsconfig.base.json");
        let gitignore = root.join(".gitignore");
        let config_path = root.join("luchta-config.sh");
        let edited_file = root.join("src/lib.rs");
        fs::create_dir_all(edited_file.parent().expect("edited file parent"))
            .expect("create edited file parent");
        fs::write(&read_file, "{}\n").expect("write read file");
        fs::write(&gitignore, "dist/\n").expect("write gitignore");
        fs::write(&config_path, "#!/bin/sh\necho '{}'\n").expect("write config file");
        fs::write(&edited_file, "export const value = 1;\n").expect("write edited file");
        let ignore_filter = IgnoreFilter::new(&root).expect("build ignore filter");
        let read_paths = vec![read_file, gitignore, config_path];

        let access_only = collect_watch_batch(
            &ignore_filter,
            vec![
                debounced_event(
                    EventKind::Access(AccessKind::Open(AccessMode::Any)),
                    read_paths.clone(),
                ),
                debounced_event(
                    EventKind::Access(AccessKind::Close(AccessMode::Read)),
                    read_paths.clone(),
                ),
                debounced_event(EventKind::Access(AccessKind::Read), read_paths.clone()),
            ],
        );
        assert!(access_only.changed_paths.is_empty(), "{access_only:?}");
        assert!(!access_only.structural);
        assert!(!access_only.rescan);

        let mixed = collect_watch_batch(
            &ignore_filter,
            vec![
                debounced_event(
                    EventKind::Access(AccessKind::Open(AccessMode::Any)),
                    read_paths,
                ),
                debounced_event(
                    EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                    vec![edited_file.clone()],
                ),
            ],
        );
        assert_eq!(mixed.changed_paths, HashSet::from([edited_file]));
        assert!(!mixed.structural);
        assert!(!mixed.rescan);
    }

    #[test]
    fn collect_watch_batch_honors_rescan_flag_on_access_event() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let event = Event::new(EventKind::Access(AccessKind::Any))
            .set_flag(Flag::Rescan)
            .add_path(root.clone());
        let ignore_filter = IgnoreFilter::new(&root).expect("build ignore filter");

        let batch = collect_watch_batch(
            &ignore_filter,
            vec![DebouncedEvent {
                event,
                time: std::time::Instant::now(),
            }],
        );

        assert!(batch.rescan);
        assert!(batch.changed_paths.is_empty());
    }

    #[test]
    fn nested_gitignore_rules_are_applied_and_can_be_reloaded() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let package = root.join("packages/app");
        let ignored_dir = package.join("generated");
        fs::create_dir_all(&ignored_dir).expect("create ignored dir");
        fs::write(package.join(".gitignore"), "generated/\n.gitignore\n")
            .expect("write self-ignoring nested gitignore");
        let ignored_file = ignored_dir.join("output.js");
        fs::write(&ignored_file, "generated").expect("write ignored file");

        let filter = IgnoreFilter::new(&root).expect("build ignore filter");
        assert!(filter.should_ignore(&ignored_file));

        fs::write(package.join(".gitignore"), "").expect("clear nested gitignore");
        let reloaded = IgnoreFilter::new(&root).expect("reload ignore filter");
        assert!(!reloaded.should_ignore(&ignored_file));
    }

    #[test]
    fn ignored_gitignore_edit_is_structural_and_requests_rescan() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let gitignore = root.join(".gitignore");
        fs::write(&gitignore, ".gitignore\n").expect("write self-ignoring gitignore");
        let ignore_filter = IgnoreFilter::new(&root).expect("build ignore filter");

        let batch = collect_watch_batch(
            &ignore_filter,
            vec![debounced_event(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                vec![gitignore.clone()],
            )],
        );

        assert!(batch.structural);
        assert!(batch.rescan);
        assert_eq!(batch.changed_paths, HashSet::from([gitignore]));
    }

    #[test]
    fn gitignore_events_under_ignored_trees_are_discarded() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        fs::write(root.join(".gitignore"), "generated/\n").expect("write root gitignore");
        let generated = root.join("generated/.gitignore");
        let dependency = root.join("node_modules/pkg/.gitignore");
        fs::create_dir_all(generated.parent().expect("generated parent"))
            .expect("create generated dir");
        fs::create_dir_all(dependency.parent().expect("dependency parent"))
            .expect("create dependency dir");
        let ignore_filter = IgnoreFilter::new(&root).expect("build ignore filter");

        let batch = collect_watch_batch(
            &ignore_filter,
            vec![debounced_event(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                vec![generated, dependency],
            )],
        );

        assert!(batch.is_empty());
    }

    fn seed_reconcile_watcher(
        root: &Path,
        keep_src: &Path,
        keep_tests: &Path,
        orphan_src: &Path,
    ) -> (
        Debouncer<RecommendedWatcher, WatcherCache>,
        HashSet<PathBuf>,
    ) {
        let mut watcher =
            create_debouncer(Duration::from_millis(DEFAULT_DEBOUNCE_MS), |_| {}).expect("watcher");
        let packages_dir = root.join("packages");
        let keep_package = root.join("packages/keep");
        let orphan_package = root.join("packages/orphan");
        for path in [
            root,
            packages_dir.as_path(),
            keep_package.as_path(),
            keep_src,
            keep_tests,
            orphan_package.as_path(),
            orphan_src,
        ] {
            watcher
                .watch(path, notify::RecursiveMode::NonRecursive)
                .expect("seed watch");
        }

        let watched_dirs = HashSet::from([
            root.to_path_buf(),
            packages_dir,
            keep_package,
            keep_src.to_path_buf(),
            keep_tests.to_path_buf(),
            orphan_package,
            orphan_src.to_path_buf(),
        ]);
        (watcher, watched_dirs)
    }

    #[test]
    fn reconcile_watched_dirs_preserves_survivors_adds_new_package_tree_and_unwatches_removed_tree()
    {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let keep_src = root.join("packages/keep/src");
        let keep_tests = root.join("packages/keep/tests");
        let orphan_src = root.join("packages/orphan/src");
        let new_src = root.join("packages/new/pkg/src");
        let new_tests = root.join("packages/new/pkg/tests");
        fs::create_dir_all(&keep_src).expect("create keep src dir");
        fs::create_dir_all(&keep_tests).expect("create keep tests dir");
        fs::create_dir_all(&orphan_src).expect("create orphan src dir");
        fs::create_dir_all(&new_src).expect("create new src dir");
        fs::create_dir_all(&new_tests).expect("create new tests dir");

        let ignore_filter = IgnoreFilter::new(&root).expect("build ignore filter");
        let (mut watcher, mut watched_dirs) =
            seed_reconcile_watcher(&root, &keep_src, &keep_tests, &orphan_src);
        let orphan_package = root.join("packages/orphan");

        fs::remove_dir_all(&orphan_package).expect("remove orphan package tree");
        let desired_dirs =
            discover_watch_dirs(&root, &ignore_filter).expect("discover desired dirs");

        let warnings =
            reconcile_watched_dirs(&mut watcher, &mut watched_dirs, desired_dirs.clone())
                .expect("reconcile watch dirs");
        assert!(warnings.is_empty());

        assert!(
            watched_dirs.contains(&keep_src),
            "surviving package content dir remains watched"
        );
        assert!(
            watched_dirs.contains(&keep_tests),
            "surviving package sibling content dir remains watched"
        );
        assert!(
            watched_dirs.contains(&new_src),
            "new package content dir added"
        );
        assert!(
            watched_dirs.contains(&new_tests),
            "new package sibling content dir added"
        );
        assert!(
            !watched_dirs.contains(&orphan_src),
            "removed package content dir unwatched"
        );
        assert_eq!(
            watched_dirs, desired_dirs,
            "authoritative set matches full discover walk"
        );

        let touch_file = new_src.join("lib.rs");
        fs::write(&touch_file, "pub fn added() {}\n").expect("write touched file");
        watcher
            .unwatch(&orphan_src)
            .expect_err("removed package dir already unwatched");
    }

    #[test]
    fn failed_watch_addition_keeps_existing_coverage() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let existing = root.join("existing");
        fs::create_dir_all(&existing).expect("create existing directory");
        let missing = root.join("missing");
        let mut watcher =
            create_debouncer(Duration::from_millis(DEFAULT_DEBOUNCE_MS), |_| {}).expect("watcher");
        for path in [&root, &existing] {
            watcher
                .watch(path, super::platform_recursive_mode())
                .expect("seed watch");
        }
        let mut watched_dirs = HashSet::from([root.clone(), existing.clone()]);
        let desired_dirs = HashSet::from([root, missing]);

        let result = reconcile_watched_dirs(&mut watcher, &mut watched_dirs, desired_dirs);

        assert!(result.is_err());
        assert!(
            watched_dirs.contains(&existing),
            "existing watches remain until all additions succeed"
        );
    }

    #[test]
    fn pending_watch_dirs_filters_duplicates() {
        let temp = tempdir().expect("create tempdir");
        let root = canonical(temp.path());
        let already_watched = root.join("src/already");
        let new_watched = root.join("src/new");
        fs::create_dir_all(&already_watched).expect("create already watched dir");
        fs::create_dir_all(&new_watched).expect("create new watched dir");
        let created = [already_watched.clone(), new_watched.clone()]
            .into_iter()
            .collect();
        let watched_dirs = HashSet::from([already_watched.clone()]);

        let pending = pending_watch_dirs(&watched_dirs, created);
        let expected: HashSet<_> = [new_watched.clone()].into_iter().collect();
        assert_eq!(pending.into_iter().collect::<HashSet<_>>(), expected);
        assert!(watched_dirs.contains(&already_watched));
        assert!(!watched_dirs.contains(&new_watched));
        assert_eq!(watched_dirs.len(), 1);
    }

    async fn receive_batch_containing(
        rx: &mut mpsc::Receiver<WatchBatch>,
        expected_path: &Path,
    ) -> HashSet<PathBuf> {
        let expected_path = canonical(expected_path);
        timeout(RECEIVE_TIMEOUT, async {
            loop {
                let batch = rx.recv().await.expect("watcher channel open");
                if batch.changed_paths.contains(&expected_path) {
                    return batch.changed_paths;
                }
            }
        })
        .await
        .expect("timed out waiting for watcher event")
    }

    fn debounced_event(kind: EventKind, paths: Vec<PathBuf>) -> DebouncedEvent {
        DebouncedEvent {
            event: Event {
                kind,
                paths,
                attrs: Default::default(),
            },
            time: std::time::Instant::now(),
        }
    }

    async fn assert_no_batch_containing(rx: &mut mpsc::Receiver<WatchBatch>, ignored_path: &Path) {
        let ignored_path = canonical(ignored_path);
        let result = timeout(QUIET_TIMEOUT, async {
            while let Some(batch) = rx.recv().await {
                assert!(
                    !batch.changed_paths.contains(&ignored_path),
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
