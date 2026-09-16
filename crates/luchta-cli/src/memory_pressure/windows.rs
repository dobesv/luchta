//! Windows memory pressure from `LowMemoryResourceNotification`.
//!
//! The OS signals this object when available physical memory is low. It is a
//! single bit with no severity, which is why `sensitivity.rs` maps every level
//! but `off` to the same behavior here.
//!
//! The two API calls are deliberately the ONLY thing in this file; CI never
//! executes Windows tests. `QueryMemoryResourceNotification` is used instead
//! of `WaitForSingleObject` because it is the dedicated, documented way to
//! poll this handle and keeps this file to a single `windows-sys` module
//! (`Win32_System_Memory`) rather than pulling in `Win32_System_Threading`
//! as well.

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Memory::{
    CreateMemoryResourceNotification, LowMemoryResourceNotification,
    QueryMemoryResourceNotification,
};

use super::sensitivity::windows_pauses;
use super::{PressureDetail, Sensitivity};

/// Returns `(should_pause, why)`. `why` is only meaningful when
/// `should_pause` is `true`; callers must gate on the bool before rendering
/// the detail.
pub(super) fn sample(sensitivity: Sensitivity) -> Option<(bool, PressureDetail)> {
    if !windows_pauses(sensitivity) {
        return None;
    }
    let low = read_low_memory()?;
    Some((low, PressureDetail::LowMemory))
}

/// Returns whether the low-memory notification is currently signalled.
///
/// The handle is created and closed per sample rather than cached: sampling
/// happens at most every 250ms, and a per-call handle keeps this file free of
/// shared state that would need its own synchronization and tests we cannot
/// run here.
fn read_low_memory() -> Option<bool> {
    // SAFETY: the call takes an enum-like i32 by value and returns a handle
    // or a null pointer on failure; no pointers are passed in.
    let handle: HANDLE = unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) };
    if handle.is_null() {
        return None;
    }

    let mut low: BOOL = 0;
    // SAFETY: `handle` is a live notification object from the call above, and
    // `low` is a live local of the exact out-parameter type the call writes.
    let ok = unsafe { QueryMemoryResourceNotification(handle, &mut low) };
    // SAFETY: `handle` is live and is not used again after this call.
    unsafe { CloseHandle(handle) };

    (ok != 0).then_some(low != 0)
}
