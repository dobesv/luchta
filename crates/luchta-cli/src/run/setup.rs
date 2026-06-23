//! Run setup helpers: building the memory monitor, the execution resources
//! (executor, cache, command map), and resolving the final run outcome.
//!
//! Extracted from `run.rs` to keep that module cohesive.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use luchta_cache::shared::{maybe_run_gc, SharedCache, DEFAULT_GC_RETENTION, DEFAULT_GC_THROTTLE};
#[cfg(unix)]
use luchta_cache::shared::{OpenExtras, RemoteConfig};
use luchta_cache::Cache;
use luchta_engine::{ExecutionRequest, TaskGraph, WeightedExecutor, WorkerManager};
use luchta_types::{EnvSpec, TaskId, WorkerDefinition};
use luchta_workspace::{PackageGraph, PackageNode};
use miette::{bail, Context, IntoDiagnostic, Result};

use super::{
    dispatch::{build_command_map, CommandMap},
    resolve_cache_dir,
};
use crate::progress::ProgressReporter;

/// Resolved memory-pressure thresholds passed to the dispatch loop.
///
/// `None` for either field means "use the default" (50% of total system memory
/// for usage, 1/16 of total for free), resolved by the `MemoryMonitor`.
pub struct MemoryPressureConfig {
    pub usage: Option<crate::memory_pressure::ThresholdSpec>,
    pub free: Option<crate::memory_pressure::ThresholdSpec>,
}

/// Builds the memory monitor and the shared pressure state from the resolved
/// threshold config. The monitor drives pause decisions; the `PressureState` is
/// shared so the status line can render the current warning suffix.
pub(super) fn build_memory_pressure(
    config: MemoryPressureConfig,
) -> (
    crate::memory_pressure::MemoryMonitor,
    Arc<crate::memory_pressure::PressureState>,
) {
    let monitor = crate::memory_pressure::MemoryMonitor::with_specs_for_current_process(
        config.usage,
        config.free,
    );
    let pressure_state = Arc::new(crate::memory_pressure::PressureState::new(
        monitor.usage_threshold,
        monitor.free_threshold,
    ));
    (monitor, pressure_state)
}

/// Resolves the dispatch loop's result into the run's final outcome: propagate
/// interruption, fail if any task failed, otherwise print the success summary.
pub(super) fn report_run_outcome(
    run_result: Result<()>,
    any_failed: &AtomicBool,
    reporter: &ProgressReporter,
    pressure_state: &crate::memory_pressure::PressureState,
) -> Result<()> {
    run_result?;

    if any_failed.load(Ordering::SeqCst) {
        bail!("one or more tasks failed");
    }

    let rss = select_summary_rss(
        pressure_state.snapshot().sample,
        crate::rss::process_tree_rss_bytes,
    );
    println!(
        "{}",
        reporter.render_summary(&crate::rss::format_rss(rss), owo_colors::Stream::Stdout)
    );
    Ok(())
}

fn select_summary_rss(
    sample: Option<crate::memory_pressure::MemorySample>,
    fallback: impl FnOnce() -> Option<u64>,
) -> Option<u64> {
    sample.map(|sample| sample.tree_rss).or_else(fallback)
}

/// Inputs for [`build_execution_resources`].
pub(super) struct BuildResourcesInputs<'a> {
    pub(super) task_graph: &'a TaskGraph,
    pub(super) packages: &'a [PackageNode],
    pub(super) workspace_root: &'a Path,
    pub(super) workers: &'a HashMap<String, WorkerDefinition>,
    pub(super) env: &'a BTreeMap<String, EnvSpec>,
    pub(super) worker_manager: &'a Arc<WorkerManager>,
    pub(super) max_weight: u32,
    pub(super) prefix_width: usize,
    pub(super) package_graph: Option<&'a PackageGraph>,
}

/// Execution resources shared across the dispatch loop and task runners.
pub(super) struct ExecutionResources {
    pub(super) executor: Arc<WeightedExecutor>,
    pub(super) cache: Arc<Cache>,
    pub(super) output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    pub(super) commands: HashMap<TaskId, ExecutionRequest>,
    pub(super) invalid: HashMap<TaskId, String>,
    pub(super) task_envs: HashMap<TaskId, BTreeMap<String, EnvSpec>>,
    pub(super) shared_cache: Option<Arc<SharedCache>>,
}

/// Environment variable enabling shared cache.
const SHARED_CACHE_ENABLED_ENV: &str = "LUCHTA_SHARED_CACHE";
/// Environment variable overriding shared cache GC retention, in days.
const SHARED_CACHE_GC_DAYS_ENV: &str = "LUCHTA_SHARED_CACHE_GC_DAYS";
/// Environment variable overriding shared cache output size cap, in megabytes.
const SHARED_CACHE_MAX_OUTPUT_MB_ENV: &str = "LUCHTA_SHARED_CACHE_MAX_OUTPUT_MB";
/// Environment variable overriding shared cache recent-commit history length.
const SHARED_CACHE_HISTORY_ENV: &str = "LUCHTA_SHARED_CACHE_HISTORY";
/// Environment variable overriding shared cache remote sync timeout, in seconds.
const SHARED_CACHE_SYNC_TIMEOUT_ENV: &str = "LUCHTA_SHARED_CACHE_SYNC_TIMEOUT";

/// Default shared cache size cap in megabytes.
const DEFAULT_SHARED_CACHE_SIZE_CAP_MB: u64 = 250;
/// Default shared cache history length (number of commits).
const DEFAULT_SHARED_CACHE_HISTORY_LEN: usize = 20;

fn parse_truthy_env_value(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some(raw) if raw.eq_ignore_ascii_case("1") || raw.eq_ignore_ascii_case("true") || raw.eq_ignore_ascii_case("on"))
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
    SharedCacheSettings {
        mode,
        sync_timeout: Duration::from_secs(sync_timeout_secs),
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

fn shared_cache_history_len() -> usize {
    parse_env_u64_or(
        SHARED_CACHE_HISTORY_ENV,
        std::env::var(SHARED_CACHE_HISTORY_ENV).ok().as_deref(),
        DEFAULT_SHARED_CACHE_HISTORY_LEN as u64,
    ) as usize
}

/// Builds the executor (with all task commands registered), the build cache,
/// the output-hash map, and the command map for a run.
pub(super) fn build_execution_resources(
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
            shared_cache_history_len(),
        )
        .map(Arc::new),
        SharedCacheMode::Remote { fs_base } => {
            #[cfg(unix)]
            {
                SharedCache::open_with_remote(
                    inputs.workspace_root,
                    shared_cache_size_cap_bytes(),
                    shared_cache_history_len(),
                    OpenExtras {
                        cache_dir: None,
                        remote: Some(RemoteConfig {
                            fs_base: fs_base.clone(),
                            sync_timeout: shared_cache_settings.sync_timeout,
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
                    shared_cache_history_len(),
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_pressure::MemorySample;

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
    fn parse_shared_cache_history_defaults_and_overrides() {
        assert_eq!(
            parse_env_u64_or(
                SHARED_CACHE_HISTORY_ENV,
                None,
                DEFAULT_SHARED_CACHE_HISTORY_LEN as u64
            ),
            20
        );
        assert_eq!(
            parse_env_u64_or(
                SHARED_CACHE_HISTORY_ENV,
                Some("64"),
                DEFAULT_SHARED_CACHE_HISTORY_LEN as u64
            ),
            64
        );
    }

    #[test]
    fn select_summary_rss_prefers_snapshot_sample_over_fallback() {
        let sample = MemorySample {
            tree_rss: 123,
            system_available: 456,
        };
        let rss = select_summary_rss(Some(sample), || panic!("fallback should not run"));
        assert_eq!(rss, Some(123));
    }

    #[test]
    fn select_summary_rss_falls_back_when_snapshot_missing() {
        let rss = select_summary_rss(None, || Some(789));
        assert_eq!(rss, Some(789));
    }
}
