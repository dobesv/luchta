//! macOS memory pressure from `kern.memorystatus_vm_pressure_level`.
//!
//! This is the same signal libdispatch's `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE`
//! publishes, read directly so luchta needs no dispatch-queue plumbing.
//!
//! The sysctl is deliberately the ONLY thing in this file. CI never executes
//! macOS tests, so every decision about what a level means lives in
//! `sensitivity.rs`, which is compiled and tested on Linux.

use super::sensitivity::macos_level_trigger;
use super::{PressureDetail, Sensitivity};

/// `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warning, 4 critical.
const PRESSURE_LEVEL_SYSCTL: &[u8] = b"kern.memorystatus_vm_pressure_level\0";
const LEVEL_CRITICAL: i32 = 4;

/// Returns `(should_pause, why)`. `why` is only meaningful when
/// `should_pause` is `true` — there is no "normal" `PressureDetail` variant,
/// so a healthy level (1) still produces `Warning` here as an ignored
/// placeholder value, not a claim that the machine is actually at the
/// warning level. Callers must gate on the bool before rendering the detail.
pub(super) fn sample(sensitivity: Sensitivity) -> Option<(bool, PressureDetail)> {
    let trigger = macos_level_trigger(sensitivity)?;
    let level = read_pressure_level()?;
    let detail = if level >= LEVEL_CRITICAL {
        PressureDetail::Critical
    } else {
        PressureDetail::Warning
    };
    Some((level >= trigger, detail))
}

/// Returns the current pressure level, or `None` if the sysctl is unavailable.
fn read_pressure_level() -> Option<i32> {
    let mut level: i32 = 0;
    let mut size = std::mem::size_of::<i32>();

    // SAFETY: `PRESSURE_LEVEL_SYSCTL` is NUL-terminated; `level` and `size` are
    // live locals of exactly the type and size the sysctl writes, and `size`
    // is pre-set to the buffer's size before the call, which is what tells
    // the sysctl how much space it may write into `level`.
    let rc = unsafe {
        libc::sysctlbyname(
            PRESSURE_LEVEL_SYSCTL.as_ptr().cast::<libc::c_char>(),
            std::ptr::addr_of_mut!(level).cast::<libc::c_void>(),
            std::ptr::addr_of_mut!(size),
            std::ptr::null_mut(),
            0,
        )
    };

    (rc == 0).then_some(level)
}
