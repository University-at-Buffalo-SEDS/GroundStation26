//! Diagnostic liveness history. Silence is not evidence of a reboot or a CPU fault.
use crate::types::{BoardDiagnostics, NetworkBoardHealthEntry, RfTxTiming, canonical_sender_id};
use std::{collections::HashMap, time::Instant};
pub const TIMEOUT_MS: u64 = 12_000;
/// Silence that marks a board offline, by its link. Umbilical (CAN) boards report at 1-250 Hz (frame age
/// p99 under 1 s in the fullnet); radio boards need margin for loss (30% loss: RF 4.3 s, PB 2.8 s, FC 1.3 s
/// longest gaps). A board whose link is not known yet keeps TIMEOUT_MS.
pub const WIRED_TIMEOUT_MS: u64 = 3_000;
pub const RADIO_TIMEOUT_MS: u64 = 6_000;
/// Window for the board clock estimate (see `observe_clock`).
const CLOCK_WINDOW_MS: u64 = 10_000;
#[derive(Default)]
pub struct Tracker {
    boards: HashMap<String, Record>,
    pub(crate) sd_retry_gate: crate::daq_sd::RetryGate,
}
struct Record {
    diagnostics: Option<(BoardDiagnostics, Instant)>,
    diagnostic_header_utc: Option<u64>,
    sd_status: Option<(crate::daq_sd::SdStatus, Instant)>,
    launch_log: Option<(crate::daq_launch_log::Observation, Instant, u64)>,
    first: Instant,
    last: Instant,
    /// Start of the current connected spell: first contact or last reconnect.
    connected_since: Instant,
    healthy: bool,
    last_seen_ms: u64,
    disconnects: u64,
    reconnects: u64,
    observations: u64,
    offline_ms: u64,
    longest_ms: u64,
    rocket: bool,
    fill: bool,
    announcements: HashMap<String, u64>,
    clock: Option<(i64, Instant)>,
}
fn elapsed(now: Instant, then: Instant) -> u64 {
    now.saturating_duration_since(then)
        .as_millis()
        .min(u64::MAX as u128) as u64
}
impl Record {
    fn timeout_ms(&self) -> u64 {
        if self.rocket { RADIO_TIMEOUT_MS } else if self.fill { WIRED_TIMEOUT_MS } else { TIMEOUT_MS }
    }
    fn advance(&mut self, now: Instant) {
        if self.healthy && elapsed(now, self.last) > self.timeout_ms() {
            self.healthy = false;
            self.disconnects = self.disconnects.saturating_add(1);
        }
    }
    fn observe(&mut self, now: Instant, wall_ms: u64, packet: bool) {
        if packet {
            self.observations = self.observations.saturating_add(1);
        }
        if now <= self.last {
            return;
        }
        self.advance(now);
        if !self.healthy {
            let outage = elapsed(now, self.last).saturating_sub(self.timeout_ms());
            self.offline_ms = self.offline_ms.saturating_add(outage);
            self.longest_ms = self.longest_ms.max(outage);
            self.reconnects = self.reconnects.saturating_add(1);
            self.connected_since = now;
        }
        self.healthy = true;
        self.last = now;
        self.last_seen_ms = wall_ms;
    }
    fn visible(&self, sender: &str, test_fire: bool) -> bool {
        if !test_fire {
            return true;
        }
        match sender {
            "FC" | "RF" | "PB" | "RRLS" => false,
            "VB" | "AB" | "GB" | "DAQ" => true,
            _ => !self.rocket || self.fill,
        }
    }
}
// Integer wire fields retain exact counter values; do not round through f32 telemetry rows.
pub(crate) fn decode_diagnostics(w: &[u32]) -> Option<BoardDiagnostics> {
    if w.len() != 24 || w[0] != 1 || w[4] == 0 || w[4] > 1_000_000_000 {
        return None;
    }
    let value = |index: usize, mask: u32| {
        if w[1] & mask != 0 {
            Some(w[index])
        } else {
            None
        }
    };
    Some(BoardDiagnostics {
        die_temperature_centi_c: if w[1] & 0x100 != 0 && ((w[1] >> 9) & 0x7f) <= 60 {
            let centi = i32::from((w[1] >> 16) as i16);
            (-4000..=15000).contains(&centi).then_some(centi)
        } else {
            None
        },
        die_temperature_sample_age_ms: ((w[1] >> 9) & 0x7f) * 100,
        version: w[0],
        received_age_ms: 0,
        uptime_ticks: u64::from(w[2]) | (u64::from(w[3]) << 32),
        uptime_frequency_hz: w[4],
        reset_flags: w[5],
        watchdog_missing_mask: w[6] & 0xFFFF,
        service_faults: service_fault_labels((w[6] >> 16) & 0xFF),
        heap_live_bytes: value(7, 1),
        heap_peak_bytes: value(8, 1),
        heap_region_bytes: value(9, 1),
        allocation_failures: value(10, 1),
        admission_refusals: w[11],
        can_rx_drops: value(12, 2),
        can_tx_rejected: value(13, 4),
        uart_rx_drops: value(14, 8),
        tx_queue_high_water: value(15, 64),
        sd_accepted_records: value(16, 16),
        sd_processed_records: value(17, 16),
        sd_rejected_records: value(18, 16),
        sd_flushed_records: value(19, 16),
        sd_write_errors: value(20, 16),
        sd_max_write_ms: value(21, 128),
        adc_overruns: value(22, 32),
        diagnostic_send_failures: w[23],
        // Tagged words owned by SD/ADC fields on other boards; never both.
        rf_tx_timing: (w[1] & (16 | 32 | 128) == 0 && w[16] == RF_TX_TIMING_TAG).then(|| {
            RfTxTiming {
                ack_frames: w[17],
                ack_mean_ms: w[18],
                ack_max_ms: w[19],
                queue_high_water: w[20],
                stale_drops: w[21],
            }
        }),
        ..Default::default()
    })
}
/// Optional router services that failed to start on RF/PB firmware
/// (telemetry.c TELEMETRY_SERVICE_*). The board keeps routing and retries
/// them every 5 s; the operator sees which feature is unavailable.
pub(crate) fn service_fault_labels(mask: u32) -> Vec<String> {
    const LABELS: [&str; 4] = [
        "Time sync setup failed",
        "OTA storage unavailable",
        "AV-bay underglow unavailable",
        "Flight-state cache unavailable",
    ];
    (0..8)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| {
            LABELS
                .get(bit as usize)
                .map_or_else(|| format!("Service {bit} unavailable"), |label| label.to_string())
        })
        .collect()
}

/// "RFT1" little-endian in BOARD_HEALTH word 16.
const RF_TX_TIMING_TAG: u32 = 0x3154_4652;

impl Tracker {
    fn record(&mut self, sender: &str, now: Instant, wall_ms: u64) -> &mut Record {
        // Allocate sender storage only on first observation, not on every sample.
        if !self.boards.contains_key(sender) {
            self.boards.insert(
                sender.to_owned(),
                Record {
                    diagnostics: None,
                    diagnostic_header_utc: None,
                    sd_status: None,
                    launch_log: None,
                    first: now,
                    last: now,
                    connected_since: now,
                    healthy: true,
                    last_seen_ms: wall_ms,
                    disconnects: 0,
                    reconnects: 0,
                    observations: 0,
                    offline_ms: 0,
                    longest_ms: 0,
                    rocket: false,
                    fill: false,
                    announcements: HashMap::new(),
                    clock: None,
                },
            );
        }
        self.boards.get_mut(sender).unwrap()
    }
    pub fn observe_diagnostics(
        &mut self,
        sender: &str,
        words: &[u32],
        now: Instant,
        wall_ms: u64,
    ) -> bool {
        let Some(report) = decode_diagnostics(words) else {
            return false;
        };
        let sender = canonical_sender_id(sender.trim());
        if sender.is_empty() || sender == "GS" || sender.starts_with('@') {
            return false;
        }
        let record = self.record(sender, now, wall_ms);
        record.diagnostics = Some((report, now));
        record.diagnostic_header_utc = None;
        true
    }
    /// Recording-only source metadata; actual acceptance stays in the unchanged
    /// shared decoder/admission path, including old callers with unknown clocks.
    pub(crate) fn observe_diagnostics_with_source(
        &mut self,
        sender: &str,
        words: &[u32],
        now: Instant,
        wall_ms: u64,
        header_utc: Option<u64>,
    ) -> bool {
        if !self.observe_diagnostics(sender, words, now, wall_ms) {
            return false;
        }
        self.boards
            .get_mut(canonical_sender_id(sender.trim()))
            .unwrap()
            .diagnostic_header_utc = header_utc;
        true
    }
    pub fn observe_sd_status(
        &mut self,
        sender: &str,
        words: &[u32],
        now: Instant,
        wall_ms: u64,
    ) -> bool {
        if canonical_sender_id(sender.trim()) != "DAQ" {
            return false;
        }
        let Some(status) = crate::daq_sd::SdStatus::decode(words) else {
            return false;
        };
        self.record("DAQ", now, wall_ms).sd_status = Some((status, now));
        true
    }

    pub fn observe_launch_log_status(
        &mut self,
        sender: &str,
        words: &[u32],
        now: Instant,
        wall_ms: u64,
        source_ms: u64,
        network_now_ms: u64,
    ) -> bool {
        if canonical_sender_id(sender.trim()) != "DAQ" {
            return false;
        }
        let Some(status) = crate::daq_launch_log::Observation::decode(words) else {
            return false;
        };
        let Some(initial_age) = crate::daq_launch_log::source_age_ms(source_ms, network_now_ms)
        else {
            return false;
        };
        self.record("DAQ", now, wall_ms).launch_log = Some((status, now, initial_age));
        true
    }

    pub(crate) fn daq_sd_status(&self, now: Instant) -> Option<(crate::daq_sd::SdStatus, u64)> {
        self.boards
            .get("DAQ")?
            .sd_status
            .as_ref()
            .map(|(status, received)| (status.clone(), elapsed(now, *received)))
    }

    pub fn observe(&mut self, sender: &str, now: Instant, wall_ms: u64) {
        let sender = canonical_sender_id(sender.trim());
        if sender.is_empty() || sender == "GS" || sender.starts_with('@') {
            return;
        }
        self.record(sender, now, wall_ms)
            .observe(now, wall_ms, true);
    }
    /// Board packet timestamp against station receive time, for clock sync display.
    /// Transport delay only ever makes a packet look older, so the least
    /// negative offset seen in the last CLOCK_WINDOW_MS is the clock estimate;
    /// the last sample alone reads a queued radio packet as a slow clock.
    pub fn observe_clock(&mut self, sender: &str, source_ms: u64, wall_ms: u64, now: Instant) {
        let sender = canonical_sender_id(sender.trim());
        if sender.is_empty() || sender == "GS" || sender.starts_with('@') || source_ms == 0 {
            return;
        }
        let offset = (source_ms as i128 - wall_ms as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
        let record = self.record(sender, now, wall_ms);
        let keep = record
            .clock
            .is_some_and(|(best, at)| best > offset && elapsed(now, at) <= CLOCK_WINDOW_MS);
        if !keep {
            record.clock = Some((offset, now));
        }
    }
    /// How long `sender` has been continuously connected, or `None` while it
    /// is offline or has never been seen.
    pub fn connected_for(&self, sender: &str, now: Instant) -> Option<std::time::Duration> {
        let r = self.boards.get(canonical_sender_id(sender.trim()))?;
        (r.healthy && elapsed(now, r.last) <= r.timeout_ms())
            .then(|| now.saturating_duration_since(r.connected_since))
    }
    pub fn membership(&mut self, sender: &str, side: &str) {
        if let Some(r) = self.boards.get_mut(canonical_sender_id(sender)) {
            r.rocket |= side == "rocket_comms";
            r.fill |= side == "umbilical_comms";
        }
    }
    pub fn announcement(
        &mut self,
        sender: &str,
        side: &str,
        stamp: u64,
        age: u64,
        now: Instant,
        wall_ms: u64,
    ) {
        let sender = canonical_sender_id(sender);
        if age > TIMEOUT_MS || sender == "GS" || sender.is_empty() || sender.starts_with('@') {
            return;
        }
        let Some(observed) = now.checked_sub(std::time::Duration::from_millis(age)) else {
            return;
        };
        let r = self.record(sender, observed, wall_ms.saturating_sub(age));
        if r.announcements.get(side) != Some(&stamp) {
            r.announcements.insert(side.to_owned(), stamp);
            r.observe(observed, wall_ms.saturating_sub(age), false);
        }
        self.membership(sender, side);
    }
    pub fn clear(&mut self) {
        self.boards.clear();
    }
    /// Recording observes all actual sources without a nested journal-store lookup.
    /// Expected launch-log matching is unknown here; raw journal reports are saved.
    pub(crate) fn recording_source_stamps(&self) -> Vec<(String, Option<u64>)> {
        self.boards
            .iter()
            .filter(|(_, r)| r.diagnostics.is_some())
            .map(|(sender, r)| (sender.clone(), r.diagnostic_header_utc))
            .collect()
    }
    pub(crate) fn recording_snapshot(&mut self, now: Instant) -> Vec<NetworkBoardHealthEntry> {
        self.snapshot_with_launch_expected(now, false, None, false)
    }
    pub fn snapshot(&mut self, now: Instant, test_fire: bool) -> Vec<NetworkBoardHealthEntry> {
        let expected = self
            .boards
            .get("DAQ")
            .and_then(|r| r.launch_log.as_ref())
            .and_then(|_| crate::network_variables::persisted_daq_log_clock_with_revision());
        self.snapshot_with_launch_expected(now, test_fire, expected, true)
    }
    /// `status_without_health`: the DAQ SD and launch-log reports are separate
    /// packets from BOARD_HEALTH. Live views show them even before (or without)
    /// a board health report, as a version 0 report with only those fields.
    fn snapshot_with_launch_expected(
        &mut self,
        now: Instant,
        test_fire: bool,
        expected: Option<([u64; 2], u64)>,
        status_without_health: bool,
    ) -> Vec<NetworkBoardHealthEntry> {
        let mut rows = Vec::new();
        for (sender, r) in &mut self.boards {
            r.advance(now);
            if !r.visible(sender, test_fire) {
                continue;
            }
            let age = elapsed(now, r.last);
            let outage = if r.healthy {
                0
            } else {
                age.saturating_sub(r.timeout_ms())
            };
            rows.push(NetworkBoardHealthEntry {
                cpu_temperature_centi_c: None,
                cpu_temperature_age_ms: 0,
                diagnostics: match &r.diagnostics {
                    Some((report, received)) => {
                        let mut report = report.clone();
                        report.received_age_ms = elapsed(now, *received);
                        Some(report)
                    }
                    None if status_without_health
                        && (r.sd_status.is_some() || r.launch_log.is_some()) =>
                    {
                        Some(BoardDiagnostics::default())
                    }
                    None => None,
                }
                .map(|mut report| {
                    if let Some((status, received)) = &r.sd_status {
                        status.overlay(&mut report, elapsed(now, *received));
                    }
                    if let Some((status, received, initial_age)) = &r.launch_log {
                        report.launch_log = Some(status.project(
                            initial_age.saturating_add(elapsed(now, *received)),
                            expected,
                        ));
                    }
                    report
                }),
                sender_id: sender.clone(),
                healthy: r.healthy,
                age_ms: age,
                last_seen_ms: r.last_seen_ms,
                disconnects: r.disconnects,
                reconnects: r.reconnects,
                rx_observations: r.observations,
                observed_ms: elapsed(now, r.first),
                offline_ms: r.offline_ms.saturating_add(outage),
                longest_outage_ms: r.longest_ms.max(outage),
                current_outage_ms: outage,
                clock_offset_ms: r
                    .clock
                    .filter(|(_, at)| elapsed(now, *at) <= TIMEOUT_MS)
                    .map(|(offset, _)| offset),
            });
        }
        rows.sort_by(|a, b| a.sender_id.cmp(&b.sender_id));
        rows
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn at(t: Instant, ms: u64) -> Instant {
        t + Duration::from_millis(ms)
    }
    /// A silent board is shown offline after a timeout set by its link, not a flat 12 s (fullnet: a hung FC
    /// read healthy for 12 s). Umbilical (CAN) boards: 3 s; radio boards: 6 s, above the longest frame gap seen
    /// at 30% radio loss (RF 4.3 s, PB 2.8 s, FC 1.3 s); a board whose link is not known yet keeps 12 s.
    #[test]
    fn a_silent_board_goes_offline_after_its_link_timeout() {
        let t = Instant::now();
        let mut tracker = Tracker::default();
        for b in ["VB", "FC", "XX"] { tracker.observe(b, t, 100_000); }
        tracker.membership("VB", "umbilical_comms");
        tracker.membership("FC", "rocket_comms");
        let healthy = |tracker: &mut Tracker, b: &str, ms| tracker.snapshot(at(t, ms), false).into_iter()
            .find(|row| row.sender_id == b).map(|row| row.healthy).unwrap();
        assert!(tracker.connected_for("FC", at(t, 5_900)).is_some());
        assert!(healthy(&mut tracker, "VB", 2_900));
        assert!(!healthy(&mut tracker, "VB", 3_100));
        assert!(healthy(&mut tracker, "FC", 5_900));
        assert!(!healthy(&mut tracker, "FC", 6_100));
        assert!(healthy(&mut tracker, "XX", 11_900));
        assert!(!healthy(&mut tracker, "XX", 12_100));
        assert!(tracker.connected_for("VB", at(t, 3_100)).is_none());
    }
    #[test]
    fn clock_offset_ignores_transport_delay_within_its_window() {
        let t = Instant::now();
        let mut tracker = Tracker::default();
        let offset = |tracker: &mut Tracker, now| {
            tracker
                .snapshot(now, false)
                .into_iter()
                .find(|row| row.sender_id == "RF")
                .and_then(|row| row.clock_offset_ms)
        };
        // A packet that arrives promptly shows the real 20 ms offset; one that
        // sat 30 s in a radio queue does not replace it.
        tracker.observe("RF", t, 100_000);
        tracker.observe_clock("RF", 100_020, 100_000, t);
        tracker.observe("RF", at(t, 2_000), 102_000);
        tracker.observe_clock("RF", 102_000 - 30_000, 102_000, at(t, 2_000));
        assert_eq!(offset(&mut tracker, at(t, 2_000)), Some(20));
        // Once the prompt sample ages out, the newest sample is the estimate,
        // so a clock that really is behind still shows.
        tracker.observe("RF", at(t, 11_000), 111_000);
        tracker.observe_clock("RF", 111_000 - 30_000, 111_000, at(t, 11_000));
        assert_eq!(offset(&mut tracker, at(t, 11_000)), Some(-30_000));
    }
    #[test]
    fn rf_tx_timing_block_is_tagged_and_never_shadows_sd_or_adc_words() {
        let mut words = [0_u32; 24];
        words[0] = 1;
        words[4] = 1000;
        words[1] = 1 | 2 | 8;
        assert_eq!(decode_diagnostics(&words).unwrap().rf_tx_timing, None); // older RF
        words[16..22].copy_from_slice(&[0x3154_4652, 7, 90, 2400, 11, 3]);
        let decoded = decode_diagnostics(&words).unwrap();
        assert_eq!(
            decoded.rf_tx_timing,
            Some(RfTxTiming {
                ack_frames: 7,
                ack_mean_ms: 90,
                ack_max_ms: 2400,
                queue_high_water: 11,
                stale_drops: 3,
            })
        );
        assert_eq!(decoded.sd_accepted_records, None);
        assert_eq!(decoded.sd_max_write_ms, None);
        assert_eq!(decoded.adc_overruns, None);
        for flag in [16, 32, 128] {
            words[1] = 1 | flag;
            assert_eq!(decode_diagnostics(&words).unwrap().rf_tx_timing, None);
        }
        words[1] = 1;
        words[16] = 0x3154_4653;
        assert_eq!(decode_diagnostics(&words).unwrap().rf_tx_timing, None);
    }
    #[test]
    fn service_faults_share_word_six_without_changing_the_watchdog_mask() {
        let mut words = [0u32; 24];
        words[0] = 1;
        words[4] = 1000;
        words[6] = 5;
        let d = decode_diagnostics(&words).unwrap();
        assert_eq!(d.watchdog_missing_mask, 5);
        assert!(d.service_faults.is_empty());
        words[6] = 5 | (0x0a << 16);
        let d = decode_diagnostics(&words).unwrap();
        assert_eq!(d.watchdog_missing_mask, 5);
        assert_eq!(d.service_faults, ["OTA storage unavailable", "Flight-state cache unavailable"]);
        words[6] = 0x80 << 16;
        assert_eq!(decode_diagnostics(&words).unwrap().service_faults, ["Service 7 unavailable"]);
        let json = serde_json::to_value(decode_diagnostics(&[1, 0, 0, 0, 1000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap()).unwrap();
        assert!(json.get("service_faults").is_none(), "omitted when every service started");
    }

    #[test]
    fn signed_temperature_extension_preserves_legacy_diagnostics() {
        let mut words = [0_u32; 24];
        words[0] = 1;
        words[4] = 1000;
        words[1] = 0xff;
        words[7] = 12345;
        words[23] = 42;
        let old = decode_diagnostics(&words).unwrap();
        assert_eq!(old.die_temperature_centi_c, None);
        words[1] |= (u32::from((-1234_i16) as u16) << 16) | 0x100 | (2 << 9);
        let mut extended = decode_diagnostics(&words).unwrap();
        assert_eq!(extended.die_temperature_centi_c, Some(-1234));
        assert_eq!(extended.die_temperature_sample_age_ms, 200);
        extended.die_temperature_centi_c = None;
        extended.die_temperature_sample_age_ms = 0;
        assert_eq!(old, extended); // every prior counter survives bit-for-bit
        words[1] = (u32::from(30000_u16) << 16) | 0x100;
        assert_eq!(
            decode_diagnostics(&words).unwrap().die_temperature_centi_c,
            None
        );
    }
    #[test]
    fn dynamic_rows_count_outages_once_and_survive_snapshot_reload() {
        let mut h = Tracker::default();
        let t = Instant::now();
        assert!(h.snapshot(t, false).is_empty());
        h.observe("NEW_BOARD", t, 100);
        assert_eq!(h.snapshot(t, false)[0].reconnects, 0);
        let row = h.snapshot(at(t, 15_000), false).remove(0);
        assert!(!row.healthy);
        assert_eq!(row.disconnects, 1);
        assert_eq!(row.current_outage_ms, 3000);
        assert_eq!(h.snapshot(at(t, 16_000), false)[0].disconnects, 1);
        h.observe("NEW_BOARD", at(t, 17_000), 1); // UTC can move backwards.
        h.observe("NEW_BOARD", at(t, 18_000), 2);
        let row = h.snapshot(at(t, 18_000), false).remove(0);
        assert!(row.healthy);
        assert_eq!(row.reconnects, 1);
        assert_eq!(row.offline_ms, 5000);
    }
    #[test]
    fn gap_is_detected_even_without_an_open_dashboard() {
        let mut h = Tracker::default();
        let t = Instant::now();
        h.observe("GW", t, 0);
        h.observe("GB", at(t, 20_000), 20_000);
        let rows = h.snapshot(at(t, 20_000), false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].disconnects, 1);
        assert_eq!(rows[0].reconnects, 1);
    }
    #[test]
    fn cached_discovery_does_not_fake_liveness_or_reconnection() {
        let mut h = Tracker::default();
        let t = Instant::now();
        h.announcement("RF", "rocket_comms", 1, 0, t, 100);
        h.announcement("RF", "rocket_comms", 1, 0, at(t, 20_000), 20_100);
        assert!(!h.snapshot(at(t, 20_000), false)[0].healthy);
        h.announcement("RF", "rocket_comms", 2, 0, at(t, 21_000), 21_100);
        assert_eq!(h.snapshot(at(t, 21_000), false)[0].reconnects, 1);
        h.announcement("GHOST", "rocket_comms", 1, 13_000, t, 100);
        assert_eq!(h.snapshot(at(t, 21_000), false).len(), 1);
    }
    #[test]
    fn test_fire_filters_av_bay_and_dynamic_rocket_only_senders() {
        let mut h = Tracker::default();
        let t = Instant::now();
        for name in ["FC", "RF", "PB", "RRLS", "DAQB", "VB", "EXTRA", "SHARED"] {
            h.observe(name, t, 0);
        }
        h.membership("EXTRA", "rocket_comms");
        h.membership("SHARED", "rocket_comms");
        h.membership("SHARED", "umbilical_comms");
        let names: Vec<_> = h
            .snapshot(t, true)
            .into_iter()
            .map(|r| r.sender_id)
            .collect();
        assert_eq!(names, ["DAQ", "SHARED", "VB"]);
        assert_eq!(h.snapshot(t, false).len(), 8);
        h.clear();
        assert!(h.snapshot(t, false).is_empty());
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    fn at(t: Instant, ms: u64) -> Instant {
        t + std::time::Duration::from_millis(ms)
    }
    #[test]
    fn exact_uptime_optional_capabilities_and_invalid_versions() {
        let mut words = [0; 24];
        words[0] = 1;
        words[2] = u32::MAX;
        words[3] = 7;
        words[4] = 1000;
        words[7] = 123;
        let report = decode_diagnostics(&words).unwrap();
        assert_eq!(report.uptime_ticks, (7u64 << 32) | u64::from(u32::MAX));
        assert_eq!(report.heap_live_bytes, None);
        assert_eq!(report.can_rx_drops, None);
        words[1] = 1 | 2 | 16;
        let report = decode_diagnostics(&words).unwrap();
        assert_eq!(report.heap_live_bytes, Some(123));
        assert_eq!(report.can_rx_drops, Some(0));
        assert_eq!(report.sd_flushed_records, Some(0));
        words[0] = 2;
        assert!(decode_diagnostics(&words).is_none());
        words[0] = 1;
        words[4] = 0;
        assert!(decode_diagnostics(&words).is_none());
        assert!(decode_diagnostics(&[]).is_none());
    }
    #[test]
    fn diagnostics_age_separately_and_do_not_fake_traffic_or_hide_modes() {
        let mut tracker = Tracker::default();
        let at = Instant::now();
        tracker.observe("FC", at, 1000);
        let mut words = [0; 24];
        words[0] = 1;
        words[2] = 1234;
        words[4] = 1000;
        assert!(tracker.observe_diagnostics("FC", &words, at, 1000));
        let rows = tracker.snapshot(at + std::time::Duration::from_secs(16), false);
        assert!(!rows[0].healthy);
        assert_eq!(rows[0].diagnostics.as_ref().unwrap().received_age_ms, 16000);
        assert!(tracker.snapshot(at, true).is_empty());
    }
    #[test]
    fn launch_log_companion_requires_daq_and_ages_independently_of_all_other_health() {
        let mut tracker = Tracker::default();
        let now = Instant::now();
        let mut base = [0; 24];
        base[0] = 1;
        base[4] = 1000;
        let launch = [42, 0, 20_000, 0, 10, 0, 7];
        let expected = Some(([42, 20_000], 10));
        assert!(!tracker.observe_launch_log_status("FC", &launch, now, 1000, 0, 0));
        assert!(!tracker.observe_launch_log_status("@123", &launch, now, 1000, 0, 0));
        assert!(tracker.observe_diagnostics("DAQ", &base, now, 1000));
        assert!(tracker.observe_launch_log_status("DAQB", &launch, at(now, 100), 1100, 0, 0));
        let projected = |tracker: &mut Tracker, time, expected| {
            tracker
                .snapshot_with_launch_expected(time, false, expected, true)
                .remove(0)
                .diagnostics
                .unwrap()
                .launch_log
                .unwrap()
        };
        let initial = projected(&mut tracker, at(now, 500), expected);
        assert_eq!(initial.received_age_ms, 400);
        assert_eq!(initial.matches_expected, Some(true));
        assert_eq!(
            initial.flags & (16 | 32),
            0,
            "matching clock is not evidence of open files"
        );
        assert!(tracker.observe_diagnostics("DAQ", &base, at(now, 1000), 2000));
        assert!(tracker.observe_sd_status("DAQ", &[3, 0, 0, 0, 0, 0, 0, 0], at(now, 1200), 2200));
        let stale = projected(&mut tracker, at(now, 13_000), expected);
        assert_eq!(stale.received_age_ms, 12_900);
        assert_eq!(stale.session, 42);
        assert!(!tracker.observe_launch_log_status("DAQ", &[0; 6], at(now, 14_000), 15_000, 0, 0));
        assert_eq!(
            projected(&mut tracker, at(now, 14_000), Some(([0, 0], 11))).matches_expected,
            Some(false)
        );
        let reset = [0, 0, 0, 0, 11, 0, 1];
        assert!(tracker.observe_launch_log_status("DAQ", &reset, at(now, 15_000), 16_000, 0, 0));
        let accepted_reset = projected(&mut tracker, at(now, 15_100), Some(([0, 0], 11)));
        assert_eq!(accepted_reset.received_age_ms, 100);
        assert_eq!(accepted_reset.matches_expected, Some(true));
        assert_eq!(accepted_reset.session, 0);
        let utc = 1_798_000_000_000;
        assert!(tracker.observe_launch_log_status(
            "DAQ",
            &reset,
            at(now, 16_000),
            17_000,
            utc - 30_000,
            utc
        ));
        let delayed = projected(&mut tracker, at(now, 16_500), Some(([0, 0], 11)));
        assert_eq!(delayed.received_age_ms, 30_500);
        assert_eq!(
            delayed.matches_expected,
            Some(true),
            "exact match never hides transport age"
        );
        assert!(!tracker.observe_launch_log_status(
            "DAQ",
            &launch,
            at(now, 17_000),
            18_000,
            utc + 101,
            utc
        ));
        assert_eq!(
            projected(&mut tracker, at(now, 17_000), Some(([0, 0], 11))).received_age_ms,
            31_000
        );
        tracker.clear();
        assert!(
            tracker
                .snapshot_with_launch_expected(at(now, 16_000), false, expected, true)
                .is_empty()
        );
    }
    #[test]
    fn sd_companion_has_independent_freshness_and_cannot_replace_base_health() {
        let mut tracker = Tracker::default();
        let now = Instant::now();
        let mut base = [0; 24];
        base[0] = 1;
        base[4] = 1000;
        base[2] = 123;
        assert!(tracker.observe_diagnostics("DAQ", &base, now, 1000));
        let sd = [3, u32::MAX, 10, 5, 2, 42, 41, 1];
        assert!(!tracker.observe_sd_status("FC", &sd, now, 1000));
        assert!(tracker.observe_sd_status("DAQ", &sd, at(now, 100), 1100));
        let row = tracker.snapshot(at(now, 500), false).remove(0);
        let report = row.diagnostics.unwrap();
        assert_eq!(report.version, 1);
        assert_eq!(report.uptime_ticks, 123);
        assert_eq!(report.received_age_ms, 500);
        assert_eq!(report.sd_status_received_age_ms, Some(400));
        assert_eq!(report.sd_last_write_age_ms, None);
        assert_eq!(report.sd_last_flush_age_ms, Some(10));
        assert_eq!(
            row.rx_observations, 0,
            "diagnostics do not fabricate ordinary traffic"
        );
        base[2] = 456;
        assert!(tracker.observe_diagnostics("DAQ", &base, at(now, 900), 1900));
        let report = tracker
            .snapshot(at(now, 1000), false)
            .remove(0)
            .diagnostics
            .unwrap();
        assert_eq!(report.uptime_ticks, 456);
        assert_eq!(report.received_age_ms, 100);
        assert_eq!(report.sd_status_received_age_ms, Some(900));
        assert_eq!(report.sd_retry_requested_token, Some(42));
        assert!(!tracker.observe_sd_status("DAQ", &[0; 7], at(now, 1100), 2100));
        assert_eq!(tracker.daq_sd_status(at(now, 13_000)).unwrap().1, 12_900);
        assert!(
            decode_diagnostics(&[0; 32]).is_none(),
            "BOARD_HEALTH remains exactly24 words/version1"
        );
    }

    #[test]
    fn daq_recording_status_is_published_before_and_without_a_board_health_report() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.observe("DAQ", now, 1000);
        assert!(tracker.observe_sd_status("DAQ", &[7, 0, 0, 0, 0, 0, 0, 0], at(now, 100), 1100));
        let rows = tracker.snapshot(at(now, 200), false);
        let report = rows[0]
            .diagnostics
            .as_ref()
            .expect("SD status reaches the operator without BOARD_HEALTH");
        assert_eq!(report.version, 0, "version 0 marks a report without board health");
        assert_eq!(report.sd_flags, Some(7));
        assert_eq!(report.sd_status_received_age_ms, Some(100));
        assert!(
            tracker.recording_snapshot(at(now, 200))[0].diagnostics.is_none(),
            "recordings never store a board health report the board did not send"
        );
        let mut base = [0_u32; 24];
        base[0] = 1;
        base[4] = 1000;
        assert!(tracker.observe_diagnostics("DAQ", &base, at(now, 300), 1300));
        let report = tracker.snapshot(at(now, 400), false)[0].diagnostics.clone().unwrap();
        assert_eq!(report.version, 1);
        assert_eq!(report.sd_flags, Some(7));
    }
}
