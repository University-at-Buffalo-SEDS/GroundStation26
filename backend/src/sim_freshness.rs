//! Wall-clock launch-proof windows under a slowed MCU simulator.
//!
//! Most procedure freshness already follows the explicit model clock in
//! simulator builds. Two launch proofs still age in wall time: the retained
//! board-state proof (`board_abort`) and the actuator retraction proof
//! (`actuator_plumbing`). A Renode bay runs many times slower than real time,
//! so a reply that takes milliseconds of board time can take seconds of wall
//! time and those windows expire before Launch admission can use them.
//!
//! Only a `firmware_sim` build (never a Raspberry Pi build), outside test-fire
//! mode, reads `GS_SIM_WALL_FRESHNESS_SCALE`, an integer 1..=100 that
//! stretches those windows. Every other build or mode, and a simulator build
//! without the variable, uses the operational value unchanged.
use std::time::Duration;

/// Environment variable read only by simulator builds.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const SCALE_ENV: &str = "GS_SIM_WALL_FRESHNESS_SCALE";
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const MAX_SCALE: u64 = 100;

/// True only for builds and modes that may stretch wall-clock proof windows.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn scaling_build() -> bool {
    cfg!(all(feature = "firmware_sim", not(feature = "raspberry_pi")))
        && !crate::runtime_mode::is_test_fire()
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_scale(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| (1..=MAX_SCALE).contains(v))
        .unwrap_or(1)
}

/// The wall-clock stretch factor: always 1 outside simulator builds.
pub(crate) fn scale() -> u64 {
    #[cfg(all(feature = "firmware_sim", not(feature = "raspberry_pi")))]
    if !crate::runtime_mode::is_test_fire() {
        static SCALE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        return *SCALE.get_or_init(|| {
            let scale = parse_scale(std::env::var(SCALE_ENV).ok().as_deref());
            if scale != 1 {
                log::warn!(
                    "Simulator build: wall-clock launch-proof windows stretched {scale}x ({SCALE_ENV})"
                );
            }
            scale
        });
    }
    1
}

/// `base` milliseconds of wall time, stretched only in a simulator build.
pub(crate) fn wall_ms(base: u64) -> u64 {
    base.saturating_mul(scale())
}

/// `base` of wall time, stretched only in a simulator build.
pub(crate) fn wall(base: Duration) -> Duration {
    base.saturating_mul(scale() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_only_bounded_positive_integers() {
        assert_eq!(parse_scale(None), 1);
        assert_eq!(parse_scale(Some("")), 1);
        assert_eq!(parse_scale(Some("0")), 1);
        assert_eq!(parse_scale(Some("-3")), 1);
        assert_eq!(parse_scale(Some("2.5")), 1);
        assert_eq!(parse_scale(Some("101")), 1);
        assert_eq!(parse_scale(Some(" 20 ")), 20);
        assert_eq!(parse_scale(Some("100")), 100);
    }

    #[test]
    fn operational_launch_proof_windows_are_unchanged() {
        // Launch / ignition-pair board proof: 10 s (92cee1c). The actuator
        // plumbing freshness window stays 5 s.
        assert_eq!(crate::board_abort::PROOF_TTL, Duration::from_secs(10));
        assert_eq!(crate::actuator_plumbing::FRESH_MS, 5_000);
    }

    #[test]
    fn only_the_simulator_build_may_stretch_windows() {
        assert_eq!(
            scaling_build(),
            cfg!(feature = "firmware_sim")
                && !crate::runtime_mode::is_test_fire()
                && !cfg!(feature = "raspberry_pi")
        );
    }

    /// Production, test-fire and HITL builds keep the operational windows
    /// even when the variable is present in their environment.
    #[test]
    fn non_simulator_builds_keep_operational_values_even_with_the_variable_set() {
        const NAME: &str =
            "sim_freshness::tests::non_simulator_builds_keep_operational_values_even_with_the_variable_set";
        if std::env::var_os("GS_SIM_FRESHNESS_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--test-threads=1"])
                .env("GS_SIM_FRESHNESS_CHILD", "1")
                .env(SCALE_ENV, "20")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        if scaling_build() {
            assert_eq!(wall_ms(5_000), 100_000);
            assert_eq!(wall(Duration::from_secs(5)), Duration::from_secs(100));
        } else {
            assert_eq!(scale(), 1);
            assert_eq!(wall_ms(5_000), 5_000);
            assert_eq!(wall(Duration::from_secs(5)), Duration::from_secs(5));
        }
    }

    #[test]
    fn simulator_build_without_the_variable_is_unchanged() {
        const NAME: &str =
            "sim_freshness::tests::simulator_build_without_the_variable_is_unchanged";
        if std::env::var_os("GS_SIM_FRESHNESS_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--test-threads=1"])
                .env("GS_SIM_FRESHNESS_CHILD", "1")
                .env_remove(SCALE_ENV)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        assert_eq!(scale(), 1);
        assert_eq!(wall_ms(5_000), 5_000);
    }
}
