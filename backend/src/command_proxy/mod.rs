//! Primary-owned control from an authenticated recovery station.
//! Local RRLS hardware execution remains denied, including when offline.
mod wire;
use crate::{rrls::Role, station_mode::Mode, station_pairing::Pairing};
use anyhow::{Result, ensure};
use std::collections::VecDeque;
pub(crate) use wire::Operation;
use wire::{Envelope, Message, Outcome};

const MAX_RESULTS: usize = 64;
const SEQUENCE_WINDOW: u32 = 4;
const DISCOVERY_COOLDOWN_MS: u64 = 30_000;
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
struct Cached {
    request: [u8; 16],
    digest: [u8; 16],
    sequence: u32,
    reply: Option<Vec<u8>>,
}
#[derive(Clone)]
struct VerifiedRequest {
    envelope: Envelope,
    digest: [u8; 16],
    pub actor: String,
    pub operation: Operation,
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
        request: [u8; 16],
        digest: [u8; 16],
        outcome: Outcome,
        observation: Vec<u8>,
    },
    Pending,
}
struct Runtime {
    role: Role,
    boot: [u8; 16],
    session: Option<u32>,
    query: Option<[u8; 16]>,
    capability: Option<([u8; 16], Mode, u64)>,
    results: VecDeque<Cached>,
    observation: Vec<u8>,
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
        Self {
            role,
            boot: nonce(),
            session: None,
            query: None,
            capability: None,
            results: VecDeque::new(),
            observation: Vec::new(),
        }
    }
    pub(crate) fn connect(&mut self, transport: Option<Transport>) {
        let session = transport.map(|view| view.session);
        if self.session != session {
            if self.role == Role::Primary {
                self.boot = nonce();
            }
            self.session = session;
            self.query = None;
            self.capability = None;
            self.results.clear();
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
                message: Message::Query,
            },
        )
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
        ensure!(
            self.role == Role::Rrls && self.session == Some(transport.session),
            "Only connected recovery requests primary control"
        );
        let (boot, primary_mode, _) = self
            .capability
            .ok_or_else(|| anyhow::anyhow!("Primary capability is not verified"))?;
        ensure!(primary_mode == mode, "Primary mode changed");
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
                    mode,
                    actor,
                    operation,
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
        ensure!(
            self.session == Some(transport.session),
            "Disconnected control transport"
        );
        let primary = self.role == Role::Primary;
        let envelope = wire::open(pairing, bytes, if primary { "rrls" } else { "primary" })?;
        ensure!(
            transport.accepts(&envelope, primary),
            "Stale primary connection contract"
        );
        match envelope.message.clone() {
            Message::Query => {
                ensure!(
                    primary && envelope.boot == [0; 16],
                    "Unexpected control query"
                );
                let reply = Envelope {
                    issuer: pairing.public.station_id.clone(),
                    boot: self.boot,
                    message: Message::Capability {
                        mode: local_mode,
                        commands: supported_commands_mask(),
                        observation: self.observation.clone(),
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
                self.capability = Some((envelope.boot, mode, commands));
                Ok(Received::Capability {
                    boot: envelope.boot,
                    mode,
                    observation,
                })
            }
            Message::Request {
                mode,
                actor,
                operation,
            } => {
                ensure!(
                    primary && envelope.boot == self.boot && mode == local_mode,
                    "Wrong primary boot or mode"
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
                    envelope,
                    digest,
                    actor,
                    operation,
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
                Ok(Received::Result {
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
        let reply = Envelope {
            issuer: pairing.public.station_id.clone(),
            message: Message::Reply {
                digest: request.digest,
                outcome,
                observation: self.observation.clone(),
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
    fn connected() -> (
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
        #[cfg(not(feature = "hitl_mode"))]
        assert!(
            serde_json::from_str::<crate::types::TelemetryCommand>("\"DeployParachute\"").is_err(),
            "normal RRLS must not need a local HITL command variant"
        );
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
async fn execute_primary(
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
    let status = match &request.operation {
        Operation::Command(name) => {
            if local_operation(name) || !principal.allows_command_name(name) {
                return Outcome::Denied;
            }
            let Ok(command) = serde_json::from_value::<crate::types::TelemetryCommand>(
                serde_json::Value::String(name.clone()),
            ) else {
                return Outcome::Invalid;
            };
            crate::web::dispatch_proxied_command(state, principal, command)
                .await
                .0
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
}
impl Default for Status {
    fn default() -> Self {
        Self{supported:true,paired:false,connected:false,available:false,primary_mode:None,supported_commands:Vec::new(),pending:None,last:None,reason:"Paired primary control has not initialized; standalone hardware authority is unavailable.".into()}
    }
}
// This includes the original local credential only in process memory. It is
// never serialized, signed into radio bytes, debug-formatted, or logged.
#[derive(Clone)]
struct Pending {
    id: [u8; 16],
    actor: String,
    token: String,
    operation: Operation,
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
}
static BROKER: OnceLock<Mutex<Broker>> = OnceLock::new();
fn broker() -> &'static Mutex<Broker> {
    BROKER.get_or_init(Default::default)
}
pub(crate) fn status() -> Status {
    broker().lock().unwrap().status.clone()
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
    use crate::auth::Permission;
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
    if let Operation::Command(name) = &operation {
        if !wire::valid_command(name)
            || local_operation(name)
            || !principal.allows_command_name(name)
        {
            return (
                StatusCode::FORBIDDEN,
                "Command is not available through the primary proxy.",
            )
                .into_response();
        }
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
    if let Operation::Command(name) = &operation {
        if !fresh.allows_command_name(name) {
            return (StatusCode::FORBIDDEN, "Command permission was revoked.").into_response();
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
    let mut broker = broker().lock().unwrap();
    if !broker.status.available {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            broker.status.reason.clone(),
        )
            .into_response();
    }
    if let Operation::Command(name) = &operation {
        if !broker
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
fn accept_presentation(state: &crate::state::AppState, bytes: &[u8], age_ms: u64) -> bool {
    let accepted = crate::primary_mirror::accept_control_observation(state, bytes, age_ms);
    if accepted {
        state.broadcast_action_policy_snapshot();
    }
    accepted
}
fn invalidate_presentation(state: &crate::state::AppState) {
    crate::primary_mirror::invalidate_control_observation();
    if crate::rrls::is_rrls() {
        state.broadcast_action_policy_snapshot();
    }
}
pub(crate) fn start(state: Arc<crate::state::AppState>) -> Option<tokio::task::JoinHandle<()>> {
    let pairing = match Pairing::load() {
        Ok(Some(pairing)) => pairing,
        Ok(None) => {
            broker().lock().unwrap().status.reason="Configure existing station pairing on both stations for authenticated primary commands. Standalone hardware authority is unavailable.".into();
            return None;
        }
        Err(_) => {
            broker().lock().unwrap().status.reason =
                "Station pairing is invalid; primary commands are disabled.".into();
            return None;
        }
    };
    let Some(mesh) = crate::rrls::settings_transport() else {
        let mut broker = broker().lock().unwrap();
        broker.status.paired = true;
        broker.status.reason =
            "Paired radio transport is unavailable; no command can be forwarded.".into();
        return None;
    };
    let role = { mesh.lock().unwrap().role };
    let mut runtime = Runtime::new(role);
    if runtime.peer(&pairing).is_err() {
        let mut broker = broker().lock().unwrap();
        broker.status.paired = true;
        broker.status.reason = "Authenticated primary control requires exactly one paired counterparty of the expected role.".into();
        return None;
    }
    let mut shutdown = state.shutdown_subscribe();
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(20));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut outbox = VecDeque::<Vec<u8>>::new();
        let mut query_attempts = 0u8;
        let mut last_query = None;
        let mut query_created_at = None;
        let mut previous_session = None;
        loop {
            tokio::select! {_=shutdown.recv()=>break,_=tick.tick()=>{}}
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
            let Some(view) = view else {
                let mut broker = broker().lock().unwrap();
                broker.status.paired = true;
                broker.status.connected = false;
                broker.status.available = false;
                broker.status.reason =
                    "Primary disconnected; standalone hardware authority is unavailable.".into();
                continue;
            };
            for bytes in frames {
                if role == Role::Primary {
                    runtime.observation =
                        crate::primary_mirror::encode_control_observation(&state, 96);
                }
                match runtime.receive(&pairing, view, crate::station_mode::local_mode(), &bytes) {
                    Ok(Received::Reply(bytes)) => {
                        if outbox.len() < 4 {
                            outbox.push_back(bytes);
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
                        if let Ok(reply) = runtime.finish(&pairing, request, outcome) {
                            if outbox.len() < 4 {
                                outbox.push_back(reply);
                            }
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
                            );
                        }
                        debug_assert_ne!(boot, [0; 16]);
                        broker().lock().unwrap().status.primary_mode = Some(mode);
                    }
                    Ok(Received::Result {
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
                            );
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
                    Err(_) => {} // Invalid metadata never authorizes or refreshes anything.
                }
            }
            if role == Role::Rrls {
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
                if runtime.capability.is_none()
                    && query_attempts < 3
                    && last_query.is_none_or(|last| now >= last && now - last >= 500)
                {
                    if let Ok(query) = runtime.query(&pairing, view) {
                        if mesh.lock().unwrap().queue_command_proxy(&query, now) {
                            query_created_at.get_or_insert(now);
                            query_attempts += 1;
                            last_query = Some(now);
                        }
                    }
                }
                let pending = broker().lock().unwrap().pending.clone();
                if let Some(mut pending) = pending {
                    let sequence_age = view.sequence.wrapping_sub(pending.sequence);
                    if !available
                        || pending.session != view.session
                        || sequence_age > SEQUENCE_WINDOW
                        || pending.attempts >= 3
                            && pending
                                .last_sent
                                .is_some_and(|last| now >= last && now - last >= 500)
                    {
                        finish_pending(pending.id,"unknown",false,Some("Primary response was not confirmed; inspect actual state before retrying.".into()));
                        invalidate_presentation(&state);
                        runtime.capability = None;
                        runtime.query = None;
                        query_attempts = 0;
                        last_query = None;
                        query_created_at = None;
                    } else if pending
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
                            match runtime.request(
                                &pairing,
                                Transport {
                                    session: pending.session,
                                    sequence: pending.sequence,
                                },
                                mode,
                                pending.id,
                                pending.actor.clone(),
                                pending.operation.clone(),
                            ) {
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
            if let Some(bytes) = outbox.front() {
                if mesh.lock().unwrap().queue_command_proxy(bytes, now) {
                    outbox.pop_front();
                }
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
    ]
}
fn supported_commands_mask() -> u64 {
    command_catalog()
        .iter()
        .enumerate()
        .filter(|(_, name)| {
            serde_json::from_value::<crate::types::TelemetryCommand>(serde_json::Value::String(
                (**name).into(),
            ))
            .is_ok()
        })
        .fold(0, |mask, (index, _)| mask | (1u64 << index))
}
#[cfg(test)]
mod handler_tests;
