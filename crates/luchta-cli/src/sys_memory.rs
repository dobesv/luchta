//! System-memory readings behind the dispatch memory-pressure gate.
//!
//! `sysinfo` provides a portable `available_memory()`, but its macOS
//! derivation is not usable as back-pressure (see [`platform`]), so macOS reads
//! the kernel counters directly and additionally surfaces the kernel's own
//! pressure verdict.

use sysinfo::System;

/// The kernel's own verdict on memory pressure, where the platform publishes
/// one.
///
/// macOS exports it as `kern.memorystatus_vm_pressure_level`. It is the signal
/// behind Activity Monitor's pressure graph and the one the OS itself uses to
/// tell processes to shrink, which makes it far more trustworthy than any byte
/// count derived from the raw VM counters.
///
/// Only macOS publishes a level today (Linux's PSI would be the equivalent), so
/// off macOS the variants are constructed only by the unit tests. The type stays
/// portable regardless so `MemorySample` has one shape on every platform.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KernelPressure {
    Normal,
    Warn,
    Critical,
}

impl KernelPressure {
    /// Maps a raw `kern.memorystatus_vm_pressure_level` value.
    ///
    /// The levels are the `DISPATCH_MEMORYPRESSURE_*` bits — 1 normal, 2 warn,
    /// 4 critical. Anything else comes from a kernel we do not understand, so
    /// report nothing rather than guess a level.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn from_raw(raw: i32) -> Option<Self> {
        match raw {
            1 => Some(Self::Normal),
            2 => Some(Self::Warn),
            4 => Some(Self::Critical),
            _ => None,
        }
    }

    /// Whether the kernel is actively asking processes to reduce their
    /// footprint.
    pub(crate) fn is_elevated(self) -> bool {
        matches!(self, Self::Warn | Self::Critical)
    }
}

/// Bytes the kernel can hand out without swapping.
pub(crate) fn available_bytes(sys: &System) -> u64 {
    platform::available_bytes(sys)
}

/// The kernel's current pressure verdict, or `None` where the platform does not
/// publish one.
pub(crate) fn kernel_pressure() -> Option<KernelPressure> {
    platform::kernel_pressure()
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::KernelPressure;
    use sysinfo::System;

    /// Linux's `MemAvailable` (and the Windows equivalent) already model
    /// reclaimable memory, so `sysinfo`'s reading is used as-is.
    pub(super) fn available_bytes(sys: &System) -> u64 {
        sys.available_memory()
    }

    /// No portable kernel-published pressure level outside macOS.
    pub(super) fn kernel_pressure() -> Option<KernelPressure> {
        None
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::sync::OnceLock;

    use sysinfo::System;

    use super::KernelPressure;

    /// The page counts from `host_statistics64(HOST_VM_INFO64)` that bear on
    /// availability, already widened from the kernel's `natural_t`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct VmCounts {
        pub(super) free: u64,
        pub(super) inactive: u64,
        pub(super) purgeable: u64,
        pub(super) compressor: u64,
        pub(super) page_size: u64,
    }

    /// Bytes macOS can hand out without swapping: `free + inactive + purgeable`.
    ///
    /// Deliberately does **not** subtract the compressor. Compressed pages are
    /// already accounted as *used* — Activity Monitor counts them under "Memory
    /// Used", and they appear in none of the three terms above — so subtracting
    /// them charges the same pages twice. `sysinfo`'s macOS `available_memory()`
    /// does subtract them, which drags the reading toward zero exactly when a
    /// build works hardest: under load the kernel compresses anonymous pages
    /// rather than freeing them, so the compressor grows as inactive shrinks.
    /// That is what makes the portable reading fire spurious "free memory low"
    /// pauses on an otherwise healthy Mac.
    pub(super) fn available_from_counts(counts: VmCounts) -> u64 {
        counts
            .free
            .saturating_add(counts.inactive)
            .saturating_add(counts.purgeable)
            .saturating_mul(counts.page_size)
    }

    pub(super) fn available_bytes(sys: &System) -> u64 {
        // Fall back to the portable reading only if the kernel call fails; a
        // pessimistic number still beats no number at all.
        vm_counts().map_or_else(|| sys.available_memory(), available_from_counts)
    }

    pub(super) fn kernel_pressure() -> Option<KernelPressure> {
        KernelPressure::from_raw(sysctl_i32(c"kern.memorystatus_vm_pressure_level")?)
    }

    static PORT: OnceLock<libc::mach_port_t> = OnceLock::new();

    /// The host port used for `host_statistics64`.
    ///
    /// `mach_host_self()` adds a send right on every call, so it is taken once
    /// and held for the life of the process rather than leaked on each sample.
    ///
    /// The deprecation points at the `mach2` crate, which is not worth a new
    /// workspace dependency for this one symbol; `host_statistics64` itself is
    /// not deprecated.
    #[allow(deprecated)]
    fn host_port() -> libc::mach_port_t {
        // SAFETY: takes no arguments and returns a port name by value.
        *PORT.get_or_init(|| unsafe { libc::mach_host_self() })
    }

    fn vm_counts() -> Option<VmCounts> {
        let mut count: libc::mach_msg_type_number_t = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: `stat` is sized for HOST_VM_INFO64 and `count` describes it in
        // the units the call expects; the kernel only writes `count` words.
        let stat = unsafe {
            let mut stat = std::mem::zeroed::<libc::vm_statistics64>();
            let status = libc::host_statistics64(
                host_port(),
                libc::HOST_VM_INFO64,
                std::ptr::addr_of_mut!(stat).cast(),
                &mut count,
            );
            if status != libc::KERN_SUCCESS {
                return None;
            }
            stat
        };

        let page_size = page_size()?;
        Some(VmCounts {
            free: u64::from(stat.free_count),
            inactive: u64::from(stat.inactive_count),
            purgeable: u64::from(stat.purgeable_count),
            compressor: u64::from(stat.compressor_page_count),
            page_size,
        })
    }

    fn page_size() -> Option<u64> {
        // SAFETY: `sysconf` takes a name and returns a scalar; no pointers.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        u64::try_from(size).ok().filter(|size| *size > 0)
    }

    fn sysctl_i32(name: &std::ffi::CStr) -> Option<i32> {
        let mut value: i32 = 0;
        let mut len = std::mem::size_of::<i32>();
        // SAFETY: `value`/`len` describe a correctly sized i32 output buffer and
        // the new-value pointer is null, so the call is read-only.
        let status = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                std::ptr::addr_of_mut!(value).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };

        (status == 0 && len == std::mem::size_of::<i32>()).then_some(value)
    }

    #[cfg(test)]
    mod tests {
        use super::{available_from_counts, vm_counts, VmCounts};

        /// The compressor must not be netted out of availability: a build that
        /// drives 8 GiB into the compressor is not 8 GiB poorer in reclaimable
        /// memory. This is the exact regression that stalls dispatch on macOS.
        #[test]
        fn available_ignores_the_compressor() {
            let counts = VmCounts {
                free: 1_000,
                inactive: 3_000,
                purgeable: 100,
                compressor: 500_000,
                page_size: 16_384,
            };

            assert_eq!(available_from_counts(counts), 4_100 * 16_384);
        }

        #[test]
        fn kernel_counters_are_readable_on_this_host() {
            let counts = vm_counts().expect("host_statistics64 should succeed on macOS");
            assert!(counts.page_size > 0);
            assert!(available_from_counts(counts) > 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{available_bytes, kernel_pressure, KernelPressure};

    #[test]
    fn maps_documented_pressure_levels() {
        assert_eq!(KernelPressure::from_raw(1), Some(KernelPressure::Normal));
        assert_eq!(KernelPressure::from_raw(2), Some(KernelPressure::Warn));
        assert_eq!(KernelPressure::from_raw(4), Some(KernelPressure::Critical));
    }

    #[test]
    fn refuses_to_guess_unknown_pressure_levels() {
        assert_eq!(KernelPressure::from_raw(0), None);
        assert_eq!(KernelPressure::from_raw(3), None);
        assert_eq!(KernelPressure::from_raw(-1), None);
    }

    #[test]
    fn only_warn_and_critical_count_as_elevated() {
        assert!(!KernelPressure::Normal.is_elevated());
        assert!(KernelPressure::Warn.is_elevated());
        assert!(KernelPressure::Critical.is_elevated());
    }

    #[test]
    fn reports_available_memory_for_this_host() {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();

        assert!(available_bytes(&sys) > 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_publishes_a_kernel_pressure_level() {
        assert!(kernel_pressure().is_some());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn other_platforms_publish_no_kernel_pressure_level() {
        assert_eq!(kernel_pressure(), None);
    }
}
