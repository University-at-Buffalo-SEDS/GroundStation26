//! Diagnostic liveness history. Silence is not evidence of a reboot or a CPU fault.
use crate::types::{NetworkBoardHealthEntry, canonical_sender_id};
use std::{collections::HashMap, time::Instant};
pub const TIMEOUT_MS: u64 = 12_000;
#[derive(Default)]
pub struct Tracker {
    boards: HashMap<String, Record>,
}
struct Record {
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
impl Tracker {
    fn record(&mut self, sender: &str, now: Instant, wall_ms: u64) -> &mut Record {
        // Allocate sender storage only on first observation, not on every sample.
        if !self.boards.contains_key(sender) {
            self.boards.insert(
                sender.to_owned(),
                Record {
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
