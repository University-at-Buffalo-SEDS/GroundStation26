//! Exact byte layouts of DAQ_DAC_COMMAND v2 (48 bytes), DAQ_DAC_STATUS v2
//! (136 bytes) and DAQ_DAC_TABLE v1 (76 bytes). DAQ-Board
//! docs/dac-waveforms.md is the normative contract.
use serde::{Deserialize, Serialize};

pub const VERSION: u8 = 2;
pub const CHANNEL_ALL: u8 = 0xFF;
pub const COMMAND_LEN: usize = 48;
pub const STATUS_LEN: usize = 136;
const CHANNEL_BLOCK: usize = 52;
const CHANNEL_BASE: usize = 8;
const TABLE_BASE: usize = CHANNEL_BASE + 2 * CHANNEL_BLOCK;
pub const TABLE_VERSION: u8 = 1;
pub const TABLE_LEN: usize = 76;
pub const CHUNK_SAMPLES: usize = 32;

pub const FLAG_RAMP_DOWN: u8 = 0x01;
pub const FLAG_LOG_SWEEP: u8 = 0x02;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Waveform {
    #[default]
    Dc = 0,
    Square = 1,
    Sine = 2,
    Triangle = 3,
    Staircase = 4,
    Sawtooth = 5,
    Chirp = 6,
    Noise = 7,
    Arbitrary = 8,
}
impl Waveform {
    pub const ALL: [Waveform; 9] = [
        Self::Dc,
        Self::Square,
        Self::Sine,
        Self::Triangle,
        Self::Staircase,
        Self::Sawtooth,
        Self::Chirp,
        Self::Noise,
        Self::Arbitrary,
    ];
    pub fn from_u8(value: u8) -> Option<Self> {
        Self::ALL.get(usize::from(value)).copied()
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Dc => "dc",
            Self::Square => "square",
            Self::Sine => "sine",
            Self::Triangle => "triangle",
            Self::Staircase => "staircase",
            Self::Sawtooth => "sawtooth",
            Self::Chirp => "chirp",
            Self::Noise => "noise",
            Self::Arbitrary => "arbitrary",
        }
    }
    /// Built-in periodic shapes whose table length the DAQ chooses.
    pub fn is_periodic(self) -> bool {
        matches!(
            self,
            Self::Square | Self::Sine | Self::Triangle | Self::Sawtooth
        )
    }
    /// Every waveform that takes `frequency_hz` (chirp: start; noise: sample
    /// rate; arbitrary: table repetitions per second).
    pub fn uses_frequency(self) -> bool {
        !matches!(self, Self::Dc | Self::Staircase)
    }
}

/// One GS -> DAQ command, in DAC-pin volts.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Command {
    pub channel: u8,
    pub enable: bool,
    pub waveform: Waveform,
    pub flags: u8,
    pub amplitude_v: f32,
    pub offset_v: f32,
    pub frequency_hz: f32,
    pub frequency_stop_hz: f32,
    pub steps: u16,
    pub lease_ms: u16,
    pub phase_q16: u16,
    pub duty_q16: u16,
    pub table_len: u16,
    pub dwell_ms: u32,
    pub sweep_ms: u32,
    pub table_crc: u32,
    pub sequence: u32,
}
impl Command {
    pub fn disable(channel: u8, sequence: u32) -> Self {
        Self {
            channel,
            sequence,
            ..Self::default()
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
        b[28..30].copy_from_slice(&self.phase_q16.to_le_bytes());
        b[30..32].copy_from_slice(&self.duty_q16.to_le_bytes());
        b[32..36].copy_from_slice(&self.frequency_stop_hz.to_le_bytes());
        b[36..40].copy_from_slice(&self.sweep_ms.to_le_bytes());
        b[40..44].copy_from_slice(&self.table_crc.to_le_bytes());
        b[44..46].copy_from_slice(&self.table_len.to_le_bytes());
        b[46] = self.flags;
        // 47 reserved = 0
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
            phase_q16: u16_at(b, 28),
            duty_q16: u16_at(b, 30),
            frequency_stop_hz: f32_at(b, 32),
            sweep_ms: u32_at(b, 36),
            table_crc: u32_at(b, 40),
            table_len: u16_at(b, 44),
            flags: b[46],
        })
    }
}

/// DAQ_DAC_TABLE operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableOp {
    Begin = 1,
    Data = 2,
    Commit = 3,
    /// Part of the wire contract; the GS replaces a shape with BEGIN instead.
    #[allow(dead_code)]
    Clear = 4,
}

/// One DAQ_DAC_TABLE message.
#[derive(Clone, Debug, PartialEq)]
pub struct TableMsg {
    pub op: TableOp,
    /// BEGIN/COMMIT: table length. DATA: first sample (a multiple of 32).
    pub index: u16,
    pub upload_id: u32,
    /// BEGIN/COMMIT: CRC-32 of the shape. DATA: sample count.
    pub value: u32,
    /// Normalized shape samples, Q15.
    pub samples: Vec<i16>,
}
impl TableMsg {
    pub fn encode(&self) -> [u8; TABLE_LEN] {
        let mut b = [0u8; TABLE_LEN];
        b[0] = TABLE_VERSION;
        b[1] = self.op as u8;
        b[2..4].copy_from_slice(&self.index.to_le_bytes());
        b[4..8].copy_from_slice(&self.upload_id.to_le_bytes());
        b[8..12].copy_from_slice(&self.value.to_le_bytes());
        for (i, s) in self.samples.iter().take(CHUNK_SAMPLES).enumerate() {
            b[12 + 2 * i..14 + 2 * i].copy_from_slice(&s.to_le_bytes());
        }
        b
    }
    #[cfg(test)]
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != TABLE_LEN || b[0] != TABLE_VERSION {
            return None;
        }
        let op = match b[1] {
            1 => TableOp::Begin,
            2 => TableOp::Data,
            3 => TableOp::Commit,
            4 => TableOp::Clear,
            _ => return None,
        };
        let value = u32_at(b, 8);
        let samples = if op == TableOp::Data {
            if value == 0 || value as usize > CHUNK_SAMPLES {
                return None;
            }
            (0..value as usize).map(|i| u16_at(b, 12 + 2 * i) as i16).collect()
        } else {
            Vec::new()
        };
        Some(Self {
            op,
            index: u16_at(b, 2),
            upload_id: u32_at(b, 4),
            value,
            samples,
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
    pub flags: u8,
    pub steps: u16,
    pub amplitude_v: f32,
    pub offset_v: f32,
    pub frequency_hz: f32,
    pub dwell_ms: u32,
    pub sequence: u32,
    pub output_v: f32,
    pub readback_v: f32,
    pub update_hz: f32,
    pub samples: u16,
    pub duty_q16: u16,
    pub phase_q16: u16,
    pub table_len: u16,
    pub table_crc: u32,
}

/// The uploaded-table block of DAQ_DAC_STATUS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TableStatus {
    pub state: u8,
    pub len: u16,
    pub upload_id: u32,
    pub expected_crc: u32,
    pub crc: u32,
    pub chunks: u64,
}

pub const TABLE_EMPTY: u8 = 0;
pub const TABLE_LOADING: u8 = 1;
pub const TABLE_READY: u8 = 2;
pub const TABLE_CRC_ERROR: u8 = 3;
pub const TABLE_BAD_SAMPLE: u8 = 4;
pub const TABLE_BAD_LENGTH: u8 = 5;
pub const TABLE_VERIFYING: u8 = 6;

pub fn table_state_name(state: u8) -> &'static str {
    match state {
        TABLE_EMPTY => "empty",
        TABLE_LOADING => "loading",
        TABLE_READY => "ready",
        TABLE_CRC_ERROR => "crc_error",
        TABLE_BAD_SAMPLE => "bad_sample",
        TABLE_BAD_LENGTH => "bad_length",
        TABLE_VERIFYING => "verifying",
        _ => "unknown",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Status {
    pub flight_state: u8,
    pub gate: u8,
    pub flags: u8,
    pub uptime_ms: u32,
    pub channels: [ChannelStatus; 2],
    pub table: TableStatus,
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
                flags: b[o + 5],
                steps: u16_at(b, o + 6),
                amplitude_v: f32_at(b, o + 8),
                offset_v: f32_at(b, o + 12),
                frequency_hz: f32_at(b, o + 16),
                dwell_ms: u32_at(b, o + 20),
                sequence: u32_at(b, o + 24),
                output_v: f32_at(b, o + 28),
                readback_v: f32_at(b, o + 32),
                update_hz: f32_at(b, o + 36),
                samples: u16_at(b, o + 40),
                duty_q16: u16_at(b, o + 42),
                phase_q16: u16_at(b, o + 44),
                table_len: u16_at(b, o + 46),
                table_crc: u32_at(b, o + 48),
            }
        };
        let t = TABLE_BASE;
        Some(Self {
            flight_state: b[1],
            gate: b[2],
            flags: b[3],
            uptime_ms: u32_at(b, 4),
            channels: [channel(0), channel(1)],
            table: TableStatus {
                state: b[t],
                len: u16_at(b, t + 2),
                upload_id: u32_at(b, t + 4),
                expected_crc: u32_at(b, t + 8),
                crc: u32_at(b, t + 12),
                chunks: u32_at(b, t + 16) as u64 | (u32_at(b, t + 20) as u64) << 32,
            },
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
            b[o + 5] = ch.flags;
            b[o + 6..o + 8].copy_from_slice(&ch.steps.to_le_bytes());
            b[o + 8..o + 12].copy_from_slice(&ch.amplitude_v.to_le_bytes());
            b[o + 12..o + 16].copy_from_slice(&ch.offset_v.to_le_bytes());
            b[o + 16..o + 20].copy_from_slice(&ch.frequency_hz.to_le_bytes());
            b[o + 20..o + 24].copy_from_slice(&ch.dwell_ms.to_le_bytes());
            b[o + 24..o + 28].copy_from_slice(&ch.sequence.to_le_bytes());
            b[o + 28..o + 32].copy_from_slice(&ch.output_v.to_le_bytes());
            b[o + 32..o + 36].copy_from_slice(&ch.readback_v.to_le_bytes());
            b[o + 36..o + 40].copy_from_slice(&ch.update_hz.to_le_bytes());
            b[o + 40..o + 42].copy_from_slice(&ch.samples.to_le_bytes());
            b[o + 42..o + 44].copy_from_slice(&ch.duty_q16.to_le_bytes());
            b[o + 44..o + 46].copy_from_slice(&ch.phase_q16.to_le_bytes());
            b[o + 46..o + 48].copy_from_slice(&ch.table_len.to_le_bytes());
            b[o + 48..o + 52].copy_from_slice(&ch.table_crc.to_le_bytes());
        }
        let t = TABLE_BASE;
        b[t] = self.table.state;
        b[t + 2..t + 4].copy_from_slice(&self.table.len.to_le_bytes());
        b[t + 4..t + 8].copy_from_slice(&self.table.upload_id.to_le_bytes());
        b[t + 8..t + 12].copy_from_slice(&self.table.expected_crc.to_le_bytes());
        b[t + 12..t + 16].copy_from_slice(&self.table.crc.to_le_bytes());
        b[t + 16..t + 24].copy_from_slice(&self.table.chunks.to_le_bytes());
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
        6 => "table_mismatch",
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
        "duty",
        "sweep",
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
            waveform: Waveform::Chirp,
            flags: FLAG_LOG_SWEEP,
            amplitude_v: 0.5,
            offset_v: 1.25,
            frequency_hz: 2.0,
            frequency_stop_hz: 300.0,
            steps: 8,
            lease_ms: 3000,
            phase_q16: 0x4000,
            duty_q16: 0x8000,
            table_len: 512,
            dwell_ms: 1000,
            sweep_ms: 5000,
            table_crc: 0xCAFE_F00D,
            sequence: 0x0102_0304,
        };
        let b = command.encode();
        assert_eq!(b.len(), 48);
        assert_eq!(&b[0..4], &[2, 1, 1, 6]);
        assert_eq!(&b[4..8], &0.5f32.to_le_bytes());
        assert_eq!(&b[8..12], &1.25f32.to_le_bytes());
        assert_eq!(&b[12..16], &2.0f32.to_le_bytes());
        assert_eq!(&b[16..18], &[8, 0]);
        assert_eq!(&b[18..20], &3000u16.to_le_bytes());
        assert_eq!(&b[20..24], &1000u32.to_le_bytes());
        assert_eq!(&b[24..28], &[4, 3, 2, 1]);
        assert_eq!(&b[28..32], &[0, 0x40, 0, 0x80]);
        assert_eq!(&b[32..36], &300.0f32.to_le_bytes());
        assert_eq!(&b[36..40], &5000u32.to_le_bytes());
        assert_eq!(&b[40..44], &0xCAFE_F00Du32.to_le_bytes());
        assert_eq!(&b[44..48], &[0, 2, 2, 0]);
        assert_eq!(Command::decode(&b), Some(command));

        let off = Command::disable(CHANNEL_ALL, 9).encode();
        assert_eq!(&off[0..4], &[2, 0xFF, 0, 0]);
        assert_eq!(&off[24..28], &9u32.to_le_bytes());
        assert!(off[4..24].iter().chain(&off[28..]).all(|b| *b == 0));
    }

    #[test]
    fn table_message_bytes_match_the_contract_layout() {
        let msg = TableMsg {
            op: TableOp::Data,
            index: 992,
            upload_id: 7,
            value: 3,
            samples: vec![248, -2, 32767],
        };
        let b = msg.encode();
        assert_eq!(b.len(), 76);
        assert_eq!(&b[0..12], &[1, 2, 0xE0, 3, 7, 0, 0, 0, 3, 0, 0, 0]);
        assert_eq!(&b[12..18], &[248, 0, 0xFE, 0xFF, 0xFF, 0x7F]);
        assert!(b[18..].iter().all(|b| *b == 0));
        assert_eq!(TableMsg::decode(&b), Some(msg));
        let begin = TableMsg {
            op: TableOp::Begin,
            index: 1000,
            upload_id: 9,
            value: 0x1234_5678,
            samples: vec![],
        };
        assert_eq!(TableMsg::decode(&begin.encode()), Some(begin));
    }

    #[test]
    fn status_decodes_every_field_at_its_offset() {
        let mut b = [0u8; 136];
        b[0] = 2;
        b[1] = 1;
        b[2] = 0;
        b[3] = 3;
        b[4..8].copy_from_slice(&123_456u32.to_le_bytes());
        // channel 1 block starts at 8 + 52 = 60
        let o = 60;
        b[o] = 1;
        b[o + 1] = 4;
        b[o + 2] = 3;
        b[o + 3] = 0b1010_0010;
        b[o + 4] = 4;
        b[o + 5] = 1;
        b[o + 6..o + 8].copy_from_slice(&16u16.to_le_bytes());
        b[o + 8..o + 12].copy_from_slice(&0.25f32.to_le_bytes());
        b[o + 12..o + 16].copy_from_slice(&1.5f32.to_le_bytes());
        b[o + 16..o + 20].copy_from_slice(&0.0f32.to_le_bytes());
        b[o + 20..o + 24].copy_from_slice(&500u32.to_le_bytes());
        b[o + 24..o + 28].copy_from_slice(&77u32.to_le_bytes());
        b[o + 28..o + 32].copy_from_slice(&1.75f32.to_le_bytes());
        b[o + 32..o + 36].copy_from_slice(&f32::NAN.to_le_bytes());
        b[o + 36..o + 40].copy_from_slice(&2.0f32.to_le_bytes());
        b[o + 40..o + 42].copy_from_slice(&16u16.to_le_bytes());
        b[o + 42..o + 44].copy_from_slice(&0x8000u16.to_le_bytes());
        b[o + 44..o + 46].copy_from_slice(&0x4000u16.to_le_bytes());
        b[o + 46..o + 48].copy_from_slice(&64u16.to_le_bytes());
        b[o + 48..o + 52].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let t = 112;
        b[t] = TABLE_READY;
        b[t + 2..t + 4].copy_from_slice(&64u16.to_le_bytes());
        b[t + 4..t + 8].copy_from_slice(&42u32.to_le_bytes());
        b[t + 8..t + 12].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        b[t + 12..t + 16].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        b[t + 16..t + 24].copy_from_slice(&0x1_0000_0003u64.to_le_bytes());
        let s = Status::decode(&b).unwrap();
        assert_eq!(
            (s.flight_state, s.gate, s.flags, s.uptime_ms),
            (1, 0, 3, 123_456)
        );
        assert!(!s.channels[0].active);
        let c = s.channels[1];
        assert!(c.active && s.any_active());
        assert_eq!((c.waveform, c.last_result, c.disable_reason, c.flags), (4, 3, 4, 1));
        assert_eq!(clamp_names(c.clamp_flags), vec!["amplitude", "lease", "sweep"]);
        assert_eq!((c.steps, c.dwell_ms, c.sequence), (16, 500, 77));
        assert_eq!((c.amplitude_v, c.offset_v, c.output_v), (0.25, 1.5, 1.75));
        assert!(c.readback_v.is_nan());
        assert_eq!((c.update_hz, c.samples, c.duty_q16, c.phase_q16), (2.0, 16, 0x8000, 0x4000));
        assert_eq!((c.table_len, c.table_crc), (64, 0xDEAD_BEEF));
        assert_eq!(s.table.state, TABLE_READY);
        assert_eq!((s.table.len, s.table.upload_id, s.table.chunks), (64, 42, 0x1_0000_0003));
        // Re-encoding gives identical bytes (NaN keeps its bit pattern).
        assert_eq!(s.encode(), b);
        assert_eq!(last_result_name(6), "table_mismatch");
        assert_eq!(table_state_name(3), "crc_error");
    }

    #[test]
    fn status_rejects_wrong_length_or_version() {
        assert!(Status::decode(&[2u8; 135]).is_none());
        assert!(Status::decode(&[2u8; 137]).is_none());
        let mut b = [0u8; 136];
        assert!(Status::decode(&b).is_none());
        b[0] = 1;
        assert!(Status::decode(&b).is_none());
        b[0] = 2;
        assert!(Status::decode(&b).is_some());
    }
}
