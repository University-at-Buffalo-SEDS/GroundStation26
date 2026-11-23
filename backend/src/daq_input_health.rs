//! DAQ live input faults (DAQ_INPUT_HEALTH): decode, operator alerts and the
//! DAQ input-health block under Board status.
//!
//! These are warnings. Nothing here gates an operator command: interlocks
//! that already depend on a sensor (e.g. fill mass from a railed load cell,
//! which the live conversion withholds) keep their own behaviour.
//!
//! Wire format (DAQ-Board Core/Inc/daq_input_faults.h), version 2, 18 UInt32
//! words (version 1 sent the first 10):
//! 0 version | flags, 1 active | latched masks, 2-5 u16 episode counters,
//! 6 VMAIN min|now, 7 chamber min|now, 8 P6 imbalance max|common now,
//! 9 supply level|chamber level; all in mV, 0xFFFF = no reading.
//! 10-13 noise (uV, u16, 0xFFFF none) | drift (uV/min, i16, 0x8000 none) of
//! KG1000, selected KG50, chamber PT, excitation; 14 excitation min|now (mV);
//! 15 MCP3564R frames skipped | slot items skipped; 16 scans restarted |
//! device recoveries; 17 latest fast-scan end | slot end (us in the frame).
use crate::types::{DaqInputFaultStatus, DaqInputHealthStatus};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

pub const WORDS: usize = 18;
pub const WORDS_V1: usize = 10;
const VERSION: u32 = 2;
const NO_MV: u16 = 0xFFFF;
/// Re-alert on a fault no sooner than this after its last alert, so an input
/// that flaps cannot flood the alert list (same holdoff as the rail alert).
pub const ALERT_HOLDOFF_MS: i64 = 30_000;
/// Supply dips are counted over this window in the alert text.
pub const DIP_WINDOW_MS: i64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    SupplyLow = 0,
    Kg1000HighRail,
    Kg1000LowRail,
    Kg50HighRail,
    Kg50LowRail,
    P6LegImbalance,
    P6CommonMode,
    ChamberNoSignal,
}

pub const FAULTS: [Fault; 8] = [
    Fault::SupplyLow,
    Fault::Kg1000HighRail,
    Fault::Kg1000LowRail,
    Fault::Kg50HighRail,
    Fault::Kg50LowRail,
    Fault::P6LegImbalance,
    Fault::P6CommonMode,
    Fault::ChamberNoSignal,
];

impl Fault {
    pub fn bit(self) -> u16 {
        1 << self as u16
    }
    pub fn key(self) -> &'static str {
        match self {
            Fault::SupplyLow => "supply_low",
            Fault::Kg1000HighRail => "kg1000_high_rail",
            Fault::Kg1000LowRail => "kg1000_low_rail",
            Fault::Kg50HighRail => "kg50_high_rail",
            Fault::Kg50LowRail => "kg50_low_rail",
            Fault::P6LegImbalance => "p6_leg_imbalance",
            Fault::P6CommonMode => "p6_common_mode",
            Fault::ChamberNoSignal => "chamber_no_signal",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Fault::SupplyLow => "DAQ supply (VMAIN) low",
            Fault::Kg1000HighRail => "1000 kg load cell at ADC full scale",
            Fault::Kg1000LowRail => "1000 kg load cell at amplifier low rail",
            Fault::Kg50HighRail => "50 kg load cell at high rail",
            Fault::Kg50LowRail => "50 kg load cell at low rail",
            Fault::P6LegImbalance => "50 kg P6 sense legs disagree",
            Fault::P6CommonMode => "50 kg P6 bridge excitation",
            Fault::ChamberNoSignal => "Chamber pressure transducer no signal",
        }
    }
    pub fn advice(self) -> &'static str {
        match self {
            Fault::SupplyLow => "Check the DAQ power/ground connector; every load-cell and chamber reading shifts while VMAIN is low.",
            Fault::Kg1000HighRail => "Check the P7 load-cell wiring and the DAQ supply.",
            Fault::Kg1000LowRail => "Open bridge lead: check the P7 load-cell connector.",
            Fault::Kg50HighRail | Fault::Kg50LowRail => "Check the 50 kg load-cell wiring and the DAQ supply.",
            Fault::P6LegImbalance => "One sense lead is intermittent: check the P6 connector (S+ is P6/3).",
            Fault::P6CommonMode => "Both legs moved together: bridge excitation or both sense leads lost; check P6.",
            Fault::ChamberNoSignal => "Check P4 wiring (signal P4/3) and +12VA excitation on P4/6. If no chamber PT is connected, mark it not installed.",
        }
    }
    /// Live-sample rail alert (b15b54c) owning the same condition, if any.
    fn rail_sensor(self) -> Option<&'static str> {
        match self {
            Fault::Kg1000HighRail | Fault::Kg1000LowRail => Some(crate::loadcell::RAW_LOADCELL_DATA_TYPE_1000KG),
            Fault::Kg50HighRail | Fault::Kg50LowRail => Some(crate::loadcell::RAW_LOADCELL_DATA_TYPE_50KG),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub kg50_p6: bool,
    pub chamber_seen: bool,
    pub active: u16,
    pub latched: u16,
    pub episodes: [u16; 8],
    pub vmain_min_mv: Option<u16>,
    pub vmain_mv: Option<u16>,
    pub chamber_min_mv: Option<u16>,
    pub chamber_mv: Option<u16>,
    pub p6_imbalance_max_mv: Option<u16>,
    pub p6_common_mv: Option<u16>,
    pub supply_low_mv: u16,
    pub chamber_floor_mv: u16,
    /// Version 2 only (None from version-1 firmware).
    pub v2: Option<ReportV2>,
}

/// Noise and drift of one channel over the DAQ's last 1 s / 60 s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ChannelNoise {
    pub noise_uv: Option<u16>,
    pub drift_uv_per_min: Option<i16>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportV2 {
    /// KG1000, selected KG50, chamber PT, excitation monitor.
    pub noise: [ChannelNoise; 4],
    pub excitation_min_mv: Option<u16>,
    pub excitation_mv: Option<u16>,
    pub frames_skipped: u16,
    pub slots_skipped: u16,
    pub scans_restarted: u16,
    pub device_recoveries: u16,
    pub fast_scan_end_us: u16,
    pub slot_end_us: u16,
}

fn channel_noise(word: u32) -> ChannelNoise {
    let noise = word as u16;
    let drift = (word >> 16) as u16 as i16;
    ChannelNoise {
        noise_uv: (noise != 0xFFFF).then_some(noise),
        drift_uv_per_min: (drift != i16::MIN).then_some(drift),
    }
}

fn mv(value: u32) -> Option<u16> {
    let value = value as u16;
    (value != NO_MV).then_some(value)
}

fn volts(mv: u16) -> String {
    let v = f32::from(mv) / 1000.0;
    if v < 0.1 { format!("{v:.2} V") } else { format!("{v:.1} V") }
}

impl Report {
    pub fn decode(words: &[u32]) -> Option<Self> {
        let version = words.first()? & 0xFF;
        let shape = match version {
            1 => WORDS_V1,
            VERSION => WORDS,
            _ => return None,
        };
        if words.len() != shape || words[0] & !0x3FF != 0 {
            return None;
        }
        let v2 = (version == VERSION).then(|| ReportV2 {
            noise: [channel_noise(words[10]), channel_noise(words[11]), channel_noise(words[12]), channel_noise(words[13])],
            excitation_min_mv: mv(words[14]),
            excitation_mv: mv(words[14] >> 16),
            frames_skipped: words[15] as u16,
            slots_skipped: (words[15] >> 16) as u16,
            scans_restarted: words[16] as u16,
            device_recoveries: (words[16] >> 16) as u16,
            fast_scan_end_us: words[17] as u16,
            slot_end_us: (words[17] >> 16) as u16,
        });
        let mut episodes = [0u16; 8];
        for (i, count) in episodes.iter_mut().enumerate() {
            *count = (words[2 + i / 2] >> (16 * (i & 1))) as u16;
        }
        Some(Self {
            kg50_p6: words[0] & 0x100 != 0,
            chamber_seen: words[0] & 0x200 != 0,
            active: words[1] as u16 & 0xFF,
            latched: (words[1] >> 16) as u16 & 0xFF,
            episodes,
            vmain_min_mv: mv(words[6]),
            vmain_mv: mv(words[6] >> 16),
            chamber_min_mv: mv(words[7]),
            chamber_mv: mv(words[7] >> 16),
            p6_imbalance_max_mv: mv(words[8]),
            p6_common_mv: mv(words[8] >> 16),
            supply_low_mv: words[9] as u16,
            chamber_floor_mv: (words[9] >> 16) as u16,
            v2,
        })
    }

    /// `chamber_pt_installed`: the station's setting; without a chamber PT the
    /// chamber check is not shown.
    pub fn status(&self, received_age_ms: u64, chamber_pt_installed: bool) -> DaqInputHealthStatus {
        DaqInputHealthStatus {
            received_age_ms,
            faults: FAULTS
                .iter()
                .filter(|fault| {
                    // P6 checks apply only while P6 is the selected KG50 input,
                    // the chamber check only once a reading arrived and while
                    // a chamber PT is installed.
                    match fault {
                        Fault::P6LegImbalance | Fault::P6CommonMode => self.kg50_p6,
                        Fault::ChamberNoSignal => self.chamber_seen && chamber_pt_installed,
                        _ => true,
                    }
                })
                .map(|fault| DaqInputFaultStatus {
                    key: fault.key().into(),
                    label: fault.label().into(),
                    active: self.active & fault.bit() != 0,
                    latched: self.latched & fault.bit() != 0,
                    episodes: self.episodes[*fault as usize],
                    advice: fault.advice().into(),
                })
                .collect(),
            kg50_p6: self.kg50_p6,
            chamber_pt_installed,
            vmain_mv: self.vmain_mv,
            vmain_min_mv: self.vmain_min_mv,
            chamber_mv: self.chamber_mv,
            chamber_min_mv: self.chamber_min_mv,
            p6_imbalance_max_mv: self.p6_imbalance_max_mv,
            p6_common_mv: self.p6_common_mv,
            supply_low_mv: self.supply_low_mv,
            chamber_floor_mv: self.chamber_floor_mv,
            kg1000_noise: self.v2.as_ref().map(|v| v.noise[0]),
            kg50_noise: self.v2.as_ref().map(|v| v.noise[1]),
            chamber_noise: self.v2.as_ref().map(|v| v.noise[2]),
            excitation_noise: self.v2.as_ref().map(|v| v.noise[3]),
            excitation_mv: self.v2.as_ref().and_then(|v| v.excitation_mv),
            excitation_min_mv: self.v2.as_ref().and_then(|v| v.excitation_min_mv),
            adc_frames_skipped: self.v2.as_ref().map(|v| v.frames_skipped),
            adc_slots_skipped: self.v2.as_ref().map(|v| v.slots_skipped),
            adc_scans_restarted: self.v2.as_ref().map(|v| v.scans_restarted),
            adc_device_recoveries: self.v2.as_ref().map(|v| v.device_recoveries),
            adc_fast_scan_end_us: self.v2.as_ref().map(|v| v.fast_scan_end_us),
            adc_slot_end_us: self.v2.as_ref().map(|v| v.slot_end_us),
        }
    }

    fn kg50_input(&self) -> &'static str {
        if self.kg50_p6 { "P6 differential" } else { "AMP2/P8" }
    }

    /// Plain-language operator text for `fault`. `brief` when the DAQ saw it
    /// only between reports, too short to activate; `dips` counts supply
    /// dips in the last DIP_WINDOW_MS.
    pub fn alert_text(&self, fault: Fault, brief: bool, dips: u32) -> String {
        let count = |n: u32, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
        match fault {
            Fault::SupplyLow => {
                let low = self.vmain_min_mv.or(self.vmain_mv).map_or_else(|| "below its limit".into(), volts);
                format!(
                    "DAQ supply dropped to {low} ({} in 60 s, limit {}) — check DAQ power/ground connector. Load-cell and chamber readings shift while it is low.",
                    count(dips.max(1), "dip"),
                    volts(self.supply_low_mv)
                )
            }
            Fault::Kg1000HighRail | Fault::Kg1000LowRail | Fault::Kg50HighRail | Fault::Kg50LowRail => {
                let (cell, rail) = match fault {
                    Fault::Kg1000HighRail => ("1000 kg load cell (P7)".to_string(), "ADC full-scale rail"),
                    Fault::Kg1000LowRail => ("1000 kg load cell (P7)".to_string(), "amplifier low rail"),
                    Fault::Kg50HighRail => (format!("50 kg load cell ({})", self.kg50_input()), "high rail"),
                    _ => (format!("50 kg load cell ({})", self.kg50_input()), "low rail"),
                };
                let when = if brief { "briefly hit" } else { "is at" };
                format!("{cell} input {when} its {rail} on the DAQ — {}", fault.advice())
            }
            Fault::P6LegImbalance => {
                let by = self.p6_imbalance_max_mv.map_or_else(String::new, |mv| format!(" by {}", volts(mv)));
                let when = if brief { "briefly disagreed" } else { "disagree" };
                format!(
                    "50 kg load cell P6 sense legs {when}{by} — one lead is intermittent; check the P6 connector (S+ is P6/3). The 50 kg weight spikes while this lasts."
                )
            }
            Fault::P6CommonMode => {
                let at = self.p6_common_mv.map_or_else(String::new, |mv| format!(" ({} mean, expected about 1.14 V)", volts(mv)));
                format!("50 kg load cell P6 legs moved together{at} — bridge excitation or both sense leads lost; check P6.")
            }
            Fault::ChamberNoSignal => {
                let reads = self.chamber_mv.or(self.chamber_min_mv).map_or_else(|| "near 0 V".into(), volts);
                // The DAQ raises this only in the launch burn window (T0 + 1.5 s
                // to T0 + 5 s) for the installed 0-5 V PX309, which reads 0 V
                // when vented; at rest it is never reported.
                format!(
                    "Chamber pressure transducer reads {reads} — no signal during the burn; check P4 wiring (signal P4/3) and +12VA excitation on P4/6 (if no chamber PT is connected, mark it not installed)."
                )
            }
        }
    }
}

/// Per-DAQ alert debounce: one alert per new episode, at most once per
/// ALERT_HOLDOFF_MS per fault, while a fault stays active no repeat.
#[derive(Default)]
pub struct AlertTracker {
    previous: Option<Report>,
    last_alert_ms: [Option<i64>; 8],
    /// (time, new dips) for the supply dip count.
    dips: VecDeque<(i64, u32)>,
}

impl AlertTracker {
    /// Alerts to raise for `report`, received at `now_ms`. `claim_rail(sensor)`
    /// returns true when the live-sample rail alert has not already told the
    /// operator (and records that this alert did), so a rail is never
    /// announced twice. Without a chamber PT installed (station setting) the
    /// chamber no-signal fault is never announced.
    pub fn observe(
        &mut self,
        report: &Report,
        now_ms: i64,
        chamber_pt_installed: bool,
        mut claim_rail: impl FnMut(&str) -> bool,
    ) -> Vec<String> {
        let mut alerts = Vec::new();
        let previous = self.previous.take();
        // A DAQ reboot restarts its counters: count only what is latched now.
        let restarted = previous
            .as_ref()
            .is_none_or(|p| report.episodes.iter().zip(p.episodes.iter()).any(|(now, before)| now < before));
        let new_episodes = |fault: Fault| -> u32 {
            let i = fault as usize;
            match &previous {
                Some(p) if !restarted => u32::from(report.episodes[i].wrapping_sub(p.episodes[i])),
                _ => u32::from(report.latched & fault.bit() != 0),
            }
        };
        self.dips.retain(|(at, _)| now_ms.saturating_sub(*at) < DIP_WINDOW_MS && *at <= now_ms);
        let supply_new = new_episodes(Fault::SupplyLow);
        if supply_new > 0 {
            self.dips.push_back((now_ms, supply_new));
        }
        let dips: u32 = self.dips.iter().map(|(_, n)| n).sum();
        let was_active = previous.as_ref().map_or(0, |p| p.active);
        for fault in FAULTS {
            let active = report.active & fault.bit() != 0;
            let started = active && was_active & fault.bit() == 0;
            let brief = !active && new_episodes(fault) > 0;
            // A chamber or excitation reading is a steady condition: only an
            // active (debounced) fault is worth an alert.
            if !(started || brief && !matches!(fault, Fault::ChamberNoSignal | Fault::P6CommonMode)) {
                continue;
            }
            if fault == Fault::ChamberNoSignal && !chamber_pt_installed {
                continue;
            }
            let i = fault as usize;
            if let Some(sensor) = fault.rail_sensor() {
                if !claim_rail(sensor) {
                    continue;
                }
            } else if self.last_alert_ms[i]
                .is_some_and(|last| now_ms >= last && now_ms - last < ALERT_HOLDOFF_MS)
            {
                continue;
            }
            self.last_alert_ms[i] = Some(now_ms);
            alerts.push(report.alert_text(fault, brief, dips));
        }
        self.previous = Some(report.clone());
        alerts
    }
}

static TRACKERS: OnceLock<Mutex<HashMap<String, AlertTracker>>> = OnceLock::new();

/// Alerts for a DAQ_INPUT_HEALTH report from `sender`, debounced per sender.
pub fn observe_alerts(
    sender: &str,
    report: &Report,
    now_ms: i64,
    chamber_pt_installed: bool,
    claim_rail: impl FnMut(&str) -> bool,
) -> Vec<String> {
    TRACKERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .entry(sender.to_owned())
        .or_default()
        .observe(report, now_ms, chamber_pt_installed, claim_rail)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words as the DAQ builds them (daq_input_faults_report).
    fn words(active: u16, latched: u16, episodes: [u16; 8], vmain: (u16, u16), chamber: (u16, u16), p6: (u16, u16), flags: u32) -> Vec<u32> {
        let mut w = vec![1 | flags, u32::from(active) | (u32::from(latched) << 16), 0, 0, 0, 0];
        for (i, n) in episodes.iter().enumerate() {
            w[2 + i / 2] |= u32::from(*n) << (16 * (i & 1));
        }
        w.push(u32::from(vmain.0) | (u32::from(vmain.1) << 16));
        w.push(u32::from(chamber.0) | (u32::from(chamber.1) << 16));
        w.push(u32::from(p6.0) | (u32::from(p6.1) << 16));
        w.push(12_500 | (50 << 16));
        w
    }
    const NONE: (u16, u16) = (NO_MV, NO_MV);
    const HEALTHY: (u16, u16) = (14_180, 14_200);

    fn decode(w: &[u32]) -> Report {
        Report::decode(w).unwrap()
    }

    #[test]
    fn decode_reads_version_2_noise_excitation_and_frame_counters() {
        let mut w = words(0, 0, [0; 8], HEALTHY, (8, 17), NONE, 0x200);
        w[0] = 2 | 0x200;
        // KG1000 48 uV noise, +600 uV/min; KG50 unknown; chamber 120 uV, -3 uV/min;
        // no excitation monitor.
        w.extend([48 | (600 << 16), 0x8000_FFFF, 120 | (u32::from(-3i16 as u16) << 16), 0x8000_FFFF]);
        w.extend([0xFFFF_FFFF, 2 | (7 << 16), 1 | (0 << 16), 770 | (930 << 16)]);
        let r = decode(&w);
        let v2 = r.v2.clone().unwrap();
        assert_eq!(v2.noise[0], ChannelNoise { noise_uv: Some(48), drift_uv_per_min: Some(600) });
        assert_eq!(v2.noise[1], ChannelNoise::default());
        assert_eq!(v2.noise[2], ChannelNoise { noise_uv: Some(120), drift_uv_per_min: Some(-3) });
        assert_eq!((v2.excitation_min_mv, v2.excitation_mv), (None, None));
        assert_eq!((v2.frames_skipped, v2.slots_skipped, v2.scans_restarted, v2.device_recoveries), (2, 7, 1, 0));
        assert_eq!((v2.fast_scan_end_us, v2.slot_end_us), (770, 930));
        let status = r.status(0, true);
        assert_eq!(status.kg1000_noise.unwrap().noise_uv, Some(48));
        assert_eq!(status.adc_slots_skipped, Some(7));
        assert!(Report::decode(&w[..10]).is_none(), "version 2 must carry 18 words");
        // Version-1 firmware still decodes, without the version-2 block.
        assert!(decode(&words(0, 0, [0; 8], HEALTHY, NONE, NONE, 0)).v2.is_none());
    }

    #[test]
    fn decode_reads_every_field_and_rejects_other_shapes() {
        let mut episodes = [0u16; 8];
        episodes[0] = 53;
        episodes[7] = 1;
        episodes[5] = 0xFFFF;
        let w = words(0x81, 0xA1, episodes, (11_030, 11_100), (8, 17), (229, 1_137), 0x300);
        let r = decode(&w);
        assert!(r.kg50_p6 && r.chamber_seen);
        assert_eq!((r.active, r.latched), (0x81, 0xA1));
        assert_eq!(r.episodes, episodes);
        assert_eq!((r.vmain_min_mv, r.vmain_mv), (Some(11_030), Some(11_100)));
        assert_eq!((r.chamber_min_mv, r.chamber_mv), (Some(8), Some(17)));
        assert_eq!((r.p6_imbalance_max_mv, r.p6_common_mv), (Some(229), Some(1_137)));
        assert_eq!((r.supply_low_mv, r.chamber_floor_mv), (12_500, 50));
        let r = decode(&words(0, 0, [0; 8], NONE, NONE, NONE, 0));
        assert_eq!((r.vmain_mv, r.chamber_mv, r.p6_common_mv), (None, None, None));
        assert!(Report::decode(&w[..9]).is_none());
        let mut bad = w.clone();
        bad[0] = 3;
        assert!(Report::decode(&bad).is_none(), "unknown version");
        bad[0] = 2;
        assert!(Report::decode(&bad).is_none(), "version 2 needs 18 words");
        bad[0] = 1 | 0x400;
        assert!(Report::decode(&bad).is_none(), "unknown flag");
    }

    #[test]
    fn status_lists_only_the_checks_that_apply() {
        let r = decode(&words(1, 1, [3, 0, 0, 0, 0, 0, 0, 0], (11_030, 11_100), NONE, NONE, 0));
        let status = r.status(250, true);
        let keys: Vec<_> = status.faults.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["supply_low", "kg1000_high_rail", "kg1000_low_rail", "kg50_high_rail", "kg50_low_rail"]);
        assert!(status.faults[0].active && status.faults[0].latched && status.faults[0].episodes == 3);
        assert_eq!(status.received_age_ms, 250);
        let status = decode(&words(0, 0, [0; 8], HEALTHY, (8, 17), (4, 1_137), 0x300)).status(0, true);
        assert_eq!(status.faults.len(), 8);
        assert_eq!(status.p6_common_mv, Some(1_137));
    }

    #[test]
    fn alert_text_is_plain_language_with_values_and_next_step() {
        let r = decode(&words(1, 1, [14, 0, 0, 0, 0, 0, 0, 0], (11_060, 11_100), (8, 12), (229, 1_137), 0x300));
        assert_eq!(
            r.alert_text(Fault::SupplyLow, false, 14),
            "DAQ supply dropped to 11.1 V (14 dips in 60 s, limit 12.5 V) — check DAQ power/ground connector. Load-cell and chamber readings shift while it is low."
        );
        assert!(r.alert_text(Fault::SupplyLow, true, 1).contains("(1 dip in 60 s"));
        let chamber = r.alert_text(Fault::ChamberNoSignal, false, 0);
        assert_eq!(chamber, "Chamber pressure transducer reads 0.01 V — no signal during the burn; check P4 wiring (signal P4/3) and +12VA excitation on P4/6 (if no chamber PT is connected, mark it not installed).");
        assert!(!chamber.contains("vented"), "the DAQ no longer reports a vented transducer at rest");
        assert!(chamber.contains("+12VA excitation on P4/6"));
        let p6 = r.alert_text(Fault::P6LegImbalance, true, 0);
        assert!(p6.contains("briefly disagreed by 0.2 V") && p6.contains("P6/3"), "{p6}");
        assert!(r.alert_text(Fault::P6CommonMode, false, 0).contains("1.1 V mean"));
        let rail = r.alert_text(Fault::Kg1000LowRail, false, 0);
        assert_eq!(rail, "1000 kg load cell (P7) input is at its amplifier low rail on the DAQ — Open bridge lead: check the P7 load-cell connector.");
        assert!(r.alert_text(Fault::Kg50HighRail, true, 0).starts_with("50 kg load cell (P6 differential) input briefly hit its high rail"));
    }

    fn observe(t: &mut AlertTracker, w: &[u32], now: i64) -> Vec<String> {
        t.observe(&decode(w), now, true, |_| true)
    }

    #[test]
    fn supply_dips_alert_once_per_holdoff_with_the_running_count() {
        let mut t = AlertTracker::default();
        assert!(observe(&mut t, &words(0, 0, [0; 8], HEALTHY, NONE, NONE, 0), 0).is_empty());
        let mut n = [0u16; 8];
        n[0] = 1;
        // A one-sample dip between reports: counted, latched, not active.
        let alerts = observe(&mut t, &words(0, 1, n, (11_060, 14_180), NONE, NONE, 0), 1_000);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].starts_with("DAQ supply dropped to 11.1 V (1 dip in 60 s"), "{}", alerts[0]);
        // More dips inside the holdoff: no repeat.
        for (i, at) in (2_000..30_000).step_by(2_000).enumerate() {
            n[0] = 2 + i as u16;
            assert!(observe(&mut t, &words(0, 1, n, (11_050, 14_200), NONE, NONE, 0), at).is_empty());
        }
        // After the holdoff, the alert carries the 60 s count.
        n[0] += 1;
        let alerts = observe(&mut t, &words(0, 1, n, (11_040, 14_200), NONE, NONE, 0), 31_000);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains(&format!("({} dips in 60 s", n[0])), "{}", alerts[0]);
        // Quiet reports raise nothing.
        assert!(observe(&mut t, &words(0, 0, n, HEALTHY, NONE, NONE, 0), 32_000).is_empty());
    }

    #[test]
    fn an_active_fault_alerts_once_and_not_again_while_it_stays_active() {
        let mut t = AlertTracker::default();
        let mut n = [0u16; 8];
        n[7] = 1;
        let chamber = words(0x80, 0x80, n, HEALTHY, (8, 12), NONE, 0x200);
        let first = observe(&mut t, &chamber, 0);
        assert_eq!(first.len(), 1);
        assert!(first[0].starts_with("Chamber pressure transducer reads 0.01 V"));
        for at in (1_000..200_000).step_by(1_000) {
            assert!(observe(&mut t, &chamber, at).is_empty(), "still the same episode at {at}");
        }
        // Cleared, then back after the holdoff: a new alert.
        let clear = words(0, 0, n, HEALTHY, (1_200, 1_250), NONE, 0x200);
        assert!(observe(&mut t, &clear, 200_000).is_empty());
        n[7] = 2;
        assert_eq!(observe(&mut t, &words(0x80, 0x80, n, HEALTHY, (8, 9), NONE, 0x200), 260_000).len(), 1);
    }

    #[test]
    fn without_a_chamber_pt_installed_the_chamber_fault_is_neither_listed_nor_announced() {
        let mut n = [0u16; 8];
        n[7] = 1;
        // The 2026-11-16 fire: no chamber PT connected, CH6 at 0.01 V in the burn.
        let chamber = words(0x81, 0x81, [1, 0, 0, 0, 0, 0, 0, 1], (11_050, 11_100), (8, 12), NONE, 0x200);
        let mut t = AlertTracker::default();
        let alerts = t.observe(&decode(&chamber), 0, false, |_| true);
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert!(alerts[0].starts_with("DAQ supply dropped"), "other faults still alert: {alerts:?}");
        for at in (1_000..60_000).step_by(1_000) {
            assert!(t.observe(&decode(&chamber), at, false, |_| true).is_empty());
        }
        let status = decode(&chamber).status(0, false);
        assert!(!status.chamber_pt_installed);
        assert!(status.faults.iter().all(|f| f.key != "chamber_no_signal"));
        assert_eq!(status.chamber_mv, Some(12), "the reading is still shown");
        // Installed: the same report is listed and announced.
        let status = decode(&chamber).status(0, true);
        assert!(status.chamber_pt_installed);
        assert!(status.faults.iter().any(|f| f.key == "chamber_no_signal" && f.active));
        let alerts = AlertTracker::default().observe(&decode(&words(0x80, 0x80, n, HEALTHY, (8, 12), NONE, 0x200)), 0, true, |_| true);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains("no signal during the burn") && alerts[0].contains("mark it not installed"));
    }

    #[test]
    fn a_brief_chamber_or_excitation_dip_is_not_an_alert() {
        let mut t = AlertTracker::default();
        observe(&mut t, &words(0, 0, [0; 8], HEALTHY, (1_000, 1_000), (4, 1_137), 0x300), 0);
        let mut n = [0u16; 8];
        n[6] = 1;
        n[7] = 1;
        assert!(observe(&mut t, &words(0, 0xC0, n, HEALTHY, (30, 1_000), (4, 1_137), 0x300), 1_000).is_empty());
    }

    #[test]
    fn rail_reports_defer_to_the_live_sample_rail_alert() {
        let mut t = AlertTracker::default();
        observe(&mut t, &words(0, 0, [0; 8], HEALTHY, NONE, NONE, 0), 0);
        let mut n = [0u16; 8];
        n[1] = 1;
        let railed = words(0x02, 0x02, n, HEALTHY, NONE, NONE, 0);
        // The station already alerted from the live KG1000 samples.
        let mut asked = Vec::new();
        let alerts = t.observe(&decode(&railed), 1_000, true, |sensor| {
            asked.push(sensor.to_string());
            false
        });
        assert!(alerts.is_empty());
        assert_eq!(asked, ["KG1000"]);
        // A brief rail the live stream never showed is announced once.
        n[1] = 2;
        let brief = words(0, 0x02, n, HEALTHY, NONE, NONE, 0);
        let alerts = t.observe(&decode(&brief), 2_000, true, |sensor| sensor == "KG1000");
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains("briefly hit its ADC full-scale rail"), "{}", alerts[0]);
    }

    #[test]
    fn a_daq_reboot_restarts_counting_without_a_burst() {
        let mut t = AlertTracker::default();
        let mut n = [0u16; 8];
        n[0] = 40;
        observe(&mut t, &words(0, 0, n, HEALTHY, NONE, NONE, 0), 0);
        // Rebooted: counters back near zero, nothing latched.
        n[0] = 0;
        assert!(observe(&mut t, &words(0, 0, n, HEALTHY, NONE, NONE, 0), 1_000).is_empty());
        n[0] = 1;
        let alerts = observe(&mut t, &words(0, 1, n, (11_100, 14_200), NONE, NONE, 0), 2_000);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains("(1 dip in 60 s"), "{}", alerts[0]);
    }

    /// The test-fire pattern: dips every few seconds over 2 minutes give one
    /// alert per 30 s, each with the dips of the last minute.
    #[test]
    fn fire_dip_pattern_is_debounced() {
        let dips_at = [327.0, 327.5, 340.5, 344.0, 351.0, 351.5, 352.0, 352.7, 353.2, 354.7, 356.7, 371.0, 374.0, 379.0,
            380.7, 381.5, 382.0, 400.5, 405.0, 408.0, 409.0, 410.7, 411.5, 413.0, 413.7, 414.7, 415.7, 420.2, 421.2,
            423.0, 424.0, 425.2, 428.0, 429.0, 429.5, 431.2, 431.7, 434.7, 443.0, 443.5, 445.0, 449.0, 451.0];
        let mut t = AlertTracker::default();
        let mut count = 0u16;
        let mut alerts = Vec::new();
        for second in 320..460 {
            let new = dips_at.iter().filter(|d| **d >= f64::from(second) && **d < f64::from(second + 1)).count() as u16;
            count += new;
            let mut n = [0u16; 8];
            n[0] = count;
            let latched = u16::from(new > 0);
            let w = words(0, latched, n, (if new > 0 { 11_050 } else { 14_180 }, 14_200), NONE, NONE, 0);
            alerts.extend(observe(&mut t, &w, i64::from(second) * 1_000));
        }
        assert!((4..=6).contains(&alerts.len()), "{alerts:#?}");
        assert!(alerts.iter().all(|a| a.starts_with("DAQ supply dropped to 11.1 V")));
        assert!(alerts.iter().any(|a| a.contains("dips in 60 s") && !a.contains("(1 dip")), "{alerts:#?}");
    }
}
