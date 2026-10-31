//! Bounded clock metadata synchronization over existing SETTINGS, never an SDK packet.
//! Signed installations require signed peers. Legacy display-only installations use
//! the current primary HEALTH session plus a fresh challenge, never control authority.
use crate::{launch_history::ClockRecord, state::AppState, station_pairing::Pairing};
use anyhow::{Result, ensure};
use ring::signature::{self, KeyPair};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
const SIGNED: &[u8] = b"TC\x03";
const DISPLAY: &[u8] = b"TC\x04";
const DOMAIN: &[u8] = b"gs-primary-launch-clock-v1\0";
const MAX_WIRE: usize = 1024;
const TTL: u64 = 30_000;
const RETRY: u64 = 5_000;
pub(crate) fn recognizes(wire: &[u8]) -> bool {
    wire.starts_with(SIGNED)
        || wire.starts_with(DISPLAY)
        || wire.starts_with(b"TC\x01")
        || wire.starts_with(b"TC\x02")
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Message {
    Query {
        nonce: String,
        ack_supported: bool,
    },
    Reply {
        nonce: String,
        clock: ClockRecord,
        ack_supported: bool,
    },
    Ack {
        authority: String,
        revision: u64,
    },
    Hint {
        authority: String,
        revision: u64,
    },
    ResetRequest {
        nonce: String,
        expected: ClockRecord,
    },
    ResetReply {
        nonce: String,
        clock: ClockRecord,
        error: Option<String>,
        durable: bool,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    installation: String,
    issuer: String,
    recipient: String,
    session: u32,
    message: Message,
}
fn seal(
    pairing: Option<&Pairing>,
    recipient: &str,
    session: u32,
    message: Message,
) -> Result<Vec<u8>> {
    let envelope = Envelope {
        installation: pairing.map_or(String::new(), |p| p.public.installation_id.clone()),
        issuer: pairing.map_or(String::new(), |p| p.public.station_id.clone()),
        recipient: recipient.into(),
        session,
        message,
    };
    let mut wire = if pairing.is_some() {
        SIGNED.to_vec()
    } else {
        DISPLAY.to_vec()
    };
    wire.extend(serde_json::to_vec(&envelope)?);
    if let Some(pairing) = pairing {
        wire.extend_from_slice(
            pairing
                .mission_signing_key()
                .sign(&[DOMAIN, wire.as_slice()].concat())
                .as_ref(),
        );
    }
    ensure!(wire.len() <= MAX_WIRE, "Oversized clock envelope");
    Ok(wire)
}
fn open(pairing: Option<&Pairing>, wire: &[u8]) -> Result<Envelope> {
    ensure!(wire.len() <= MAX_WIRE, "Oversized clock envelope");
    if let Some(pairing) = pairing {
        ensure!(
            wire.starts_with(SIGNED) && wire.len() >= SIGNED.len() + 64,
            "Unsigned clock in configured installation"
        );
        let end = wire.len() - 64;
        signature::UnparsedPublicKey::new(
            &signature::ED25519,
            pairing.mission_signing_key().public_key().as_ref(),
        )
        .verify(&[DOMAIN, &wire[..end]].concat(), &wire[end..])
        .map_err(|_| anyhow::anyhow!("Invalid clock signature"))?;
        let e: Envelope = serde_json::from_slice(&wire[SIGNED.len()..end])?;
        ensure!(
            e.installation == pairing.public.installation_id
                && e.recipient == pairing.public.station_id
                && pairing.public.peers.iter().any(|p| p.id == e.issuer),
            "Unpaired clock sender"
        );
        Ok(e)
    } else {
        ensure!(
            wire.starts_with(DISPLAY),
            "Signed clock requires configured trust"
        );
        let e: Envelope = serde_json::from_slice(&wire[DISPLAY.len()..])?;
        ensure!(
            e.installation.is_empty() && e.issuer.is_empty() && e.recipient.is_empty(),
            "Invalid legacy display identity"
        );
        Ok(e)
    }
}
enum Outcome {
    Clock(ClockRecord),
    ConfirmClock,
    Reset {
        nonce: String,
        expected: ClockRecord,
        issuer: String,
    },
}
#[derive(Clone, Default, Serialize)]
pub struct ResetStatus {
    pub pending: bool,
    pub expected_session: Option<String>,
    pub error: Option<String>,
}
struct ResetIntent {
    nonce: String,
    expected: ClockRecord,
    last_sent: Option<u64>,
    remaining: u8,
}
#[derive(Default)]
struct ResetBroker {
    intent: Option<ResetIntent>,
    result: Option<(String, Result<(), String>)>,
    status: ResetStatus,
}
static RESET: std::sync::OnceLock<std::sync::Mutex<ResetBroker>> = std::sync::OnceLock::new();
fn broker() -> &'static std::sync::Mutex<ResetBroker> {
    RESET.get_or_init(Default::default)
}
pub fn reset_status() -> ResetStatus {
    broker().lock().unwrap().status.clone()
}
/// Authenticated HTTP caller only. A queued request never clears the local clock.
pub async fn request_reset(state: &Arc<AppState>) -> Result<bool> {
    let expected = state.launch_history.lock().unwrap().snapshot();
    let nonce = queue_reset(expected)?;
    await_reset_nonce(nonce).await
}
pub async fn await_existing_reset() -> Result<bool> {
    let nonce = {
        let broker = broker().lock().unwrap();
        broker
            .intent
            .as_ref()
            .map(|intent| intent.nonce.clone())
            .or_else(|| broker.result.as_ref().map(|(nonce, _)| nonce.clone()))
            .ok_or_else(|| anyhow::anyhow!("No primary reset request is pending"))?
    };
    await_reset_nonce(nonce).await
}
async fn await_reset_nonce(nonce: String) -> Result<bool> {
    // A finite HTTP wait. Offline state stays visibly pending; it is not an Idle ACK.
    for _ in 0..100 {
        {
            let broker = broker().lock().unwrap();
            if let Some((id, result)) = &broker.result
                && id == &nonce
            {
                return result
                    .as_ref()
                    .map(|_| true)
                    .map_err(|error| anyhow::anyhow!(error.clone()));
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(false)
}

fn queue_reset(expected: ClockRecord) -> Result<String> {
    let nonce = {
        let mut broker = broker().lock().unwrap();
        if let Some(intent) = &broker.intent {
            ensure!(
                intent.expected == expected,
                "An older clock reset is still pending"
            );
            intent.nonce.clone()
        } else {
            let nonce = format!("{:032x}", rand::random::<u128>());
            broker.status = ResetStatus {
                pending: true,
                expected_session: Some(expected.session.clone()),
                error: None,
            };
            broker.intent = Some(ResetIntent {
                nonce: nonce.clone(),
                expected,
                last_sent: None,
                remaining: 3,
            });
            broker.result = None;
            nonce
        }
    };
    Ok(nonce)
}
fn complete_reset(nonce: &str, error: Option<String>) -> bool {
    let mut broker = broker().lock().unwrap();
    if broker
        .intent
        .as_ref()
        .is_none_or(|intent| intent.nonce != nonce)
    {
        return false;
    }
    broker.intent = None;
    broker.status.pending = false;
    broker.status.error = error.clone();
    broker.result = Some((nonce.into(), error.map_or(Ok(()), Err)));
    true
}
struct Runtime {
    observer: bool,
    primary_durable: bool,
    primary: Option<String>,
    session: Option<u32>,
    connected: bool,
    pending: Option<(String, u64)>,
    remaining: u8,
    last_sent: Option<u64>,
    hint: Option<(String, u64)>,
    hint_remaining: u8,
    hint_sent: Option<u64>,
    hint_attempts: u8,
    peer_ack_supported: bool,
    confirmed: bool,
    ack: Option<Vec<u8>>,
    reply: Option<Vec<u8>>,
}
impl Runtime {
    fn new(pairing: Option<&Pairing>, observer: bool) -> Result<Self> {
        let primaries = pairing
            .map(|p| {
                p.public
                    .peers
                    .iter()
                    .filter(|p| p.role == "primary")
                    .map(|p| p.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        ensure!(
            !observer || pairing.is_none() || primaries.len() == 1,
            "Exactly one primary clock authority must be paired"
        );
        Ok(Self {
            observer,
            primary_durable: true,
            primary: primaries.first().cloned(),
            session: None,
            connected: false,
            pending: None,
            remaining: 0,
            last_sent: None,
            hint: None,
            hint_remaining: 0,
            hint_sent: None,
            hint_attempts: 0,
            peer_ack_supported: false,
            confirmed: false,
            ack: None,
            reply: None,
        })
    }
    fn connect(&mut self, connected: bool, session: Option<u32>) {
        if (self.observer && self.connected != connected) || self.session != session {
            self.pending = None;
            self.remaining = if connected && self.observer { 3 } else { 0 };
            self.last_sent = None;
            self.reply = None;
            self.hint_sent = None;
            self.hint = None;
            self.hint_remaining = 3;
            self.hint_attempts = 0;
            self.ack = None;
            self.peer_ack_supported = false;
            self.confirmed = false;
        }
        self.connected = connected;
        self.session = session;
    }
    fn verification_pending(&self) -> bool {
        self.observer && (!self.connected || self.pending.is_some() || self.remaining > 0)
    }
    fn receive(
        &mut self,
        pairing: Option<&Pairing>,
        wire: &[u8],
        now: u64,
        current: &ClockRecord,
    ) -> Result<Option<Outcome>> {
        let e = open(pairing, wire)?;
        ensure!(
            self.session == Some(e.session),
            "Clock belongs to an old HEALTH session"
        );
        if self.observer {
            ensure!(
                self.connected,
                "Clock reply without current primary connection"
            );
            if pairing.is_some() {
                ensure!(
                    Some(&e.issuer) == self.primary.as_ref(),
                    "Clock publisher is not primary"
                );
            }
            match e.message {
                Message::Reply {
                    nonce,
                    clock,
                    ack_supported,
                } => {
                    ensure!(
                        ack_supported,
                        "Clock peer does not support revision acknowledgements"
                    );
                    ensure!(
                        self.pending
                            .as_ref()
                            .is_some_and(|(n, at)| n == &nonce && now >= *at && now - *at < TTL),
                        "Stale/reordered clock challenge"
                    );
                    clock.validate()?;
                    ensure!(
                        clock.authority != current.authority || clock.revision >= current.revision,
                        "Older clock revision"
                    );
                    ensure!(
                        clock.authority != current.authority
                            || clock.revision != current.revision
                            || &clock == current,
                        "Conflicting clock revision"
                    );
                    self.pending = None;
                    self.remaining = 0;
                    Ok(Some(Outcome::Clock(clock)))
                }
                Message::Hint {
                    authority,
                    revision,
                } => {
                    ensure!(
                        authority.len() == 32
                            && authority.bytes().all(|byte| byte.is_ascii_hexdigit()),
                        "Invalid clock hint authority"
                    );
                    if authority == current.authority && revision == current.revision {
                        return Ok(Some(Outcome::ConfirmClock));
                    }
                    let expired = self.remaining == 0
                        && self
                            .pending
                            .as_ref()
                            .is_none_or(|(_, at)| now >= *at && now - *at >= TTL);
                    if (self.hint.as_ref() != Some(&(authority.clone(), revision)) || expired)
                        && (authority != current.authority || revision > current.revision)
                    {
                        self.hint = Some((authority, revision));
                        self.remaining = 3;
                        self.pending = None;
                        self.last_sent = None;
                    }
                    Ok(None)
                }
                Message::ResetReply {
                    nonce,
                    clock,
                    error,
                    durable,
                } => {
                    clock.validate()?;
                    let broker = broker().lock().unwrap();
                    ensure!(
                        broker
                            .intent
                            .as_ref()
                            .is_some_and(|intent| intent.nonce == nonce
                                && intent
                                    .last_sent
                                    .is_some_and(|at| now >= at && now - at < TTL)),
                        "Unrequested/stale reset reply"
                    );
                    drop(broker);
                    ensure!(
                        clock.authority != current.authority || clock.revision >= current.revision,
                        "Older reset reply"
                    );
                    ensure!(
                        clock.authority != current.authority
                            || clock.revision != current.revision
                            || &clock == current,
                        "Conflicting reset reply"
                    );
                    // Apply the current primary snapshot even when its rejection says
                    // this expected session was already superseded. Never retry it on a new launch.
                    complete_reset(
                        &nonce,
                        error.or_else(|| {
                            (!durable).then(|| "Primary reset has not been saved".into())
                        }),
                    );
                    Ok(durable.then_some(Outcome::Clock(clock)))
                }
                Message::Query { .. } | Message::ResetRequest { .. } | Message::Ack { .. } => {
                    anyhow::bail!("Observer cannot author primary clock")
                }
            }
        } else {
            if let Some(pairing) = pairing {
                ensure!(
                    pairing
                        .public
                        .peers
                        .iter()
                        .any(|p| p.id == e.issuer && p.role == "rrls"),
                    "Only paired observer may query clock"
                );
            }
            match e.message {
                Message::Query {
                    nonce,
                    ack_supported,
                } => {
                    ensure!(
                        self.primary_durable,
                        "Primary clock journal is not durable; query deferred"
                    );
                    ensure!(
                        valid_nonce(&nonce) && ack_supported,
                        "Clock query must negotiate revision acknowledgements"
                    );
                    self.peer_ack_supported = true;
                    self.confirmed = false;
                    self.reply = Some(seal(
                        pairing,
                        &e.issuer,
                        e.session,
                        Message::Reply {
                            nonce,
                            clock: current.clone(),
                            ack_supported: true,
                        },
                    )?);
                    Ok(None)
                }
                Message::Ack {
                    authority,
                    revision,
                } => {
                    ensure!(
                        self.peer_ack_supported,
                        "Clock acknowledgement capability was not negotiated"
                    );
                    ensure!(
                        authority == current.authority && revision == current.revision,
                        "Stale clock revision acknowledgement"
                    );
                    self.peer_ack_supported = true;
                    self.hint = Some((authority, revision));
                    self.confirmed = true;
                    self.hint_remaining = 0;
                    Ok(None)
                }
                Message::ResetRequest { nonce, expected } => {
                    ensure!(valid_nonce(&nonce), "Invalid reset challenge");
                    expected.validate()?;
                    Ok(Some(Outcome::Reset {
                        nonce,
                        expected,
                        issuer: e.issuer,
                    }))
                }
                _ => anyhow::bail!("Primary clock cannot be overwritten remotely"),
            }
        }
    }
    fn acknowledge(&mut self, pairing: Option<&Pairing>, clock: &ClockRecord) -> Result<()> {
        let session = self
            .session
            .ok_or_else(|| anyhow::anyhow!("No current primary HEALTH session"))?;
        self.ack = Some(seal(
            pairing,
            self.primary.as_deref().unwrap_or(""),
            session,
            Message::Ack {
                authority: clock.authority.clone(),
                revision: clock.revision,
            },
        )?);
        Ok(())
    }
    fn offer_reset(
        &mut self,
        pairing: Option<&Pairing>,
        current: &ClockRecord,
        now: u64,
        mut send: impl FnMut(&[u8]) -> bool,
    ) -> Result<()> {
        if !self.observer || !self.connected {
            return Ok(());
        }
        let Some(session) = self.session else {
            return Ok(());
        };
        let mut broker = broker().lock().unwrap();
        let Some(intent) = broker.intent.as_mut() else {
            return Ok(());
        };
        if &intent.expected != current {
            let nonce = intent.nonce.clone();
            drop(broker);
            complete_reset(&nonce,Some("Clock changed while reset was pending; request was not sent to the new session".into()));
            return Ok(());
        }
        if intent.remaining == 0
            && intent
                .last_sent
                .is_some_and(|last| now >= last && now - last >= TTL)
        {
            let nonce = intent.nonce.clone();
            drop(broker);
            complete_reset(
                &nonce,
                Some(
                    "Primary reset was not confirmed; clock retained. Retry when connected.".into(),
                ),
            );
            return Ok(());
        }
        if intent.remaining > 0
            && intent
                .last_sent
                .is_none_or(|last| now >= last && now - last >= RETRY)
        {
            let wire = seal(
                pairing,
                self.primary.as_deref().unwrap_or(""),
                session,
                Message::ResetRequest {
                    nonce: intent.nonce.clone(),
                    expected: intent.expected.clone(),
                },
            )?;
            if send(&wire) {
                intent.remaining -= 1;
                intent.last_sent = Some(now);
            }
        }
        Ok(())
    }
    fn offer(
        &mut self,
        pairing: Option<&Pairing>,
        current: &ClockRecord,
        now: u64,
        mut send: impl FnMut(bool, &[u8]) -> bool,
    ) -> Result<()> {
        if let Some(reply) = &self.reply
            && send(true, reply)
        {
            self.reply = None;
        }
        let Some(session) = self.session else {
            return Ok(());
        };
        if !self.connected && (self.observer || !self.peer_ack_supported) {
            return Ok(());
        }
        if self.observer {
            if let Some(ack) = &self.ack {
                if send(false, ack) {
                    self.ack = None;
                } else {
                    return Ok(());
                }
            }
            if self.remaining > 0
                && self
                    .last_sent
                    .is_none_or(|last| now >= last && now - last >= RETRY)
            {
                let nonce = format!("{:032x}", rand::random::<u128>());
                let wire = seal(
                    pairing,
                    self.primary.as_deref().unwrap_or(""),
                    session,
                    Message::Query {
                        nonce: nonce.clone(),
                        ack_supported: true,
                    },
                )?;
                if send(false, &wire) {
                    self.pending = Some((nonce, now));
                    self.last_sent = Some(now);
                    self.remaining -= 1;
                }
            }
        } else {
            if !self.primary_durable {
                return Ok(());
            }
            let revision = (current.authority.clone(), current.revision);
            if self.hint.as_ref() != Some(&revision) {
                self.hint = Some(revision);
                self.hint_remaining = 3;
                self.hint_sent = None;
                self.hint_attempts = 0;
                self.confirmed = false;
            }
            if self.confirmed {
                return Ok(());
            }
            let interval = RETRY
                .saturating_mul(1u64 << self.hint_attempts.saturating_sub(1).min(3))
                .min(TTL);
            if (self.peer_ack_supported || self.hint_remaining > 0)
                && self
                    .hint_sent
                    .is_none_or(|last| now >= last && now - last >= interval)
            {
                let recipient = pairing
                    .and_then(|p| p.public.peers.iter().find(|p| p.role == "rrls"))
                    .map_or("", |p| p.id.as_str());
                let wire = seal(
                    pairing,
                    recipient,
                    session,
                    Message::Hint {
                        authority: current.authority.clone(),
                        revision: current.revision,
                    },
                )?;
                if send(self.peer_ack_supported && !self.connected, &wire) {
                    self.hint_remaining = self.hint_remaining.saturating_sub(1);
                    self.hint_attempts = self.hint_attempts.saturating_add(1);
                    self.hint_sent = Some(now);
                }
            }
        }
        Ok(())
    }
}
fn valid_nonce(nonce: &str) -> bool {
    nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}
async fn acknowledge_current(
    state: &Arc<AppState>,
    runtime: &mut Runtime,
    pairing: Option<&Pairing>,
) -> Result<()> {
    let (clock, waiter) = {
        let mut history = state.launch_history.lock().unwrap();
        ensure!(history.clock_known(), "Clock T0 is unknown");
        history.retry_save()?;
        (history.snapshot(), history.waiter())
    };
    crate::launch_history::flush(waiter).await?;
    runtime.acknowledge(pairing, &clock)?;
    state.launch_history.lock().unwrap().synchronized = true;
    Ok(())
}
pub fn start(state: Arc<AppState>) -> Option<tokio::task::JoinHandle<()>> {
    let pairing = match Pairing::load() {
        Ok(p) => p,
        Err(error) => {
            state.add_notification(format!(
                "Clock synchronization disabled: configured pairing failed: {error}"
            ));
            return None;
        }
    };
    let mut runtime = match Runtime::new(pairing.as_ref(), crate::rrls::is_rrls()) {
        Ok(r) => r,
        Err(error) => {
            state.add_notification(format!("Clock synchronization disabled: {error}"));
            return None;
        }
    };
    let mesh = crate::rrls::settings_transport()?;
    let mut shutdown = state.shutdown_subscribe();
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {_=shutdown.recv()=>break,_=interval.tick()=>{}}
            let (now, connected, session, frames) = {
                let mut mesh = mesh.lock().unwrap();
                let now = mesh.now();
                (
                    now,
                    mesh.settings_available(now),
                    mesh.operating_mode_session(now),
                    (0..4)
                        .filter_map(|_| mesh.take_launch_clock())
                        .collect::<Vec<_>>(),
                )
            };
            let reconnected =
                runtime.observer && connected && (!runtime.connected || runtime.session != session);
            runtime.connect(connected, session);
            if reconnected && let Some(intent) = broker().lock().unwrap().intent.as_mut() {
                intent.remaining = 3;
                intent.last_sent = None;
            }
            if runtime.observer && (!connected || reconnected) {
                state.launch_history.lock().unwrap().synchronized = false;
            }
            let (mut current, clock_waiter, clock_known) = {
                let history = state.launch_history.lock().unwrap();
                (history.snapshot(), history.waiter(), history.clock_known())
            };
            if !runtime.observer {
                runtime.primary_durable =
                    clock_known && crate::launch_history::flush(clock_waiter).await.is_ok();
            }
            for frame in frames {
                match runtime.receive(pairing.as_ref(), &frame, now, &current) {
                    Ok(Some(Outcome::Clock(clock))) => {
                        match state.apply_primary_launch_clock(clock) {
                            Ok(_) => {
                                current = state.launch_history.lock().unwrap().snapshot();
                                if let Err(error) =
                                    acknowledge_current(&state, &mut runtime, pairing.as_ref())
                                        .await
                                {
                                    state.launch_history.lock().unwrap().synchronized = false;
                                    log::debug!("Clock acknowledgement deferred: {error}");
                                }
                            }
                            Err(error) => {
                                state.add_notification(format!(
                                    "Primary clock snapshot was rejected: {error}"
                                ));
                            }
                        }
                    }
                    Ok(Some(Outcome::ConfirmClock)) => {
                        if let Err(error) =
                            acknowledge_current(&state, &mut runtime, pairing.as_ref()).await
                        {
                            state.launch_history.lock().unwrap().synchronized = false;
                            log::debug!("Clock acknowledgement deferred: {error}");
                        }
                    }
                    Ok(Some(Outcome::Reset {
                        nonce,
                        expected,
                        issuer,
                    })) => {
                        let work = state.clone();
                        let reset = tokio::task::spawn_blocking(move || {
                            work.try_reset_expected_launch_clock(&expected)
                        })
                        .await;
                        let mut error = match reset {
                            Ok(Ok(())) => None,
                            Ok(Err(error)) => Some(error.to_string()),
                            Err(error) => Some(error.to_string()),
                        };
                        let (clock, waiter) = {
                            let mut history = state.launch_history.lock().unwrap();
                            if let Err(failure) = history.retry_save() {
                                error = Some(failure.to_string());
                            }
                            (history.snapshot(), history.waiter())
                        };
                        let durable = match crate::launch_history::flush(waiter).await {
                            Ok(()) => true,
                            Err(failure) => {
                                error =
                                    Some(format!("Primary reset has not been saved: {failure}"));
                                false
                            }
                        };
                        runtime.primary_durable = durable;
                        current = state.launch_history.lock().unwrap().snapshot();
                        if let Some(session) = runtime.session {
                            match seal(
                                pairing.as_ref(),
                                &issuer,
                                session,
                                Message::ResetReply {
                                    nonce,
                                    clock,
                                    error,
                                    durable,
                                },
                            ) {
                                Ok(reply) => runtime.reply = Some(reply),
                                Err(error) => log::warn!("Reset reply not encoded: {error}"),
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => log::debug!("Ignored clock metadata: {error}"),
                }
            }
            if let Err(error) = runtime.offer_reset(pairing.as_ref(), &current, now, |wire| {
                crate::mission_sync::wire::offer(crate::mission_sync::wire::Class::ClockQuery, wire)
            }) {
                log::warn!("Clock reset request: {error}");
            }
            if let Err(error) = runtime.offer(pairing.as_ref(), &current, now, |reply, wire| {
                crate::mission_sync::wire::offer(
                    if reply {
                        crate::mission_sync::wire::Class::ClockReply
                    } else {
                        crate::mission_sync::wire::Class::ClockQuery
                    },
                    wire,
                )
            }) {
                log::warn!("Clock synchronization: {error}");
            }
            if runtime.observer && (runtime.verification_pending() || reset_status().pending) {
                state.launch_history.lock().unwrap().synchronized = false;
            }
            crate::mission_sync::wire::flush(&mesh, now);
        }
    }))
}
#[cfg(test)]
mod tests;

#[cfg(all(test,target_os="linux",any(feature="firmware_sim",feature="hitl_mode")))]
pub(crate) mod pipeline_fixture;
