//! Absolute (UTC) time of the station network.
//!
//! Relative sync stays GS-led: the station is the time source the AV bay
//! follows (GS -> RF -> FC/PB). This module only decides what UTC the station
//! feeds its SEDSnet clock: host time plus a correction.
//!
//! Source order (the Pi has no RTC):
//! 1. GPS with PPS: an RF report whose error is within [`PPS_REPORT_ERROR_NS`]
//!    (a PPS-disciplined receiver; this RF revision has none, a future board
//!    may). It is the most accurate source.
//! 2. NTP, when the kernel reports the host clock synchronized: correction 0.
//!    While NTP is synced, NMEA GPS reports only teach the GPS reporting
//!    delay (GPS UTC as reported minus true UTC), which is persisted.
//! 3. NMEA GPS with the learned delay subtracted (the default delay when it
//!    was never learned).
//! 4. The last estimate, held, with a growing bound.
//!
//! RF reports `GPS UTC - network time` from NMEA without removing any delay;
//! RF's network clock follows the station's within its sync bound, so the
//! radio path is already removed by time sync.
//!
//! Corrections slew at [`SLEW_PPM`]. A step (coarse set) is taken only while
//! no board is locked or the vehicle is in Startup/Idle, and never in a flight
//! state (Armed through Recovery).

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// GPS reports older than this leave the target window.
const GPS_WINDOW: Duration = Duration::from_secs(30);
/// GPS is a live source while its newest report is younger than this.
const GPS_FRESH: Duration = Duration::from_secs(10);
/// Reports at or below this error come from a PPS-disciplined receiver.
pub const PPS_REPORT_ERROR_NS: u64 = 2_000_000;
/// Slew rate for absolute corrections, parts per million (1%: 10 ms/s).
pub const SLEW_PPM: i64 = 10_000;
/// A correction larger than this is stepped when stepping is allowed.
const STEP_THRESHOLD_NS: i64 = 100_000_000;
/// Host oscillator drift assumed while holding (no RTC, no discipline), ppb.
const HOLD_DRIFT_PPB: u64 = 50_000;
/// Error assumed for a kernel-synchronized (NTP) host clock.
const NTP_BOUND_NS: u64 = 10_000_000;
/// GPS reporting delay used until one is learned (NEO-M9N NMEA over SPI).
pub const DEFAULT_GPS_DELAY_NS: i64 = 40_000_000;
/// Delay samples kept for learning (one per report, about 0.25-1 per second).
const DELAY_WINDOW: usize = 240;
/// Samples needed before a learned delay is used.
const DELAY_MIN_SAMPLES: usize = 30;
/// Plausible GPS reporting delay range; samples outside are rejected.
const DELAY_VALID_NS: (i64, i64) = (-50_000_000, 500_000_000);
/// Learned delay is accepted only when 3/4 of the samples lie this close to the median.
const DELAY_MAX_MAD_NS: i64 = 20_000_000;
const DELAY_FILE: &str = "gps_delay.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AbsoluteSource {
    GpsPps,
    Ntp,
    Gps,
    Held,
    #[default]
    Unsynced,
}

/// What the UI shows: "UTC via NTP ±X ms", "via GPS ±X ms (delay 41 ms learned)".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct AbsoluteStatus {
    pub source: AbsoluteSource,
    /// Error bound of station UTC, microseconds (`None` when unknown).
    pub bound_us: Option<u64>,
    /// Correction applied to host time, microseconds.
    pub correction_us: i64,
    /// GPS reporting delay in use, microseconds.
    pub gps_delay_us: i64,
    /// The delay was learned against NTP (else the default).
    pub gps_delay_learned: bool,
    /// GPS UTC (delay removed) minus NTP host UTC, microseconds, while both run.
    pub gps_minus_ntp_us: Option<i64>,
}

/// Inputs for one update (about once per second).
#[derive(Debug, Clone)]
pub struct Inputs {
    pub now: Instant,
    /// The kernel reports the host clock NTP-synchronized.
    pub ntp_synced: bool,
    /// RF reports since the last update: (GPS UTC - network time as reported, error), ns.
    pub reports: Vec<(i64, u64)>,
    /// A coarse step is allowed now (no board locked or Startup/Idle, never in flight).
    pub may_step: bool,
}

/// Persisted learned delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct SavedDelay {
    delay_us: i64,
    samples: u32,
}

#[derive(Debug, Default)]
struct DelayLearner {
    samples: VecDeque<i64>,
    learned: Option<i64>,
    saved: Option<i64>,
    path: Option<PathBuf>,
}

fn median(values: &mut [i64]) -> i64 {
    values.sort_unstable();
    values[values.len() / 2]
}

impl DelayLearner {
    fn load(path: &Path) -> Self {
        let saved = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<SavedDelay>(&b).ok())
            .map(|s| s.delay_us.saturating_mul(1_000))
            .filter(|d| (DELAY_VALID_NS.0..=DELAY_VALID_NS.1).contains(d));
        Self { learned: saved, saved, path: Some(path.to_path_buf()), ..Default::default() }
    }

    fn delay_ns(&self) -> i64 {
        self.learned.unwrap_or(DEFAULT_GPS_DELAY_NS)
    }

    /// One sample: reported GPS UTC minus true (NTP) UTC.
    fn offer(&mut self, delay_ns: i64) {
        if !(DELAY_VALID_NS.0..=DELAY_VALID_NS.1).contains(&delay_ns) {
            return;
        }
        if self.samples.len() == DELAY_WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(delay_ns);
        if self.samples.len() < DELAY_MIN_SAMPLES {
            return;
        }
        let mut v: Vec<i64> = self.samples.iter().copied().collect();
        let med = median(&mut v);
        // At least three quarters of the window must agree with the median.
        let agree = v.iter().filter(|d| (**d - med).abs() <= DELAY_MAX_MAD_NS).count();
        if agree * 4 < v.len() * 3 {
            return;
        }
        self.learned = Some(med);
        // Persist when it moved by more than 1 ms.
        if self.saved.is_none_or(|s| (s - med).abs() > 1_000_000)
            && let Some(path) = &self.path
        {
            let saved = SavedDelay { delay_us: med / 1_000, samples: self.samples.len() as u32 };
            if let Ok(json) = serde_json::to_vec(&saved) {
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, path).is_ok() {
                    self.saved = Some(med);
                }
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct AbsoluteTime {
    correction_ns: i64,
    /// (received, implied correction with zero delay, error, PPS-grade).
    window: VecDeque<(Instant, i64, u64, bool)>,
    last_gps: Option<Instant>,
    last_update: Option<Instant>,
    synced_once: bool,
    status: AbsoluteStatus,
    bound_ns: u64,
    held_since: Option<Instant>,
    delay: DelayLearner,
}

impl AbsoluteTime {
    /// Uses (and persists the learned GPS delay in) `dir`.
    pub fn with_data_dir(dir: &Path) -> Self {
        Self { delay: DelayLearner::load(&dir.join(DELAY_FILE)), ..Default::default() }
    }

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
        // Each report measured the network clock as it read then (host + the
        // correction then in force); with zero GPS delay it implies this
        // correction.
        for (offset_ns, error_ns) in &input.reports {
            let implied = self.correction_ns.saturating_add(*offset_ns);
            let pps = *error_ns <= PPS_REPORT_ERROR_NS;
            self.window.push_back((input.now, implied, *error_ns, pps));
            self.last_gps = Some(input.now);
            if input.ntp_synced && !pps {
                // NTP says the true correction is 0: the report is late by -implied.
                self.delay.offer(-implied);
            }
        }
        while self
            .window
            .front()
            .is_some_and(|(t, ..)| input.now.saturating_duration_since(*t) > GPS_WINDOW)
        {
            self.window.pop_front();
        }
        let gps_live = self
            .last_gps
            .is_some_and(|t| input.now.saturating_duration_since(t) <= GPS_FRESH)
            && !self.window.is_empty();
        let pps_live = gps_live && self.window.iter().rev().take(4).any(|r| r.3);
        let delay = self.delay.delay_ns();
        let gps_target = |pps_only: bool, w: &VecDeque<(Instant, i64, u64, bool)>| {
            let mut t: Vec<i64> = w
                .iter()
                .filter(|r| r.3 == pps_only)
                .map(|r| if r.3 { r.1 } else { r.1 + delay })
                .collect();
            let mut e: Vec<i64> = w.iter().filter(|r| r.3 == pps_only).map(|r| r.2 as i64).collect();
            (!t.is_empty()).then(|| (median(&mut t), median(&mut e).max(0) as u64))
        };
        let gps_minus_ntp_us = if input.ntp_synced && gps_live {
            gps_target(false, &self.window).map(|(t, _)| t / 1_000)
        } else {
            None
        };
        let (target, source, bound) = if let Some((t, e)) =
            gps_target(true, &self.window).filter(|_| pps_live)
        {
            (Some(t), AbsoluteSource::GpsPps, e)
        } else if input.ntp_synced {
            (Some(0), AbsoluteSource::Ntp, NTP_BOUND_NS)
        } else if let Some((t, e)) = gps_target(false, &self.window).filter(|_| gps_live) {
            // An unlearned delay is a guess: widen the bound by the default.
            let extra = if self.delay.learned.is_some() { 0 } else { DEFAULT_GPS_DELAY_NS as u64 };
            (Some(t), AbsoluteSource::Gps, e.saturating_add(extra))
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
            // Unfinished slew counts toward the bound.
            self.bound_ns = bound.saturating_add(target.abs_diff(self.correction_ns));
        }
        let bound = if target.is_some() { self.bound_ns } else { bound };
        self.status = AbsoluteStatus {
            source,
            bound_us: (bound != u64::MAX).then_some(bound / 1_000),
            correction_us: self.correction_ns / 1_000,
            gps_delay_us: self.delay.delay_ns() / 1_000,
            gps_delay_learned: self.delay.learned.is_some(),
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

    const MS: i64 = 1_000_000;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "gs-abs-time-{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn input(now: Instant, ntp: bool, reports: Vec<(i64, u64)>, may_step: bool) -> Inputs {
        Inputs { now, ntp_synced: ntp, reports, may_step }
    }

    /// RF report for a host `host_err` behind true UTC with GPS `delay` late:
    /// reported GPS UTC - network = -(delay) - (correction + host_err) + ...
    fn report(a: &AbsoluteTime, host_behind: i64, delay: i64) -> (i64, u64) {
        // network = host + correction = true - host_behind + correction
        // GPS as reported = true - delay
        (host_behind - a.correction_ns() - delay, 20 * MS as u64)
    }

    #[test]
    fn ntp_leads_and_teaches_the_gps_delay_which_persists() {
        let dir = TempDir::new();
        let t0 = Instant::now();
        let mut a = AbsoluteTime::with_data_dir(dir.path());
        assert_eq!(a.status().gps_delay_learned, false);
        for i in 0..40u64 {
            let jitter = ((i * 7) % 5) as i64 * MS;
            let r = report(&a, 0, 37 * MS + jitter);
            a.update(input(t0 + Duration::from_secs(i), true, vec![r], true));
            assert_eq!(a.status().source, AbsoluteSource::Ntp);
            assert_eq!(a.correction_ns(), 0, "NTP stays the source while synced");
        }
        let st = a.status();
        assert!(st.gps_delay_learned);
        assert!((st.gps_delay_us - 39_000).abs() <= 2_000, "{st:?}");
        // A new station loads the learned value.
        let b = AbsoluteTime::with_data_dir(dir.path());
        assert!(b.delay.learned.is_some_and(|d| (d - st.gps_delay_us * 1_000).abs() <= 1_000_000));
    }

    #[test]
    fn without_ntp_gps_with_the_learned_delay_sets_utc() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join(DELAY_FILE), br#"{"delay_us":45000,"samples":200}"#).unwrap();
        let t0 = Instant::now();
        let mut a = AbsoluteTime::with_data_dir(dir.path());
        // Host 5 s behind true UTC (no RTC), GPS 45 ms late.
        let r = report(&a, 5_000 * MS, 45 * MS);
        assert!(a.update(input(t0, false, vec![r], true)));
        assert_eq!(a.status().source, AbsoluteSource::Gps);
        assert!((a.correction_ns() - 5_000 * MS).abs() < MS, "{}", a.correction_ns());
        assert!(a.status().gps_delay_learned);
    }

    #[test]
    fn pps_gps_outranks_ntp() {
        let t0 = Instant::now();
        let mut a = AbsoluteTime::default();
        // NTP host 3 ms off; a PPS receiver reports with 50 us error.
        a.update(input(t0, true, vec![(3 * MS, 50_000)], true));
        assert_eq!(a.status().source, AbsoluteSource::GpsPps);
        assert_eq!(a.correction_ns(), 3 * MS);
        assert!(a.status().bound_us.unwrap() <= 50);
    }

    #[test]
    fn median_rejects_outliers_and_steps_only_when_allowed() {
        let t0 = Instant::now();
        let mut a = AbsoluteTime::default();
        let d = DEFAULT_GPS_DELAY_NS;
        let reports = vec![(5_000 * MS - d, 20 * MS as u64), (5_002 * MS - d, 20 * MS as u64), (9_000 * MS, 20 * MS as u64)];
        assert!(a.update(input(t0, false, reports, true)));
        assert_eq!(a.correction_ns(), 5_002 * MS);
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
        for _ in 0..10 {
            let r = (2_000 * MS - a.correction_ns() - DEFAULT_GPS_DELAY_NS, 20 * MS as u64);
            a.update(input(now, false, vec![r], false));
            now += Duration::from_secs(1);
        }
        assert!(a.correction_ns() <= 100 * MS && a.correction_ns() >= 80 * MS, "{}", a.correction_ns());
    }

    #[test]
    fn dropout_holds_then_falls_back() {
        let t0 = Instant::now();
        let mut a = AbsoluteTime::default();
        assert!(!a.update(input(t0, false, vec![], true)));
        assert_eq!(a.status().source, AbsoluteSource::Unsynced);
        assert_eq!(a.status().bound_us, None);
        a.update(input(t0 + Duration::from_secs(1), false, vec![(1_000 * MS, 20 * MS as u64)], true));
        assert_eq!(a.status().source, AbsoluteSource::Gps);
        let c = a.correction_ns();
        a.update(input(t0 + Duration::from_secs(40), false, vec![], false));
        let held = a.status();
        assert_eq!(held.source, AbsoluteSource::Held);
        a.update(input(t0 + Duration::from_secs(400), false, vec![], false));
        assert!(a.status().bound_us.unwrap() > held.bound_us.unwrap());
        assert_eq!(a.correction_ns(), c, "held, not moved");
        a.update(input(t0 + Duration::from_secs(401), true, vec![], false));
        assert_eq!(a.status().source, AbsoluteSource::Ntp);
    }

    #[test]
    fn implausible_delay_samples_are_ignored() {
        let mut l = DelayLearner::default();
        for _ in 0..100 {
            l.offer(2_000 * MS);
        }
        assert!(l.learned.is_none());
        // Widely scattered samples never settle.
        for i in 0..100 {
            l.offer(if i % 2 == 0 { 10 * MS } else { 300 * MS });
        }
        assert!(l.learned.is_none());
    }
}
