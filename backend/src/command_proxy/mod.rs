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
const DISCOVERY_COOLDOWN_MS: u64 = 30_000;
/// RRLS live view of the primary's control observation (fill boards
/// included). One signed QueryObserve and one Capability reply per period,
/// each at most MAX_WIRE bytes plus mesh framing: under 55 B/s worst case,
/// about a fifth of the 256 B/s settings allowance. Replies are bound to a
/// fresh nonce and their ages are source age plus the elapsed upper bound
/// since this query, so a lost or late reply only lets rows go stale.
pub(crate) const OBSERVE_REFRESH_MS: u64 = 10_000;
fn observe_refresh_due(now: u64, last: Option<u64>, link_idle_direct: bool, idle: bool) -> bool {
    link_idle_direct
        && idle
        && last.is_none_or(|last| now.checked_sub(last).is_none_or(|age| age >= OBSERVE_REFRESH_MS))
}
fn discovery_retry_due(now: u64, last: Option<u64>, attempts: u8) -> bool {
    attempts >= 3
        && last
            .and_then(|last| now.checked_sub(last))
            .is_some_and(|age| age >= DISCOVERY_COOLDOWN_MS)
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
            self.session = session;
            self.query = None;
            self.capability = None;
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
        ensure!(
            transport.accepts(&envelope, primary) || own_late_reply,
            "Stale primary connection contract"
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
                ensure!(
                    primary && envelope.boot == self.boot && mode == local_mode,
                    "Wrong primary boot or mode"
                );
                let manual_stop = matches!(&operation, Operation::Command(name) if name == "Abort");
                ensure!(
                    self.admission.request_allowed(envelope.boot, manual_stop),
                    "Primary recovery is pending"
                );
                let digest = wire::contract_digest(bytes);
                if let Some(previous) = self
                    .results
                    .iter()
                    .find(|item| item.request == envelope.request)
                {
                    ensure!(
                        previous.digest == digest,
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
        ensure!(
            request.envelope.boot == self.boot,
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
        assert!(observe_refresh_due(0, None, true, true));
        assert!(!observe_refresh_due(0, None, false, true));
        assert!(!observe_refresh_due(0, None, true, false));
        assert!(!observe_refresh_due(OBSERVE_REFRESH_MS - 1, Some(0), true, true));
        assert!(observe_refresh_due(OBSERVE_REFRESH_MS, Some(0), true, true));
        // A clock that moved backwards restarts the period instead of stalling.
        assert!(observe_refresh_due(5, Some(10), true, true));
        // Query plus reply, each a full frame with mesh framing, per period.
        let worst_bytes_per_s = 2 * (MAX_WIRE as u64 + 16) * 1000 / OBSERVE_REFRESH_MS;
        assert!(worst_bytes_per_s <= 256 / 4, "{worst_bytes_per_s} B/s");
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
    fn capability_recovery_uses_bounded_bursts_and_does_not_reinterpret_clock_rollback() {
        assert!(!discovery_retry_due(30_000, Some(0), 2));
        assert!(!discovery_retry_due(29_999, Some(0), 3));
        assert!(discovery_retry_due(30_000, Some(0), 3));
        assert!(!discovery_retry_due(0, Some(30_000), 3));
        assert!(!discovery_retry_due(u64::MAX, None, 3));
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
            | "StartWritingNow"
            | "StartWritingLastTwoMinutes"
            | "PauseWritingDb"
            | "StopWritingDb"
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
        mesh.command_proxy_transport(mesh.now())
            .is_some_and(|view| view.accepts(&request.envelope, true))
            && matches!(request.envelope.message,Message::Request{mode,..} if mode==crate::station_mode::local_mode())
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
        status.reason = "RRLS takeover: the primary GroundStation is out of range, so this RRLS commands the AV bay directly. Authority returns to the primary automatically once it is heard again.".into();
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
        state.manual_abort_intent.request();
        let mut broker = broker().lock().unwrap();
        broker.status.last = Some(RequestStatus {
            request_id: hex(&nonce()),
            operation: "Abort".into(),
            phase: "local_stop_retained",
            primary_accepted: false,
            device_confirmed: false,
            error: Some("Primary observation and actual board readback are pending.".into()),
        });
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
                if runtime.capability.is_none()
                    && discovery_retry_due(now, last_query, query_attempts)
                {
                    // A brief control-only loss must not permanently disable a
                    // healthy connection. Retry bounded discovery, never an old command.
                    runtime.query = None;
                    query_attempts = 0;
                    last_query = None;
                    query_created_at = None;
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
                    broker.status.reason=if available {"Requests are checked by the primary; device confirmation still requires actual telemetry."} else if runtime.capability.is_none() && query_attempts >= 3 && last_query.is_some_and(|last| now.saturating_sub(last) >= 500) {"The primary did not provide a signed control capability; bounded discovery will retry after its cooldown."} else {"Waiting for signed primary control capability and matching current mode."}.into();
                }
                if (runtime.capability.is_none() || matches!(readiness_phase, 1 | 3 | 4))
                    && readiness_phase != 2
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
                let idle = runtime.capability.is_some()
                    && readiness_reply.is_none()
                    && !matches!(readiness_phase, 1..=3)
                    && last_query != Some(now)
                    && broker().lock().unwrap().pending.is_none();
                let link_idle_direct = mesh.lock().unwrap().observation_refresh_allowed(now);
                if observe_refresh_due(now, last_observe_refresh, link_idle_direct, idle) {
                    last_observe_refresh = Some(now);
                    // Fresh nonce: an unanswered earlier query cannot be replayed.
                    runtime.query = None;
                    if let Ok(query) = runtime.query_observe(&pairing, view) {
                        if mesh.lock().unwrap().queue_command_proxy(&query, now) {
                            query_created_at = Some(now);
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
                    // At most three copies; the result may still arrive until the
                    // request's own timeout (LATE_REPLY_WINDOW), since a relayed
                    // round trip can outlast the sequence window.
                    if !available
                        || pending.session != view.session
                        || sequence_age > LATE_REPLY_WINDOW
                    {
                        finish_pending(pending.id,"unknown",false,Some("Primary response was not confirmed; inspect actual state before retrying.".into()));
                        invalidate_presentation(&state);
                        runtime.capability = None;
                        runtime.query = None;
                        query_attempts = 0;
                        last_query = None;
                        query_created_at = None;
                    } else if pending.attempts < 3
                        && sequence_age <= SEQUENCE_WINDOW
                        && pending
                            .last_sent
                            .is_none_or(|last| now >= last && now - last >= 500)
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
                        if pending.wire.is_none() {
                            let Some(mode) = mode.effective_mode else {
                                continue;
                            };
                            let encoded = if pending.plumbing_origin_utc_ms.is_some() {
                                runtime.request_with_origins(
                                    &pairing,
                                    Transport {
                                        session: pending.session,
                                        sequence: pending.sequence,
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
                                        sequence: pending.sequence,
                                    },
                                    mode,
                                    pending.id,
                                    pending.actor.clone(),
                                    pending.operation.clone(),
                                )
                            };
                            match encoded {
                                Ok(bytes) => pending.wire = Some(bytes),
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
        assert_eq!(primary.observation.len(), 88);
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
        assert_eq!(observation.len(), 88);
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
