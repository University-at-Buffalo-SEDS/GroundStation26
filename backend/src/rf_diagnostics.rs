//! RF application diagnostics; no deployment commands or router mutations.
use serde_json::{Value, json};
use std::{
    sync::{Mutex, OnceLock},
    time::Instant,
};
const FIELDS: [&str; 32] = [
    "version",
    "uptime_ms",
    "reset_flags",
    "watchdog_feeds",
    "loop_completions",
    "radio_receive_failures",
    "radio_receive_last",
    "can_receive_failures",
    "can_receive_last",
    "queue_failures",
    "queue_last",
    "radio_frames",
    "radio_isr_drops",
    "radio_bad_length",
    "radio_errors",
    "radio_restart_errors",
    "radio_tx",
    "radio_tx_errors",
    "radio_tx_busy",
    "radio_tx_drops",
    "radio_tx_queued",
    "radio_dma_recoveries",
    "can_pending",
    "can_admission_failures",
    "underglow_updates",
    "underglow_persist_errors",
    "underglow_enabled",
    "underglow_persist_writes",
    "discovery_seen",
    "radio_seen",
    "radio_rx_bytes",
    "radio_sync_loss",
];
#[derive(Default)]
struct Samples {
    latest: Option<([u32; 32], Instant)>,
    observed_restarts: u64,
}
impl Samples {
    fn observe(&mut self, payload: &[u8], now: Instant) {
        if payload.len() != 128 {
            return;
        }
        let words: [u32; 32] = std::array::from_fn(|i| {
            u32::from_le_bytes(payload[i * 4..i * 4 + 4].try_into().unwrap())
        });
        if words[0] != 1 {
            return;
        }
        if let Some((previous, _)) = self.latest {
            // A normal u32 uptime wrap advances by a small wrapping difference.
            if words[1].wrapping_sub(previous[1]) > i32::MAX as u32 {
                self.observed_restarts += 1;
                log::warn!(
                    "RF uptime moved backwards; observed restart; reset_flags={:#x}",
                    words[2]
                );
            }
        }
        self.latest = Some((words, now));
    }
    fn snapshot(&self) -> Value {
        let Some((words, at)) = self.latest else {
            return Value::Null;
        };
        let mut out = serde_json::Map::new();
        for (i, name) in FIELDS.iter().enumerate() {
            let value = if matches!(i, 6 | 8 | 10) {
                json!(words[i] as i32)
            } else {
                json!(words[i])
            };
            out.insert((*name).into(), value);
        }
        out.insert("age_ms".into(), json!(at.elapsed().as_millis() as u64));
        out.insert("observed_restarts".into(), json!(self.observed_restarts));
        Value::Object(out)
    }
}
static SAMPLES: OnceLock<Mutex<Samples>> = OnceLock::new();
pub fn observe(payload: &[u8]) {
    SAMPLES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .observe(payload, Instant::now());
}
pub fn snapshot() -> Value {
    SAMPLES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .snapshot()
}
#[cfg(test)]
mod tests {
    use super::*;
    fn payload(uptime: u32, last: i32) -> Vec<u8> {
        let mut words = [0u32; 32];
        words[0] = 1;
        words[1] = uptime;
        words[6] = last as u32;
        words.into_iter().flat_map(u32::to_le_bytes).collect()
    }
    #[test]
    fn distinguishes_outage_from_restart_and_uptime_wrap() {
        let now = Instant::now();
        let mut samples = Samples::default();
        samples.observe(&payload(10000, -13), now);
        samples.observe(&payload(30000, 0), now);
        assert_eq!(samples.observed_restarts, 0);
        samples.observe(&payload(5000, -14), now);
        assert_eq!(samples.observed_restarts, 1);
        assert_eq!(samples.snapshot()["radio_receive_last"], -14);
        let mut wrap = Samples::default();
        wrap.observe(&payload(u32::MAX - 100, 0), now);
        wrap.observe(&payload(200, 0), now);
        assert_eq!(wrap.observed_restarts, 0);
    }
    #[test]
    fn malformed_or_unknown_version_does_not_replace_valid_sample() {
        let now = Instant::now();
        let mut samples = Samples::default();
        samples.observe(&payload(10000, 0), now);
        samples.observe(&[0; 127], now);
        samples.observe(&[0; 128], now);
        assert_eq!(samples.snapshot()["uptime_ms"], 10000);
    }
}
