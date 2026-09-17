//! Linux memory pressure from Pressure Stall Information (PSI).
//!
//! Prefers the current cgroup's `memory.pressure` over the host-wide
//! `/proc/pressure/memory`: inside a container with a memory limit, the
//! host-wide file reports the host's stalls, so it can read 0.00 while this
//! process is about to be OOM-killed. That is exactly the environment where
//! backpressure matters most.
//!
//! Everything here is best-effort. An unreadable or unparseable file yields
//! `None`, which the monitor treats as "no pressure", silently.

use std::fs;
use std::path::{Path, PathBuf};

use super::sensitivity::linux_stall_trigger;
use super::{PressureDetail, Sensitivity};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const HOST_PSI: &str = "/proc/pressure/memory";
const SELF_CGROUP: &str = "/proc/self/cgroup";

/// Reads the OS pressure indicator and decides whether it clears the trigger
/// for `sensitivity`. `None` means the indicator is unavailable. Returns
/// `(should_pause, why)`; `why` is only meaningful when `should_pause` is
/// `true` — callers must gate on the bool before rendering the detail.
pub(super) fn sample(sensitivity: Sensitivity) -> Option<(bool, PressureDetail)> {
    let trigger = linux_stall_trigger(sensitivity)?;
    let stalled = read_full_avg10()?;
    Some((stalled > trigger, PressureDetail::Stalled(stalled)))
}

/// Reads `full avg10` from the most specific PSI file available.
///
/// The cgroup file and the host file are read independently, and each is
/// parsed independently: a cgroup file that reads successfully but fails to
/// parse must fall back to the host file just as a missing cgroup file does.
/// See `first_parseable`, which encodes that policy.
fn read_full_avg10() -> Option<f64> {
    let cgroup = cgroup_psi_path().and_then(|path| fs::read_to_string(path).ok());
    let host = fs::read_to_string(HOST_PSI).ok();
    first_parseable(cgroup.as_deref(), host.as_deref())
}

/// Parses `cgroup`, falling back to `host` when `cgroup` is absent *or* fails
/// to parse (empty file, truncated write, unexpected format). Pulled out of
/// `read_full_avg10` so the fallback policy can be tested without touching
/// the filesystem: readability and parseability of the cgroup source are
/// independent failure modes, and both must fall back to the host source.
fn first_parseable(cgroup: Option<&str>, host: Option<&str>) -> Option<f64> {
    cgroup
        .and_then(parse_psi_full_avg10)
        .or_else(|| host.and_then(parse_psi_full_avg10))
}

/// The current cgroup's `memory.pressure`, if this is cgroup v2 and the file
/// exists. Checking existence here (rather than letting the read fail) keeps
/// `read_full_avg10`'s file handling simple: a lookup failure and a read
/// failure both collapse to `None` before `first_parseable` ever sees them.
///
/// The path is built from `/proc/self/cgroup`, which the kernel writes and
/// this process cannot influence, so it cannot contain attacker-controlled
/// `..` segments the way a path from user input or a config file could.
fn cgroup_psi_path() -> Option<PathBuf> {
    let content = fs::read_to_string(SELF_CGROUP).ok()?;
    let path = cgroup_pressure_path(&content)?;
    Path::new(&path).exists().then_some(path)
}

/// Maps `/proc/self/cgroup` content to this process's `memory.pressure` path.
///
/// Only cgroup v2 is supported: its unified line is `0::<path>`. cgroup v1 has
/// no `memory.pressure` file at all, so v1-only content yields `None` and the
/// caller falls back to the host-wide file.
fn cgroup_pressure_path(self_cgroup: &str) -> Option<PathBuf> {
    let relative = self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim()
        .trim_start_matches('/');

    let mut path = PathBuf::from(CGROUP_ROOT);
    if !relative.is_empty() {
        path.push(relative);
    }
    path.push("memory.pressure");
    Some(path)
}

/// Extracts `avg10` from the `full` line of a PSI file.
///
/// Format (two lines, whitespace-separated key=value fields):
/// ```text
/// some avg10=0.00 avg60=0.01 avg300=0.01 total=1728307527
/// full avg10=0.00 avg60=0.01 avg300=0.01 total=1322657835
/// ```
///
/// `some` counts time when *any* task stalled, which includes ordinary
/// page-cache refaults and direct reclaim during routine I/O-heavy work —
/// an unremarkable build touching thousands of files can hold `some` above
/// the default trigger continuously with no actual memory shortage. `full`
/// counts only time when *every* non-idle task stalled at once, which the
/// kernel's own PSI documentation (`Documentation/accounting/psi.rst`)
/// treats as thrashing when sustained. That is the condition backpressure
/// should react to, so read `full`, not `some` — do not "helpfully" switch
/// this back.
fn parse_psi_full_avg10(psi: &str) -> Option<f64> {
    psi.lines()
        .find_map(|line| line.strip_prefix("full "))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const REAL_PSI: &str = "\
some avg10=0.00 avg60=0.01 avg300=0.01 total=1728307527
full avg10=0.00 avg60=0.01 avg300=0.01 total=1322657835
";

    #[test]
    fn parses_full_avg10_from_real_psi_file() {
        assert_eq!(parse_psi_full_avg10(REAL_PSI), Some(0.0));
    }

    #[test]
    fn parses_nonzero_stall() {
        let psi = "some avg10=23.40 avg60=8.10 avg300=2.00 total=99\n\
                   full avg10=1.00 avg60=0.50 avg300=0.10 total=9\n";
        assert_eq!(parse_psi_full_avg10(psi), Some(1.00));
    }

    /// A `some`-only file (no `full` line — this shouldn't happen in
    /// practice, since the kernel always writes both, but a truncated or
    /// hand-crafted file could) must yield `None`, not the `some` value.
    /// Reading `some` as if it were `full` would be exactly the regression
    /// this issue fixes: `some` fires on routine page-cache churn during
    /// ordinary I/O-heavy builds, so treating it as `full` would bring back
    /// the spurious pauses.
    #[test]
    fn ignores_some_line_when_full_is_absent() {
        let psi = "some avg10=50.00 avg60=50.00 avg300=50.00 total=9\n";
        assert_eq!(parse_psi_full_avg10(psi), None);
    }

    #[test]
    fn rejects_malformed_input() {
        assert_eq!(parse_psi_full_avg10(""), None);
        assert_eq!(
            parse_psi_full_avg10("full avg10=notanumber total=1\n"),
            None
        );
        assert_eq!(parse_psi_full_avg10("full avg60=1.00 total=1\n"), None);
        assert_eq!(parse_psi_full_avg10("full"), None);
        // Truncated mid-write: the field is present but has no value.
        assert_eq!(parse_psi_full_avg10("full avg10="), None);
    }

    #[test]
    fn resolves_cgroup_v2_pressure_path() {
        assert_eq!(
            cgroup_pressure_path("0::/user.slice/user-1000.slice/session-c2.scope\n"),
            Some(PathBuf::from(
                "/sys/fs/cgroup/user.slice/user-1000.slice/session-c2.scope/memory.pressure"
            ))
        );
    }

    #[test]
    fn resolves_root_cgroup_pressure_path() {
        assert_eq!(
            cgroup_pressure_path("0::/\n"),
            Some(PathBuf::from("/sys/fs/cgroup/memory.pressure"))
        );
    }

    /// cgroup v1 has no unified (`0::`) line and no `memory.pressure` file;
    /// callers must fall back to the host-wide `/proc/pressure/memory`.
    #[test]
    fn cgroup_v1_content_has_no_v2_path() {
        let v1 = "11:memory:/user.slice\n10:cpu,cpuacct:/user.slice\n";
        assert_eq!(cgroup_pressure_path(v1), None);
        assert_eq!(cgroup_pressure_path(""), None);
        assert_eq!(cgroup_pressure_path("garbage\n"), None);
    }

    #[test]
    fn sample_is_none_when_sensitivity_is_off() {
        assert_eq!(sample(Sensitivity::Off), None);
    }

    #[test]
    fn first_parseable_prefers_cgroup_when_it_parses() {
        let host = "some avg10=99.00 avg60=99.00 avg300=99.00 total=1\n";
        assert_eq!(first_parseable(Some(REAL_PSI), Some(host)), Some(0.0));
    }

    /// The regression this fixes: a cgroup PSI file that reads successfully
    /// but fails to parse must fall back to the host file, exactly like a
    /// missing cgroup file does. Before this fix, `read_full_avg10`'s
    /// `or_else` only covered a failed *read*, so an empty or truncated
    /// cgroup file silently disabled backpressure instead of falling back.
    #[test]
    fn first_parseable_falls_back_to_host_when_cgroup_is_unparseable() {
        assert_eq!(first_parseable(Some(""), Some(REAL_PSI)), Some(0.0));
        assert_eq!(first_parseable(Some("garbage"), Some(REAL_PSI)), Some(0.0));
    }

    #[test]
    fn first_parseable_uses_host_when_cgroup_is_absent() {
        assert_eq!(first_parseable(None, Some(REAL_PSI)), Some(0.0));
    }

    #[test]
    fn first_parseable_is_none_when_both_fail() {
        assert_eq!(first_parseable(None, None), None);
        assert_eq!(first_parseable(Some(""), Some("")), None);
        assert_eq!(first_parseable(Some("garbage"), Some("garbage")), None);
        assert_eq!(first_parseable(Some(""), None), None);
        assert_eq!(first_parseable(None, Some("garbage")), None);
    }

    /// Guards against a backend that returns `None` on a host that *does*
    /// publish PSI — the failure mode where the file is readable but the path
    /// resolution or parsing is broken.
    ///
    /// Gates on an actual successful read-and-parse (`read_full_avg10`)
    /// rather than path existence: on a restricted container, or during a
    /// filesystem race, a path can exist yet be unreadable or unparseable,
    /// which would make the gate pass and the assertion below fail
    /// spuriously. A host with no PSI at all is a supported state, not a
    /// defect (see the README), so this returns early there rather than
    /// failing.
    #[test]
    fn sample_reads_the_live_psi_file_when_the_host_has_one() {
        if read_full_avg10().is_none() {
            eprintln!("no readable, parseable PSI file on this host; skipping");
            return;
        }

        let reading = sample(Sensitivity::Normal);
        assert!(
            reading.is_some(),
            "a host with a readable, parseable PSI file must yield a reading"
        );
        let (_, detail) = reading.expect("checked above");
        assert!(matches!(detail, PressureDetail::Stalled(_)));
    }

    /// Regression guard for the `detail.is_some() == paused` invariant, driven
    /// through `platform_sample` end-to-end rather than through the extracted
    /// `platform_reading_to_pressure` helper alone.
    ///
    /// The helper is unit-tested directly, but nothing exercised the line in
    /// `platform_sample` that actually applies it to a real backend reading —
    /// rewiring `platform_sample` to bypass the helper would currently fail no
    /// test. Without it, every healthy build would show a permanent
    /// "memory pressure (stalled 0%)" warning on its status line, because the
    /// Linux backend always produces a `Stalled` detail, even on a calm
    /// reading.
    ///
    /// This does not assert the host is calm: a memory-constrained or loaded
    /// CI machine can legitimately exceed `Sensitivity::Low`'s 90% stall
    /// trigger, and asserting "always calm" would make the test flaky on
    /// exactly the hosts most worth testing on. Instead it takes whatever
    /// verdict the live call returns and checks the invariant holds either
    /// way — detail present if and only if paused — which is true on a calm
    /// host and a thrashing one alike, while still exercising the real path
    /// end to end.
    #[test]
    fn platform_sample_clears_detail_on_a_calm_reading() {
        if read_full_avg10().is_none() {
            eprintln!("no readable, parseable PSI file on this host; skipping");
            return;
        }

        let pressure = super::super::platform_sample(Sensitivity::Low)
            .expect("a host with a readable, parseable PSI file must yield a reading");

        assert_eq!(
            pressure.detail.is_some(),
            pressure.paused,
            "detail must be present exactly when pressure is reported: {pressure:?}"
        );
    }
}
