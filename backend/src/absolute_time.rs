//! Absolute (UTC) time of the station network.
//!
//! Relative sync stays GS-led: the station is the time source the AV bay
//! follows (GS -> RF -> FC/PB). This module only decides what UTC the station
//! feeds its SEDSnet clock as: host time plus a correction.
//!
//! Source order (the Pi has no RTC, so host time is unknown at power-up):
//! 1. RF GPS: RF reports (GPS UTC - network time) in its time sync requests
//!    (SEDSnet UTC reference reports, no extra frames). The median of the last
//!    30 s of reports is the correction target.
//! 2. NTP, when the kernel reports the host clock synchronized: correction 0.
//!    With GPS present it is only a cross-check (the GPS-NTP difference is
//!    logged as a free bias calibration).
//! 3. The last estimate, held, with a growing bound.
//!
//! Corrections slew at [`SLEW_PPM`]. A step (coarse set) is taken only while
//! no board is locked or the vehicle is in Startup/Idle, and never in a flight
//! state (Armed through Recovery).

use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Reports older than this leave the GPS window.
const GPS_WINDOW: Duration = Duration::from_secs(30);
/// GPS is the active source while its newest report is younger than this.
const GPS_FRESH: Duration = Duration::from_secs(10);
/// Slew rate for absolute corrections, parts per million (1%: 10 ms/s).
pub const SLEW_PPM: i64 = 10_000;
/// A correction larger than this is stepped when stepping is allowed.
const STEP_THRESHOLD_NS: i64 = 100_000_000;
/// Host oscillator drift assumed while holding (no RTC, no discipline), ppb.
const HOLD_DRIFT_PPB: u64 = 50_000;
/// Error assumed for a kernel-synchronized (NTP) host clock.
const NTP_BOUND_NS: u64 = 10_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AbsoluteSource {
    Gps,
    Ntp,
    Held,
    #[default]
    Unsynced,
}

/// What the UI shows: "UTC via GPS +-X ms", "via NTP", "unsynced".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct AbsoluteStatus {
    pub source: AbsoluteSource,
    /// Error bound of station UTC, microseconds (`None` when unknown).
    pub bound_us: Option<u64>,
    /// Correction applied to host time, microseconds.
    pub correction_us: i64,
    /// GPS UTC minus NTP host UTC, microseconds, when both were available.
    pub gps_minus_ntp_us: Option<i64>,
}

/// Inputs for one update (about once per second).
#[derive(Debug, Clone)]
pub struct Inputs {
    pub now: Instant,
    /// The kernel reports the host clock NTP-synchronized.
    pub ntp_synced: bool,
    /// UTC reports received since the last update: (GPS UTC - network time, error), ns.
    pub reports: Vec<(i64, u64)>,
    /// A coarse step is allowed now (no board locked or Startup/Idle, never in flight).
    pub may_step: bool,
}

#[derive(Debug, Default)]
pub struct AbsoluteTime {
    correction_ns: i64,
    window: VecDeque<(Instant, i64, u64)>,
    last_gps: Option<Instant>,
    last_update: Option<Instant>,
    synced_once: bool,
    status: AbsoluteStatus,
    bound_ns: u64,
    held_since: Option<Instant>,
}

fn median(values: &mut [i64]) -> i64 {
    values.sort_unstable();
    values[values.len() / 2]
}

impl AbsoluteTime {
    /// Correction to add to host UTC, ns.
    pub fn correction_ns(&self) -> i64 {
        self.correction_ns
    }

    pub fn status(&self) -> AbsoluteStatus {
        self.status
    }

    /// Updates the estimate. Returns `true` when the correction was stepped
    /// (the caller re-anchors the network clock instead of slewing it).
    pub fn update(&mut self, input: Inputs) -> bool {
        let period = self
            .last_update
            .map_or(Duration::from_secs(1), |t| input.now.saturating_duration_since(t))
            .min(Duration::from_secs(10));
        self.last_update = Some(input.now);
        // Reports measure the network clock as it read then: host + the
        // correction then in force. The absolute target is that plus the offset.
        for (offset_ns, error_ns) in &input.reports {
            self.window
                .push_back((input.now, self.correction_ns.saturating_add(*offset_ns), *error_ns));
            self.last_gps = Some(input.now);
        }
        while self
            .window
            .front()
            .is_some_and(|(t, _, _)| input.now.saturating_duration_since(*t) > GPS_WINDOW)
        {
            self.window.pop_front();
        }
        let gps_live = self
            .last_gps
            .is_some_and(|t| input.now.saturating_duration_since(t) <= GPS_FRESH)
            && !self.window.is_empty();
        let mut gps_minus_ntp_us = self.status.gps_minus_ntp_us;
        let (target, source, bound) = if gps_live {
            let mut targets: Vec<i64> = self.window.iter().map(|(_, c, _)| *c).collect();
            let target = median(&mut targets);
            let mut errors: Vec<i64> = self.window.iter().map(|(_, _, e)| *e as i64).collect();
            let bound = median(&mut errors).max(0) as u64;
            if input.ntp_synced {
                // Host is NTP-correct: the GPS target is the GPS-NTP difference.
                gps_minus_ntp_us = Some(target / 1_000);
            }
            (Some(target), AbsoluteSource::Gps, bound)
        } else if input.ntp_synced {
            (Some(0), AbsoluteSource::Ntp, NTP_BOUND_NS)
        } else if self.synced_once {
            let since = *self.held_since.get_or_insert(input.now);
            let drift = (input.now.saturating_duration_since(since).as_nanos() as u64)
                .saturating_mul(HOLD_DRIFT_PPB)
                / 1_000_000_000;
            (None, AbsoluteSource::Held, self.bound_ns.saturating_add(drift))
        } else {
            (None, AbsoluteSource::Unsynced, u64::MAX)
        };
        if source != AbsoluteSource::Held {
            self.held_since = None;
        }
        let mut stepped = false;
        if let Some(target) = target {
            let error = target.saturating_sub(self.correction_ns);
            if input.may_step && error.abs() > STEP_THRESHOLD_NS {
                self.correction_ns = target;
                stepped = true;
            } else {
                let max = (period.as_nanos() as i64).saturating_mul(SLEW_PPM) / 1_000_000;
                self.correction_ns += error.clamp(-max, max);
            }
            self.synced_once = true;
            if source != AbsoluteSource::Held {
                // Unfinished slew counts toward the bound.
                self.bound_ns = bound.saturating_add(target.abs_diff(self.correction_ns));
            }
        }
        let bound = if target.is_some() { self.bound_ns } else { bound };
        self.status = AbsoluteStatus {
            source,
            bound_us: (bound != u64::MAX).then_some(bound / 1_000),
            correction_us: self.correction_ns / 1_000,
            gps_minus_ntp_us,
        };
        stepped
    }
}

/// The kernel's view of NTP synchronization (chrony, ntpd or timesyncd all
/// clear STA_UNSYNC through adjtimex).
#[cfg(target_os = "linux")]
pub fn host_ntp_synced() -> bool {
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    let state = unsafe { libc::adjtimex(&mut tx) };
    state >= 0 && state != libc::TIME_ERROR && tx.status & libc::STA_UNSYNC == 0
}

#[cfg(not(target_os = "linux"))]
pub fn host_ntp_synced() -> bool {
    false
}

/// A step is allowed before any board is locked or while the vehicle is in
/// Startup/Idle, never from Armed through Recovery (or Aborted/InFlight).
pub fn may_step(flight_state: u8, any_board_locked: bool) -> bool {
    let in_flight = (6..=16).contains(&flight_state);
    !in_flight && (!any_board_locked || flight_state <= 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(now: Instant, ntp: bool, reports: Vec<(i64, u64)>, may_step: bool) -> Inputs {
        Inputs { now, ntp_synced: ntp, reports, may_step }
    }

    #[test]
    fn gps_median_rejects_outliers_and_steps_only_when_allowed() {
        let t0 = Instant::now();
        let mut a = AbsoluteTime::default();
        // Host clock 5 s behind GPS; one wild report.
        let reports = vec![(5_000_000_000, 20_000_000), (5_002_000_000, 20_000_000), (9_000_000_000, 20_000_000)];
        assert!(a.update(input(t0, false, reports, true)));
        assert_eq!(a.correction_ns(), 5_002_000_000);
        assert_eq!(a.status().source, AbsoluteSource::Gps);
        assert!(a.status().bound_us.unwrap() <= 21_000);
    }

    #[test]
    fn no_step_in_flight_states_only_slew() {
        assert!(!may_step(6, false));
        assert!(!may_step(16, false));
        assert!(!may_step(14, false));
        assert!(may_step(1, true));
        assert!(!may_step(2, true));
        assert!(may_step(2, false));
        let t0 = Instant::now();
        let mut a = AbsoluteTime::default();
        let mut now = t0;
        // 2 s error while flying: slews at 10 ms/s, never jumps.
        for _ in 0..10 {
            a.update(input(now, false, vec![(2_000_000_000 - a.correction_ns(), 20_000_000)], false));
            now += Duration::from_secs(1);
        }
        assert!(a.correction_ns() <= 100_000_000, "{}", a.correction_ns());
        assert!(a.correction_ns() >= 80_000_000);
    }

    #[test]
    fn source_order_gps_then_ntp_then_held() {
        let t0 = Instant::now();
        let mut a = AbsoluteTime::default();
        assert!(!a.update(input(t0, false, vec![], true)));
        assert_eq!(a.status().source, AbsoluteSource::Unsynced);
        assert_eq!(a.status().bound_us, None);
        // NTP only.
        a.update(input(t0 + Duration::from_secs(1), true, vec![], true));
        assert_eq!(a.status().source, AbsoluteSource::Ntp);
        // GPS arrives: primary; difference to NTP logged.
        a.update(input(t0 + Duration::from_secs(2), true, vec![(3_000_000, 15_000_000)], true));
        assert_eq!(a.status().source, AbsoluteSource::Gps);
        assert_eq!(a.status().gps_minus_ntp_us, Some(3_000));
        // GPS drops out, NTP lost too: hold, bound grows.
        a.update(input(t0 + Duration::from_secs(40), false, vec![], true));
        let held = a.status();
        assert_eq!(held.source, AbsoluteSource::Held);
        a.update(input(t0 + Duration::from_secs(400), false, vec![], true));
        assert!(a.status().bound_us.unwrap() > held.bound_us.unwrap());
        assert_eq!(a.status().correction_us, held.correction_us, "held, not moved");
        // NTP returns: falls back to NTP (target 0), slewing.
        a.update(input(t0 + Duration::from_secs(401), true, vec![], false));
        assert_eq!(a.status().source, AbsoluteSource::Ntp);
    }

    #[test]
    fn gps_dropout_holds_then_resumes_without_a_step_once_locked() {
        let t0 = Instant::now();
        let mut a = AbsoluteTime::default();
        a.update(input(t0, false, vec![(1_000_000_000, 10_000_000)], true));
        assert_eq!(a.correction_ns(), 1_000_000_000);
        a.update(input(t0 + Duration::from_secs(20), false, vec![], false));
        assert_eq!(a.status().source, AbsoluteSource::Held);
        // GPS back with a 300 ms different view, boards locked in PreFill: slew.
        let stepped = a.update(input(
            t0 + Duration::from_secs(21),
            false,
            vec![(300_000_000, 10_000_000)],
            false,
        ));
        assert!(!stepped);
        assert!(a.correction_ns() > 1_000_000_000 && a.correction_ns() < 1_100_000_000);
    }
}
