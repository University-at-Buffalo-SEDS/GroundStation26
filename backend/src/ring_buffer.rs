use std::collections::VecDeque;

// Status decisions are fixed at ingress. AppliedStatus carries either a live
// VALVE_STATE snapshot or an unconfirmed raw-status snapshot with no values.
// The bulk worker only persists it; discovery/recording cannot re-admit it.
pub enum ReceivedPacket {
    Telemetry {
        packet: sedsnet::packet::Packet,
        ingress_generation: u64,
    },
    AppliedStatus {
        packet: sedsnet::packet::Packet,
        row: crate::types::TelemetryRow,
    },
}

/// Ingress ring counters. `dropped` and `high_water` are cumulative for the
/// process: `clear` (a recording reset) empties the ring but keeps them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct RingStats {
    pub capacity: usize,
    pub len: usize,
    pub high_water: usize,
    pub dropped: u64,
}

pub struct RingBuffer<T> {
    max: usize,
    buf: VecDeque<T>,
    dropped: u64,
    high_water: usize,
}

impl<T> RingBuffer<T> {
    pub fn new(max: usize) -> Self {
        Self {
            max,
            buf: VecDeque::with_capacity(max),
            dropped: 0,
            high_water: 0,
        }
    }

    /// Appends `item`; when the ring is full the oldest entry is discarded and
    /// counted. Returns true when an entry was dropped.
    pub fn push(&mut self, item: impl Into<T>) -> bool {
        let item = item.into();
        let dropped = self.buf.len() >= self.max;
        if dropped {
            self.buf.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.buf.push_back(item);
        self.high_water = self.high_water.max(self.buf.len());
        dropped
    }

    #[allow(dead_code)]
    pub fn recent(&self, n: usize) -> Vec<&T> {
        self.buf.iter().rev().take(n).collect()
    }

    pub fn pop_oldest(&mut self) -> Option<T> {
        self.buf.pop_front()
    }
    #[allow(dead_code)]
    pub fn pop_newest(&mut self) -> Option<T> {
        self.buf.pop_back()
    }
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn stats(&self) -> RingStats {
        RingStats {
            capacity: self.max,
            len: self.buf.len(),
            high_water: self.high_water,
            dropped: self.dropped,
        }
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// Every packet the ingest takes out of the ring, kept for the safety task's
/// value-range checks. The ingest drains the ring continuously, so a snapshot
/// of the ring every safety cycle saw only the few packets still queued at that
/// instant and missed almost all telemetry (a PB pack at 5.6 V never warned).
/// Bounded: when no safety task drains it (HITL, test fire) the oldest go.
pub struct SafetyTap {
    max: usize,
    buf: VecDeque<sedsnet::packet::Packet>,
}

impl Default for SafetyTap {
    fn default() -> Self {
        Self::new(SAFETY_TAP_CAPACITY)
    }
}

/// Packets kept between two safety cycles (500 ms) at most.
pub const SAFETY_TAP_CAPACITY: usize = 16_384;

impl SafetyTap {
    pub fn new(max: usize) -> Self {
        Self { max: max.max(1), buf: VecDeque::new() }
    }

    /// Records the packet of an entry the ingest took from the ring.
    pub fn record(&mut self, received: &ReceivedPacket) {
        if self.buf.len() >= self.max {
            self.buf.pop_front();
        }
        let packet = match received {
            ReceivedPacket::Telemetry { packet, .. } | ReceivedPacket::AppliedStatus { packet, .. } => packet,
        };
        self.buf.push_back(packet.clone());
    }

    /// Everything recorded since the last call, oldest first.
    pub fn take(&mut self) -> Vec<sedsnet::packet::Packet> {
        self.buf.drain(..).collect()
    }
}

/// Minimum spacing of the operator warning for ingress ring overflow.
pub const INGRESS_DROP_WARN_INTERVAL_MS: u64 = 30_000;

/// Turns the cumulative drop counter into a rate-limited operator warning: the
/// first loss warns at once, further losses at most every
/// `INGRESS_DROP_WARN_INTERVAL_MS`, each naming the packets lost since the
/// previous warning. Nothing is lost from the count while a warning is held.
#[derive(Debug, Default)]
pub struct DropWarning {
    reported: u64,
    last_warn_ms: Option<u64>,
}

impl DropWarning {
    pub fn observe(&mut self, dropped: u64, now_ms: u64) -> Option<String> {
        if dropped <= self.reported {
            return None;
        }
        if self
            .last_warn_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < INGRESS_DROP_WARN_INTERVAL_MS)
        {
            return None;
        }
        let new = dropped - self.reported;
        self.reported = dropped;
        self.last_warn_ms = Some(now_ms);
        Some(format!(
            "Warning: telemetry ingress ring full; {new} oldest packet(s) dropped unrecorded ({dropped} since start)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_safety_tap_keeps_the_newest_packets_up_to_its_bound() {
        crate::telemetry_schema::initialize().unwrap();
        let mut tap = SafetyTap::new(2);
        for at in 1..=3u64 {
            tap.record(&ReceivedPacket::Telemetry {
                packet: sedsnet::packet::Packet::new(
                    crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                    "PB",
                    at,
                    std::sync::Arc::<[u8]>::from(7.4f32.to_le_bytes().to_vec()),
                )
                .unwrap(),
                ingress_generation: 0,
            });
        }
        assert_eq!(tap.take().iter().map(|p| p.timestamp()).collect::<Vec<_>>(), [2, 3]);
        assert!(tap.take().is_empty());
    }

    #[test]
    fn full_ring_counts_each_dropped_packet() {
        let mut ring = RingBuffer::<u32>::new(3);
        assert!(!ring.push(1u32));
        assert!(!ring.push(2u32));
        assert!(!ring.push(3u32));
        assert_eq!(ring.dropped(), 0);
        assert!(ring.push(4u32));
        assert!(ring.push(5u32));
        assert_eq!(ring.pop_oldest(), Some(3));
        assert_eq!(
            ring.stats(),
            RingStats {
                capacity: 3,
                len: 2,
                high_water: 3,
                dropped: 2
            }
        );
        // A reset empties the ring but keeps the process counters.
        ring.clear();
        assert_eq!(ring.stats().dropped, 2);
        assert_eq!(ring.stats().high_water, 3);
        assert_eq!(ring.stats().len, 0);
    }

    #[test]
    fn drop_warning_is_immediate_then_rate_limited_and_lossless() {
        let mut warn = DropWarning::default();
        assert_eq!(warn.observe(0, 1_000), None);
        let first = warn.observe(5, 1_000).expect("first loss warns at once");
        assert!(first.contains("5 oldest packet(s)"), "{first}");
        assert_eq!(
            warn.observe(9, 1_000 + INGRESS_DROP_WARN_INTERVAL_MS - 1),
            None
        );
        let second = warn
            .observe(12, 1_000 + INGRESS_DROP_WARN_INTERVAL_MS)
            .expect("warns again after the interval");
        assert!(second.contains("7 oldest packet(s)"), "{second}");
        assert!(second.contains("12 since start"), "{second}");
        // No new loss, no warning, however long it has been.
        assert_eq!(warn.observe(12, u64::MAX), None);
    }
}
