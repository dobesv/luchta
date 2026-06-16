use std::collections::HashSet;
use std::time::{Duration, Instant};

use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq)]
pub enum ThresholdSpec {
    Percent(f64),
    Absolute(u64),
}

impl ThresholdSpec {
    pub fn resolve(&self, total_mem: u64) -> u64 {
        match *self {
            Self::Percent(percent) => {
                if !percent.is_finite() || percent <= 0.0 {
                    return 0;
                }

                let resolved = (total_mem as f64) * percent / 100.0;
                if !resolved.is_finite() || resolved <= 0.0 {
                    0
                } else if resolved >= u64::MAX as f64 {
                    u64::MAX
                } else {
                    resolved as u64
                }
            }
            Self::Absolute(bytes) => bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MemorySample {
    pub(crate) tree_rss: u64,
    pub(crate) system_available: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PressureReason {
    UsageHigh,
    FreeLow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryPressure {
    pub(crate) sample: MemorySample,
    pub(crate) reasons: Vec<PressureReason>,
    pub(crate) paused: bool,
}

/// Shared state tracking the most recent pressure check result.
///
/// Used by `dispatch_loop` to check pressure before dispatching, and by the
/// progress-rendering path to display warnings (Task 5).
#[derive(Debug, Default)]
pub(crate) struct PressureState {
    /// The reasons from the most recent `check()` call.
    /// Empty when not paused.
    latest_reasons: std::sync::RwLock<Vec<PressureReason>>,
}

impl PressureState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Update with the latest pressure check result.
    pub(crate) fn update(&self, pressure: &MemoryPressure) {
        *self
            .latest_reasons
            .write()
            .expect("pressure state lock poisoned") = pressure.reasons.clone();
    }

    /// Get the current pressure reasons (for Task 5 status-line rendering).
    pub(crate) fn reasons(&self) -> std::sync::RwLockReadGuard<'_, Vec<PressureReason>> {
        self.latest_reasons
            .read()
            .expect("pressure state lock poisoned")
    }
}

pub(crate) struct MemoryMonitor {
    sys: System,
    root_pid: Pid,
    pub(crate) usage_threshold: u64,
    pub(crate) free_threshold: u64,
    cache: Option<(Instant, MemorySample)>,
    /// Test seam: when Some, this closure is called instead of the real pressure check.
    /// Used by integration tests to force paused=true deterministically.
    #[cfg(test)]
    test_override: Option<std::sync::Arc<dyn Fn() -> MemoryPressure + Send + Sync>>,
    ttl: Duration,
    recompute_count: u64,
}

impl MemoryMonitor {
    const DEFAULT_TTL: Duration = Duration::from_millis(250);

    /// Constructs a monitor with explicit absolute thresholds and root PID.
    /// Test-only: production builds resolve thresholds from specs via
    /// [`MemoryMonitor::with_specs`] / [`with_specs_for_current_process`].
    #[cfg(test)]
    pub(crate) fn new(usage_threshold: u64, free_threshold: u64, root_pid: Pid) -> Self {
        Self {
            sys: System::new(),
            root_pid,
            usage_threshold,
            free_threshold,
            cache: None,
            ttl: Self::DEFAULT_TTL,
            recompute_count: 0,
            test_override: None,
        }
    }

    /// Test-only convenience constructor rooted at the current process.
    #[cfg(test)]
    pub(crate) fn for_current_process(usage_threshold: u64, free_threshold: u64) -> Self {
        Self::new(
            usage_threshold,
            free_threshold,
            Pid::from_u32(std::process::id()),
        )
    }

    pub(crate) fn with_specs(
        usage_threshold: Option<ThresholdSpec>,
        free_threshold: Option<ThresholdSpec>,
        root_pid: Pid,
    ) -> Self {
        let mut sys = System::new();
        sys.refresh_memory();
        let total_memory = sys.total_memory();
        let usage_threshold = usage_threshold
            .unwrap_or(ThresholdSpec::Percent(50.0))
            .resolve(total_memory);
        let free_threshold = free_threshold
            .unwrap_or(ThresholdSpec::Absolute(total_memory / 16))
            .resolve(total_memory);

        Self {
            sys,
            root_pid,
            usage_threshold,
            free_threshold,
            cache: None,
            ttl: Self::DEFAULT_TTL,
            recompute_count: 0,
            #[cfg(test)]
            test_override: None,
        }
    }

    pub(crate) fn with_specs_for_current_process(
        usage_threshold: Option<ThresholdSpec>,
        free_threshold: Option<ThresholdSpec>,
    ) -> Self {
        Self::with_specs(
            usage_threshold,
            free_threshold,
            Pid::from_u32(std::process::id()),
        )
    }

    pub(crate) fn check(&mut self) -> MemoryPressure {
        #[cfg(test)]
        if let Some(ref override_fn) = self.test_override {
            return override_fn();
        }
        self.check_at(Instant::now())
    }

    fn check_at(&mut self, now: Instant) -> MemoryPressure {
        let sample = match self.cache {
            Some((cached_at, sample)) if now.duration_since(cached_at) < self.ttl => sample,
            _ => {
                let sample = self.recompute_sample();
                self.cache = Some((now, sample));
                sample
            }
        };

        let mut reasons = Vec::new();
        if sample.tree_rss > self.usage_threshold {
            reasons.push(PressureReason::UsageHigh);
        }
        if sample.system_available < self.free_threshold {
            reasons.push(PressureReason::FreeLow);
        }

        let paused = !reasons.is_empty();
        MemoryPressure {
            sample,
            reasons,
            paused,
        }
    }

    fn recompute_sample(&mut self) -> MemorySample {
        self.recompute_count += 1;
        self.sys.refresh_memory();
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );

        MemorySample {
            // sysinfo 0.36 `Process::memory()` returns bytes, not KiB.
            tree_rss: compute_tree_rss(self.sys.processes(), self.root_pid).unwrap_or(0),
            system_available: self.sys.available_memory(),
        }
    }

    #[cfg(test)]
    fn set_ttl(&mut self, ttl: Duration) {
        self.ttl = ttl;
    }

    #[cfg(test)]
    fn recompute_count(&self) -> u64 {
        self.recompute_count
    }

    /// Test seam: set a closure to override check() results.
    /// When set, `check()` will call this closure instead of doing
    /// the real memory sampling.
    #[cfg(test)]
    pub(crate) fn set_test_override(
        &mut self,
        override_fn: Option<std::sync::Arc<dyn Fn() -> MemoryPressure + Send + Sync>>,
    ) {
        self.test_override = override_fn;
    }
}

fn compute_tree_rss(
    processes: &std::collections::HashMap<Pid, Process>,
    root_pid: Pid,
) -> Option<u64> {
    if !processes.contains_key(&root_pid) {
        return None;
    }

    let process_count = processes.len().max(1);
    let mut tree_rss = 0_u64;

    for (pid, process) in processes {
        if pid == &root_pid || is_descendant_of(processes, *pid, root_pid, process_count) {
            tree_rss = tree_rss.saturating_add(process.memory());
        }
    }

    Some(tree_rss)
}

fn is_descendant_of(
    processes: &std::collections::HashMap<Pid, Process>,
    pid: Pid,
    root_pid: Pid,
    max_steps: usize,
) -> bool {
    let mut current = pid;
    let mut visited = HashSet::with_capacity(max_steps.min(64));

    for _ in 0..max_steps {
        if !visited.insert(current) {
            return false;
        }

        let Some(process) = processes.get(&current) else {
            return false;
        };
        let Some(parent) = process.parent() else {
            return false;
        };
        if parent == root_pid {
            return true;
        }
        current = parent;
    }

    false
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ThresholdParseError {
    #[error("memory threshold cannot be empty")]
    Empty,
    #[error("memory threshold must be a non-negative number")]
    InvalidNumber,
    #[error("memory threshold unit '{unit}' is not recognized")]
    UnknownUnit { unit: String },
    #[error("memory threshold value is too large")]
    Overflow,
}

pub fn parse_threshold(s: &str) -> Result<ThresholdSpec, ThresholdParseError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(ThresholdParseError::Empty);
    }
    if trimmed.starts_with('-') {
        return Err(ThresholdParseError::InvalidNumber);
    }

    match trimmed.strip_suffix('%') {
        Some(number_part) => parse_percent(number_part),
        None => parse_absolute(trimmed),
    }
}

/// Parses the `<number>%` form into a [`ThresholdSpec::Percent`].
fn parse_percent(number_part: &str) -> Result<ThresholdSpec, ThresholdParseError> {
    let number = number_part.trim();
    if number.is_empty() {
        return Err(ThresholdParseError::InvalidNumber);
    }

    let percent = number
        .parse::<f64>()
        .map_err(|_| ThresholdParseError::InvalidNumber)?;
    if !percent.is_finite() || percent < 0.0 {
        return Err(ThresholdParseError::InvalidNumber);
    }

    Ok(ThresholdSpec::Percent(percent))
}

/// Parses a bare integer with an optional binary/decimal unit suffix into a
/// [`ThresholdSpec::Absolute`] byte count.
fn parse_absolute(trimmed: &str) -> Result<ThresholdSpec, ThresholdParseError> {
    let split_idx = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(trimmed.len());
    let (number_part, unit_part) = trimmed.split_at(split_idx);

    if number_part.is_empty() || number_part.contains('.') {
        return Err(ThresholdParseError::InvalidNumber);
    }

    let value = number_part
        .parse::<u64>()
        .map_err(|_| ThresholdParseError::InvalidNumber)?;

    let multiplier = unit_multiplier(unit_part.trim())?;
    let bytes = value
        .checked_mul(multiplier)
        .ok_or(ThresholdParseError::Overflow)?;

    Ok(ThresholdSpec::Absolute(bytes))
}

fn unit_multiplier(unit: &str) -> Result<u64, ThresholdParseError> {
    if unit.is_empty() {
        return Ok(1);
    }

    let normalized = unit.to_ascii_lowercase();
    match normalized.as_str() {
        "b" => Ok(1),
        "k" | "ki" | "kib" => Ok(1024),
        "kb" => Ok(1000),
        "m" | "mi" | "mib" => Ok(1024_u64.pow(2)),
        "mb" => Ok(1000_u64.pow(2)),
        "g" | "gi" | "gib" => Ok(1024_u64.pow(3)),
        "gb" => Ok(1000_u64.pow(3)),
        _ => Err(ThresholdParseError::UnknownUnit {
            unit: unit.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        compute_tree_rss, is_descendant_of, parse_threshold, MemoryMonitor, PressureReason,
        ThresholdParseError, ThresholdSpec,
    };
    use sysinfo::Pid;

    #[test]
    fn parses_percent_threshold() {
        assert_eq!(parse_threshold("50%"), Ok(ThresholdSpec::Percent(50.0)));
        assert_eq!(parse_threshold("12.5 %"), Ok(ThresholdSpec::Percent(12.5)));
    }

    #[test]
    fn parses_binary_gibibytes() {
        assert_eq!(
            parse_threshold("4GiB"),
            Ok(ThresholdSpec::Absolute(4 * 1024 * 1024 * 1024))
        );
    }

    #[test]
    fn parses_binary_mebibytes() {
        assert_eq!(
            parse_threshold("512MiB"),
            Ok(ThresholdSpec::Absolute(512 * 1024 * 1024))
        );
    }

    #[test]
    fn parses_decimal_gigabytes() {
        assert_eq!(
            parse_threshold("2GB"),
            Ok(ThresholdSpec::Absolute(2 * 1000 * 1000 * 1000))
        );
    }

    #[test]
    fn parses_bare_integer_as_bytes() {
        assert_eq!(
            parse_threshold("1048576"),
            Ok(ThresholdSpec::Absolute(1_048_576))
        );
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(parse_threshold("   "), Err(ThresholdParseError::Empty));
    }

    #[test]
    fn rejects_bad_unit() {
        assert_eq!(
            parse_threshold("5XB"),
            Err(ThresholdParseError::UnknownUnit {
                unit: "XB".to_string()
            })
        );
    }

    #[test]
    fn rejects_overflow() {
        assert_eq!(
            parse_threshold("18446744073709551615GiB"),
            Err(ThresholdParseError::Overflow)
        );
    }

    #[test]
    fn rejects_garbage_and_negative_values() {
        assert_eq!(
            parse_threshold("%"),
            Err(ThresholdParseError::InvalidNumber)
        );
        assert_eq!(
            parse_threshold("-1GB"),
            Err(ThresholdParseError::InvalidNumber)
        );
        assert_eq!(
            parse_threshold("12MBps"),
            Err(ThresholdParseError::UnknownUnit {
                unit: "MBps".to_string()
            })
        );
        assert_eq!(
            parse_threshold("1.5GiB"),
            Err(ThresholdParseError::InvalidNumber)
        );
    }

    #[test]
    fn resolves_thresholds() {
        assert_eq!(ThresholdSpec::Percent(50.0).resolve(1_000), 500);
        assert_eq!(ThresholdSpec::Percent(12.5).resolve(1_000), 125);
        assert_eq!(ThresholdSpec::Percent(500.0).resolve(10), 50);
        assert_eq!(ThresholdSpec::Absolute(123).resolve(1_000), 123);
    }

    #[test]
    fn monitor_reports_sane_values_for_running_process() {
        let mut monitor = MemoryMonitor::with_specs_for_current_process(None, None);
        let pressure = monitor.check();

        assert!(pressure.sample.tree_rss > 0);
        assert!(pressure.sample.system_available > 0);
    }

    #[test]
    fn monitor_reuses_cached_sample_within_ttl() {
        let mut monitor = MemoryMonitor::for_current_process(u64::MAX, 0);
        monitor.set_ttl(Duration::from_secs(60));

        let first = monitor.check_at(Instant::now());
        let second = monitor.check_at(Instant::now() + Duration::from_millis(1));

        assert_eq!(first.sample, second.sample);
        assert_eq!(monitor.recompute_count(), 1);
    }

    #[test]
    fn monitor_recomputes_after_ttl_expires() {
        let mut monitor = MemoryMonitor::for_current_process(u64::MAX, 0);
        monitor.set_ttl(Duration::from_millis(1));

        let start = Instant::now();
        let _ = monitor.check_at(start);
        let _ = monitor.check_at(start + Duration::from_millis(2));

        assert_eq!(monitor.recompute_count(), 2);
    }

    #[test]
    fn missing_root_pid_degrades_gracefully() {
        let mut monitor = MemoryMonitor::new(u64::MAX, 0, Pid::from_u32(u32::MAX));
        let pressure = monitor.check();

        assert_eq!(pressure.sample.tree_rss, 0);
        assert!(!pressure.paused);
        assert!(pressure.reasons.is_empty());
    }

    #[test]
    fn tiny_thresholds_force_pause_for_tests() {
        let mut monitor = MemoryMonitor::for_current_process(1, u64::MAX);
        let pressure = monitor.check();

        assert!(pressure.paused);
        assert!(pressure.reasons.contains(&PressureReason::UsageHigh));
        assert!(pressure.reasons.contains(&PressureReason::FreeLow));
    }

    #[test]
    fn missing_process_memory_counts_as_zero() {
        let sys = sysinfo::System::new();
        assert_eq!(
            compute_tree_rss(sys.processes(), Pid::from_u32(u32::MAX)),
            None
        );
    }

    #[test]
    fn parent_chain_walk_handles_missing_parents_without_looping() {
        let sys = sysinfo::System::new();
        assert!(!is_descendant_of(
            sys.processes(),
            Pid::from_u32(1),
            Pid::from_u32(2),
            1,
        ));
    }
}
