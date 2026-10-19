//! Diagnostic liveness history. Silence is not evidence of a reboot or a CPU fault.
use crate::types::{BoardDiagnostics, NetworkBoardHealthEntry, canonical_sender_id};
use std::{collections::HashMap, time::Instant};
pub const TIMEOUT_MS: u64 = 12_000;
#[derive(Default)]
pub struct Tracker {
    boards: HashMap<String, Record>,
}
struct Record {
    diagnostics: Option<(BoardDiagnostics, Instant)>,
    first: Instant,
    last: Instant,
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
}
fn elapsed(now: Instant, then: Instant) -> u64 {
    now.saturating_duration_since(then)
        .as_millis()
        .min(u64::MAX as u128) as u64
}
impl Record {
    fn advance(&mut self, now: Instant) {
        if self.healthy && elapsed(now, self.last) > TIMEOUT_MS {
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
            let outage = elapsed(now, self.last).saturating_sub(TIMEOUT_MS);
            self.offline_ms = self.offline_ms.saturating_add(outage);
            self.longest_ms = self.longest_ms.max(outage);
            self.reconnects = self.reconnects.saturating_add(1);
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
        version: w[0],
        received_age_ms: 0,
        uptime_ticks: u64::from(w[2]) | (u64::from(w[3]) << 32),
        uptime_frequency_hz: w[4],
        reset_flags: w[5],
        watchdog_missing_mask: w[6],
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
    })
}

impl Tracker {
    fn record(&mut self, sender: &str, now: Instant, wall_ms: u64) -> &mut Record {
        // Allocate sender storage only on first observation, not on every sample.
        if !self.boards.contains_key(sender) {
            self.boards.insert(
                sender.to_owned(),
                Record {
                    diagnostics: None,
                    first: now,
                    last: now,
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
        self.record(&sender, now, wall_ms).diagnostics = Some((report, now));
        true
    }
    pub fn observe(&mut self, sender: &str, now: Instant, wall_ms: u64) {
        let sender = canonical_sender_id(sender.trim());
        if sender.is_empty() || sender == "GS" || sender.starts_with('@') {
            return;
        }
        self.record(sender, now, wall_ms)
            .observe(now, wall_ms, true);
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
    pub fn snapshot(&mut self, now: Instant, test_fire: bool) -> Vec<NetworkBoardHealthEntry> {
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
                age.saturating_sub(TIMEOUT_MS)
            };
            rows.push(NetworkBoardHealthEntry {
                diagnostics: r.diagnostics.as_ref().map(|(report, received)| {
                    let mut report = report.clone();
                    report.received_age_ms = elapsed(now, *received);
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
}
