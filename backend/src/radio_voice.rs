//! Optional host-only push-to-talk over the GS–RRLS radio. No camera path or
//! MCU codec is involved; encoding and decoding run on the two Linux hosts.
//!
//! Budget (32 kbit/s usable = 4000 B/s, 16 B modem allowance per frame):
//! Codec2 1200 bit/s, eight 40 ms codec frames per radio packet (320 ms).
//! 48 B speech, 7 B voice header, 8 B RRLS envelope, 4 B UART framing and
//! 16 B modem allowance make 83 B every 320 ms = 259 B/s.
//! That is 26% of the primary's 25% share (1000 B/s), 18.5% of the RRLS's
//! 35% (1400 B/s) and 13% of its 50%. At the primary's 15% (600 B/s, an RRLS
//! relaying) it would be 43% next to commands and telemetry, so the primary
//! does not send voice below 25%. The previous Opus 8 kbit/s profile needed
//! 99 B per 60 ms (1650 B/s) and fit no host share.
use crate::rrls::Role;
use codec2::{Codec2, Codec2Mode};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// Browser frames: 20 ms of mono PCM16 at 16 kHz.
const PCM_BYTES: usize = 640;
#[cfg(test)]
const BROWSER_SAMPLES: usize = PCM_BYTES / 2;
/// Codec2 1200 runs at 8 kHz: 320 samples (40 ms) into 6 bytes.
const CODEC_SAMPLES: usize = 320;
const CODEC_BYTES: usize = 6;
pub(crate) const FRAMES_PER_PACKET: usize = 8;
pub(crate) const PACKET_MS: u64 = 40 * FRAMES_PER_PACKET as u64;
/// Version 2 header: kind|role, stream id, sequence.
const HEADER: usize = 7;
const KIND_CODEC2_1200: u8 = 0x20;
pub const MAX_PACKET: usize = HEADER + CODEC_BYTES * FRAMES_PER_PACKET;
/// RRLS envelope + UART framing + configured modem allowance per radio frame.
pub(crate) const LINK_OVERHEAD: usize = 8 + 4 + 16;
pub(crate) const BITRATE_BPS: u32 = 1200;
/// Smallest host share that leaves room for commands and telemetry.
pub(crate) const MIN_SENDER_SHARE_PERCENT: u8 = 25;
const SPEECH_SAMPLES_16K: usize = 2 * CODEC_SAMPLES * FRAMES_PER_PACKET;

/// Air bytes per second this profile spends while someone talks.
pub(crate) const fn air_bytes_per_second() -> u64 {
    ((MAX_PACKET + LINK_OVERHEAD) as u64 * 1000).div_ceil(PACKET_MS)
}

#[derive(Clone)]
pub struct Audio {
    pub pcm: Vec<i16>,
    pub created: Instant,
}
static AUDIO: OnceLock<broadcast::Sender<Audio>> = OnceLock::new();
fn audio() -> &'static broadcast::Sender<Audio> {
    AUDIO.get_or_init(|| broadcast::channel(8).0)
}
pub fn subscribe() -> broadcast::Receiver<Audio> {
    audio().subscribe()
}

/// 31-tap windowed-sinc low-pass at 3.4 kHz for 16 kHz audio, used to
/// decimate to and interpolate from Codec2's 8 kHz.
fn taps() -> &'static [f32; 31] {
    static TAPS: OnceLock<[f32; 31]> = OnceLock::new();
    TAPS.get_or_init(|| {
        let mut t = [0f32; 31];
        let cutoff = 3400.0 / 16000.0;
        let mut sum = 0.0;
        for (i, tap) in t.iter_mut().enumerate() {
            let n = i as f32 - 15.0;
            let sinc = if n == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * std::f32::consts::PI * cutoff * n).sin() / (std::f32::consts::PI * n)
            };
            let window = 0.54 - 0.46 * (2.0 * std::f32::consts::PI * i as f32 / 30.0).cos();
            *tap = sinc * window;
            sum += *tap;
        }
        for tap in &mut t {
            *tap /= sum;
        }
        t
    })
}
#[derive(Clone)]
struct Fir {
    history: [f32; 31],
    at: usize,
}
impl Fir {
    fn new() -> Self {
        Self {
            history: [0.0; 31],
            at: 0,
        }
    }
    fn push(&mut self, x: f32) -> f32 {
        self.history[self.at] = x;
        self.at = (self.at + 1) % 31;
        let t = taps();
        (0..31)
            .map(|k| t[k] * self.history[(self.at + 30 - k) % 31])
            .sum()
    }
}
fn clamp(x: f32) -> i16 {
    x.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

struct Tx {
    codec: Codec2,
    filter: Fir,
    owner: u32,
    stream: u32,
    sequence: u16,
    last: Instant,
    narrow: Vec<i16>,
    odd: bool,
}
impl Tx {
    fn new(owner: u32) -> Self {
        Self {
            codec: Codec2::new(Codec2Mode::MODE_1200),
            filter: Fir::new(),
            owner,
            stream: rand::random(),
            sequence: 0,
            last: Instant::now(),
            narrow: Vec::with_capacity(CODEC_SAMPLES * FRAMES_PER_PACKET),
            odd: false,
        }
    }
    fn push(&mut self, role: Role, pcm: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if pcm.len() != PCM_BYTES {
            return Err("Radio voice accepts only 20 ms mono PCM16 frames".into());
        }
        self.last = Instant::now();
        for pair in pcm.as_chunks::<2>().0.iter() {
            let y = self.filter.push(f32::from(i16::from_le_bytes(*pair)));
            // Keep every second filtered sample: 16 kHz to 8 kHz.
            if self.odd {
                self.narrow.push(clamp(y));
            }
            self.odd = !self.odd;
        }
        if self.narrow.len() < CODEC_SAMPLES * FRAMES_PER_PACKET {
            return Ok(None);
        }
        let mut packet = Vec::with_capacity(MAX_PACKET);
        packet.push(KIND_CODEC2_1200 | if role == Role::Primary { 0 } else { 1 });
        packet.extend_from_slice(&self.stream.to_le_bytes());
        packet.extend_from_slice(&self.sequence.to_le_bytes());
        for speech in self.narrow.as_chunks::<CODEC_SAMPLES>().0 {
            let mut bits = [0u8; CODEC_BYTES];
            self.codec.encode(&mut bits, speech.as_slice());
            packet.extend_from_slice(&bits);
        }
        self.narrow.clear();
        self.sequence = self.sequence.wrapping_add(1);
        Ok(Some(packet))
    }
}
struct Rx {
    codec: Codec2,
    filter: Fir,
    key: (u8, u32),
    sequence: Option<u16>,
}
impl Rx {
    fn new(key: (u8, u32)) -> Self {
        Self {
            codec: Codec2::new(Codec2Mode::MODE_1200),
            filter: Fir::new(),
            key,
            sequence: None,
        }
    }
    fn decode(&mut self, sequence: u16, data: &[u8]) -> Option<Vec<i16>> {
        if data.len() != CODEC_BYTES * FRAMES_PER_PACKET
            || self.sequence.is_some_and(|old| {
                let delta = sequence.wrapping_sub(old);
                delta == 0 || delta > i16::MAX as u16
            })
        {
            return None;
        }
        self.sequence = Some(sequence);
        let mut out = Vec::with_capacity(SPEECH_SAMPLES_16K);
        let mut speech = [0i16; CODEC_SAMPLES];
        for bits in data.as_chunks::<CODEC_BYTES>().0 {
            self.codec.decode(&mut speech, bits.as_slice());
            // 8 kHz to 16 kHz: zero-stuff, then the same low-pass with gain 2.
            for &s in &speech {
                out.push(clamp(2.0 * self.filter.push(f32::from(s))));
                out.push(clamp(2.0 * self.filter.push(0.0)));
            }
        }
        Some(out)
    }
}
#[derive(Default)]
struct Codec {
    tx: Option<Tx>,
    rx: Option<Rx>,
    received: u64,
    dropped: u64,
    sent: u64,
}
static CODEC: OnceLock<Mutex<Codec>> = OnceLock::new();
fn codec() -> &'static Mutex<Codec> {
    CODEC.get_or_init(|| Mutex::new(Codec::default()))
}
pub fn status() -> serde_json::Value {
    let (configured, available) = crate::rrls::voice_status();
    let codec = codec().lock().unwrap();
    serde_json::json!({"configured":configured,"available":available,
        "codec":"codec2-1200","bitrate_bps":BITRATE_BPS,"frame_ms":PACKET_MS,
        "air_bytes_per_second":air_bytes_per_second(),
        "received":codec.received,"dropped":codec.dropped,"sent":codec.sent})
}
pub fn release(owner: u32) {
    let mut codec = codec().lock().unwrap();
    if codec.tx.as_ref().is_some_and(|tx| tx.owner == owner) {
        codec.tx = None;
    }
}
pub fn submit(owner: u32, pcm: &[u8]) {
    let Some(shared) = crate::rrls::voice_transport() else {
        return;
    };
    // Never hold the codec mutex while acquiring the mesh mutex. RX arrives
    // under the mesh lock; this ordering keeps the serial worker independent.
    let role = {
        let mesh = shared.lock().unwrap();
        if !mesh.voice_sendable(mesh.now()) {
            return;
        }
        mesh.role
    };
    let packet = {
        let mut state = codec().lock().unwrap();
        let idle = state
            .tx
            .as_ref()
            .is_none_or(|tx| tx.last.elapsed() > Duration::from_millis(200));
        if idle {
            state.tx = Some(Tx::new(owner));
        }
        let Some(tx) = state.tx.as_mut() else {
            return;
        };
        if tx.owner != owner {
            state.dropped += 1;
            return;
        } // One local PTT speaker.
        match tx.push(role, pcm) {
            Ok(Some(packet)) => {
                state.sent += 1;
                Some(packet)
            }
            Ok(None) => None,
            Err(_) => {
                state.dropped += 1;
                state.tx = None;
                None
            }
        }
    };
    if let Some(packet) = packet {
        let mut mesh = shared.lock().unwrap();
        let now = mesh.now();
        mesh.queue_voice(&packet, now);
    }
}
/// Only Mesh's role-checked host namespace invokes this; it is never echoed.
pub fn receive(role: Role, packet: &[u8]) {
    let expected = KIND_CODEC2_1200 | if role == Role::Primary { 1 } else { 0 };
    if packet.len() != MAX_PACKET || packet[0] != expected {
        return;
    }
    let key = (
        packet[0],
        u32::from_le_bytes(packet[1..5].try_into().unwrap()),
    );
    let sequence = u16::from_le_bytes(packet[5..7].try_into().unwrap());
    let mut state = codec().lock().unwrap();
    if state.rx.as_ref().is_none_or(|rx| rx.key != key) {
        state.rx = Some(Rx::new(key));
    }
    let pcm = state
        .rx
        .as_mut()
        .and_then(|rx| rx.decode(sequence, &packet[HEADER..]));
    if let Some(pcm) = pcm {
        state.received += 1;
        let _ = audio().send(Audio {
            pcm,
            created: Instant::now(),
        });
    } else {
        state.dropped += 1;
    }
}
#[cfg(test)]
pub(crate) fn test_packet(role: Role) -> Vec<u8> {
    let mut tx = Tx::new(7);
    let pcm = [0; PCM_BYTES];
    loop {
        if let Some(packet) = tx.push(role, &pcm).unwrap() {
            return packet;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn tone(frames: usize, hz: f32) -> Vec<Vec<u8>> {
        (0..frames)
            .map(|f| {
                (0..BROWSER_SAMPLES)
                    .flat_map(|i| {
                        let t = (f * BROWSER_SAMPLES + i) as f32 / 16000.0;
                        let v = 8000.0 * (2.0 * std::f32::consts::PI * hz * t).sin()
                            + 3000.0 * (2.0 * std::f32::consts::PI * 2.3 * hz * t).sin();
                        clamp(v).to_le_bytes()
                    })
                    .collect()
            })
            .collect()
    }
    #[test]
    fn airtime_fits_the_primary_share_with_room_for_control() {
        // 48 B speech + 7 B header + 8 B envelope + 4 B UART + 16 B modem = 83 B.
        assert_eq!(MAX_PACKET, 55);
        assert_eq!(MAX_PACKET + LINK_OVERHEAD, 83);
        assert_eq!(PACKET_MS, 320);
        assert_eq!(air_bytes_per_second(), 260); // 259.4 rounded up
        let usable = 32_000u64 / 8; // B/s on the matched profile
        let share = |percent: u64| usable * percent / 100;
        assert_eq!(share(25), 1000);
        // Primary at 25%: voice takes about a quarter, leaving 740 B/s.
        assert!(air_bytes_per_second() * 100 / share(25) <= 26);
        // At 15% (RRLS relaying) it would be 43%: the primary does not send.
        assert!(air_bytes_per_second() * 100 / share(15) >= 43);
        assert!(air_bytes_per_second() * 100 / share(MIN_SENDER_SHARE_PERCENT.into()) <= 26);
        // RRLS at its 35% base and 50% relay share.
        assert!(air_bytes_per_second() * 1000 / share(35) <= 186);
        assert!(air_bytes_per_second() * 100 / share(50) <= 13);
        // The old Opus profile: 83 B + 16 B modem per 60 ms.
        assert!((83 + 16) * 1000 / 60 > share(35));
    }
    #[test]
    fn codec2_round_trip_is_fixed_size_ordered_and_never_replays() {
        let mut tx = Tx::new(1);
        let mut rx = None;
        let frames = tone(16 * 6, 440.0);
        let mut packets = Vec::new();
        for (i, frame) in frames.iter().enumerate() {
            let out = tx.push(Role::Primary, frame).unwrap();
            assert_eq!(
                out.is_some(),
                (i + 1) % 16 == 0,
                "one packet per 16 browser frames"
            );
            packets.extend(out);
        }
        assert_eq!(packets.len(), 6);
        let mut energy = 0f64;
        for packet in &packets {
            assert_eq!(packet.len(), MAX_PACKET);
            assert_eq!(packet[0], KIND_CODEC2_1200);
            let receiver = rx.get_or_insert_with(|| Rx::new((packet[0], tx.stream)));
            let seq = u16::from_le_bytes(packet[5..7].try_into().unwrap());
            let pcm = receiver.decode(seq, &packet[HEADER..]).unwrap();
            assert_eq!(pcm.len(), SPEECH_SAMPLES_16K);
            energy += pcm.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>();
            assert!(
                receiver.decode(seq, &packet[HEADER..]).is_none(),
                "duplicates cannot replay audio"
            );
        }
        assert!(energy > 0.0, "decoded speech is not silent");
        let old = u16::from_le_bytes(packets[0][5..7].try_into().unwrap());
        assert!(
            rx.as_mut()
                .unwrap()
                .decode(old, &packets[0][HEADER..])
                .is_none(),
            "older packets are discarded"
        );
        assert!(tx.push(Role::Primary, b"video").is_err());
    }
    #[test]
    fn foreign_and_old_format_packets_are_ignored() {
        let packet = test_packet(Role::Primary);
        let before = codec().lock().unwrap().received;
        receive(Role::Primary, &packet); // own direction is never accepted
        let mut opus = vec![0u8; 11 + 60]; // previous 11-byte header + Opus
        opus[0] = 0;
        receive(Role::Rrls, &opus);
        assert_eq!(codec().lock().unwrap().received, before);
    }
}
