//! Run setup helpers: building the memory monitor, the execution resources
//! (executor, cache, command map), and resolving the final run outcome.
//!
//! Extracted from `run.rs` to keep that module cohesive.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use luchta_cache::shared::{
    maybe_run_gc, SharedCache, DEFAULT_GC_RETENTION, DEFAULT_GC_THROTTLE,
    DEFAULT_SHARED_CACHE_DAY_WINDOW,
};
#[cfg(unix)]
use luchta_cache::shared::{OpenExtras, RemoteConfig};
use luchta_cache::{Cache, ListingCache};
use luchta_engine::{ExecutionRequest, TaskGraph, WeightedExecutor, WorkerManager};
use luchta_types::{EnvSpec, TaskId, WorkerDefinition};
use luchta_workspace::{PackageGraph, PackageNode};
use miette::{Context, IntoDiagnostic, Result};

use crate::outcome::TasksFailed;

use super::{
    dispatch::{build_command_map, CommandMap},
    resolve_cache_dir,
};
use crate::progress::ProgressReporter;

/// Builds the memory monitor and the shared pressure state.
///
/// The monitor drives pause decisions; the `PressureState` is shared so the
/// status line can render the current warning suffix.
pub(crate) fn build_memory_pressure(
    enabled: bool,
) -> (
    crate::memory_pressure::MemoryMonitor,
    Arc<crate::memory_pressure::PressureState>,
) {
    (
        crate::memory_pressure::MemoryMonitor::new(enabled),
        Arc::new(crate::memory_pressure::PressureState::new()),
    )
}

/// Resolves dispatch loop result into final outcome: propagate genuine setup or
/// interrupt errors loudly, always print summary after successful setup, then
/// return silent sentinel if one or more tasks failed.
pub(super) fn report_run_outcome(
    run_result: Result<()>,
    any_failed: &AtomicBool,
    reporter: &ProgressReporter,
    was_cancelled: bool,
) -> Result<()> {
    reporter.output().clear_progress();
    run_result?;

    reporter.output().stdout_line(&reporter.render_summary(
        &crate::rss::format_rss(reporter.tree_rss_blocking()),
        was_cancelled,
        owo_colors::Stream::Stdout,
    ));

    if any_failed.load(Ordering::SeqCst) {
        return Err(miette::Report::new(TasksFailed));
    }

    Ok(())
}

/// Inputs for [`build_execution_resources`].
pub(crate) struct BuildResourcesInputs<'a> {
    pub(crate) task_graph: &'a TaskGraph,
    pub(crate) packages: &'a [PackageNode],
    pub(crate) workspace_root: &'a Path,
    pub(crate) workers: &'a HashMap<String, WorkerDefinition>,
    pub(crate) env: &'a BTreeMap<String, EnvSpec>,
    pub(crate) worker_manager: &'a Arc<WorkerManager>,
    pub(crate) max_weight: u32,
    pub(crate) prefix_width: usize,
    pub(crate) package_graph: Option<&'a PackageGraph>,
}

/// Execution resources shared across the dispatch loop and task runners.
pub(crate) struct ExecutionResources {
    pub(crate) executor: Arc<WeightedExecutor>,
    pub(crate) cache: Arc<Cache>,
    pub(crate) output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    pub(crate) commands: HashMap<TaskId, ExecutionRequest>,
    pub(crate) invalid: HashMap<TaskId, String>,
    pub(crate) task_envs: HashMap<TaskId, BTreeMap<String, EnvSpec>>,
    pub(crate) shared_cache: Option<Arc<SharedCache>>,
    pub(crate) listing_cache: Arc<ListingCache>,
}

/// Environment variable enabling shared cache.
const SHARED_CACHE_ENABLED_ENV: &str = "LUCHTA_SHARED_CACHE";
/// Environment variable overriding shared cache GC retention, in days.
const SHARED_CACHE_GC_DAYS_ENV: &str = "LUCHTA_SHARED_CACHE_GC_DAYS";
/// Environment variable overriding shared cache output size cap, in megabytes.
const SHARED_CACHE_MAX_OUTPUT_MB_ENV: &str = "LUCHTA_SHARED_CACHE_MAX_OUTPUT_MB";
/// Environment variable overriding the shared cache read window, in days.
const SHARED_CACHE_DAYS_ENV: &str = "LUCHTA_SHARED_CACHE_DAYS";
/// Deprecated alias for `SHARED_CACHE_DAYS_ENV`, kept for one release. The old
/// name counted git commits, which this design no longer has — see
/// `shared_cache_day_window`'s doc comment.
const SHARED_CACHE_HISTORY_ENV_DEPRECATED: &str = "LUCHTA_SHARED_CACHE_HISTORY";
/// Environment variable overriding shared cache remote sync timeout, in seconds.
const SHARED_CACHE_SYNC_TIMEOUT_ENV: &str = "LUCHTA_SHARED_CACHE_SYNC_TIMEOUT";
/// Environment variable controlling how many consecutive rclone timeouts disable remote.
const SHARED_CACHE_TIMEOUT_DISABLE_THRESHOLD_ENV: &str =
    "LUCHTA_SHARED_CACHE_TIMEOUT_DISABLE_THRESHOLD";
/// Environment variable capping in-flight rclone operations against rcd.
const SHARED_CACHE_RCLONE_CONCURRENCY_ENV: &str = "LUCHTA_SHARED_CACHE_RCLONE_CONCURRENCY";

/// Default consecutive-timeout threshold before the remote cache is disabled.
///
/// Declared locally (not imported) because the authoritative constant lives in
/// the `#[cfg(unix)]`-only remote-cache modules, and `shared_cache_settings()`
/// compiles on all platforms. On unix a compile-time assertion below keeps this
/// in sync with `luchta_cache::shared::DEFAULT_TIMEOUT_DISABLE_THRESHOLD`.
const DEFAULT_TIMEOUT_DISABLE_THRESHOLD: usize = 8;
/// Default cap on in-flight rclone operations. See the note above re: unix.
const DEFAULT_RCLONE_CONCURRENCY: usize = 16;

#[cfg(unix)]
const _: () = {
    assert!(
        DEFAULT_TIMEOUT_DISABLE_THRESHOLD
            == luchta_cache::shared::DEFAULT_TIMEOUT_DISABLE_THRESHOLD,
        "CLI default timeout-disable threshold drifted from luchta-cache",
    );
    assert!(
        DEFAULT_RCLONE_CONCURRENCY == luchta_cache::shared::DEFAULT_RCLONE_CONCURRENCY,
        "CLI default rclone concurrency drifted from luchta-cache",
    );
};

/// Environment variable to disable all caching (no restore, no shared read/write).
pub(crate) const NO_CACHE_ENV: &str = "LUCHTA_NO_CACHE";

/// Environment variable to disable memory-pressure backpressure entirely.
pub(crate) const NO_MEM_PRESSURE_ENV: &str = "LUCHTA_NO_MEM_PRESSURE";

/// Default shared cache size cap in megabytes.
const DEFAULT_SHARED_CACHE_SIZE_CAP_MB: u64 = 250;

fn parse_truthy_env_value(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some(raw) if raw.eq_ignore_ascii_case("1") || raw.eq_ignore_ascii_case("true") || raw.eq_ignore_ascii_case("on"))
}

pub(crate) fn no_cache_env() -> bool {
    parse_truthy_env_value(std::env::var(NO_CACHE_ENV).ok().as_deref())
}

pub(crate) fn no_mem_pressure_env() -> bool {
    parse_truthy_env_value(std::env::var(NO_MEM_PRESSURE_ENV).ok().as_deref())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SharedCacheMode {
    Off,
    LocalOnly,
    Remote { fs_base: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SharedCacheSettings {
    mode: SharedCacheMode,
    sync_timeout: Duration,
    timeout_disable_threshold: usize,
    rclone_concurrency: usize,
}

fn parse_shared_cache_mode(value: Option<&str>) -> SharedCacheMode {
    match value.map(str::trim) {
        None | Some("") => SharedCacheMode::Off,
        Some(raw) if raw.len() >= 7 && raw[..7].eq_ignore_ascii_case("rclone:") => {
            let fs_spec = raw[7..].trim();
            if fs_spec.is_empty() {
                SharedCacheMode::Off
            } else {
                let fs_base = if fs_spec.contains(':') {
                    fs_spec.to_owned()
                } else {
                    format!("{fs_spec}:")
                };
                SharedCacheMode::Remote { fs_base }
            }
        }
        Some(raw) if raw.eq_ignore_ascii_case("local") || parse_truthy_env_value(Some(raw)) => {
            SharedCacheMode::LocalOnly
        }
        Some(_) => SharedCacheMode::Off,
    }
}

fn shared_cache_settings() -> SharedCacheSettings {
    let mode = parse_shared_cache_mode(std::env::var(SHARED_CACHE_ENABLED_ENV).ok().as_deref());
    let sync_timeout_secs = parse_env_u64_or(
        SHARED_CACHE_SYNC_TIMEOUT_ENV,
        std::env::var(SHARED_CACHE_SYNC_TIMEOUT_ENV).ok().as_deref(),
        30,
    );
    // A `0` (or invalid/unset) value falls back to the default: a 0 threshold
    // would disable the remote on the very first queued timeout — exactly the
    // behavior this policy exists to prevent — and a 0 concurrency limit would
    // stall all remote I/O.
    let timeout_disable_threshold = non_zero_env_u64_or(
        SHARED_CACHE_TIMEOUT_DISABLE_THRESHOLD_ENV,
        std::env::var(SHARED_CACHE_TIMEOUT_DISABLE_THRESHOLD_ENV)
            .ok()
            .as_deref(),
        DEFAULT_TIMEOUT_DISABLE_THRESHOLD as u64,
    ) as usize;
    let rclone_concurrency = non_zero_env_u64_or(
        SHARED_CACHE_RCLONE_CONCURRENCY_ENV,
        std::env::var(SHARED_CACHE_RCLONE_CONCURRENCY_ENV)
            .ok()
            .as_deref(),
        DEFAULT_RCLONE_CONCURRENCY as u64,
    ) as usize;
    SharedCacheSettings {
        mode,
        sync_timeout: Duration::from_secs(sync_timeout_secs),
        timeout_disable_threshold,
        rclone_concurrency,
    }
}

fn parse_env_u64_or(var: &str, value: Option<&str>, default: u64) -> u64 {
    match value.map(str::trim) {
        None | Some("") => default,
        Some(raw) => match raw.parse::<u64>() {
            Ok(parsed) => parsed,
            Err(err) => {
                eprintln!(
                    "warning: invalid {}={:?}: {}; using {}",
                    var, raw, err, default
                );
                default
            }
        },
    }
}

/// Like [`parse_env_u64_or`], but treats an explicit `0` as "use the default"
/// rather than a literal zero. Used for knobs where 0 is nonsensical/harmful
/// (a 0 disable-threshold or 0 concurrency limit).
fn non_zero_env_u64_or(var: &str, value: Option<&str>, default: u64) -> u64 {
    match parse_env_u64_or(var, value, default) {
        0 => default,
        n => n,
    }
}

fn shared_cache_gc_retention() -> Duration {
    let days = parse_env_u64_or(
        SHARED_CACHE_GC_DAYS_ENV,
        std::env::var(SHARED_CACHE_GC_DAYS_ENV).ok().as_deref(),
        DEFAULT_GC_RETENTION.as_secs() / (24 * 60 * 60),
    );
    Duration::from_secs(days.saturating_mul(24 * 60 * 60))
}

fn shared_cache_size_cap_bytes() -> u64 {
    let mb = parse_env_u64_or(
        SHARED_CACHE_MAX_OUTPUT_MB_ENV,
        std::env::var(SHARED_CACHE_MAX_OUTPUT_MB_ENV)
            .ok()
            .as_deref(),
        DEFAULT_SHARED_CACHE_SIZE_CAP_MB,
    );
    mb.saturating_mul(1024 * 1024)
}

/// Message printed to stderr when the deprecated `LUCHTA_SHARED_CACHE_HISTORY`
/// env var is set, or `None` if it isn't. Pulled out as a pure function of the
/// two "is set" flags so the exact wording — and the both-set precedence rule
/// — can be asserted directly, without going through real env vars or
/// capturing stderr.
fn shared_cache_days_deprecation_warning(old_set: bool, new_set: bool) -> Option<String> {
    if !old_set {
        return None;
    }
    Some(if new_set {
        format!(
            "warning: {SHARED_CACHE_HISTORY_ENV_DEPRECATED} is deprecated in favor of \
             {SHARED_CACHE_DAYS_ENV} and will be removed in a future release; both are set, \
             {SHARED_CACHE_DAYS_ENV} wins"
        )
    } else {
        format!(
            "warning: {SHARED_CACHE_HISTORY_ENV_DEPRECATED} is deprecated and will be removed \
             in a future release; set {SHARED_CACHE_DAYS_ENV} instead"
        )
    })
}

/// Days of shared-cache history to read (`SharedCache`'s `day_window`), not a
/// count of commits or shards: computed bucket keys replaced the old
/// commit/shard discovery scheme. Defaults to
/// `luchta_cache::shared::DEFAULT_SHARED_CACHE_DAY_WINDOW`, not a local
/// constant, so the CLI default can't drift from the crate's own default.
///
/// Reads `LUCHTA_SHARED_CACHE_DAYS`, falling back to the deprecated
/// `LUCHTA_SHARED_CACHE_HISTORY` for one release. If both are set, the new
/// name wins, and the deprecation warning says so.
fn shared_cache_day_window() -> usize {
    let new_value = std::env::var(SHARED_CACHE_DAYS_ENV).ok();
    let old_value = std::env::var(SHARED_CACHE_HISTORY_ENV_DEPRECATED).ok();

    if let Some(message) =
        shared_cache_days_deprecation_warning(old_value.is_some(), new_value.is_some())
    {
        eprintln!("{message}");
    }

    let (var_name, value) = match &new_value {
        Some(v) => (SHARED_CACHE_DAYS_ENV, Some(v.as_str())),
        None => (SHARED_CACHE_HISTORY_ENV_DEPRECATED, old_value.as_deref()),
    };

    non_zero_env_u64_or(var_name, value, DEFAULT_SHARED_CACHE_DAY_WINDOW as u64) as usize
}

/// Builds the executor (with all task commands registered), the build cache,
/// the output-hash map, and the command map for a run.
pub(crate) fn build_execution_resources(
    inputs: BuildResourcesInputs<'_>,
) -> Result<ExecutionResources> {
    let executor = Arc::new(
        WeightedExecutor::new(inputs.max_weight)
            .with_worker_manager(Arc::clone(inputs.worker_manager))
            .with_prefix_width(inputs.prefix_width),
    );
    let cache = Arc::new(
        Cache::open(&resolve_cache_dir(inputs.workspace_root))
            .into_diagnostic()
            .wrap_err("open cache")?,
    );
    let output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>> = Arc::new(Mutex::new(HashMap::new()));

    let shared_cache_settings = shared_cache_settings();
    let shared_cache = match &shared_cache_settings.mode {
        SharedCacheMode::Off => None,
        SharedCacheMode::LocalOnly => SharedCache::open(
            inputs.workspace_root,
            shared_cache_size_cap_bytes(),
            shared_cache_day_window(),
        )
        .map(Arc::new),
        SharedCacheMode::Remote { fs_base } => {
            #[cfg(unix)]
            {
                SharedCache::open_with_remote(
                    inputs.workspace_root,
                    shared_cache_size_cap_bytes(),
                    shared_cache_day_window(),
                    OpenExtras {
                        cache_dir: None,
                        remote: Some(RemoteConfig {
                            fs_base: fs_base.clone(),
                            sync_timeout: shared_cache_settings.sync_timeout,
                            timeout_disable_threshold: shared_cache_settings
                                .timeout_disable_threshold,
                            rclone_concurrency: shared_cache_settings.rclone_concurrency,
                        }),
                    },
                )
                .map(Arc::new)
            }
            #[cfg(not(unix))]
            {
                let _ = fs_base;
                SharedCache::open(
                    inputs.workspace_root,
                    shared_cache_size_cap_bytes(),
                    shared_cache_day_window(),
                )
                .map(Arc::new)
            }
        }
    };

    if let Some(shared_cache) = shared_cache.as_ref() {
        let _ = maybe_run_gc(
            shared_cache.paths(),
            shared_cache_gc_retention(),
            DEFAULT_GC_THROTTLE,
        );
    }

    let CommandMap {
        commands,
        invalid,
        task_envs,
    } = build_command_map(
        inputs.task_graph,
        inputs.packages,
        inputs.workspace_root,
        inputs.env,
        inputs.workers,
        inputs.package_graph,
    );

    for request in commands.values() {
        executor.register(request.clone());
    }

    Ok(ExecutionResources {
        executor,
        cache,
        output_hashes,
        commands,
        invalid,
        task_envs,
        shared_cache,
        listing_cache: Arc::new(ListingCache::default()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use luchta_cache::shared::SHARED_CACHE_SHARD_COUNT;
    use luchta_test_support::require_nextest;
    use std::sync::Mutex;

    /// Process-wide lock to serialize env-mutating tests.
    /// Prevents races when multiple tests use set_var/remove_var concurrently.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Guard that restores an environment variable to its prior value on drop.
    /// Captures the current value on construction (if any) and restores it
    /// (or removes if it was absent) when dropped, even on panic.
    struct EnvVarGuard {
        name: &'static str,
        prior: Option<String>,
    }

    impl EnvVarGuard {
        /// Set an env var and return a guard that will restore the prior value.
        fn set(name: &'static str, value: &str) -> Self {
            let prior = std::env::var(name).ok();
            std::env::set_var(name, value);
            Self { name, prior }
        }

        /// Remove an env var and return a guard that will restore the prior value.
        fn remove(name: &'static str) -> Self {
            let prior = std::env::var(name).ok();
            std::env::remove_var(name);
            Self { name, prior }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(ref value) = self.prior {
                std::env::set_var(self.name, value);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }

    #[test]
    fn parse_truthy_env_value_accepts_expected_values() {
        for value in ["1", "true", "on", "TRUE", "On"] {
            assert!(
                parse_truthy_env_value(Some(value)),
                "expected {value} to enable"
            );
        }
    }

    #[test]
    fn parse_truthy_env_value_rejects_non_truthy_values() {
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("off"),
            Some("nope"),
        ] {
            assert!(
                !parse_truthy_env_value(value),
                "expected {value:?} to disable"
            );
        }
    }

    #[test]
    fn parse_shared_cache_mode_matrix() {
        assert_eq!(parse_shared_cache_mode(None), SharedCacheMode::Off);
        assert_eq!(parse_shared_cache_mode(Some("")), SharedCacheMode::Off);
        assert_eq!(
            parse_shared_cache_mode(Some("local")),
            SharedCacheMode::LocalOnly
        );
        assert_eq!(
            parse_shared_cache_mode(Some("1")),
            SharedCacheMode::LocalOnly
        );
        assert_eq!(
            parse_shared_cache_mode(Some("true")),
            SharedCacheMode::LocalOnly
        );
        assert_eq!(
            parse_shared_cache_mode(Some("rclone:luchta")),
            SharedCacheMode::Remote {
                fs_base: "luchta:".to_owned()
            }
        );
        assert_eq!(
            parse_shared_cache_mode(Some("rclone:luchta:bucket/prefix")),
            SharedCacheMode::Remote {
                fs_base: "luchta:bucket/prefix".to_owned()
            }
        );
    }

    #[test]
    fn shared_cache_settings_default_timeout() {
        let settings = SharedCacheSettings {
            mode: parse_shared_cache_mode(Some("local")),
            sync_timeout: Duration::from_secs(parse_env_u64_or(
                SHARED_CACHE_SYNC_TIMEOUT_ENV,
                None,
                30,
            )),
            timeout_disable_threshold: DEFAULT_TIMEOUT_DISABLE_THRESHOLD,
            rclone_concurrency: DEFAULT_RCLONE_CONCURRENCY,
        };
        assert_eq!(settings.sync_timeout, Duration::from_secs(30));
    }

    #[test]
    fn shared_cache_settings_override_timeout() {
        let settings = SharedCacheSettings {
            mode: parse_shared_cache_mode(Some("rclone:luchta")),
            sync_timeout: Duration::from_secs(parse_env_u64_or(
                SHARED_CACHE_SYNC_TIMEOUT_ENV,
                Some("5"),
                30,
            )),
            timeout_disable_threshold: DEFAULT_TIMEOUT_DISABLE_THRESHOLD,
            rclone_concurrency: DEFAULT_RCLONE_CONCURRENCY,
        };
        assert_eq!(settings.sync_timeout, Duration::from_secs(5));
        assert_eq!(
            settings.mode,
            SharedCacheMode::Remote {
                fs_base: "luchta:".to_owned()
            }
        );
    }

    #[test]
    fn parse_env_u64_or_uses_default_for_unset_or_empty() {
        assert_eq!(parse_env_u64_or("TEST_VAR", None, 14), 14);
        assert_eq!(parse_env_u64_or("TEST_VAR", Some(""), 14), 14);
        assert_eq!(parse_env_u64_or("TEST_VAR", Some("   "), 14), 14);
    }

    #[test]
    fn parse_env_u64_or_parses_valid_values() {
        assert_eq!(parse_env_u64_or("TEST_VAR", Some("42"), 14), 42);
        assert_eq!(parse_env_u64_or("TEST_VAR", Some(" 7 "), 14), 7);
    }

    #[test]
    fn parse_env_u64_or_falls_back_for_invalid_values() {
        assert_eq!(parse_env_u64_or("TEST_VAR", Some("abc"), 14), 14);
        assert_eq!(parse_env_u64_or("TEST_VAR", Some("-3"), 14), 14);
    }

    #[test]
    fn parse_shared_cache_gc_retention_uses_default_when_unset() {
        assert_eq!(
            parse_env_u64_or(
                SHARED_CACHE_GC_DAYS_ENV,
                None,
                DEFAULT_GC_RETENTION.as_secs() / (24 * 60 * 60),
            ),
            14
        );
        assert_eq!(Duration::from_secs(14 * 24 * 60 * 60), DEFAULT_GC_RETENTION);
    }

    #[test]
    fn non_zero_env_u64_or_maps_zero_and_invalid_to_default() {
        // Unset (None) → default
        assert_eq!(non_zero_env_u64_or("TEST_VAR", None, 42), 42);
        // Empty string → default
        assert_eq!(non_zero_env_u64_or("TEST_VAR", Some(""), 42), 42);
        // "0" maps to default (key behavior: 0 is nonsensical for threshold/concurrency)
        assert_eq!(non_zero_env_u64_or("TEST_VAR", Some("0"), 42), 42);
        // Valid non-zero → parsed value
        assert_eq!(non_zero_env_u64_or("TEST_VAR", Some("5"), 42), 5);
        // Invalid → default
        assert_eq!(non_zero_env_u64_or("TEST_VAR", Some("abc"), 42), 42);
    }

    #[test]
    fn parse_shared_cache_gc_retention_overrides_when_set() {
        let days = parse_env_u64_or(SHARED_CACHE_GC_DAYS_ENV, Some("3"), 14);
        assert_eq!(
            Duration::from_secs(days * 24 * 60 * 60),
            Duration::from_secs(3 * 24 * 60 * 60)
        );
    }

    #[test]
    fn parse_shared_cache_size_cap_defaults_and_overrides() {
        assert_eq!(
            parse_env_u64_or(
                SHARED_CACHE_MAX_OUTPUT_MB_ENV,
                None,
                DEFAULT_SHARED_CACHE_SIZE_CAP_MB
            ),
            250
        );
        assert_eq!(
            parse_env_u64_or(
                SHARED_CACHE_MAX_OUTPUT_MB_ENV,
                Some("512"),
                DEFAULT_SHARED_CACHE_SIZE_CAP_MB
            ),
            512
        );
        assert_eq!(
            DEFAULT_SHARED_CACHE_SIZE_CAP_MB * 1024 * 1024,
            250 * 1024 * 1024
        );
    }

    #[test]
    fn parse_shared_cache_days_defaults_and_overrides() {
        assert_eq!(
            non_zero_env_u64_or(
                SHARED_CACHE_DAYS_ENV,
                None,
                DEFAULT_SHARED_CACHE_DAY_WINDOW as u64
            ),
            3
        );
        assert_eq!(
            non_zero_env_u64_or(
                SHARED_CACHE_DAYS_ENV,
                Some("64"),
                DEFAULT_SHARED_CACHE_DAY_WINDOW as u64
            ),
            64
        );
        // "0" would otherwise make `bucket_keys_for` return an empty read
        // set, silently disabling shared-cache reads — fall back to the default.
        assert_eq!(
            non_zero_env_u64_or(
                SHARED_CACHE_DAYS_ENV,
                Some("0"),
                DEFAULT_SHARED_CACHE_DAY_WINDOW as u64
            ),
            3
        );
    }

    #[test]
    fn shared_cache_days_deprecation_warning_matrix() {
        // Neither set: no warning.
        assert_eq!(shared_cache_days_deprecation_warning(false, false), None);
        // New-only: no warning; the deprecated name was never touched.
        assert_eq!(shared_cache_days_deprecation_warning(false, true), None);
        // Old-only: warns, but doesn't claim a precedence conflict that isn't happening.
        let old_only = shared_cache_days_deprecation_warning(true, false).unwrap();
        assert!(old_only.contains(SHARED_CACHE_HISTORY_ENV_DEPRECATED));
        assert!(old_only.contains(SHARED_CACHE_DAYS_ENV));
        assert!(old_only.contains("deprecated"));
        assert!(!old_only.contains("wins"));
        // Both set: warns AND says which one wins.
        let both_set = shared_cache_days_deprecation_warning(true, true).unwrap();
        assert!(both_set.contains("both are set"));
        assert!(both_set.contains("wins"));
    }

    #[test]
    fn shared_cache_day_window_uses_deprecated_env_when_only_it_is_set() {
        require_nextest();
        let _lock = ENV_LOCK.lock().unwrap();
        let _new_guard = EnvVarGuard::remove(SHARED_CACHE_DAYS_ENV);
        let _old_guard = EnvVarGuard::set(SHARED_CACHE_HISTORY_ENV_DEPRECATED, "9");
        assert_eq!(shared_cache_day_window(), 9);
    }

    #[test]
    fn shared_cache_day_window_prefers_new_env_when_both_are_set() {
        require_nextest();
        let _lock = ENV_LOCK.lock().unwrap();
        let _new_guard = EnvVarGuard::set(SHARED_CACHE_DAYS_ENV, "5");
        let _old_guard = EnvVarGuard::set(SHARED_CACHE_HISTORY_ENV_DEPRECATED, "9");
        assert_eq!(shared_cache_day_window(), 5);
    }

    #[test]
    fn shared_cache_day_window_treats_zero_as_default() {
        require_nextest();
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::set(SHARED_CACHE_DAYS_ENV, "0");
        let _old_guard = EnvVarGuard::remove(SHARED_CACHE_HISTORY_ENV_DEPRECATED);
        assert_eq!(shared_cache_day_window(), DEFAULT_SHARED_CACHE_DAY_WINDOW);
    }

    #[test]
    fn shared_cache_day_window_default_is_three_days_of_six_shards_each() {
        // Pins both constants together: a future change to either one that
        // isn't deliberate would silently inflate (or shrink) the number of
        // buckets fetched per restore. Also guards against the CLI's default
        // drifting from `luchta-cache`'s own default the way it did before
        // this test existed (`DEFAULT_SHARED_CACHE_HISTORY_LEN = 20` fed into
        // what is now `day_window`, an 18-vs-120-key blow-up).
        require_nextest();
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::remove(SHARED_CACHE_DAYS_ENV);
        let _old_guard = EnvVarGuard::remove(SHARED_CACHE_HISTORY_ENV_DEPRECATED);
        assert_eq!(DEFAULT_SHARED_CACHE_DAY_WINDOW, 3);
        assert_eq!(shared_cache_day_window(), DEFAULT_SHARED_CACHE_DAY_WINDOW);
        assert_eq!(
            shared_cache_day_window() * SHARED_CACHE_SHARD_COUNT,
            18,
            "default read set must be 18 computed bucket keys"
        );
    }

    // ---------------------------------------------------------------------------
    // Truthy boolean env vars: no_cache_env() and no_mem_pressure_env()
    //
    // Both are `parse_truthy_env_value` applied to a different env var, so
    // their unset/truthy/non-truthy behavior is identical by construction.
    // Table-driven across both rather than duplicated per variable — a third
    // `LUCHTA_NO_*` switch added later just extends the table.
    // ---------------------------------------------------------------------------

    /// A `LUCHTA_NO_*` boolean env var paired with its accessor.
    type TruthyEnvCase = (&'static str, fn() -> bool);

    fn truthy_env_cases() -> [TruthyEnvCase; 2] {
        [
            (NO_CACHE_ENV, no_cache_env as fn() -> bool),
            (NO_MEM_PRESSURE_ENV, no_mem_pressure_env as fn() -> bool),
        ]
    }

    #[test]
    fn truthy_env_vars_return_true_for_truthy_values() {
        require_nextest();
        let _lock = ENV_LOCK.lock().unwrap();
        for (env_var, env_fn) in truthy_env_cases() {
            let _remove_guard = EnvVarGuard::remove(env_var);
            for value in ["1", "true", "on", "TRUE", "On", " 1 ", " TRUE "] {
                let _guard = EnvVarGuard::set(env_var, value);
                assert!(env_fn(), "expected {env_var}={value:?} to return true");
            }
        }
    }

    #[test]
    fn truthy_env_vars_return_false_for_non_truthy_values() {
        require_nextest();
        let _lock = ENV_LOCK.lock().unwrap();
        for (env_var, env_fn) in truthy_env_cases() {
            let _remove_guard = EnvVarGuard::remove(env_var);
            for value in ["0", "false", "off", "nope", "", "  "] {
                let _guard = EnvVarGuard::set(env_var, value);
                assert!(!env_fn(), "expected {env_var}={value:?} to return false");
            }
        }
    }

    #[test]
    fn truthy_env_vars_return_false_when_unset() {
        require_nextest();
        let _lock = ENV_LOCK.lock().unwrap();
        for (env_var, env_fn) in truthy_env_cases() {
            let _guard = EnvVarGuard::remove(env_var);
            assert!(!env_fn(), "expected unset {env_var} to return false");
        }
    }

    // ---------------------------------------------------------------------------
    // Flag-or-env composition: the CLI flag ORs with the env var, computed the
    // same way in main.rs for both `no_cache` and `no_mem_pressure`. Shared
    // matrix runner; the two effective-value closures below are what differ —
    // in particular the `no_mem_pressure` one negates the result, which is the
    // exact inversion the second test below exists to pin.
    // ---------------------------------------------------------------------------

    /// Runs `effective(cli_flag)` against each `(cli_flag, env_value, expected,
    /// why)` case, setting or removing `env_var` before every case.
    fn assert_flag_or_env_matrix(
        env_var: &'static str,
        effective: impl Fn(bool) -> bool,
        cases: &[(bool, Option<&str>, bool, &str)],
    ) {
        require_nextest();
        let _lock = ENV_LOCK.lock().unwrap();
        for &(cli_flag, env_value, expected, why) in cases {
            let _guard = match env_value {
                Some(value) => EnvVarGuard::set(env_var, value),
                None => EnvVarGuard::remove(env_var),
            };
            assert_eq!(effective(cli_flag), expected, "{why}");
        }
    }

    #[test]
    fn no_cache_flag_or_env_semantics() {
        // Models the computation in main.rs: effective = CLI flag OR env var.
        fn effective_no_cache(cli_flag: bool) -> bool {
            cli_flag || no_cache_env()
        }

        assert_flag_or_env_matrix(
            NO_CACHE_ENV,
            effective_no_cache,
            &[
                (true, Some("0"), true, "cli_flag=true should always be true"),
                (true, Some("1"), true, "cli_flag=true should always be true"),
                (
                    false,
                    Some("1"),
                    true,
                    "cli_flag=false, env=true => effective=true",
                ),
                (
                    false,
                    Some("0"),
                    false,
                    "cli_flag=false, env=false => effective=false",
                ),
                (
                    false,
                    None,
                    false,
                    "cli_flag=false, env unset => effective=false",
                ),
            ],
        );
    }

    /// Pins the flag/env combination against the exact bug the design doc
    /// warns about: the CLI flag is negative (`--no-mem-pressure`) but the
    /// field it feeds (`memory_pressure_enabled`) reads positively. Getting
    /// that inversion backwards would silently disable backpressure by
    /// default and never turn it off when asked, or vice versa.
    #[test]
    fn no_mem_pressure_flag_or_env_inverts_to_enabled_correctly() {
        // Models the computation in main.rs:
        //   no_mem_pressure = cli_flag || no_mem_pressure_env()
        //   memory_pressure_enabled = !no_mem_pressure
        fn effective_memory_pressure_enabled(cli_flag: bool) -> bool {
            !(cli_flag || no_mem_pressure_env())
        }

        assert_flag_or_env_matrix(
            NO_MEM_PRESSURE_ENV,
            effective_memory_pressure_enabled,
            &[
                (
                    false,
                    Some("0"),
                    true,
                    "no flag, env=0 => memory_pressure_enabled=true",
                ),
                (true, None, false, "--no-mem-pressure always disables"),
                (
                    true,
                    Some("1"),
                    false,
                    "--no-mem-pressure always disables, even with env=1",
                ),
                (
                    false,
                    Some("1"),
                    false,
                    "no flag, env=1 => memory_pressure_enabled=false",
                ),
                (
                    false,
                    None,
                    true,
                    "no flag, env unset => memory_pressure_enabled=true",
                ),
            ],
        );
    }
}
