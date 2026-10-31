//! Suspend-inclusive local clock for finite SETTINGS ownership only.
/// Unlike Instant on some hosts, these clocks include system suspend.
pub(super) fn now(_virtual_now: u64) -> Option<u64> {
    #[cfg(any(test, feature = "firmware_sim"))]
    {
        return Some(_virtual_now);
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
        return (stamp.tv_sec as u64)
            .checked_mul(1000)?
            .checked_add(stamp.tv_nsec as u64 / 1_000_000);
    }
    #[cfg(all(not(any(test, feature = "firmware_sim")), target_os = "macos"))]
    {
        unsafe extern "C" {
            fn mach_continuous_time() -> u64;
        }
        let mut ratio = libc::mach_timebase_info_data_t { numer: 0, denom: 0 };
        if unsafe { libc::mach_timebase_info(&mut ratio) } != 0 || ratio.denom == 0 {
            return None;
        }
        let nanos = u128::from(unsafe { mach_continuous_time() }) * u128::from(ratio.numer)
            / u128::from(ratio.denom);
        return u64::try_from(nanos / 1_000_000).ok();
    }
    #[cfg(all(
        not(any(test, feature = "firmware_sim")),
        not(any(target_os = "linux", target_os = "macos"))
    ))]
    {
        None
    }
}
