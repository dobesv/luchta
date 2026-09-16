//! How eagerly luchta pauses dispatch on OS-reported memory pressure, and what
//! that means on each platform.
//!
//! The trigger functions live here — compiled on every platform — rather than
//! in the cfg-gated backends, because CI runs tests on Linux only. Keeping the
//! decisions here means they are covered everywhere; the backends hold nothing
//! but the syscall that fetches a number.
//!
//! That arrangement is why each trigger function carries `#[allow(dead_code)]`:
//! every one is consumed by exactly one platform's backend, so on the other two
//! it really is dead, and `-D warnings` would fail the build there. **Do not
//! remove those attributes** — they look unnecessary on Linux, where all three
//! are reachable from the tests below, but deleting them turns the macOS or
//! Windows build red. `cfg`-gating the functions instead would make the other
//! platforms' tests uncompilable here and lose the coverage this split exists
//! to provide.
//!
//! The same does not hold inside the backend files themselves: there, an item
//! the compiler reports as dead genuinely is unreachable on the platform that
//! compiles it, and the warning is worth keeping.

/// How eagerly to pause new task dispatch when the OS reports memory pressure.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Sensitivity {
    /// Never pause. The escape hatch for a machine whose pressure signal
    /// misbehaves, or a build that must not stall behind an unrelated hog.
    Off,
    /// Pause only under severe pressure.
    Low,
    /// Pause under sustained pressure.
    #[default]
    Normal,
    /// Pause at the first sign of pressure.
    High,
}

/// Percent of `some avg10` (Linux PSI) above which dispatch pauses.
///
/// `avg10` is the share of the last 10 seconds during which at least one task
/// stalled waiting on memory reclaim. `None` means never pause.
#[allow(dead_code)]
pub(crate) fn linux_stall_trigger(sensitivity: Sensitivity) -> Option<f64> {
    match sensitivity {
        Sensitivity::Off => None,
        Sensitivity::Low => Some(20.0),
        Sensitivity::Normal => Some(10.0),
        Sensitivity::High => Some(5.0),
    }
}

/// Minimum `kern.memorystatus_vm_pressure_level` (macOS) at which dispatch
/// pauses: 1 normal, 2 warning, 4 critical. `None` means never pause.
///
/// Three levels cannot express four sensitivities, so `high` and `normal` both
/// land on warning.
#[allow(dead_code)]
pub(crate) fn macos_level_trigger(sensitivity: Sensitivity) -> Option<i32> {
    match sensitivity {
        Sensitivity::Off => None,
        Sensitivity::Low => Some(4),
        Sensitivity::Normal | Sensitivity::High => Some(2),
    }
}

/// Whether a signalled Windows low-memory notification pauses dispatch.
///
/// `LowMemoryResourceNotification` is a single bit with no severity, so every
/// sensitivity but `off` treats it the same way.
#[allow(dead_code)]
pub(crate) fn windows_pauses(sensitivity: Sensitivity) -> bool {
    !matches!(sensitivity, Sensitivity::Off)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sensitivity_is_normal() {
        assert_eq!(Sensitivity::default(), Sensitivity::Normal);
    }

    #[test]
    fn linux_triggers_tighten_as_sensitivity_rises() {
        assert_eq!(linux_stall_trigger(Sensitivity::Off), None);
        assert_eq!(linux_stall_trigger(Sensitivity::Low), Some(20.0));
        assert_eq!(linux_stall_trigger(Sensitivity::Normal), Some(10.0));
        assert_eq!(linux_stall_trigger(Sensitivity::High), Some(5.0));
    }

    /// macOS exposes only three levels (1 normal / 2 warning / 4 critical), so
    /// `high` and `normal` necessarily coincide. Pinned deliberately: this is a
    /// documented limitation of the signal, not a bug to "fix" by inventing a
    /// fourth distinction.
    #[test]
    fn macos_high_and_normal_both_trigger_at_warning() {
        assert_eq!(macos_level_trigger(Sensitivity::Off), None);
        assert_eq!(macos_level_trigger(Sensitivity::Low), Some(4));
        assert_eq!(macos_level_trigger(Sensitivity::Normal), Some(2));
        assert_eq!(macos_level_trigger(Sensitivity::High), Some(2));
    }

    /// `LowMemoryResourceNotification` is a single bit, so Windows cannot honor
    /// low/normal/high. Pinned deliberately — see the README note.
    #[test]
    fn windows_ignores_sensitivity_except_off() {
        assert!(!windows_pauses(Sensitivity::Off));
        assert!(windows_pauses(Sensitivity::Low));
        assert!(windows_pauses(Sensitivity::Normal));
        assert!(windows_pauses(Sensitivity::High));
    }
}
