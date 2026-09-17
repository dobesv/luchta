//! Parses the per-platform tuning env vars for OS-reported memory pressure.
//!
//! The parsers live here — compiled on every platform — rather than in the
//! cfg-gated backends, because CI runs tests on Linux only. Keeping the
//! decisions here means they are covered everywhere; the backends hold nothing
//! but the syscall (or file read) that fetches a number.
//!
//! That arrangement is why `psi_threshold` and `macos_min_level` each carry
//! `#[allow(dead_code)]`: one is consumed by the Linux backend, the other by
//! the macOS backend, so on the other two platforms it really is dead, and
//! `-D warnings` would fail the build there. **Do not remove those
//! attributes** — they look unnecessary on Linux, where both are reachable
//! from the tests below, but deleting them turns the macOS or Windows build
//! red. `cfg`-gating the functions instead would make the other platform's
//! tests uncompilable here and lose the coverage this split exists to
//! provide.
//!
//! `parse_psi_threshold` and `parse_macos_level` are not similarly gated: both
//! are also called directly from `main.rs`, on every platform, to reject an
//! unparseable env value at startup rather than silently defaulting it.
//!
//! The same reasoning does not hold inside the backend files themselves:
//! there, an item the compiler reports as dead genuinely is unreachable on
//! the platform that compiles it, and the warning is worth keeping.

/// Parses a raw `LUCHTA_MEM_PSI_THRESHOLD` value (already trimmed non-empty).
/// `None` means it does not parse as a finite, nonnegative percentage.
///
/// Exposed so `main.rs` can reject an unparseable value at startup instead of
/// silently falling back to the default, which would leave a user believing
/// they had tuned something.
pub(crate) fn parse_psi_threshold(raw: &str) -> Option<f64> {
    let value: f64 = raw.trim().parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

/// Percent of `full avg10` (Linux PSI) above which dispatch pauses. Default
/// 60.0 when `raw` is `None`, blank, or fails to parse — the last case cannot
/// happen in practice since `main.rs` rejects an unparseable
/// `LUCHTA_MEM_PSI_THRESHOLD` before this ever runs.
///
/// `avg10` is the share of the last 10 seconds during which every non-idle
/// task stalled waiting on memory reclaim at the same time — the kernel's own
/// definition of thrashing.
///
/// This gate is a last resort against thrashing, not an early-warning memory
/// governor: luchta would rather run a machine into occasional, recoverable
/// memory exhaustion than pay the immediate, visible cost of pausing dispatch
/// on every build. A paused build stalls right now, on every run that meets
/// the trigger; running low on memory is occasional and usually survivable.
/// So the default is set high, deliberately.
///
/// Evidence behind the number: PSI captured every 5s through an ordinary
/// build that completed in 28s with no pause peaked at `full avg10 = 6.1%`
/// (`some avg10` peaked at 7.4% over the same window — `full` tracks `some`
/// at roughly 0.8x on this workload, not an order of magnitude lower, so
/// switching metrics alone would not have cleared the noise floor). A build
/// that previously triggered a spurious pause read `some avg10 = 13%`, which
/// corresponds to `full avg10 ≈ 11%`. A threshold near either of those
/// figures fires on healthy work.
///
/// The default of 60% is deliberately high-set: roughly 6 of every 10 seconds
/// with every non-idle task blocked on memory, which is already a badly
/// degraded machine — far above the noise floor measured above.
///
/// These numbers come from measurements on one machine (61 GB RAM, heavily
/// swapped) plus an explicit preference for throughput over caution — a
/// considered starting point, not a derived constant. Retuning it needs
/// equivalent evidence — PSI samples through both a healthy build and a
/// stalling one — not just intuition about what "feels" low.
#[allow(dead_code)]
pub(crate) fn psi_threshold(raw: Option<&str>) -> f64 {
    const DEFAULT: f64 = 60.0;
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(parse_psi_threshold)
        .unwrap_or(DEFAULT)
}

/// Parses a raw `LUCHTA_MEM_MACOS_LEVEL` value (already trimmed non-empty),
/// case-insensitively, to the `kern.memorystatus_vm_pressure_level` it names:
/// `warning` -> 2, `critical` -> 4. `None` means it is neither.
///
/// Exposed so `main.rs` can reject an unparseable value at startup instead of
/// silently falling back to the default.
pub(crate) fn parse_macos_level(raw: &str) -> Option<i32> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "critical" => Some(4),
        "warning" => Some(2),
        _ => None,
    }
}

/// Minimum `kern.memorystatus_vm_pressure_level` (macOS) at which dispatch
/// pauses: 1 normal, 2 warning, 4 critical. Default 4 (critical) when `raw`
/// is `None`, blank, or fails to parse — the last case cannot happen in
/// practice since `main.rs` rejects an unparseable `LUCHTA_MEM_MACOS_LEVEL`
/// before this ever runs.
///
/// Critical is the default because macOS raises *warning* routinely as an
/// advisory to release cached memory, not as a sign the machine is actually
/// struggling — pausing dispatch on it stalls builds on an otherwise healthy
/// Mac (issue #347). Only critical is worth pausing on by default; a user who
/// wants the more sensitive behavior can opt in with `LUCHTA_MEM_MACOS_LEVEL=warning`.
#[allow(dead_code)]
pub(crate) fn macos_min_level(raw: Option<&str>) -> i32 {
    const DEFAULT: i32 = 4;
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(parse_macos_level)
        .unwrap_or(DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a `(label, input, expected)` table through `f`, naming the failing
    /// case in the assertion message rather than relying on a long run of bare
    /// `assert_eq!` calls to place the blame.
    fn check_cases<I: Copy, O: Copy + PartialEq + std::fmt::Debug>(
        cases: &[(&str, I, O)],
        f: impl Fn(I) -> O,
    ) {
        for &(label, input, expected) in cases {
            assert_eq!(f(input), expected, "case: {label}");
        }
    }

    /// Covers unset, blank, valid (plain, fractional, whitespace-padded), and
    /// invalid (non-numeric, unit suffix, negative, NaN) input — everything
    /// that can reach the defaulting wrapper.
    #[test]
    fn psi_threshold_cases() {
        check_cases(
            &[
                ("unset", None, 60.0),
                ("blank", Some(""), 60.0),
                ("whitespace only", Some("   "), 60.0),
                ("valid integer", Some("30"), 30.0),
                ("valid fractional", Some("30.5"), 30.5),
                ("valid, whitespace-padded", Some("  45  "), 45.0),
                ("non-numeric", Some("nope"), 60.0),
                ("unit suffix", Some("60%"), 60.0),
                ("negative", Some("-5"), 60.0),
                ("NaN literal", Some("NaN"), 60.0),
            ],
            psi_threshold,
        );
    }

    /// Covers the same shape as `psi_threshold_cases` but for the checked
    /// parser main.rs uses to validate the env var at startup, plus the
    /// explicit out-of-range (above 100%) case that has no upper bound.
    #[test]
    fn parse_psi_threshold_cases() {
        check_cases(
            &[
                ("blank", "", None),
                ("zero", "0", Some(0.0)),
                ("typical", "60", Some(60.0)),
                ("fractional", "100.25", Some(100.25)),
                // No upper bound is enforced: a threshold above 100 just never
                // fires, which is a valid (if unusual) way to say "never pause".
                ("out of range (above 100)", "150", Some(150.0)),
                ("non-numeric", "nope", None),
                ("unit suffix", "60%", None),
                ("negative", "-1", None),
                ("NaN literal", "NaN", None),
                ("infinite", "inf", None),
            ],
            parse_psi_threshold,
        );
    }

    /// Covers unset, blank, both known levels in mixed case, whitespace
    /// padding, and invalid (unknown word, numeric) input.
    #[test]
    fn macos_min_level_cases() {
        check_cases(
            &[
                ("unset", None, 4),
                ("blank", Some(""), 4),
                ("whitespace only", Some("   "), 4),
                ("critical lowercase", Some("critical"), 4),
                ("critical uppercase", Some("CRITICAL"), 4),
                ("critical mixed case", Some("Critical"), 4),
                ("warning lowercase", Some("warning"), 2),
                ("warning uppercase", Some("WARNING"), 2),
                ("warning, whitespace-padded", Some("  warning  "), 2),
                ("unknown word", Some("nope"), 4),
                ("unsupported level name", Some("normal"), 4),
                ("numeric level", Some("2"), 4),
            ],
            macos_min_level,
        );
    }

    /// Covers the same shape as `macos_min_level_cases` but for the checked
    /// parser main.rs uses to validate the env var at startup.
    #[test]
    fn parse_macos_level_cases() {
        check_cases(
            &[
                ("blank", "", None),
                ("critical lowercase", "critical", Some(4)),
                ("critical uppercase", "CRITICAL", Some(4)),
                ("warning lowercase", "warning", Some(2)),
                ("warning mixed case", "WaRnInG", Some(2)),
                ("unsupported level name", "normal", None),
                ("unknown word", "nope", None),
                ("numeric level", "2", None),
            ],
            parse_macos_level,
        );
    }
}
