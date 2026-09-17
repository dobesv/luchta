//! Pauses new task dispatch while the operating system reports memory
//! pressure.
//!
//! luchta asks the OS rather than computing a proxy from its own resident-set
//! size: a build using 60% of RAM on an idle machine is fine, while one using
//! 20% on a machine that is already swapping is not. Each platform publishes
//! the answer directly — Linux via PSI, macOS via
//! `kern.memorystatus_vm_pressure_level`, Windows via the low-memory resource
//! notification.
//!
//! When the indicator cannot be read, luchta dispatches normally and says
//! nothing.

use std::time::{Duration, Instant};

mod sensitivity;

pub use sensitivity::Sensitivity;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Why the OS says memory is under pressure. Rendered into the status line so
/// the user can judge severity rather than just seeing that luchta stopped.
///
/// A value is only a claim about the machine when it reaches here attached to a
/// paused verdict — see [`MemoryPressure`]'s invariant. The backends construct
/// one from every reading, including calm ones, and
/// [`platform_reading_to_pressure`] drops it unless the reading says to pause.
///
/// `Display` is the single source of the status-line wording, which keeps that
/// text testable on any platform — including the macOS and Windows variants,
/// whose backends CI never executes.
///
/// `dead_code` is allowed because only one backend is compiled in at a time, so
/// the other platforms' variants are constructed nowhere in a given build. The
/// `Display` tests construct all of them on every platform, deliberately.
#[allow(dead_code)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) enum PressureDetail {
    /// Linux PSI `full avg10`: percent of the last 10s during which every
    /// non-idle task stalled on memory reclaim at once (thrashing).
    Stalled(f64),
    /// macOS `kern.memorystatus_vm_pressure_level` below critical. Also the
    /// placeholder a calm macOS reading carries, which is why it means nothing
    /// unless the verdict is paused.
    Warning,
    /// macOS `kern.memorystatus_vm_pressure_level` at critical (4).
    Critical,
    /// The Windows low-memory resource notification.
    LowMemory,
}

impl std::fmt::Display for PressureDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stalled(percent) => write!(f, "stalled {percent:.0}%"),
            Self::Warning => f.write_str("warning"),
            Self::Critical => f.write_str("critical"),
            Self::LowMemory => f.write_str("low memory"),
        }
    }
}

/// The current verdict: whether to hold new dispatch, and why.
///
/// Invariant: `detail.is_some() == paused`. The status line renders a warning
/// suffix from `detail` alone, so a calm verdict must carry `None` or every
/// healthy build would display a permanent memory-pressure warning.
///
/// Not `Eq` — `PressureDetail::Stalled` carries an `f64`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MemoryPressure {
    pub(crate) paused: bool,
    pub(crate) detail: Option<PressureDetail>,
}

impl MemoryPressure {
    /// The verdict when there is nothing to report: the OS is calm, the
    /// indicator is unreadable, or the user turned backpressure off. All three
    /// dispatch identically and show nothing in the status line.
    fn clear() -> Self {
        Self {
            paused: false,
            detail: None,
        }
    }
}

/// Reads the platform indicator and applies `sensitivity`. `None` means the
/// indicator is unavailable.
type SampleFn = fn(Sensitivity) -> Option<MemoryPressure>;

/// The platform backend, or a stub on platforms with no indicator.
fn platform_sample(sensitivity: Sensitivity) -> Option<MemoryPressure> {
    #[cfg(target_os = "linux")]
    let reading = linux::sample(sensitivity);
    #[cfg(target_os = "macos")]
    let reading = macos::sample(sensitivity);
    #[cfg(target_os = "windows")]
    let reading = windows::sample(sensitivity);
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let reading = {
        let _ = sensitivity;
        None::<(bool, PressureDetail)>
    };

    reading.map(|(paused, detail)| platform_reading_to_pressure(paused, detail))
}

/// Applies the `detail.is_some() == paused` invariant to a backend's reading.
///
/// A backend always produces a detail, even when it reports no pressure — it
/// has just read a number and has something to say about it. But the status
/// line renders its warning suffix from `detail` alone, so a calm reading must
/// drop it. Without this, every healthy build would show a permanent
/// "memory pressure (stalled 0%)" warning.
///
/// Extracted from `platform_sample` so the invariant is testable on every
/// platform, not only the one whose backend is compiled in.
fn platform_reading_to_pressure(paused: bool, detail: PressureDetail) -> MemoryPressure {
    MemoryPressure {
        paused,
        detail: paused.then_some(detail),
    }
}

pub(crate) struct MemoryMonitor {
    sensitivity: Sensitivity,
    sample_fn: SampleFn,
    cache: Option<(Instant, MemoryPressure)>,
    ttl: Duration,
    recompute_count: u64,
}

impl MemoryMonitor {
    /// How long a reading is reused. Matches the pause loop's re-check cadence,
    /// so a paused loop samples the OS once per tick rather than once per task.
    const DEFAULT_TTL: Duration = Duration::from_millis(250);

    pub(crate) fn new(sensitivity: Sensitivity) -> Self {
        Self::with_sample_fn(sensitivity, platform_sample)
    }

    /// Test seam: substitutes the platform backend. A plain function pointer
    /// rather than a `#[cfg(test)]` field, so the production struct carries no
    /// test-only state.
    pub(crate) fn with_sample_fn(sensitivity: Sensitivity, sample_fn: SampleFn) -> Self {
        Self {
            sensitivity,
            sample_fn,
            cache: None,
            ttl: Self::DEFAULT_TTL,
            recompute_count: 0,
        }
    }

    pub(crate) fn check(&mut self) -> MemoryPressure {
        self.check_at(Instant::now())
    }

    fn check_at(&mut self, now: Instant) -> MemoryPressure {
        if self.sensitivity == Sensitivity::Off {
            return MemoryPressure::clear();
        }

        if let Some((sampled_at, ref pressure)) = self.cache {
            if now.duration_since(sampled_at) < self.ttl {
                return pressure.clone();
            }
        }

        self.recompute_count += 1;
        let pressure = (self.sample_fn)(self.sensitivity).unwrap_or_else(MemoryPressure::clear);
        self.cache = Some((now, pressure.clone()));
        pressure
    }

    #[cfg(test)]
    fn set_ttl(&mut self, ttl: Duration) {
        self.ttl = ttl;
    }

    #[cfg(test)]
    fn recompute_count(&self) -> u64 {
        self.recompute_count
    }
}

/// The latest verdict, shared with the progress renderer so the status line can
/// say why dispatch is held.
#[derive(Debug)]
pub(crate) struct PressureState {
    latest: std::sync::RwLock<MemoryPressure>,
}

impl PressureState {
    pub(crate) fn new() -> Self {
        Self {
            latest: std::sync::RwLock::new(MemoryPressure::clear()),
        }
    }

    pub(crate) fn update(&self, pressure: &MemoryPressure) {
        *self.latest.write().expect("pressure state lock poisoned") = pressure.clone();
    }

    pub(crate) fn snapshot(&self) -> MemoryPressure {
        self.latest
            .read()
            .expect("pressure state lock poisoned")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use luchta_test_support::require_nextest;

    use super::{
        platform_reading_to_pressure, MemoryMonitor, MemoryPressure, PressureDetail, PressureState,
        Sensitivity,
    };

    static SAMPLE_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn counting_paused_sample(_: Sensitivity) -> Option<MemoryPressure> {
        SAMPLE_CALLS.fetch_add(1, Ordering::SeqCst);
        Some(MemoryPressure {
            paused: true,
            detail: Some(PressureDetail::Stalled(42.0)),
        })
    }

    fn unavailable_sample(_: Sensitivity) -> Option<MemoryPressure> {
        None
    }

    #[test]
    fn pressure_detail_renders_for_status_line() {
        assert_eq!(PressureDetail::Stalled(23.4).to_string(), "stalled 23%");
        assert_eq!(PressureDetail::Stalled(5.0).to_string(), "stalled 5%");
        // Rounds rather than truncates, so 9.6% does not read as a calmer 9%.
        assert_eq!(PressureDetail::Stalled(9.6).to_string(), "stalled 10%");
        assert_eq!(PressureDetail::Warning.to_string(), "warning");
        assert_eq!(PressureDetail::Critical.to_string(), "critical");
        assert_eq!(PressureDetail::LowMemory.to_string(), "low memory");
    }

    /// `off` must not even consult the OS — that is what makes it a usable
    /// escape hatch on a machine whose pressure signal is broken.
    #[test]
    fn off_sensitivity_never_samples() {
        require_nextest();
        SAMPLE_CALLS.store(0, Ordering::SeqCst);
        let mut monitor = MemoryMonitor::with_sample_fn(Sensitivity::Off, counting_paused_sample);

        let pressure = monitor.check();

        assert!(!pressure.paused);
        assert_eq!(pressure.detail, None);
        assert_eq!(SAMPLE_CALLS.load(Ordering::SeqCst), 0);
    }

    /// An unreadable indicator means "no pressure", silently — no warning, no
    /// fallback heuristic.
    #[test]
    fn unavailable_indicator_never_pauses() {
        let mut monitor = MemoryMonitor::with_sample_fn(Sensitivity::Normal, unavailable_sample);

        let pressure = monitor.check();

        assert!(!pressure.paused);
        assert_eq!(pressure.detail, None);
    }

    /// Runs two `check_at` calls `second_offset` apart against a freshly
    /// constructed monitor with the given TTL, and returns both verdicts
    /// alongside how many times the sample function actually ran. Shared by
    /// the TTL-reuse and TTL-expiry tests below, which differ only in the
    /// TTL/offset relationship and in which of these three results they care
    /// about.
    fn check_twice_with_ttl(
        ttl: Duration,
        second_offset: Duration,
    ) -> (MemoryPressure, MemoryPressure, u64) {
        require_nextest();
        let mut monitor =
            MemoryMonitor::with_sample_fn(Sensitivity::Normal, counting_paused_sample);
        monitor.set_ttl(ttl);

        let start = Instant::now();
        let first = monitor.check_at(start);
        let second = monitor.check_at(start + second_offset);

        (first, second, monitor.recompute_count())
    }

    #[test]
    fn monitor_reuses_cached_sample_within_ttl() {
        let (first, second, recomputes) =
            check_twice_with_ttl(Duration::from_secs(60), Duration::from_millis(1));

        assert_eq!(first, second);
        assert_eq!(recomputes, 1);
    }

    #[test]
    fn monitor_recomputes_after_ttl_expires() {
        let (_, _, recomputes) =
            check_twice_with_ttl(Duration::from_millis(1), Duration::from_millis(2));

        assert_eq!(recomputes, 2);
    }

    /// Regression guard for the invariant `detail.is_some() == paused`.
    ///
    /// The status line renders its warning suffix from `detail` alone. If a
    /// calm reading kept its detail, every healthy build would show a
    /// permanent "memory pressure (stalled 0%)" warning — the backends always
    /// produce a detail, even when they report no pressure.
    #[test]
    fn a_calm_reading_carries_no_detail() {
        fn calm_but_detailed(_: Sensitivity) -> Option<MemoryPressure> {
            // Mimics what `platform_sample` receives from a backend on an idle
            // machine: a real reading that does not clear the trigger.
            Some(MemoryPressure {
                paused: false,
                detail: Some(PressureDetail::Stalled(0.0)),
            })
        }

        let calm = platform_reading_to_pressure(false, PressureDetail::Stalled(0.0));
        assert!(!calm.paused);
        assert_eq!(calm.detail, None, "a calm reading must not carry a detail");

        let pressured = platform_reading_to_pressure(true, PressureDetail::Critical);
        assert!(pressured.paused);
        assert_eq!(pressured.detail, Some(PressureDetail::Critical));

        // The monitor passes a backend verdict through unchanged; the mapping
        // above is what the real backends go through.
        let mut monitor = MemoryMonitor::with_sample_fn(Sensitivity::Normal, calm_but_detailed);
        assert!(!monitor.check().paused);
    }

    #[test]
    fn real_monitor_reports_a_verdict_without_panicking() {
        let mut monitor = MemoryMonitor::new(Sensitivity::Normal);
        let _ = monitor.check();
    }

    #[test]
    fn pressure_state_round_trips_the_latest_verdict() {
        let state = PressureState::new();
        assert!(!state.snapshot().paused);

        state.update(&MemoryPressure {
            paused: true,
            detail: Some(PressureDetail::Critical),
        });

        let snapshot = state.snapshot();
        assert!(snapshot.paused);
        assert_eq!(snapshot.detail, Some(PressureDetail::Critical));
    }
}
