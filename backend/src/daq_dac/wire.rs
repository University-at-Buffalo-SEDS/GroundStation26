//! Exact byte layouts of DAQ_DAC_COMMAND (32 bytes) and DAQ_DAC_STATUS (80 bytes).
use serde::{Deserialize, Serialize};

pub const VERSION: u8 = 1;
pub const CHANNEL_ALL: u8 = 0xFF;
pub const COMMAND_LEN: usize = 32;
pub const STATUS_LEN: usize = 80;
const CHANNEL_BLOCK: usize = 36;
const CHANNEL_BASE: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Waveform {
    Dc = 0,
    Square = 1,
    Sine = 2,
    Triangle = 3,
    Staircase = 4,
}
impl Waveform {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Dc,
            1 => Self::Square,
            2 => Self::Sine,
            3 => Self::Triangle,
            4 => Self::Staircase,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Dc => "dc",
            Self::Square => "square",
            Self::Sine => "sine",
            Self::Triangle => "triangle",
            Self::Staircase => "staircase",
        }
    }
    /// Square, sine and triangle run at a frequency; staircase at steps x dwell.
    pub fn uses_frequency(self) -> bool {
        matches!(self, Self::Square | Self::Sine | Self::Triangle)
    }
}

/// One GS -> DAQ command, in DAC-pin volts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Command {
    pub channel: u8,
    pub enable: bool,
    pub waveform: Waveform,
    pub amplitude_v: f32,
    pub offset_v: f32,
    pub frequency_hz: f32,
    pub steps: u16,
    pub lease_ms: u16,
    pub dwell_ms: u32,
    pub sequence: u32,
}
impl Command {
    pub fn disable(channel: u8, sequence: u32) -> Self {
        Self {
            channel,
            enable: false,
            waveform: Waveform::Dc,
            amplitude_v: 0.0,
            offset_v: 0.0,
            frequency_hz: 0.0,
            steps: 0,
            lease_ms: 0,
            dwell_ms: 0,
            sequence,
        }
    }
    pub fn encode(&self) -> [u8; COMMAND_LEN] {
        let mut b = [0u8; COMMAND_LEN];
        b[0] = VERSION;
        b[1] = self.channel;
        b[2] = u8::from(self.enable);
        b[3] = self.waveform as u8;
        b[4..8].copy_from_slice(&self.amplitude_v.to_le_bytes());
        b[8..12].copy_from_slice(&self.offset_v.to_le_bytes());
        b[12..16].copy_from_slice(&self.frequency_hz.to_le_bytes());
        b[16..18].copy_from_slice(&self.steps.to_le_bytes());
        b[18..20].copy_from_slice(&self.lease_ms.to_le_bytes());
        b[20..24].copy_from_slice(&self.dwell_ms.to_le_bytes());
        b[24..28].copy_from_slice(&self.sequence.to_le_bytes());
        // 28..32 reserved = 0
        b
    }
    #[cfg(test)]
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != COMMAND_LEN || b[0] != VERSION || b[2] > 1 {
            return None;
        }
        Some(Self {
            channel: b[1],
            enable: b[2] == 1,
            waveform: Waveform::from_u8(b[3])?,
            amplitude_v: f32_at(b, 4),
            offset_v: f32_at(b, 8),
            frequency_hz: f32_at(b, 12),
            steps: u16_at(b, 16),
            lease_ms: u16_at(b, 18),
            dwell_ms: u32_at(b, 20),
            sequence: u32_at(b, 24),
        })
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn f32_at(b: &[u8], at: usize) -> f32 {
    f32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// One channel block of DAQ_DAC_STATUS, applied values in DAC-pin volts.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct ChannelStatus {
    pub active: bool,
    pub waveform: u8,
    pub last_result: u8,
    pub clamp_flags: u8,
    pub disable_reason: u8,
    pub steps: u16,
    pub amplitude_v: f32,
    pub offset_v: f32,
    pub frequency_hz: f32,
    pub dwell_ms: u32,
    pub sequence: u32,
    pub output_v: f32,
    pub readback_v: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Status {
    pub flight_state: u8,
    pub gate: u8,
    pub flags: u8,
    pub uptime_ms: u32,
    pub channels: [ChannelStatus; 2],
}

pub const FLAG_HARDWARE_READY: u8 = 1;
pub const FLAG_READBACK: u8 = 2;

impl Status {
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != STATUS_LEN || b[0] != VERSION {
            return None;
        }
        let channel = |c: usize| {
            let o = CHANNEL_BASE + CHANNEL_BLOCK * c;
            ChannelStatus {
                active: b[o] != 0,
                waveform: b[o + 1],
                last_result: b[o + 2],
                clamp_flags: b[o + 3],
                disable_reason: b[o + 4],
                steps: u16_at(b, o + 6),
                amplitude_v: f32_at(b, o + 8),
                offset_v: f32_at(b, o + 12),
                frequency_hz: f32_at(b, o + 16),
                dwell_ms: u32_at(b, o + 20),
                sequence: u32_at(b, o + 24),
                output_v: f32_at(b, o + 28),
                readback_v: f32_at(b, o + 32),
            }
        };
        Some(Self {
            flight_state: b[1],
            gate: b[2],
            flags: b[3],
            uptime_ms: u32_at(b, 4),
            channels: [channel(0), channel(1)],
        })
    }
    /// The DAQ firmware's encoder, for tests and simulated status.
    #[cfg(test)]
    pub fn encode(&self) -> [u8; STATUS_LEN] {
        let mut b = [0u8; STATUS_LEN];
        b[0] = VERSION;
        b[1] = self.flight_state;
        b[2] = self.gate;
        b[3] = self.flags;
        b[4..8].copy_from_slice(&self.uptime_ms.to_le_bytes());
        for (c, ch) in self.channels.iter().enumerate() {
            let o = CHANNEL_BASE + CHANNEL_BLOCK * c;
            b[o] = u8::from(ch.active);
            b[o + 1] = ch.waveform;
            b[o + 2] = ch.last_result;
            b[o + 3] = ch.clamp_flags;
            b[o + 4] = ch.disable_reason;
            b[o + 6..o + 8].copy_from_slice(&ch.steps.to_le_bytes());
            b[o + 8..o + 12].copy_from_slice(&ch.amplitude_v.to_le_bytes());
            b[o + 12..o + 16].copy_from_slice(&ch.offset_v.to_le_bytes());
            b[o + 16..o + 20].copy_from_slice(&ch.frequency_hz.to_le_bytes());
            b[o + 20..o + 24].copy_from_slice(&ch.dwell_ms.to_le_bytes());
            b[o + 24..o + 28].copy_from_slice(&ch.sequence.to_le_bytes());
            b[o + 28..o + 32].copy_from_slice(&ch.output_v.to_le_bytes());
            b[o + 32..o + 36].copy_from_slice(&ch.readback_v.to_le_bytes());
        }
        b
    }
    pub fn any_active(&self) -> bool {
        self.channels.iter().any(|c| c.active)
    }
}

pub fn last_result_name(code: u8) -> &'static str {
    match code {
        0 => "ok",
        1 => "refused_state",
        2 => "refused_unknown_state",
        3 => "bad_request",
        4 => "hardware_error",
        5 => "not_ready",
        _ => "unknown",
    }
}

pub fn disable_reason_name(code: u8) -> &'static str {
    match code {
        0 => "none",
        1 => "commanded",
        2 => "flight_state",
        3 => "abort",
        4 => "lease_expired",
        5 => "hardware_error",
        _ => "unknown",
    }
}

pub fn clamp_names(flags: u8) -> Vec<&'static str> {
    [
        "offset",
        "amplitude",
        "frequency",
        "steps",
        "dwell",
        "lease",
    ]
    .into_iter()
    .enumerate()
    .filter(|(bit, _)| flags & (1 << bit) != 0)
    .map(|(_, name)| name)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_bytes_match_the_contract_layout() {
        let command = Command {
            channel: 1,
            enable: true,
            waveform: Waveform::Sine,
            amplitude_v: 0.5,
            offset_v: 1.25,
            frequency_hz: 2.0,
            steps: 8,
            lease_ms: 3000,
            dwell_ms: 1000,
            sequence: 0x0102_0304,
        };
        let b = command.encode();
        assert_eq!(b.len(), 32);
        assert_eq!(&b[0..4], &[1, 1, 1, 2]);
        assert_eq!(&b[4..8], &0.5f32.to_le_bytes());
        assert_eq!(&b[8..12], &1.25f32.to_le_bytes());
        assert_eq!(&b[12..16], &2.0f32.to_le_bytes());
        assert_eq!(&b[16..18], &[8, 0]);
        assert_eq!(&b[18..20], &3000u16.to_le_bytes());
        assert_eq!(&b[20..24], &1000u32.to_le_bytes());
        assert_eq!(&b[24..28], &[4, 3, 2, 1]);
        assert_eq!(&b[28..32], &[0, 0, 0, 0]);
        assert_eq!(Command::decode(&b), Some(command));

        let off = Command::disable(CHANNEL_ALL, 9).encode();
        assert_eq!(&off[0..4], &[1, 0xFF, 0, 0]);
        assert_eq!(&off[24..28], &9u32.to_le_bytes());
        assert!(off[4..24].iter().all(|b| *b == 0));
    }

    #[test]
    fn status_decodes_every_field_at_its_offset() {
        let mut b = [0u8; 80];
        b[0] = 1;
        b[1] = 1;
        b[2] = 0;
        b[3] = 3;
        b[4..8].copy_from_slice(&123_456u32.to_le_bytes());
        // channel 1 block starts at 8 + 36 = 44
        let o = 44;
        b[o] = 1;
        b[o + 1] = 4;
        b[o + 2] = 3;
        b[o + 3] = 0b10_0010;
        b[o + 4] = 4;
        b[o + 6..o + 8].copy_from_slice(&16u16.to_le_bytes());
        b[o + 8..o + 12].copy_from_slice(&0.25f32.to_le_bytes());
        b[o + 12..o + 16].copy_from_slice(&1.5f32.to_le_bytes());
        b[o + 16..o + 20].copy_from_slice(&0.0f32.to_le_bytes());
        b[o + 20..o + 24].copy_from_slice(&500u32.to_le_bytes());
        b[o + 24..o + 28].copy_from_slice(&77u32.to_le_bytes());
        b[o + 28..o + 32].copy_from_slice(&1.75f32.to_le_bytes());
        b[o + 32..o + 36].copy_from_slice(&f32::NAN.to_le_bytes());
        let s = Status::decode(&b).unwrap();
        assert_eq!(
            (s.flight_state, s.gate, s.flags, s.uptime_ms),
            (1, 0, 3, 123_456)
        );
        assert!(!s.channels[0].active);
        let c = s.channels[1];
        assert!(c.active && s.any_active());
        assert_eq!((c.waveform, c.last_result, c.disable_reason), (4, 3, 4));
        assert_eq!(clamp_names(c.clamp_flags), vec!["amplitude", "lease"]);
        assert_eq!((c.steps, c.dwell_ms, c.sequence), (16, 500, 77));
        assert_eq!((c.amplitude_v, c.offset_v, c.output_v), (0.25, 1.5, 1.75));
        assert!(c.readback_v.is_nan());
        // Re-encoding gives identical bytes (NaN keeps its bit pattern).
        assert_eq!(s.encode(), b);
        assert_eq!(last_result_name(3), "bad_request");
        assert_eq!(disable_reason_name(4), "lease_expired");
    }

    #[test]
    fn status_rejects_wrong_length_or_version() {
        assert!(Status::decode(&[1u8; 79]).is_none());
        assert!(Status::decode(&[1u8; 81]).is_none());
        let mut b = [0u8; 80];
        assert!(Status::decode(&b).is_none());
        b[0] = 1;
        assert!(Status::decode(&b).is_some());
    }
}
