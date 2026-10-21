//! Single recovery relay over the RFD asynchronous mesh. Wi-Fi is local only.
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

const LOSS_MS: u64 = 3_000;
const RESTORE_MS: u64 = 2_000;
const LEASE_MS: u64 = 2_000;
const MAX_FRAMES: usize = 64;
const MAX_BYTES: usize = 65_536;
const MAX_PAYLOAD: usize = 4_080;
const HEALTH: u8 = 0;
const DOWN_DATA: u8 = 1;
const UP_DATA: u8 = 2;
const DOWN_COMMAND: u8 = 3;
const UP_COMMAND: u8 = 4;
const AV_BAY_DATA: u8 = 5;
const AV_BAY_COMMAND: u8 = 6;
const READY: u8 = 7;
const POSITION: u8 = 8;
const VOICE: u8 = 9;
#[derive(Clone, Copy)]
struct FrameClass {
    priority: u8,
    avionics: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Primary,
    Rrls,
}

pub fn is_rrls() -> bool {
    std::env::var("GS_RRLS_ROLE").ok().as_deref() == Some("rrls")
}

pub type Shared = Arc<Mutex<Mesh>>;
static RADIO: OnceLock<(String, Shared)> = OnceLock::new();

pub fn configure(link: &crate::comms_config::CommsLinkConfig) -> anyhow::Result<()> {
    use crate::comms_config::{CommsLinkConfig as L, SerialProtocol};
    let role = match std::env::var("GS_RRLS_ROLE").ok().as_deref() {
        None | Some("off") => return Ok(()),
        Some("primary") => Role::Primary,
        Some("rrls") => Role::Rrls,
        Some(_) => anyhow::bail!("GS_RRLS_ROLE must be off, primary, or rrls"),
    };
    let serial = match link {
        L::Serial { serial } | L::CustomSerial { serial } | L::RaspberryPiGpioUart { serial } => {
            serial
        }
        _ => anyhow::bail!("RRLS requires the AV-bay raw UART radio interface"),
    };
    anyhow::ensure!(
        serial.protocol == SerialProtocol::RawUart,
        "RRLS requires raw_uart framing"
    );
    let mut random = [0; 4];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random)
        .map_err(|_| anyhow::anyhow!("cannot generate recovery session identity"))?;
    let mut mesh = Mesh::new(role, u32::from_le_bytes(random));
    mesh.voice_enabled = std::env::var("GS_RRLS_VOICE").ok().as_deref() == Some("1");
    RADIO
        .set((serial.port.clone(), Arc::new(Mutex::new(mesh))))
        .map_err(|_| anyhow::anyhow!("RRLS radio already configured"))?;
    Ok(())
}

pub fn attach(port: &str) -> Option<Shared> {
    RADIO
        .get()
        .filter(|(p, _)| p == port)
        .map(|(_, s)| s.clone())
}

#[derive(Serialize)]
pub struct Status {
    pub enabled: bool,
    pub role: Option<Role>,
    pub forwarding: bool,
    pub relay_requested: bool,
    pub direct_age_ms: Option<u64>,
    pub queued_frames: usize,
    pub dropped_frames: u64,
    pub forwarded_frames: u64,
}
pub fn status() -> Status {
    if let Some((_, shared)) = RADIO.get() {
        let m = shared.lock().unwrap();
        let now = m.now();
        Status {
            enabled: true,
            role: Some(m.role),
            forwarding: m.active(now),
            relay_requested: m.fallback,
            direct_age_ms: m.last_direct.map(|v| now.saturating_sub(v)),
            queued_frames: m.queue.len(),
            dropped_frames: m.dropped,
            forwarded_frames: m.forwarded,
        }
    } else {
        Status {
            enabled: false,
            role: None,
            forwarding: false,
            relay_requested: false,
            direct_age_ms: None,
            queued_frames: 0,
            dropped_frames: 0,
            forwarded_frames: 0,
        }
    }
}

pub struct Outgoing {
    pub command: bool,
    pub bytes: Vec<u8>,
}
struct Queued {
    frame: Outgoing,
    at: u64,
    priority: u8,
}
pub struct Mesh {
    pub role: Role,
    voice_enabled: bool,
    started: Instant,
    session: u32,
    sequence: u32,
    last_health_sent: Option<u64>,
    last_position_sent: Option<u64>,
    primary_session: Option<u32>,
    primary_sequence: u32,
    health_received: Option<u64>,
    primary_direct: bool,
    last_direct: Option<u64>,
    direct_since: Option<u64>,
    fallback: bool,
    relay_seen: Option<u64>,
    last_ready_sequence: Option<u32>,
    last_ready_sent: Option<u64>,
    queue: VecDeque<Queued>,
    bytes: usize,
    dropped: u64,
    forwarded: u64,
    templates: VecDeque<(u32, FrameClass)>,
    credit_millibytes: u64,
    credit_at: u64,
}

impl Mesh {
    pub fn new(role: Role, session: u32) -> Self {
        Self {
            role,
            voice_enabled: false,
            started: Instant::now(),
            session,
            sequence: 0,
            last_health_sent: None,
            last_position_sent: None,
            primary_session: None,
            primary_sequence: 0,
            health_received: None,
            primary_direct: true,
            last_direct: None,
            direct_since: None,
            fallback: false,
            relay_seen: None,
            last_ready_sequence: None,
            last_ready_sent: None,
            queue: VecDeque::new(),
            bytes: 0,
            dropped: 0,
            forwarded: 0,
            templates: VecDeque::new(),
            credit_millibytes: 4_160_000,
            credit_at: 0,
        }
    }
    pub fn now(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
    fn direct(&mut self, now: u64) {
        if self.last_direct.is_none_or(|v| now.saturating_sub(v) > 750) {
            self.direct_since = Some(now);
        }
        self.last_direct = Some(now);
    }
    pub fn active(&self, now: u64) -> bool {
        match self.role {
            Role::Primary => {
                self.fallback
                    && self
                        .relay_seen
                        .is_some_and(|v| now.saturating_sub(v) < LEASE_MS)
            }
            Role::Rrls => {
                !self.primary_direct
                    && self
                        .health_received
                        .is_some_and(|v| now.saturating_sub(v) < LEASE_MS)
            }
        }
    }
    pub fn tick(&mut self, now: u64) {
        if self.role == Role::Primary {
            if self.last_direct.map_or(now, |v| now.saturating_sub(v)) >= LOSS_MS {
                self.fallback = true;
            } else if self
                .direct_since
                .is_some_and(|v| now.saturating_sub(v) >= RESTORE_MS)
            {
                self.fallback = false;
            }
            if self
                .last_health_sent
                .is_none_or(|v| now.saturating_sub(v) >= 500)
            {
                let mut payload = self.session.to_le_bytes().to_vec();
                payload.extend_from_slice(&self.sequence.to_le_bytes());
                payload.push(u8::from(!self.fallback));
                self.sequence = self.sequence.wrapping_add(1);
                self.last_health_sent = Some(now);
                self.enqueue(
                    Outgoing {
                        command: true,
                        bytes: encode(HEALTH, &payload),
                    },
                    255,
                    now,
                );
            }
        } else {
            if !self.active(now) {
                self.clear();
                if !self.voice_enabled {
                    self.last_ready_sent = None;
                }
            }
            let peer_fresh = self
                .health_received
                .is_some_and(|last| now.saturating_sub(last) < LEASE_MS);
            if (self.active(now) || self.voice_enabled && peer_fresh)
                && self
                    .last_ready_sent
                    .is_none_or(|last| now.saturating_sub(last) >= 500)
            {
                let mut ready = self.primary_session.unwrap().to_le_bytes().to_vec();
                ready.extend_from_slice(&self.primary_sequence.to_le_bytes());
                self.enqueue(
                    Outgoing {
                        command: true,
                        bytes: encode(READY, &ready),
                    },
                    255,
                    now,
                );
                self.last_ready_sent = Some(now);
            }
        }
        if self
            .last_position_sent
            .is_none_or(|last| now.saturating_sub(last) >= 5_000)
        {
            if let Some(fix) = crate::antenna::local_fix()
                && let Some(encoded) = crate::antenna::encode_position(&fix)
            {
                let mut payload = Vec::with_capacity(39);
                payload.push(if self.role == Role::Primary { 0 } else { 1 });
                payload.extend_from_slice(&encoded);
                self.enqueue(
                    Outgoing {
                        command: true,
                        bytes: encode(POSITION, &payload),
                    },
                    250,
                    now,
                );
                self.last_position_sent = Some(now);
            }
        }
    }
    fn clear(&mut self) {
        let old = self.queue.len();
        self.queue
            .retain(|q| matches!(q.frame.bytes.get(3), Some(&POSITION) | Some(&VOICE)));
        self.dropped += (old - self.queue.len()) as u64;
        self.bytes = self.queue.iter().map(|q| q.frame.bytes.len()).sum();
    }
    fn enqueue(&mut self, frame: Outgoing, priority: u8, now: u64) {
        if frame.bytes.len() > MAX_PAYLOAD + 8 {
            self.dropped += 1;
            return;
        }
        // GPS and health are current values, not history. Keep only the newest.
        if priority == 255
            && (frame.bytes.get(3) == Some(&HEALTH)
                || !frame
                    .bytes
                    .get(4..)
                    .is_some_and(|body| body.starts_with(b"SDT")))
        {
            if let Some(i) = self
                .queue
                .iter()
                .position(|q| q.priority == 255 && q.frame.bytes.get(3) == frame.bytes.get(3))
            {
                let old = self.queue.remove(i).unwrap();
                self.bytes -= old.frame.bytes.len();
                self.dropped += 1;
            }
        }
        while self.queue.len() >= MAX_FRAMES || self.bytes + frame.bytes.len() > MAX_BYTES {
            let Some((i, p)) = self
                .queue
                .iter()
                .enumerate()
                .min_by_key(|(_, q)| q.priority)
                .map(|(i, q)| (i, q.priority))
            else {
                break;
            };
            if p > priority {
                self.dropped += 1;
                return;
            }
            let old = self.queue.remove(i).unwrap();
            self.bytes -= old.frame.bytes.len();
            self.dropped += 1;
        }
        self.bytes += frame.bytes.len();
        self.queue.push_back(Queued {
            frame,
            at: now,
            priority,
        });
    }
    pub fn pop(&mut self, now: u64) -> Option<Outgoing> {
        // At 64 kbps, a two-hop relay must leave airtime for the rocket and
        // ground uplink. Bound accepted UART work before the modem's FIFO.
        self.credit_millibytes = self
            .credit_millibytes
            .saturating_add(now.saturating_sub(self.credit_at).saturating_mul(2_000))
            .min(4_160_000);
        self.credit_at = now;
        loop {
            let i = self
                .queue
                .iter()
                .enumerate()
                .max_by_key(|(i, q)| (q.priority, std::cmp::Reverse(*i)))
                .map(|(i, _)| i)?;
            let q = &self.queue[i];
            if now.saturating_sub(q.at)
                > if q.frame.bytes.get(3) == Some(&VOICE) {
                    180
                } else if q.priority == 255 {
                    1500
                } else {
                    250
                }
            {
                let old = self.queue.remove(i).unwrap();
                self.bytes -= old.frame.bytes.len();
                self.dropped += 1;
                continue;
            }
            let cost = (q.frame.bytes.len() as u64 + 4) * 1000;
            if self.role == Role::Rrls && self.credit_millibytes < cost {
                return None;
            }
            let q = self.queue.remove(i).unwrap();
            self.bytes -= q.frame.bytes.len();
            if self.role == Role::Rrls {
                self.credit_millibytes -= cost;
            }
            self.forwarded += 1;
            return Some(q.frame);
        }
    }
    pub fn voice_available(&self, now: u64) -> bool {
        self.voice_enabled
            && match self.role {
                Role::Primary => self
                    .relay_seen
                    .is_some_and(|last| now.saturating_sub(last) < LEASE_MS),
                Role::Rrls => self
                    .health_received
                    .is_some_and(|last| now.saturating_sub(last) < LEASE_MS),
            }
    }
    pub fn queue_voice(&mut self, payload: &[u8], now: u64) {
        if !self.voice_available(now) || payload.len() > crate::radio_voice::MAX_PACKET {
            return;
        }
        while self
            .queue
            .iter()
            .filter(|q| q.frame.bytes.get(3) == Some(&VOICE))
            .count()
            >= 3
        {
            let index = self
                .queue
                .iter()
                .position(|q| q.frame.bytes.get(3) == Some(&VOICE))
                .unwrap();
            let old = self.queue.remove(index).unwrap();
            self.bytes -= old.frame.bytes.len();
            self.dropped += 1;
        }
        self.enqueue(
            Outgoing {
                command: true,
                bytes: encode(VOICE, payload),
            },
            1,
            now,
        );
    }
    fn classify(&mut self, bytes: &[u8]) -> Option<FrameClass> {
        fn id(bytes: &[u8], off: &mut usize) -> Option<u32> {
            let mut value = 0u32;
            for shift in (0..35).step_by(7) {
                let b = *bytes.get(*off)?;
                *off += 1;
                if shift == 28 && b > 15 {
                    return None;
                }
                value |= u32::from(b & 127) << shift;
                if b & 128 == 0 {
                    return Some(value);
                }
            }
            None
        }
        fn canonical(bytes: &[u8]) -> Option<FrameClass> {
            let p = sedsnet::wire_format::peek_frame_info(bytes).ok()?;
            let gps = p.envelope.ty == crate::telemetry_schema::data_type("GPS_DATA");
            Some(FrameClass {
                priority: if gps {
                    255
                } else if p.reliable.is_some() {
                    254
                } else {
                    sedsnet::transport_priority(p.envelope.ty).min(254)
                },
                avionics: matches!(p.envelope.sender.as_ref(), "RF" | "FC" | "PB"),
            })
        }
        if !bytes.starts_with(b"SDT") {
            return canonical(bytes);
        }
        if bytes.len() < 8 {
            return None;
        }
        let end = bytes.len() - 4;
        if crc(&bytes[..end]) != u32::from_le_bytes(bytes[end..].try_into().ok()?) {
            return None;
        }
        let body = &bytes[4..end];
        match bytes[3] {
            1 => {
                let mut off = 0;
                let template = id(body, &mut off)?;
                if let Some(class) = canonical(&body[off..]) {
                    if let Some(i) = self.templates.iter().position(|(key, _)| *key == template) {
                        self.templates.remove(i);
                    }
                    if self.templates.len() >= 256 {
                        self.templates.pop_front();
                    }
                    self.templates.push_back((template, class));
                    Some(class)
                } else {
                    canonical(body)
                }
            }
            2 | 4 | 5 => {
                let mut off = 1;
                let template = id(body, &mut off)?;
                self.templates
                    .iter()
                    .find(|(key, _)| *key == template)
                    .map(|(_, class)| *class)
            }
            3 => Some(FrameClass {
                priority: 253,
                avionics: false,
            }),
            _ => None,
        }
    }

    /// Local router traffic is disabled on RRLS: it observes, never owns launch controls.
    pub fn local_tx(&self, command: bool, payload: &[u8], now: u64) -> Option<Outgoing> {
        if self.role == Role::Rrls {
            return None;
        }
        Some(Outgoing {
            command: true,
            bytes: encode(
                if self.active(now) {
                    if command { UP_COMMAND } else { UP_DATA }
                } else if command {
                    AV_BAY_COMMAND
                } else {
                    AV_BAY_DATA
                },
                payload,
            ),
        })
    }
    /// Returns only locally delivered data/commands. Wrapped traffic is never relayed twice.
    pub fn incoming(&mut self, command: bool, payload: &[u8], now: u64) -> Option<Outgoing> {
        if command && payload.starts_with(b"RR") {
            let (kind, data) = decode(payload)?;
            match (self.role, kind) {
                (_, VOICE) if self.voice_available(now) => {
                    crate::radio_voice::receive(self.role, data);
                    None
                }

                (_, POSITION)
                    if data.len() == 39
                        && data[0] == if self.role == Role::Primary { 1 } else { 0 } =>
                {
                    if let Some(fix) = crate::antenna::decode_position(&data[1..]) {
                        crate::antenna::accept_remote_fix(fix);
                    }
                    None
                }
                (Role::Primary, READY) if data.len() == 8 => {
                    let session = u32::from_le_bytes(data[..4].try_into().ok()?);
                    let seq = u32::from_le_bytes(data[4..].try_into().ok()?);
                    if session != self.session {
                        return None;
                    }
                    if self.last_ready_sequence.is_some_and(|old| {
                        let delta = seq.wrapping_sub(old);
                        delta == 0 || delta > i32::MAX as u32
                    }) {
                        return None;
                    }
                    if self.sequence.wrapping_sub(seq) == 0 || self.sequence.wrapping_sub(seq) > 4 {
                        return None;
                    }
                    self.last_ready_sequence = Some(seq);
                    self.relay_seen = Some(now);
                    None
                }
                (Role::Rrls, HEALTH) if data.len() == 9 && data[8] <= 1 => {
                    let session = u32::from_le_bytes(data[..4].try_into().ok()?);
                    let seq = u32::from_le_bytes(data[4..8].try_into().ok()?);
                    if self.primary_session == Some(session) {
                        let delta = seq.wrapping_sub(self.primary_sequence);
                        if delta == 0 || delta > i32::MAX as u32 {
                            return None;
                        }
                    } else if self
                        .health_received
                        .is_some_and(|v| now.saturating_sub(v) < LEASE_MS)
                    {
                        return None;
                    }
                    self.primary_session = Some(session);
                    self.primary_sequence = seq;
                    self.primary_direct = data[8] != 0;
                    self.health_received = Some(now);
                    if !self.active(now) {
                        self.clear();
                    }
                    None
                }
                (Role::Primary, DOWN_DATA) => Some(Outgoing {
                    command: false,
                    bytes: data.to_vec(),
                }),
                (Role::Primary, DOWN_COMMAND) => Some(Outgoing {
                    command: true,
                    bytes: data.to_vec(),
                }),
                (Role::Rrls, UP_DATA | UP_COMMAND) if self.active(now) => {
                    self.enqueue(
                        Outgoing {
                            command: true,
                            bytes: encode(
                                if kind == UP_COMMAND {
                                    AV_BAY_COMMAND
                                } else {
                                    AV_BAY_DATA
                                },
                                data,
                            ),
                        },
                        254,
                        now,
                    );
                    None
                }
                _ => None,
            }
        } else {
            let frame_class = if !command {
                self.classify(payload)
            } else {
                None
            };
            if self.role == Role::Primary {
                // RF window advertisements are nine bytes; ground replies are seven.
                if command && payload.starts_with(b"RS\x01") && payload.len() == 9 {
                    self.direct(now);
                }
                if !command && frame_class.is_some_and(|class| class.avionics) {
                    self.direct(now);
                }
            } else if self.active(now) {
                let priority = if command {
                    254
                } else {
                    frame_class.map_or(
                        if payload.starts_with(b"SDT") {
                            254
                        } else {
                            128
                        },
                        |class| class.priority,
                    )
                };
                self.enqueue(
                    Outgoing {
                        command: true,
                        bytes: encode(if command { DOWN_COMMAND } else { DOWN_DATA }, payload),
                    },
                    priority,
                    now,
                );
            }
            // RRLS never answers the radio's scheduling windows itself.
            if command && self.role == Role::Rrls {
                None
            } else {
                Some(Outgoing {
                    command,
                    bytes: payload.to_vec(),
                })
            }
        }
    }
}
fn crc(bytes: &[u8]) -> u32 {
    let mut state = u32::MAX;
    for b in bytes {
        state ^= u32::from(*b);
        for _ in 0..8 {
            state = (state >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(state & 1));
        }
    }
    !state
}
fn encode(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 8);
    v.extend_from_slice(b"RR\x01");
    v.push(kind);
    v.extend_from_slice(payload);
    v.extend_from_slice(&crc(&v).to_le_bytes());
    v
}
fn decode(bytes: &[u8]) -> Option<(u8, &[u8])> {
    if bytes.len() < 8 || bytes.len() > MAX_PAYLOAD + 8 || &bytes[..3] != b"RR\x01" {
        return None;
    }
    let n = bytes.len() - 4;
    (crc(&bytes[..n]) == u32::from_le_bytes(bytes[n..].try_into().ok()?))
        .then_some((bytes[3], &bytes[4..n]))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn health(session: u32, seq: u32, direct: bool) -> Vec<u8> {
        let mut p = session.to_le_bytes().to_vec();
        p.extend_from_slice(&seq.to_le_bytes());
        p.push(direct as u8);
        encode(HEALTH, &p)
    }
    #[test]
    fn optional_voice_presence_does_not_enable_standby_forwarding_or_flood_ready() {
        let mut primary = Mesh::new(Role::Primary, 1);
        primary.voice_enabled = true;
        let mut recovery = Mesh::new(Role::Rrls, 2);
        recovery.voice_enabled = true;
        primary.tick(0);
        let health = primary.pop(0).unwrap();
        recovery.incoming(true, &health.bytes, 0);
        recovery.tick(0);
        let ready = recovery.pop(0).unwrap();
        assert_eq!(decode(&ready.bytes).unwrap().0, READY);
        primary.incoming(true, &ready.bytes, 0);
        assert!(primary.voice_available(0));
        assert!(recovery.voice_available(0));
        assert!(!primary.active(0));
        assert!(!recovery.active(0));
        for now in 1..500 {
            recovery.tick(now);
            assert!(recovery.pop(now).is_none());
        }
        assert!(!primary.voice_available(LEASE_MS));
    }
    #[test]
    fn paired_radio_voice_decodes_once_without_echo_and_stops_after_link_loss() {
        let mut primary = Mesh::new(Role::Primary, 1);
        let mut recovery = Mesh::new(Role::Rrls, 2);
        primary.voice_enabled = true;
        recovery.voice_enabled = true;
        primary.tick(0);
        recovery.incoming(true, &primary.pop(0).unwrap().bytes, 0);
        recovery.tick(0);
        primary.incoming(true, &recovery.pop(0).unwrap().bytes, 0);
        let mut audio = crate::radio_voice::subscribe();
        for role in [Role::Primary, Role::Rrls] {
            let (sender, receiver) = if role == Role::Primary {
                (&mut primary, &mut recovery)
            } else {
                (&mut recovery, &mut primary)
            };
            sender.queue_voice(&crate::radio_voice::test_packet(role), 1);
            let packet = sender.pop(1).unwrap();
            assert!(receiver.incoming(true, &packet.bytes, 1).is_none());
            assert_eq!(audio.try_recv().unwrap().pcm.len(), 960);
            assert!(receiver.incoming(true, &packet.bytes, 2).is_none());
            assert!(audio.try_recv().is_err(), "duplicate audio cannot replay");
            assert!(receiver.pop(2).is_none(), "received voice cannot echo");
            receiver.incoming(true, &packet.bytes, LEASE_MS + 1);
            sender.queue_voice(&crate::radio_voice::test_packet(role), LEASE_MS + 1);
            assert!(sender.pop(LEASE_MS + 1).is_none());
            assert!(audio.try_recv().is_err());
        }
    }
    #[test]
    fn ten_minutes_of_voice_congestion_never_delays_command_or_gps_turns() {
        let mut mesh = Mesh::new(Role::Primary, 1);
        mesh.voice_enabled = true;
        let mut controls = 0;
        let mut gps = 0;
        for now in (0..600_000).step_by(20) {
            // Repeated ten-second disconnects must stop voice admission.
            let connected = now % 60_000 < 50_000;
            if connected {
                mesh.relay_seen = Some(now);
            }
            let before = mesh.queue.len();
            mesh.queue_voice(&[0; 71], now);
            if !mesh.voice_available(now) {
                assert_eq!(mesh.queue.len(), before);
            }
            if now % 100 == 0 {
                mesh.enqueue(
                    Outgoing {
                        command: true,
                        bytes: encode(UP_COMMAND, b"command"),
                    },
                    254,
                    now,
                );
                mesh.enqueue(
                    Outgoing {
                        command: true,
                        bytes: encode(DOWN_DATA, b"gps"),
                    },
                    255,
                    now,
                );
                assert_eq!(decode(&mesh.pop(now).unwrap().bytes).unwrap().1, b"gps");
                gps += 1;
                assert_eq!(decode(&mesh.pop(now).unwrap().bytes).unwrap().1, b"command");
                controls += 1;
            }
            // A deliberately slow radio drains only one voice frame per 120 ms.
            if now % 120 == 0 {
                mesh.pop(now);
            }
            assert!(mesh.queue.len() <= 3);
            assert!(mesh.bytes <= 3 * (crate::radio_voice::MAX_PACKET + 8));
        }
        assert_eq!((controls, gps), (6000, 6000));
        assert!(
            mesh.dropped > 0,
            "congestion sheds voice instead of growing memory"
        );
        assert!(
            mesh.pop(600_200).is_none(),
            "old audio expires across a reconnect"
        );
    }
    #[test]
    fn voice_is_bounded_expires_and_yields_to_gps_and_commands() {
        let mut mesh = Mesh::new(Role::Primary, 1);
        mesh.voice_enabled = true;
        mesh.relay_seen = Some(0);
        let payload = vec![0; 71];
        for _ in 0..100 {
            mesh.queue_voice(&payload, 0);
        }
        assert_eq!(mesh.queue.len(), 3);
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: encode(UP_COMMAND, b"command"),
            },
            254,
            0,
        );
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: encode(DOWN_DATA, b"gps"),
            },
            255,
            0,
        );
        assert_eq!(decode(&mesh.pop(0).unwrap().bytes).unwrap().0, DOWN_DATA);
        assert_eq!(decode(&mesh.pop(0).unwrap().bytes).unwrap().0, UP_COMMAND);
        assert!(mesh.pop(181).is_none(), "expired voice is never replayed");
        assert!(mesh.queue.is_empty());
        mesh.queue_voice(&vec![0; crate::radio_voice::MAX_PACKET + 1], 181);
        assert!(
            mesh.pop(181).is_none(),
            "video-sized payloads cannot enter voice"
        );
    }
    #[test]
    fn direct_ground_uplink_is_namespaced_away_from_rocket_templates() {
        let p = Mesh::new(Role::Primary, 1);
        let mut r = Mesh::new(Role::Rrls, 2);
        let f = p
            .local_tx(false, b"ground-side compact template", 0)
            .unwrap();
        assert!(f.command);
        assert_eq!(decode(&f.bytes).unwrap().0, AV_BAY_DATA);
        assert!(r.incoming(f.command, &f.bytes, 0).is_none());
        assert!(r.pop(0).is_none());
    }
    #[test]
    fn compact_gps_keeps_priority_without_decoding_or_coalescing_payloads() {
        crate::telemetry_schema::initialize().unwrap();
        let p = sedsnet::packet::Packet::new(
            crate::telemetry_schema::data_type("GPS_DATA"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "RF",
            1,
            vec![0; 12].into(),
        )
        .unwrap();
        let mut full = vec![7];
        full.extend_from_slice(&sedsnet::wire_format::pack_packet(&p));
        let wrap = |kind: u8, body: &[u8]| {
            let mut b = b"SDT".to_vec();
            b.push(kind);
            b.extend_from_slice(body);
            b.extend_from_slice(&crc(&b).to_le_bytes());
            b
        };
        let mut r = Mesh::new(Role::Rrls, 2);
        r.incoming(true, &health(1, 0, false), 0);
        assert_eq!(r.classify(&wrap(1, &full)).unwrap().priority, 255);
        let compact = wrap(4, &[0, 7, 12, 1, 0, 0]);
        assert_eq!(r.classify(&compact).unwrap().priority, 255);
        r.incoming(false, &compact, 1);
        r.incoming(false, &compact, 2);
        assert_eq!(r.queue.len(), 2, "opaque delta frames must retain order");
        let mut bad = compact.clone();
        bad[5] ^= 1;
        assert!(r.classify(&bad).is_none());
    }

    #[test]
    fn disconnected_rrls_keeps_direct_commands_and_expired_proofs_restore_direct() {
        let mut p = Mesh::new(Role::Primary, 1);
        p.tick(3001);
        assert!(!p.active(3001));
        assert_eq!(
            decode(&p.local_tx(false, b"cmd", 3001).unwrap().bytes)
                .unwrap()
                .0,
            AV_BAY_DATA
        );
        let mut proof = 1u32.to_le_bytes().to_vec();
        proof.extend_from_slice(&0u32.to_le_bytes());
        p.incoming(true, &encode(READY, &proof), 3001);
        assert!(p.active(3001));
        assert_eq!(
            decode(&p.local_tx(false, b"cmd", 3001).unwrap().bytes)
                .unwrap()
                .0,
            UP_DATA
        );
        p.incoming(true, &encode(READY, &proof), 4999);
        assert!(
            !p.active(5001),
            "duplicate readiness cannot renew an unplugged relay"
        );
        assert_eq!(
            decode(&p.local_tx(false, b"cmd", 5001).unwrap().bytes)
                .unwrap()
                .0,
            AV_BAY_DATA
        );
    }

    #[test]
    fn equal_priority_commands_remain_fifo() {
        let mut m = Mesh::new(Role::Rrls, 2);
        for b in 0..10 {
            m.enqueue(
                Outgoing {
                    command: false,
                    bytes: vec![b],
                },
                254,
                0,
            );
        }
        for b in 0..10 {
            assert_eq!(m.pop(1).unwrap().bytes, vec![b]);
        }
    }
    #[test]
    fn gps_uses_real_schema_priority_and_latest_value() {
        crate::telemetry_schema::initialize().unwrap();
        let mut r = Mesh::new(Role::Rrls, 2);
        r.incoming(true, &health(1, 0, false), 0);
        r.incoming(false, b"bulk fragment", 1);
        for timestamp in [10, 20] {
            let pkt = sedsnet::packet::Packet::new(
                crate::telemetry_schema::data_type("GPS_DATA"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "RF",
                timestamp,
                vec![0; 12].into(),
            )
            .unwrap();
            r.incoming(false, &sedsnet::wire_format::pack_packet(&pkt), 1);
        }
        assert_eq!(r.queue.len(), 2);
        let f = r.pop(1).unwrap();
        let (_, bytes) = decode(&f.bytes).unwrap();
        assert_eq!(
            sedsnet::wire_format::unpack_packet(bytes)
                .unwrap()
                .timestamp(),
            20
        );
    }
    #[test]
    fn ten_minutes_of_mesh_loss_recovery_and_commands_remain_bounded() {
        let mut p = Mesh::new(Role::Primary, 1);
        let mut r = Mesh::new(Role::Rrls, 2);
        let mut downlinks = 0;
        let mut uplinks = 0;
        let mut relayed = 0;
        for now in (0..600_000).step_by(50) {
            let direct = now < 60_000 || (120_000..240_000).contains(&now) || now >= 300_000;
            if direct {
                p.direct(now);
            }
            p.tick(now);
            while let Some(frame) = p.pop(now) {
                r.incoming(frame.command, &frame.bytes, now);
            }
            r.tick(now);
            if now % 200 == 0 {
                if direct {
                    downlinks += 1;
                }
                r.incoming(false, b"unchanged opaque telemetry fragment", now);
            }
            if !direct && now % 1000 == 0 && p.active(now) {
                let f = p.local_tx(false, b"original command bytes", now).unwrap();
                r.incoming(f.command, &f.bytes, now);
            }
            while let Some(frame) = r.pop(now) {
                if decode(&frame.bytes).is_some_and(|(kind, _)| kind == AV_BAY_DATA) {
                    assert_eq!(decode(&frame.bytes).unwrap().1, b"original command bytes");
                    uplinks += 1;
                } else if frame.command {
                    if let Some(data) = p.incoming(true, &frame.bytes, now) {
                        assert_eq!(data.bytes, b"unchanged opaque telemetry fragment");
                        downlinks += 1;
                        relayed += 1;
                    }
                } else {
                    assert_eq!(frame.bytes, b"original command bytes");
                    uplinks += 1;
                }
            }
            assert!(r.queue.len() <= MAX_FRAMES && r.bytes <= MAX_BYTES);
        }
        assert!(relayed > 550 && uplinks > 100 && downlinks > 2900);
        assert!(!p.active(599_950) && !r.active(599_950));
    }

    #[test]
    fn standby_is_receive_only_and_never_repeats_packets() {
        let mut m = Mesh::new(Role::Rrls, 2);
        m.incoming(true, &health(1, 0, true), 0);
        assert_eq!(m.incoming(false, b"data", 1).unwrap().bytes, b"data");
        assert!(m.pop(1).is_none());
        assert!(m.local_tx(false, b"command", 1).is_none());
    }
    #[test]
    fn loss_activates_only_a_live_primary_lease() {
        let mut m = Mesh::new(Role::Rrls, 2);
        assert!(!m.active(10_000));
        m.incoming(true, &health(1, 0, false), 10_000);
        assert!(m.active(10_001));
        assert!(!m.active(12_000));
    }
    #[test]
    fn corrupt_and_replayed_health_cannot_extend_the_lease() {
        let mut m = Mesh::new(Role::Rrls, 2);
        let h = health(1, 3, false);
        m.incoming(true, &h, 0);
        m.incoming(true, &h, 1999);
        assert!(!m.active(2001));
        let mut bad = health(1, 4, false);
        bad[8] ^= 1;
        m.incoming(true, &bad, 3000);
        assert!(!m.active(3001));
    }
    #[test]
    fn active_relay_preserves_fragments_and_does_not_echo_wrappers() {
        let mut r = Mesh::new(Role::Rrls, 2);
        r.incoming(true, &health(1, 0, false), 0);
        r.incoming(false, b"opaque transport fragment", 1);
        let f = r.pop(1).unwrap();
        let mut p = Mesh::new(Role::Primary, 1);
        assert_eq!(
            p.incoming(true, &f.bytes, 1).unwrap().bytes,
            b"opaque transport fragment"
        );
        assert!(r.incoming(true, &f.bytes, 2).is_none());
        assert!(r.pop(2).is_none());
    }
    #[test]
    fn uplink_is_not_replayed_after_recovery() {
        let mut r = Mesh::new(Role::Rrls, 2);
        r.incoming(true, &health(1, 0, false), 0);
        r.incoming(true, &encode(UP_DATA, b"cmd"), 1);
        r.incoming(true, &health(1, 1, true), 2);
        assert!(r.pop(2).is_none());
        r.incoming(true, &encode(UP_DATA, b"cmd"), 3);
        assert!(r.pop(3).is_none());
    }
    #[test]
    fn relayed_frames_do_not_restore_the_direct_link() {
        let mut p = Mesh::new(Role::Primary, 1);
        p.tick(3001);
        let mut proof = 1u32.to_le_bytes().to_vec();
        proof.extend_from_slice(&0u32.to_le_bytes());
        p.incoming(true, &encode(READY, &proof), 3001);
        assert!(p.active(3001));
        p.incoming(true, &encode(DOWN_DATA, b"data"), 4000);
        p.tick(4000);
        assert!(p.active(4000));
    }
    #[test]
    fn stable_direct_recovery_has_hysteresis() {
        let mut p = Mesh::new(Role::Primary, 1);
        p.tick(3001);
        for now in (3100..=5100).step_by(100) {
            p.direct(now);
            p.tick(now);
        }
        assert!(!p.active(5100));
    }
    #[test]
    fn queue_is_bounded_and_gps_precedes_bulk() {
        let mut m = Mesh::new(Role::Rrls, 2);
        for _ in 0..1000 {
            m.enqueue(
                Outgoing {
                    command: false,
                    bytes: vec![0; 1000],
                },
                128,
                0,
            );
        }
        assert!(m.queue.len() <= MAX_FRAMES && m.bytes <= MAX_BYTES);
        m.enqueue(
            Outgoing {
                command: true,
                bytes: encode(DOWN_DATA, b"gps"),
            },
            255,
            1,
        );
        assert_eq!(decode(&m.pop(1).unwrap().bytes).unwrap().1, b"gps");
        assert!(m.dropped > 0);
    }
    #[test]
    fn obsolete_traffic_expires() {
        let mut m = Mesh::new(Role::Rrls, 2);
        m.enqueue(
            Outgoing {
                command: false,
                bytes: vec![1],
            },
            128,
            0,
        );
        assert!(m.pop(251).is_none());
    }
    #[test]
    fn health_session_restart_requires_expired_previous_lease() {
        let mut m = Mesh::new(Role::Rrls, 2);
        m.incoming(true, &health(1, 0, true), 0);
        m.incoming(true, &health(3, 0, false), 1);
        assert!(!m.active(1));
        m.incoming(true, &health(3, 0, false), 2001);
        assert!(m.active(2001));
    }
}

/// RRLS has no fill-system hardware.

pub fn voice_transport() -> Option<Shared> {
    RADIO
        .get()
        .map(|(_, shared)| shared.clone())
        .filter(|shared| shared.lock().unwrap().voice_enabled)
}
pub fn voice_status() -> (bool, bool) {
    if let Some(shared) = voice_transport() {
        let mesh = shared.lock().unwrap();
        (true, mesh.voice_available(mesh.now()))
    } else {
        (false, false)
    }
}
