//! Single recovery relay over the RFD asynchronous mesh. Wi-Fi is local only.
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

#[cfg(test)]
#[path = "comms/virtual_uart.rs"]
mod receipt_test_wire;

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
const SETTINGS: u8 = 10;
const MAX_SETTINGS_INBOX: usize = 4;
const SETTINGS_PEER_AGE_MS: u64 = 15_000;
#[derive(Clone, Copy)]
struct FrameClass {
    priority: u8,
}
struct TemplateClass {
    id: u32,
    uplink: bool,
    class: FrameClass,
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

fn role_for_link(
    link: &crate::comms_config::CommsLinkConfig,
    configured: Option<&str>,
) -> anyhow::Result<Option<Role>> {
    use crate::comms_config::{CommsLinkConfig as L, SerialProtocol};
    match configured {
        Some("off") => Ok(None),
        Some("primary") => Ok(Some(Role::Primary)),
        Some("rrls") => Ok(Some(Role::Rrls)),
        Some(_) => anyhow::bail!("GS_RRLS_ROLE must be off, primary, or rrls"),
        None => Ok(match link {
            L::Serial { serial }
            | L::CustomSerial { serial }
            | L::RaspberryPiGpioUart { serial }
                if serial.protocol == SerialProtocol::RawUart =>
            {
                Some(Role::Primary)
            }
            _ => None,
        }),
    }
}

pub fn configure(link: &crate::comms_config::CommsLinkConfig) -> anyhow::Result<()> {
    use crate::comms_config::{CommsLinkConfig as L, SerialProtocol};
    let configured = std::env::var("GS_RRLS_ROLE").ok();
    let Some(role) = role_for_link(link, configured.as_deref())? else {
        log::info!(
            "Radio recovery disabled: {}",
            if configured.as_deref() == Some("off") {
                "GS_RRLS_ROLE=off"
            } else {
                "AV-bay interface is not raw UART"
            }
        );
        return Ok(());
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
    log::info!(
        "Radio recovery role={role:?} port={} automatic={}",
        serial.port,
        configured.is_none()
    );
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
            queued_frames: m.queue.len() + usize::from(m.active_frame_count),
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
    mode_reply: bool,
    frame: Outgoing,
    at: u64,
    priority: u8,
    independent_discovery: bool,
    sdt_kind: u8,
    sdt_template: u32,
}
impl Queued {
    fn closes_packet_batch(&self) -> bool {
        !self.independent_discovery
            && decode(&self.frame.bytes).is_some_and(|(_, body)| {
                crate::comms::closes_radio_packet_batch(self.priority, body)
            })
    }
    fn boundary_contract(&self) -> Option<(u64, bool)> {
        if self.independent_discovery {
            return None;
        }
        decode(&self.frame.bytes)
            .and_then(|(_, body)| crate::comms::radio_packet_boundary_contract(self.priority, body))
    }
}
/// One owned frame, still charged to the mesh until serial delivery finishes.
/// Keeping metadata with the frame permits retry without copying its payload.
pub(crate) struct Transmission {
    queued: Queued,
    follower: Option<Queued>,
    id: u64,
}
impl Transmission {
    pub(crate) fn frame(&self) -> &Outgoing {
        &self.queued.frame
    }
    pub(crate) fn follower_frame(&self) -> Option<&Outgoing> {
        self.follower.as_ref().map(|queued| &queued.frame)
    }
    pub(crate) fn batch_contracts(&self) -> Option<[(u64, bool); 2]> {
        Some([
            Self::batch_contract(&self.queued)?,
            Self::batch_contract(self.follower.as_ref()?)?,
        ])
    }
    fn batch_contract(queued: &Queued) -> Option<(u64, bool)> {
        if queued.priority < 254 || queued.independent_discovery {
            return None;
        }
        let (_, body) = decode(&queued.frame.bytes)?;
        crate::comms::radio_batch_control_contract(queued.priority, body)
    }
    /// Ordinary-lane wire bytes eligible for funding only after full successful IO.
    /// Dependent SDT stays in its existing lane; independent discovery/control does not.
    pub(crate) fn successful_ordinary_service_bytes(&self) -> usize {
        if self.queued.priority < 254
            && !self.queued.closes_packet_batch()
            && !self.queued.independent_discovery
            && matches!(
                self.queued.frame.bytes.get(3),
                Some(&DOWN_DATA) | Some(&AV_BAY_DATA)
            )
        {
            self.queued.frame.bytes.len() + 4
        } else {
            0
        }
    }
    pub(crate) fn closes_packet_batch(&self) -> bool {
        self.queued.closes_packet_batch()
    }
    pub(crate) fn boundary_contract(&self) -> Option<(u64, bool)> {
        self.queued.boundary_contract()
    }
    #[cfg(test)]
    pub(crate) fn is_receipt(&self) -> bool {
        self.queued
            .boundary_contract()
            .is_some_and(|(_, receipt)| receipt)
    }
}

pub struct Mesh {
    pub role: Role,
    voice_enabled: bool,
    settings_peer_seen: Option<u64>,
    settings_inbox: VecDeque<Vec<u8>>,
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
    topology_peer_seen: Option<u64>,
    topology_relay_rx: Option<u64>,
    last_ready_sequence: Option<u32>,
    last_ready_sent: Option<u64>,
    queue: VecDeque<Queued>,
    bytes: usize,
    dropped: u64,
    forwarded: u64,
    templates: VecDeque<TemplateClass>,
    active_transmission: Option<u64>,
    active_frame_count: u8,
    active_cancelled: bool,
    active_survives_clear: bool,
    next_transmission: u64,
    discovery_balance_bytes: i32,
    receipt_service_pending: bool,
    active_fair_delta_bytes: i32,
    fair_overhead_bytes: u16,
}

impl Mesh {
    pub fn new(role: Role, session: u32) -> Self {
        Self {
            role,
            voice_enabled: false,
            settings_peer_seen: None,
            settings_inbox: VecDeque::new(),
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
            topology_peer_seen: None,
            topology_relay_rx: None,
            last_ready_sequence: None,
            last_ready_sent: None,
            queue: VecDeque::new(),
            bytes: 0,
            dropped: 0,
            forwarded: 0,
            templates: VecDeque::new(),
            active_transmission: None,
            active_frame_count: 0,
            active_cancelled: false,
            active_survives_clear: false,
            next_transmission: 0,
            discovery_balance_bytes: 0,
            receipt_service_pending: false,
            active_fair_delta_bytes: 0,
            fair_overhead_bytes: std::env::var("GS_RADIO_AIR_FRAME_OVERHEAD_BYTES")
                .ok()
                .and_then(|value| value.parse::<u16>().ok())
                .filter(|value| *value <= 256)
                .unwrap_or(16),
        }
    }
    pub fn now(&self) -> u64 {
        // A linked emulator can run far slower than wall time. Liveness, relay
        // leases and UART drain must advance on the same explicit test clock.
        // Ordinary stations and standalone HITL retain their monotonic clock.
        crate::simulation_clock_ms()
            .unwrap_or_else(|| self.started.elapsed().as_millis().min(u64::MAX as u128) as u64)
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
    pub(crate) fn recovery_air_share(&self, now: u64) -> Option<u8> {
        match self.role {
            // Match the advertised relay request even before READY is received.
            Role::Primary if self.fallback => Some(15),
            Role::Rrls if self.active(now) => Some(35),
            _ => None,
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
        self.active_cancelled |= self.active_transmission.is_some() && !self.active_survives_clear;
        let old = self.queue.len();
        let mut removed_bytes = 0;
        self.queue.retain(|q| {
            let keep = matches!(
                q.frame.bytes.get(3),
                Some(&POSITION) | Some(&VOICE) | Some(&SETTINGS)
            );
            if !keep {
                removed_bytes += q.frame.bytes.len();
            }
            keep
        });
        self.dropped += (old - self.queue.len()) as u64;
        // An active write is outside this queue and remains owned/charged.
        // Only bytes actually removed above release their reservation.
        self.bytes -= removed_bytes;
    }
    fn independent_discovery(frame: &Outgoing) -> bool {
        if !matches!(frame.bytes.get(3), Some(&DOWN_DATA) | Some(&AV_BAY_DATA)) {
            return false;
        }
        let Some(body) = frame.bytes.get(4..frame.bytes.len().saturating_sub(4)) else {
            return false;
        };
        // Canonical snapshots have no SDT dictionary/chunk dependency. Never
        // classify ground SDT through the avionics-side template namespace.
        !body.starts_with(b"SDT")
            && sedsnet::wire_format::peek_frame_info(body).is_ok_and(|packet| {
                packet.envelope.ty == sedsnet::config::DataType::DiscoveryAddress
            })
    }

    fn sdt_dependency(frame: &Outgoing) -> (u8, u32) {
        if !matches!(frame.bytes.get(3), Some(&DOWN_DATA) | Some(&AV_BAY_DATA)) {
            return (0, 0);
        }
        let Some(bytes) = frame.bytes.get(4..frame.bytes.len().saturating_sub(4)) else {
            return (0, 0);
        };
        if !bytes.starts_with(b"SDT") {
            return (0, 0);
        }
        if bytes.len() < 8 {
            return (2, 0);
        }
        let end = bytes.len() - 4;
        if crc(&bytes[..end]) != u32::from_le_bytes(bytes[end..].try_into().unwrap()) {
            return (2, 0);
        }
        let mut offset = match bytes[3] {
            1 => 4,
            2 | 4 | 5 => 5,
            // A chunk's template cannot be known without reassembly. It is a
            // conservative barrier within its explicit RR wire namespace.
            _ => return (2, 0),
        };
        let mut template = 0u32;
        for shift in (0..=28).step_by(7) {
            let Some(&byte) = bytes.get(offset).filter(|_| offset < end) else {
                return (2, 0);
            };
            offset += 1;
            if shift == 28 && byte > 15 {
                return (2, 0);
            }
            template |= u32::from(byte & 127) << shift;
            if byte & 128 == 0 {
                return (1, template);
            }
        }
        (2, 0)
    }

    fn dependency_ready(&self, index: usize) -> bool {
        let item = &self.queue[index];
        item.sdt_kind == 0
            || !self.queue.iter().take(index).any(|earlier| {
                earlier.frame.bytes.get(3) == item.frame.bytes.get(3)
                    && earlier.sdt_kind != 0
                    && (earlier.sdt_kind == 2
                        || item.sdt_kind == 2
                        || earlier.sdt_template == item.sdt_template)
            })
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
        while self.queue.len() + usize::from(self.active_frame_count) >= MAX_FRAMES
            || self.bytes + frame.bytes.len() > MAX_BYTES
        {
            let Some((i, p)) = self
                .queue
                .iter()
                .enumerate()
                .min_by_key(|(_, q)| q.priority)
                .map(|(i, q)| (i, q.priority))
            else {
                // The only reservation may belong to an active serial write;
                // it cannot be evicted or admitted a second time.
                self.dropped += 1;
                return;
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
        let independent_discovery = Self::independent_discovery(&frame);
        let (sdt_kind, sdt_template) = Self::sdt_dependency(&frame);
        self.queue.push_back(Queued {
            mode_reply: false,
            frame,
            at: now,
            priority,
            independent_discovery,
            sdt_kind,
            sdt_template,
        });
    }
    #[cfg(test)]
    pub(crate) fn begin_transmission(&mut self, now: u64) -> Option<Transmission> {
        self.begin_transmission_at_priority(now, 0)
    }
    pub(crate) fn begin_transmission_at_priority(
        &mut self,
        now: u64,
        minimum_priority: u8,
    ) -> Option<Transmission> {
        self.begin_matching_transmission(now, minimum_priority, false)
    }
    pub(crate) fn begin_critical_transmission(
        &mut self,
        now: u64,
        minimum_priority: u8,
    ) -> Option<Transmission> {
        self.begin_matching_transmission(now, minimum_priority, true)
    }
    /// One independent background reply; it never takes critical capacity/loans.
    pub(crate) fn begin_mode_reply_transmission(&mut self, now: u64) -> Option<Transmission> {
        if self.role != Role::Primary
            || self.active_transmission.is_some()
            || self
                .queue
                .iter()
                .enumerate()
                .any(|(i, q)| q.priority >= 254 && self.dependency_ready(i))
        {
            return None;
        }
        let index = self.queue.iter().position(|q| q.mode_reply)?;
        if now < self.queue[index].at || now.saturating_sub(self.queue[index].at) > 5_000 {
            let expired = self.queue.remove(index).unwrap();
            self.bytes -= expired.frame.bytes.len();
            self.dropped += 1;
            return None;
        }
        let id = self.next_transmission.checked_add(1)?;
        let queued = self.queue.remove(index).unwrap();
        self.next_transmission = id;
        self.active_transmission = Some(id);
        self.active_frame_count = 1;
        self.active_survives_clear = true;
        self.active_fair_delta_bytes = 0;
        Some(Transmission {
            queued,
            follower: None,
            id,
        })
    }
    /// Reserve at most two already-owned canonical controls. The second is
    /// the existing normal protected winner: never search past a higher SDT,
    /// READY or GPS entry, and never consume an ordinary/metadata service turn.
    pub(crate) fn begin_bounded_transmission(
        &mut self,
        now: u64,
        minimum_priority: u8,
        critical_only: bool,
        frame_budget: usize,
    ) -> Option<Transmission> {
        if frame_budget == 0 {
            return None;
        }
        let mut transmission =
            self.begin_matching_transmission(now, minimum_priority, critical_only)?;
        if frame_budget < 2
            || self.active_fair_delta_bytes != 0
            || Transmission::batch_contract(&transmission.queued).is_none()
        {
            return Some(transmission);
        }
        let candidate = self
            .queue
            .iter()
            .enumerate()
            .filter(|(index, item)| {
                !item.independent_discovery && item.priority >= 254 && self.dependency_ready(*index)
            })
            .max_by_key(|(index, item)| (item.priority, std::cmp::Reverse(*index)))
            .map(|(index, _)| index);
        if let Some(index) = candidate {
            let queued = &self.queue[index];
            let wire_bytes = transmission.queued.frame.bytes.len() + queued.frame.bytes.len() + 8;
            let expired =
                now.saturating_sub(queued.at) > if queued.priority == 255 { 1500 } else { 250 };
            if queued.priority >= minimum_priority
                && !expired
                && wire_bytes <= crate::comms::RADIO_COMMAND_BATCH_MAX_BYTES
                && Transmission::batch_contract(queued).is_some()
            {
                transmission.follower = self.queue.remove(index);
                self.active_frame_count = 2;
                self.active_survives_clear = false;
            }
        }
        Some(transmission)
    }

    fn begin_matching_transmission(
        &mut self,
        now: u64,
        minimum_priority: u8,
        critical_only: bool,
    ) -> Option<Transmission> {
        if self.active_transmission.is_some() {
            return None;
        }
        // Never reuse a reservation identity after wraparound.
        let id = self.next_transmission.checked_add(1)?;
        // The caller gates this reservation through its shared physical UART
        // Capacity. Mesh retains ownership/priorities; it has no second bucket.
        loop {
            let protected = self
                .queue
                .iter()
                .enumerate()
                .filter(|(index, item)| {
                    !item.independent_discovery
                        && (if critical_only {
                            item.priority >= minimum_priority && item.closes_packet_batch()
                        } else {
                            item.priority >= 254
                        })
                        && self.dependency_ready(*index)
                })
                .max_by_key(|(index, item)| (item.priority, std::cmp::Reverse(*index)))
                .map(|(index, _)| index);
            if critical_only && protected.is_none() {
                return None;
            }
            let discovery = if critical_only {
                None
            } else {
                self.queue
                    .iter()
                    .position(|item| item.independent_discovery)
            };
            // Only the ordinary head may yield into service: a later packet
            // must never leapfrog an earlier opaque SDT bootstrap or follower.
            let ordinary = if critical_only {
                None
            } else {
                self.queue
                    .iter()
                    .enumerate()
                    .find(|(index, item)| {
                        item.priority < 254
                            && !item.independent_discovery
                            && matches!(
                                item.frame.bytes.get(3),
                                Some(&DOWN_DATA) | Some(&AV_BAY_DATA)
                            )
                            && self.dependency_ready(*index)
                    })
                    .map(|(index, _)| index)
            };
            let lanes_ready = discovery.is_some() && ordinary.is_some();
            if !critical_only && !lanes_ready {
                self.discovery_balance_bytes = 0;
                if discovery.is_none() {
                    self.receipt_service_pending = false;
                }
            }
            let contested = protected.is_none() && lanes_ready;
            let i = if let Some(index) = protected {
                index
            } else if contested {
                if self.discovery_balance_bytes > 0 {
                    ordinary.unwrap()
                } else {
                    discovery.unwrap()
                }
            } else {
                // No credit earned while a lane is absent or dependency-blocked.
                self.discovery_balance_bytes = 0;
                if discovery.is_none() {
                    self.receipt_service_pending = false;
                }
                let highest = self.queue
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| self.dependency_ready(*index))
                    .max_by_key(|(index, item)| (item.priority, std::cmp::Reverse(*index)))
                    .map(|(index, _)| index)?;
                // Preserve priority between lanes (including voice/settings),
                // then use dependency-safe FIFO inside ordinary data. A fresh
                // routine heartbeat must not starve an accepted sensor report
                // merely because no independent snapshot is currently queued.
                if matches!(self.queue[highest].frame.bytes.get(3), Some(&DOWN_DATA) | Some(&AV_BAY_DATA))
                    && !self.queue[highest].independent_discovery
                {
                    ordinary.unwrap_or(highest)
                } else {
                    highest
                }
            };
            let q = &self.queue[i];
            if q.priority < minimum_priority {
                return None;
            }
            if now.saturating_sub(q.at)
                > if q.mode_reply {
                    5_000
                } else if q.frame.bytes.get(3) == Some(&VOICE) {
                    180
                } else if q.independent_discovery {
                    3_000
                } else if q.priority == 255
                    || (q.priority < 254 && q.frame.bytes.get(3) == Some(&DOWN_DATA))
                {
                    // Retain sparse reports through bounded sensor bursts. Reliable
                    // interactive controls retain their 250 ms deadline at priority 254.
                    // Stateful SDT fragments remain owned and ordered; never coalesce.
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
            let queued = self.queue.remove(i).unwrap();
            self.active_fair_delta_bytes = if contested {
                let cost =
                    (queued.frame.bytes.len() + 4 + usize::from(self.fair_overhead_bytes)) as i32;
                // Source-paced discovery bursts reserve four ordinary bytes per
                // independent snapshot byte to keep accepted reports below 1.5 s with allowance for radio transit.
                if queued.independent_discovery {
                    4 * cost
                } else {
                    -cost
                }
            } else {
                0
            };
            self.next_transmission = id;
            self.active_transmission = Some(id);
            self.active_frame_count = 1;
            self.active_survives_clear = matches!(
                queued.frame.bytes.get(3),
                Some(&POSITION) | Some(&VOICE) | Some(&SETTINGS)
            );
            return Some(Transmission { queued, follower: None, id });
        }
    }

    pub(crate) fn finish_transmission(
        &mut self,
        transmission: Transmission,
        sent: bool,
    ) -> Option<Outgoing> {
        assert!(
            transmission.follower.is_none(),
            "paired owner requires paired completion"
        );
        self.finish_bounded_transmission(transmission, sent)
            .map(|(first, _)| first)
    }

    pub(crate) fn finish_bounded_transmission(
        &mut self,
        transmission: Transmission,
        sent: bool,
    ) -> Option<(Outgoing, Option<Outgoing>)> {
        assert_eq!(self.active_transmission, Some(transmission.id));
        let count = 1 + usize::from(transmission.follower.is_some());
        assert_eq!(usize::from(self.active_frame_count), count);
        self.active_transmission = None;
        self.active_frame_count = 0;
        let fair_delta = std::mem::take(&mut self.active_fair_delta_bytes);
        let owned_bytes = transmission.queued.frame.bytes.len()
            + transmission
                .follower
                .as_ref()
                .map_or(0, |queued| queued.frame.bytes.len());
        if !sent && self.active_cancelled {
            self.active_cancelled = false;
            self.bytes -= owned_bytes;
            self.dropped += count as u64;
            return None;
        }
        self.active_cancelled = false;
        if !sent {
            if let Some(follower) = transmission.follower {
                self.queue.push_front(follower);
            }
            self.queue.push_front(transmission.queued);
            return None;
        }
        self.discovery_balance_bytes = self.discovery_balance_bytes.saturating_add(fair_delta);
        if transmission.queued.independent_discovery {
            self.receipt_service_pending = false;
        }
        self.bytes -= owned_bytes;
        self.forwarded += count as u64;
        Some((
            transmission.queued.frame,
            transmission.follower.map(|queued| queued.frame),
        ))
    }


    /// Replacing a live ordinary/metadata batch with a receipt idle gap costs
    /// ordinary service. Reserve only that charged-byte service before the next
    /// independent snapshot. A successful snapshot must occur before another
    /// allowance can accrue, so repeated receipts cannot starve metadata.
    pub(crate) fn note_receipt_packet_boundary(&mut self, charged_bytes: usize) {
        let metadata_ready = self.queue.iter().any(|item| item.independent_discovery);
        let ordinary_ready = self.queue.iter().enumerate().any(|(index, item)| {
            item.priority < 254
                && !item.independent_discovery
                && matches!(
                    item.frame.bytes.get(3),
                    Some(&DOWN_DATA) | Some(&AV_BAY_DATA)
                )
                && self.dependency_ready(index)
        });
        if !metadata_ready {
            self.receipt_service_pending = false;
            return;
        }
        if !ordinary_ready || self.receipt_service_pending || charged_bytes == 0 {
            return;
        }
        let bytes = charged_bytes.min(MAX_BYTES) as i32;
        self.discovery_balance_bytes = self.discovery_balance_bytes.saturating_add(bytes);
        self.receipt_service_pending = true;
    }

    #[cfg(test)]
    pub(crate) fn pop(&mut self, now: u64) -> Option<Outgoing> {
        let transmission = self.begin_transmission(now)?;
        self.finish_transmission(transmission, true)
    }
    /// Only the authenticated settings runtime calls this after verifying an
    /// installation signature. Raw mesh reception never establishes presence.
    pub fn authenticated_settings_peer(&mut self, now: u64) {
        self.settings_peer_seen = Some(now);
    }
    /// Display metadata only: never grants settings authentication or control.
    pub(crate) fn operating_mode_session(&self, now: u64) -> Option<u32> {
        match self.role {
            Role::Primary => Some(self.session),
            Role::Rrls => self
                .health_received
                .filter(|last| now >= *last && now - *last < LEASE_MS)
                .and(self.primary_session),
        }
    }
    pub fn settings_available(&self, now: u64) -> bool {
        let radio_seen = match self.role {
            Role::Primary => self.relay_seen,
            Role::Rrls => self.health_received,
        };
        radio_seen.is_some_and(|last| now >= last && now - last < LEASE_MS)
            || self
                .settings_peer_seen
                .is_some_and(|last| now >= last && now - last < SETTINGS_PEER_AGE_MS)
    }
    /// One pending low-priority frame; no eviction of commands, GPS, voice or
    /// telemetry to admit settings. Caller retains its frame on backpressure.
    pub fn queue_settings(&mut self, payload: &[u8], now: u64) -> bool {
        self.queue_settings_inner(payload, now, false)
    }
    /// Only the mode worker's validated query replies use this admission.
    /// It does not grant general settings presence, liveness or a command lease.
    pub(crate) fn queue_operating_mode_reply(&mut self, payload: &[u8], now: u64) -> bool {
        if !self.queue_settings_inner(payload, now, self.role == Role::Primary) {
            return false;
        }
        self.queue
            .back_mut()
            .expect("accepted settings owner")
            .mode_reply = true;
        true
    }
    fn queue_settings_inner(&mut self, payload: &[u8], now: u64, mode_reply: bool) -> bool {
        if !(mode_reply || self.settings_available(now))
            || payload.len() > paired_settings::auth::MAX_WIRE
            || self.queue.len() + usize::from(self.active_frame_count) >= MAX_FRAMES
            || self.bytes + payload.len() + 8 > MAX_BYTES
            || self
                .queue
                .iter()
                .any(|q| q.frame.bytes.get(3) == Some(&SETTINGS))
        {
            return false;
        }
        self.enqueue(
            Outgoing {
                command: true,
                bytes: encode(SETTINGS, payload),
            },
            0,
            now,
        );
        true
    }
    pub fn take_settings(&mut self) -> Option<Vec<u8>> {
        let index = self
            .settings_inbox
            .iter()
            .position(|wire| !crate::station_mode::recognizes(wire))?;
        self.settings_inbox.remove(index)
    }
    pub(crate) fn take_operating_mode(&mut self) -> Option<Vec<u8>> {
        let index = self
            .settings_inbox
            .iter()
            .position(|wire| crate::station_mode::recognizes(wire))?;
        self.settings_inbox.remove(index)
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
        self.classify_namespace(bytes, false)
    }
    fn classify_namespace(&mut self, bytes: &[u8], uplink: bool) -> Option<FrameClass> {
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
                // Reliable low-priority telemetry keeps its schema priority and
                // telemetry lifetime; use the UART batch-closing priority floor.
                } else if sedsnet::message_priority(p.envelope.ty) >= 16
                    && (sedsnet::is_reliable_type(p.envelope.ty)
                        || p.reliable.is_some_and(|header| {
                            header.flags & sedsnet::wire_format::RELIABLE_FLAG_UNSEQUENCED == 0
                        }))
                {
                    254
                } else {
                    sedsnet::transport_priority(p.envelope.ty).min(254)
                },
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
                    if let Some(i) = self
                        .templates
                        .iter()
                        .position(|entry| entry.id == template && entry.uplink == uplink)
                    {
                        self.templates.remove(i);
                    }
                    if self.templates.len() >= 256 {
                        self.templates.pop_front();
                    }
                    self.templates.push_back(TemplateClass {
                        id: template,
                        uplink,
                        class,
                    });
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
                    .find(|entry| entry.id == template && entry.uplink == uplink)
                    .map(|entry| entry.class)
            }
            3 => Some(FrameClass { priority: 253 }),
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
                (_, SETTINGS) => {
                    if data.len() <= paired_settings::auth::MAX_WIRE
                        && self.settings_inbox.len() < MAX_SETTINGS_INBOX
                    {
                        self.settings_inbox.push_back(data.to_vec());
                    } else {
                        self.dropped += 1;
                    }
                    // Never forward these application envelopes into SEDSnet,
                    // refresh liveness, or treat them as output commands.
                    None
                }
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
                    // Presence is separate from the command lease: a delayed
                    // valid reply proves reception, not current readiness.
                    self.topology_peer_seen = Some(now);
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
                (Role::Primary, DOWN_DATA | DOWN_COMMAND) => {
                    self.topology_peer_seen = Some(now);
                    self.topology_relay_rx = Some(now);
                    Some(Outgoing {
                        command: kind == DOWN_COMMAND,
                        bytes: data.to_vec(),
                    })
                }
                (Role::Rrls, AV_BAY_DATA) => {
                    // Observe the direct uplink's independent SDT namespace
                    // before fallback; do not forward or manufacture traffic.
                    self.classify_namespace(data, true);
                    Some(Outgoing {
                        command: false,
                        bytes: data.to_vec(),
                    })
                }
                (Role::Rrls, UP_DATA | UP_COMMAND) => {
                    let priority = if kind == UP_COMMAND {
                        254
                    } else {
                        self.classify_namespace(data, true)
                            .map_or(if data.starts_with(b"SDT") { 254 } else { 128 }, |class| {
                                class.priority
                            })
                    };
                    if self.active(now) {
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
                            priority,
                            now,
                        );
                    }
                    (kind == UP_DATA).then(|| Outgoing {
                        command: false,
                        bytes: data.to_vec(),
                    })
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
                // This radio's unwrapped, CRC-valid SDK traffic is the direct
                // AV-bay path. RRLS forwarding is always an RRv1 envelope and
                // took the separate branch above. Lean v4 headers carry source
                // addresses, so display names (often @addr:...) cannot establish
                // physical link liveness. Valid metadata also renews it before
                // any sensor type or source identity has been learned.
                if !command && frame_class.is_some() {
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

/// Observed physical recovery path, separate from SEDSnet's logical routes.
#[derive(Clone, Copy)]
pub(crate) struct TopologyObservation {
    pub peer_age_ms: Option<u64>,
    pub direct_age_ms: Option<u64>,
    pub relaying: bool,
    pub command_ready: bool,
}
impl Mesh {
    fn topology_observation(&self, now: u64) -> TopologyObservation {
        TopologyObservation {
            peer_age_ms: self.topology_peer_seen.map(|last| now.saturating_sub(last)),
            direct_age_ms: self.last_direct.map(|last| now.saturating_sub(last)),
            relaying: self.active(now)
                || self
                    .topology_relay_rx
                    .is_some_and(|last| now.saturating_sub(last) < LOSS_MS),
            command_ready: self.active(now),
        }
    }
}
pub(crate) fn topology_observation() -> Option<TopologyObservation> {
    let (_, shared) = RADIO.get()?;
    let mesh = shared.lock().unwrap();
    (mesh.role == Role::Primary).then(|| mesh.topology_observation(mesh.now()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_boundary_metadata_survives_downlink_wrapping_and_priority_cap() {
        crate::telemetry_schema::initialize().unwrap();
        let packet = sedsnet::packet::Packet::new(
            sedsnet::config::DataType::ReliableAck,
            &[sedsnet::config::DataEndpoint::TelemetryError],
            "RF",
            1,
            42u64.to_le_bytes().to_vec().into(),
        )
        .unwrap();
        let bytes = sedsnet::wire_format::pack_packet(&packet);
        let mut relay = Mesh::new(Role::Rrls, 2);
        relay.incoming(true, &health(1, 0, false), 0);
        while relay.pop(0).is_some() {}
        relay.incoming(false, &bytes, 1);
        assert_eq!(relay.queue.front().unwrap().priority, 254);
        let tx = relay.begin_transmission(1).unwrap();
        assert!(tx.closes_packet_batch());
        assert!(tx.is_receipt());
        assert_eq!(
            decode(&tx.frame().bytes).unwrap(),
            (DOWN_DATA, bytes.as_ref())
        );
    }
    #[test]
    fn delayed_ready_is_presence_without_extending_command_lease() {
        let mut mesh = Mesh::new(Role::Primary, 42);
        mesh.sequence = 20;
        mesh.fallback = true;
        let mut payload = 42u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&1u32.to_le_bytes());
        assert!(
            mesh.incoming(true, &encode(READY, &payload), 10_000)
                .is_none()
        );
        let observed = mesh.topology_observation(10_000);
        assert_eq!(observed.peer_age_ms, Some(0));
        assert!(!observed.command_ready);
        assert!(!observed.relaying);
        mesh.incoming(true, &encode(DOWN_DATA, &[1, 2, 3]), 10_001);
        assert!(mesh.topology_observation(10_002).relaying);
        assert!(!mesh.topology_observation(10_002).command_ready);
        assert!(!mesh.topology_observation(13_001).relaying);
    }

    #[test]
    fn raw_uart_service_launch_defaults_to_primary_and_preserves_explicit_roles() {
        for interface in ["serial", "custom_serial", "raspberry_pi_gpio_uart"] {
            let link = serde_json::from_value(serde_json::json!({
                "interface": interface, "port": "/dev/test-radio", "protocol": "raw_uart"
            }))
            .unwrap();
            assert_eq!(role_for_link(&link, None).unwrap(), Some(Role::Primary));
            assert_eq!(role_for_link(&link, Some("off")).unwrap(), None);
            assert_eq!(
                role_for_link(&link, Some("rrls")).unwrap(),
                Some(Role::Rrls)
            );
            assert_eq!(
                role_for_link(&link, Some("primary")).unwrap(),
                Some(Role::Primary)
            );
            assert!(role_for_link(&link, Some("invalid")).is_err());
        }
        for config in [
            serde_json::json!({"interface":"serial","port":"/dev/test-radio","protocol":"packet_framed"}),
            serde_json::json!({"interface":"i2c"}),
        ] {
            let link = serde_json::from_value(config).unwrap();
            assert_eq!(role_for_link(&link, None).unwrap(), None);
        }
    }

    #[test]
    #[cfg(feature = "hitl_mode")]
    fn recovery_liveness_uses_the_shared_virtual_clock_when_configured() {
        const CHILD: &str = "GS_RRLS_CLOCK_REGRESSION_CHILD";
        const NAME: &str =
            "rrls::tests::recovery_liveness_uses_the_shared_virtual_clock_when_configured";
        if std::env::var_os(CHILD).is_some() {
            let path = std::path::PathBuf::from(std::env::var_os("GS_SIM_CLOCK_FILE").unwrap());
            let mesh = Mesh::new(Role::Rrls, 1);
            assert_eq!(
                mesh.now(),
                1000,
                "RRLS is using wall time instead of virtual time"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert_eq!(
                mesh.now(),
                1000,
                "a paused MCU must not expire RRLS liveness"
            );
            std::fs::write(&path, "2000").unwrap();
            assert_eq!(mesh.now(), 2000);
            std::fs::write(&path, "").unwrap();
            assert_eq!(
                mesh.now(),
                2000,
                "truncate/write races must retain the last tick"
            );
            std::fs::write(&path, "5").unwrap();
            assert_eq!(
                mesh.now(),
                2000,
                "clock reversal cannot renew expired leases"
            );
            println!("RRLS_SHARED_CLOCK_PASS");
            return;
        }
        // Isolate main's OnceLock clock configuration from concurrent tests.
        struct ClockDirectory(std::path::PathBuf);
        impl Drop for ClockDirectory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = ClockDirectory(
            std::env::temp_dir().join(format!("rrls-clock-{}-{stamp}", std::process::id())),
        );
        std::fs::create_dir(&directory.0).unwrap();
        let path = directory.0.join("clock.txt");
        std::fs::write(&path, "1000").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("GS_SIM_CLOCK_FILE", path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if started.elapsed() > std::time::Duration::from_secs(10) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("isolated RRLS clock check timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("RRLS_SHARED_CLOCK_PASS"),
            "RRLS clock regression: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn background_mode_reservation_is_bounded_mode_only_and_keeps_critical_precedence() {
        let mut mesh = Mesh::new(Role::Primary, 7);
        mesh.authenticated_settings_peer(0);
        assert!(mesh.queue_settings(b"OM\x02not-worker-tagged", 0));
        assert!(
            mesh.begin_mode_reply_transmission(500).is_none(),
            "background cannot promote documents or magic alone"
        );
        // The existing250ms document expiry remains unchanged.
        assert!(mesh.begin_transmission(500).is_none());
        assert!(mesh.queue_operating_mode_reply(b"OM\x02reply", 500));
        mesh.tick(500);
        assert!(
            mesh.begin_mode_reply_transmission(800).is_none(),
            "pending HEALTH is protected"
        );
        let health = mesh.begin_transmission_at_priority(800, 255).unwrap();
        mesh.finish_transmission(health, true);
        let mode = mesh
            .begin_mode_reply_transmission(800)
            .expect("mode remains owned after old250ms limit");
        assert!(
            decode(&mode.frame().bytes)
                .unwrap()
                .1
                .starts_with(b"OM\x02")
        );
        mesh.finish_transmission(mode, false);
        assert!(
            mesh.begin_mode_reply_transmission(5501).is_none(),
            "bounded5s expiry matches retry cadence"
        );
        assert_eq!(mesh.bytes, 0);
        assert!(mesh.queue_operating_mode_reply(b"OM\x02reply", 6000));
        assert!(
            mesh.begin_mode_reply_transmission(5999).is_none(),
            "clock reset cannot preserve future mode ownership indefinitely"
        );
        assert_eq!(mesh.bytes, 0);
    }
    #[test]
    fn display_mode_session_requires_fresh_accepted_health_not_settings_presence() {
        let mut rr = Mesh::new(Role::Rrls, 99);
        rr.authenticated_settings_peer(0);
        assert!(rr.settings_available(0));
        assert_eq!(rr.operating_mode_session(0), None);
        let health = |session: u32, seq: u32| {
            let mut body = session.to_le_bytes().to_vec();
            body.extend(seq.to_le_bytes());
            body.push(1);
            encode(HEALTH, &body)
        };
        rr.incoming(true, &health(7, 1), 100);
        assert_eq!(rr.operating_mode_session(100), Some(7));
        rr.incoming(true, &health(8, 1), 101);
        assert_eq!(
            rr.operating_mode_session(101),
            Some(7),
            "another session cannot replace a live primary"
        );
        rr.incoming(true, &health(7, 1), 1999);
        assert_eq!(
            rr.operating_mode_session(2100),
            None,
            "duplicate HEALTH cannot renew lease"
        );
        rr.incoming(true, &health(8, 1), 2100);
        assert_eq!(rr.operating_mode_session(2100), Some(8));
        assert_eq!(
            rr.operating_mode_session(0),
            None,
            "clock regression is not fresh"
        );
        assert_eq!(
            Mesh::new(Role::Primary, 11).operating_mode_session(0),
            Some(11)
        );
    }
    #[test]
    fn operating_mode_demux_preserves_shared_bounded_settings_inbox() {
        let mut rr = Mesh::new(Role::Rrls, 99);
        let document = vec![1, 2, 3];
        let mode = b"OM\x02display".to_vec();
        rr.incoming(true, &encode(SETTINGS, &mode), 0);
        rr.incoming(true, &encode(SETTINGS, &document), 0);
        assert_eq!(rr.take_settings(), Some(document));
        assert_eq!(rr.take_settings(), None);
        assert_eq!(rr.take_operating_mode(), Some(mode));
        assert_eq!(rr.take_operating_mode(), None);
    }
    #[test]
    fn settings_are_bounded_and_never_displace_other_radio_work() {
        let mut mesh = Mesh::new(Role::Primary, 7);
        assert!(!mesh.queue_settings(&[1], 0));
        mesh.authenticated_settings_peer(0);
        assert!(mesh.queue_settings(&[1, 2, 3], 0));
        assert!(!mesh.queue_settings(&[4], 0));
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: encode(POSITION, &[0; 39]),
            },
            255,
            0,
        );
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: encode(UP_COMMAND, &[7]),
            },
            254,
            0,
        );
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: encode(VOICE, &[8]),
            },
            1,
            0,
        );
        for expected in [POSITION, UP_COMMAND, VOICE, SETTINGS] {
            assert_eq!(decode(&mesh.pop(1).unwrap().bytes).unwrap().0, expected);
        }
        assert!(!mesh.queue_settings(&vec![0; paired_settings::auth::MAX_WIRE + 1], 1));
        assert!(!mesh.queue_settings(&[1], SETTINGS_PEER_AGE_MS));
        assert!(!mesh.voice_available(1));
        assert!(!mesh.active(1));
    }
    #[test]
    fn unverified_settings_cannot_refresh_presence_forward_or_grow_an_unbounded_inbox() {
        let mut mesh = Mesh::new(Role::Primary, 7);
        for byte in 0..12 {
            assert!(mesh.incoming(true, &encode(SETTINGS, &[byte]), 0).is_none());
        }
        assert!(!mesh.settings_available(0));
        assert!(!mesh.active(0));
        assert!(mesh.pop(0).is_none());
        let frames: Vec<_> = std::iter::from_fn(|| mesh.take_settings()).collect();
        assert_eq!(frames, vec![vec![0], vec![1], vec![2], vec![3]]);
        assert_eq!(mesh.dropped, 8);
        let before = mesh.dropped;
        assert!(
            mesh.incoming(
                true,
                &encode(SETTINGS, &vec![0; paired_settings::auth::MAX_WIRE + 1]),
                0
            )
            .is_none()
        );
        assert!(mesh.take_settings().is_none());
        assert_eq!(mesh.dropped, before + 1);
    }
    #[test]
    fn settings_expire_without_enabling_relay_or_local_recovery_commands() {
        let mut mesh = Mesh::new(Role::Rrls, 7);
        mesh.health_received = Some(0);
        assert!(mesh.queue_settings(&[9], 0));
        assert!(mesh.pop(251).is_none());
        assert!(mesh.local_tx(true, &[1], 251).is_none());
        assert!(!mesh.active(251));
    }

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
    fn lean_v4_direct_frames_prevent_false_relay_fallback_and_real_loss_recovers() {
        const FIXTURES: &str = "GS_RRLS_LEAN_FIXTURES";
        if let Ok(encoded) = std::env::var(FIXTURES) {
            let fixtures: Vec<Vec<u8>> = serde_json::from_str(&encoded).unwrap();
            assert_eq!(fixtures.len(), 4);
            crate::telemetry_schema::initialize().unwrap();
            let mut primary = Mesh::new(Role::Primary, 1);
            // This fresh process did not serialize the sources, so the actual
            // SDK exposes their lean numeric identities as an @addr alias.
            for (index, wire) in fixtures.iter().enumerate() {
                let info = sedsnet::wire_format::peek_frame_info(wire).unwrap();
                assert!(info.envelope.sender.starts_with("@addr:"));
                assert_ne!(info.envelope.source_address, 0);
                let at = index as u64 * 1000;
                primary.incoming(false, wire, at).unwrap();
                primary.relay_seen = Some(at);
                primary.tick(at);
                assert!(!primary.active(at));
                assert_eq!(primary.last_direct, Some(at));
            }
            let last = 3000;
            let mut broken = fixtures[0].clone();
            *broken.last_mut().unwrap() ^= 1;
            primary.incoming(false, &broken, last + 100);
            assert_eq!(
                primary.last_direct,
                Some(last),
                "invalid CRC cannot renew direct liveness"
            );
            primary.incoming(true, &encode(DOWN_DATA, &fixtures[0]), last + 200);
            assert_eq!(
                primary.last_direct,
                Some(last),
                "relay data cannot renew direct liveness"
            );
            let lost_at = last + LOSS_MS;
            primary.relay_seen = Some(lost_at);
            primary.tick(lost_at);
            assert!(
                primary.active(lost_at),
                "real direct loss must enable a live relay"
            );
            assert_eq!(
                decode(&primary.local_tx(false, b"pending", lost_at).unwrap().bytes)
                    .unwrap()
                    .0,
                UP_DATA
            );
            for delta in [100, 600, 1100, 1600, 2100] {
                primary.incoming(false, &fixtures[0], lost_at + delta);
                primary.relay_seen = Some(lost_at + delta);
                primary.tick(lost_at + delta);
            }
            assert!(
                !primary.active(lost_at + 2100),
                "stable direct recovery must exit fallback"
            );
            assert_eq!(
                decode(
                    &primary
                        .local_tx(false, b"pending", lost_at + 2100)
                        .unwrap()
                        .bytes
                )
                .unwrap()
                .0,
                AV_BAY_DATA
            );
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let fixtures: Vec<Vec<u8>> = ["RF", "FC", "PB", "NEW_SENSOR_ROUTER"]
            .iter()
            .enumerate()
            .map(|(index, sender)| {
                let ty = if index < 3 {
                    crate::telemetry_schema::data_type("GPS_DATA")
                } else {
                    sedsnet::config::DataType::identity("FUTURE_LINK_METADATA")
                };
                let packet = sedsnet::packet::Packet::new(
                    ty,
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                    sender,
                    1,
                    vec![0; 12].into(),
                )
                .unwrap();
                sedsnet::wire_format::pack_packet(&packet).to_vec()
            })
            .collect();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "rrls::tests::lean_v4_direct_frames_prevent_false_relay_fallback_and_real_loss_recovers", "--nocapture"])
            .env(FIXTURES, serde_json::to_string(&fixtures).unwrap())
            .output().unwrap();
        assert!(
            output.status.success(),
            "fresh-process actual lean codec regression failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
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
        let observed = r.incoming(f.command, &f.bytes, 0).unwrap();
        assert!(!observed.command);
        assert_eq!(observed.bytes, b"ground-side compact template");
        assert!(r.pop(0).is_none());
    }
    #[test]
    fn ground_data_is_observed_without_local_command_delivery_or_unleased_relay() {
        let mut relay = Mesh::new(Role::Rrls, 2);
        let data = encode(UP_DATA, b"opaque ground SDK data");
        let observed = relay.incoming(true, &data, 0).unwrap();
        assert!(!observed.command);
        assert_eq!(observed.bytes, b"opaque ground SDK data");
        assert!(
            relay.queue.is_empty(),
            "unleased observation never forwards"
        );
        assert!(
            relay
                .incoming(true, &encode(UP_COMMAND, b"command"), 0)
                .is_none()
        );
        assert!(relay.queue.is_empty());
        relay.incoming(true, &health(1, 0, false), 1);
        let observed = relay.incoming(true, &data, 1).unwrap();
        assert!(!observed.command);
        assert_eq!(
            decode(&relay.queue.back().unwrap().frame.bytes).unwrap().0,
            AV_BAY_DATA
        );
        assert!(
            relay
                .incoming(true, &encode(UP_COMMAND, b"command"), 1)
                .is_none()
        );
        assert_eq!(
            decode(&relay.queue.back().unwrap().frame.bytes).unwrap().0,
            AV_BAY_COMMAND
        );
    }
    #[test]
    fn shared_template_ids_are_classified_in_independent_radio_namespaces() {
        crate::telemetry_schema::initialize().unwrap();
        let canonical = |name: &str| {
            let packet = sedsnet::packet::Packet::new(
                crate::telemetry_schema::data_type(name),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "GS",
                1,
                vec![0; if name == "HEARTBEAT" { 0 } else { 12 }].into(),
            )
            .unwrap();
            sedsnet::wire_format::pack_packet_with_reliable(
                &packet,
                sedsnet::wire_format::ReliableHeader {
                    flags: sedsnet::wire_format::RELIABLE_FLAG_UNSEQUENCED,
                    seq: 0,
                    ack: 0,
                },
            )
        };
        let wrap = |kind: u8, body: &[u8]| {
            let mut out = b"SDT".to_vec();
            out.push(kind);
            out.extend_from_slice(body);
            out.extend_from_slice(&crc(&out).to_le_bytes());
            out
        };
        let mut gps = vec![7];
        gps.extend_from_slice(&canonical("GPS_DATA"));
        let mut heartbeat = vec![7];
        heartbeat.extend_from_slice(&canonical("HEARTBEAT"));
        let compact = wrap(4, &[0, 7, 12, 1, 0, 0]);
        let mut relay = Mesh::new(Role::Rrls, 2);
        assert_eq!(relay.classify(&wrap(1, &gps)).unwrap().priority, 255);
        relay.incoming(true, &encode(AV_BAY_DATA, &wrap(1, &heartbeat)), 0);
        assert_eq!(relay.classify(&compact).unwrap().priority, 255);
        assert_eq!(
            relay.classify_namespace(&compact, true).unwrap().priority,
            253
        );
        assert_eq!(
            std::mem::size_of::<TemplateClass>(),
            std::mem::size_of::<(u32, FrameClass)>(),
            "namespace tag fits existing cache entry padding"
        );
        assert_eq!(relay.templates.len(), 2);
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
    fn protected_mesh_service_leaves_lower_priority_bytes_owned_and_queued() {
        let mut mesh = Mesh::new(Role::Primary, 1);
        mesh.tick(0);
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: encode(SETTINGS, b"settings"),
            },
            20,
            0,
        );
        let health = mesh.begin_transmission_at_priority(0, 255).unwrap();
        assert_eq!(decode(&health.frame().bytes).unwrap().0, HEALTH);
        mesh.finish_transmission(health, true);
        let owned = mesh.bytes;
        assert!(mesh.begin_transmission_at_priority(1, 255).is_none());
        assert_eq!(mesh.bytes, owned);
        assert_eq!(mesh.queue.len(), 1);
        let settings = mesh.begin_transmission(1).unwrap();
        assert_eq!(decode(&settings.frame().bytes).unwrap().0, SETTINGS);
        mesh.finish_transmission(settings, true);
        assert_eq!(mesh.bytes, 0);
    }

    #[test]
    fn serial_failure_keeps_the_frame_and_reservation_until_success() {
        let mut mesh = Mesh::new(Role::Rrls, 2);
        for byte in 0..4 {
            mesh.enqueue(
                Outgoing {
                    command: false,
                    bytes: vec![byte],
                },
                254,
                0,
            );
        }
        let transmission = mesh.begin_transmission(0).unwrap();
        assert_eq!(transmission.frame().bytes, [0]);
        let payload_pointer = transmission.frame().bytes.as_ptr();
        assert_eq!(mesh.bytes, 4);
        assert_eq!(mesh.forwarded, 0);
        assert!(mesh.begin_transmission(0).is_none());
        assert!(mesh.finish_transmission(transmission, false).is_none());
        assert_eq!(mesh.bytes, 4);
        for byte in 0..4 {
            let frame = mesh.pop(0).unwrap();
            assert_eq!(frame.bytes, [byte]);
            if byte == 0 {
                assert_eq!(
                    frame.bytes.as_ptr(),
                    payload_pointer,
                    "retry copied the payload"
                );
            }
        }
        assert_eq!(mesh.forwarded, 4);
        assert_eq!(mesh.bytes, 0);
    }

    #[test]
    fn active_serial_frame_stays_inside_queue_limits_and_survives_clear() {
        let mut mesh = Mesh::new(Role::Primary, 1);
        for byte in 0..MAX_FRAMES {
            mesh.enqueue(
                Outgoing {
                    command: false,
                    bytes: vec![byte as u8],
                },
                254,
                0,
            );
        }
        let transmission = mesh.begin_transmission(0).unwrap();
        for _ in 0..MAX_FRAMES {
            mesh.enqueue(
                Outgoing {
                    command: false,
                    bytes: vec![99],
                },
                254,
                0,
            );
            assert_eq!(mesh.queue.len() + 1, MAX_FRAMES);
            assert_eq!(mesh.bytes, MAX_FRAMES);
        }
        mesh.clear();
        assert_eq!(
            mesh.bytes, 1,
            "clear cannot release an active write's bytes"
        );
        assert!(mesh.queue.is_empty());
        mesh.finish_transmission(transmission, true);
        assert_eq!(mesh.bytes, 0);
        assert_eq!(mesh.forwarded, 1);
    }

    #[test]
    fn active_settings_keep_clear_semantics_and_prevent_false_queue_admission() {
        let mut mesh = Mesh::new(Role::Primary, 1);
        mesh.authenticated_settings_peer(0);
        for byte in 0..MAX_FRAMES {
            mesh.enqueue(
                Outgoing {
                    command: false,
                    bytes: vec![byte as u8],
                },
                254,
                0,
            );
        }
        let transmission = mesh.begin_transmission(0).unwrap();
        let drops = mesh.dropped;
        assert!(!mesh.queue_settings(b"not accepted", 0));
        assert_eq!(
            mesh.dropped, drops,
            "settings cannot evict already admitted traffic"
        );
        mesh.finish_transmission(transmission, true);
        mesh.clear();
        assert!(mesh.queue_settings(b"accepted", 0));
        let transmission = mesh.begin_transmission(0).unwrap();
        let expected = transmission.frame().bytes.clone();
        mesh.clear();
        mesh.finish_transmission(transmission, false);
        assert_eq!(mesh.pop(0).unwrap().bytes, expected);
        assert_eq!(mesh.bytes, 0);
    }

    #[test]
    fn topology_clear_during_failed_write_does_not_restore_old_route_frames() {
        let mut mesh = Mesh::new(Role::Primary, 1);
        mesh.enqueue(
            Outgoing {
                command: false,
                bytes: vec![1],
            },
            254,
            0,
        );
        let transmission = mesh.begin_transmission(0).unwrap();
        mesh.clear();
        mesh.finish_transmission(transmission, false);
        assert_eq!(mesh.bytes, 0);
        assert_eq!(mesh.dropped, 1);
        assert_eq!(mesh.forwarded, 0);
        assert!(mesh.pop(0).is_none());
    }

    #[test]
    fn failed_serial_write_does_not_renew_deadline_or_block_new_priority() {
        let mut mesh = Mesh::new(Role::Primary, 1);
        mesh.enqueue(
            Outgoing {
                command: false,
                bytes: vec![1],
            },
            2,
            0,
        );
        let transmission = mesh.begin_transmission(0).unwrap();
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: vec![2],
            },
            254,
            100,
        );
        mesh.finish_transmission(transmission, false);
        assert_eq!(mesh.pop(100).unwrap().bytes, [2]);
        assert!(
            mesh.pop(251).is_none(),
            "failed bulk retains its original age"
        );
        assert_eq!(mesh.dropped, 1);
        assert_eq!(mesh.bytes, 0);
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
    fn reliable_telemetry_headers_preserve_schema_priority() {
        crate::telemetry_schema::initialize().unwrap();
        for name in ["BATTERY_VOLTAGE", "BATTERY_CURRENT", "FLIGHT_COMMAND"] {
            let ty = crate::telemetry_schema::data_type(name);
            assert!(sedsnet::is_reliable_type(ty));
            let payload = if name == "FLIGHT_COMMAND" {
                vec![1]
            } else {
                vec![0; 4]
            };
            let packet = sedsnet::packet::Packet::new(
                ty,
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "RF",
                0,
                payload.into(),
            )
            .unwrap();
            for flags in [0, sedsnet::wire_format::RELIABLE_FLAG_UNSEQUENCED] {
                let wire = sedsnet::wire_format::pack_packet_with_reliable(
                    &packet,
                    sedsnet::wire_format::ReliableHeader {
                        flags,
                        seq: 7,
                        ack: 0,
                    },
                );
                let mut relay = Mesh::new(Role::Rrls, 2);
                let expected = if name == "FLIGHT_COMMAND" {
                    254
                } else {
                    sedsnet::transport_priority(ty)
                };
                assert_eq!(
                    relay.classify(&wire).unwrap().priority,
                    expected,
                    "{name} flags{flags}"
                );
            }
        }
    }

    #[test]
    fn telemetry_retention_does_not_extend_reliable_control_deadlines() {
        crate::telemetry_schema::initialize().unwrap();
        for (name, size, age, survives) in [
            ("FLIGHT_COMMAND", 1, 251, false),
            ("BATTERY_VOLTAGE", 4, 1_499, true),
            ("BATTERY_VOLTAGE", 4, 1_501, false),
        ] {
            let packet = sedsnet::packet::Packet::new(
                crate::telemetry_schema::data_type(name),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "RF",
                0,
                vec![0; size].into(),
            )
            .unwrap();
            let wire = sedsnet::wire_format::pack_packet(&packet);
            let mut relay = Mesh::new(Role::Rrls, 2);
            relay.incoming(true, &health(1, 0, false), 0);
            relay.incoming(false, &wire, 0);
            assert_eq!(relay.pop(age).is_some(), survives, "{name} age{age}");
            assert_eq!(relay.dropped, u64::from(!survives));
            assert!(relay.queue.is_empty() && relay.bytes == 0);
        }
    }

    fn independent_address(now: u64) -> Vec<u8> {
        let ad = sedsnet::discovery::AddressAdvertisement {
            hostname: "DISCOVERY_BURST_ROUTER".into(),
            address: 101,
            requested_address: 101,
            mode: 2,
            state: 1,
            birth_ms: 0,
            owner_hash: 123,
            reachable_endpoints: vec![
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
                crate::telemetry_schema::endpoint("SD_CARD"),
            ],
            reachable_network_variables: vec![
                crate::telemetry_schema::data_type("FLIGHT_STATE"),
                crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
            ],
            reachable_timesync_sources: vec!["RF".into()],
            link_capabilities: sedsnet::discovery::LinkCapabilities {
                version: 1,
                flags: 3,
                profile: 0,
                max_frame_bytes: 192,
                compact_header_target_bytes: 12,
                max_side_transport_templates: 256,
            },
        };
        let packet = sedsnet::discovery::build_discovery_address("RF", now, &ad).unwrap();
        let wire = sedsnet::wire_format::pack_packet(&packet);
        wire.to_vec()
    }

    #[test]
    fn independent_metadata_fairness_protects_controls_and_commits_only_successful_bytes() {
        crate::telemetry_schema::initialize().unwrap();
        let wire = |name: &str, size| {
            sedsnet::wire_format::pack_packet(
                &sedsnet::packet::Packet::new(
                    crate::telemetry_schema::data_type(name),
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                    "RF",
                    1,
                    vec![0; size].into(),
                )
                .unwrap(),
            )
        };
        let mut mesh = Mesh::new(Role::Rrls, 2);
        mesh.incoming(true, &health(1, 0, false), 0);
        mesh.incoming(false, &independent_address(1), 0);
        mesh.incoming(true, &encode(UP_DATA, &independent_address(2)), 0);
        let battery = wire("BATTERY_VOLTAGE", 4);
        mesh.incoming(false, &battery, 0);
        mesh.incoming(false, &wire("FLIGHT_COMMAND", 1), 0);
        mesh.incoming(false, &wire("GPS_DATA", 12), 0);
        for expected in ["GPS_DATA", "FLIGHT_COMMAND"] {
            let frame = mesh.pop(0).unwrap();
            let (_, body) = decode(&frame.bytes).unwrap();
            assert_eq!(
                sedsnet::wire_format::peek_frame_info(body)
                    .unwrap()
                    .envelope
                    .ty,
                crate::telemetry_schema::data_type(expected)
            );
        }
        let transmission = mesh.begin_transmission(0).unwrap();
        assert!(transmission.queued.independent_discovery);
        let cost =
            transmission.frame().bytes.len() as i32 + 4 + i32::from(mesh.fair_overhead_bytes);
        mesh.finish_transmission(transmission, false);
        assert_eq!(
            mesh.discovery_balance_bytes, 0,
            "failed IO earns no fairness debit"
        );
        let transmission = mesh.begin_transmission(0).unwrap();
        mesh.finish_transmission(transmission, true);
        assert_eq!(mesh.discovery_balance_bytes, 4 * cost);
        let frame = mesh.pop(0).unwrap();
        assert_eq!(decode(&frame.bytes).unwrap().1, battery.as_ref());
        let transmission = mesh.begin_transmission(0).unwrap();
        assert_eq!(
            mesh.discovery_balance_bytes, 0,
            "no credit survives a missing ordinary lane"
        );
        mesh.finish_transmission(transmission, false);
        assert_eq!(
            std::mem::size_of::<Queued>(),
            std::mem::size_of::<(Outgoing, u64, u8)>(),
            "queue metadata fits existing item padding"
        );
    }

    #[test]
    fn receipt_service_is_latched_until_metadata_succeeds_without_idle_credit() {
        crate::telemetry_schema::initialize().unwrap();
        let mut mesh = Mesh::new(Role::Rrls, 2);
        mesh.incoming(true, &health(1, 0, false), 0);
        while mesh.pop(0).is_some() {}
        mesh.incoming(false, &independent_address(0), 0);
        mesh.note_receipt_packet_boundary(154);
        assert_eq!(mesh.discovery_balance_bytes, 0, "no idle ordinary credit");
        let battery = sedsnet::wire_format::pack_packet(
            &sedsnet::packet::Packet::new(
                crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "PB",
                1,
                vec![0; 4].into(),
            )
            .unwrap(),
        );
        mesh.incoming(false, &battery, 0);
        mesh.note_receipt_packet_boundary(154);
        assert_eq!(mesh.discovery_balance_bytes, 154);
        assert!(mesh.receipt_service_pending);
        let tx = mesh.begin_transmission(0).unwrap();
        assert!(!tx.queued.independent_discovery);
        mesh.finish_transmission(tx, false);
        mesh.note_receipt_packet_boundary(154);
        assert_eq!(
            mesh.discovery_balance_bytes, 154,
            "failed IO cannot rearm compensation"
        );
        mesh.pop(0).unwrap();
        let tx = mesh.begin_transmission(0).unwrap();
        assert!(tx.queued.independent_discovery);
        assert!(
            mesh.receipt_service_pending,
            "an empty ordinary lane cannot rearm before metadata"
        );
        mesh.finish_transmission(tx, false);
        mesh.incoming(false, &battery, 0);
        mesh.note_receipt_packet_boundary(154);
        assert_eq!(
            mesh.discovery_balance_bytes, 0,
            "lane flapping cannot renew the allowance"
        );
        let tx = mesh.begin_transmission(0).unwrap();
        assert!(tx.queued.independent_discovery);
        mesh.finish_transmission(tx, true);
        assert!(!mesh.receipt_service_pending);
    }

    #[test]
    fn repeated_new_receipts_preserve_bounded_metadata_service_under_refusal() {
        crate::telemetry_schema::initialize().unwrap();
        let wall = Instant::now();
        let mut cap = crate::comms::rfd_capacity::Capacity::new(64_000, 35, 16);
        let mut wire = receipt_test_wire::WireDrain::default();
        let mut mesh = Mesh::new(Role::Rrls, 2);
        mesh.incoming(true, &health(1, 0, false), 0);
        let mut metadata = 0;
        let mut ordinary = 0;
        let mut refused_once = false;
        for now in 0..6000 {
            if now % 500 == 0 {
                mesh.incoming(true, &health(1, (now / 500) as u32, false), now);
            }
            if now < 4000 && now % 1000 == 0 {
                mesh.incoming(false, &independent_address(now), now);
            }
            if now < 4000 && now % 100 == 0 {
                let packet = sedsnet::packet::Packet::new(
                    crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                    "PB",
                    now,
                    vec![0; 4].into(),
                )
                .unwrap();
                mesh.incoming(false, &sedsnet::wire_format::pack_packet(&packet), now);
            }
            if now < 4000 && now % 155 == 0 {
                let packet = sedsnet::packet::Packet::new(
                    sedsnet::config::DataType::ReliableAck,
                    &[sedsnet::config::DataEndpoint::TelemetryError],
                    "RF",
                    now,
                    now.to_le_bytes().to_vec().into(),
                )
                .unwrap();
                mesh.incoming(false, &sedsnet::wire_format::pack_packet(&packet), now);
            }
            if !wire.ready(now) {
                continue;
            }
            let tx = if cap.ready(wall, Some(now)) {
                mesh.begin_transmission(now)
            } else {
                mesh.begin_critical_transmission(now, 0)
            };
            let Some(tx) = tx else {
                continue;
            };
            if !cap.ready_for(wall, Some(now), tx.closes_packet_batch()) {
                mesh.finish_transmission(tx, false);
                continue;
            }
            if tx.queued.independent_discovery && !refused_once {
                mesh.finish_transmission(tx, false);
                refused_once = true;
                continue;
            }
            let age = now.saturating_sub(tx.queued.at);
            if tx.queued.independent_discovery {
                metadata += 1;
                assert!(age <= 3000, "metadata service was starved: {age}");
            } else if tx.queued.priority < 254 {
                ordinary += 1;
                assert!(age <= 1500, "ordinary service was starved: {age}");
            }
            let bytes = tx.frame().bytes.len() + 4;
            wire.committed(now, bytes, 115_200);
            cap.committed(wall, Some(now), bytes);
            cap.drained_virtual(wire.stop_us());
            if tx.closes_packet_batch() {
                let contract = tx.boundary_contract();
                if cap.close_packet_batch_for_contract(wall, Some(now), contract)
                    && contract.is_some_and(|(_, receipt)| receipt)
                {
                    mesh.note_receipt_packet_boundary(cap.packet_idle_service_bytes());
                }
            }
            mesh.finish_transmission(tx, true);
            assert!(mesh.queue.len() <= MAX_FRAMES && mesh.bytes <= MAX_BYTES);
        }
        assert!(refused_once);
        assert_eq!(metadata, 4);
        assert_eq!(ordinary, 40);
        assert_eq!(mesh.dropped, 0);
    }

    #[test]
    fn fair_metadata_cannot_overtake_real_sdt_chunk_bootstrap_and_compact_followers() {
        use sedsnet::router::{EndpointHandler, Router, RouterConfig};
        crate::telemetry_schema::initialize().unwrap();
        let endpoint = crate::telemetry_schema::endpoint("GROUND_STATION");
        let sensor = crate::telemetry_schema::data_type("IMU_DATA");
        let captured = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let output = captured.clone();
        let source =
            Router::new_with_clock(RouterConfig::new([]).with_sender("FC"), Box::new(|| 0));
        source.add_side_packed_small_packets(
            "radio",
            move |bytes| {
                if bytes.starts_with(b"SDT") {
                    output.lock().unwrap().push(bytes.to_vec());
                }
                Ok(())
            },
            48,
        );
        for timestamp in [1, 2] {
            source
                .tx(sedsnet::packet::Packet::new(
                    sensor,
                    &[endpoint],
                    "FC",
                    timestamp,
                    vec![0; 24].into(),
                )
                .unwrap()
                .with_nonce(timestamp as u16))
                .unwrap();
        }
        let frames = captured.lock().unwrap().clone();
        assert!(
            frames.iter().any(|frame| frame[3] == 3),
            "actual SDK chunks the bootstrap"
        );
        assert!(
            frames.iter().any(|frame| matches!(frame[3], 2 | 4 | 5)),
            "actual SDK compact follower"
        );
        let timestamps = Arc::new(Mutex::new(Vec::new()));
        let seen = timestamps.clone();
        let receiver = Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                endpoint,
                move |packet| {
                    if packet.data_type() == sensor {
                        seen.lock().unwrap().push(packet.timestamp());
                    }
                    Ok(())
                },
            )]),
            Box::new(|| 0),
        );
        let side = receiver.add_side_packed_small_packets("radio", |_| Ok(()), 48);
        let mut mesh = Mesh::new(Role::Rrls, 2);
        mesh.incoming(true, &health(1, 0, false), 0);
        mesh.incoming(false, &independent_address(0), 0);
        for frame in &frames {
            mesh.incoming(false, frame, 0);
        }
        let mut forwarded = Vec::new();
        loop {
            mesh.note_receipt_packet_boundary(154);
            let Some(frame) = mesh.pop(0) else { break };
            let (_, body) = decode(&frame.bytes).unwrap();
            if body.starts_with(b"SDT") {
                forwarded.push(body.to_vec());
                receiver.rx_packed_from_side(body, side).unwrap();
            }
        }
        assert_eq!(
            forwarded, frames,
            "never reorder an opaque dependency stream"
        );
        assert_eq!(*timestamps.lock().unwrap(), vec![1, 2]);
    }

    fn source_paced_campaign(with_discovery: bool) {
        crate::telemetry_schema::initialize().unwrap();
        fn wire(name: &str, sender: &'static str, timestamp: u64, bytes: usize) -> Vec<u8> {
            let packet = sedsnet::packet::Packet::new(
                crate::telemetry_schema::data_type(name),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                sender,
                timestamp,
                vec![0; bytes].into(),
            )
            .unwrap();
            sedsnet::wire_format::pack_packet(&packet).to_vec()
        }
        let mut relay = Mesh::new(Role::Rrls, 2);
        let mut batteries = Vec::new();
        let mut commands = Vec::new();
        let mut gps = Vec::new();
        let mut ordinary_lateness = Vec::new();
        let mut peak = 0;
        let mut next_egress_ms = 0;
        let mut discovery_delivered = 0;
        for now in (0..=22_000u64).step_by(10) {
            if now % 500 == 0 {
                relay.incoming(true, &health(1, (now / 500) as u32, false), now);
            }
            relay.tick(now);
            if now < 20_000 && now % 5_000 == 0 {
                // The observed24-frame relay burst fits the existing64-frame cap.
                for index in 0..24 {
                    let (name, bytes) = if index % 2 == 0 {
                        ("IMU_DATA", 24)
                    } else {
                        ("BAROMETER_DATA", 12)
                    };
                    relay.incoming(false, &wire(name, "FC", now + index, bytes), now);
                }
                relay.incoming(false, &wire("BATTERY_VOLTAGE", "PB", now, 4), now);
            }
            if now < 20_000 && now % 200 == 100 {
                relay.incoming(false, &wire("IMU_DATA", "FC", now, 24), now);
                relay.incoming(false, &wire("BAROMETER_DATA", "FC", now, 12), now);
            }
            if now < 20_000 && now % 5_000 == 50 {
                let command = wire("FLIGHT_COMMAND", "FC", now, 1);
                assert!(
                    sedsnet::wire_format::peek_frame_info(&command)
                        .unwrap()
                        .reliable
                        .is_some()
                );
                relay.incoming(false, &command, now);
                relay.incoming(false, &wire("GPS_DATA", "RF", now, 12), now);
            }
            if with_discovery && (5_000..7_040).contains(&now) && (now - 5_000) % 120 == 0 {
                let wire = independent_address(now);
                if ((now - 5_000) / 120) % 2 == 0 {
                    relay.incoming(false, &wire, now);
                } else {
                    relay.incoming(true, &encode(UP_DATA, &wire), now);
                }
            }
            peak = peak.max(relay.queue.len());
            assert!(relay.queue.len() <= MAX_FRAMES && relay.bytes <= MAX_BYTES);
            // Same16kbps share and16-byte overhead as production capacity.
            if now >= next_egress_ms
                && let Some(frame) = relay.pop(now)
            {
                let framed_cost = frame.bytes.len() as u64 + 4 + 16;
                next_egress_ms = now + (framed_cost * 8 * 1000).div_ceil(16_000);
                assert!(frame.bytes.len() + 4 <= 220);
                let (kind, body) = decode(&frame.bytes).unwrap();
                if kind != DOWN_DATA && kind != AV_BAY_DATA {
                    continue;
                }
                let packet = sedsnet::wire_format::unpack_packet(body).unwrap();
                if ["BATTERY_VOLTAGE", "IMU_DATA", "BAROMETER_DATA"]
                    .iter()
                    .any(|name| packet.data_type() == crate::telemetry_schema::data_type(name))
                {
                    ordinary_lateness.push(now - packet.timestamp());
                }
                if packet.data_type() == sedsnet::config::DataType::DiscoveryAddress {
                    discovery_delivered += 1;
                } else if packet.data_type()
                    == crate::telemetry_schema::data_type("BATTERY_VOLTAGE")
                {
                    batteries.push((packet.timestamp(), now));
                } else if packet.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND")
                {
                    commands.push(now - packet.timestamp());
                } else if packet.data_type() == crate::telemetry_schema::data_type("GPS_DATA") {
                    gps.push(now - packet.timestamp());
                }
            }
        }
        eprintln!(
            "RRLS_SOURCE_PACED peak={peak} battery_deliveries={batteries:?} command_delays_ms={commands:?} gps_delays_ms={gps:?} max_sensor_lateness_ms={} dropped={}",
            ordinary_lateness.iter().max().copied().unwrap_or(0),
            relay.dropped
        );
        assert!(
            ordinary_lateness.iter().all(|delay| *delay <= 1500),
            "sensor lateness remains bounded"
        );
        assert_eq!(
            discovery_delivered,
            if with_discovery { 17 } else { 0 },
            "all independent route snapshots must survive fair service"
        );
        assert!(peak >= 24, "exercise the measured burst depth");
        assert_eq!(
            batteries.len(),
            4,
            "every sparse5s report must survive the bounded burst"
        );
        assert!(
            batteries
                .windows(2)
                .all(|pair| pair[1].1 - pair[0].1 <= 5_500)
        );
        assert_eq!(commands.len(), 4);
        assert!(commands.iter().all(|delay| *delay <= 500));
        assert_eq!(gps.len(), 4);
        assert!(gps.iter().all(|delay| *delay <= 500));
        assert_eq!(relay.dropped, 0, "do not hide expired accepted telemetry");
    }
    #[test]
    fn source_paced_sparse_telemetry_survives_bursts_without_delaying_controls() {
        source_paced_campaign(false);
    }
    #[test]
    fn discovery_burst_byte_fairness_preserves_sparse_reports_and_controls() {
        source_paced_campaign(true);
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
    fn fallback_air_shares_match_advertised_request_and_recovery() {
        let mut primary = Mesh::new(Role::Primary, 1);
        let mut relay = Mesh::new(Role::Rrls, 2);
        assert_eq!(primary.recovery_air_share(0), None);
        assert_eq!(relay.recovery_air_share(0), None);
        primary.tick(3_000);
        assert!(!primary.active(3_000), "no READY proof yet");
        assert_eq!(primary.recovery_air_share(3_000), Some(15));
        relay.incoming(true, &health(1, 0, false), 3_000);
        assert_eq!(relay.recovery_air_share(3_000), Some(35));
        assert_eq!(
            40 + 15 + 35,
            90,
            "RF share remains unchanged and total default reservation stays90%"
        );
        primary.direct(3_100);
        primary.tick(5_100);
        assert_eq!(primary.recovery_air_share(5_100), None);
        relay.incoming(true, &health(1, 1, true), 5_100);
        assert_eq!(relay.recovery_air_share(5_100), None);
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

pub fn settings_transport() -> Option<Shared> {
    RADIO.get().map(|(_, shared)| shared.clone())
}
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

#[cfg(test)]
#[path = "rrls/air_campaign.rs"]
mod air_campaign;
#[cfg(test)]
#[path = "rrls/captured_replay.rs"]
mod captured_replay;
#[cfg(test)]
#[path = "rrls/shared_air.rs"]
mod shared_air;

#[cfg(test)]
mod pair_tests {
    use super::*;
    use sedsnet::{packet::Packet, wire_format as serialize};
    use std::sync::Arc;

    fn control(receipt: bool, value: u64) -> Vec<u8> {
        crate::telemetry_schema::initialize().unwrap();
        let ty = if receipt {
            sedsnet::config::DataType::ReliableAck
        } else {
            crate::telemetry_schema::data_type("FLIGHT_COMMAND")
        };
        let payload: Arc<[u8]> = if receipt {
            Arc::from(value.to_le_bytes())
        } else {
            Arc::from([1u8])
        };
        serialize::pack_packet(
            &Packet::new(
                ty,
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "RF",
                value,
                payload,
            )
            .unwrap(),
        )
        .to_vec()
    }
    fn enqueue(mesh: &mut Mesh, bytes: &[u8], now: u64) {
        mesh.enqueue(Outgoing { command: true, bytes: encode(DOWN_DATA, bytes) }, 254, now);
    }
    #[test]
    fn bounded_pair_retains_two_owners_bytes_and_stats_through_refusal() {
        let a = control(true, 10);
        let b = control(true, 11);
        let mut mesh = Mesh::new(Role::Rrls, 2);
        enqueue(&mut mesh, &a, 0);
        enqueue(&mut mesh, &b, 0);
        let bytes = mesh.bytes;
        let tx = mesh.begin_bounded_transmission(0, 0, false, 2).unwrap();
        assert!(tx.follower_frame().is_some());
        assert_eq!(mesh.active_frame_count, 2);
        assert_eq!(mesh.bytes, bytes);
        assert_eq!(mesh.forwarded, 0);
        assert!(mesh.begin_transmission(0).is_none());
        mesh.finish_bounded_transmission(tx, false);
        assert_eq!(mesh.active_frame_count, 0);
        assert_eq!(mesh.bytes, bytes);
        let tx = mesh.begin_bounded_transmission(1, 0, false, 2).unwrap();
        assert_eq!(decode(&tx.frame().bytes).unwrap().1, a);
        assert_eq!(decode(&tx.follower_frame().unwrap().bytes).unwrap().1, b);
        let (first, second) = mesh.finish_bounded_transmission(tx, true).unwrap();
        assert_eq!(decode(&first.bytes).unwrap().1, a);
        assert_eq!(decode(&second.unwrap().bytes).unwrap().1, b);
        assert_eq!(mesh.forwarded, 2);
        assert_eq!(mesh.bytes, 0);
        assert!(mesh.queue.is_empty());
    }
    #[test]
    fn bounded_pair_never_searches_past_protected_ready_or_sdt() {
        let command = control(false, 10);
        for (blocker, priority) in [
            (encode(READY, &[0; 8]), 255),
            (encode(DOWN_DATA, b"SDT\x03opaque"), 255),
            (encode(DOWN_DATA, b"SDT\x03opaque"), 254),
        ] {
            let mut mesh = Mesh::new(Role::Rrls, 2);
            enqueue(&mut mesh, &command, 0);
            mesh.enqueue(
                Outgoing {
                    command: true,
                    bytes: blocker.clone(),
                },
                priority,
                0,
            );
            enqueue(&mut mesh, &control(true, 12), 0);
            let tx = mesh.begin_bounded_transmission(0, 0, true, 2).unwrap();
            assert!(tx.follower_frame().is_none());
            assert_eq!(decode(&tx.frame().bytes).unwrap().1, command);
            mesh.finish_bounded_transmission(tx, true);
            assert!(mesh.queue.iter().any(|item| item.frame.bytes == blocker));
        }
    }
    #[test]
    fn bounded_pair_rejects_ordered_corrupt_and_oversized_followers_without_reordering() {
        let command = control(false, 10);
        let packet = serialize::unpack_packet(&command).unwrap();
        let ordered = serialize::pack_packet_with_reliable(
            &packet,
            serialize::ReliableHeader {
                flags: 0,
                seq: 7,
                ack: 0,
            },
        )
        .to_vec();
        let mut corrupt = command.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        let oversized = serialize::pack_packet(
            &Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
                &std::array::from_fn::<_, 32, _>(|i| {
                    sedsnet::config::DataEndpoint(u32::MAX - i as u32)
                }),
                "RF",
                11,
                Arc::from([1u8]),
            )
            .unwrap(),
        )
        .to_vec();
        for follower in [ordered, corrupt, oversized] {
            let mut mesh = Mesh::new(Role::Rrls, 2);
            enqueue(&mut mesh, &command, 0);
            enqueue(&mut mesh, &follower, 0);
            let tx = mesh.begin_bounded_transmission(0, 0, false, 2).unwrap();
            assert!(tx.follower_frame().is_none());
            assert_eq!(
                decode(&mesh.queue.front().unwrap().frame.bytes).unwrap().1,
                follower
            );
            mesh.finish_bounded_transmission(tx, true);
        }
    }
    #[test]
    fn bounded_pair_clear_accounts_both_reserved_slots_at_full_queue() {
        let mut mesh = Mesh::new(Role::Rrls, 2);
        enqueue(&mut mesh, &control(false, 1), 0);
        enqueue(&mut mesh, &control(true, 2), 0);
        for _ in 2..MAX_FRAMES {
            mesh.enqueue(
                Outgoing {
                    command: false,
                    bytes: encode(DOWN_DATA, b"ordinary"),
                },
                0,
                0,
            );
        }
        let tx = mesh.begin_bounded_transmission(0, 0, false, 2).unwrap();
        assert!(tx.follower_frame().is_some());
        assert_eq!(
            mesh.queue.len() + usize::from(mesh.active_frame_count),
            MAX_FRAMES
        );
        enqueue(&mut mesh, &control(true, 3), 0);
        assert_eq!(
            mesh.queue.len() + usize::from(mesh.active_frame_count),
            MAX_FRAMES
        );
        let retained = tx.frame().bytes.len() + tx.follower_frame().unwrap().bytes.len();
        mesh.clear();
        assert_eq!(mesh.bytes, retained);
        assert!(mesh.queue.is_empty());
        mesh.finish_bounded_transmission(tx, false);
        assert_eq!(mesh.bytes, 0);
        assert_eq!(mesh.active_frame_count, 0);
        assert_eq!(mesh.forwarded, 0);
        assert_eq!(mesh.dropped, MAX_FRAMES as u64 + 1);
    }
    #[test]
    fn bounded_pair_honors_remaining_frame_budget_and_never_waits_for_future_data() {
        let mut mesh = Mesh::new(Role::Rrls, 2);
        enqueue(&mut mesh, &control(false, 1), 0);
        enqueue(&mut mesh, &control(true, 2), 0);
        assert!(mesh.begin_bounded_transmission(0, 0, false, 0).is_none());
        let tx = mesh.begin_bounded_transmission(0, 0, false, 1).unwrap();
        assert!(tx.follower_frame().is_none());
        mesh.finish_bounded_transmission(tx, true);
        let tx = mesh.begin_bounded_transmission(0, 0, false, 2).unwrap();
        assert!(tx.follower_frame().is_none());
        mesh.finish_bounded_transmission(tx, true);
        assert!(mesh.begin_bounded_transmission(0, 0, false, 2).is_none());
    }
}

#[cfg(test)]
#[path = "rrls/continuous_service_audit.rs"]
mod continuous_service_audit;
