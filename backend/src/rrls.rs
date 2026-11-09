//! Single recovery relay over the RFD asynchronous mesh. Wi-Fi is local only.
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

#[cfg(test)]
use crate::comms::virtual_uart as receipt_test_wire;

#[path = "rrls/continuous_clock.rs"]
mod continuous_clock;
#[path = "rrls/settings_framing.rs"]
mod settings_framing;

pub(crate) mod budget_negotiation;
const BUDGET_CONTROL: u8 = 13;
const RF_PROFILE_CONTROL: u8 = 14;

const LOSS_MS: u64 = 3_000;
/// Direct AV-bay frames must arrive with no gap longer than `DIRECT_GAP_MS`
/// for this long before the primary leaves relay mode.
const RESTORE_MS: u64 = 5_000;
/// Longest gap between direct frames that still counts as a continuous direct
/// path while restoring. One-Hz reports qualify; sparse discovery bursts from a
/// relay board that keeps restarting its router (about every 2-3 s) do not.
const DIRECT_GAP_MS: u64 = 1_500;
/// Minimum time in relay mode before the direct path may be restored, and in
/// direct mode (after a restore) before relay may be requested again. Together
/// with `RESTORE_MS` this bounds a marginal link to one change per 10 s.
const RELAY_MIN_DWELL_MS: u64 = 10_000;
const DIRECT_MIN_DWELL_MS: u64 = 10_000;
/// RRLS only: how recently it must have heard the AV bay itself before it
/// offers or performs uplink relaying. A relay that cannot hear the AV bay
/// only duplicates the primary's uplink on the air.
const RRLS_AV_BAY_FRESH_MS: u64 = 5_000;
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
/// A voice packet carries 320 ms of speech; one waiting longer than this is stale.
pub(crate) const VOICE_EXPIRY_MS: u64 = 400;
/// At most this many voice packets wait for the radio.
const VOICE_QUEUE: usize = 2;
const SETTINGS: u8 = 10;
// Host application control; kind12 remains reserved for legacy framing. Budget negotiation uses13.
const HOST_CONTROL: u8 = 11;
const MAX_SETTINGS_INBOX: usize = 4;
const SETTINGS_PEER_AGE_MS: u64 = 15_000;
/// Covers the RRLS's 2 s pairing resend with margin.
const PAIRING_PEER_AGE_MS: u64 = 5_000;
#[derive(Clone, Copy)]
struct FrameClass {
    priority: u8,
    recovery_control: bool,
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
    if crate::comms::rfd_profile::active().is_some_and(|p| {
        p.transport.usable_air_bps == 32_000 && p.transport.frame_overhead_bytes == 16
    }) && std::env::var_os("GS_RADIO_AIR_SHARE_PERCENT").is_none()
    {
        mesh.enable_settings_framing();
    }
    if let Some(expected) =
        crate::comms::rfd_profile::active().and_then(|p| p.recovery_budget_tuple())
    {
        mesh.enable_budget(expected);
    }
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

/// Typed queued/active ownership only. A busy mesh is uncertainty, not proof
/// that an accepted controller owner has drained. No parsing/SDK lock here.
pub(crate) fn has_pending_recovery_control() -> bool {
    RADIO.get().is_some_and(|(_,mesh)|mesh.try_lock().map_or(true,|mesh|mesh.pending_recovery_control()))
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
    /// All mesh transmissions, including presence, voice and settings.
    pub forwarded_frames: u64,
    /// Application envelopes actually transmitted by the recovery relay.
    pub relayed_frames: u64,
    /// Bytes in those RR envelopes; excludes the outer UART header.
    pub relay_envelope_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_framing: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_budget: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_decision: Option<RelayDecision>,
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
            relayed_frames: m.relayed_frames,
            relay_envelope_bytes: m.relay_envelope_bytes,
            settings_framing: m.settings_framing.as_ref().map(|state| state.diagnostics()),
            recovery_budget:m.budget_requested.then(||serde_json::json!({"requested_share_percent":m.recovery_air_share(now),"initialized":m.budget.is_some(),"effective_transport_compatible":!m.budget_transport_fault,"outbound_paused":m.budget_transport_fault,
                "fault_reason":if m.budget_transport_fault {Some("effective application/UART tuple differs from validated profile")}else{None},
                "tuple_scope":"application and UART configuration, not live modem AT readback",
                "expected_tuple":m.budget_profile.map(|p|serde_json::json!({"uart_baud":p.baud,"usable_air_bps":p.usable_bps,"rf_share_percent":p.rf_share,"frame_overhead_bytes":p.overhead,"packet_burst_ms":p.burst_ms,"packet_gap_ms":p.gap_ms,"flags":p.flags})),
                "actual_effective_tuple":m.budget_actual,"pending_queued_owner":m.queue.iter().any(|q|Mesh::budget_kind(&q.frame.bytes)),"inflight_owner":m.budget_inflight})),
            relay_decision: Some(m.relay_decision(now)),
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
            relayed_frames: 0,
            relay_envelope_bytes: 0,
            settings_framing: None,
            recovery_budget: None,
            relay_decision: None,
        }
    }
}

pub struct Outgoing {
    pub command: bool,
    pub bytes: Vec<u8>,
}
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsOrigin {
    None,
    Settings,
    Reply,
    FramingOffer,
}

struct Queued {
    settings_origin: SettingsOrigin,
    frame: Outgoing,
    at: u64,
    priority: u8,
    flags: u8,
    sdt_kind: u8,
    sdt_template: u32,
}
impl Queued {
    const INDEPENDENT_DISCOVERY: u8 = 1;
    const BUDGET_EVER_RESERVED: u8 = 2;
    const RECOVERY_CONTROL: u8 = 4;
    fn recovery_control(&self)->bool {self.flags&Self::RECOVERY_CONTROL!=0}
    fn independent_discovery(&self) -> bool {
        self.flags & Self::INDEPENDENT_DISCOVERY != 0
    }
    fn budget_ever_reserved(&self) -> bool {
        self.flags & Self::BUDGET_EVER_RESERVED != 0
    }
    fn set_independent_discovery(&mut self, value: bool) {
        if value {
            self.flags |= Self::INDEPENDENT_DISCOVERY;
        } else {
            self.flags &= !Self::INDEPENDENT_DISCOVERY;
        }
    }
    // Freeze protocol bytes on the first reservation, including zero-byte refusal.
    fn mark_budget_reserved(&mut self) {
        self.flags |= Self::BUDGET_EVER_RESERVED;
    }
    fn closes_packet_batch(&self) -> bool {
        if self.frame.bytes.get(3) == Some(&HOST_CONTROL) {
            return true;
        }
        !self.independent_discovery()
            && decode(&self.frame.bytes).is_some_and(|(_, body)| {
                crate::comms::closes_radio_packet_batch(self.priority, body)
            })
    }
    fn boundary_contract(&self) -> Option<(u64, bool)> {
        if self.frame.bytes.get(3) == Some(&HOST_CONTROL) {
            let (_, body) = decode(&self.frame.bytes)?;
            return crate::command_proxy::wire_boundary(body).map(|id| (id, false));
        }
        if self.independent_discovery() {
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
    physical: Option<Outgoing>,
    settings_action: Option<settings_framing::Action>,
}
impl Transmission {
    fn recovery_control(&self)->bool {
        self.queued.recovery_control() || self.follower.as_ref().is_some_and(|q|q.recovery_control())
    }

    pub(crate) fn frame(&self) -> &Outgoing {
        self.physical.as_ref().unwrap_or(&self.queued.frame)
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
    pub(crate) fn single_control_contract(&self) -> Option<(u64, bool)> {
        if self.settings_action.is_some() || self.follower.is_some() {
            return None;
        }
        Self::batch_contract(&self.queued)
    }
    fn batch_contract(queued: &Queued) -> Option<(u64, bool)> {
        if queued.priority < 254 || queued.independent_discovery() {
            return None;
        }
        if queued.frame.bytes.get(3) == Some(&HOST_CONTROL) {
            return queued.boundary_contract();
        }
        let (_, body) = decode(&queued.frame.bytes)?;
        crate::comms::radio_batch_control_contract(queued.priority, body)
    }
    /// Ordinary-lane wire bytes eligible for funding only after full successful IO.
    /// Dependent SDT stays in its existing lane; independent discovery/control does not.
    pub(crate) fn ordinary_background_service_bytes(&self) -> usize {
        if Mesh::budget_kind(&self.frame().bytes) {
            return 0;
        }
        if self.queued.priority >= 254
            || self.queued.settings_origin == SettingsOrigin::FramingOffer
            || (self.queued.frame.bytes.get(3) == Some(&SETTINGS)
                && self.queued.settings_origin == SettingsOrigin::None)
            || (self.settings_action.is_none()
                && self.queued.settings_origin == SettingsOrigin::None
                && self.queued.closes_packet_batch())
        {
            return 0;
        }
        self.frame().bytes.len() + 4
    }
    /// One completed physical ordinary owner, not ordinary fairness funding.
    /// Protocol negotiation/HostControl is never a source of this permission.
    pub(crate) fn completed_ordinary_owner_bytes(&self) -> usize {
        if self.queued.priority >= 254
            || self.queued.independent_discovery()
            || self.queued.settings_origin == SettingsOrigin::FramingOffer
            || self.queued.closes_packet_batch()
        {
            return 0;
        }
        let kind = self.frame().bytes.get(3);
        // A timed-out negotiation still sends the original whole legacy owner.
        // Probe/fragment actions and their physical kind15 envelopes never fund it.
        if self.settings_action.is_some()
            && !(self.settings_action == Some(settings_framing::Action::Legacy)
                && kind == Some(&SETTINGS)
                && matches!(
                    self.queued.settings_origin,
                    SettingsOrigin::Settings | SettingsOrigin::Reply
                ))
        {
            return 0;
        }
        if matches!(kind, Some(&DOWN_DATA) | Some(&UP_DATA) | Some(&AV_BAY_DATA))
            || (kind == Some(&SETTINGS)
                && matches!(
                    self.queued.settings_origin,
                    SettingsOrigin::Settings | SettingsOrigin::Reply
                ))
        {
            self.frame().bytes.len() + 4
        } else {
            0
        }
    }
    pub(crate) fn successful_ordinary_service_bytes(&self) -> usize {
        if self.queued.priority < 254
            && !self.queued.closes_packet_batch()
            && !self.queued.independent_discovery()
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
        self.settings_action.is_none() && self.queued.closes_packet_batch()
    }
    pub(crate) fn boundary_contract(&self) -> Option<(u64, bool)> {
        if self.settings_action.is_some() {
            return None;
        }
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
    /// A station-pairing frame from the peer arrived over the radio. Link
    /// freshness only for the pairing exchange's own frames: no settings
    /// authority, lease or control (the exchange authenticates itself).
    pairing_peer_seen: Option<u64>,
    settings_inbox: VecDeque<Vec<u8>>,
    command_proxy_inbox: VecDeque<Vec<u8>>,
    started: Instant,
    // The shared simulation clock survives host process restarts.
    clock_origin_ms: u64,
    #[cfg(test)]
    budget_test_clock: Option<u64>,
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
    /// Primary relay decision record, for diagnostics and topology.
    relay_changed_at: Option<u64>,
    relay_reason: Option<RelayReason>,
    relay_transitions: u64,
    /// RRLS: last CRC-valid frame heard directly from the AV bay.
    av_bay_heard: Option<u64>,
    /// RRLS: uplink frames not relayed because it could not hear the AV bay.
    relay_suppressed_no_av_bay: u64,
    relay_seen: Option<u64>,
    topology_peer_seen: Option<u64>,
    topology_relay_rx: Option<u64>,
    last_ready_sequence: Option<u32>,
    last_ready_sent: Option<u64>,
    queue: VecDeque<Queued>,
    bytes: usize,
    dropped: u64,
    forwarded: u64,
    relayed_frames: u64,
    relay_envelope_bytes: u64,
    templates: VecDeque<TemplateClass>,
    active_transmission: Option<u64>,
    active_frame_count: u8,
    active_recovery_control: bool,
    active_cancelled: bool,
    active_survives_clear: bool,
    next_transmission: u64,
    discovery_balance_bytes: i32,
    receipt_service_pending: bool,
    active_fair_delta_bytes: i32,
    fair_overhead_bytes: u16,
    settings_framing: Option<Box<settings_framing::State>>,
    budget: Option<budget_negotiation::Driver>,
    budget_requested: bool,
    budget_profile: Option<budget_negotiation::Profile>,
    budget_actual: Option<crate::comms::EffectiveRadioBudget>,
    budget_inflight: bool,
    budget_transport_fault: bool,
}

impl Mesh {
    pub fn new(role: Role, session: u32) -> Self {
        Self {
            role,
            settings_framing: None,
            budget: None,
            budget_requested: false,
            budget_profile: None,
            budget_actual: None,
            budget_inflight: false,
            budget_transport_fault: false,
            voice_enabled: false,
            settings_peer_seen: None,
            pairing_peer_seen: None,
            settings_inbox: VecDeque::new(),
            command_proxy_inbox: VecDeque::new(),
            started: Instant::now(),
            clock_origin_ms: crate::simulation_clock_ms().unwrap_or(0),
            #[cfg(test)]
            budget_test_clock: None,
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
            relay_changed_at: None,
            relay_reason: None,
            relay_transitions: 0,
            av_bay_heard: None,
            relay_suppressed_no_av_bay: 0,
            relay_seen: None,
            topology_peer_seen: None,
            topology_relay_rx: None,
            last_ready_sequence: None,
            last_ready_sent: None,
            queue: VecDeque::new(),
            bytes: 0,
            dropped: 0,
            forwarded: 0,
            relayed_frames: 0,
            relay_envelope_bytes: 0,
            templates: VecDeque::new(),
            active_transmission: None,
            active_frame_count: 0,
            active_recovery_control: false,
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
        #[cfg(test)]
        if let Some(now) = self.budget_test_clock {
            return now;
        }
        // A linked emulator can run far slower than wall time. Liveness, relay
        // leases and UART drain must advance on the same explicit test clock.
        // Ordinary stations and standalone HITL retain their monotonic clock.
        crate::simulation_clock_ms()
            .unwrap_or_else(|| self.started.elapsed().as_millis().min(u64::MAX as u128) as u64)
    }
    fn direct(&mut self, now: u64) {
        // A gap longer than DIRECT_GAP_MS restarts restoration. One-Hz reports
        // keep it running; sparse bursts every few seconds never complete it.
        if self
            .last_direct
            .is_none_or(|v| now.saturating_sub(v) >= DIRECT_GAP_MS)
        {
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
        if self.budget_requested {
            return Some(
                self.budget
                    .as_ref()
                    .map_or(if self.role == Role::Primary { 15 } else { 35 }, |budget| {
                        budget.peek_share(now)
                    }),
            );
        }
        match self.role {
            // Match the advertised relay request even before READY is received.
            Role::Primary if self.fallback => Some(15),
            Role::Rrls if self.active(now) => Some(35),
            _ => None,
        }
    }

    /// Primary relay decision with hysteresis. Enter relay only after
    /// LOSS_MS with no direct AV-bay frame (and not within DIRECT_MIN_DWELL_MS
    /// of a restore); leave only after RESTORE_MS of continuous direct frames
    /// and at least RELAY_MIN_DWELL_MS in relay.
    fn decide_relay(&mut self, now: u64) {
        let gap = self
            .last_direct
            .map_or(now.saturating_sub(self.clock_origin_ms), |v| now.saturating_sub(v));
        let dwell = self.relay_changed_at.map(|at| now.saturating_sub(at));
        if !self.fallback {
            // The first request after startup has no prior restore to respect.
            let settled = dwell.is_none_or(|d| d >= DIRECT_MIN_DWELL_MS);
            if gap >= LOSS_MS && settled {
                self.set_relay(true, now, RelayReason::DirectLost { gap_ms: gap });
            }
        } else {
            let continuous = self
                .direct_since
                .filter(|_| gap < DIRECT_GAP_MS)
                .map(|since| now.saturating_sub(since));
            let settled = dwell.is_none_or(|d| d >= RELAY_MIN_DWELL_MS);
            if let Some(continuous) = continuous
                && continuous >= RESTORE_MS
                && settled
            {
                self.set_relay(false, now, RelayReason::DirectRestored { continuous_ms: continuous });
            }
        }
    }
    fn set_relay(&mut self, fallback: bool, now: u64, reason: RelayReason) {
        if self.fallback != fallback {
            self.relay_transitions = self.relay_transitions.saturating_add(1);
            log::info!(
                "Radio recovery: {} ({})",
                if fallback { "requesting RRLS relay" } else { "direct AV-bay path restored" },
                reason.describe()
            );
        }
        self.fallback = fallback;
        self.relay_changed_at = Some(now);
        self.relay_reason = Some(reason);
    }
    /// Test fixtures that model an RRLS which hears the AV bay throughout
    /// (the relay case) without feeding it direct AV-bay frames.
    #[cfg(test)]
    pub(crate) fn assume_av_bay_heard_for_test(&mut self) {
        // hears_av_bay() saturates now - heard to 0: always fresh.
        self.av_bay_heard = Some(u64::MAX);
    }
    /// RRLS: it heard the AV bay recently enough for relaying to help.
    fn hears_av_bay(&self, now: u64) -> bool {
        self.av_bay_heard
            .is_some_and(|heard| now.saturating_sub(heard) < RRLS_AV_BAY_FRESH_MS)
    }
    pub(crate) fn relay_decision(&self, now: u64) -> RelayDecision {
        RelayDecision {
            relay_requested: self.fallback,
            changed_ago_ms: self.relay_changed_at.map(|at| now.saturating_sub(at)),
            reason: self.relay_reason.map(|reason| reason.describe()),
            transitions: self.relay_transitions,
            direct_age_ms: self.last_direct.map(|last| now.saturating_sub(last)),
            direct_continuous_ms: self
                .direct_since
                .filter(|_| self.last_direct.is_some_and(|l| now.saturating_sub(l) < DIRECT_GAP_MS))
                .map(|since| now.saturating_sub(since)),
            rrls_hears_av_bay: (self.role == Role::Rrls).then(|| self.hears_av_bay(now)),
            rrls_suppressed_no_av_bay: self.relay_suppressed_no_av_bay,
        }
    }

    pub fn tick(&mut self, now: u64) {
        self.observe_settings_framing(now);
        if self.role == Role::Primary {
            self.decide_relay(now);
            if self
                .last_health_sent
                .is_none_or(|v| now.saturating_sub(v) >= 500)
            {
                let mut payload = self.session.to_le_bytes().to_vec();
                payload.extend_from_slice(&self.sequence.to_le_bytes());
                payload.push(u8::from(!self.fallback));
                if let Some(budget) = self.budget.as_mut() {
                    budget.health(self.session, self.sequence, !self.fallback, now);
                }
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
            // READY moves the primary's uplink onto this relay. Offer it only
            // while this station hears the AV bay; otherwise relaying would
            // just repeat the primary's uplink on the same air.
            if (self.active(now) && self.hears_av_bay(now) || self.voice_enabled && peer_fresh)
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
            && let Some(fix) = crate::antenna::local_fix()
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
        self.poll_budget(now);
    }
    fn secure_budget_nonce() -> Option<budget_negotiation::Nonce> {
        let mut bytes = [0; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).ok()?;
        budget_negotiation::Nonce::new(bytes)
    }
    pub(crate) fn enable_budget(&mut self, expected: budget_negotiation::Profile) {
        self.enable_budget_with_entropy(expected, Self::secure_budget_nonce);
    }
    pub(crate) fn enable_budget_with_entropy(
        &mut self,
        expected: budget_negotiation::Profile,
        entropy: fn() -> Option<budget_negotiation::Nonce>,
    ) {
        self.budget_requested = true;
        self.budget_profile = Some(expected);
        self.budget = budget_negotiation::Driver::new(
            self.role == Role::Primary,
            self.session,
            expected,
            entropy,
        );
        if self.budget.is_none() {
            log::warn!(
                "Recovery budget requested but initialization failed; retaining conservative role share"
            );
        }
    }
    pub(crate) fn note_budget_effective(
        &mut self,
        actual: Option<crate::comms::EffectiveRadioBudget>,
    ) {
        self.budget_actual = actual;
    }
    pub(crate) fn budget_expected_profile(&self) -> Option<budget_negotiation::Profile> {
        self.budget
            .as_ref()
            .map(|b| b.expected())
            .or(self.budget_profile)
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn budget_transport_fault_for_test(&self) -> bool {
        self.budget_transport_fault
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn queued_budget_for_test(&self) -> Option<(Vec<u8>, bool)> {
        self.queue
            .iter()
            .find(|q| Self::budget_kind(&q.frame.bytes))
            .map(|q| (q.frame.bytes.clone(), q.budget_ever_reserved()))
    }
    pub(crate) fn budget_effective_transport(&mut self, matches: bool) -> bool {
        if !matches {
            if !self.budget_transport_fault {
                log::warn!(
                    "Recovery budget negotiation blocked: effective UART/capacity tuple differs from validated profile"
                );
            }
            self.budget_transport_fault = true;
            if let Some(budget) = self.budget.as_mut() {
                budget.fail_closed();
            }
            return false;
        }
        if self.budget_transport_fault {
            let Some(expected) = self.budget_expected_profile() else {
                return false;
            };
            let Some(budget) = budget_negotiation::Driver::new(
                self.role == Role::Primary,
                self.session,
                expected,
                Self::secure_budget_nonce,
            ) else {
                return false;
            };
            self.budget = Some(budget);
            self.budget_transport_fault = false;
        }
        true
    }
    fn budget_kind(bytes: &[u8]) -> bool {
        matches!(
            bytes.get(3),
            Some(&BUDGET_CONTROL) | Some(&RF_PROFILE_CONTROL)
        )
    }
    fn poll_budget(&mut self, now: u64) {
        let Some(mut state) = self.budget.take() else {
            return;
        };
        state.share(now);
        let mut removed = 0usize;
        let mut expired = 0u64;
        self.queue.retain(|q| {
            let is_budget = Self::budget_kind(&q.frame.bytes);
            let too_old = is_budget && (now < q.at || now.saturating_sub(q.at) > 8_000);
            if is_budget && (too_old || !state.valid(&q.frame.bytes, now)) {
                removed += q.frame.bytes.len();
                expired += u64::from(too_old);
                false
            } else {
                true
            }
        });
        self.bytes -= removed;
        self.dropped = self.dropped.saturating_add(expired);
        for queued in &mut self.queue {
            if !queued.budget_ever_reserved()
                && let Some(frame) = state.refresh_unreserved_revoke(&queued.frame.bytes, now)
            {
                debug_assert_eq!(queued.frame.bytes.len(), frame.wire().len());
                queued.frame.bytes.copy_from_slice(frame.wire());
            }
        }
        if !self.budget_inflight
            && !self.queue.iter().any(|q| Self::budget_kind(&q.frame.bytes))
            && let Some(frame) = state.prepare(now)
        {
            self.enqueue(
                Outgoing {
                    command: true,
                    bytes: frame.wire().to_vec(),
                },
                // Protected selection only: normal paid Capacity readiness still
                // applies; these frames carry no command or fairness credit.
                254,
                now,
            );
        }
        self.budget = Some(state);
    }
    fn budget_committed(&mut self, frame: &Outgoing, sent: bool) {
        if sent
            && let Some((HEALTH, payload)) = decode(&frame.bytes)
            && payload.len() == 9
            && payload[8] <= 1
        {
            let now = self.now();
            if let Some(state) = self.budget.as_mut() {
                state.full_health_written(
                    u32::from_le_bytes(payload[..4].try_into().unwrap()),
                    u32::from_le_bytes(payload[4..8].try_into().unwrap()),
                    payload[8] != 0,
                    now,
                );
            }
        }
        if Self::budget_kind(&frame.bytes) {
            let now = self.now();
            if let Some(state) = self.budget.as_mut() {
                state.committed(&frame.bytes, now, sent);
            }
        }
    }
    pub(crate) fn enable_settings_framing(&mut self) {
        self.settings_framing = Some(Box::new(settings_framing::State::new(
            self.role == Role::Primary,
        )));
    }
    fn settings_context(&self, now: u64) -> settings_framing::Context {
        settings_framing::Context {
            mesh_ms: now,
            continuous_ms: continuous_clock::now(now),
            session: self.operating_mode_session(now),
            sequence: if self.role == Role::Primary {
                self.sequence.wrapping_sub(1)
            } else {
                self.primary_sequence
            },
            peer_fresh: self.settings_available(now) || self.pairing_peer_fresh(now),
        }
    }
    /// An unpaired standby RRLS sends no READY and holds no authenticated
    /// settings presence, so without this the primary's framing layer failed
    /// closed on the first pairing reply too large for one frame (BUNDLE) and
    /// silently dropped it on every REVEAL retry.
    fn pairing_peer_fresh(&self, now: u64) -> bool {
        self.pairing_peer_seen
            .is_some_and(|last| now >= last && now - last < PAIRING_PEER_AGE_MS)
    }
    /// The pairing worker took a station-pairing frame from the peer.
    pub(crate) fn note_pairing_peer(&mut self, now: u64) {
        self.pairing_peer_seen = Some(self.pairing_peer_seen.map_or(now, |old| old.max(now)));
    }
    fn observe_settings_framing(&mut self, now: u64) {
        let context = self.settings_context(now);
        if let Some(state) = self.settings_framing.as_mut() {
            state.observe(context);
        }
    }
    fn settings_reserved_bytes(&self) -> usize {
        if let Some(state) = self.settings_framing.as_ref() {
            state.reserved_bytes() + self.settings_inbox.iter().map(Vec::capacity).sum::<usize>()
        } else {
            0
        }
    }
    fn settings_parent(&self, queued: &Queued) -> bool {
        queued.frame.bytes.get(3) == Some(&SETTINGS)
            && self
                .settings_framing
                .as_ref()
                .is_some_and(|state| state.owns(settings_framing::OwnerId::of(&queued.frame.bytes)))
    }
    fn retire_settings_parent(&mut self, queued: &Queued) {
        if queued.settings_origin == SettingsOrigin::FramingOffer
            && let Some((_, body)) = decode(&queued.frame.bytes)
                && let Some(state) = self.settings_framing.as_mut() {
                    state.offer_pending(body, false);
                }
        if queued.frame.bytes.get(3) == Some(&SETTINGS)
            && let Some(state) = self.settings_framing.as_mut() {
                state.retire(settings_framing::OwnerId::of(&queued.frame.bytes));
            }
    }
    fn prepare_settings_physical(&mut self, tx: &mut Transmission, now: u64) -> bool {
        if !self.settings_parent(&tx.queued) {
            return true;
        }
        let context = self.settings_context(now);
        let Some((SETTINGS, inner)) = decode(&tx.queued.frame.bytes) else {
            return false;
        };
        let Some(physical) = self.settings_framing.as_mut().and_then(|state| {
            state.physical(
                inner,
                settings_framing::OwnerId::of(&tx.queued.frame.bytes),
                context,
            )
        }) else {
            return false;
        };
        tx.settings_action = Some(physical.action);
        if physical.action != settings_framing::Action::Legacy {
            let bytes = settings_framing::wrap(&physical.body);
            assert!(bytes.len() + 4 <= settings_framing::QUANTUM);
            tx.physical = Some(Outgoing {
                command: true,
                bytes,
            });
        }
        true
    }
    pub(crate) fn note_settings_physical_prefix(&mut self, tx: &Transmission, accepted: usize) {
        if tx.settings_action.is_some()
            && let Some(action) = tx.settings_action
                && let Some(state) = self.settings_framing.as_mut() {
                    state.note_accepted_prefix(
                        settings_framing::OwnerId::of(&tx.queued.frame.bytes),
                        accepted,
                        action,
                    );
                }
    }
    pub(crate) fn note_settings_counted_drain(
        &mut self,
        tx: &Transmission,
        model_stop_us: Option<u64>,
    ) {
        let Some(action) = tx.settings_action else {
            return;
        };
        let context = self.settings_context(self.now());
        let Some(now) = context.continuous_ms else {
            if let Some(s) = self.settings_framing.as_mut() {
                s.observe(context);
            }
            return;
        };
        // Compare model timestamps only with model timestamps; add the
        // remaining serialization DURATION to local continuous time.
        let remaining =
            model_stop_us.map_or(0, |us| us.div_ceil(1000).saturating_sub(context.mesh_ms));
        let Some(complete) = now.checked_add(remaining) else {
            return;
        };
        if let Some(s) = self.settings_framing.as_mut() {
            s.note_counted_drain(
                settings_framing::OwnerId::of(&tx.queued.frame.bytes),
                action,
                complete,
            );
        }
    }

    fn clear(&mut self) {
        self.active_cancelled |= self.active_transmission.is_some() && !self.active_survives_clear;
        let old = self.queue.len();
        let mut removed_bytes = 0;
        let framing = &mut self.settings_framing;
        self.queue.retain(|q| {
            let keep = matches!(
                q.frame.bytes.get(3),
                Some(&POSITION)
                    | Some(&VOICE)
                    | Some(&SETTINGS)
                    | Some(&HOST_CONTROL)
                    | Some(&BUDGET_CONTROL)
                    | Some(&RF_PROFILE_CONTROL)
            );
            if !keep {
                removed_bytes += q.frame.bytes.len();
                if q.settings_origin == SettingsOrigin::FramingOffer
                    && let Some((_, body)) = decode(&q.frame.bytes)
                        && let Some(state) = framing.as_mut() {
                            state.offer_pending(body, false);
                        }
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
        let offset = match bytes[3] {
            1 => 4,
            2 | 4 | 5 => 5,
            // A chunk's template cannot be known without reassembly. It is a
            // conservative barrier within its explicit RR wire namespace.
            _ => return (2, 0),
        };
        let mut template = 0u32;
        for (offset, shift) in (offset..).zip((0..=28).step_by(7)) {
            let Some(&byte) = bytes.get(offset).filter(|_| offset < end) else {
                return (2, 0);
            };
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

    fn queued_expired(&self, q: &Queued, now: u64) -> bool {
        // New context-bound budget owner only; application/control/source TTLs below are untouched.
        if Self::budget_kind(&q.frame.bytes) {
            return now < q.at || now.saturating_sub(q.at) > 8_000;
        }

        if q.settings_origin == SettingsOrigin::FramingOffer {
            return !decode(&q.frame.bytes).is_some_and(|(kind, body)| {
                kind == settings_framing::KIND
                    && self
                        .settings_framing
                        .as_ref()
                        .is_some_and(|state| state.offer_is_live(body, self.settings_context(now)))
            });
        }
        if self.settings_parent(q) {
            let state = self.settings_framing.as_ref().unwrap();
            if state.failed(
                settings_framing::OwnerId::of(&q.frame.bytes),
                continuous_clock::now(now),
            ) {
                return true;
            }
            if state.started(settings_framing::OwnerId::of(&q.frame.bytes)) {
                return false;
            }
        }
        now.saturating_sub(q.at)
            > if q.settings_origin == SettingsOrigin::Reply {
                5_000
            } else if q.frame.bytes.get(3) == Some(&VOICE) {
                VOICE_EXPIRY_MS
            } else if q.independent_discovery() {
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
    }

    #[cfg(test)]
    pub(crate) fn has_settings_parent_for_test(&self) -> bool {
        self.queue.iter().any(|queued| self.settings_parent(queued))
    }

    pub(crate) fn ordinary_background_state(&self) -> crate::comms::OrdinaryLaneState {
        use crate::comms::OrdinaryLaneState;
        let mut owned = false;
        for (index, queued) in self.queue.iter().enumerate() {
            if queued.priority >= 254 {
                continue;
            }
            if queued.frame.bytes.get(3) == Some(&SETTINGS)
                && queued.settings_origin == SettingsOrigin::None
            {
                owned = true;
                continue;
            }
            if queued.settings_origin == SettingsOrigin::None && queued.closes_packet_batch() {
                // Unknown opaque ownership is retained conservatively; never
                // infer a metadata service contract from TC/CP-looking bytes.
                owned = true;
                continue;
            }
            owned = true;
            if self.dependency_ready(index) {
                return OrdinaryLaneState::Ready;
            }
        }
        if owned || self.active_transmission.is_some() {
            OrdinaryLaneState::Blocked
        } else {
            OrdinaryLaneState::Empty
        }
    }

    fn budget_reservation_ready(&self, queued: &Queued, now: u64) -> bool {
        queued.budget_ever_reserved()
            || !Self::budget_kind(&queued.frame.bytes)
            || self
                .budget
                .as_ref()
                .is_some_and(|state| state.unreserved_revoke_ready(&queued.frame.bytes, now))
    }
    fn dependency_ready(&self, index: usize) -> bool {
        if !self.budget_reservation_ready(&self.queue[index], self.now()) {
            return false;
        }
        if self.settings_parent(&self.queue[index])
            && self.settings_framing.as_ref().is_some_and(|state| {
                !state.selectable(settings_framing::OwnerId::of(
                    &self.queue[index].frame.bytes,
                ))
            })
        {
            return false;
        }
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

    pub(crate) fn pending_recovery_control(&self)->bool {
        self.active_recovery_control || self.queue.iter().any(|queued|queued.recovery_control())
    }
    fn recovery_control_for_frame(&mut self,frame:&Outgoing,origin:SettingsOrigin)->bool {
        let Some((kind,body))=decode(&frame.bytes) else {return true;};
        match kind {
            DOWN_COMMAND|UP_COMMAND|AV_BAY_COMMAND|HOST_CONTROL=>true,
            DOWN_DATA|UP_DATA|AV_BAY_DATA=>self.classify_namespace(body,kind!=DOWN_DATA)
                .is_none_or(|class|class.recovery_control),
            HEALTH|READY|POSITION|VOICE|BUDGET_CONTROL|RF_PROFILE_CONTROL=>false,
            SETTINGS=>origin!=SettingsOrigin::FramingOffer,
            _=>true,
        }
    }

    fn enqueue(&mut self, frame: Outgoing, priority: u8, now: u64) {
        self.enqueue_with_origin(frame, priority, now, SettingsOrigin::None);
    }
    fn enqueue_with_origin(
        &mut self,
        frame: Outgoing,
        priority: u8,
        now: u64,
        origin: SettingsOrigin,
    ) -> bool {
        // A bounded receiver reply is protocol metadata for admission only.
        // Its physical priority remains0 and it never receives an early loan.
        let incoming_rank = if origin == SettingsOrigin::FramingOffer {
            507
        } else {
            u16::from(priority) * 2
        };
        if frame.bytes.len() > MAX_PAYLOAD + 8 {
            self.dropped += 1;
            return false;
        }
        // GPS and health are current values, not history. Keep only the newest.
        if priority == 255
            && (frame.bytes.get(3) == Some(&HEALTH)
                || !frame
                    .bytes
                    .get(4..)
                    .is_some_and(|body| body.starts_with(b"SDT")))
            && let Some(i) = self
                .queue
                .iter()
                .position(|q| q.priority == 255 && q.frame.bytes.get(3) == frame.bytes.get(3))
            {
                let old = self.queue.remove(i).unwrap();
                self.bytes -= old.frame.bytes.len();
                self.dropped += 1;
            }
        while self.queue.len() + usize::from(self.active_frame_count) >= MAX_FRAMES
            || self.bytes + frame.bytes.len() + self.settings_reserved_bytes() > MAX_BYTES
        {
            let Some((i, p)) = self
                .queue
                .iter()
                .enumerate()
                .filter(|(_, q)| !self.settings_parent(q))
                .map(|(i, q)| {
                    (
                        i,
                        if q.settings_origin == SettingsOrigin::FramingOffer
                            && !self.queued_expired(q, now)
                        {
                            507
                        } else {
                            u16::from(q.priority) * 2
                        },
                    )
                })
                .min_by_key(|(_, rank)| *rank)
            else {
                // The only reservation may belong to an active serial write;
                // it cannot be evicted or admitted a second time.
                self.dropped += 1;
                return false;
            };
            if p > incoming_rank {
                self.dropped += 1;
                return false;
            }
            let old = self.queue.remove(i).unwrap();
            self.bytes -= old.frame.bytes.len();
            self.retire_settings_parent(&old);
            self.dropped += 1;
        }
        self.bytes += frame.bytes.len();
        let independent_discovery = Self::independent_discovery(&frame);
        let (sdt_kind, sdt_template) = Self::sdt_dependency(&frame);
        let recovery_control=self.recovery_control_for_frame(&frame,origin);
        let mut queued = Queued {
            settings_origin: origin,
            frame,
            at: now,
            priority,
            flags: 0,
            sdt_kind,
            sdt_template,
        };
        if recovery_control {queued.flags|=Queued::RECOVERY_CONTROL;}
        queued.set_independent_discovery(independent_discovery);
        self.queue.push_back(queued);
        true
    }
    #[cfg(test)]
    pub(crate) fn begin_transmission(&mut self, now: u64) -> Option<Transmission> {
        self.begin_transmission_at_priority(now, 0)
    }
    #[cfg(test)]
    pub(crate) fn begin_transmission_at_priority(
        &mut self,
        now: u64,
        minimum_priority: u8,
    ) -> Option<Transmission> {
        self.begin_matching_transmission(now, minimum_priority, false)
    }
    #[cfg(test)]
    pub(crate) fn begin_critical_transmission(
        &mut self,
        now: u64,
        minimum_priority: u8,
    ) -> Option<Transmission> {
        self.begin_matching_transmission(now, minimum_priority, true)
    }
    /// Returning owner receipts may pass only already-forwarded command
    /// retries at the same priority. Fresh controls and opaque SDT/READY/GPS
    /// entries remain barriers; ownership, age and service credits are unchanged.
    pub(crate) fn prioritize_receipt_over_replays(&mut self, replayed: impl Fn(u64) -> bool) {
        if self.active_transmission.is_some() {
            return;
        }
        let Some(head) = self
            .queue
            .iter()
            .enumerate()
            .filter(|(i, item)| {
                !item.independent_discovery() && item.priority >= 254 && self.dependency_ready(*i)
            })
            .max_by_key(|(i, item)| (item.priority, std::cmp::Reverse(*i)))
            .map(|(i, _)| i)
        else {
            return;
        };
        let priority = self.queue[head].priority;
        if !Transmission::batch_contract(&self.queue[head])
            .is_some_and(|(id, receipt)| !receipt && replayed(id))
        {
            return;
        }
        for index in head + 1..self.queue.len() {
            let item = &self.queue[index];
            if item.independent_discovery() || item.priority < priority {
                continue;
            }
            if item.priority != priority || !self.dependency_ready(index) {
                break;
            }
            match Transmission::batch_contract(item) {
                Some((_, true)) => {
                    let receipt = self.queue.remove(index).unwrap();
                    self.queue.insert(head, receipt);
                    break;
                }
                Some((id, false)) if replayed(id) => {}
                _ => break,
            }
        }
    }

    /// One independent background reply; it never takes critical capacity/loans.
    pub(crate) fn begin_mode_reply_transmission(&mut self, now: u64) -> Option<Transmission> {
        self.observe_settings_framing(now);
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
        let index = self
            .queue
            .iter()
            .enumerate()
            .find(|(index, q)| {
                q.settings_origin == SettingsOrigin::Reply && self.dependency_ready(*index)
            })
            .map(|(index, _)| index)?;
        if now < self.queue[index].at || now.saturating_sub(self.queue[index].at) > 5_000 {
            let expired = self.queue.remove(index).unwrap();
            self.bytes -= expired.frame.bytes.len();
            self.retire_settings_parent(&expired);
            self.dropped += 1;
            return None;
        }
        let id = self.next_transmission.checked_add(1)?;
        let queued = self.queue.remove(index).unwrap();
        let mut tx = Transmission {
            queued,
            follower: None,
            id,
            physical: None,
            settings_action: None,
        };
        if !self.prepare_settings_physical(&mut tx, now) {
            self.queue.push_back(tx.queued);
            return None;
        }
        self.next_transmission = id;
        self.active_transmission = Some(id);
        self.active_frame_count = 1;
        self.active_recovery_control=tx.recovery_control();
        self.active_survives_clear = true;
        self.active_fair_delta_bytes = 0;
        Some(tx)
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
                !item.independent_discovery()
                    && item.priority >= 254
                    && self.dependency_ready(*index)
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
                self.active_recovery_control=transmission.recovery_control();
                self.active_survives_clear = false;
            }
        }
        Some(transmission)
    }

    /// A typed receiver reservation reply remains an ordinary paid owner.
    /// The caller must hold the UART writer and require full Capacity readiness.
    fn begin_framing_offer_transmission(
        &mut self,
        now: u64,
        minimum_priority: u8,
    ) -> Option<Transmission> {
        if self.active_transmission.is_some() {
            return None;
        }
        loop {
            let index = self.queue.iter().position(|q| {
                q.settings_origin == SettingsOrigin::FramingOffer && q.priority >= minimum_priority
            })?;
            if self.queued_expired(&self.queue[index], now) {
                let old = self.queue.remove(index).unwrap();
                self.bytes -= old.frame.bytes.len();
                self.retire_settings_parent(&old);
                self.dropped += 1;
                continue;
            }
            let id = self.next_transmission.checked_add(1)?;
            let queued = self.queue.remove(index).unwrap();
            self.next_transmission = id;
            self.active_transmission = Some(id);
            self.active_frame_count = 1;
            self.active_recovery_control=queued.recovery_control();
            self.active_fair_delta_bytes = 0;
            self.active_survives_clear = false;
            return Some(Transmission {
                queued,
                follower: None,
                id,
                physical: None,
                settings_action: None,
            });
        }
    }
    pub(crate) fn queued_budget_owner(&mut self, now: u64) -> bool {
        self.poll_budget(now);
        self.queue
            .iter()
            .any(|q| Self::budget_kind(&q.frame.bytes) && self.budget_reservation_ready(q, now))
    }
    pub(crate) fn begin_paid_budget_transmission(&mut self, now: u64) -> Option<Transmission> {
        if self.active_transmission.is_some() {
            return None;
        }
        self.poll_budget(now);
        let index = self.queue.iter().position(|q| {
            Self::budget_kind(&q.frame.bytes) && self.budget_reservation_ready(q, now)
        })?;
        if self.queued_expired(&self.queue[index], now) {
            return None;
        }
        let id = self.next_transmission.checked_add(1)?;
        let mut queued = self.queue.remove(index)?;
        queued.mark_budget_reserved();
        self.next_transmission = id;
        self.active_transmission = Some(id);
        self.active_frame_count = 1;
        self.active_recovery_control=queued.recovery_control();
        self.active_fair_delta_bytes = 0;
        self.budget_inflight = true;
        self.active_survives_clear = true;
        Some(Transmission {
            queued,
            follower: None,
            id,
            physical: None,
            settings_action: None,
        })
    }
    pub(crate) fn begin_primary_background_transmission(
        &mut self,
        now: u64,
        minimum_priority: u8,
        normal_ready: bool,
        allow_ordinary: bool,
    ) -> Option<Transmission> {
        let protected =
            self.begin_bounded_transmission(now, minimum_priority.max(254), !normal_ready, 1);
        if protected.is_some() {
            return protected;
        }
        if !normal_ready {
            return None;
        }
        if let Some(offer) = self.begin_framing_offer_transmission(now, minimum_priority) {
            return Some(offer);
        }
        if !allow_ordinary {
            return None;
        }
        self.begin_mode_reply_transmission(now)
            .or_else(|| self.begin_bounded_transmission(now, minimum_priority, false, 1))
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
        self.poll_budget(now);
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
                    !item.independent_discovery()
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
            if !critical_only && protected.is_none()
                && let Some(offer) = self.begin_framing_offer_transmission(now, minimum_priority) {
                    return Some(offer);
                }
            let discovery = if critical_only {
                None
            } else {
                self.queue
                    .iter()
                    .position(|item| item.independent_discovery())
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
                            && !item.independent_discovery()
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
                let highest = self
                    .queue
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| self.dependency_ready(*index))
                    .max_by_key(|(index, item)| (item.priority, std::cmp::Reverse(*index)))
                    .map(|(index, _)| index)?;
                // Preserve priority between lanes (including voice/settings),
                // then use dependency-safe FIFO inside ordinary data. A fresh
                // routine heartbeat must not starve an accepted sensor report
                // merely because no independent snapshot is currently queued.
                if matches!(
                    self.queue[highest].frame.bytes.get(3),
                    Some(&DOWN_DATA) | Some(&AV_BAY_DATA)
                ) && !self.queue[highest].independent_discovery()
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
            if self.queued_expired(q, now)
                || (Self::budget_kind(&q.frame.bytes)
                    && self
                        .budget
                        .as_mut()
                        .is_none_or(|b| !b.valid(&q.frame.bytes, now)))
            {
                let old = self.queue.remove(i).unwrap();
                self.bytes -= old.frame.bytes.len();
                self.retire_settings_parent(&old);
                self.dropped += 1;
                continue;
            }
            let mut queued = self.queue.remove(i).unwrap();
            if Self::budget_kind(&queued.frame.bytes) {
                queued.mark_budget_reserved();
            }
            self.active_fair_delta_bytes = if contested {
                let cost =
                    (queued.frame.bytes.len() + 4 + usize::from(self.fair_overhead_bytes)) as i32;
                // Source-paced discovery bursts reserve four ordinary bytes per
                // independent snapshot byte to keep accepted reports below 1.5 s with allowance for radio transit.
                if queued.independent_discovery() {
                    4 * cost
                } else {
                    -cost
                }
            } else {
                0
            };
            let mut tx = Transmission {
                queued,
                follower: None,
                id,
                physical: None,
                settings_action: None,
            };
            if !self.prepare_settings_physical(&mut tx, now) {
                self.queue.push_back(tx.queued);
                return None;
            }
            self.next_transmission = id;
            self.active_transmission = Some(id);
            self.active_frame_count = 1;
            self.active_recovery_control=tx.recovery_control();
            self.budget_inflight = Self::budget_kind(&tx.queued.frame.bytes);
            self.active_survives_clear = matches!(
                tx.queued.frame.bytes.get(3),
                Some(&POSITION)
                    | Some(&VOICE)
                    | Some(&SETTINGS)
                    | Some(&HOST_CONTROL)
                    | Some(&BUDGET_CONTROL)
                    | Some(&RF_PROFILE_CONTROL)
            );
            return Some(tx);
        }
    }

    #[cfg(test)]
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
        self.active_recovery_control=false;
        self.budget_inflight = false;
        self.budget_committed(&transmission.queued.frame, sent);
        let fair_delta = std::mem::take(&mut self.active_fair_delta_bytes);
        let owned_bytes = transmission.queued.frame.bytes.len()
            + transmission
                .follower
                .as_ref()
                .map_or(0, |queued| queued.frame.bytes.len());
        if !sent && self.active_cancelled {
            self.active_cancelled = false;
            self.bytes -= owned_bytes;
            self.retire_settings_parent(&transmission.queued);
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
        if let Some(action) = transmission.settings_action {
            let context = self.settings_context(self.now());
            let complete = self.settings_framing.as_mut().is_some_and(|state| {
                state.complete_physical(
                    settings_framing::OwnerId::of(&transmission.queued.frame.bytes),
                    action,
                    true,
                    context,
                )
            });
            if !complete {
                self.queue.push_back(transmission.queued);
                return None;
            }
        }
        if transmission.queued.settings_origin == SettingsOrigin::FramingOffer
            && let Some((_, body)) = decode(&transmission.queued.frame.bytes)
                && let Some(state) = self.settings_framing.as_mut() {
                    state.offer_completed(body);
                }
        self.retire_settings_parent(&transmission.queued);
        self.discovery_balance_bytes = self.discovery_balance_bytes.saturating_add(fair_delta);
        if transmission.queued.independent_discovery() {
            self.receipt_service_pending = false;
        }
        self.bytes -= owned_bytes;
        self.forwarded += count as u64;
        if self.role == Role::Rrls {
            for frame in core::iter::once(&transmission.queued.frame)
                .chain(transmission.follower.iter().map(|queued| &queued.frame))
            {
                if matches!(
                    frame.bytes.get(3),
                    Some(&DOWN_DATA)
                        | Some(&DOWN_COMMAND)
                        | Some(&AV_BAY_DATA)
                        | Some(&AV_BAY_COMMAND)
                ) {
                    self.relayed_frames += 1;
                    self.relay_envelope_bytes += frame.bytes.len() as u64;
                }
            }
        }
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
        let metadata_ready = self.queue.iter().any(|item| item.independent_discovery());
        let ordinary_ready = self.queue.iter().enumerate().any(|(index, item)| {
            item.priority < 254
                && !item.independent_discovery()
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
    /// Primary only: the RRLS sent a station-mode or command-capability frame
    /// that its worker verified (pairing signature, or the mesh session for an
    /// unpaired mode display). Topology presence only; it grants no relay,
    /// settings or command lease and sends nothing.
    pub(crate) fn note_verified_peer(&mut self, now: u64) {
        if self.role == Role::Primary {
            self.topology_peer_seen = Some(self.topology_peer_seen.map_or(now, |old| old.max(now)));
        }
    }
    /// RRLS only: the primary's HEALTH is fresh and reports its own direct
    /// link, so this station is not relaying and spare air exists for a
    /// bounded observation refresh.
    pub(crate) fn observation_refresh_allowed(&self, now: u64) -> bool {
        self.role == Role::Rrls
            && self.primary_direct
            && !self.active(now)
            && self
                .health_received
                .is_some_and(|last| now.saturating_sub(last) < LEASE_MS)
    }
    /// Link freshness only; signed proxy capability and actor policy remain independent.
    pub(crate) fn command_proxy_transport(
        &self,
        now: u64,
    ) -> Option<crate::command_proxy::Transport> {
        let session = self.operating_mode_session(now)?;
        Some(crate::command_proxy::Transport {
            session,
            sequence: if self.role == Role::Primary {
                self.sequence
            } else {
                self.primary_sequence
            },
        })
    }
    pub(crate) fn queue_command_proxy(&mut self, payload: &[u8], now: u64) -> bool {
        if payload.len() > crate::command_proxy::MAX_WIRE
            || crate::command_proxy::wire_boundary(payload).is_none()
            || self.queue.len() + usize::from(self.active_frame_count) >= MAX_FRAMES
            || self.bytes + payload.len() + 8 > MAX_BYTES
            || self
                .queue
                .iter()
                .any(|queued| queued.frame.bytes.get(3) == Some(&HOST_CONTROL))
        {
            return false;
        }
        // Independent bounded operator control, below owner/HEALTH receipts255.
        // No extra capacity, eviction, or lease/presence renewal is introduced.
        self.enqueue(
            Outgoing {
                command: true,
                bytes: encode(HOST_CONTROL, payload),
            },
            254,
            now,
        );
        true
    }
    pub(crate) fn take_command_proxy(&mut self) -> Option<Vec<u8>> {
        self.command_proxy_inbox.pop_front()
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
            .settings_origin = SettingsOrigin::Reply;
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
        let context = self.settings_context(now);
        let original = Outgoing {
            command: true,
            bytes: encode(SETTINGS, payload),
        };
        let owner = settings_framing::OwnerId::of(&original.bytes);
        let lifetime = if mode_reply || crate::station_mode::recognizes(payload) {
            5_000
        } else if crate::launch_clock_sync::recognizes(payload) {
            30_000
        } else {
            5_000
        };
        let profile = crate::comms::rfd_profile::active();
        let usable = profile.map_or(32_000, |p| p.transport.usable_air_bps);
        let default_share = profile.map_or(25, |p| p.transport.host_default_share_percent as u8);
        let share = self.recovery_air_share(now).unwrap_or(default_share);
        let local_bytes_per_second = usable * u64::from(share) / 800;
        // Conservative existing peer rates: Primary may use15%, standby RRLS25%.
        let peer_min_bytes_per_second =
            usable * if self.role == Role::Primary { 25 } else { 15 } / 800;
        // Pairing frames (at most ~120 B) always travel whole. A probe that
        // gets no OFFER costs PROBE_MS plus a 30 s legacy cooldown per step,
        // which stalled BUNDLE and DONE past the pairing window.
        let pairing = crate::station_pairing::exchange::recognizes(payload);
        if let Some(state) = self.settings_framing.as_mut().filter(|_| !pairing) {
            let own = local_bytes_per_second;
            let peer = peer_min_bytes_per_second;
            if state.admit(payload, owner, now, lifetime, context, own, peer)
                == settings_framing::Admission::Busy
            {
                return false;
            }
        }
        let before = self.queue.len();
        self.enqueue(original, 0, now);
        let admitted = self.queue.len() > before;
        if admitted {
            self.queue
                .back_mut()
                .expect("accepted SETTINGS parent")
                .settings_origin = if mode_reply {
                SettingsOrigin::Reply
            } else {
                SettingsOrigin::Settings
            };
        } else if let Some(state) = self.settings_framing.as_mut() {
            state.cancel_unadmitted(owner);
        }
        admitted
    }
    pub fn take_settings(&mut self) -> Option<Vec<u8>> {
        let index = self.settings_inbox.iter().position(|wire| {
            !crate::station_mode::recognizes(wire)
                && !crate::launch_clock_sync::recognizes(wire)
                && !crate::station_pairing::exchange::recognizes(wire)
        })?;
        self.settings_inbox.remove(index)
    }
    pub(crate) fn take_station_pairing(&mut self) -> Option<Vec<u8>> {
        let index = self
            .settings_inbox
            .iter()
            .position(|wire| crate::station_pairing::exchange::recognizes(wire))?;
        self.settings_inbox.remove(index)
    }
    /// UI pairing frames. The RRLS asks while it hears the primary; the primary
    /// answers the same way it answers mode queries, since a standby RRLS is
    /// otherwise silent. Grants no settings presence, lease or control.
    pub(crate) fn queue_station_pairing(&mut self, payload: &[u8], now: u64) -> bool {
        if !crate::station_pairing::exchange::recognizes(payload) {
            return false;
        }
        if self.role == Role::Primary {
            self.queue_operating_mode_reply(payload, now)
        } else {
            self.queue_settings(payload, now)
        }
    }
    pub(crate) fn take_launch_clock(&mut self) -> Option<Vec<u8>> {
        let index = self
            .settings_inbox
            .iter()
            .position(|wire| crate::launch_clock_sync::recognizes(wire))?;
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
    /// Voice may be sent: the link is fresh and this station's airtime share
    /// leaves room for commands and telemetry. A primary held at 15% while
    /// an RRLS relays still receives voice but does not send it.
    pub fn voice_sendable(&self, now: u64) -> bool {
        self.voice_available(now)
            && (self.role == Role::Rrls
                || self
                    .recovery_air_share(now)
                    .is_none_or(|share| share >= crate::radio_voice::MIN_SENDER_SHARE_PERCENT))
    }
    pub fn queue_voice(&mut self, payload: &[u8], now: u64) {
        if !self.voice_sendable(now) || payload.len() > crate::radio_voice::MAX_PACKET {
            return;
        }
        while self
            .queue
            .iter()
            .filter(|q| q.frame.bytes.get(3) == Some(&VOICE))
            .count()
            >= VOICE_QUEUE
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
                recovery_control: crate::telemetry_task::is_recovery_control_frame(bytes),
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
            3 => Some(FrameClass { priority: 253, recovery_control: true }),
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
                (_, BUDGET_CONTROL | RF_PROFILE_CONTROL) => {
                    if let Some(budget) = self.budget.as_mut() {
                        budget.incoming(payload, now);
                    }
                    self.poll_budget(now);
                    None
                }
                (_, settings_framing::KIND) => {
                    let context = self.settings_context(now);
                    let free = MAX_BYTES
                        .saturating_sub(self.bytes)
                        .saturating_sub(self.settings_reserved_bytes());
                    let result = self.settings_framing.as_mut().map(|state| {
                        state.incoming(data, context, self.settings_inbox.len(), free)
                    });
                    if let Some(result) = result {
                        if let Some(reply) = result.reply {
                            let live = self
                                .settings_framing
                                .as_ref()
                                .is_some_and(|state| state.offer_is_live(&reply, context));
                            if live
                                && self.enqueue_with_origin(
                                    Outgoing {
                                        command: true,
                                        bytes: settings_framing::wrap(&reply),
                                    },
                                    0,
                                    now,
                                    SettingsOrigin::FramingOffer,
                                )
                                && let Some(state) = self.settings_framing.as_mut() {
                                    state.offer_pending(&reply, true);
                                }
                        }
                        if let Some(original) = result.complete {
                            if self.settings_inbox.len() < MAX_SETTINGS_INBOX {
                                self.settings_inbox.push_back(original);
                            } else {
                                self.dropped += 1;
                            }
                        }
                    }
                    None
                }
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
                    if let Some(budget) = self.budget.as_mut() {
                        budget.health(session, seq, data[8] != 0, now);
                    }
                    if !self.active(now) {
                        self.clear();
                    }
                    None
                }
                (_, HOST_CONTROL) => {
                    // Verify signatures in the bounded dedicated worker, never in a
                    // radio callback. This metadata grants no HEALTH/settings lease.
                    if data.len() <= crate::command_proxy::MAX_WIRE
                        && self.command_proxy_inbox.len() < 4
                    {
                        self.command_proxy_inbox.push_back(data.to_vec());
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
                    if self.active(now) && !self.hears_av_bay(now) {
                        self.relay_suppressed_no_av_bay =
                            self.relay_suppressed_no_av_bay.saturating_add(1);
                    } else if self.active(now) {
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
            } else {
                if (!command && frame_class.is_some())
                    || (command && payload.starts_with(b"RS\x01") && payload.len() == 9)
                {
                    self.av_bay_heard = Some(now);
                }
            }
            if self.role == Role::Rrls && self.active(now) {
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

/// Why the primary last changed its relay request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RelayReason {
    DirectLost { gap_ms: u64 },
    DirectRestored { continuous_ms: u64 },
}
impl RelayReason {
    pub(crate) fn describe(self) -> String {
        match self {
            Self::DirectLost { gap_ms } => format!(
                "no direct AV-bay frame for {:.1} s",
                gap_ms as f64 / 1000.0
            ),
            Self::DirectRestored { continuous_ms } => format!(
                "direct AV-bay frames continuous for {:.1} s",
                continuous_ms as f64 / 1000.0
            ),
        }
    }
}

/// Relay decision diagnostics for `/api/rrls/status` and the topology page.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct RelayDecision {
    pub relay_requested: bool,
    pub changed_ago_ms: Option<u64>,
    pub reason: Option<String>,
    pub transitions: u64,
    pub direct_age_ms: Option<u64>,
    pub direct_continuous_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rrls_hears_av_bay: Option<bool>,
    pub rrls_suppressed_no_av_bay: u64,
}

/// Observed physical recovery path, separate from SEDSnet's logical routes.
#[derive(Clone)]
pub(crate) struct TopologyObservation {
    pub peer_age_ms: Option<u64>,
    pub direct_age_ms: Option<u64>,
    pub relaying: bool,
    pub command_ready: bool,
    pub relay_requested: bool,
    /// "since N s: reason" for the last relay request change, if any.
    pub relay_change: Option<String>,
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
            relay_requested: self.fallback,
            relay_change: self.relay_reason.zip(self.relay_changed_at).map(|(reason, at)| {
                format!("{} s ago: {}", now.saturating_sub(at) / 1000, reason.describe())
            }),
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
    fn host_control_is_bounded_independent_and_retains_serial_ownership() {
        let mut payload = vec![0; 132];
        payload[..4].copy_from_slice(b"CP\x01\x00");
        let mut mesh = Mesh::new(Role::Primary, 1);
        assert!(mesh.queue_command_proxy(&payload, 0));
        assert!(!mesh.queue_command_proxy(&payload, 0));
        let control = mesh.begin_transmission(0).unwrap();
        assert!(control.closes_packet_batch());
        assert!(!control.is_receipt());
        let original = control.frame().bytes.clone();
        mesh.clear();
        assert!(mesh.finish_transmission(control, false).is_none());
        let retry = mesh.begin_transmission(1).unwrap();
        assert_eq!(retry.frame().bytes, original);
        mesh.finish_transmission(retry, true);
        assert_eq!(mesh.bytes, 0);
        for _ in 0..10 {
            mesh.incoming(true, &encode(HOST_CONTROL, &payload), 2);
        }
        assert_eq!(mesh.command_proxy_inbox.len(), 4);
        assert!(mesh.take_command_proxy().is_some());
        assert!(mesh.queue_command_proxy(&payload, 3));
        assert!(!mesh.queue_command_proxy(&vec![0; 257], 3));
    }
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
    fn process_restart_uses_its_boot_clock_for_the_initial_loss_deadline() {
        if !cfg!(feature = "firmware_sim") {
            crate::mode_suite!(Hitl);
        }
        const CHILD: &str = "GS_RELAY_BOOT_CLOCK_CHILD";
        const NAME: &str =
            "rrls::tests::process_restart_uses_its_boot_clock_for_the_initial_loss_deadline";
        if std::env::var_os(CHILD).is_none() {
            let clock =
                std::env::temp_dir().join(format!("gs-relay-boot-{}.clock", std::process::id()));
            std::fs::write(&clock, "80008").unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .env("GS_SIM_CLOCK_FILE", &clock)
                .output()
                .unwrap();
            let _ = std::fs::remove_file(clock);
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let mut mesh = Mesh::new(Role::Primary, 3);
        assert_eq!(mesh.now(), 80008);
        for now in [80008, 81008, 83007] {
            mesh.tick(now);
            assert!(
                !mesh.fallback,
                "fresh process has not yet observed three seconds without a direct link"
            );
            assert_eq!(
                mesh.topology_observation(now).direct_age_ms,
                None,
                "boot clock must never fabricate observed direct traffic"
            );
        }
        mesh.tick(83008);
        assert!(
            mesh.fallback,
            "the original three-second loss threshold remains exact"
        );
        // Restoration (a5ab02b): RESTORE_MS of continuous direct frames and at
        // least RELAY_MIN_DWELL_MS in relay, both measured on the boot clock.
        let restore_at = 83008 + RELAY_MIN_DWELL_MS;
        let mut now = 83009;
        while now < restore_at {
            mesh.direct(now);
            mesh.tick(now);
            assert!(mesh.fallback, "relay keeps its minimum dwell");
            now += 500;
        }
        mesh.direct(restore_at);
        mesh.tick(restore_at - 1);
        assert!(mesh.fallback);
        mesh.tick(restore_at);
        assert!(
            !mesh.fallback,
            "restoration is exact on the process boot clock"
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
    fn recovery_liveness_uses_the_shared_virtual_clock_when_configured() {
        crate::mode_suite!(Hitl);
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
            assert_eq!(audio.try_recv().unwrap().pcm.len(), 2 * 320 * crate::radio_voice::FRAMES_PER_PACKET);
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
            mesh.queue_voice(&[0; crate::radio_voice::MAX_PACKET], now);
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
            assert!(mesh.queue.len() <= VOICE_QUEUE);
            assert!(mesh.bytes <= VOICE_QUEUE * (crate::radio_voice::MAX_PACKET + 8));
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
        let payload = vec![0; crate::radio_voice::MAX_PACKET];
        for _ in 0..100 {
            mesh.queue_voice(&payload, 0);
        }
        assert_eq!(mesh.queue.len(), VOICE_QUEUE);
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
        assert!(mesh.pop(VOICE_EXPIRY_MS + 1).is_none(), "expired voice is never replayed");
        assert!(mesh.queue.is_empty());
        mesh.queue_voice(&[0; crate::radio_voice::MAX_PACKET + 1], VOICE_EXPIRY_MS + 1);
        assert!(
            mesh.pop(VOICE_EXPIRY_MS + 1).is_none(),
            "video-sized payloads cannot enter voice"
        );
    }
    #[test]
    fn primary_held_at_fifteen_percent_receives_but_does_not_send_voice() {
        let mut mesh = Mesh::new(Role::Primary, 1);
        mesh.voice_enabled = true;
        mesh.relay_seen = Some(0);
        assert!(mesh.voice_sendable(0));
        // Relay requested: the primary's share drops to 15%.
        mesh.fallback = true;
        assert_eq!(mesh.recovery_air_share(0), Some(15));
        assert!(mesh.voice_available(0), "RRLS voice is still received");
        assert!(!mesh.voice_sendable(0));
        mesh.queue_voice(&[0; crate::radio_voice::MAX_PACKET], 0);
        assert!(mesh.queue.is_empty());
        // The RRLS always has at least 35%.
        let mut rrls = Mesh::new(Role::Rrls, 2);
        rrls.voice_enabled = true;
        rrls.health_received = Some(0);
        assert!(rrls.voice_sendable(0));
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
            let restored = lost_at + RELAY_MIN_DWELL_MS;
            for at in (lost_at + 100..restored).step_by(500) {
                primary.incoming(false, &fixtures[0], at);
                primary.relay_seen = Some(at);
                primary.tick(at);
                assert!(primary.active(at), "relay keeps its minimum dwell");
            }
            primary.incoming(false, &fixtures[0], restored);
            primary.tick(restored);
            assert!(
                !primary.active(restored),
                "stable direct recovery must exit fallback"
            );
            assert_eq!(
                decode(
                    &primary
                        .local_tx(false, b"pending", restored)
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
        assert!(relay.queue.is_empty(), "a relay that cannot hear the AV bay never repeats the uplink");
        assert_eq!(relay.relay_suppressed_no_av_bay, 1);
        relay.av_bay_heard = Some(1);
        let observed_again = relay.incoming(true, &data, 1).unwrap();
        assert_eq!(observed_again.bytes, observed.bytes);
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
        assert!(transmission.queued.independent_discovery());
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
        assert!(!tx.queued.independent_discovery());
        mesh.finish_transmission(tx, false);
        mesh.note_receipt_packet_boundary(154);
        assert_eq!(
            mesh.discovery_balance_bytes, 154,
            "failed IO cannot rearm compensation"
        );
        mesh.pop(0).unwrap();
        let tx = mesh.begin_transmission(0).unwrap();
        assert!(tx.queued.independent_discovery());
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
        assert!(tx.queued.independent_discovery());
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
            if tx.queued.independent_discovery() && !refused_once {
                mesh.finish_transmission(tx, false);
                refused_once = true;
                continue;
            }
            let age = now.saturating_sub(tx.queued.at);
            if tx.queued.independent_discovery() {
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
                // The RRLS keeps hearing the AV bay while the primary does not.
                r.av_bay_heard = Some(now);
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
    fn relay_activity_counters_exclude_presence_and_cancelled_work() {
        let mut relay = Mesh::new(Role::Rrls, 2);
        relay.enqueue(
            Outgoing {
                command: true,
                bytes: encode(READY, &[0; 8]),
            },
            255,
            0,
        );
        assert!(relay.pop(0).is_some());
        assert_eq!(relay.forwarded, 1);
        assert_eq!(relay.relayed_frames, 0);
        relay.incoming(true, &health(1, 0, false), 1);
        relay.incoming(false, b"application", 2);
        let frame = relay.pop(2).unwrap();
        assert_eq!(relay.relayed_frames, 1);
        assert_eq!(relay.relay_envelope_bytes, frame.bytes.len() as u64);
        relay.incoming(false, b"queued before restore", 3);
        relay.incoming(true, &health(1, 1, true), 4);
        assert!(relay.pop(4).is_none());
        assert_eq!(
            relay.relayed_frames, 1,
            "cancelled work is not transmitted relay traffic"
        );
        relay.incoming(false, b"standby traffic", 5);
        assert!(relay.pop(5).is_none());
        assert_eq!(relay.relayed_frames, 1);
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
        let mut now = 3_100;
        while now <= 3_100 + RELAY_MIN_DWELL_MS {
            primary.direct(now);
            primary.tick(now);
            now += 500;
        }
        assert_eq!(primary.recovery_air_share(now), None);
        relay.incoming(true, &health(1, 1, true), now);
        assert_eq!(relay.recovery_air_share(now), None);
    }

    #[test]
    fn observation_refresh_only_while_primary_reports_direct_link() {
        let mut rrls = Mesh::new(Role::Rrls, 2);
        assert!(!rrls.observation_refresh_allowed(1_000), "no primary heard");
        rrls.incoming(true, &health(1, 1, true), 1_000);
        assert!(rrls.observation_refresh_allowed(1_500));
        assert!(!rrls.observation_refresh_allowed(1_000 + LEASE_MS), "stale HEALTH");
        rrls.incoming(true, &health(1, 2, false), 2_000);
        assert!(!rrls.observation_refresh_allowed(2_100), "relaying needs the air");
        let primary = Mesh::new(Role::Primary, 1);
        assert!(!primary.observation_refresh_allowed(0));
    }

    #[test]
    fn verified_peer_exchange_shows_standby_rrls_without_radio_traffic() {
        let mut primary = Mesh::new(Role::Primary, 1);
        assert_eq!(primary.topology_observation(1_000).peer_age_ms, None);
        let queued = primary.queue.len();
        primary.note_verified_peer(1_000);
        primary.note_verified_peer(900);
        assert_eq!(primary.queue.len(), queued, "presence sends nothing");
        let seen = primary.topology_observation(5_000);
        assert_eq!(seen.peer_age_ms, Some(4_000), "a delayed note never moves presence back");
        assert!(!seen.relaying && !seen.command_ready, "presence grants no lease");
        let mut rrls = Mesh::new(Role::Rrls, 2);
        rrls.note_verified_peer(1_000);
        assert_eq!(rrls.topology_peer_seen, None);
    }

    #[test]
    fn one_hz_direct_reports_restore_without_a_false_relay_request() {
        let mut primary = Mesh::new(Role::Primary, 1);
        primary.tick(3_000);
        assert!(primary.fallback);
        // One-Hz reports count as continuous; restoration still waits for
        // both RESTORE_MS of them and the relay's minimum dwell.
        let mut now = 3_100;
        while now < 3_000 + RELAY_MIN_DWELL_MS {
            primary.direct(now);
            primary.tick(now);
            assert!(primary.fallback, "relay keeps its minimum dwell");
            now += 1_000;
        }
        primary.direct(now);
        primary.tick(now);
        assert!(!primary.fallback, "one-Hz reports keep the direct link live and restore it");
        // A real loss right after a restore waits for the direct dwell.
        let restored_at = now;
        primary.tick(now + LOSS_MS);
        assert!(!primary.fallback, "no flap back within the direct dwell");
        primary.tick(restored_at + DIRECT_MIN_DWELL_MS);
        assert!(primary.fallback, "a sustained loss still requests the relay");
        assert_eq!(primary.relay_transitions, 3);
    }

    #[test]
    fn direct_loss_gap_restarts_the_restoration_hysteresis() {
        let mut primary = Mesh::new(Role::Primary, 1);
        primary.tick(3_000);
        // Frames separated by a gap of DIRECT_GAP_MS or more restart it.
        let mut now = 3_100;
        while now < 3_000 + RELAY_MIN_DWELL_MS + RESTORE_MS {
            primary.direct(now);
            primary.tick(now);
            now += DIRECT_GAP_MS;
        }
        assert!(primary.fallback, "gapped frames never form a continuous direct path");
        let since = now;
        while now < since + RESTORE_MS {
            primary.direct(now);
            primary.tick(now);
            assert!(primary.fallback);
            now += 500;
        }
        primary.direct(now);
        primary.tick(now);
        assert!(!primary.fallback, "continuous direct frames restore");
    }

    #[test]
    fn sparse_direct_bursts_never_flap_the_relay() {
        // Live 2026-11-08: the RF board restarted its router every 2-3 s, so
        // only a discovery burst arrived every 2-3.3 s. The old rule (3 s
        // loss, 2 s restore, restoration reset only by a 3 s gap) flapped.
        let mut primary = Mesh::new(Role::Primary, 1);
        let mut changes = 0;
        let mut last = primary.fallback;
        let gaps = [2_300u64, 3_300, 2_100, 2_900, 3_350, 2_500];
        let mut now = 0;
        for i in 0..400 {
            let next = now + gaps[i % gaps.len()];
            for t in (now..next).step_by(100) {
                primary.tick(t);
                if primary.fallback != last {
                    changes += 1;
                    last = primary.fallback;
                }
            }
            now = next;
            primary.direct(now);
        }
        assert!(primary.fallback, "sparse bursts are not a direct path");
        assert_eq!(changes, 1, "exactly one request, never a flap");
        let d = primary.relay_decision(now);
        assert!(d.relay_requested && d.transitions == 1);
        assert!(d.reason.unwrap().starts_with("no direct AV-bay frame"));
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
        (true, mesh.voice_sendable(mesh.now()))
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
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: encode(DOWN_DATA, bytes),
            },
            254,
            now,
        );
    }
    #[test]
    fn receipt_can_pass_only_replayed_controls_without_changing_ownership_or_age() {
        let first = control(false, 10);
        let second = control(false, 11);
        let receipt = control(true, 12);
        let ids = [&first, &second].map(|p| {
            crate::comms::radio_batch_control_contract(254, p)
                .unwrap()
                .0
        });
        let mut mesh = Mesh::new(Role::Rrls, 2);
        enqueue(&mut mesh, &first, 10);
        enqueue(&mut mesh, &second, 11);
        enqueue(&mut mesh, &receipt, 12);
        let bytes = mesh.bytes;
        mesh.prioritize_receipt_over_replays(|id| ids.contains(&id));
        assert_eq!(mesh.bytes, bytes);
        assert_eq!(mesh.dropped, 0);
        assert_eq!(mesh.forwarded, 0);
        assert_eq!(
            mesh.queue.iter().map(|q| q.at).collect::<Vec<_>>(),
            [12, 10, 11]
        );
        let tx = mesh.begin_bounded_transmission(12, 0, true, 1).unwrap();
        assert_eq!(decode(&tx.frame().bytes).unwrap().1, receipt);
        mesh.finish_bounded_transmission(tx, false);
        assert_eq!(mesh.bytes, bytes);
        assert_eq!(mesh.queue.len(), 3);
    }
    #[test]
    fn receipt_retry_preference_preserves_fresh_controls_and_metadata_barriers() {
        let retry = control(false, 10);
        let id = crate::comms::radio_batch_control_contract(254, &retry)
            .unwrap()
            .0;
        let gps = serialize::pack_packet(
            &Packet::new(
                crate::telemetry_schema::data_type("GPS_DATA"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "RF",
                22,
                Arc::from([0u8; 12]),
            )
            .unwrap(),
        )
        .to_vec();
        for blocker in [
            encode(DOWN_DATA, &control(false, 11)),
            encode(READY, &[0; 8]),
            encode(DOWN_DATA, b"SDT\x03opaque"),
            encode(DOWN_DATA, &gps),
        ] {
            let mut mesh = Mesh::new(Role::Rrls, 2);
            enqueue(&mut mesh, &retry, 10);
            mesh.enqueue(
                Outgoing {
                    command: true,
                    bytes: blocker.clone(),
                },
                254,
                11,
            );
            enqueue(&mut mesh, &control(true, 12), 12);
            let before = mesh
                .queue
                .iter()
                .map(|q| (q.frame.bytes.clone(), q.at, q.priority))
                .collect::<Vec<_>>();
            mesh.prioritize_receipt_over_replays(|candidate| candidate == id);
            assert_eq!(
                mesh.queue
                    .iter()
                    .map(|q| (q.frame.bytes.clone(), q.at, q.priority))
                    .collect::<Vec<_>>(),
                before
            );
        }
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

#[cfg(test)]
#[path = "rrls/heartbeat_owner_relay_regression.rs"]
mod heartbeat_owner_relay_regression;

#[cfg(test)]
#[path = "rrls/ordinary_provenance_tests.rs"]
mod ordinary_provenance_tests;

#[cfg(test)]
#[path = "rrls/offer_service_regression.rs"]
mod offer_service_regression;

#[cfg(test)]
mod completed_owner_certificate_tests {
    use super::*;
    #[test]
    fn framing_offer_origin_never_certifies_a_completed_owner() {
        for kind in [SETTINGS, DOWN_DATA] {
            for action in [None, Some(settings_framing::Action::Legacy)] {
                let mut mesh = Mesh::new(Role::Primary, 1);
                mesh.enqueue(
                    Outgoing {
                        command: true,
                        bytes: encode(kind, b"owner"),
                    },
                    0,
                    0,
                );
                let mut tx = mesh.begin_transmission(0).unwrap();
                // Invalid mixed shapes must not turn the new protocol-only tag
                // into ordinary-data or legacy-reply credit.
                tx.queued.settings_origin = SettingsOrigin::FramingOffer;
                tx.settings_action = action;
                assert_eq!(tx.ordinary_background_service_bytes(), 0);
                assert_eq!(tx.completed_ordinary_owner_bytes(), 0);
            }
        }
    }
    #[test]
    fn unpaired_standby_rrls_still_receives_a_bundle_sized_pairing_reply() {
        // Live 2026-11-08: HELLO/KEY/REVEAL fit one frame, but the primary's
        // 110-byte BUNDLE went to the framing layer, which failed closed
        // because an unpaired standby RRLS sends no READY and has no
        // authenticated settings presence. Pairing frames now travel whole.
        let mut bundle = crate::station_pairing::exchange::MAGIC.to_vec();
        bundle.push(4);
        bundle.extend([7u8; 16 + 90]);
        let mut mesh = Mesh::new(Role::Primary, 1);
        mesh.enable_settings_framing();
        assert!(!mesh.settings_available(0));
        assert!(mesh.queue_station_pairing(&bundle, 0));
        let tx = mesh.begin_mode_reply_transmission(0).unwrap();
        assert_ne!(tx.settings_action, Some(settings_framing::Action::Probe));
        let (kind, body) = decode(&tx.frame().bytes).unwrap();
        assert_eq!(kind, SETTINGS);
        assert_eq!(body, bundle.as_slice());
        // Pairing link evidence is short-lived and grants no settings presence.
        mesh.note_pairing_peer(0);
        assert!(mesh.pairing_peer_fresh(PAIRING_PEER_AGE_MS - 1));
        assert!(!mesh.pairing_peer_fresh(PAIRING_PEER_AGE_MS));
        assert!(!mesh.settings_available(0));
    }

    #[test]
    fn completed_owner_negotiated_reply_has_legacy_action_only_after_probe_timeout() {
        let legacy = include_bytes!("comms/fixtures/legacy374-settings-body.bin");
        let mut mesh = Mesh::new(Role::Primary, 1);
        mesh.enable_settings_framing();
        // Real peer presence, but deliberately no negotiated OFFER.
        mesh.relay_seen = Some(0);
        assert!(mesh.queue_operating_mode_reply(&legacy[4..legacy.len() - 4], 0));
        let probe = mesh.begin_mode_reply_transmission(0).unwrap();
        assert_eq!(probe.settings_action, Some(settings_framing::Action::Probe));
        assert_eq!(probe.frame().bytes.len() + 4, 59);
        assert_eq!(probe.completed_ordinary_owner_bytes(), 0);
        mesh.note_settings_physical_prefix(&probe, 59);
        mesh.note_settings_counted_drain(&probe, None);
        mesh.finish_transmission(probe, true);
        let tx = mesh.begin_mode_reply_transmission(800).unwrap();
        assert_eq!(tx.settings_action, Some(settings_framing::Action::Legacy));
        assert_eq!(tx.queued.settings_origin, SettingsOrigin::Reply);
        assert_eq!(&tx.frame().bytes, legacy);
        assert_eq!(tx.completed_ordinary_owner_bytes(), 374);
        assert_eq!(tx.successful_ordinary_service_bytes(), 0);
    }
    #[test]
    fn completed_owner_negotiation_actions_require_trusted_physical_legacy_settings() {
        for action in [
            None,
            Some(settings_framing::Action::Legacy),
            Some(settings_framing::Action::Probe),
            Some(settings_framing::Action::Fragment(0)),
        ] {
            for kind in [SETTINGS, settings_framing::KIND, DOWN_DATA, HOST_CONTROL] {
                for trusted in [false, true] {
                    let mut mesh = Mesh::new(Role::Primary, 1);
                    mesh.enqueue(
                        Outgoing {
                            command: true,
                            bytes: encode(kind, b"owner"),
                        },
                        0,
                        0,
                    );
                    if trusted {
                        mesh.queue.back_mut().unwrap().settings_origin = SettingsOrigin::Settings;
                    }
                    let mut tx = mesh.begin_transmission(0).unwrap();
                    tx.settings_action = action;
                    let expected = match action {
                        None => kind == DOWN_DATA || (kind == SETTINGS && trusted),
                        Some(settings_framing::Action::Legacy) => kind == SETTINGS && trusted,
                        Some(
                            settings_framing::Action::Probe | settings_framing::Action::Fragment(_),
                        ) => false,
                    };
                    assert_eq!(
                        tx.completed_ordinary_owner_bytes(),
                        if expected {
                            tx.frame().bytes.len() + 4
                        } else {
                            0
                        },
                        "action={action:?} physical kind={kind} trusted={trusted}"
                    );
                    assert_eq!(
                        tx.successful_ordinary_service_bytes(),
                        if kind == DOWN_DATA {
                            tx.frame().bytes.len() + 4
                        } else {
                            0
                        },
                        "separate ordinary bank remains unchanged"
                    );
                }
            }
        }
    }
    #[test]
    fn only_ordinary_data_or_typed_whole_legacy_settings_certifies_an_owner() {
        for kind in [
            DOWN_DATA,
            UP_DATA,
            AV_BAY_DATA,
            SETTINGS,
            HOST_CONTROL,
            settings_framing::KIND,
            HEALTH,
        ] {
            let mut mesh = Mesh::new(Role::Primary, 1);
            mesh.enqueue(
                Outgoing {
                    command: true,
                    bytes: encode(kind, b"payload"),
                },
                0,
                0,
            );
            mesh.queue.back_mut().unwrap().settings_origin = SettingsOrigin::Settings;
            let tx = mesh.begin_transmission(0).unwrap();
            let expected = if matches!(kind, DOWN_DATA | UP_DATA | AV_BAY_DATA | SETTINGS) {
                tx.frame().bytes.len() + 4
            } else {
                0
            };
            assert_eq!(
                tx.completed_ordinary_owner_bytes(),
                expected,
                "physical kind{kind}"
            );
        }
        let mut mesh = Mesh::new(Role::Primary, 1);
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: encode(SETTINGS, b"opaque"),
            },
            0,
            0,
        );
        let tx = mesh.begin_transmission(0).unwrap();
        assert_eq!(
            tx.completed_ordinary_owner_bytes(),
            0,
            "untagged opaque SETTINGS cannot certify"
        );
        let mut mesh = Mesh::new(Role::Primary, 1);
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: encode(DOWN_DATA, b"urgent"),
            },
            255,
            0,
        );
        let tx = mesh.begin_transmission(0).unwrap();
        assert_eq!(
            tx.completed_ordinary_owner_bytes(),
            0,
            "urgent frame cannot certify"
        );
    }
}

#[cfg(test)]
#[path = "rrls/budget_mesh_tests.rs"]
mod budget_mesh_tests;

#[cfg(test)]
#[path = "rrls/recovery_control_tests.rs"]
mod recovery_control_tests;
