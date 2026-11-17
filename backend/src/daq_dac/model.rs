//! Input paths the DAC loopback cable can reach, and how a DAC-pin voltage
//! shows up in each path's reported value.
use serde::{Deserialize, Serialize};

/// Connector volts = BUFFER_GAIN x DAC-pin volts (reworked OPA2994 x2 buffer).
pub const BUFFER_GAIN: f64 = 2.0;
/// Linear DAC pin range with the output buffer on.
pub const DAC_MIN_V: f64 = 0.20;
pub const DAC_MAX_V: f64 = 3.10;
/// Load-cell amplifier gain (AMP1/AMP2), ADC-pin volts per input volt.
pub const LOADCELL_GAIN: f64 = 100.9;
/// Sweep spans this fraction of the valid range.
pub const SWEEP_LO_FRACTION: f64 = 0.05;
pub const SWEEP_HI_FRACTION: f64 = 0.95;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Polarity {
    /// Cable on S- (pin 4), S+ to GND: reported rises with V_in.
    #[default]
    SMinus,
    /// Cable on S+ (pin 3): reported falls with V_in.
    SPlus,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Path {
    /// STM32 ADC4 internal readback of the DAC pin; no cable, no gain, no k.
    Internal,
    /// Reported value is the input-connector voltage itself.
    Direct,
    /// Load-cell amplifier: reported = zero + gain x V_in (ADC-pin volts).
    LoadCell { zero: f64 },
}

/// Where a target's samples arrive: a DAQ data type and the value index.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Source {
    pub data_type: &'static str,
    pub index: usize,
    /// Required element count, and for tagged streams the tag at index 0.
    pub len: Option<usize>,
    pub tag: Option<f32>,
    /// A fallback source is used only while the primary one is silent.
    pub fallback: bool,
}
const fn src(data_type: &'static str, index: usize) -> Source {
    Source {
        data_type,
        index,
        len: None,
        tag: None,
        fallback: false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Tolerances {
    pub offset: f64,
    pub gain_pct: f64,
    pub nonlinearity_pct_fs: f64,
    pub noise_rms: f64,
}

#[derive(Debug)]
pub struct Target {
    pub id: &'static str,
    pub label: &'static str,
    pub path: Path,
    pub sources: &'static [Source],
    /// Nominal valid V_in (S- polarity for load cells; pin volts for the readback).
    pub valid_vin: (f64, f64),
    /// Packets per second the GS receives for this stream.
    pub stream_hz: f64,
    pub tolerances: Tolerances,
}

/// reported = intercept + slope x V_dac_pin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathModel {
    pub intercept: f64,
    pub slope: f64,
}
impl PathModel {
    pub fn reported(&self, dac_pin_v: f64) -> f64 {
        self.intercept + self.slope * dac_pin_v
    }
    pub fn dac_pin_for(&self, reported: f64) -> f64 {
        (reported - self.intercept) / self.slope
    }
}

const AUX_HZ: f64 = 4.0;
const LOADCELL_HZ: f64 = 40.0;
const STATUS_HZ: f64 = 5.0;
const DIRECT_TOL: Tolerances = Tolerances {
    offset: 0.01,
    gain_pct: 1.0,
    nonlinearity_pct_fs: 0.25,
    noise_rms: 0.002,
};
const SAR_TOL: Tolerances = Tolerances {
    offset: 0.02,
    gain_pct: 1.5,
    nonlinearity_pct_fs: 0.5,
    noise_rms: 0.005,
};
const HVAIN_TOL: Tolerances = Tolerances {
    offset: 0.05,
    gain_pct: 1.5,
    nonlinearity_pct_fs: 0.25,
    noise_rms: 0.01,
};
const P6_TOL: Tolerances = Tolerances {
    offset: 0.002,
    gain_pct: 2.0,
    nonlinearity_pct_fs: 0.5,
    noise_rms: 0.0005,
};
const LOADCELL_TOL: Tolerances = Tolerances {
    offset: 0.03,
    gain_pct: 3.0,
    nonlinearity_pct_fs: 0.5,
    noise_rms: 0.005,
};
const INTERNAL_TOL: Tolerances = Tolerances {
    offset: 0.02,
    gain_pct: 1.0,
    nonlinearity_pct_fs: 0.5,
    noise_rms: 0.005,
};

macro_rules! sar {
    ($id:literal, $label:literal, $i:literal) => {
        Target {
            id: $id,
            label: $label,
            path: Path::Direct,
            sources: &[src("DAQ_SAR_VOLTAGES", $i)],
            valid_vin: (0.0, 3.3),
            stream_hz: AUX_HZ,
            tolerances: SAR_TOL,
        }
    };
}
macro_rules! mcp {
    ($id:literal, $label:literal, $i:literal) => {
        Target {
            id: $id,
            label: $label,
            path: Path::Direct,
            sources: &[src("DAQ_SDADC_VOLTAGES", $i)],
            valid_vin: (0.0, 2.4),
            stream_hz: AUX_HZ,
            tolerances: DIRECT_TOL,
        }
    };
}

pub static TARGETS: &[Target] = &[
    Target {
        id: "internal_ch",
        label: "STM32 ADC4 internal DAC readback (no cable)",
        path: Path::Internal,
        sources: &[],
        valid_vin: (DAC_MIN_V, DAC_MAX_V),
        stream_hz: STATUS_HZ,
        tolerances: INTERNAL_TOL,
    },
    sar!("sar1", "SAR_IN1 (P9 pin 3)", 0),
    sar!("sar2", "SAR_IN2 (P9 pin 4)", 1),
    sar!("sar3", "SAR_IN3 (P3 pin 3)", 2),
    sar!("sar4", "SAR_IN4 (P3 pin 4)", 3),
    sar!("sar5", "SAR_IN5 (P2 pin 3)", 4),
    sar!("sar6", "SAR_IN6 (P2 pin 4)", 5),
    sar!("sar7", "SAR_IN7 (P1 pin 3)", 6),
    sar!("sar8", "SAR_IN8 (P1 pin 4)", 7),
    mcp!("mcp_ch2", "MCP CH2 (P6 pin 3)", 0),
    mcp!("mcp_ch3", "MCP CH3 (P6 pin 4)", 1),
    mcp!("mcp_ch4", "MCP CH4 (P5 pin 3)", 2),
    mcp!("mcp_ch5", "MCP CH5 (P5 pin 4)", 3),
    Target {
        id: "hvain1",
        label: "HVAIN1 (P4 pin 3, MCP CH6, x11)",
        path: Path::Direct,
        sources: &[
            src("DAQ_SDADC_VOLTAGES", 4),
            Source {
                data_type: "DAQ_LOADCELL_CHAMBER",
                index: 2,
                len: Some(3),
                tag: None,
                fallback: true,
            },
        ],
        valid_vin: (0.0, 26.4),
        stream_hz: AUX_HZ,
        tolerances: HVAIN_TOL,
    },
    Target {
        id: "hvain2",
        label: "HVAIN2 (P4 pin 4, MCP CH7, x11)",
        path: Path::Direct,
        sources: &[src("DAQ_SDADC_VOLTAGES", 5)],
        valid_vin: (0.0, 26.4),
        stream_hz: AUX_HZ,
        tolerances: HVAIN_TOL,
    },
    Target {
        id: "p6_diff",
        label: "P6 differential (pin 3 - pin 4, PGA x16, KG50 input = P6)",
        path: Path::Direct,
        sources: &[Source {
            data_type: "DAQ_KG50_SELECTED",
            index: 1,
            len: Some(2),
            tag: Some(1.0),
            fallback: false,
        }],
        valid_vin: (0.0, 0.15),
        stream_hz: LOADCELL_HZ,
        tolerances: P6_TOL,
    },
    Target {
        id: "loadcell_kg1000",
        label: "KG1000 load cell AMP1 (P7)",
        path: Path::LoadCell { zero: 0.218 },
        sources: &[
            Source {
                data_type: "KG1000",
                index: 0,
                len: Some(1),
                tag: None,
                fallback: false,
            },
            Source {
                data_type: "DAQ_LOADCELL_CHAMBER",
                index: 0,
                len: Some(3),
                tag: None,
                fallback: false,
            },
        ],
        valid_vin: (-0.00216, 0.0216),
        stream_hz: LOADCELL_HZ,
        tolerances: LOADCELL_TOL,
    },
    Target {
        id: "loadcell_kg50",
        label: "KG50 load cell AMP2 (P8)",
        path: Path::LoadCell { zero: 0.600 },
        sources: &[Source {
            data_type: "KG50",
            index: 0,
            len: Some(1),
            tag: None,
            fallback: false,
        }],
        valid_vin: (-0.0059, 0.0178),
        stream_hz: LOADCELL_HZ,
        tolerances: LOADCELL_TOL,
    },
];

pub fn target(id: &str) -> Option<&'static Target> {
    TARGETS.iter().find(|t| t.id == id)
}

impl Target {
    /// reported = intercept + gain x V_in.
    fn vin_gain(&self, polarity: Polarity) -> (f64, f64) {
        match self.path {
            Path::Internal | Path::Direct => (0.0, 1.0),
            Path::LoadCell { zero } => (
                zero,
                match polarity {
                    Polarity::SMinus => LOADCELL_GAIN,
                    Polarity::SPlus => -LOADCELL_GAIN,
                },
            ),
        }
    }
    /// The model from DAC-pin volts to this target's reported value.
    pub fn model(&self, attenuation: f64, polarity: Polarity) -> PathModel {
        if self.path == Path::Internal {
            return PathModel {
                intercept: 0.0,
                slope: 1.0,
            };
        }
        let (intercept, gain) = self.vin_gain(polarity);
        PathModel {
            intercept,
            slope: gain * BUFFER_GAIN * attenuation,
        }
    }
    /// The reported values the valid input range spans (independent of polarity).
    pub fn reported_range(&self) -> (f64, f64) {
        let (intercept, gain) = self.vin_gain(Polarity::SMinus);
        (
            intercept + gain * self.valid_vin.0,
            intercept + gain * self.valid_vin.1,
        )
    }
    /// Valid V_in for this polarity.
    pub fn valid_vin_for(&self, polarity: Polarity) -> (f64, f64) {
        let (intercept, gain) = self.vin_gain(polarity);
        let (lo, hi) = self.reported_range();
        let a = (lo - intercept) / gain;
        let b = (hi - intercept) / gain;
        (a.min(b), a.max(b))
    }
    pub fn full_scale(&self) -> f64 {
        let (lo, hi) = self.reported_range();
        (hi - lo).abs()
    }
    /// The smallest connector voltage the DAC can drive is above the valid range.
    pub fn needs_attenuator(&self) -> bool {
        self.path != Path::Internal && BUFFER_GAIN * DAC_MIN_V > self.valid_vin.1
    }
    pub fn model_text(&self) -> &'static str {
        match (self.path, self.id) {
            (Path::Internal, _) => "reported = V_dac (pin volts; no buffer gain, no k)",
            (_, "loadcell_kg1000") => {
                "reported = 0.218 + 100.9 x V_in (cable on S-; on S+: 0.218 - 100.9 x V_in)"
            }
            (_, "loadcell_kg50") => {
                "reported = 0.600 + 100.9 x V_in (cable on S-; on S+: 0.600 - 100.9 x V_in)"
            }
            _ => "reported = V_in",
        }
    }
    pub fn is_internal(&self) -> bool {
        self.path == Path::Internal
    }
    /// The value for this target in one DAQ packet, and whether it came from a
    /// fallback source.
    pub fn extract(&self, data_type: &str, values: &[Option<f32>]) -> Option<(f64, bool)> {
        self.sources.iter().find_map(|s| {
            if s.data_type != data_type
                || s.len.is_some_and(|len| values.len() != len)
                || s.tag
                    .is_some_and(|tag| values.first().copied().flatten() != Some(tag))
            {
                return None;
            }
            let value = values.get(s.index).copied().flatten()?;
            value.is_finite().then_some((f64::from(value), s.fallback))
        })
    }
}

/// DC sweep levels in DAC-pin volts: SWEEP_LO..SWEEP_HI of the target's valid
/// range, inverted through the model and clamped to what the DAC reaches.
/// Levels that collapse onto each other after clamping are dropped.
pub fn sweep_levels(target: &Target, model: PathModel, steps: usize) -> Vec<f64> {
    let (lo, hi) = target.reported_range();
    let a = lo + SWEEP_LO_FRACTION * (hi - lo);
    let b = lo + SWEEP_HI_FRACTION * (hi - lo);
    let mut pins: Vec<f64> = (0..steps.max(2))
        .map(|i| {
            let reported = a + (b - a) * i as f64 / (steps.max(2) - 1) as f64;
            model.dac_pin_for(reported).clamp(DAC_MIN_V, DAC_MAX_V)
        })
        .collect();
    pins.sort_by(f64::total_cmp);
    pins.dedup_by(|x, y| (*x - *y).abs() < 0.005);
    pins
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn every_contract_target_exists_once() {
        let ids: Vec<_> = TARGETS.iter().map(|t| t.id).collect();
        for id in [
            "internal_ch",
            "sar1",
            "sar8",
            "mcp_ch2",
            "mcp_ch5",
            "hvain1",
            "hvain2",
            "p6_diff",
            "loadcell_kg1000",
            "loadcell_kg50",
        ] {
            assert_eq!(ids.iter().filter(|i| **i == id).count(), 1, "{id}");
        }
        assert_eq!(TARGETS.len(), 1 + 8 + 4 + 2 + 1 + 2);
    }

    #[test]
    fn expected_signal_scales_per_input_path() {
        // Internal readback: pin volts, ignores buffer gain and attenuation.
        let m = target("internal_ch").unwrap().model(0.01, Polarity::SPlus);
        assert!(close(m.reported(1.5), 1.5));
        // Direct inputs: V_in = 2 x V_dac x k.
        let m = target("mcp_ch2").unwrap().model(1.0, Polarity::SMinus);
        assert!(close(m.reported(1.0), 2.0));
        let m = target("sar3").unwrap().model(0.5, Polarity::SMinus);
        assert!(close(m.reported(1.0), 1.0));
        // HVAIN reports connector volts (firmware already multiplies by 11).
        let m = target("hvain2").unwrap().model(1.0, Polarity::SMinus);
        assert!(close(m.reported(3.0), 6.0));
        // P6 differential reports input volts.
        let m = target("p6_diff").unwrap().model(0.02, Polarity::SMinus);
        assert!(close(m.reported(1.0), 0.04));
        // Load cells: zero + 100.9 x V_in, sign flips on S+.
        let t = target("loadcell_kg1000").unwrap();
        let m = t.model(0.01, Polarity::SMinus);
        assert!(close(m.reported(0.5), 0.218 + 100.9 * 0.01));
        let m = t.model(0.01, Polarity::SPlus);
        assert!(close(m.reported(0.5), 0.218 - 100.9 * 0.01));
        let m = target("loadcell_kg50")
            .unwrap()
            .model(0.005, Polarity::SMinus);
        assert!(close(m.reported(1.0), 0.6 + 100.9 * 0.01));
        assert!(close(m.dac_pin_for(m.reported(1.7)), 1.7));
    }

    #[test]
    fn ranges_and_attenuator_needs_follow_the_contract() {
        let t = target("loadcell_kg1000").unwrap();
        let (lo, hi) = t.reported_range();
        assert!((lo - (0.218 - 100.9 * 0.00216)).abs() < 1e-9);
        assert!((hi - (0.218 + 100.9 * 0.0216)).abs() < 1e-9);
        let (a, b) = t.valid_vin_for(Polarity::SPlus);
        assert!((a + 0.0216).abs() < 1e-9 && (b - 0.00216).abs() < 1e-9);
        assert!(t.needs_attenuator());
        assert!(target("p6_diff").unwrap().needs_attenuator());
        assert!(target("loadcell_kg50").unwrap().needs_attenuator());
        assert!(!target("mcp_ch2").unwrap().needs_attenuator());
        assert!(!target("hvain1").unwrap().needs_attenuator());
        assert!(!target("internal_ch").unwrap().needs_attenuator());
    }

    #[test]
    fn extraction_picks_the_right_index_tag_and_shape() {
        let v = |xs: &[f32]| xs.iter().map(|x| Some(*x)).collect::<Vec<_>>();
        let sar5 = target("sar5").unwrap();
        let sar = v(&[0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7]);
        assert_eq!(
            sar5.extract("DAQ_SAR_VOLTAGES", &sar).unwrap().0,
            0.4f32 as f64
        );
        assert!(sar5.extract("DAQ_SDADC_VOLTAGES", &sar).is_none());
        let p6 = target("p6_diff").unwrap();
        assert!(p6.extract("DAQ_KG50_SELECTED", &v(&[1.0, 0.07])).is_some());
        assert!(p6.extract("DAQ_KG50_SELECTED", &v(&[2.0, 0.07])).is_none());
        assert!(p6.extract("DAQ_KG50_SELECTED", &v(&[1.0])).is_none());
        let hv = target("hvain1").unwrap();
        let (_, fallback) = hv
            .extract("DAQ_LOADCELL_CHAMBER", &v(&[1.0, 2.0, 5.0]))
            .unwrap();
        assert!(fallback);
        let mut sdadc = v(&[0.0; 6]);
        sdadc[4] = None;
        assert!(hv.extract("DAQ_SDADC_VOLTAGES", &sdadc).is_none());
        let kg = target("loadcell_kg1000").unwrap();
        assert_eq!(
            kg.extract("DAQ_LOADCELL_CHAMBER", &v(&[0.5, 1.0, 2.0])),
            Some((0.5, false))
        );
        assert!(
            target("internal_ch")
                .unwrap()
                .extract("KG1000", &v(&[1.0]))
                .is_none()
        );
    }

    #[test]
    fn sweep_levels_cover_five_to_ninety_five_percent_clamped_to_the_dac() {
        let t = target("mcp_ch2").unwrap();
        let levels = sweep_levels(t, t.model(1.0, Polarity::SMinus), 11);
        // 0.12 V .. 2.28 V at the input = 0.06 .. 1.14 V pin; low end clamps to 0.20.
        assert!(close(levels[0], DAC_MIN_V));
        assert!(close(*levels.last().unwrap(), 1.14));
        assert!(levels.windows(2).all(|w| w[1] > w[0]));
        let t = target("hvain1").unwrap();
        let levels = sweep_levels(t, t.model(1.0, Polarity::SMinus), 11);
        assert!(close(levels[0], 0.66));
        assert!(close(*levels.last().unwrap(), DAC_MAX_V));
        // Clamping collapses many high levels onto 3.10; they are dropped.
        assert!(levels.len() < 11);
        let t = target("internal_ch").unwrap();
        let levels = sweep_levels(t, t.model(1.0, Polarity::SMinus), 3);
        assert!(close(levels[0], 0.345) && close(levels[2], 2.955));
        // Reverse polarity load cell with a 1/200 attenuator still ascends in pin volts.
        let t = target("loadcell_kg1000").unwrap();
        let levels = sweep_levels(t, t.model(0.005, Polarity::SPlus), 5);
        assert!(levels.windows(2).all(|w| w[1] > w[0]));
        assert!(levels.iter().all(|v| (DAC_MIN_V..=DAC_MAX_V).contains(v)));
    }
}
