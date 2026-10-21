//! Continuous simulator acceptance; a short burst must not qualify a link.
//! This observer never generates telemetry or changes flight/valve behavior.
const CHANNELS: usize = 5;
const WINDOW_MS: u64 = 10_000;
const WARMUP_MS: u64 = 20_000;
pub const NAMES: [&str; CHANNELS] = ["KG1000", "KG50", "VB_BATTERY", "VB_CURRENT", "VB_PRESSURE"];
// Loadcell target is 250 Hz; require at least 95% in each complete window.
// Slow sensors require fresh >=1 Hz delivery with bounded scheduler tolerance.
const MIN_MILLIHZ: [u64; CHANNELS] = [237_500, 237_500, 900, 900, 900];
const MAX_GAP_MS: [u64; CHANNELS] = [500, 500, 3_000, 3_000, 3_000];
/// Only the currently selected physical input can satisfy the logical stream.
pub fn selected_kg50(input: u8, native: bool, tagged: bool, values: &[f32]) -> bool {
    values.iter().all(|v| v.is_finite())
        && ((input == 0 && native && values.len() == 1)
            || (input != 0 && tagged && values.len() == 2 && values[0] == f32::from(input)))
}
#[derive(Clone, Copy)]
struct Channel {
    recent: [Option<u64>; 64],
    cursor: usize,
    last_received: Option<u64>,
    count: u64,
    max_gap: u64,
}
impl Default for Channel {
    fn default() -> Self {
        Self {
            recent: [None; 64],
            cursor: 0,
            last_received: None,
            count: 0,
            max_gap: 0,
        }
    }
}
pub struct Probe {
    window_start: u64,
    channels: [Channel; CHANNELS],
}
#[derive(Debug)]
pub struct Window {
    pub start_ms: u64,
    pub elapsed_ms: u64,
    pub received: [u64; CHANNELS],
    pub max_gap_ms: [u64; CHANNELS],
    pub passed: bool,
}
impl Default for Probe {
    fn default() -> Self {
        Self {
            window_start: WARMUP_MS,
            channels: [Channel::default(); CHANNELS],
        }
    }
}
impl Probe {
    pub fn observe(&mut self, channel: usize, packet_id: u64, now: u64) {
        let Some(c) = self.channels.get_mut(channel) else {
            return;
        };
        if c.recent.contains(&Some(packet_id)) {
            return;
        }
        c.recent[c.cursor] = Some(packet_id);
        c.cursor = (c.cursor + 1) % c.recent.len();
        if now >= self.window_start {
            c.max_gap = c.max_gap.max(
                now.saturating_sub(c.last_received.unwrap_or(self.window_start))
                    .min(now.saturating_sub(self.window_start)),
            );
            c.count += 1;
        }
        c.last_received = Some(now);
    }
    pub fn take_window(&mut self, now: u64) -> Option<Window> {
        let elapsed = now.saturating_sub(self.window_start);
        if elapsed < WINDOW_MS {
            return None;
        }
        let mut out = Window {
            start_ms: self.window_start,
            elapsed_ms: elapsed,
            received: [0; CHANNELS],
            max_gap_ms: [0; CHANNELS],
            passed: true,
        };
        for (i, c) in self.channels.iter_mut().enumerate() {
            let end_gap = now.saturating_sub(c.last_received.unwrap_or(self.window_start));
            out.received[i] = c.count;
            out.max_gap_ms[i] = c.max_gap.max(end_gap);
            out.passed &= c.count.saturating_mul(1_000_000)
                >= MIN_MILLIHZ[i].saturating_mul(elapsed)
                && out.max_gap_ms[i] <= MAX_GAP_MS[i];
            c.count = 0;
            c.max_gap = 0;
        }
        self.window_start = now;
        Some(out)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn healthy(probe: &mut Probe, start: u64, end: u64) {
        for now in (start..end).step_by(4) {
            for c in 0..2 {
                probe.observe(c, now, now);
            }
            if now % 1000 == 0 {
                for c in 2..CHANNELS {
                    probe.observe(c, now, now);
                }
            }
        }
    }
    #[test]
    fn unselected_adc_or_invalid_payload_cannot_inflate_p6_delivery() {
        assert!(!selected_kg50(1, true, false, &[0.01]));
        assert!(!selected_kg50(1, false, true, &[0.0, 0.01]));
        assert!(!selected_kg50(1, false, true, &[1.0, f32::NAN]));
        assert!(selected_kg50(1, false, true, &[1.0, 0.01]));
        assert!(!selected_kg50(0, false, true, &[1.0, 0.01]));
        assert!(selected_kg50(0, true, false, &[0.01]));
    }
    #[test]
    fn healthy_loadcells_cannot_hide_stalled_valve_reports() {
        let mut p = Probe::default();
        for now in (0..30000).step_by(4) {
            for c in 0..2 {
                p.observe(c, now, now);
            }
            if now % 1000 == 0 && now < 22000 {
                for c in 2..CHANNELS {
                    p.observe(c, now, now);
                }
            }
        }
        let w = p.take_window(30000).unwrap();
        assert_eq!(&w.received[..2], &[2500, 2500]);
        assert!(!w.passed);
        assert!(w.max_gap_ms[2..].iter().all(|gap| *gap >= 9000));
    }
    #[test]
    fn sustained_five_streams_pass_and_counts_reset_each_window() {
        let mut p = Probe::default();
        healthy(&mut p, 0, 30_000);
        let w = p.take_window(30_000).unwrap();
        assert!(w.passed);
        assert_eq!(w.received, [2500, 2500, 10, 10, 10]);
        healthy(&mut p, 30_000, 40_000);
        assert!(p.take_window(40_000).unwrap().passed);
    }
    #[test]
    fn five_fast_points_then_silence_cannot_qualify_250_hz() {
        let mut p = Probe::default();
        for now in (20_000..20_024).step_by(4) {
            p.observe(0, now, now);
        }
        let w = p.take_window(30_000).unwrap();
        assert!(!w.passed);
        assert_eq!(w.received[0], 6);
    }
    #[test]
    fn equal_total_bursts_still_fail_a_long_gap() {
        let mut p = Probe::default();
        for now in 20_000..22_500 {
            for c in 0..2 {
                p.observe(c, now, now);
            }
        }
        let w = p.take_window(30_000).unwrap();
        assert!(!w.passed);
        assert_eq!(w.received[0], 2500);
        assert!(w.max_gap_ms[0] > 7000);
    }
    #[test]
    fn no_first_sensor_report_is_a_failure_instead_of_uninitialized_success() {
        let mut p = Probe::default();
        assert!(!p.take_window(30_000).unwrap().passed);
    }
    #[test]
    fn duplicates_cannot_inflate_rate_or_keep_a_sensor_alive() {
        let mut p = Probe::default();
        for now in 20_000..30_000 {
            p.observe(0, 5, now);
        }
        let w = p.take_window(30_000).unwrap();
        assert_eq!(w.received[0], 1);
        assert_eq!(w.max_gap_ms[0], 10_000);
        assert!(!w.passed);
    }
    #[test]
    fn reboot_timestamp_rollback_can_recover_without_resetting_other_channels() {
        let mut p = Probe::default();
        healthy(&mut p, 0, 30_000);
        assert!(p.take_window(30_000).unwrap().passed);
        for now in (30_000..40_000).step_by(4) {
            for c in 0..2 {
                p.observe(c, (1u64 << 32) | (now - 30_000), now);
            }
            if now % 1000 == 0 {
                for c in 2..CHANNELS {
                    p.observe(c, (1u64 << 32) | (now - 30_000), now);
                }
            }
        }
        assert!(p.take_window(40_000).unwrap().passed);
    }
    #[test]
    fn a_bad_window_cannot_be_hidden_by_a_previous_good_one() {
        let mut p = Probe::default();
        healthy(&mut p, 0, 30_000);
        assert!(p.take_window(30_000).unwrap().passed);
        assert!(!p.take_window(40_000).unwrap().passed);
        healthy(&mut p, 40_000, 50_000);
        assert!(p.take_window(50_000).unwrap().passed);
        healthy(&mut p, 50_000, 60_000);
        assert!(p.take_window(60_000).unwrap().passed);
    }
}
