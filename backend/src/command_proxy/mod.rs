//! Primary-owned control from an authenticated recovery station.
//! Local RRLS hardware execution remains denied, including when offline.
pub(crate) mod recovery;
pub(crate) mod retained_config;
pub(crate) mod simulated_primary;
mod wire;
// Staged coordinator seam; this module does not authorize reset completion.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn begin_primary_recovery() -> anyhow::Result<Option<recovery::Barrier>> {
    recovery::begin_primary_recovery()
}
use crate::{rrls::Role, station_mode::Mode, station_pairing::Pairing};
use anyhow::{Result, ensure};
use std::collections::VecDeque;
pub(crate) use wire::Operation;
use wire::{Envelope, Message, Outcome};

const MAX_RESULTS: usize = 64;
const SEQUENCE_WINDOW: u32 = 4;
/// A reply that exactly matches the RRLS's own outstanding query or request
/// is accepted up to this many periods after it was sealed: the request's own
/// timeout. Over a hidden-node relay one GS-RRLS round trip can take 4 s.
const LATE_REPLY_WINDOW: u32 = SEQUENCE_WINDOW * 2;
/// RRLS discovery while it holds no capability: one query per step, each
/// step jittered by DISCOVERY_JITTER_PERCENT, then every 8 s for as long as
/// it lasts. The air the RRLS shares with the primary and the AV bay is busy
/// most of the time (no carrier sense in the shipped profile), so the old
/// burst of three queries in one second shared one busy period and was then
/// followed by 30 s of silence with every RRLS button disabled.
const DISCOVERY_STEPS_MS: [u64; 5] = [500, 1_000, 2_000, 4_000, 8_000];
const DISCOVERY_JITTER_PERCENT: u64 = 30;
/// A discovery query keeps its nonce this long, so the answer to an earlier
/// copy still counts; later steps seal a fresh nonce. Well inside the reply's
/// own LATE_REPLY_WINDOW.
const DISCOVERY_NONCE_MS: u64 = 3_000;
fn discovery_delay(step: u8) -> u64 {
    jittered(
        DISCOVERY_STEPS_MS[usize::from(step).min(DISCOVERY_STEPS_MS.len() - 1)],
        DISCOVERY_JITTER_PERCENT,
    )
}
/// `base` plus or minus `percent` of it, uniformly.
fn jittered(base: u64, percent: u64) -> u64 {
    let spread = base * percent / 100;
    base - spread + rand::random::<u64>() % (2 * spread + 1)
}
/// RRLS live view of the primary's control observation (fill boards
/// included), and the RRLS's only presence at the primary while idle (the
/// primary shows it Offline once its last verified exchange is 15 s old).
/// One signed QueryObserve per period; an unanswered one is sent again with
/// a fresh nonce up to REFRESH_RETRIES times, REFRESH_REPLY_MS plus 200 to
/// 700 ms apart, and the period halves while no exchange has been verified
/// for REFRESH_STALE_MS. Replies are bound to their nonce and their ages are
/// source age plus the elapsed upper bound since that query, so a lost or
/// late reply only lets rows go stale.
pub(crate) const OBSERVE_REFRESH_MS: u64 = 10_000;
const REFRESH_STALE_MS: u64 = 10_000;
const REFRESH_STALE_PERIOD_MS: u64 = 5_000;
const REFRESH_REPLY_MS: u64 = 1_500;
/// How long a queued HOST_CONTROL frame (query, request copy or the
/// primary's reply) may wait for the radio: the RRLS's own reply window.
/// Later, the RRLS has re-asked with a fresh nonce.
pub(crate) const HOST_CONTROL_REPLY_MS: u64 = REFRESH_REPLY_MS;
const REFRESH_RETRIES: u8 = 3;
fn refresh_retry_delay() -> u64 {
    REFRESH_REPLY_MS + 200 + rand::random::<u64>() % 501
}
/// Period of the observe refresh: shorter while the RRLS has not verified an
/// exchange (Capability or Result) recently.
fn observe_refresh_period(now: u64, last_exchange: Option<u64>) -> u64 {
    if last_exchange
        .and_then(|at| now.checked_sub(at))
        .is_some_and(|age| age < REFRESH_STALE_MS)
    {
        OBSERVE_REFRESH_MS
    } else {
        REFRESH_STALE_PERIOD_MS
    }
}
fn observe_refresh_due(
    now: u64,
    last: Option<u64>,
    link_idle_direct: bool,
    idle: bool,
    period: u64,
) -> bool {
    link_idle_direct
        && idle
        && last.is_none_or(|last| now.checked_sub(last).is_none_or(|age| age >= period))
}
/// An unanswered refresh copy is sent again, with a fresh nonce, after its
/// own delay; at most REFRESH_RETRIES times per refresh.
fn refresh_retry_due(now: u64, copies: u8, sent: Option<(u64, u64)>, link_idle_direct: bool, idle: bool) -> bool {
    link_idle_direct
        && idle
        && (1..=REFRESH_RETRIES).contains(&copies)
        && sent.is_some_and(|(at, delay)| now.checked_sub(at).is_none_or(|age| age >= delay))
}
/// A pending request is sent again only after its previous copy had a full
/// turn of the shared air (RRLS -> primary -> RRLS, about 400 ms), with up to
/// 150 ms of jitter from the request's own random id so copies do not lock
/// onto a periodic sender. Copies stop at REQUEST_COPIES or once the sealed
/// HEALTH sequence leaves the primary's SEQUENCE_WINDOW; freshness is unchanged.
/// A capability kept across a lost transport is restored only if the same
/// primary session returns within this long; after it, discovery runs again.
const CACHED_CAPABILITY_TTL: std::time::Duration = std::time::Duration::from_secs(30);
const REQUEST_RESEND_MS: u64 = 400;
const REQUEST_COPIES: u8 = 5;
/// What makes two copies one request: everything the RRLS signed except the
/// HEALTH sequence it sealed that copy with.
fn request_content(envelope: &wire::Envelope) -> [u8; 16] {
    wire::contract_digest(
        format!("{}|{}|{:?}|{:?}", envelope.issuer, envelope.session, envelope.boot, envelope.message).as_bytes(),
    )
}
/// Whether the RRLS sends another copy of a pending request first sealed at
/// HEALTH sequence `first`, given its current view `view`, and the sequence
/// that copy is sealed with.
///
/// Every copy is sealed afresh with the current view, so each one is as fresh
/// as the RRLS can make it when it leaves. Copies used to repeat the first
/// seal: queueing, air time and missed HEALTH aged the later copies past the
/// primary's SEQUENCE_WINDOW and the primary refused them as stale (fullnet
/// B-fix: 7 of 12 RRLS commands). The primary recognises a copy by its request
/// ID and contents (`request_content`), so a request still runs once, and each
/// copy must still be within SEQUENCE_WINDOW when it arrives: replay
/// protection is unchanged. Copies continue while a result can still arrive.
fn request_copy_sequence(first: u32, view: u32, attempts: u8) -> Option<u32> {
    let age = view.wrapping_sub(first);
    (attempts < REQUEST_COPIES && age <= LATE_REPLY_WINDOW).then_some(view)
}
fn request_resend_delay(id: &[u8; 16], attempts: u8) -> u64 {
    REQUEST_RESEND_MS + u64::from(id[usize::from(attempts) % id.len()]) % 151
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Transport {
    pub session: u32,
    pub sequence: u32,
}
impl Transport {
    fn accepts(&self, envelope: &Envelope, primary: bool) -> bool {
        if self.session != envelope.session {
            return false;
        }
        let age = self.sequence.wrapping_sub(envelope.sequence);
        if primary {
            age != 0 && age <= SEQUENCE_WINDOW
        } else {
            age <= SEQUENCE_WINDOW
        }
    }
}
fn note_rejected_frame(role: Role, reason: &str) {
    static LAST: std::sync::Mutex<Vec<(String, std::time::Instant)>> = std::sync::Mutex::new(Vec::new());
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    last.retain(|(_, at)| at.elapsed() < std::time::Duration::from_secs(10));
    if last.iter().any(|(seen, _)| seen == reason) || last.len() >= 16 {
        return;
    }
    last.push((reason.to_string(), std::time::Instant::now()));
    log::warn!("paired control frame rejected ({role:?}): {reason}");
}
struct Cached {
    request: [u8; 16],
    digest: [u8; 16],
    /// `request_content`: the same request in every copy, whatever its seal.
    content: [u8; 16],
    sequence: u32,
    reply: Option<Vec<u8>>,
}
#[derive(Clone)]
struct VerifiedRequest {
    admission: std::sync::Arc<recovery::Admission>,
    envelope: Envelope,
    digest: [u8; 16],
    pub actor: String,
    pub operation: Operation,
    plumbing_origin_utc_ms: Option<u64>,
    launch_revision: Option<u64>,
}
enum Received {
    Reply(Vec<u8>),
    Execute(VerifiedRequest),
    Capability {
        boot: [u8; 16],
        mode: Mode,
        observation: Vec<u8>,
    },
    Result {
        boot: [u8; 16],
        request: [u8; 16],
        digest: [u8; 16],
        outcome: Outcome,
        observation: Vec<u8>,
    },
    Pending,
}
struct Runtime {
    admission: std::sync::Arc<recovery::Admission>,
    role: Role,
    boot: [u8; 16],
    session: Option<u32>,
    query: Option<[u8; 16]>,
    /// RRLS: the query whose Capability it already accepted. The RRLS re-sends
    /// an unanswered query every period and the primary answers each copy, so
    /// later copies of the same answer are dropped quietly, not as rejections.
    answered_query: Option<[u8; 16]>,
    /// RRLS: the request it last sealed, until its result is accepted. Only
    /// this request's result may arrive after the normal sequence window.
    awaiting: std::cell::Cell<Option<[u8; 16]>>,
    capability: Option<([u8; 16], Mode, u64)>,
    /// RRLS: the capability verified under a primary session, kept while the
    /// transport is briefly lost (HEALTH unheard for PROXY_LEASE_MS) and
    /// restored if the same session returns. A new session means the primary
    /// restarted, and it is dropped. The primary still checks boot, mode,
    /// sequence freshness, nonce and signature on every request.
    /// Restored only within CACHED_CAPABILITY_TTL of the loss, and the worker
    /// re-verifies it with an immediate observe refresh.
    cached_capability: Option<(u32, ([u8; 16], Mode, u64), std::time::Instant)>,
    results: VecDeque<Cached>,
    observation: Vec<u8>,
    extended_observation: bool,
    readiness_demand: bool,
    readiness_seen: VecDeque<(u32, [u8; 16])>,
    readiness_last: Option<std::time::Instant>,
}
fn nonce() -> [u8; 16] {
    loop {
        let value = rand::random::<[u8; 16]>();
        if value != [0; 16] {
            return value;
        }
    }
}
impl Runtime {
    pub(crate) fn new(role: Role) -> Self {
        let admission = recovery::Admission::new();
        Self {
            boot: admission.boot(),
            admission,
            role,
            session: None,
            query: None,
            answered_query: None,
            awaiting: std::cell::Cell::new(None),
            capability: None,
            cached_capability: None,
            results: VecDeque::new(),
            observation: Vec::new(),
            extended_observation: false,
            readiness_demand: false,
            readiness_seen: VecDeque::new(),
            readiness_last: None,
        }
    }
    fn synchronize_recovery(&mut self) -> bool {
        let boot = self.admission.boot();
        if self.role != Role::Primary || self.boot == boot {
            return false;
        }
        self.boot = boot;
        self.query = None;
        self.capability = None;
        self.results.clear();
        self.extended_observation = false;
        self.readiness_demand = false;
        self.readiness_seen.clear();
        self.readiness_last = None;
        true
    }
    pub(crate) fn connect(&mut self, transport: Option<Transport>) {
        let session = transport.map(|view| view.session);
        if self.session != session {
            if self.role == Role::Primary {
                self.admission.rotate_transport();
                self.boot = self.admission.boot();
            }
            if self.role == Role::Rrls {
                if let (Some(old), Some(capability)) = (self.session, self.capability) {
                    self.cached_capability = Some((old, capability, std::time::Instant::now()));
                }
            }
            self.session = session;
            self.query = None;
            self.capability = match (self.cached_capability, session) {
                (Some((cached, capability, lost)), Some(now))
                    if cached == now && lost.elapsed() < CACHED_CAPABILITY_TTL =>
                {
                    Some(capability)
                }
                _ => None,
            };
            if session.is_some() {
                self.cached_capability = None;
            }
            self.results.clear();
            self.extended_observation = false;
            self.readiness_demand = false;
            self.readiness_seen.clear();
            self.readiness_last = None;
        }
    }
    fn peer<'a>(&self, pairing: &'a Pairing) -> Result<&'a str> {
        let role = if self.role == Role::Primary {
            "rrls"
        } else {
            "primary"
        };
        let peers: Vec<_> = pairing
            .public
            .peers
            .iter()
            .filter(|peer| peer.role == role)
            .collect();
        ensure!(
            peers.len() == 1,
            "Command proxy requires exactly one paired counterparty"
        );
        Ok(&peers[0].id)
    }
    pub(crate) fn query(&mut self, pairing: &Pairing, transport: Transport) -> Result<Vec<u8>> {
        self.query_for_readiness(pairing, transport, false)
    }
    fn query_for_readiness(
        &mut self,
        pairing: &Pairing,
        transport: Transport,
        readiness: bool,
    ) -> Result<Vec<u8>> {
        ensure!(
            self.role == Role::Rrls && self.session == Some(transport.session),
            "Only connected recovery queries primary control"
        );
        let request = *self.query.get_or_insert_with(nonce);
        wire::seal(
            pairing,
            self.peer(pairing)?,
            &Envelope {
                issuer: pairing.public.station_id.clone(),
                session: transport.session,
                sequence: transport.sequence,
                request,
                boot: [0; 16],
                message: if readiness {
                    Message::QueryReadiness
                } else {
                    Message::Query
                },
            },
        )
    }
    fn query_observe(&mut self, pairing: &Pairing, transport: Transport) -> Result<Vec<u8>> {
        ensure!(
            self.role == Role::Rrls && self.session == Some(transport.session),
            "Only connected recovery queries primary control"
        );
        let request = *self.query.get_or_insert_with(nonce);
        wire::seal(
            pairing,
            self.peer(pairing)?,
            &Envelope {
                issuer: pairing.public.station_id.clone(),
                session: transport.session,
                sequence: transport.sequence,
                request,
                boot: [0; 16],
                message: Message::QueryObserve,
            },
        )
    }
    fn take_readiness_demand(&mut self) -> bool {
        std::mem::take(&mut self.readiness_demand)
    }
    pub(crate) fn request(
        &self,
        pairing: &Pairing,
        transport: Transport,
        mode: Mode,
        request: [u8; 16],
        actor: String,
        operation: Operation,
    ) -> Result<Vec<u8>> {
        self.request_with_plumbing_origin(pairing, transport, mode, request, actor, operation, None)
    }
    #[expect(clippy::too_many_arguments, reason = "Session, actor, operation and original plumbing evidence remain independent")]
    pub(crate) fn request_with_plumbing_origin(
        &self,
        pairing: &Pairing,
        transport: Transport,
        mode: Mode,
        request: [u8; 16],
        actor: String,
        operation: Operation,
        plumbing_origin_utc_ms: Option<u64>,
    ) -> Result<Vec<u8>> {
        self.request_with_origins(
            pairing,
            transport,
            mode,
            request,
            actor,
            operation,
            plumbing_origin_utc_ms,
            None,
        )
    }
    #[expect(clippy::too_many_arguments, reason = "Session, actor, operation and original plumbing and launch evidence remain independent")]
    pub(crate) fn request_with_origins(
        &self,
        pairing: &Pairing,
        transport: Transport,
        mode: Mode,
        request: [u8; 16],
        actor: String,
        operation: Operation,
        plumbing_origin_utc_ms: Option<u64>,
        launch_origin: Option<crate::primary_mirror::LaunchOrigin>,
    ) -> Result<Vec<u8>> {
        ensure!(
            self.role == Role::Rrls && self.session == Some(transport.session),
            "Only connected recovery requests primary control"
        );
        let (boot, primary_mode, _) = self
            .capability
            .ok_or_else(|| anyhow::anyhow!("Primary capability is not verified"))?;
        ensure!(primary_mode == mode, "Primary mode changed");
        if matches!(&operation,Operation::Command(name) if wire::launch_command(name)) {
            let origin=launch_origin.ok_or_else(||anyhow::anyhow!("Original primary launch proof is missing; check readiness and make a new explicit request"))?;
            ensure!(
                origin.boot == boot && origin.session == transport.session,
                "Primary boot/session changed after launch ingress"
            );
        } else {
            ensure!(launch_origin.is_none(), "Unexpected launch origin");
        }
        self.awaiting.set(Some(request));
        wire::seal(
            pairing,
            self.peer(pairing)?,
            &Envelope {
                issuer: pairing.public.station_id.clone(),
                session: transport.session,
                sequence: transport.sequence,
                request,
                boot,
                message: Message::Request {
                    launch_revision: launch_origin.map(|origin| origin.revision),
                    mode,
                    actor,
                    operation,
                    plumbing_origin_utc_ms,
                },
            },
        )
    }
    pub(crate) fn receive(
        &mut self,
        pairing: &Pairing,
        transport: Transport,
        local_mode: Mode,
        bytes: &[u8],
    ) -> Result<Received> {
        self.synchronize_recovery();
        ensure!(
            self.session == Some(transport.session),
            "Disconnected control transport"
        );
        let primary = self.role == Role::Primary;
        let envelope = wire::open(pairing, bytes, if primary { "rrls" } else { "primary" })?;
        // Over a hidden-node relay the RRLS can hear a reply after its view
        // moved past the window. It still takes one that exactly matches its
        // own outstanding query or request, until that exchange's timeout.
        // The primary's request freshness is unchanged.
        let own_late_reply = !primary
            && transport.session == envelope.session
            && transport.sequence.wrapping_sub(envelope.sequence) <= LATE_REPLY_WINDOW
            && match envelope.message {
                Message::Capability { .. } => self.query == Some(envelope.request),
                Message::Reply { .. } => self.awaiting.get() == Some(envelope.request),
                _ => false,
            };
        if !primary
            && transport.session == envelope.session
            && matches!(envelope.message, Message::Capability { .. })
            && self.query.is_none()
            && self.answered_query == Some(envelope.request)
        {
            return Ok(Received::Pending);
        }
        // On-air diagnosis of request freshness: the sealed HEALTH sequence
        // against this station's current one (age 1..=SEQUENCE_WINDOW accepted).
        log::debug!(
            "paired control frame ({}) session {} sequence {} vs current {} age {}",
            if primary { "primary" } else { "rrls" },
            envelope.session,
            envelope.sequence,
            transport.sequence,
            transport.sequence.wrapping_sub(envelope.sequence)
        );
        ensure!(
            transport.accepts(&envelope, primary) || own_late_reply,
            "Stale primary connection contract (sequence {} vs current {}, session {})",
            envelope.sequence,
            transport.sequence,
            envelope.session == transport.session
        );
        match envelope.message.clone() {
            Message::Query | Message::QueryObserve | Message::QueryReadiness => {
                ensure!(
                    primary && envelope.boot == [0; 16],
                    "Unexpected control query"
                );
                // A legacy query explicitly selects legacy replies too, even
                // after a peer restart that retained the same transport session.
                self.extended_observation = !matches!(envelope.message, Message::Query);
                if matches!(envelope.message, Message::QueryReadiness) {
                    let identity = (envelope.session, envelope.request);
                    let unseen = !self.readiness_seen.contains(&identity);
                    if unseen {
                        if self.readiness_seen.len() >= 16 {
                            self.readiness_seen.pop_front();
                        }
                        self.readiness_seen.push_back(identity);
                        if self
                            .readiness_last
                            .is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(5))
                        {
                            self.readiness_last = Some(std::time::Instant::now());
                            self.readiness_demand = true;
                        }
                    }
                }
                let reply = Envelope {
                    issuer: pairing.public.station_id.clone(),
                    boot: self.boot,
                    message: Message::Capability {
                        mode: local_mode,
                        commands: supported_commands_mask_for_peer(self.extended_observation),
                        observation: if matches!(envelope.message, Message::Query) {
                            crate::primary_mirror::legacy_observation(&self.observation)
                        } else {
                            self.observation.clone()
                        },
                    },
                    ..envelope.clone()
                };
                Ok(Received::Reply(wire::seal(
                    pairing,
                    &envelope.issuer,
                    &reply,
                )?))
            }
            Message::Capability {
                mode,
                commands,
                observation,
            } => {
                ensure!(
                    !primary && self.query == Some(envelope.request) && envelope.boot != [0; 16],
                    "Unsolicited primary capability"
                );
                self.query = None;
                self.answered_query = Some(envelope.request);
                self.capability = Some((envelope.boot, mode, commands));
                Ok(Received::Capability {
                    boot: envelope.boot,
                    mode,
                    observation,
                })
            }
            Message::Request {
                launch_revision,
                mode,
                actor,
                operation,
                plumbing_origin_utc_ms,
            } => {
                let manual_stop = matches!(&operation, Operation::Command(name) if name == "Abort");
                // A signed, fresh Abort from the paired RRLS is never refused for a
                // renewed boot or a mode change (a Reset all renews the boot while
                // the RRLS still holds the old one): re-applying a stop is safe.
                ensure!(
                    primary && (manual_stop || (envelope.boot == self.boot && mode == local_mode)),
                    "Wrong primary boot or mode"
                );
                ensure!(
                    self.admission.request_allowed(envelope.boot, manual_stop),
                    "Primary recovery is pending"
                );
                let digest = wire::contract_digest(bytes);
                let content = request_content(&envelope);
                if let Some(previous) = self
                    .results
                    .iter()
                    .find(|item| item.request == envelope.request)
                {
                    // A copy sealed afresh (request_copy_sequence) is the same
                    // request: the cached result, never a second execution.
                    ensure!(
                        previous.content == content,
                        "Request ID reused with different contents"
                    );
                    return Ok(previous
                        .reply
                        .as_ref()
                        .map_or(Received::Pending, |reply| Received::Reply(reply.clone())));
                }
                self.results.retain(|item| {
                    transport.sequence.wrapping_sub(item.sequence) <= SEQUENCE_WINDOW * 2
                });
                ensure!(
                    self.results.len() < MAX_RESULTS,
                    "Primary request cache is busy"
                );
                self.results.push_back(Cached {
                    request: envelope.request,
                    digest,
                    content,
                    sequence: envelope.sequence,
                    reply: None,
                });
                Ok(Received::Execute(VerifiedRequest {
                    admission: self.admission.clone(),
                    envelope,
                    digest,
                    actor,
                    operation,
                    plumbing_origin_utc_ms,
                    launch_revision,
                }))
            }
            Message::Reply {
                digest,
                outcome,
                observation,
            } => {
                ensure!(
                    !primary
                        && self
                            .capability
                            .is_some_and(|(boot, _, _)| boot == envelope.boot),
                    "Wrong primary result boot"
                );
                if self.awaiting.get() == Some(envelope.request) {
                    self.awaiting.set(None);
                } else {
                    ensure!(!own_late_reply, "Stale primary connection contract");
                }
                Ok(Received::Result {
                    boot: envelope.boot,
                    request: envelope.request,
                    digest,
                    outcome,
                    observation,
                })
            }
        }
    }
    pub(crate) fn finish(
        &mut self,
        pairing: &Pairing,
        request: VerifiedRequest,
        outcome: Outcome,
    ) -> Result<Vec<u8>> {
        ensure!(
            self.role == Role::Primary,
            "Only primary completes a request"
        );
        self.synchronize_recovery();
        let manual_stop = matches!(&request.operation, Operation::Command(name) if name == "Abort");
        ensure!(
            request.envelope.boot == self.boot || manual_stop,
            "Proxy capability was revoked"
        );
        let reply = Envelope {
            issuer: pairing.public.station_id.clone(),
            message: Message::Reply {
                digest: request.digest,
                outcome,
                observation: if self.extended_observation {
                    self.observation.clone()
                } else {
                    crate::primary_mirror::legacy_observation(&self.observation)
                },
            },
            ..request.envelope.clone()
        };
        let bytes = wire::seal(pairing, &request.envelope.issuer, &reply)?;
        let entry = self
            .results
            .iter_mut()
            .find(|item| item.request == request.envelope.request && item.digest == request.digest)
            .ok_or_else(|| anyhow::anyhow!("Expired request owner"))?;
        entry.reply = Some(bytes.clone());
        Ok(bytes)
    }
}

#[cfg(test)]
mod observe_refresh_tests {
    use super::*;
    #[test]
    fn refresh_is_bounded_and_only_when_idle_on_a_direct_link() {
        let p = OBSERVE_REFRESH_MS;
        assert!(observe_refresh_due(0, None, true, true, p));
        assert!(!observe_refresh_due(0, None, false, true, p));
        assert!(!observe_refresh_due(0, None, true, false, p));
        assert!(!observe_refresh_due(OBSERVE_REFRESH_MS - 1, Some(0), true, true, p));
        assert!(observe_refresh_due(OBSERVE_REFRESH_MS, Some(0), true, true, p));
        // A clock that moved backwards restarts the period instead of stalling.
        assert!(observe_refresh_due(5, Some(10), true, true, p));
        // Worst case: no reply is ever heard, so one copy at most every
        // REFRESH_REPLY_MS + 200 ms (retries, then the shortened period),
        // each answered by a full reply. Counting both as MAX_WIRE frames with
        // mesh framing (actual: 132 B query, 238 B reply, about 240 B/s), it
        // stays under a tenth of the shipped profile's 30 kbit/s usable air.
        // An RRLS that hears replies sends one query per 10 s.
        let worst_bytes_per_s = 2 * (MAX_WIRE as u64 + 16) * 1000 / (REFRESH_REPLY_MS + 200);
        assert!(worst_bytes_per_s <= 30_000 / 8 / 10, "{worst_bytes_per_s} B/s");
    }
    /// Bench 2026-11-11 (rrls-txgap-ROOTCAUSE.md): the idle refresh was one
    /// unacknowledged frame every 10 s, and 79% of them were lost on the
    /// shared air, so the primary showed the RRLS Offline for most of the run.
    #[test]
    fn an_unanswered_refresh_is_retried_with_jitter_then_the_period_shortens() {
        // Retries: copies 1..=3 may be followed by another, after its delay.
        let delay = refresh_retry_delay();
        assert!((REFRESH_REPLY_MS + 200..=REFRESH_REPLY_MS + 700).contains(&delay));
        assert!(!refresh_retry_due(1_000 + delay - 1, 1, Some((1_000, delay)), true, true));
        assert!(refresh_retry_due(1_000 + delay, 1, Some((1_000, delay)), true, true));
        assert!(refresh_retry_due(1_000 + delay, 3, Some((1_000, delay)), true, true));
        assert!(!refresh_retry_due(1_000 + delay, 4, Some((1_000, delay)), true, true), "three retries");
        assert!(!refresh_retry_due(1_000 + delay, 0, Some((1_000, delay)), true, true), "answered");
        assert!(!refresh_retry_due(1_000 + delay, 1, Some((1_000, delay)), false, true), "relaying");
        assert!(!refresh_retry_due(1_000 + delay, 1, Some((1_000, delay)), true, false), "busy");
        // Period: 10 s while an exchange was verified in the last 10 s, else 5 s.
        assert_eq!(observe_refresh_period(20_000, Some(15_000)), OBSERVE_REFRESH_MS);
        assert_eq!(observe_refresh_period(20_000, Some(10_000)), REFRESH_STALE_PERIOD_MS);
        assert_eq!(observe_refresh_period(20_000, None), REFRESH_STALE_PERIOD_MS);
        assert_eq!(observe_refresh_period(20_000, Some(30_000)), REFRESH_STALE_PERIOD_MS);
        // With the reply window, a stale RRLS still sends within the primary's
        // 15 s Offline threshold several times over.
        assert!(REFRESH_STALE_PERIOD_MS + REFRESH_REPLY_MS + 700 < 15_000 / 2);
    }
    #[test]
    fn discovery_is_spread_and_never_silent_for_long() {
        let mut previous = 0;
        for step in 0..20u8 {
            let base = DISCOVERY_STEPS_MS[usize::from(step).min(4)];
            for _ in 0..50 {
                let delay = discovery_delay(step);
                assert!(delay >= base * 7 / 10 && delay <= base * 13 / 10, "step {step}: {delay}");
                assert!(delay <= 8_000 * 13 / 10, "never silent past 8 s + 30%");
            }
            assert!(base >= previous, "backs off");
            previous = base;
        }
        // The first five steps spread over about 15 s, not one second.
        assert_eq!(DISCOVERY_STEPS_MS.iter().sum::<u64>(), 15_500);
        // Spread, not fixed: one of 200 delays differs from the base.
        assert!((0..200).any(|_| discovery_delay(2) != 2_000));
    }
    #[test]
    fn request_copies_wait_a_turn_and_stay_inside_the_freshness_window() {
        for id in [[0u8; 16], [255; 16], [150; 16], [7; 16]] {
            for attempts in 0..REQUEST_COPIES {
                let delay = request_resend_delay(&id, attempts);
                assert!((REQUEST_RESEND_MS..=REQUEST_RESEND_MS + 150).contains(&delay));
            }
        }
        // All copies fit in the 4-period window (HEALTH every 500 ms).
        assert!(u64::from(REQUEST_COPIES - 1) * REQUEST_RESEND_MS <= u64::from(SEQUENCE_WINDOW) * 500);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn fixtures() -> (
        crate::recording_export::tests::TestDirectory,
        Pairing,
        Pairing,
    ) {
        let root = crate::recording_export::tests::TestDirectory::new();
        std::fs::write(root.0.join("key"), [42u8; 32]).unwrap();
        let load = |id: &str, peer: &str, role: &str| {
            let path = root.0.join(format!("{id}.json"));
            std::fs::write(&path,serde_json::to_vec(&serde_json::json!({"version":1,"installation_id":"pad","station_id":id,"signing_seed_file":"key","peers":[{"id":peer,"role":role,"urls":[]}]})).unwrap()).unwrap();
            Pairing::from_file(&path).unwrap()
        };
        let primary = load("primary", "rrls", "rrls");
        let recovery = load("rrls", "primary", "primary");
        (root, primary, recovery)
    }
    pub(super) fn connected() -> (
        crate::recording_export::tests::TestDirectory,
        Pairing,
        Pairing,
        Runtime,
        Runtime,
        Transport,
        Transport,
    ) {
        let (root, p, r) = fixtures();
        let mut primary = Runtime::new(Role::Primary);
        let mut rr = Runtime::new(Role::Rrls);
        let pview = Transport {
            session: 7,
            sequence: 21,
        };
        let rview = Transport {
            session: 7,
            sequence: 20,
        };
        primary.connect(Some(pview));
        rr.connect(Some(rview));
        let query = rr.query(&r, rview).unwrap();
        let Received::Reply(reply) = primary.receive(&p, pview, Mode::Hitl, &query).unwrap() else {
            panic!("capability reply")
        };
        let Received::Capability { boot, mode, .. } =
            rr.receive(&r, rview, Mode::Normal, &reply).unwrap()
        else {
            panic!("capability")
        };
        assert_ne!(boot, [0; 16]);
        assert_eq!(mode, Mode::Hitl);
        (root, p, r, primary, rr, pview, rview)
    }
    /// During a relay the RRLS hears the primary's HEALTH through hidden-node
    /// collisions (about 31% heard in the faithful sim), so its view of the
    /// primary's sequence lags several HEALTH periods. Sealed on the RRLS's
    /// extrapolated view (rrls command_proxy_transport), a query and its reply
    /// are accepted without widening the freshness window.
    /// pyroH2: over the hidden-node relay the primary's capability reply
    /// reached the RRLS after its view had moved past SEQUENCE_WINDOW, and was
    /// refused as stale, so RRLS commands stayed 503. A reply that exactly
    /// matches the RRLS's own outstanding query is accepted until that query's
    /// timeout; nothing else is.
    #[test]
    fn a_delayed_reply_to_the_rrls_own_query_is_accepted_until_its_timeout() {
        let (_root, p, r) = fixtures();
        let mut primary = Runtime::new(Role::Primary);
        let mut rr = Runtime::new(Role::Rrls);
        let sealed = Transport { session: 7, sequence: 20 };
        primary.connect(Some(Transport { session: 7, sequence: 21 }));
        rr.connect(Some(sealed));
        let query = rr.query(&r, sealed).unwrap();
        let Received::Reply(reply) = primary.receive(&p, Transport { session: 7, sequence: 21 }, Mode::Hitl, &query).unwrap() else {
            panic!("capability reply")
        };
        // Past the late bound the reply is refused even though it matches.
        let too_late = Transport { session: 7, sequence: 20 + LATE_REPLY_WINDOW + 1 };
        assert!(rr.receive(&r, too_late, Mode::Normal, &reply).is_err());
        // Beyond the 4-period window but within the query's timeout: accepted.
        let later = Transport { session: 7, sequence: 20 + SEQUENCE_WINDOW + 2 };
        assert!(matches!(rr.receive(&r, later, Mode::Normal, &reply).unwrap(), Received::Capability { .. }));
        // Replayed once the query is answered: dropped without effect (it
        // refreshes nothing; the RRLS's repeated query draws such copies).
        assert!(matches!(rr.receive(&r, later, Mode::Normal, &reply).unwrap(), Received::Pending));
    }
    #[test]
    fn a_late_reply_that_is_not_the_rrls_own_outstanding_exchange_is_rejected() {
        let (_root, p, r, mut primary, mut rr, pview, rview) = connected();
        // Another query's reply (a different nonce), arriving late.
        let mut other = Runtime::new(Role::Rrls);
        other.connect(Some(rview));
        let foreign = other.query(&r, rview).unwrap();
        let Received::Reply(foreign_reply) = primary.receive(&p, pview, Mode::Hitl, &foreign).unwrap() else {
            panic!("capability reply")
        };
        let _ = rr.query(&r, rview).unwrap();
        let late = Transport { sequence: rview.sequence + SEQUENCE_WINDOW + 2, ..rview };
        assert!(rr.receive(&r, late, Mode::Normal, &foreign_reply).is_err(), "unmatched nonce");
        // A command result for this RRLS's own request, delayed past the window:
        // accepted once; a replay, or a result for another request, is not.
        rr.query = None;
        let request = rr.request(&r, rview, Mode::Hitl, [5; 16], "operator".into(), Operation::Command("Pilot".into())).unwrap();
        let Received::Execute(work) = primary.receive(&p, pview, Mode::Hitl, &request).unwrap() else { panic!("execute") };
        let result = primary.finish(&p, work, Outcome::Queued).unwrap();
        assert!(matches!(rr.receive(&r, late, Mode::Normal, &result).unwrap(), Received::Result { .. }));
        assert!(rr.receive(&r, late, Mode::Normal, &result).is_err(), "replayed result");
        let _ = rr.request(&r, rview, Mode::Hitl, [6; 16], "operator".into(), Operation::Command("Pilot".into())).unwrap();
        assert!(rr.receive(&r, late, Mode::Normal, &result).is_err(), "result for another request");
        // Within the normal window replies keep their existing behaviour.
        assert!(matches!(rr.receive(&r, rview, Mode::Normal, &result).unwrap(), Received::Result { .. }));
    }
    /// Bench 2026-11-17 (relay mode): repeated underglow toggles from the RRLS
    /// reached the boards, yet each ended "outcome unknown ... Primary
    /// response was not confirmed". The primary queued its signed result as a
    /// HOST_CONTROL frame, wrote that lane only once no SDK control was queued
    /// (1.4-3 s in the fullnet), and the frame expired after 250 ms, so the
    /// RRLS never heard the result inside its LATE_REPLY_WINDOW. Through both
    /// meshes: the result waits out a busy lane and still confirms.
    #[test]
    fn a_relayed_command_result_survives_a_busy_primary_lane_and_confirms() {
        use crate::rrls::{Mesh, Role as MeshRole};
        let (_root, p, r, mut primary, mut rr, pview, rview) = connected();
        let mut primary_mesh = Mesh::new(MeshRole::Primary, 1);
        let mut rrls_mesh = Mesh::new(MeshRole::Rrls, 2);
        rr.query = None;
        let request = rr
            .request(&r, rview, Mode::Hitl, [9; 16], "operator".into(), Operation::Command("ToggleAvBayUnderglow".into()))
            .unwrap();
        // RRLS -> primary.
        assert!(rrls_mesh.queue_command_proxy(&request, 0));
        let tx = rrls_mesh.begin_transmission(0).unwrap();
        let wire = tx.frame().bytes.clone();
        rrls_mesh.finish_transmission(tx, true);
        primary_mesh.incoming(true, &wire, 100);
        let bytes = primary_mesh.take_command_proxy().unwrap();
        let Received::Execute(work) = primary.receive(&p, pview, Mode::Hitl, &bytes).unwrap() else {
            panic!("execute")
        };
        let result = primary.finish(&p, work, Outcome::Queued).unwrap();
        // The primary queues its result; SDK receipts hold its lane for 1 s.
        assert!(primary_mesh.queue_command_proxy(&result, 200));
        let tx = primary_mesh
            .begin_transmission(1_200)
            .expect("the result is still owed after a 1 s busy lane");
        let wire = tx.frame().bytes.clone();
        primary_mesh.finish_transmission(tx, true);
        // Primary -> RRLS: the RRLS gets its signed result.
        rrls_mesh.incoming(true, &wire, 1_300);
        let bytes = rrls_mesh.take_command_proxy().unwrap();
        let late = Transport { sequence: rview.sequence + 3, ..rview };
        assert!(matches!(
            rr.receive(&r, late, Mode::Normal, &bytes).unwrap(),
            Received::Result { outcome: Outcome::Queued, .. }
        ));
    }
    /// Bench 2026-11-10: the RRLS re-sent its unanswered query each period and
    /// the primary answered every copy; each later copy of the accepted answer
    /// was logged as "Unsolicited primary capability".
    #[test]
    fn a_repeated_answer_to_an_accepted_query_is_dropped_quietly() {
        let (_root, p, r) = fixtures();
        let mut primary = Runtime::new(Role::Primary);
        let mut rr = Runtime::new(Role::Rrls);
        let rview = Transport { session: 7, sequence: 20 };
        let pview = Transport { session: 7, sequence: 21 };
        primary.connect(Some(pview));
        rr.connect(Some(rview));
        let first = rr.query(&r, rview).unwrap();
        let again = rr.query(&r, Transport { session: 7, sequence: 21 }).unwrap();
        let Received::Reply(reply) = primary.receive(&p, pview, Mode::Hitl, &first).unwrap() else {
            panic!("capability reply")
        };
        let Received::Reply(copy) = primary.receive(&p, Transport { session: 7, sequence: 22 }, Mode::Hitl, &again).unwrap() else {
            panic!("second capability reply")
        };
        assert!(matches!(rr.receive(&r, rview, Mode::Normal, &reply).unwrap(), Received::Capability { .. }));
        // Arriving after the sequence window, too.
        let later = Transport { session: 7, sequence: 40 };
        assert!(matches!(rr.receive(&r, later, Mode::Normal, &copy).unwrap(), Received::Pending));
        assert!(matches!(rr.receive(&r, later, Mode::Normal, &reply).unwrap(), Received::Pending));
        // A new query is still answered normally.
        let next = rr.query(&r, later).unwrap();
        let Received::Reply(fresh) = primary.receive(&p, Transport { session: 7, sequence: 41 }, Mode::Hitl, &next).unwrap() else {
            panic!("fresh capability reply")
        };
        assert!(matches!(rr.receive(&r, later, Mode::Normal, &fresh).unwrap(), Received::Capability { .. }));
    }
    #[test]
    fn capability_exchange_survives_a_lagging_health_view() {
        let (_root, p, r) = fixtures();
        let mut primary = Runtime::new(Role::Primary);
        let mut rr = Runtime::new(Role::Rrls);
        // Last HEALTH heard was 20, 4 s ago: the RRLS estimates 26 (650 ms
        // periods) while the primary has actually reached 28.
        let rview = Transport { session: 7, sequence: 26 };
        let pview = Transport { session: 7, sequence: 28 };
        primary.connect(Some(pview));
        rr.connect(Some(rview));
        let query = rr.query(&r, rview).unwrap();
        let Received::Reply(reply) = primary.receive(&p, pview, Mode::Hitl, &query).unwrap() else {
            panic!("capability reply")
        };
        assert!(matches!(rr.receive(&r, rview, Mode::Normal, &reply).unwrap(), Received::Capability { .. }));
        // Freshness is unchanged: the unextrapolated view (8 periods behind)
        // is still refused by the primary.
        let stale = rr.query(&r, Transport { session: 7, sequence: 20 }).unwrap();
        assert!(primary.receive(&p, pview, Mode::Hitl, &stale).is_err());
    }
    #[test]
    fn signed_control_roundtrip_uses_primary_build_and_replays_exact_result_without_execution() {
        let (_root, p, r, mut primary, mut rr, pview, rview) = connected();
        let request = rr
            .request(
                &r,
                rview,
                Mode::Hitl,
                [2; 16],
                "operator".into(),
                Operation::Command("DeployParachute".into()),
            )
            .unwrap();
        if !crate::runtime_mode::is_hitl() {
            assert!(
                serde_json::from_str::<crate::types::TelemetryCommand>("\"DeployParachute\"").is_err(),
                "normal RRLS must not need a local HITL command variant"
            );
        }
        let Received::Execute(work) = primary.receive(&p, pview, Mode::Hitl, &request).unwrap()
        else {
            panic!("one execution")
        };
        assert!(matches!(work.operation,Operation::Command(ref name) if name=="DeployParachute"));
        assert!(matches!(
            primary.receive(&p, pview, Mode::Hitl, &request).unwrap(),
            Received::Pending
        ));
        let reply = primary.finish(&p, work, Outcome::Queued).unwrap();
        let Received::Result {
            request: id,
            digest,
            outcome,
            ..
        } = rr.receive(&r, rview, Mode::Normal, &reply).unwrap()
        else {
            panic!("result")
        };
        assert_eq!(id, [2; 16]);
        assert_eq!(digest, wire::contract_digest(&request));
        assert_eq!(outcome, Outcome::Queued);
        let Received::Reply(replayed) = primary.receive(&p, pview, Mode::Hitl, &request).unwrap()
        else {
            panic!("replay no execution")
        };
        assert_eq!(replayed, reply);
    }
    #[test]
    fn stale_session_mode_reconnect_and_changed_id_contract_are_rejected() {
        let (_root, p, r, mut primary, rr, pview, rview) = connected();
        let request = rr
            .request(
                &r,
                rview,
                Mode::Hitl,
                [3; 16],
                "operator".into(),
                Operation::Command("Pilot".into()),
            )
            .unwrap();
        assert!(primary.receive(&p, pview, Mode::Normal, &request).is_err());
        let _ = primary.receive(&p, pview, Mode::Hitl, &request).unwrap();
        let changed = rr
            .request(
                &r,
                rview,
                Mode::Hitl,
                [3; 16],
                "operator".into(),
                Operation::Command("Abort".into()),
            )
            .unwrap();
        assert!(primary.receive(&p, pview, Mode::Hitl, &changed).is_err());
        assert!(
            primary
                .receive(
                    &p,
                    Transport {
                        sequence: 25,
                        ..pview
                    },
                    Mode::Hitl,
                    &request
                )
                .is_err()
        );
        primary.connect(None);
        primary.connect(Some(pview));
        assert!(
            primary.receive(&p, pview, Mode::Hitl, &request).is_err(),
            "same32-bit session after reconnect cannot reuse old boot proof"
        );
    }
    #[test]
    fn result_cache_is_bounded_and_observations_are_exact_signed_snapshots() {
        assert_eq!(elapsed_upper_bound(100, 101), u64::MAX);
        assert_eq!(elapsed_upper_bound(100, 90), 10);
        let (_root, p, r, mut primary, mut rr, pview, rview) = connected();
        primary.observation = vec![7; 96];
        rr.capability = None;
        let query = rr.query(&r, rview).unwrap();
        let Received::Reply(reply) = primary.receive(&p, pview, Mode::Hitl, &query).unwrap() else {
            panic!("capability")
        };
        assert_eq!(reply.len(), 238);
        let Received::Capability { observation, .. } =
            rr.receive(&r, rview, Mode::Normal, &reply).unwrap()
        else {
            panic!("capability")
        };
        assert_eq!(observation, vec![7; 96]);
        let mut first = None;
        for id in 1..=MAX_RESULTS + 1 {
            let request = rr
                .request(
                    &r,
                    rview,
                    Mode::Hitl,
                    [id as u8; 16],
                    "operator".into(),
                    Operation::Command("Pilot".into()),
                )
                .unwrap();
            let received = primary.receive(&p, pview, Mode::Hitl, &request);
            if id > MAX_RESULTS {
                assert!(received.is_err());
                break;
            }
            let Received::Execute(work) = received.unwrap() else {
                panic!("execute")
            };
            let reply = primary.finish(&p, work, Outcome::Queued).unwrap();
            assert_eq!(reply.len(), 246);
            if id == 1 {
                first = Some((request, reply));
            }
        }
        assert_eq!(primary.results.len(), MAX_RESULTS);
        primary.observation = vec![9; 96];
        let (request, original) = first.unwrap();
        let Received::Reply(replay) = primary.receive(&p, pview, Mode::Hitl, &request).unwrap()
        else {
            panic!("cached")
        };
        assert_eq!(
            replay, original,
            "replay must never refresh cached observation age"
        );
        primary.observation = vec![0; 97];
        let query = rr.query(&r, rview).unwrap();
        assert!(primary.receive(&p, pview, Mode::Hitl, &query).is_err());
    }
    #[test]
    fn capability_recovery_uses_a_fresh_nonce_per_discovery_round() {
        let (_root, p, r, mut primary, mut rr, pview, rview) = connected();
        let old = rr.query(&r, rview).unwrap();
        rr.query = None; // The worker changes only this nonce after cooldown.
        let new = rr.query(&r, rview).unwrap();
        assert_ne!(old, new);
        let Received::Reply(old_reply) = primary.receive(&p, pview, Mode::Hitl, &old).unwrap()
        else {
            panic!("old query")
        };
        assert!(rr.receive(&r, rview, Mode::Normal, &old_reply).is_err());
        let Received::Reply(new_reply) = primary.receive(&p, pview, Mode::Hitl, &new).unwrap()
        else {
            panic!("new query")
        };
        assert!(matches!(
            rr.receive(&r, rview, Mode::Normal, &new_reply).unwrap(),
            Received::Capability { .. }
        ));
    }
    /// Bench 2026-11-11 15:30: one lost request or reply wiped the RRLS's
    /// capability ("Primary response was not confirmed"), and every later
    /// request was refused until discovery succeeded again on a full channel.
    /// The capability survives an unanswered request and a brief transport
    /// loss in the same primary session; the primary's checks are unchanged.
    #[test]
    fn a_valid_capability_survives_lost_air_and_only_the_primary_retires_it() {
        let (_root, p, r, mut primary, mut rr, pview, rview) = connected();
        let held = rr.capability.expect("capability");
        // Request sealed and lost on the air: nothing came back.
        let _lost = rr
            .request(&r, rview, Mode::Hitl, [8; 16], "operator".into(), Operation::Command("Pilot".into()))
            .unwrap();
        // HEALTH unheard past PROXY_LEASE_MS, then heard again: same session.
        rr.connect(None);
        assert!(rr.capability.is_none(), "no capability while disconnected");
        rr.connect(Some(rview));
        assert_eq!(rr.capability, Some(held), "same primary session keeps it");
        let next = rr
            .request(&r, rview, Mode::Hitl, [9; 16], "operator".into(), Operation::Command("Pilot".into()))
            .unwrap();
        assert!(matches!(primary.receive(&p, pview, Mode::Hitl, &next).unwrap(), Received::Execute(_)));
        // Freshness is still enforced on the restored capability.
        let stale = rr
            .request(&r, Transport { sequence: 10, ..rview }, Mode::Hitl, [10; 16], "operator".into(), Operation::Command("Pilot".into()))
            .unwrap();
        assert!(primary.receive(&p, pview, Mode::Hitl, &stale).is_err());
        // A new primary session (the primary restarted) never inherits it.
        rr.connect(None);
        rr.connect(Some(Transport { session: 8, ..rview }));
        assert!(rr.capability.is_none());
        rr.connect(Some(rview));
        assert!(rr.capability.is_none(), "dropped once another session was seen");
        // Past its time-to-live a kept capability is not restored, even for
        // the same session.
        let (_root, _p, _r, _primary, mut rr, _pview, rview) = connected();
        rr.connect(None);
        let (session, capability, lost) = rr.cached_capability.unwrap();
        rr.cached_capability = Some((session, capability, lost - CACHED_CAPABILITY_TTL));
        rr.connect(Some(rview));
        assert!(rr.capability.is_none(), "expired");
        // The primary retires an old boot itself: a request sealed on it is
        // refused, and the next Capability replaces it.
        let (_root, p, r, mut primary, mut rr, pview, rview) = connected();
        let old = rr.capability.unwrap().0;
        primary.connect(None);
        primary.connect(Some(pview));
        let refused = rr
            .request(&r, rview, Mode::Hitl, [11; 16], "operator".into(), Operation::Command("Pilot".into()))
            .unwrap();
        assert!(primary.receive(&p, pview, Mode::Hitl, &refused).is_err());
        rr.query = None;
        let query = rr.query_observe(&r, rview).unwrap();
        let Received::Reply(reply) = primary.receive(&p, pview, Mode::Hitl, &query).unwrap() else {
            panic!("capability reply")
        };
        assert!(matches!(rr.receive(&r, rview, Mode::Normal, &reply).unwrap(), Received::Capability { .. }));
        assert_ne!(rr.capability.unwrap().0, old);
    }
    /// The RRLS sends up to REQUEST_COPIES copies of one sealed request. The
    /// primary executes it once per request nonce (its results cache, keyed by
    /// request id and contract digest) and answers every later copy with the
    /// cached signed result or Pending, never a second execution. This covers
    /// pyro, Abort and every other proxied command alike.
    #[test]
    fn every_copy_of_a_request_executes_at_most_once() {
        for command in ["DeployParachute", "ExpandParachute", "Pilot", "ToggleAvBayUnderglow"] {
            let (_root, p, r, mut primary, rr, pview, rview) = connected();
            let id = nonce();
            let request = rr
                .request(&r, rview, Mode::Hitl, id, "operator".into(), Operation::Command(command.into()))
                .unwrap();
            let mut executions = 0;
            let mut work = None;
            // Copies spread over the whole freshness window, some before and
            // some after the primary finished the first.
            for copy in 0..REQUEST_COPIES {
                let view = Transport { sequence: pview.sequence + u32::from(copy) / 2, ..pview };
                match primary.receive(&p, view, Mode::Hitl, &request).unwrap() {
                    Received::Execute(w) => {
                        executions += 1;
                        work = Some(w);
                    }
                    Received::Pending | Received::Reply(_) => {}
                    _ => panic!("unexpected"),
                }
                if copy == 1 {
                    primary.finish(&p, work.take().unwrap(), Outcome::Queued).unwrap();
                }
            }
            assert_eq!(executions, 1, "{command}");
            // A copy past the freshness window is refused, not executed.
            let late = Transport { sequence: pview.sequence + SEQUENCE_WINDOW, ..pview };
            assert!(!matches!(primary.receive(&p, late, Mode::Hitl, &request), Ok(Received::Execute(_))));
        }
    }
    #[test]
    fn a_late_result_upgrades_only_the_matching_unconfirmed_request() {
        let id = [0x5a; 16];
        let unknown = Some(RequestStatus {
            request_id: hex(&id),
            operation: "Pilot".into(),
            phase: "unknown",
            primary_accepted: false,
            device_confirmed: false,
            error: Some("Primary response was not confirmed; inspect actual state before retrying.".into()),
        });
        let mut last = unknown.clone();
        upgrade_late_result(&mut last, [0x5b; 16], &Outcome::Queued);
        assert_eq!(last.as_ref().unwrap().phase, "unknown");
        upgrade_late_result(&mut last, id, &Outcome::Queued);
        let accepted = last.clone().unwrap();
        assert_eq!((accepted.phase, accepted.primary_accepted, accepted.error), ("accepted_by_primary", true, None));
        // Only an "unknown" status is rewritten.
        upgrade_late_result(&mut last, id, &Outcome::Busy);
        assert_eq!(last.as_ref().unwrap().phase, "accepted_by_primary");
        let mut last = unknown;
        upgrade_late_result(&mut last, id, &Outcome::Busy);
        assert_eq!(last.as_ref().unwrap().phase, "rejected_by_primary");
        assert!(!last.unwrap().primary_accepted);
    }
    #[test]
    fn frame_bound_and_signed_identity_role_tamper_checks() {
        let (_root, p, r, mut primary, rr, pview, rview) = connected();
        let largest = rr
            .request(
                &r,
                rview,
                Mode::Hitl,
                [4; 16],
                "u".repeat(64),
                Operation::Command("C".repeat(48)),
            )
            .unwrap();
        assert!(largest.len() <= 256);
        assert!(primary.receive(&p, pview, Mode::Hitl, &largest).is_ok());
        for index in [3, 4, 12, 20, 28, 36, 52, largest.len() - 1] {
            let mut corrupt = largest.clone();
            corrupt[index] ^= 1;
            assert!(wire::open(&p, &corrupt, "rrls").is_err());
        }
        assert!(wire::open(&r, &largest, "primary").is_err());
        assert!(wire::open(&p, &largest, "primary").is_err());
        assert!(
            rr.request(
                &r,
                rview,
                Mode::Hitl,
                [5; 16],
                "u".repeat(65),
                Operation::Command("Pilot".into())
            )
            .is_err()
        );
        assert!(
            rr.request(
                &r,
                rview,
                Mode::Hitl,
                [5; 16],
                "u".into(),
                Operation::Command("AutomaticAbort".into())
            )
            .is_err()
        );
    }
}

pub(crate) const MAX_WIRE: usize = wire::MAX_WIRE;
pub(crate) fn wire_boundary(bytes: &[u8]) -> Option<u64> {
    (bytes.len() >= 132 && bytes.len() <= MAX_WIRE && bytes.starts_with(b"CP\x01") && bytes[3] <= 3)
        .then(|| u64::from_le_bytes(wire::contract_digest(bytes)[..8].try_into().unwrap()))
}
/// AV-bay commands an RRLS sends itself during a takeover (the primary is out
/// of range). Fill-system, launch and ignition controls need the primary's
/// umbilical and are never taken over; the station Abort is not yet included.
pub(crate) fn takeover_operation(name: &str) -> bool {
    matches!(
        name,
        "DeployParachute" | "ExpandParachute"
            | "MonitorAltitude" | "RevokeMonitorAltitude"
            | "ConsecutiveSamples" | "RevokeConsecutiveSamples"
            | "ResetFailures" | "RevokeResetFailures"
            | "ValidateMeasms" | "RevokeValidateMeasms"
            | "EvaluationRelax" | "EvaluationFocus" | "EvaluationAbort"
            | "ReinitSensors" | "ReinitBarometer" | "EnableIMU" | "DisableIMU"
            | "AbortAfter40" | "AbortAfter100" | "AbortAfter250"
            | "ReinitAfter15" | "ReinitAfter30" | "ReinitAfter50"
            | "ToggleAvBayUnderglow" | "ToggleFlightBuzzer"
    )
}
/// The RRLS runs `name` itself: always for local operations, and for AV-bay
/// commands while it holds a takeover.
pub(crate) fn rrls_runs_locally(name: &str) -> bool {
    local_operation(name) || (crate::rrls::takeover_active() && takeover_operation(name))
}
pub(crate) fn local_operation(name: &str) -> bool {
    matches!(
        name,
        "ResetTClock"
    )
}
#[cfg(test)]
#[derive(Default)]
struct AdmissionPause {
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}
#[cfg(test)]
static ADMISSION_PAUSE: std::sync::OnceLock<
    std::sync::Mutex<Option<std::sync::Arc<AdmissionPause>>>,
> = std::sync::OnceLock::new();
#[cfg(test)]
async fn pause_before_proxy_admission() {
    let pause = ADMISSION_PAUSE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .clone();
    if let Some(pause) = pause {
        pause.reached.notify_one();
        pause.resume.notified().await;
    }
}
async fn execute_primary(state: Arc<crate::state::AppState>, request: &VerifiedRequest) -> Outcome {
    let Some(lease) = state.control_ingress.handler() else {
        return Outcome::Unavailable;
    };
    lease.run(execute_primary_inner(state, request)).await
}
async fn execute_primary_inner(
    state: std::sync::Arc<crate::state::AppState>,
    request: &VerifiedRequest,
) -> Outcome {
    fn live(request: &VerifiedRequest) -> bool {
        let Some(mesh) = crate::rrls::settings_transport() else {
            return false;
        };
        let mesh = mesh.lock().unwrap();
        let manual_stop = matches!(&request.operation, Operation::Command(name) if name == "Abort");
        mesh.command_proxy_transport(mesh.now())
            .is_some_and(|view| view.accepts(&request.envelope, true))
            && (manual_stop || matches!(request.envelope.message,Message::Request{mode,..} if mode==crate::station_mode::local_mode()))
    }
    let fill_captured =
        matches!(&request.operation, Operation::Command(name) if name == "StartFill")
            .then(|| crate::fill_recording::Captured::capture(&state));
    // Same origin capture as the local clear-abort endpoint, before auth can wait.
    let clear_abort_origin = matches!(&request.operation, Operation::Command(name) if name == "ClearGseAbort")
        .then(|| crate::gse::capture_operator_origin(&state).unwrap_or(0));
    // Preserve the signed remote source time and snapshot the local mode revision
    // before actor/config authentication can wait across a drive change.
    let plumbing_origin = request
        .plumbing_origin_utc_ms
        .map(|stamp| crate::actuator_plumbing::capture_remote_motion_origin(&state, stamp));
    use crate::auth::Permission;
    if crate::rrls::is_rrls() {
        return Outcome::Denied;
    }
    let permission = match request.operation {
        Operation::Command(_) => Permission::SendCommands,
        Operation::NetworkUtc(_) => Permission::SetSystemTime,
    };
    let actor = request.actor.clone();
    let auth = state.auth.clone();
    let principal = match tokio::task::spawn_blocking(move || {
        auth.paired_proxy_actor(&actor, permission)
    })
    .await
    {
        Ok(Ok(principal)) => principal,
        _ => return Outcome::Denied,
    };
    // User/config I/O ran outside locks; recheck the live contract immediately
    // before the shared primary policy and nonblocking queue admission.
    if !live(request) {
        return Outcome::Stale;
    }
    // A request paused during authentication cannot acquire dispatch after
    // recovery revoked its capability. Already acquired dispatch keeps the
    // barrier non-quiescent until this async call returns; no mutex crosses it.
    #[cfg(test)]
    pause_before_proxy_admission().await;
    let manual_stop = matches!(&request.operation, Operation::Command(name) if name == "Abort");
    let Ok(_lease) = request.admission.admit(request.envelope.boot, manual_stop) else {
        return Outcome::Stale;
    };
    let status = match &request.operation {
        Operation::Command(name) => {
            if local_operation(name) || !principal.allows_command_name(permission_name(name)) {
                return Outcome::Denied;
            }
            if let Some(origin) = clear_abort_origin {
                crate::gse::clear_abort_for_proxy(&state, &principal, origin).await
            } else {
            let Ok(command) = serde_json::from_value::<crate::types::TelemetryCommand>(
                serde_json::Value::String(name.clone()),
            ) else {
                return Outcome::Invalid;
            };
            let command = if let Some(captured) = fill_captured {
                let initial = request.clone();
                let accepted = request.clone();
                let authority = crate::fill_recording::Authority::Proxy {
                    actor: request.actor.clone(),
                    admission_current: Arc::new(move || {
                        live(&initial)
                            && initial
                                .admission
                                .request_allowed(initial.envelope.boot, false)
                    }),
                    session_current: Arc::new(move || {
                        if !accepted
                            .admission
                            .request_allowed(accepted.envelope.boot, false)
                        {
                            return false;
                        }
                        let Some(mesh) = crate::rrls::settings_transport() else {
                            return false;
                        };
                        let mesh = mesh.lock().unwrap();
                        mesh.command_proxy_transport(mesh.now())
                            .is_some_and(|view| view.session == accepted.envelope.session)
                            && matches!(accepted.envelope.message, Message::Request { mode, .. } if mode == crate::station_mode::local_mode())
                    }),
                };
                crate::types::TelemetryCommand::FillWithRecording {
                    origin: captured.bind(authority, Some(&principal)),
                }
            } else {
                command
            };
            if name == "RetractPlumbing" {
                crate::web::dispatch_proxied_command_with_origin(
                    state,
                    principal,
                    command,
                    plumbing_origin,
                    request.launch_revision,
                )
                .await
                .0
            } else {
                crate::web::dispatch_proxied_command(
                    state,
                    principal,
                    command,
                    request.launch_revision,
                )
                .await
                .0
            }
            }
        }
        Operation::NetworkUtc(utc) => {
            crate::system_clock::apply_authorized_network_utc(state, principal, *utc)
                .await
                .status()
        }
    };
    match status.as_u16() {
        200..=299 => Outcome::Queued,
        400 => Outcome::Invalid,
        401 | 403 => Outcome::Denied,
        409 => Outcome::Stale,
        _ => Outcome::Unavailable,
    }
}

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::sync::{Arc, Mutex, OnceLock};
#[derive(Clone, Debug, Serialize)]
pub struct RequestStatus {
    pub request_id: String,
    pub operation: String,
    pub phase: &'static str,
    pub primary_accepted: bool,
    pub device_confirmed: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub supported: bool,
    pub paired: bool,
    pub connected: bool,
    pub available: bool,
    pub primary_mode: Option<Mode>,
    pub supported_commands: Vec<String>,
    pub pending: Option<RequestStatus>,
    pub last: Option<RequestStatus>,
    pub reason: String,
    /// RRLS takeover: the primary is out of range and this RRLS sends these
    /// AV-bay commands itself (its own command endpoint, not the proxy).
    #[serde(default)]
    pub takeover: bool,
    #[serde(default)]
    pub takeover_commands: Vec<String>,
}
impl Default for Status {
    fn default() -> Self {
        Self{supported:true,paired:false,connected:false,available:false,primary_mode:None,supported_commands:Vec::new(),pending:None,last:None,reason:"Paired primary control has not initialized; standalone hardware authority is unavailable.".into(),takeover:false,takeover_commands:Vec::new()}
    }
}
// This includes the original local credential only in process memory. It is
// never serialized, signed into radio bytes, debug-formatted, or logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteLaunchOrigin {
    primary: crate::primary_mirror::LaunchOrigin,
    local_revision: u64,
}
#[derive(Clone)]
struct Pending {
    id: [u8; 16],
    actor: String,
    token: String,
    operation: Operation,
    plumbing_origin_utc_ms: Option<u64>,
    launch_origin: Option<RemoteLaunchOrigin>,
    session: u32,
    sequence: u32,
    wire: Option<Vec<u8>>,
    /// Contract digests of every copy sent (each copy is sealed afresh).
    copy_digests: Vec<[u8; 16]>,
    last_sent: Option<u64>,
    attempts: u8,
    created_at: u64,
}
#[derive(Default)]
struct Broker {
    status: Status,
    pending: Option<Pending>,
    readiness_request: Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
    readiness_last: Option<std::time::Instant>,
}
static BROKER: OnceLock<Mutex<Broker>> = OnceLock::new();
fn broker() -> &'static Mutex<Broker> {
    BROKER.get_or_init(Default::default)
}
pub(crate) async fn request_readiness_check() -> std::result::Result<(), String> {
    let (send, receive) = tokio::sync::oneshot::channel();
    {
        let mut broker = broker().lock().unwrap();
        if !broker.status.paired || !broker.status.connected {
            return Err("Connect the paired primary before checking launch readiness.".into());
        }
        if broker.readiness_request.is_some()
            || broker
                .readiness_last
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(5))
        {
            return Err(
                "A readiness check is pending or cooling down. Retry after five seconds.".into(),
            );
        }
        broker.readiness_last = Some(std::time::Instant::now());
        broker.readiness_request = Some(send);
    }
    tokio::time::timeout(std::time::Duration::from_secs(5),receive).await
        .map_err(|_|"Primary readiness check timed out. Update both stations if the primary does not support this check; no Launch was sent.".to_string())?
        .map_err(|_|"Primary readiness service stopped; no Launch was sent.".to_string())?
}
pub(crate) fn status() -> Status {
    let mut status = broker().lock().unwrap().status.clone();
    if crate::rrls::is_rrls() && crate::rrls::takeover_active() {
        status.takeover = true;
        status.takeover_commands = crate::sequences::all_command_names().into_iter()
            .filter(|name| takeover_operation(name)).map(String::from).collect();
        status.reason = if crate::rrls::handback_waiting() {
            "RRLS takeover: GS heard, but it has no route to the AV bay yet, so this RRLS keeps commanding the AV bay directly. Authority returns to the primary once it reaches the AV bay.".into()
        } else {
            "RRLS takeover: the primary GroundStation is out of range, so this RRLS commands the AV bay directly. Authority returns to the primary automatically once it is heard again and reaches the AV bay.".into()
        };
    }
    status
}
fn elapsed_upper_bound(now: u64, created: u64) -> u64 {
    // A simulator clock reset or any unknown epoch must expire observations.
    now.checked_sub(created).unwrap_or(u64::MAX)
}
fn hex(id: &[u8; 16]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn operation_name(operation: &Operation) -> String {
    match operation {
        Operation::Command(name) => name.clone(),
        Operation::NetworkUtc(_) => "NetworkUtc".into(),
    }
}
fn finish_pending(id: [u8; 16], phase: &'static str, accepted: bool, error: Option<String>) {
    let mut broker = broker().lock().unwrap();
    if broker
        .pending
        .as_ref()
        .is_none_or(|pending| pending.id != id)
    {
        return;
    }
    let pending = broker.pending.take().unwrap();
    broker.status.pending = None;
    broker.status.last = Some(RequestStatus {
        request_id: hex(&id),
        operation: operation_name(&pending.operation),
        phase,
        primary_accepted: accepted,
        device_confirmed: false,
        error,
    });
}
/// The primary's signed result for a request this RRLS already reported as
/// unconfirmed arrived late (its id and contract digest matched): report the
/// primary's actual outcome instead of "unknown".
fn note_late_result(id: [u8; 16], outcome: &Outcome) {
    upgrade_late_result(&mut broker().lock().unwrap().status.last, id, outcome);
}
fn upgrade_late_result(last: &mut Option<RequestStatus>, id: [u8; 16], outcome: &Outcome) {
    let Some(last) = last
        .as_mut()
        .filter(|last| last.request_id == hex(&id) && last.phase == "unknown")
    else {
        return;
    };
    if matches!(outcome, Outcome::Queued) {
        last.phase = "accepted_by_primary";
        last.primary_accepted = true;
        last.error = None;
    } else {
        last.phase = "rejected_by_primary";
        last.error = Some(format!("Primary refused request: {outcome:?}"));
    }
}
fn finish_revoked(pending: &Pending) {
    let sent = pending.attempts != 0;
    finish_pending(
        pending.id,
        if sent { "unknown" } else { "denied" },
        false,
        Some(if sent {
            "Operator authorization ended after a request was sent; inspect actual state before retrying."
        } else {
            "Operator session permission expired or was revoked before any request was sent."
        }.into()),
    );
}
pub(crate) async fn submit(
    state: &Arc<crate::state::AppState>,
    principal: &crate::auth::AuthPrincipal,
    token: Option<&str>,
    operation: Operation,
) -> Response {
    submit_with_plumbing_origin(state, principal, token, operation, None).await
}
pub(crate) async fn submit_with_plumbing_origin(
    state: &Arc<crate::state::AppState>,
    principal: &crate::auth::AuthPrincipal,
    token: Option<&str>,
    operation: Operation,
    plumbing_origin_utc_ms: Option<u64>,
) -> Response {
    let launch_origin = match &operation {
        Operation::Command(name) => capture_rrls_launch_origin(state, name),
        _ => None,
    };
    submit_with_origins(
        state,
        principal,
        token,
        operation,
        plumbing_origin_utc_ms,
        launch_origin,
    )
    .await
}
pub(crate) fn capture_rrls_launch_origin(
    state: &crate::state::AppState,
    name: &str,
) -> Option<RemoteLaunchOrigin> {
    if !crate::rrls::is_rrls() || !wire::launch_command(name) {
        return None;
    }
    let local_revision = crate::board_abort::capture_launch_revision_named(state, name)?;
    let primary = crate::primary_mirror::capture_launch_origin(state)?;
    Some(RemoteLaunchOrigin {
        primary,
        local_revision,
    })
}
pub(crate) async fn submit_with_origins(
    state: &Arc<crate::state::AppState>,
    principal: &crate::auth::AuthPrincipal,
    token: Option<&str>,
    operation: Operation,
    plumbing_origin_utc_ms: Option<u64>,
    launch_origin: Option<RemoteLaunchOrigin>,
) -> Response {
    // The proximity lock of the devices reporting to this RRLS. The primary
    // applies its own devices' lock when it admits the forwarded command.
    if crate::rrls::is_rrls()
        && let Operation::Command(name) = &operation
        && let Some(message) = crate::proximity_lock::rrls_forward_refusal(state, name, principal.username.as_deref())
    {
        return (StatusCode::CONFLICT, message).into_response();
    }
    let Some(lease) = state.control_ingress.handler() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Control ingress is closed").into_response();
    };
    lease
        .run(submit_with_origins_inner(
            state,
            principal,
            token,
            operation,
            plumbing_origin_utc_ms,
            launch_origin,
        ))
        .await
}
async fn submit_with_origins_inner(
    state: &Arc<crate::state::AppState>,
    principal: &crate::auth::AuthPrincipal,
    token: Option<&str>,
    operation: Operation,
    plumbing_origin_utc_ms: Option<u64>,
    launch_origin: Option<RemoteLaunchOrigin>,
) -> Response {
    use crate::auth::Permission;
    if plumbing_origin_utc_ms.is_some()
        && !matches!(&operation,Operation::Command(name) if name=="RetractPlumbing")
    {
        return (
            StatusCode::BAD_REQUEST,
            "Motion origin is only valid for plumbing retract.",
        )
            .into_response();
    }
    let permission = match operation {
        Operation::Command(_) => Permission::SendCommands,
        Operation::NetworkUtc(_) => Permission::SetSystemTime,
    };
    if !crate::rrls::is_rrls() || principal.anonymous || !principal.permissions.allows(permission) {
        return (
            StatusCode::FORBIDDEN,
            "Authenticated recovery operator required.",
        )
            .into_response();
    }
    if let Operation::Command(name) = &operation
        && (!wire::valid_command(name)
            || local_operation(name)
            || !principal.allows_command_name(permission_name(name)))
        {
            return (
                StatusCode::FORBIDDEN,
                "Command is not available through the primary proxy.",
            )
                .into_response();
        }
    let Some(token) = token.filter(|value| !value.is_empty() && value.len() <= 8192) else {
        return (StatusCode::UNAUTHORIZED, "Authenticated session required.").into_response();
    };
    // Revalidate revocation/current grants at admission, not just WS connection.
    let fresh = match state
        .auth
        .authorize_token(&state.auth_db, Some(token), permission)
        .await
    {
        Ok(principal) if !principal.anonymous => principal,
        _ => {
            return (
                StatusCode::FORBIDDEN,
                "Operator session is no longer authorized.",
            )
                .into_response();
        }
    };
    let Some(actor) = fresh
        .username
        .as_deref()
        .filter(|name| !name.is_empty() && name.len() <= wire::MAX_ACTOR)
    else {
        return (StatusCode::FORBIDDEN, "A named operator is required.").into_response();
    };
    if let Operation::Command(name) = &operation
        && !fresh.allows_command_name(permission_name(name)) {
            return (StatusCode::FORBIDDEN, "Command permission was revoked.").into_response();
        }
    if matches!(&operation, Operation::Command(name) if name == "Abort") {
        // Direct path first: a listening RRLS may put this Abort on the radio itself.
        if let Some(mesh) = crate::rrls::settings_transport() {
            let mut mesh = mesh.lock().unwrap();
            let now = mesh.now();
            mesh.open_abort_egress(now);
        }
        state.manual_abort_intent.request();
        // The local stop alone never reaches the AV bay from a listening RRLS:
        // send the Abort to the primary at once as well. The primary admits it
        // through its manual-stop path (never refused for recovery or build
        // support), and it replaces any ordinary request still waiting.
        let view = crate::rrls::settings_transport().and_then(|mesh| {
            let mesh = mesh.lock().unwrap();
            let now = mesh.now();
            mesh.command_proxy_transport(now).map(|view| (now, view))
        });
        let mut broker = broker().lock().unwrap();
        let forwarded = match view {
            Some((created_at, view)) if broker.status.available => {
                let already = broker.pending.as_ref().is_some_and(|pending| pending.operation == operation);
                if !already {
                    if let Some(previous) = broker.pending.take() {
                        broker.status.last = Some(RequestStatus {
                            request_id: hex(&previous.id),
                            operation: operation_name(&previous.operation),
                            phase: "superseded_by_abort",
                            primary_accepted: false,
                            device_confirmed: false,
                            error: Some("An operator Abort replaced this request before the primary answered; it may still have run.".into()),
                        });
                    }
                    let id = nonce();
                    broker.pending = Some(Pending {
                        id,
                        actor: actor.into(),
                        token: token.into(),
                        operation: operation.clone(),
                        plumbing_origin_utc_ms: None,
                        launch_origin: None,
                        session: view.session,
                        sequence: view.sequence,
                        wire: None,
                        copy_digests: Vec::new(),
                        last_sent: None,
                        attempts: 0,
                        created_at,
                    });
                    broker.status.pending = Some(RequestStatus {
                        request_id: hex(&id),
                        operation: "Abort".into(),
                        phase: "pending_primary",
                        primary_accepted: false,
                        device_confirmed: false,
                        error: None,
                    });
                }
                true
            }
            _ => false,
        };
        if !forwarded {
            broker.status.last = Some(RequestStatus {
                request_id: hex(&nonce()),
                operation: "Abort".into(),
                phase: "local_stop_retained",
                primary_accepted: false,
                device_confirmed: false,
                error: Some("Primary observation and actual board readback are pending.".into()),
            });
        }
        return (StatusCode::ACCEPTED, Json(broker.status.clone())).into_response();
    }
    if matches!(&operation, Operation::Command(name) if crate::board_abort::launch_command(name)) {
        if let Some(message) = crate::board_abort::blocked_message(state) {
            return (StatusCode::CONFLICT, message).into_response();
        }
        if launch_origin.is_none()
            || capture_rrls_launch_origin(
                state,
                match &operation {
                    Operation::Command(name) => name,
                    _ => "",
                },
            ) != launch_origin
        {
            return (
                StatusCode::CONFLICT,
                "Fresh authenticated primary board-state proof is required before Launch.",
            )
                .into_response();
        }
    }
    let Some(mesh) = crate::rrls::settings_transport() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Primary radio transport unavailable; standalone control is not authorized.",
        )
            .into_response();
    };
    let (created_at, view) = {
        let mesh = mesh.lock().unwrap();
        let now = mesh.now();
        (now, mesh.command_proxy_transport(now))
    };
    let Some(view) = view else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Primary is disconnected; standalone control is not authorized.",
        )
            .into_response();
    };
    if launch_origin.is_some_and(|origin| origin.primary.session != view.session) {
        return (
            StatusCode::CONFLICT,
            "Primary connection changed; check readiness and make a new explicit Launch request.",
        )
            .into_response();
    }
    let mut broker = broker().lock().unwrap();
    if !broker.status.available {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            broker.status.reason.clone(),
        )
            .into_response();
    }
    if let Operation::Command(name) = &operation
        && !broker
            .status
            .supported_commands
            .iter()
            .any(|supported| supported == name)
        {
            return (
                StatusCode::BAD_REQUEST,
                "Command is unsupported by the connected primary build.",
            )
                .into_response();
        }
    if let Some(pending) = &broker.pending {
        if pending.actor.eq_ignore_ascii_case(actor) && pending.operation == operation {
            return (StatusCode::ACCEPTED, Json(broker.status.clone())).into_response();
        }
        return (
            StatusCode::CONFLICT,
            "Another primary request is pending; no command was queued.",
        )
            .into_response();
    }
    let id = nonce();
    broker.pending = Some(Pending {
        id,
        actor: actor.into(),
        token: token.into(),
        operation: operation.clone(),
        plumbing_origin_utc_ms,
        launch_origin,
        session: view.session,
        sequence: view.sequence,
        wire: None,
        copy_digests: Vec::new(),
        last_sent: None,
        attempts: 0,
        created_at,
    });
    broker.status.pending = Some(RequestStatus {
        request_id: hex(&id),
        operation: operation_name(&operation),
        phase: "pending_primary",
        primary_accepted: false,
        device_confirmed: false,
        error: None,
    });
    // Accepted here means only LOCAL bounded request admission. A signed result
    // and actual board telemetry remain separate, explicitly visible states.
    (StatusCode::ACCEPTED, Json(broker.status.clone())).into_response()
}
pub(crate) async fn get_status(
    axum::extract::State(state): axum::extract::State<Arc<crate::state::AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Err(error) =
        crate::web::authorize_headers(&state, &headers, crate::auth::Permission::ViewData).await
    {
        return error;
    }
    Json(status()).into_response()
}
// Call only after a current signed nonce or exact pending request matched.
// The cache lock is released before the local WebSocket snapshot is built.
fn accept_presentation(
    state: &crate::state::AppState,
    bytes: &[u8],
    age_ms: u64,
    boot: [u8; 16],
    session: u32,
) -> bool {
    let accepted = crate::primary_mirror::accept_control_observation_with_context(
        state, bytes, age_ms, boot, session,
    );
    if accepted {
        state.broadcast_action_policy_snapshot();
    }
    accepted
}
fn invalidate_presentation(state: &crate::state::AppState) {
    retained_config::disconnected();
    crate::primary_mirror::invalidate_control_observation(state);
    if crate::rrls::is_rrls() {
        state.broadcast_action_policy_snapshot();
    }
}
pub(crate) fn start(state: Arc<crate::state::AppState>) -> Option<tokio::task::JoinHandle<()>> {
    let pairing = match Pairing::load() {
        Ok(Some(pairing)) => pairing,
        Ok(None) => {
            broker().lock().unwrap().status.reason="Pair this RRLS with the primary (Administration > Station pairing) for authenticated primary commands. Standalone hardware authority is unavailable.".into();
            return None;
        }
        Err(_) => {
            broker().lock().unwrap().status.reason =
                "Station pairing is invalid; primary commands are disabled.".into();
            return None;
        }
    };
    let pairing = Arc::new(pairing);
    let Some(mesh) = crate::rrls::settings_transport() else {
        let mut broker = broker().lock().unwrap();
        broker.status.paired = true;
        broker.status.reason =
            "Paired radio transport is unavailable; no command can be forwarded.".into();
        return None;
    };
    let role = { mesh.lock().unwrap().role };
    let mut runtime = Runtime::new(role);
    if role == Role::Primary && recovery::register(&runtime.admission).is_err() {
        log::error!("A primary proxy is already active; duplicate task was not started");
        return None;
    }
    if role == Role::Primary
        && state
            .operator_admission_barrier
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
        && let Err(error) = runtime.admission.begin() {
            log::error!("Restored recovery proxy closure failed: {error}");
            return None;
        }
    if runtime.peer(&pairing).is_err() {
        let mut broker = broker().lock().unwrap();
        broker.status.paired = true;
        broker.status.reason = "Authenticated primary control requires exactly one paired counterparty of the expected role.".into();
        return None;
    }
    let mut shutdown = state.shutdown_subscribe();
    Some(tokio::spawn(async move {
        let mut retained = if role == Role::Rrls { Some(retained_config::Worker::open(pairing.clone())) } else { None };
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(20));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut outbox = VecDeque::<([u8; 16], Vec<u8>)>::new();
        let mut query_attempts = 0u8;
        let mut last_query = None;
        let mut query_created_at = None;
        let mut previous_session = None;
        let mut readiness_reply: Option<
            tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
        > = None;
        let mut readiness_started = None::<std::time::Instant>;
        let mut readiness_phase = 0u8;
        let mut readiness_poll_at = None::<std::time::Instant>;
        let mut last_observe_refresh = None::<u64>;
        // RRLS: the last Capability or Result it verified, the copies of the
        // current observe refresh with the last copy's time and retry delay,
        // and the discovery schedule while it holds no capability.
        let mut last_exchange = None::<u64>;
        let mut refresh_copies = 0u8;
        let mut refresh_sent = None::<(u64, u64)>;
        let mut discovery_step = 0u8;
        let mut next_discovery = None::<u64>;
        // RRLS: the last request that timed out unconfirmed (id, contract
        // digest), so a signed result that arrives late still updates it.
        let mut unconfirmed = None::<([u8; 16], Vec<[u8; 16]>)>;

        loop {
            tokio::select! {_=shutdown.recv()=>break,_=tick.tick()=>{}}
            if let Some(retained) = &mut retained {retained.service().await;}
            let (now, view, frames) = {
                let mut mesh = mesh.lock().unwrap();
                let now = mesh.now();
                (
                    now,
                    mesh.command_proxy_transport(now),
                    (0..4)
                        .filter_map(|_| mesh.take_command_proxy())
                        .collect::<Vec<_>>(),
                )
            };
            if previous_session != view.map(|view| view.session) {
                previous_session = view.map(|view| view.session);
                if let Some(reply) = readiness_reply.take() {
                    let _ = reply.send(Err(
                        "Primary connection changed; request a new readiness check.".into(),
                    ));
                }
                if let Some(reply) = broker().lock().unwrap().readiness_request.take() {
                    let _ = reply.send(Err(
                        "Primary connection changed; request a new readiness check.".into(),
                    ));
                }
                readiness_phase = 0;
                query_attempts = 0;
                last_query = None;
                query_created_at = None;
                discovery_step = 0;
                next_discovery = None;
                last_observe_refresh = None;
                refresh_copies = 0;
                refresh_sent = None;
                unconfirmed = None;
                outbox.clear();
                invalidate_presentation(&state);
                let id = broker()
                    .lock()
                    .unwrap()
                    .pending
                    .as_ref()
                    .map(|pending| pending.id);
                if let Some(id) = id {
                    finish_pending(
                        id,
                        "unknown",
                        false,
                        Some(
                            "Primary connection changed; inspect actual state before retrying."
                                .into(),
                        ),
                    );
                }
            }
            runtime.connect(view);
            if runtime.synchronize_recovery() {
                outbox.clear();
                invalidate_presentation(&state);
            }
            let Some(view) = view else {
                let mut broker = broker().lock().unwrap();
                broker.status.paired = true;
                broker.status.connected = false;
                broker.status.available = false;
                broker.status.reason =
                    "Primary disconnected; standalone hardware authority is unavailable.".into();
                continue;
            };
            let received_at = std::time::Instant::now();
            for bytes in frames {
                if role == Role::Primary {
                    runtime.observation =
                        crate::primary_mirror::encode_control_observation(&state, 96);
                }
                let received =
                    runtime.receive(&pairing, view, crate::station_mode::local_mode(), &bytes);
                if role == Role::Primary && received.is_ok() {
                    mesh.lock().unwrap().note_verified_peer(now);
                }
                match received {
                    Ok(Received::Reply(bytes)) => {
                        if runtime.take_readiness_demand()
                            && let Err(error) =
                                crate::board_abort::request_paired_readiness_queries(&state)
                            {
                                log::debug!(
                                    "Read-only readiness demand could not be queued: {error}"
                                );
                            }
                        if outbox.len() < 4 {
                            outbox.push_back((runtime.boot, bytes));
                        }
                    }
                    Ok(Received::Execute(request)) => {
                        let outcome = execute_primary(state.clone(), &request).await;
                        crate::primary_mirror::note_command_outcome(&state);
                        runtime.observation =
                            crate::primary_mirror::encode_control_observation(&state, 96);
                        log::info!(
                            "Primary paired request issuer={} actor={} operation={} outcome={:?}",
                            request.envelope.issuer,
                            request.actor,
                            operation_name(&request.operation),
                            outcome
                        );
                        if let Ok(reply) = runtime.finish(&pairing, request, outcome)
                            && outbox.len() < 4 {
                                outbox.push_back((runtime.boot, reply));
                            }
                    }
                    Ok(Received::Capability {
                        boot,
                        mode,
                        observation,
                    }) => {
                        if let Some(created) = query_created_at {
                            accept_presentation(
                                &state,
                                &observation,
                                elapsed_upper_bound(now, created),
                                boot,
                                view.session,
                            );
                        }
                        if let Some(created) = query_created_at
                            && let Some(retained) = &mut retained
                        {
                            retained.accepted_at(
                                mode,
                                boot,
                                &observation,
                                elapsed_upper_bound(now, created),
                                received_at,
                            );
                        }
                        debug_assert_ne!(boot, [0; 16]);
                        last_exchange = Some(now);
                        refresh_copies = 0;
                        refresh_sent = None;
                        discovery_step = 0;
                        next_discovery = None;
                        broker().lock().unwrap().status.primary_mode = Some(mode);
                        if readiness_phase == 4 {
                            readiness_phase = 0;
                        }
                        if readiness_reply.is_some() {
                            if !crate::primary_mirror::observation_has_board_guard(&observation) {
                                if let Some(reply) = readiness_reply.take() {
                                    let _=reply.send(Err("Primary software does not support verified board readiness. Update both stations; no Launch was sent.".into()));
                                }
                                readiness_phase = 0;
                            } else if readiness_phase == 1 {
                                readiness_phase = 2;
                                readiness_poll_at = Some(
                                    std::time::Instant::now()
                                        + std::time::Duration::from_millis(800),
                                );
                            } else if readiness_phase == 3 {
                                if let Some(reply) = readiness_reply.take() {
                                    let _ = reply.send(Ok(()));
                                }
                                readiness_phase = 0;
                            }
                        }
                    }
                    Ok(Received::Result {
                        boot,
                        request,
                        digest,
                        outcome,
                        observation,
                    }) => {
                        last_exchange = Some(now);
                        if unconfirmed.as_ref().is_some_and(|(id, digests)| *id == request && digests.contains(&digest)) {
                            unconfirmed = None;
                            note_late_result(request, &outcome);
                        }
                        let created =
                            broker()
                                .lock()
                                .unwrap()
                                .pending
                                .as_ref()
                                .and_then(|pending| {
                                    (pending.id == request
                                        && pending.wire.as_ref().is_some_and(|wire| {
                                            wire::contract_digest(wire) == digest
                                        }))
                                    .then_some(pending.created_at)
                                });
                        if let Some(created) = created {
                            accept_presentation(
                                &state,
                                &observation,
                                elapsed_upper_bound(now, created),
                                boot,
                                view.session,
                            );
                            // Live observation is stamped BEFORE any persistence await:
                            // slow disk must never freshen proof/source authority.
                            // Mode comes only from this exact accepted capability boot.
                            if let Some((capability_boot, mode, _)) = runtime.capability
                                && capability_boot == boot
                                && let Some(retained) = &mut retained
                            {
                                retained.accepted_at(
                                    mode,
                                    boot,
                                    &observation,
                                    elapsed_upper_bound(now, created),
                                    received_at,
                                );
                            }
                            match outcome {
                                Outcome::Queued => {
                                    finish_pending(request, "accepted_by_primary", true, None)
                                }
                                _ => finish_pending(
                                    request,
                                    "rejected_by_primary",
                                    false,
                                    Some(format!("Primary refused request: {outcome:?}")),
                                ),
                            }
                        }
                    }
                    Ok(Received::Pending) => {}
                    // Invalid metadata never authorizes or refreshes anything,
                    // but say why, at most once per reason every 10 s: a silent
                    // rejection left RRLS commands unavailable with no trace.
                    Err(error) => note_rejected_frame(role, &error.to_string()),
                }
            }
            if role == Role::Rrls {
                if readiness_reply.is_none()
                    && let Some(request) = broker().lock().unwrap().readiness_request.take() {
                        readiness_reply = Some(request);
                        readiness_started = Some(std::time::Instant::now());
                        readiness_phase = 1;
                        runtime.query = None;
                        query_attempts = 0;
                        last_query = None;
                        query_created_at = None;
                    }
                if readiness_reply.is_some()
                    && readiness_started
                        .is_some_and(|at| at.elapsed() >= std::time::Duration::from_secs(4))
                {
                    if let Some(reply) = readiness_reply.take() {
                        let _=reply.send(Err("Primary readiness check was unsupported or timed out. Update both stations if needed; no Launch was sent.".into()));
                    }
                    readiness_phase = 4;
                    runtime.query = None;
                    query_attempts = 0;
                    last_query = None;
                    query_created_at = None;
                }
                if readiness_phase == 2
                    && readiness_poll_at.is_some_and(|at| std::time::Instant::now() >= at)
                {
                    readiness_phase = 3;
                    runtime.query = None;
                    query_attempts = 0;
                    last_query = None;
                    query_created_at = None;
                }
                if readiness_phase == 4 && runtime.capability.is_none() && query_attempts >= 3 {
                    // The readiness re-check went unanswered and no capability
                    // is held: ordinary discovery takes over.
                    readiness_phase = 0;
                }
                let mode = crate::station_mode::snapshot();
                let available = runtime.capability.is_some_and(|(_, primary, _)| {
                    mode.synchronized && mode.effective_mode == Some(primary)
                });
                {
                    let mut broker = broker().lock().unwrap();
                    broker.status.paired = true;
                    broker.status.connected = true;
                    broker.status.available = available;
                    broker.status.primary_mode = runtime.capability.map(|(_, mode, _)| mode);
                    broker.status.supported_commands =
                        runtime.capability.map_or_else(Vec::new, |(_, _, mask)| {
                            command_catalog()
                                .iter()
                                .enumerate()
                                .filter(|(index, _)| mask & (1u64 << index) != 0)
                                .map(|(_, name)| (*name).into())
                                .collect()
                        });
                    broker.status.reason=if available {"Requests are checked by the primary; device confirmation still requires actual telemetry."} else if runtime.capability.is_none() && discovery_step >= 3 {"The primary has not answered this RRLS's signed control query yet; discovery keeps retrying every few seconds."} else {"Waiting for signed primary control capability and matching current mode."}.into();
                }
                // Readiness checks keep their own bounded burst (at most three
                // queries 500 ms apart inside the 4 s readiness timeout).
                let readiness_query = matches!(readiness_phase, 1 | 3)
                    || (readiness_phase == 4 && runtime.capability.is_some());
                if readiness_query
                    && query_attempts < 3
                    && last_query.is_none_or(|last| now >= last && now - last >= 500)
                    && let Ok(query) = if readiness_phase == 1 {
                        runtime.query_for_readiness(&pairing, view, true)
                    } else if readiness_phase == 3 {
                        runtime.query_observe(&pairing, view)
                    } else {
                        runtime.query(&pairing, view)
                    }
                        && mesh.lock().unwrap().queue_command_proxy(&query, now) {
                            query_created_at.get_or_insert(now);
                            query_attempts += 1;
                            last_query = Some(now);
                        }
                if !readiness_query
                    && runtime.capability.is_none()
                    && readiness_phase != 2
                    && next_discovery.is_none_or(|at| now >= at)
                {
                    if query_created_at
                        .is_some_and(|created| now.checked_sub(created).is_none_or(|age| age >= DISCOVERY_NONCE_MS))
                    {
                        runtime.query = None;
                        query_created_at = None;
                    }
                    if let Ok(query) = runtime.query(&pairing, view)
                        && mesh.lock().unwrap().queue_command_proxy(&query, now)
                    {
                        query_created_at.get_or_insert(now);
                        last_query = Some(now);
                        next_discovery = Some(now.saturating_add(discovery_delay(discovery_step)));
                        discovery_step = discovery_step.saturating_add(1);
                    }
                }
                let idle = runtime.capability.is_some()
                    && readiness_reply.is_none()
                    && !matches!(readiness_phase, 1..=3)
                    && last_query != Some(now)
                    && broker().lock().unwrap().pending.is_none();
                let link_idle_direct = mesh.lock().unwrap().observation_refresh_allowed(now);
                let retry = refresh_retry_due(now, refresh_copies, refresh_sent, link_idle_direct, idle);
                // A new refresh waits for the previous copy's reply window, so
                // retries and the next period never send back to back.
                let period = observe_refresh_period(now, last_exchange);
                if retry
                    || (observe_refresh_due(now, last_observe_refresh, link_idle_direct, idle, period)
                        && refresh_sent.is_none_or(|(at, _)| now.checked_sub(at).is_none_or(|age| age >= REFRESH_REPLY_MS)))
                {
                    // Fresh nonce: an unanswered earlier query cannot be replayed.
                    runtime.query = None;
                    if let Ok(query) = runtime.query_observe(&pairing, view) {
                        if mesh.lock().unwrap().queue_command_proxy(&query, now) {
                            query_created_at = Some(now);
                            if !retry {
                                last_observe_refresh = Some(now);
                                refresh_copies = 0;
                            }
                            refresh_copies = refresh_copies.saturating_add(1);
                            refresh_sent = Some((now, refresh_retry_delay()));
                        } else {
                            runtime.query = None;
                        }
                    }
                }
                let pending = broker().lock().unwrap().pending.clone();
                if let Some(mut pending) = pending {
                    if pending.launch_origin.is_some_and(|origin| {
                        origin.local_revision != state.board_abort.abort_revision()
                    }) {
                        finish_pending(pending.id,"unknown",false,Some("A locally observed board Abort superseded the queued Launch. Check readiness and make a new explicit request.".into()));
                        continue;
                    }
                    let sequence_age = view.sequence.wrapping_sub(pending.sequence);
                    // Copies while the sealed sequence is fresh; the result may
                    // still arrive until the request's own timeout
                    // (LATE_REPLY_WINDOW), since a relayed round trip can
                    // outlast the sequence window.
                    if !available
                        || pending.session != view.session
                        || sequence_age > LATE_REPLY_WINDOW
                    {
                        finish_pending(pending.id,"unknown",false,Some("Primary response was not confirmed; inspect actual state before retrying.".into()));
                        // A lost reply ends only this request. The capability is
                        // still the primary's signed, fresh grant: dropping it here
                        // disabled every RRLS control after one lost reply and
                        // waited out a 30 s discovery cooldown on a busy channel.
                        if !available || pending.session != view.session {
                            invalidate_presentation(&state);
                            runtime.capability = None;
                            runtime.query = None;
                            query_attempts = 0;
                            last_query = None;
                            query_created_at = None;
                            discovery_step = 0;
                            next_discovery = None;
                        } else {
                            // Lost air, not a rejection: the primary refuses with a
                            // signed result, and a changed boot or mode arrives in
                            // its next Capability. Re-verify now, and let a late
                            // signed result still report this request's outcome.
                            unconfirmed = (!pending.copy_digests.is_empty()).then(|| (pending.id, pending.copy_digests.clone()));
                            last_observe_refresh = None;
                            refresh_copies = 0;
                            refresh_sent = None;
                        }
                    } else if let Some(copy_sequence) = request_copy_sequence(pending.sequence, view.sequence, pending.attempts)
                        && pending.last_sent.is_none_or(|last| {
                            now >= last
                                && now - last >= request_resend_delay(&pending.id, pending.attempts)
                        })
                    {
                        let required = match pending.operation {
                            Operation::Command(_) => crate::auth::Permission::SendCommands,
                            Operation::NetworkUtc(_) => crate::auth::Permission::SetSystemTime,
                        };
                        let authorized = state
                            .auth
                            .authorize_token(&state.auth_db, Some(&pending.token), required)
                            .await
                            .ok()
                            .is_some_and(|principal| {
                                !principal.anonymous
                                    && principal.username.as_deref().is_some_and(|name| {
                                        name.eq_ignore_ascii_case(&pending.actor)
                                    })
                                    && match &pending.operation {
                                        Operation::Command(name) => {
                                            principal.allows_command_name(name)
                                        }
                                        Operation::NetworkUtc(_) => {
                                            principal.permissions.set_system_time
                                        }
                                    }
                            });
                        if !authorized {
                            finish_revoked(&pending);
                            continue;
                        }
                        {
                            let Some(mode) = mode.effective_mode else {
                                continue;
                            };
                            let encoded = if pending.plumbing_origin_utc_ms.is_some() {
                                runtime.request_with_origins(
                                    &pairing,
                                    Transport {
                                        session: pending.session,
                                        sequence: copy_sequence,
                                    },
                                    mode,
                                    pending.id,
                                    pending.actor.clone(),
                                    pending.operation.clone(),
                                    pending.plumbing_origin_utc_ms,
                                    pending.launch_origin.map(|origin| origin.primary),
                                )
                            } else {
                                runtime.request(
                                    &pairing,
                                    Transport {
                                        session: pending.session,
                                        sequence: copy_sequence,
                                    },
                                    mode,
                                    pending.id,
                                    pending.actor.clone(),
                                    pending.operation.clone(),
                                )
                            };
                            match encoded {
                                Ok(bytes) => {
                                    if pending.copy_digests.len() < usize::from(REQUEST_COPIES) {
                                        pending.copy_digests.push(wire::contract_digest(&bytes));
                                    }
                                    pending.wire = Some(bytes);
                                }
                                Err(_) => {
                                    finish_pending(
                                        pending.id,
                                        "denied",
                                        false,
                                        Some("Primary control contract is unavailable.".into()),
                                    );
                                    continue;
                                }
                            }
                        }
                        if mesh
                            .lock()
                            .unwrap()
                            .queue_command_proxy(pending.wire.as_ref().unwrap(), now)
                        {
                            pending.attempts += 1;
                            pending.last_sent = Some(now);
                        }
                        let mut broker = broker().lock().unwrap();
                        if broker
                            .pending
                            .as_ref()
                            .is_some_and(|original| original.id == pending.id)
                        {
                            broker.pending = Some(pending);
                        }
                    }
                }
            }
            if role == Role::Primary {
                let boot = runtime.admission.boot();
                outbox.retain(|(created_boot, _)| *created_boot == boot);
            }
            if let Some((_, bytes)) = outbox.front()
                && mesh.lock().unwrap().queue_command_proxy(bytes, now) {
                    outbox.pop_front();
                }
        }
        invalidate_presentation(&state);
        let id = broker()
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .map(|pending| pending.id);
        if let Some(id) = id {
            finish_pending(
                id,
                "unknown",
                false,
                Some("Station shut down before primary confirmation.".into()),
            );
        }
        if let Some(retained) = retained {retained.shutdown().await;}
    }))
}

// Application protocol catalog, independent of this executable's feature set.
// Append only: bit positions are fixed for the host control protocol version.
pub(crate) fn command_catalog() -> &'static [&'static str] {
    &[
        "Postinit",
        "Launch",
        "Dump",
        "Abort",
        "NormallyOpen",
        "Pilot",
        "Igniter",
        "IgniterSequence",
        "RetractPlumbing",
        "Nitrogen",
        "NitrogenClose",
        "Nitrous",
        "NitrousClose",
        "ResetSim",
        "ContinueFillSequence",
        "MonitorAltitude",
        "RevokeMonitorAltitude",
        "ConsecutiveSamples",
        "RevokeConsecutiveSamples",
        "ResetFailures",
        "RevokeResetFailures",
        "ValidateMeasms",
        "RevokeValidateMeasms",
        "GroundStationLaunch",
        "ToggleButtonInterlock",
        "ToggleGroundStationControl",
        "ToggleLaunchInterlock",
        "TogglePhysicalLaunchMode",
        "ToggleAvBayUnderglow",
        "ToggleFlightBuzzer",
        "ResetLaunchLatch",
        "ResetFlightState",
        "DeployParachute",
        "ExpandParachute",
        "EvaluationRelax",
        "EvaluationFocus",
        "EvaluationAbort",
        "ReinitSensors",
        "ReinitBarometer",
        "EnableIMU",
        "DisableIMU",
        "AdvanceFlightState",
        "RewindFlightState",
        "AbortAfter100",
        "AbortAfter40",
        "AbortAfter250",
        "ReinitAfter15",
        "ReinitAfter30",
        "ReinitAfter50",
        "StartFill",
        "PauseFill",
        "CancelFill",
        "ValveSelfTest",
        "NitrogenTest",
        "Turbocharge",
        "TogglePlumbingDelayInterlock",
        "ToggleChecklistInterlock",
        "ToggleSupplyInterlock",
        "ToggleBoardStateProofInterlock",
        "ClearGseAbort",
    ]
}
/// Proxy-only operations: catalog entries that are primary HTTP actions rather
/// than telemetry commands. Each runs the same primary-side checks as its
/// local endpoint and is authorized by the command that endpoint checks.
pub(crate) fn proxy_only_operation(name: &str) -> Option<&'static str> {
    match name {
        // `/api/gse/abort/clear` is authorized by ValveSelfTest.
        "ClearGseAbort" => Some("ValveSelfTest"),
        _ => None,
    }
}
/// The command name whose account permission authorizes `name`.
pub(crate) fn permission_name(name: &str) -> &str {
    proxy_only_operation(name).unwrap_or(name)
}
fn supported_commands_mask() -> u64 {
    command_catalog()
        .iter()
        .enumerate()
        .filter(|(_, name)| {
            proxy_only_operation(name).is_some()
                || serde_json::from_value::<crate::types::TelemetryCommand>(
                    serde_json::Value::String((**name).into()),
                )
                .is_ok()
        })
        .fold(0, |mask, (index, _)| mask | (1u64 << index))
}
fn supported_commands_mask_for_peer(launch_origin_supported: bool) -> u64 {
    let mut mask = supported_commands_mask();
    if !launch_origin_supported {
        for (index, name) in command_catalog().iter().enumerate() {
            if matches!(
                *name,
                "Launch" | "GroundStationLaunch" | "AdvanceFlightState" | "RewindFlightState"
            ) {
                mask &= !(1u64 << index);
            }
        }
    }
    mask
}

#[cfg(test)]
mod handler_tests;
#[cfg(test)]
mod proxy_freshness_tests;

#[cfg(test)]
mod recovery_wire_tests {
    use super::*;
    #[test]
    fn delayed_signed_request_and_cached_completion_are_revoked_by_barrier() {
        let (_root, p, r, mut primary, mut rr, pv, rv) = tests::connected();
        let bytes = rr
            .request_with_origins(
                &r,
                rv,
                Mode::Hitl,
                [71; 16],
                "operator".into(),
                Operation::Command("Launch".into()),
                None,
                Some(crate::primary_mirror::LaunchOrigin {
                    revision: 0,
                    boot: rr.capability.unwrap().0,
                    session: rv.session,
                }),
            )
            .unwrap();
        let Received::Execute(work) = primary.receive(&p, pv, Mode::Hitl, &bytes).unwrap() else {
            panic!("verified")
        };
        let barrier = primary.admission.begin().unwrap();
        assert!(barrier.is_quiescent());
        assert!(primary.receive(&p, pv, Mode::Hitl, &bytes).is_err());
        assert!(primary.finish(&p, work, Outcome::Queued).is_err());
        assert!(primary.results.is_empty());
        let query_during = rr.query(&r, rv).unwrap();
        let Received::Reply(cap_during) =
            primary.receive(&p, pv, Mode::Hitl, &query_during).unwrap()
        else {
            panic!("barrier cap")
        };
        rr.receive(&r, rv, Mode::Normal, &cap_during).unwrap();
        let during = rr
            .request_with_origins(
                &r,
                rv,
                Mode::Hitl,
                [73; 16],
                "operator".into(),
                Operation::Command("Launch".into()),
                None,
                Some(crate::primary_mirror::LaunchOrigin {
                    revision: 0,
                    boot: rr.capability.unwrap().0,
                    session: rv.session,
                }),
            )
            .unwrap();
        assert!(primary.receive(&p, pv, Mode::Hitl, &during).is_err());
        barrier.reopen_after_verified_recovery().unwrap();
        assert!(primary.receive(&p, pv, Mode::Hitl, &during).is_err());
        assert!(primary.receive(&p, pv, Mode::Hitl, &bytes).is_err());
        let query = rr.query(&r, rv).unwrap();
        let Received::Reply(cap) = primary.receive(&p, pv, Mode::Hitl, &query).unwrap() else {
            panic!("new cap")
        };
        rr.receive(&r, rv, Mode::Normal, &cap).unwrap();
        let fresh = rr
            .request_with_origins(
                &r,
                rv,
                Mode::Hitl,
                [72; 16],
                "operator".into(),
                Operation::Command("Launch".into()),
                None,
                Some(crate::primary_mirror::LaunchOrigin {
                    revision: 0,
                    boot: rr.capability.unwrap().0,
                    session: rv.session,
                }),
            )
            .unwrap();
        assert!(matches!(
            primary.receive(&p, pv, Mode::Hitl, &fresh).unwrap(),
            Received::Execute(_)
        ));
    }
}

#[cfg(test)]
mod board_abort_readiness_tests {
    use super::*;
    #[tokio::test]
    async fn board_abort_signed_observe_never_demands_and_refresh_is_deduped_rate_limited_and_session_bound()
     {
        let (_root, p, r, mut primary, mut rr, pv, rv) = super::tests::connected();
        let state = crate::state::tests::test_app_state().await;
        primary.observation = crate::primary_mirror::encode_control_observation(&state, 96);
        assert_eq!(primary.observation.len(), 89);
        let legacy = rr.query(&r, rv).unwrap();
        let Received::Reply(reply) = primary.receive(&p, pv, Mode::Normal, &legacy).unwrap() else {
            panic!("legacy reply")
        };
        let Received::Capability { observation, .. } =
            rr.receive(&r, rv, Mode::Normal, &reply).unwrap()
        else {
            panic!("legacy cap")
        };
        assert_eq!(observation.len(), 57);
        assert!(!primary.extended_observation);
        assert!(!primary.take_readiness_demand());
        let observe = rr.query_observe(&r, rv).unwrap();
        let Received::Reply(reply) = primary.receive(&p, pv, Mode::Normal, &observe).unwrap()
        else {
            panic!("observe reply")
        };
        let Received::Capability { observation, .. } =
            rr.receive(&r, rv, Mode::Normal, &reply).unwrap()
        else {
            panic!("observe cap")
        };
        assert_eq!(observation.len(), 89);
        assert!(primary.extended_observation);
        assert!(!primary.take_readiness_demand());
        let refresh = rr.query_for_readiness(&r, rv, true).unwrap();
        let mut unsigned = refresh.clone();
        *unsigned.last_mut().unwrap() ^= 1;
        assert!(primary.receive(&p, pv, Mode::Normal, &unsigned).is_err());
        assert!(!primary.take_readiness_demand());
        assert!(
            primary
                .receive(
                    &p,
                    Transport {
                        session: pv.session + 1,
                        ..pv
                    },
                    Mode::Normal,
                    &refresh
                )
                .is_err()
        );
        assert!(!primary.take_readiness_demand());
        let Received::Reply(reply) = primary.receive(&p, pv, Mode::Normal, &refresh).unwrap()
        else {
            panic!("refresh reply")
        };
        assert!(primary.take_readiness_demand());
        assert!(!primary.take_readiness_demand());
        assert!(primary.receive(&p, pv, Mode::Normal, &refresh).is_ok());
        assert!(
            !primary.take_readiness_demand(),
            "exact signed replay cannot schedule another batch"
        );
        rr.receive(&r, rv, Mode::Normal, &reply).unwrap();
        let another = rr.query_for_readiness(&r, rv, true).unwrap();
        primary.receive(&p, pv, Mode::Normal, &another).unwrap();
        assert!(
            !primary.take_readiness_demand(),
            "new nonce does not bypass five-second request budget"
        );
        let mode = rr.capability.unwrap().1;
        let view = rr.session;
        // An unsupported extension does not mutate the transport lease/capability.
        assert!(rr.receive(&r, rv, Mode::Normal, &unsigned).is_err());
        assert_eq!(rr.session, view);
        assert_eq!(rr.capability.unwrap().1, mode);
        primary.connect(Some(Transport {
            session: pv.session + 1,
            ..pv
        }));
        assert!(!primary.extended_observation);
        assert!(!primary.take_readiness_demand());
    }
    #[test]
    fn board_abort_legacy_peer_loses_only_origin_bound_launch_capabilities() {
        let full = supported_commands_mask();
        let legacy = supported_commands_mask_for_peer(false);
        assert_eq!(supported_commands_mask_for_peer(true), full);
        for (index, name) in command_catalog().iter().enumerate() {
            let bit = 1u64 << index;
            if matches!(
                *name,
                "Launch" | "GroundStationLaunch" | "AdvanceFlightState" | "RewindFlightState"
            ) {
                assert_eq!(legacy & bit, 0, "{name}");
            } else {
                assert_eq!(legacy & bit, full & bit, "{name}");
            }
        }
    }
    #[tokio::test]
    async fn board_abort_legacy_query_restores_same_session_legacy_reply_after_peer_restart() {
        let (_root, p, r, mut primary, mut rr, pv, rv) = super::tests::connected();
        let state = crate::state::tests::test_app_state().await;
        primary.observation = crate::primary_mirror::encode_control_observation(&state, 96);
        let observe = rr.query_observe(&r, rv).unwrap();
        let Received::Reply(reply) = primary.receive(&p, pv, Mode::Normal, &observe).unwrap()
        else {
            panic!("reply")
        };
        rr.receive(&r, rv, Mode::Normal, &reply).unwrap();
        assert!(primary.extended_observation);
        let old = rr.query(&r, rv).unwrap();
        let Received::Reply(reply) = primary.receive(&p, pv, Mode::Normal, &old).unwrap() else {
            panic!("reply")
        };
        let Received::Capability { observation, .. } =
            rr.receive(&r, rv, Mode::Normal, &reply).unwrap()
        else {
            panic!("cap")
        };
        assert_eq!(observation.len(), 57);
        assert!(!primary.extended_observation);
        assert!(!primary.take_readiness_demand());
        assert!(!crate::primary_mirror::observation_has_board_guard(
            &observation
        ));
        assert!(crate::primary_mirror::accept_control_observation(
            &state,
            &observation,
            0
        ));
        assert!(!crate::primary_mirror::launch_provenance_current(&state));
    }
}

#[cfg(test)]
mod cross_build_tests;

// Its tests still carry their own per-build cfgs (normal build only).
#[cfg(test)]
mod rrls_required_proof_tests;
