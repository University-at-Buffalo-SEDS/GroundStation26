//! Suspend-inclusive local clock for finite SETTINGS ownership only.
/// Unlike Instant on some hosts, these clocks include system suspend.
pub(super) fn now(_virtual_now: u64) -> Option<u64> {
    #[cfg(any(test, feature = "firmware_sim"))]
    {
        Some(_virtual_now)
    }
    #[cfg(all(not(any(test, feature = "firmware_sim")), target_os = "linux"))]
    {
        let mut stamp = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut stamp) } != 0
            || stamp.tv_sec < 0
            || !(0..1_000_000_000).contains(&stamp.tv_nsec)
        {
            return None;
        }
        (stamp.tv_sec as u64)
            .checked_mul(1000)?
            .checked_add(stamp.tv_nsec as u64 / 1_000_000)
    }
    #[cfg(all(not(any(test, feature = "firmware_sim")), target_os = "macos"))]
    {
        // Darwin's mach_timebase_info_data_t is two uint32_t fields. Keep the
        // system ABI local rather than using libc's deprecated Mach aliases.
        #[repr(C)]
        struct MachTimebaseInfo {
            numer: u32,
            denom: u32,
        }
        unsafe extern "C" {
            fn mach_continuous_time() -> u64;
            fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
        }
        let mut ratio = MachTimebaseInfo { numer: 0, denom: 0 };
        if unsafe { mach_timebase_info(&mut ratio) } != 0 || ratio.denom == 0 {
            return None;
        }
        mach_ticks_to_ms(unsafe { mach_continuous_time() }, ratio.numer, ratio.denom)
    }
    #[cfg(all(
        not(any(test, feature = "firmware_sim")),
        not(any(target_os = "linux", target_os = "macos"))
    ))]
    {
        None
    }
}

#[cfg(any(test, all(target_os = "macos", not(feature = "firmware_sim"))))]
fn mach_ticks_to_ms(ticks: u64, numer: u32, denom: u32) -> Option<u64> {
    if denom == 0 {
        return None;
    }
    let nanos = u128::from(ticks) * u128::from(numer) / u128::from(denom);
    u64::try_from(nanos / 1_000_000).ok()
}

#[cfg(test)]
mod portable_mach_time_tests {
    #[test]
    fn continuous_ticks_conversion_preserves_ratio_and_bounds() {
        assert_eq!(super::mach_ticks_to_ms(5_000_000, 1, 1), Some(5));
        assert_eq!(super::mach_ticks_to_ms(1_500_000, 2, 3), Some(1));
        assert_eq!(super::mach_ticks_to_ms(999_999, 1, 1), Some(0));
        assert_eq!(super::mach_ticks_to_ms(u64::MAX, u32::MAX, 1), None);
        assert_eq!(super::mach_ticks_to_ms(1, 1, 0), None);
        assert_eq!(super::mach_ticks_to_ms(0, 1, 1), Some(0));
    }
}
