//! Raw-UART radio receive quality: good frames and CRC-rejected frames.
//!
//! Counted by the raw-UART frame parser for every link it serves (the AV-bay
//! radio on GS and RRLS). Kept as one-second buckets over the last minute plus
//! monotonic totals; no allocation after start-up.
use serde::Serialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const BUCKETS: usize = 60;

#[derive(Clone, Copy, Default)]
struct Bucket {
    second: u64,
    good: u32,
    crc: u32,
}

struct Quality {
    epoch: Option<Instant>,
    buckets: [Bucket; BUCKETS],
    good_total: u64,
    crc_total: u64,
    /// Totals at the last log summary.
    logged: (u64, u64, Option<Instant>),
}

const EMPTY: Quality = Quality {
    epoch: None,
    buckets: [Bucket {
        second: 0,
        good: 0,
        crc: 0,
    }; BUCKETS],
    good_total: 0,
    crc_total: 0,
    logged: (0, 0, None),
};
static QUALITY: Mutex<Quality> = Mutex::new(EMPTY);

impl Quality {
    fn second(&mut self, now: Instant) -> u64 {
        let epoch = *self.epoch.get_or_insert(now);
        now.saturating_duration_since(epoch).as_secs() + 1
    }
    fn record(&mut self, now: Instant, good: u32, crc: u32) {
        let s = self.second(now);
        let bucket = &mut self.buckets[(s % BUCKETS as u64) as usize];
        if bucket.second != s {
            *bucket = Bucket {
                second: s,
                good: 0,
                crc: 0,
            };
        }
        bucket.good = bucket.good.saturating_add(good);
        bucket.crc = bucket.crc.saturating_add(crc);
        self.good_total = self.good_total.saturating_add(u64::from(good));
        self.crc_total = self.crc_total.saturating_add(u64::from(crc));
    }
    fn snapshot(&mut self, now: Instant) -> Snapshot {
        let s = self.second(now);
        let mut out = Snapshot {
            good_frames_total: self.good_total,
            crc_failures_total: self.crc_total,
            ..Default::default()
        };
        for bucket in &self.buckets {
            // The current second and the 59 before it.
            if bucket.second != 0 && bucket.second + (BUCKETS as u64) > s {
                out.good_frames_last_minute += u64::from(bucket.good);
                out.crc_failures_last_minute += u64::from(bucket.crc);
            }
        }
        out
    }
    fn summary(&mut self, now: Instant, interval: Duration) -> Option<String> {
        let (good_then, crc_then, at) = self.logged;
        if at.is_some_and(|at| now.saturating_duration_since(at) < interval) {
            return None;
        }
        let crc = self.crc_total - crc_then;
        if crc == 0 {
            return None;
        }
        let good = self.good_total - good_then;
        let span = at.map_or(interval, |at| now.saturating_duration_since(at));
        self.logged = (self.good_total, self.crc_total, Some(now));
        Some(format!(
            "{crc} raw UART CRC mismatches in the last {} s, {good} good frames ({}% rejected)",
            span.as_secs().max(1),
            crc * 100 / (crc + good).max(1)
        ))
    }
}

fn with<T>(f: impl FnOnce(&mut Quality) -> T) -> T {
    f(&mut QUALITY.lock().unwrap_or_else(|e| e.into_inner()))
}

/// A raw-UART frame passed its integrity check (or needed none) and was taken.
pub(crate) fn good_frame() {
    with(|q| q.record(Instant::now(), 1, 0));
}

/// A raw-UART frame failed its inner CRC32 and was rejected.
pub(crate) fn crc_failure() {
    with(|q| q.record(Instant::now(), 0, 1));
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub good_frames_last_minute: u64,
    pub crc_failures_last_minute: u64,
    pub good_frames_total: u64,
    pub crc_failures_total: u64,
}

pub(crate) fn snapshot() -> Snapshot {
    with(|q| q.snapshot(Instant::now()))
}

/// One summary line per `interval` while CRC failures occur, instead of one
/// line per rejected frame: "N CRC mismatches in the last 30 s, M good frames".
pub(crate) fn crc_summary(interval: Duration) -> Option<String> {
    with(|q| q.summary(Instant::now(), interval))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minute_window_totals_and_summary() {
        let mut q = EMPTY;
        let t0 = Instant::now();
        for i in 0..10 {
            q.record(t0 + Duration::from_secs(i), 3, 1);
        }
        let s = q.snapshot(t0 + Duration::from_secs(9));
        assert_eq!((s.good_frames_last_minute, s.crc_failures_last_minute), (30, 10));
        // Buckets older than a minute leave the window but stay in the totals.
        let s = q.snapshot(t0 + Duration::from_secs(65));
        assert_eq!((s.good_frames_last_minute, s.crc_failures_last_minute), (12, 4));
        assert_eq!((s.good_frames_total, s.crc_failures_total), (30, 10));
        let line = q.summary(t0 + Duration::from_secs(10), Duration::from_secs(30)).unwrap();
        assert!(
            line.starts_with("10 raw UART CRC mismatches in the last 30 s, 30 good frames (25% rejected)"),
            "{line}"
        );
        // Rate-limited, and silent when nothing failed.
        q.record(t0 + Duration::from_secs(11), 0, 1);
        assert!(q.summary(t0 + Duration::from_secs(20), Duration::from_secs(30)).is_none());
        let line = q.summary(t0 + Duration::from_secs(41), Duration::from_secs(30)).unwrap();
        assert!(line.starts_with("1 raw UART CRC mismatches in the last 31 s, 0 good frames"), "{line}");
        q.record(t0 + Duration::from_secs(42), 5, 0);
        assert!(q.summary(t0 + Duration::from_secs(80), Duration::from_secs(30)).is_none());
    }
}
