//! Primary-authoritative operating mode over the existing authenticated settings budget.
//! This changes observer metadata, never compiled capabilities or actuation authority.
use crate::{state::AppState, station_pairing::Pairing};
use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::get,
};
use ring::signature::{self, KeyPair};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
const MAGIC: &[u8] = b"OM\x01";
const DISPLAY_MAGIC: &[u8] = b"OM\x02";
const DOMAIN: &[u8] = b"gs-primary-operating-mode-v1\0";
const MAX_WIRE: usize = 1024;
const CHALLENGE_TTL_MS: u64 = 30_000;
const RETRY_MS: u64 = 5_000;
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Normal,
    Hitl,
    TestFire,
    Testing,
}
pub fn local_mode() -> Mode {
    if cfg!(feature = "testing") {
        Mode::Testing
    } else if cfg!(feature = "hitl_mode") {
        Mode::Hitl
    } else if cfg!(feature = "test_fire_mode") {
        Mode::TestFire
    } else {
        Mode::Normal
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub effective_mode: Option<Mode>,
    pub local_build_mode: Mode,
    pub primary_station: Option<String>,
    pub synchronized: bool,
    pub persisted: bool,
    pub local_capabilities_match: bool,
    pub error: Option<String>,
    pub authority: &'static str,
}
impl Snapshot {
    fn unknown() -> Self {
        Self {
            effective_mode: None,
            local_build_mode: local_mode(),
            primary_station: None,
            synchronized: false,
            persisted: false,
            local_capabilities_match: false,
            error: None,
            authority: "unknown",
        }
    }
}
static STATUS: OnceLock<Mutex<Snapshot>> = OnceLock::new();
pub fn snapshot() -> Snapshot {
    if !crate::rrls::is_rrls() {
        return Snapshot {
            effective_mode: Some(local_mode()),
            local_build_mode: local_mode(),
            primary_station: None,
            synchronized: true,
            persisted: false,
            local_capabilities_match: true,
            error: None,
            authority: "local_primary",
        };
    }
    STATUS
        .get_or_init(|| Mutex::new(Snapshot::unknown()))
        .lock()
        .unwrap()
        .clone()
}
pub(crate) fn recognizes(wire: &[u8]) -> bool {
    wire.starts_with(MAGIC) || wire.starts_with(DISPLAY_MAGIC)
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Message {
    Query {
        nonce: String,
    },
    Reply {
        nonce: String,
        generation: u64,
        mode: Mode,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    installation: String,
    issuer: String,
    recipient: String,
    message: Message,
}
fn signed(wire: &[u8]) -> Vec<u8> {
    [DOMAIN, wire].concat()
}
fn seal(pairing: &Pairing, recipient: &str, message: Message) -> anyhow::Result<Vec<u8>> {
    let e = Envelope {
        installation: pairing.public.installation_id.clone(),
        issuer: pairing.public.station_id.clone(),
        recipient: recipient.into(),
        message,
    };
    let mut wire = MAGIC.to_vec();
    wire.extend(serde_json::to_vec(&e)?);
    let signature = pairing.mission_signing_key().sign(&signed(&wire));
    wire.extend_from_slice(signature.as_ref());
    anyhow::ensure!(wire.len() <= MAX_WIRE, "Mode envelope too large");
    Ok(wire)
}
fn open(pairing: &Pairing, wire: &[u8]) -> anyhow::Result<Envelope> {
    anyhow::ensure!(
        wire.starts_with(MAGIC) && (MAGIC.len() + 64..=MAX_WIRE).contains(&wire.len()),
        "Invalid mode envelope"
    );
    let end = wire.len() - 64;
    signature::UnparsedPublicKey::new(
        &signature::ED25519,
        pairing.mission_signing_key().public_key().as_ref(),
    )
    .verify(&signed(&wire[..end]), &wire[end..])
    .map_err(|_| anyhow::anyhow!("Invalid mode signature"))?;
    let e: Envelope = serde_json::from_slice(&wire[MAGIC.len()..end])?;
    anyhow::ensure!(
        e.installation == pairing.public.installation_id
            && e.recipient == pairing.public.station_id
            && pairing.public.peers.iter().any(|p| p.id == e.issuer),
        "Unpaired mode identity"
    );
    Ok(e)
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    installation: String,
    primary: String,
    signed_reply: Vec<u8>,
}
fn save(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = path.with_extension("mode-pending");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
struct Runtime {
    observer: bool,
    primary: Option<String>,
    path: PathBuf,
    generation: u64,
    mode: Option<Mode>,
    persisted: bool,
    last_verified: Option<u64>,
    pending: Option<(String, u64)>,
    last_sent: Option<u64>,
    connected: bool,
    mesh_session: Option<u32>,
    reply: Option<Vec<u8>>,
    error: Option<String>,
}
impl Runtime {
    fn new(pairing: &Pairing, observer: bool, path: PathBuf) -> anyhow::Result<Self> {
        let primaries = pairing
            .public
            .peers
            .iter()
            .filter(|p| p.role == "primary")
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !observer || primaries.len() == 1,
            "Exactly one authoritative primary must be paired"
        );
        let mut this = Self {
            observer,
            primary: primaries.first().map(|p| p.id.clone()),
            path,
            generation: 0,
            mode: None,
            persisted: false,
            last_verified: None,
            pending: None,
            last_sent: None,
            connected: false,
            mesh_session: None,
            reply: None,
            error: None,
        };
        if observer {
            if this.path.exists() {
                let bytes = std::fs::read(&this.path)?;
                anyhow::ensure!(bytes.len() <= MAX_WIRE * 5, "Oversized saved mode");
                let saved: Saved = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(
                    saved.version == 1
                        && saved.installation == pairing.public.installation_id
                        && Some(&saved.primary) == this.primary.as_ref(),
                    "Saved mode authority changed"
                );
                let e = open(pairing, &saved.signed_reply)?;
                match e.message {
                    Message::Reply {
                        generation, mode, ..
                    } if Some(&e.issuer) == this.primary.as_ref() => {
                        this.generation = generation;
                        this.mode = Some(mode);
                        this.persisted = true;
                    }
                    _ => anyhow::bail!("Invalid saved primary mode"),
                }
            }
        } else {
            // A durable primary boot generation rejects older replies after an
            // RRLS has observed a newer primary process/configuration.
            let previous = if this.path.exists() {
                serde_json::from_slice::<u64>(&std::fs::read(&this.path)?)?
            } else {
                0
            };
            this.generation = previous
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("Mode generation exhausted"))?;
            save(&this.path, &serde_json::to_vec(&this.generation)?)?;
        }
        Ok(this)
    }
    fn snapshot(&self, now: u64) -> Snapshot {
        Snapshot {
            effective_mode: self.mode,
            local_build_mode: local_mode(),
            primary_station: self.primary.clone(),
            synchronized: self.connected && self.last_verified.is_some_and(|v| now >= v),
            persisted: self.persisted,
            local_capabilities_match: self.mode == Some(local_mode()),
            error: self.error.clone(),
            authority: "paired_signature",
        }
    }
    fn receive(&mut self, pairing: &Pairing, wire: &[u8], now: u64) -> anyhow::Result<()> {
        let e = open(pairing, wire)?;
        if self.observer {
            anyhow::ensure!(
                Some(&e.issuer) == self.primary.as_ref(),
                "Mode publisher is not the primary"
            );
            let Message::Reply {
                nonce,
                generation,
                mode,
            } = e.message
            else {
                anyhow::bail!("RRLS cannot author mode")
            };
            anyhow::ensure!(
                self.connected
                    && self.pending.as_ref().is_some_and(|(n, at)| *n == nonce
                        && now >= *at
                        && now - *at < CHALLENGE_TTL_MS),
                "Stale/replayed mode challenge"
            );
            anyhow::ensure!(
                generation >= self.generation
                    && (generation != self.generation || self.mode.is_none_or(|old| old == mode)),
                "Replayed/conflicting mode generation"
            );
            if !self.persisted || self.mode != Some(mode) || self.generation != generation {
                let saved = Saved {
                    version: 1,
                    installation: pairing.public.installation_id.clone(),
                    primary: e.issuer,
                    signed_reply: wire.to_vec(),
                };
                save(&self.path, &serde_json::to_vec(&saved)?)?;
            }
            // A failed durable write must not expose an effective update.
            self.mode = Some(mode);
            self.generation = generation;
            self.persisted = true;
            self.last_verified = Some(now);
            self.pending = None;
            self.error = None;
        } else {
            anyhow::ensure!(
                pairing
                    .public
                    .peers
                    .iter()
                    .any(|p| p.id == e.issuer && p.role == "rrls"),
                "Only a paired observer may query mode"
            );
            let Message::Query { nonce } = e.message else {
                anyhow::bail!("Primary mode cannot be changed remotely")
            };
            anyhow::ensure!(
                nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid mode nonce"
            );
            self.reply = Some(seal(
                pairing,
                &e.issuer,
                Message::Reply {
                    nonce,
                    generation: self.generation,
                    mode: local_mode(),
                },
            )?);
        }
        Ok(())
    }
    fn connect(&mut self, connected: bool) {
        if self.connected != connected {
            self.pending = None;
            self.last_sent = None;
            self.last_verified = None;
            self.reply = None;
        }
        self.connected = connected;
    }
    fn connect_mesh(&mut self, connected: bool, session: Option<u32>) {
        if self.mesh_session != session {
            self.connect(false);
            self.mesh_session = session;
        }
        self.connect(connected);
    }
    fn step(&mut self, pairing: &Pairing, connected: bool, now: u64) -> anyhow::Result<()> {
        self.connect(connected);
        if !connected {
            return Ok(());
        }
        if !self.observer {
            if let Some(wire) = self.reply.as_ref()
                && crate::mission_sync::wire::offer(
                    crate::mission_sync::wire::Class::ModeReply,
                    wire,
                )
            {
                self.reply = None;
            }
            return Ok(());
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, at)| now < *at || now - *at >= CHALLENGE_TTL_MS)
        {
            self.pending = None;
        }
        // Build mode stays fixed until this connection ends. Retry the
        // challenge only until the first verified reply.
        if self.last_verified.is_none()
            && self
                .last_sent
                .is_none_or(|v| now < v || now - v >= RETRY_MS)
        {
            let (nonce, at) = self
                .pending
                .get_or_insert_with(|| (format!("{:032x}", rand::random::<u128>()), now));
            let wire = seal(
                pairing,
                self.primary.as_deref().expect("validated primary"),
                Message::Query {
                    nonce: nonce.clone(),
                },
            )?;
            if crate::mission_sync::wire::offer(crate::mission_sync::wire::Class::ModeQuery, &wire)
            {
                self.last_sent = Some(now);
                let _ = at;
            }
        }
        Ok(())
    }
}
// This protocol is deliberately non-authorizing. Its session is the already
// accepted primary HEALTH session; a fresh random challenge rejects buffered
// replies. No unsigned message enters the paired-signature runtime.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DisplaySaved {
    version: u8,
    mode: Mode,
}
struct DisplayRuntime {
    observer: bool,
    path: PathBuf,
    session: Option<u32>,
    mode: Option<Mode>,
    persisted: bool,
    pending: Option<(u128, u64)>,
    last_sent: Option<u64>,
    verified: Option<u64>,
    reply: Option<Vec<u8>>,
    error: Option<String>,
}
fn display_frame(session: u32, nonce: u128, mode: Option<Mode>) -> Vec<u8> {
    let mut wire = DISPLAY_MAGIC.to_vec();
    wire.push(u8::from(mode.is_some()));
    wire.extend(session.to_le_bytes());
    wire.extend(nonce.to_le_bytes());
    if let Some(mode) = mode {
        wire.push(match mode {
            Mode::Normal => 0,
            Mode::Hitl => 1,
            Mode::TestFire => 2,
            Mode::Testing => 3,
        });
    }
    wire
}
impl DisplayRuntime {
    fn new(observer: bool, path: PathBuf) -> anyhow::Result<Self> {
        let mode = if path.exists() {
            let bytes = std::fs::read(&path)?;
            anyhow::ensure!(bytes.len() <= 128, "Oversized saved display mode");
            let saved: DisplaySaved = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(saved.version == 2, "Invalid saved display mode version");
            Some(saved.mode)
        } else {
            None
        };
        Ok(Self {
            observer,
            path,
            session: None,
            mode,
            persisted: mode.is_some(),
            pending: None,
            last_sent: None,
            verified: None,
            reply: None,
            error: None,
        })
    }
    fn connect(&mut self, session: Option<u32>) {
        if self.session != session {
            self.pending = None;
            self.last_sent = None;
            self.verified = None;
            self.reply = None;
        }
        self.session = session;
    }
    fn receive(&mut self, wire: &[u8], now: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            wire.starts_with(DISPLAY_MAGIC) && matches!(wire.len(), 24 | 25),
            "Invalid display mode envelope"
        );
        let session = u32::from_le_bytes(wire[4..8].try_into()?);
        anyhow::ensure!(
            Some(session) == self.session,
            "Display mode session is not current"
        );
        let nonce = u128::from_le_bytes(wire[8..24].try_into()?);
        if self.observer {
            anyhow::ensure!(
                wire.len() == 25 && wire[3] == 1,
                "Observer cannot author mode"
            );
            anyhow::ensure!(
                self.pending
                    .is_some_and(|(n, at)| n == nonce && now >= at && now - at < CHALLENGE_TTL_MS),
                "Stale/replayed display challenge"
            );
            let mode = match wire[24] {
                0 => Mode::Normal,
                1 => Mode::Hitl,
                2 => Mode::TestFire,
                3 => Mode::Testing,
                _ => anyhow::bail!("Invalid display mode"),
            };
            if !self.persisted || self.mode != Some(mode) {
                save(
                    &self.path,
                    &serde_json::to_vec(&DisplaySaved { version: 2, mode })?,
                )?;
            }
            self.mode = Some(mode);
            self.persisted = true;
            self.verified = Some(now);
            self.pending = None;
            self.error = None;
        } else {
            anyhow::ensure!(
                wire.len() == 24 && wire[3] == 0,
                "Primary mode cannot be changed remotely"
            );
            self.reply = Some(display_frame(session, nonce, Some(local_mode())));
        }
        Ok(())
    }
    fn step(&mut self, now: u64) {
        let Some(session) = self.session else { return };
        if !self.observer {
            if let Some(wire) = self.reply.as_ref()
                && crate::mission_sync::wire::offer(
                    crate::mission_sync::wire::Class::ModeReply,
                    wire,
                )
            {
                self.reply = None;
            }
            return;
        }
        if self
            .pending
            .is_some_and(|(_, at)| now < at || now - at >= CHALLENGE_TTL_MS)
        {
            self.pending = None;
        }
        if self.verified.is_none()
            && self
                .last_sent
                .is_none_or(|v| now < v || now - v >= RETRY_MS)
        {
            let (nonce, _) = *self
                .pending
                .get_or_insert_with(|| (rand::random::<u128>(), now));
            if crate::mission_sync::wire::offer(
                crate::mission_sync::wire::Class::ModeQuery,
                &display_frame(session, nonce, None),
            ) {
                self.last_sent = Some(now);
            }
        }
    }
    fn snapshot(&self, now: u64) -> Snapshot {
        Snapshot {
            effective_mode: self.mode,
            local_build_mode: local_mode(),
            primary_station: None,
            synchronized: self.session.is_some() && self.verified.is_some_and(|v| now >= v),
            persisted: self.persisted,
            local_capabilities_match: self.mode == Some(local_mode()),
            error: self.error.clone(),
            authority: "mesh_session",
        }
    }
}
enum WorkerRuntime {
    Signed(Runtime),
    Display(DisplayRuntime),
}
struct Observation {
    now: u64,
    connected: bool,
    session: Option<u32>,
    frames: Vec<Vec<u8>>,
}
impl WorkerRuntime {
    fn new(pairing: Option<&Pairing>, observer: bool, path: PathBuf) -> anyhow::Result<Self> {
        match pairing {
            Some(pairing) => Ok(Self::Signed(Runtime::new(pairing, observer, path)?)),
            None => Ok(Self::Display(DisplayRuntime::new(observer, path)?)),
        }
    }
    fn poll(
        &mut self,
        pairing: Option<&Pairing>,
        observation: Observation,
    ) -> anyhow::Result<Snapshot> {
        let Observation {
            now,
            connected,
            session,
            frames,
        } = observation;
        match self {
            Self::Signed(rt) => {
                let pairing =
                    pairing.ok_or_else(|| anyhow::anyhow!("Signed runtime has no pairing"))?;
                // Invalidate the old challenge before receiving any buffered reply.
                rt.connect_mesh(connected, session);
                for wire in frames {
                    if let Err(error) = rt.receive(pairing, &wire, now) {
                        rt.error = Some(error.to_string());
                    }
                }
                rt.step(pairing, connected, now)?;
                Ok(rt.snapshot(now))
            }
            Self::Display(rt) => {
                anyhow::ensure!(pairing.is_none(), "Configured pairing cannot downgrade");
                rt.connect(session);
                for wire in frames {
                    if let Err(error) = rt.receive(&wire, now) {
                        rt.error = Some(error.to_string());
                    }
                }
                rt.step(now);
                Ok(rt.snapshot(now))
            }
        }
    }
}
pub fn start(state: Arc<AppState>) -> Option<tokio::task::JoinHandle<()>> {
    let pairing = match Pairing::load() {
        Ok(p) => p.map(Arc::new),
        Err(e) => {
            // Configured trust failure must never downgrade to unsigned metadata.
            log::warn!("Mode pairing unavailable: {e}");
            let mut status = STATUS
                .get_or_init(|| Mutex::new(Snapshot::unknown()))
                .lock()
                .unwrap();
            status.error = Some(format!("Configured pairing unavailable: {e}"));
            return None;
        }
    };
    let mesh = crate::rrls::settings_transport()?;
    let observer = crate::rrls::is_rrls();
    let path = std::env::var_os("GS_STATION_MODE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let name = pairing.as_ref().map_or_else(
                || {
                    if observer {
                        "rrls-display"
                    } else {
                        "primary-display"
                    }
                    .to_string()
                },
                |p| p.public.station_id.clone(),
            );
            PathBuf::from("data").join(format!("station-mode-{name}.json"))
        });
    let runtime = Arc::new(Mutex::new(None::<WorkerRuntime>));
    let mut shutdown = state.shutdown_subscribe();
    Some(tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_millis(500));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = ticks.tick() => {}
            }
            let observation = {
                let mut link = mesh.lock().unwrap();
                let now = link.now();
                Observation {
                    now,
                    connected: link.settings_available(now),
                    session: link.operating_mode_session(now),
                    frames: (0..4).filter_map(|_| link.take_operating_mode()).collect(),
                }
            };
            let now = observation.now;
            let (runtime, pairing, path) = (runtime.clone(), pairing.clone(), path.clone());
            // Exactly one bounded blocking job per tick, awaited before another.
            // Persistence and signature operations never hold the radio lock.
            let work = tokio::task::spawn_blocking(move || -> anyhow::Result<Snapshot> {
                let mut runtime = runtime.lock().unwrap();
                if runtime.is_none() {
                    *runtime = Some(WorkerRuntime::new(pairing.as_deref(), observer, path)?);
                }
                runtime
                    .as_mut()
                    .unwrap()
                    .poll(pairing.as_deref(), observation)
            })
            .await;
            {
                let mut status = STATUS
                    .get_or_init(|| Mutex::new(Snapshot::unknown()))
                    .lock()
                    .unwrap();
                match work {
                    Ok(Ok(snapshot)) => *status = snapshot,
                    other => {
                        status.synchronized = false;
                        status.error = Some(format!("Mode synchronization unavailable: {other:?}"));
                    }
                }
            }
            crate::mission_sync::wire::flush(&mesh, now);
        }
    }))
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) =
        crate::web::authorize_headers(&state, &headers, crate::auth::Permission::ViewData).await
    {
        return error;
    }
    Json(snapshot()).into_response()
}
pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/station_mode", get(status))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn testing_mode_is_explicit_in_local_signed_display_and_persisted_metadata() {
        if cfg!(feature = "testing") {
            assert_eq!(local_mode(), Mode::Testing);
        }
        let root = root();
        let gs = fixture(&root, "gs", 1);
        let pair = fixture(&root, "rrls", 1);
        let mut signed = Runtime::new(&pair, true, root.join("signed-testing")).unwrap();
        let nonce = "b".repeat(32);
        waiting(&mut signed, &nonce, 0);
        let wire = seal(
            &gs,
            "rrls",
            Message::Reply {
                nonce,
                generation: 1,
                mode: Mode::Testing,
            },
        )
        .unwrap();
        signed.receive(&pair, &wire, 1).unwrap();
        assert_eq!(signed.mode, Some(Mode::Testing));
        assert_eq!(
            Runtime::new(&pair, true, root.join("signed-testing"))
                .unwrap()
                .mode,
            Some(Mode::Testing)
        );
        let mut display = DisplayRuntime::new(true, root.join("display-testing")).unwrap();
        display.connect(Some(7));
        display.pending = Some((123, 0));
        let wire = display_frame(7, 123, Some(Mode::Testing));
        assert_eq!(wire.len(), 25);
        assert_eq!(wire[24], 3);
        display.receive(&wire, 1).unwrap();
        assert_eq!(display.mode, Some(Mode::Testing));
        assert_eq!(
            DisplayRuntime::new(true, root.join("display-testing"))
                .unwrap()
                .mode,
            Some(Mode::Testing)
        );
        assert_eq!(
            serde_json::to_string(&Mode::Testing).unwrap(),
            "\"testing\""
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn default_direct_mesh_syncs_without_pairing_voice_ready_or_document_presence() {
        use crate::rrls::{Mesh, Role};
        let root = root();
        let mut gs = Mesh::new(Role::Primary, 42);
        let mut rr = Mesh::new(Role::Rrls, 73);
        gs.tick(0);
        let health = gs.pop(0).unwrap();
        assert!(rr.incoming(health.command, &health.bytes, 0).is_none());
        assert_eq!(rr.operating_mode_session(0), Some(42));
        assert!(
            !gs.settings_available(0),
            "direct mode has no READY or signed settings presence"
        );
        let mut observer = DisplayRuntime::new(true, root.join("observer")).unwrap();
        observer.connect(rr.operating_mode_session(0));
        observer.pending = Some((123, 0));
        assert!(rr.queue_settings(&display_frame(42, 123, None), 0));
        let query = rr.pop(0).unwrap();
        assert!(gs.incoming(query.command, &query.bytes, 0).is_none());
        let mut primary = DisplayRuntime::new(false, root.join("primary")).unwrap();
        primary.connect(gs.operating_mode_session(0));
        primary
            .receive(&gs.take_operating_mode().unwrap(), 0)
            .unwrap();
        let reply = primary.reply.take().unwrap();
        assert!(
            !gs.queue_settings(&reply, 0),
            "document admission stays unavailable"
        );
        assert!(
            gs.queue_operating_mode_reply(&reply, 0),
            "validated mode reply alone can bootstrap"
        );
        assert!(
            !gs.settings_available(0),
            "reply does not create settings authority/presence"
        );
        let reply = gs.pop(0).unwrap();
        assert!(rr.incoming(reply.command, &reply.bytes, 0).is_none());
        observer
            .receive(&rr.take_operating_mode().unwrap(), 0)
            .unwrap();
        assert_eq!(observer.mode, Some(local_mode()));
        assert!(observer.snapshot(0).synchronized);
        assert!(observer.persisted);
        assert!(rr.take_settings().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unpaired_display_roundtrip_persists_stale_and_binds_live_session_nonce() {
        let root = root();
        let path = root.join("display.json");
        let mut rr = DisplayRuntime::new(true, path.clone()).unwrap();
        assert_eq!(rr.snapshot(0).effective_mode, None);
        rr.connect(Some(44));
        rr.pending = Some((123, 100));
        let wire = display_frame(44, 123, Some(Mode::Hitl));
        assert!(
            rr.receive(&display_frame(43, 123, Some(Mode::Normal)), 101)
                .is_err()
        );
        assert!(
            rr.receive(&display_frame(44, 124, Some(Mode::Normal)), 101)
                .is_err()
        );
        rr.receive(&wire, 102).unwrap();
        assert_eq!(rr.snapshot(102).effective_mode, Some(Mode::Hitl));
        assert!(
            crate::layout::default_layout_path(rr.snapshot(102).effective_mode.unwrap())
                .ends_with("layout_hitl.json")
        );
        assert!(rr.snapshot(102).synchronized);
        assert_eq!(rr.snapshot(102).authority, "mesh_session");
        assert!(rr.receive(&wire, 103).is_err());
        let restored = DisplayRuntime::new(true, path).unwrap();
        assert_eq!(restored.mode, Some(Mode::Hitl));
        assert!(!restored.snapshot(104).synchronized);
        assert!(
            crate::layout::default_layout_path(restored.snapshot(104).effective_mode.unwrap())
                .ends_with("layout_hitl.json")
        );
        rr.connect(None);
        assert!(!rr.snapshot(105).synchronized);
        assert!(rr.receive(&wire, 105).is_err());
        rr.connect(Some(45));
        rr.pending = Some((123, 106));
        assert!(rr.receive(&wire, 107).is_err());
        rr.pending = Some((555, 107));
        rr.path = root.clone();
        assert!(
            rr.receive(&display_frame(45, 555, Some(Mode::Normal)), 108)
                .is_err()
        );
        assert_eq!(rr.mode, Some(Mode::Hitl));
        assert_eq!(rr.pending, Some((555, 107)));
        // Unchanged renewal performs no disk write, even with an unwritable destination.
        rr.receive(&display_frame(45, 555, Some(Mode::Hitl)), 109)
            .unwrap();
        rr.path = root.join("display-transition.json");
        rr.pending = Some((556, 110));
        rr.receive(&display_frame(45, 556, Some(Mode::Normal)), 111)
            .unwrap();
        assert!(
            crate::layout::default_layout_path(rr.snapshot(111).effective_mode.unwrap())
                .ends_with("layout.json")
        );
        rr.pending = Some((557, 112));
        rr.receive(&display_frame(45, 557, Some(Mode::Hitl)), 113)
            .unwrap();
        assert!(
            crate::layout::default_layout_path(rr.snapshot(113).effective_mode.unwrap())
                .ends_with("layout_hitl.json")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn synchronized_mode_stays_latched_until_disconnect_or_session_replacement() {
        let root = root();
        let gs = fixture(&root, "gs", 1);
        let pair = fixture(&root, "rrls", 1);
        let mut signed = Runtime::new(&pair, true, root.join("signed-latch")).unwrap();
        signed.connect_mesh(true, Some(42));
        let nonce = "a".repeat(32);
        signed.pending = Some((nonce.clone(), 1));
        let wire = seal(
            &gs,
            "rrls",
            Message::Reply {
                nonce,
                generation: 1,
                mode: Mode::Hitl,
            },
        )
        .unwrap();
        signed.receive(&pair, &wire, 2).unwrap();
        let mut display = DisplayRuntime::new(true, root.join("display-latch")).unwrap();
        display.connect(Some(42));
        display.pending = Some((123, 1));
        display
            .receive(&display_frame(42, 123, Some(Mode::Hitl)), 2)
            .unwrap();
        for now in [30_002, 90_002, 3_600_002] {
            signed.step(&pair, true, now).unwrap();
            display.step(now);
            assert!(signed.pending.is_none(), "no periodic signed mode query");
            assert!(display.pending.is_none(), "no periodic display mode query");
            assert!(signed.last_sent.is_none());
            assert!(display.last_sent.is_none());
            assert!(signed.snapshot(now).synchronized);
            assert!(display.snapshot(now).synchronized);
            assert_eq!(signed.mode, Some(Mode::Hitl));
            assert_eq!(display.mode, Some(Mode::Hitl));
        }
        signed.connect_mesh(false, None);
        display.connect(None);
        assert!(!signed.snapshot(3_600_003).synchronized);
        assert!(!display.snapshot(3_600_003).synchronized);
        assert_eq!(signed.mode, Some(Mode::Hitl));
        assert_eq!(display.mode, Some(Mode::Hitl));
        signed.connect_mesh(true, Some(43));
        display.connect(Some(43));
        assert!(signed.last_verified.is_none());
        assert!(display.verified.is_none());
        assert!(signed.persisted && display.persisted);
        // A new boot session must invalidate the latch even if no worker tick
        // happened to sample the disconnected interval.
        signed.last_verified = Some(3_600_004);
        signed.connect_mesh(true, Some(44));
        assert!(signed.last_verified.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unpaired_primary_only_answers_current_session_query_and_cannot_change_build() {
        let root = root();
        let mut gs = DisplayRuntime::new(false, root.join("primary")).unwrap();
        gs.connect(Some(8));
        gs.receive(&display_frame(8, 123, None), 1).unwrap();
        assert_eq!(gs.reply, Some(display_frame(8, 123, Some(local_mode()))));
        assert!(
            gs.receive(&display_frame(8, 123, Some(Mode::Hitl)), 2)
                .is_err()
        );
        assert!(gs.receive(&display_frame(7, 123, None), 2).is_err());
        gs.connect(Some(9));
        assert!(gs.reply.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn paired_reconnect_invalidates_old_challenge_and_queued_reply() {
        let root = root();
        let gs = fixture(&root, "gs", 1);
        let rr = fixture(&root, "rrls", 1);
        let mut rt = Runtime::new(&rr, true, root.join("saved")).unwrap();
        waiting(&mut rt, "abc", 0);
        let old = seal(
            &gs,
            "rrls",
            Message::Reply {
                nonce: "abc".into(),
                generation: 1,
                mode: Mode::Hitl,
            },
        )
        .unwrap();
        rt.reply = Some(vec![1]);
        rt.connect(false);
        rt.connect(true);
        assert!(rt.reply.is_none());
        assert!(rt.pending.is_none());
        assert!(rt.receive(&rr, &old, 1).is_err());
        assert_eq!(rt.mode, None);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn signed_authority_never_accepts_unsigned_display_messages() {
        let root = root();
        let pair = fixture(&root, "rrls", 1);
        assert!(open(&pair, &display_frame(1, 2, Some(Mode::Hitl))).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    fn fixture(root: &Path, id: &str, seed: u8) -> Pairing {
        let key = root.join(format!("{id}-{seed}.seed"));
        std::fs::write(&key, [seed; 32]).unwrap();
        let peer = if id == "gs" { "rrls" } else { "gs" };
        let role = if id == "gs" { "rrls" } else { "primary" };
        let config = root.join(format!("{id}-{seed}.json"));
        std::fs::write(&config,serde_json::to_vec(&serde_json::json!({"version":1,"installation_id":"launch","station_id":id,"signing_seed_file":key.file_name().unwrap().to_str().unwrap(),"peers":[{"id":peer,"role":role,"urls":[]}]})).unwrap()).unwrap();
        Pairing::from_file(&config).unwrap()
    }
    fn root() -> PathBuf {
        let p = std::env::temp_dir().join(format!("gs-mode-test-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    fn waiting(rr: &mut Runtime, nonce: &str, now: u64) {
        rr.connected = true;
        rr.pending = Some((nonce.into(), now));
    }
    #[test]
    fn signed_primary_mode_persists_and_restores_stale_without_rewriting_renewal() {
        let root = root();
        let gs = fixture(&root, "gs", 1);
        let rr = fixture(&root, "rrls", 1);
        let mut rt = Runtime::new(&rr, true, root.join("saved-mode")).unwrap();
        let nonce = "a".repeat(32);
        waiting(&mut rt, &nonce, 0);
        let wire = seal(
            &gs,
            "rrls",
            Message::Reply {
                nonce: nonce.clone(),
                generation: 1,
                mode: Mode::Hitl,
            },
        )
        .unwrap();
        rt.receive(&rr, &wire, 1).unwrap();
        assert_eq!(rt.mode, Some(Mode::Hitl));
        assert!(rt.snapshot(1).synchronized);
        let restored = Runtime::new(&rr, true, root.join("saved-mode")).unwrap();
        assert_eq!(restored.mode, Some(Mode::Hitl));
        assert!(!restored.snapshot(2).synchronized);
        assert!(restored.persisted);
        assert!(
            rt.receive(&rr, &wire, 2).is_err(),
            "consumedchallenge cannot renew freshness"
        );
        rt.path = root.clone();
        waiting(&mut rt, &nonce, 3);
        rt.receive(&rr, &wire, 4).unwrap();
        assert_eq!(
            rt.mode,
            Some(Mode::Hitl),
            "unchangedmode renewal must not write to nowinvalid destination"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn wrong_key_nonce_generation_and_role_cannot_mutate_mode() {
        let root = root();
        let gs = fixture(&root, "gs", 1);
        let rr = fixture(&root, "rrls", 1);
        let wrong = fixture(&root, "gs", 2);
        let mut rt = Runtime::new(&rr, true, root.join("mode")).unwrap();
        let nonce = "b".repeat(32);
        waiting(&mut rt, &nonce, 10);
        let signed = |key: &Pairing, n: String, generation, mode| {
            seal(
                key,
                "rrls",
                Message::Reply {
                    nonce: n,
                    generation,
                    mode,
                },
            )
            .unwrap()
        };
        assert!(
            rt.receive(&rr, &signed(&wrong, nonce.clone(), 1, Mode::Hitl), 11)
                .is_err()
        );
        assert!(
            rt.receive(&rr, &signed(&gs, "c".repeat(32), 1, Mode::Hitl), 11)
                .is_err()
        );
        assert!(rt.mode.is_none());
        rt.receive(&rr, &signed(&gs, nonce.clone(), 2, Mode::Hitl), 11)
            .unwrap();
        waiting(&mut rt, &nonce, 12);
        assert!(
            rt.receive(&rr, &signed(&gs, nonce.clone(), 1, Mode::Normal), 13)
                .is_err()
        );
        assert!(
            rt.receive(&rr, &signed(&gs, nonce.clone(), 2, Mode::Normal), 13)
                .is_err()
        );
        assert_eq!(rt.mode, Some(Mode::Hitl));
        rt.connected = false;
        assert!(
            rt.receive(&rr, &signed(&gs, nonce.clone(), 3, Mode::Normal), 14)
                .is_err()
        );
        waiting(&mut rt, &nonce, 15);
        assert!(
            rt.receive(
                &rr,
                &signed(&gs, nonce, 3, Mode::Normal),
                CHALLENGE_TTL_MS + 16
            )
            .is_err()
        );
        let mut primary = Runtime::new(&gs, false, root.join("primary-generation")).unwrap();
        let illegal = seal(
            &rr,
            "gs",
            Message::Reply {
                nonce: "d".repeat(32),
                generation: 100,
                mode: Mode::Hitl,
            },
        )
        .unwrap();
        assert!(primary.receive(&gs, &illegal, 17).is_err());
        assert!(primary.mode.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn durable_write_failure_keeps_last_effective_mode_and_nonce() {
        let root = root();
        let gs = fixture(&root, "gs", 1);
        let rr = fixture(&root, "rrls", 1);
        let mut rt = Runtime::new(&rr, true, root.join("mode")).unwrap();
        let nonce = "e".repeat(32);
        waiting(&mut rt, &nonce, 0);
        let first = seal(
            &gs,
            "rrls",
            Message::Reply {
                nonce: nonce.clone(),
                generation: 1,
                mode: Mode::Normal,
            },
        )
        .unwrap();
        rt.receive(&rr, &first, 1).unwrap();
        rt.path = root.clone();
        waiting(&mut rt, &nonce, 2);
        let update = seal(
            &gs,
            "rrls",
            Message::Reply {
                nonce: nonce.clone(),
                generation: 2,
                mode: Mode::Hitl,
            },
        )
        .unwrap();
        assert!(rt.receive(&rr, &update, 3).is_err());
        assert_eq!(rt.mode, Some(Mode::Normal));
        assert_eq!(rt.generation, 1);
        assert_eq!(rt.pending.as_ref().unwrap().0, nonce);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn primary_generation_advances_once_per_restart_and_legacy_is_not_normal() {
        let root = root();
        let gs = fixture(&root, "gs", 1);
        let p = root.join("epoch");
        assert_eq!(Runtime::new(&gs, false, p.clone()).unwrap().generation, 1);
        assert_eq!(Runtime::new(&gs, false, p).unwrap().generation, 2);
        assert!(open(&gs, b"normal").is_err());
        assert!(Snapshot::unknown().effective_mode.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
