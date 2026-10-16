//! Optional host-only Opus voice. No camera path or MCU codec is involved.
use crate::rrls::Role;
use opus::{Application, Bitrate, Channels, Decoder, Encoder};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
const PCM_BYTES: usize = 640;
const SAMPLES: usize = 960; // 60 ms at 16 kHz; three existing browser PCM frames.
const HEADER: usize = 11;
const MAX_OPUS: usize = 64;
pub const MAX_PACKET: usize = HEADER + MAX_OPUS;
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
struct Tx {
    encoder: Encoder,
    owner: u32,
    stream: u32,
    sequence: u16,
    last: Instant,
    pcm: [i16; SAMPLES],
    used: usize,
}
impl Tx {
    fn new(owner: u32) -> Result<Self, String> {
        let mut encoder =
            Encoder::new(16000, Channels::Mono, Application::Voip).map_err(|e| e.to_string())?;
        encoder
            .set_bitrate(Bitrate::Bits(8_000))
            .map_err(|e| e.to_string())?;
        encoder.set_vbr(false).map_err(|e| e.to_string())?;
        encoder.set_complexity(5).map_err(|e| e.to_string())?;
        Ok(Self {
            encoder,
            owner,
            stream: rand::random(),
            sequence: 0,
            last: Instant::now(),
            pcm: [0; SAMPLES],
            used: 0,
        })
    }
    fn push(&mut self, role: Role, pcm: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if pcm.len() != PCM_BYTES {
            return Err("Radio voice accepts only 20 ms mono PCM16 frames".into());
        }
        self.last = Instant::now();
        for pair in pcm.chunks_exact(2) {
            self.pcm[self.used] = i16::from_le_bytes([pair[0], pair[1]]);
            self.used += 1;
        }
        if self.used < SAMPLES {
            return Ok(None);
        }
        self.used = 0;
        let mut output = [0u8; MAX_OPUS];
        let size = self
            .encoder
            .encode(&self.pcm, &mut output)
            .map_err(|e| e.to_string())?;
        if size == 0 || size > MAX_OPUS {
            return Err("Opus packet exceeds radio budget".into());
        }
        let mut packet = Vec::with_capacity(HEADER + size);
        packet.push(if role == Role::Primary { 0 } else { 1 });
        packet.extend_from_slice(&self.stream.to_le_bytes());
        packet.extend_from_slice(&self.owner.to_le_bytes());
        packet.extend_from_slice(&self.sequence.to_le_bytes());
        packet.extend_from_slice(&output[..size]);
        self.sequence = self.sequence.wrapping_add(1);
        Ok(Some(packet))
    }
}
struct Rx {
    decoder: Decoder,
    key: (u8, u32, u32),
    sequence: Option<u16>,
}
impl Rx {
    fn new(key: (u8, u32, u32)) -> Result<Self, String> {
        Ok(Self {
            decoder: Decoder::new(16000, Channels::Mono).map_err(|e| e.to_string())?,
            key,
            sequence: None,
        })
    }
    fn decode(&mut self, sequence: u16, data: &[u8]) -> Option<Vec<i16>> {
        if self.sequence.is_some_and(|old| {
            let delta = sequence.wrapping_sub(old);
            delta == 0 || delta > i16::MAX as u16
        }) {
            return None;
        }
        let mut pcm = vec![0i16; SAMPLES];
        let size = self.decoder.decode(data, &mut pcm, false).ok()?;
        if size != SAMPLES {
            return None;
        }
        self.sequence = Some(sequence);
        Some(pcm)
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
    serde_json::json!({"configured":configured,"available":available,"bitrate_bps":8000,
        "frame_ms":60,"received":codec.received,"dropped":codec.dropped,"sent":codec.sent})
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
        if !mesh.voice_available(mesh.now()) {
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
            state.tx = Tx::new(owner).ok();
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
    let expected = if role == Role::Primary { 1 } else { 0 };
    if !(HEADER + 1..=MAX_PACKET).contains(&packet.len()) || packet[0] != expected {
        return;
    }
    let key = (
        packet[0],
        u32::from_le_bytes(packet[1..5].try_into().unwrap()),
        u32::from_le_bytes(packet[5..9].try_into().unwrap()),
    );
    let sequence = u16::from_le_bytes(packet[9..11].try_into().unwrap());
    let mut state = codec().lock().unwrap();
    if state.rx.as_ref().is_none_or(|rx| rx.key != key) {
        state.rx = Rx::new(key).ok();
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
    let mut tx = Tx::new(7).unwrap();
    let pcm = [0; PCM_BYTES];
    tx.push(role, &pcm).unwrap();
    tx.push(role, &pcm).unwrap();
    tx.push(role, &pcm).unwrap().unwrap()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_radio_budget_and_decode_without_a_second_pcm_copy() {
        let mut encoder = Tx::new(1).unwrap();
        let mut decoder = None;
        let frame: Vec<u8> = (0..320)
            .flat_map(|i| ((i * 50) as i16).to_le_bytes())
            .collect();
        for _ in 0..100 {
            assert!(encoder.push(Role::Primary, &frame).unwrap().is_none());
            assert!(encoder.push(Role::Primary, &frame).unwrap().is_none());
            let packet = encoder.push(Role::Primary, &frame).unwrap().unwrap();
            assert_eq!(packet.len(), HEADER + 60); // 8 kb/s, 60 ms CBR.
            let receiver = decoder.get_or_insert_with(|| Rx::new((0, encoder.stream, 1)).unwrap());
            let seq = u16::from_le_bytes(packet[9..11].try_into().unwrap());
            assert_eq!(receiver.decode(seq, &packet[HEADER..]).unwrap().len(), 960);
            assert!(
                receiver.decode(seq, &packet[HEADER..]).is_none(),
                "duplicates cannot replay audio"
            );
        }
        // 71 payload + 8 RR wrapper + 4 UART framing, every 60 ms.
        assert_eq!((HEADER + 60 + 8 + 4) * 8 * 1000 / 60, 11066);
        assert!(encoder.push(Role::Primary, b"video").is_err());
    }
}
