//! Historical primary configuration for display only. Never command authority.
use crate::{station_mode::Mode, station_pairing::Pairing};
use anyhow::{Result, ensure};
use ring::signature::{self, KeyPair};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

const DOMAIN: &[u8] = b"gs26-rrls-retained-configuration-v1\0";
const MAX_BYTES: u64 = 8192;
#[cfg(test)]
struct PauseState {
    entered: std::sync::atomic::AtomicBool,
    release: Mutex<bool>,
    changed: std::sync::Condvar,
}
#[cfg(test)]
static WRITE_PAUSE: OnceLock<Mutex<Option<Arc<PauseState>>>> = OnceLock::new();
#[cfg(test)]
pub(super) struct WritePause(Arc<PauseState>);
#[cfg(test)]
impl WritePause {
    pub(super) fn entered(&self) -> bool {
        self.0.entered.load(std::sync::atomic::Ordering::Acquire)
    }
    pub(super) fn release(&self) {
        *self.0.release.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
#[cfg(test)]
impl Drop for WritePause {
    fn drop(&mut self) {
        self.release();
    }
}
#[cfg(test)]
pub(super) fn hold_next_write() -> WritePause {
    let pause = Arc::new(PauseState {
        entered: std::sync::atomic::AtomicBool::new(false),
        release: Mutex::new(false),
        changed: std::sync::Condvar::new(),
    });
    *WRITE_PAUSE.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(pause.clone());
    WritePause(pause)
}
#[cfg(test)]
fn pause_write() {
    let pause = WRITE_PAUSE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .take();
    if let Some(pause) = pause {
        pause
            .entered
            .store(true, std::sync::atomic::Ordering::Release);
        let mut release = pause.release.lock().unwrap();
        while !*release {
            release = pause.changed.wait(release).unwrap();
        }
    }
}

const FRESH_MS: u128 = 1500;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Source {
    installation: String,
    primary: String,
    recipient: String,
    verify_key: String,
}
impl Source {
    fn new(pairing: &Pairing) -> Result<Self> {
        let primary = pairing
            .public
            .peers
            .iter()
            .filter(|p| p.role == "primary")
            .collect::<Vec<_>>();
        ensure!(primary.len() == 1, "Exactly one paired primary required");
        Ok(Self {
            installation: pairing.public.installation_id.clone(),
            primary: primary[0].id.clone(),
            recipient: pairing.public.station_id.clone(),
            verify_key: pairing.public.verify_key.clone(),
        })
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Policy {
    pub(crate) settings: [Option<bool>; 4],
    pub(crate) source_revision: u64,
    pub(crate) signed_test_fire_scope: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Configuration {
    version: u8,
    generation: u64,
    source: Source,
    mode: Mode,
    boot: [u8; 16],
    source_revision: Option<u64>,
    policies: [Option<bool>; 4],
    signed_test_fire_scope: Option<bool>,
}
impl Configuration {
    fn validate(&self, source: &Source) -> Result<()> {
        ensure!(
            self.version == 1
                && self.generation > 0
                && self.source == *source
                && self.boot != [0; 16],
            "Unknown configuration identity/version"
        );
        ensure!(
            self.source_revision != Some(u64::MAX),
            "Exhausted source revision"
        );
        ensure!(
            self.source_revision.is_some() || self.policies.iter().all(Option::is_none),
            "Unknown policy source revision"
        );
        ensure!(
            self.mode == Mode::Hitl || self.policies[..3].iter().all(Option::is_none),
            "Non-HITL optional policies"
        );
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signed {
    configuration: Configuration,
    signature: Vec<u8>,
}
fn signed_bytes(configuration: &Configuration) -> Result<Vec<u8>> {
    let mut out = DOMAIN.to_vec();
    out.extend(serde_json::to_vec(configuration)?);
    Ok(out)
}
fn decode(bytes: &[u8], pairing: &Pairing, source: &Source) -> Result<Configuration> {
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Configuration exceeds bound"
    );
    let record: Signed = serde_json::from_slice(bytes)?;
    record.configuration.validate(source)?;
    signature::UnparsedPublicKey::new(
        &signature::ED25519,
        pairing.mission_signing_key().public_key().as_ref(),
    )
    .verify(&signed_bytes(&record.configuration)?, &record.signature)
    .map_err(|_| anyhow::anyhow!("Invalid configuration signature"))?;
    Ok(record.configuration)
}
fn read(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Configuration exceeds bound"
    );
    Ok(bytes)
}
#[derive(Debug)]
struct Store {
    path: PathBuf,
    source: Source,
    generation: u64,
    saved: Option<Configuration>,
}
impl Store {
    fn open(path: PathBuf, pairing: &Pairing) -> Result<(Self, Option<Configuration>)> {
        let source = Source::new(pairing)?;
        let saved = match read(&path) {
            Ok(bytes) => Some(decode(&bytes, pairing, &source)?),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                None
            }
            Err(e) => return Err(e),
        };
        let generation = saved.as_ref().map_or(0, |c| c.generation);
        Ok((
            Self {
                path,
                source,
                generation,
                saved: saved.clone(),
            },
            saved,
        ))
    }
    fn accept(
        &mut self,
        pairing: &Pairing,
        mode: Mode,
        boot: [u8; 16],
        policy: Option<Policy>,
    ) -> Result<Option<Configuration>> {
        ensure!(
            self.source == Source::new(pairing)?,
            "Configuration source changed"
        );
        if let Some(saved) = &self.saved
            && ignore_older(Evidence::stored(saved), Evidence { mode, boot, policy })?
        {
            return Ok(None);
        }
        self.save(pairing, mode, boot, policy).map(Some)
    }

    fn save(
        &mut self,
        pairing: &Pairing,
        mode: Mode,
        boot: [u8; 16],
        policy: Option<Policy>,
    ) -> Result<Configuration> {
        let policies = policy.map_or([None; 4], |p| p.settings);
        let source_revision = policy.map(|p| p.source_revision);
        let signed_test_fire_scope = policy.and_then(|p| p.signed_test_fire_scope);
        ensure!(
            self.source == Source::new(pairing)?,
            "Configuration source changed"
        );
        if let Some(saved) = self.saved.as_ref().filter(|saved| {
            saved.mode == mode
                && saved.boot == boot
                && saved.policies == policies
                && saved.signed_test_fire_scope == signed_test_fire_scope
        }) {
            return Ok(saved.clone());
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Configuration generation exhausted"))?;
        let configuration = Configuration {
            version: 1,
            generation,
            source: self.source.clone(),
            mode,
            boot,
            source_revision,
            policies,
            signed_test_fire_scope,
        };
        configuration.validate(&Source::new(pairing)?)?;
        let signature = pairing
            .mission_signing_key()
            .sign(&signed_bytes(&configuration)?)
            .as_ref()
            .to_vec();
        let bytes = serde_json::to_vec(&Signed {
            configuration: configuration.clone(),
            signature,
        })?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "Configuration exceeds bound"
        );
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        // Once a write is attempted, do not deduplicate against an older
        // receipt after uncertain rename/directory-fsync failure. Gaps in this
        // local record generation are permitted; it is never remote authority.
        self.generation = generation;
        self.saved = None;
        std::fs::create_dir_all(parent)?;
        let pending = self.path.with_extension("config-pending");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&pending)?;
        file.write_all(&bytes)?;
        #[cfg(test)]
        pause_write();
        file.sync_all()?;
        drop(file);
        ensure!(
            decode(&read(&pending)?, pairing, &self.source)? == configuration,
            "Configuration temporary readback mismatch"
        );
        std::fs::rename(&pending, &self.path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        ensure!(
            decode(&read(&self.path)?, pairing, &self.source)? == configuration,
            "Configuration readback mismatch"
        );
        self.generation = generation;
        self.saved = Some(configuration.clone());
        Ok(configuration)
    }
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Snapshot {
    pub status: &'static str,
    pub mode: Option<Mode>,
    pub settings: [Option<bool>; 4],
    pub source_revision: Option<u64>,
    pub signed_test_fire_scope: Option<bool>,
    pub record_generation: Option<u64>,
    pub error: Option<String>,
    pub persistence_pending: bool,
}
struct Presentation {
    configuration: Option<Configuration>,
    accepted: Option<Instant>,
    transit_ms: u64,
    connected: bool,
    error: Option<String>,
    pending: bool,
    owner: u64,
    connection: u64,
}
static PRESENTATION: OnceLock<Mutex<Presentation>> = OnceLock::new();
fn presentation() -> &'static Mutex<Presentation> {
    PRESENTATION.get_or_init(|| {
        Mutex::new(Presentation {
            configuration: None,
            accepted: None,
            transit_ms: 0,
            connected: false,
            error: None,
            pending: false,
            owner: 0,
            connection: 0,
        })
    })
}
pub(crate) fn disconnected() {
    let mut p = presentation().lock().unwrap();
    p.connected = false;
    p.accepted = None;
    p.connection = p.connection.saturating_add(1);
}
pub(crate) fn snapshot(mode: Option<Mode>, synchronized: bool) -> Snapshot {
    let p = presentation().lock().unwrap();
    let c = p.configuration.as_ref().filter(|c| Some(c.mode) == mode);
    let known = c.is_some_and(|c| c.policies.iter().any(Option::is_some));
    let connected = p.connected && synchronized;
    let current = connected
        && p.accepted.is_some_and(|at| {
            at.elapsed()
                .as_millis()
                .saturating_add(u128::from(p.transit_ms))
                <= FRESH_MS
        });
    Snapshot {
        status: if !known {
            "unknown"
        } else if current {
            "current"
        } else if connected {
            "last_known"
        } else {
            "disconnected"
        },
        mode: c.map(|c| c.mode),
        settings: c.map_or([None; 4], |c| c.policies),
        source_revision: c.and_then(|c| c.source_revision),
        signed_test_fire_scope: c.and_then(|c| c.signed_test_fire_scope),
        record_generation: c.map(|c| c.generation),
        error: p.error.clone(),
        persistence_pending: p.pending,
    }
}
/// One blocking job and one coalesced latest slot, owned by the existing CP task.
/// `service` only awaits a terminal handle. Normal shutdown drains and joins it.
#[derive(Clone, Copy, Debug)]
struct Evidence {
    mode: Mode,
    boot: [u8; 16],
    policy: Option<Policy>,
}
type ConfigurationTuple = (Mode, [u8; 16], [Option<bool>; 4], Option<bool>);
impl Evidence {
    fn tuple(self) -> ConfigurationTuple {
        (
            self.mode,
            self.boot,
            self.policy.map_or([None; 4], |p| p.settings),
            self.policy.and_then(|p| p.signed_test_fire_scope),
        )
    }
    fn stored(c: &Configuration) -> Self {
        Self {
            mode: c.mode,
            boot: c.boot,
            policy: c.source_revision.map(|r| Policy {
                settings: c.policies,
                source_revision: r,
                signed_test_fire_scope: c.signed_test_fire_scope,
            }),
        }
    }
}
fn ignore_older(previous: Evidence, incoming: Evidence) -> Result<bool> {
    if previous.mode == incoming.mode
        && previous.boot == incoming.boot
        && previous.tuple() != incoming.tuple()
        && let (Some(old), Some(new)) = (previous.policy, incoming.policy)
    {
        if new.source_revision < old.source_revision {
            return Ok(true);
        }
        let contradiction = old
            .settings
            .into_iter()
            .zip(new.settings)
            .any(|(a, b)| a.zip(b).is_some_and(|(a, b)| a != b))
            || old
                .signed_test_fire_scope
                .zip(new.signed_test_fire_scope)
                .is_some_and(|(a, b)| a != b);
        ensure!(
            new.source_revision != old.source_revision || !contradiction,
            "Conflicting configuration at the same source revision"
        );
    }
    Ok(false)
}
#[derive(Clone, Copy, Debug)]
struct Update {
    evidence: Evidence,
    at: Option<Instant>,
    transit_ms: u64,
    connection: u64,
    serial: u64,
}
enum Completion {
    Loaded(Result<(Store, Option<Configuration>)>),
    Saved(Store, Result<Option<Configuration>>),
}
pub(crate) struct Worker {
    pairing: Arc<Pairing>,
    store: Option<Store>,
    job: Option<tokio::task::JoinHandle<Completion>>,
    active: Option<Evidence>,
    pending: Option<Update>,
    latest: Option<Update>,
    serial: u64,
    owner: u64,
    closed: bool,
}
impl Worker {
    pub(crate) fn open(pairing: Arc<Pairing>) -> Self {
        let path = std::env::var_os("GS_RRLS_CONFIGURATION_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("backend/data/rrls-configuration.json"));
        Self::open_at(pairing, path)
    }
    pub(super) fn open_at(pairing: Arc<Pairing>, path: PathBuf) -> Self {
        let owner = {
            let mut p = presentation().lock().unwrap();
            p.owner = p
                .owner
                .checked_add(1)
                .expect("configuration owner exhausted");
            p.configuration = None;
            p.accepted = None;
            p.connected = false;
            p.pending = true;
            p.error = None;
            p.owner
        };
        let p = pairing.clone();
        let job = tokio::task::spawn_blocking(move || Completion::Loaded(Store::open(path, &p)));
        Self {
            pairing,
            store: None,
            job: Some(job),
            active: None,
            pending: None,
            latest: None,
            serial: 0,
            owner,
            closed: false,
        }
    }
    fn fail(&self, message: String) {
        let mut p = presentation().lock().unwrap();
        if p.owner != self.owner {
            return;
        }
        p.configuration = None;
        p.accepted = None;
        p.pending = false;
        p.error = Some(message);
    }
    fn publish(&self, c: Configuration, current: Option<Update>) {
        let mut p = presentation().lock().unwrap();
        if p.owner != self.owner {
            return;
        }
        let update = current.filter(|u| {
            !self.closed
                && u.serial == self.serial
                && u.connection == p.connection
                && p.connection != u64::MAX
        });
        p.configuration = Some(c);
        p.accepted = update.and_then(|u| u.at);
        p.transit_ms = update.map_or(0, |u| u.transit_ms);
        p.connected = update.is_some();
        p.pending = self.pending.is_some();
        p.error = None;
    }
    /// Called only after current signed CP acceptance; no disk/SDK waits here.
    #[cfg(test)]
    pub(super) fn accepted(
        &mut self,
        mode: Mode,
        boot: [u8; 16],
        observation: &[u8],
        transit_ms: u64,
    ) {
        self.accepted_at(mode, boot, observation, transit_ms, Instant::now());
    }
    pub(crate) fn accepted_at(
        &mut self,
        mode: Mode,
        boot: [u8; 16],
        observation: &[u8],
        transit_ms: u64,
        received_at: Instant,
    ) {
        if self.closed {
            return;
        }
        let evidence = Evidence {
            mode,
            boot,
            policy: crate::primary_mirror::configuration_policy(observation, mode),
        };
        if let Some(previous) = self.latest {
            match ignore_older(previous.evidence, evidence) {
                Ok(true) => return,
                Err(e) => {
                    self.serial = self.serial.saturating_add(1);
                    if self.serial == u64::MAX {
                        self.latest = None;
                    }
                    self.pending = None;
                    self.fail(e.to_string());
                    return;
                }
                Ok(false) => {}
            }
        }
        let Some(serial) = self.serial.checked_add(1) else {
            self.latest = None;
            self.pending = None;
            self.fail("Configuration sequence exhausted".into());
            return;
        };
        self.serial = serial;
        let connection = {
            let mut p = presentation().lock().unwrap();
            if p.owner != self.owner {
                return;
            }
            if p.configuration
                .as_ref()
                .is_some_and(|c| c.mode != mode || c.boot != boot)
            {
                p.configuration = None;
            }
            p.accepted = None;
            p.connected = p.connection != u64::MAX;
            p.pending = true;
            p.connection
        };
        let mut update = Update {
            evidence,
            at: Some(received_at),
            transit_ms,
            connection,
            serial,
        };
        // Keep the strongest observed ordering watermark even when the values
        // are unchanged and no filesystem write is necessary.
        if let Some(previous) = self.latest
            && previous.evidence.tuple() == evidence.tuple()
            && previous.evidence.policy.map(|p| p.source_revision)
                > evidence.policy.map(|p| p.source_revision)
        {
            update.evidence = previous.evidence;
        }
        self.latest = Some(update);
        if self.job.is_none()
            && let Some(saved) = self.store.as_ref().and_then(|s| s.saved.as_ref())
            && Evidence::stored(saved).tuple() == evidence.tuple()
        {
            self.pending = None;
            self.publish(saved.clone(), Some(update));
            return;
        }
        self.pending = if self
            .active
            .is_some_and(|active| active.tuple() == evidence.tuple())
        {
            None
        } else {
            Some(update)
        };
        self.start_pending();
    }
    fn start_pending(&mut self) {
        if self.job.is_some() {
            return;
        }
        let Some(update) = self.pending.take() else {
            return;
        };
        let Some(mut store) = self.store.take() else {
            self.fail("Saved primary configuration is unavailable".into());
            return;
        };
        let pairing = self.pairing.clone();
        self.active = Some(update.evidence);
        {
            let mut p = presentation().lock().unwrap();
            if p.owner == self.owner {
                p.pending = true;
            }
        }
        self.job = Some(tokio::task::spawn_blocking(move || {
            let result = store.accept(
                &pairing,
                update.evidence.mode,
                update.evidence.boot,
                update.evidence.policy,
            );
            Completion::Saved(store, result)
        }));
    }
    fn completed(&mut self, completion: Completion) {
        self.active = None;
        match completion {
            Completion::Loaded(Ok((store, saved))) => {
                if self.latest.is_none()
                    && let Some(saved) = saved
                {
                    self.publish(saved, None);
                }
                self.store = Some(store);
            }
            Completion::Loaded(Err(e)) => {
                self.pending = None;
                self.fail(format!("Saved primary configuration unavailable: {e}"));
            }
            Completion::Saved(store, result) => {
                match result {
                    Ok(Some(c)) => {
                        if let Some(latest) = self.latest
                            && latest.evidence.tuple() == Evidence::stored(&c).tuple()
                            && latest.serial == self.serial
                        {
                            self.publish(c, Some(latest));
                        }
                    }
                    Ok(None) => {
                        if self.pending.is_none()
                            && let Some(c) = &store.saved
                        {
                            self.latest = Some(Update {
                                evidence: Evidence::stored(c),
                                at: None,
                                transit_ms: 0,
                                connection: 0,
                                serial: self.serial,
                            });
                            self.publish(c.clone(), None);
                        }
                    }
                    Err(e) => self.fail(format!("Primary configuration save failed: {e}")),
                }
                self.store = Some(store);
            }
        }
        if self.job.is_none() && self.pending.is_none() {
            let mut p = presentation().lock().unwrap();
            if p.owner == self.owner {
                p.pending = false;
            }
        }
    }
    /// The existing network tick never awaits an unfinished filesystem job.
    pub(crate) async fn service(&mut self) {
        if self.job.as_ref().is_some_and(|job| job.is_finished()) {
            let result = self
                .job
                .take()
                .unwrap()
                .await
                .expect("configuration writer panicked");
            self.completed(result);
        }
        self.start_pending();
    }
    pub(crate) async fn shutdown(mut self) {
        self.closed = true;
        disconnected();
        loop {
            if let Some(job) = self.job.take() {
                self.completed(job.await.expect("configuration writer panicked"));
            }
            self.start_pending();
            if self.job.is_none() {
                break;
            }
        }
        disconnected();
    }
    #[cfg(test)]
    pub(super) async fn idle(&mut self) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                self.service().await;
                if self.job.is_none() && self.pending.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("configuration writer did not become idle");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn isolated(name: &str) -> bool {
        if std::env::var_os("RETAINED_CONCURRENT_CHILD").is_some() {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env("RETAINED_CONCURRENT_CHILD", "1")
            .env("GS_RRLS_ROLE", "rrls")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        true
    }
    fn observation(revision: u64, first: bool) -> Vec<u8> {
        let mut bytes = vec![0; 88];
        bytes[0] = 1;
        bytes[57] = 5;
        bytes[75..83].copy_from_slice(&revision.to_le_bytes());
        bytes[83] = 0b111110 | u8::from(first);
        bytes[85..87].copy_from_slice(&u16::MAX.to_le_bytes());
        bytes[87] = 2;
        bytes
    }
    async fn entered(pause: &WritePause) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !pause.entered() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn held_write_keeps_actual_cp_readiness_and_http_manual_stop_responsive() {
        if isolated(
            "command_proxy::retained_config::tests::held_write_keeps_actual_cp_readiness_and_http_manual_stop_responsive",
        ) {
            return;
        }
        use tower::ServiceExt;
        assert!(
            crate::rrls::is_rrls(),
            "real RRLS HTTP Stop path is the production-reachable case"
        );
        let (root, p, r, mut primary, mut rr, pv, rv) = super::super::tests::connected();
        let mut worker = Worker::open_at(
            Arc::new(Pairing::from_file(&root.0.join("rrls.json")).unwrap()),
            root.0.join("configuration.json"),
        );
        worker.idle().await;
        let pause = hold_next_write();
        let before = Instant::now();
        worker.accepted(Mode::Hitl, [1; 16], &observation(9, false), 0);
        assert!(before.elapsed() < std::time::Duration::from_secs(1));
        entered(&pause).await;
        tokio::time::timeout(std::time::Duration::from_millis(200), worker.service())
            .await
            .expect("CP tick must not wait for filesystem");
        assert!(snapshot(Some(Mode::Hitl), true).persistence_pending);
        let query = rr.query_for_readiness(&r, rv, true).unwrap();
        let super::super::Received::Reply(reply) =
            primary.receive(&p, pv, Mode::Hitl, &query).unwrap()
        else {
            panic!("readiness reply")
        };
        assert!(primary.take_readiness_demand());
        assert!(matches!(
            rr.receive(&r, rv, Mode::Normal, &reply).unwrap(),
            super::super::Received::Capability { .. }
        ));
        let state = crate::recording_export::tests::export_state(&root.0, false).await;
        super::super::handler_tests::user(&state.auth, true, false, false, &["Abort"]);
        crate::ensure_auth_sessions_table(&state.auth_db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES ('retained-token','operator','password',1,1,0,9223372036854775807)").execute(&state.auth_db).await.unwrap();
        let old = state.manual_abort_intent.epoch();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            crate::web::router(state.clone(), String::new()).oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/command")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer retained-token")
                    .body(axum::body::Body::from("\"Abort\""))
                    .unwrap(),
            ),
        )
        .await
        .expect("manual Stop must not wait for metadata fsync")
        .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
        assert!(state.manual_abort_intent.epoch() > old);
        let closing_before = presentation().lock().unwrap().connection;
        let shutdown = tokio::spawn(worker.shutdown());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let entered = presentation().lock().unwrap().connection != closing_before;
                if entered {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual writer shutdown must reach its closure boundary");
        assert!(
            !shutdown.is_finished(),
            "actual owned writer must be joined before terminal receipt"
        );
        pause.release();
        shutdown.await.unwrap();
        assert_eq!(snapshot(Some(Mode::Hitl), false).status, "disconnected");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn coalesced_saturation_mode_change_disconnect_and_shutdown_never_publish_old_current() {
        if isolated(
            "command_proxy::retained_config::tests::coalesced_saturation_mode_change_disconnect_and_shutdown_never_publish_old_current",
        ) {
            return;
        }
        let (root, _, pairing) = fixture();
        let path = root.0.join("configuration.json");
        let pairing = Arc::new(pairing);
        let mut worker = Worker::open_at(pairing.clone(), path.clone());
        worker.idle().await;
        let first = hold_next_write();
        worker.accepted(Mode::Hitl, [1; 16], &observation(9, false), 0);
        entered(&first).await;
        for index in 10..1011 {
            worker.accepted(Mode::Hitl, [1; 16], &observation(index, index % 2 == 0), 0);
        }
        assert!(worker.job.is_some());
        assert!(
            worker.pending.is_some(),
            "only one fixed latest slot, not a growing queue"
        );
        worker.accepted(Mode::Normal, [2; 16], &observation(1, false), 0);
        let second = hold_next_write();
        first.release();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !second.entered() {
                worker.service().await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            snapshot(Some(Mode::Hitl), true).status,
            "unknown",
            "late old-boot write must not publish old settings"
        );
        assert_eq!(snapshot(Some(Mode::Normal), true).status, "unknown");
        disconnected();
        second.release();
        worker.idle().await;
        let value = snapshot(Some(Mode::Normal), true);
        assert_eq!(value.status, "disconnected");
        assert_eq!(value.settings, [None, None, None, Some(false)]);
        assert!(
            value.record_generation.unwrap() <= 3,
            "intermediate coalesced changes do not fsync per packet"
        );
        let (_, saved) = Store::open(path, &pairing).unwrap();
        let saved = saved.unwrap();
        assert_eq!(saved.boot, [2; 16]);
        assert_eq!(saved.mode, Mode::Normal);
        // Finite publication serials never permit an old completion to become
        // Current after a conflicting offer at exhaustion.
        let third = hold_next_write();
        worker.serial = u64::MAX - 1;
        worker.accepted(Mode::Hitl, [3; 16], &observation(2, false), 0);
        entered(&third).await;
        worker.accepted(Mode::Hitl, [3; 16], &observation(2, true), 0);
        assert_eq!(snapshot(Some(Mode::Hitl), true).status, "unknown");
        third.release();
        worker.idle().await;
        assert_eq!(snapshot(Some(Mode::Hitl), true).status, "unknown");
        assert!(snapshot(Some(Mode::Hitl), true).error.is_some());
        worker.shutdown().await;
    }
    #[test]
    fn same_revision_knowledge_enrichment_is_not_a_contradiction() {
        let (root, _, pairing) = fixture();
        let (mut store, _) = Store::open(root.0.join("configuration.json"), &pairing).unwrap();
        let partial = Policy {
            settings: [Some(false), None, None, None],
            source_revision: 9,
            signed_test_fire_scope: None,
        };
        store
            .accept(&pairing, Mode::Hitl, [1; 16], Some(partial))
            .unwrap();
        let complete = store
            .accept(&pairing, Mode::Hitl, [1; 16], policy())
            .unwrap()
            .unwrap();
        assert_eq!(complete.policies, policy().unwrap().settings);
    }

    fn fixture() -> (
        crate::recording_export::tests::TestDirectory,
        Pairing,
        Pairing,
    ) {
        super::super::tests::fixtures()
    }
    fn policy() -> Option<Policy> {
        Some(Policy {
            settings: [Some(false), Some(true), Some(true), Some(false)],
            source_revision: 9,
            signed_test_fire_scope: Some(false),
        })
    }
    #[test]
    fn signed_roundtrip_deduplicates_unrelated_source_revisions_and_binds_mode_boot_key() {
        let (root, _, pairing) = fixture();
        let path = root.0.join("configuration.json");
        let (mut store, saved) = Store::open(path.clone(), &pairing).unwrap();
        assert!(saved.is_none());
        let first = store.save(&pairing, Mode::Hitl, [1; 16], policy()).unwrap();
        assert_eq!(first.generation, 1);
        let original = read(&path).unwrap();
        let unchanged = store
            .save(
                &pairing,
                Mode::Hitl,
                [1; 16],
                Some(Policy {
                    source_revision: 700,
                    ..policy().unwrap()
                }),
            )
            .unwrap();
        assert_eq!(unchanged, first);
        assert_eq!(read(&path).unwrap(), original);
        let (_, restored) = Store::open(path.clone(), &pairing).unwrap();
        assert_eq!(restored, Some(first));
        let changed = store
            .save(
                &pairing,
                Mode::Normal,
                [2; 16],
                Some(Policy {
                    settings: [None, None, None, Some(true)],
                    source_revision: 900,
                    signed_test_fire_scope: Some(false),
                }),
            )
            .unwrap();
        assert_eq!(changed.generation, 2);
        assert!(changed.policies[..3].iter().all(Option::is_none));
        let (_, _, mut wrong) = fixture();
        wrong.public.installation_id = "other".into();
        assert!(Store::open(path.clone(), &wrong).is_err());
        wrong.public.installation_id = pairing.public.installation_id.clone();
        wrong.public.station_id = "other-rrls".into();
        assert!(Store::open(path.clone(), &wrong).is_err());
        wrong.public.station_id = pairing.public.station_id.clone();
        wrong.public.verify_key = "changed-public-key".into();
        assert!(Store::open(path.clone(), &wrong).is_err());
        let mut tampered: serde_json::Value =
            serde_json::from_slice(&read(&path).unwrap()).unwrap();
        tampered["configuration"]["policies"][3] = serde_json::json!(false);
        std::fs::write(&path, serde_json::to_vec(&tampered).unwrap()).unwrap();
        assert!(Store::open(path, &pairing).is_err());
    }
    #[test]
    fn older_changed_source_revision_keeps_newer_receipt_and_equal_conflict_refuses() {
        let (root, _, pairing) = fixture();
        let path = root.0.join("configuration.json");
        let (mut store, _) = Store::open(path.clone(), &pairing).unwrap();
        let latest = store
            .accept(
                &pairing,
                Mode::Hitl,
                [1; 16],
                Some(Policy {
                    source_revision: 20,
                    ..policy().unwrap()
                }),
            )
            .unwrap()
            .unwrap();
        let bytes = read(&path).unwrap();
        let mut older = policy().unwrap();
        older.source_revision = 19;
        older.settings[0] = Some(true);
        assert!(
            store
                .accept(&pairing, Mode::Hitl, [1; 16], Some(older))
                .unwrap()
                .is_none()
        );
        assert_eq!(store.saved, Some(latest.clone()));
        assert_eq!(read(&path).unwrap(), bytes);
        older.source_revision = 20;
        assert!(
            store
                .accept(&pairing, Mode::Hitl, [1; 16], Some(older))
                .is_err()
        );
        assert_eq!(read(&path).unwrap(), bytes);
        older.source_revision = 21;
        let next = store
            .accept(&pairing, Mode::Hitl, [1; 16], Some(older))
            .unwrap()
            .unwrap();
        assert_eq!(next.generation, latest.generation + 1);
        // A verified new boot does not borrow the old boot's revision domain.
        older.source_revision = 1;
        assert_eq!(
            store
                .accept(&pairing, Mode::Hitl, [2; 16], Some(older))
                .unwrap()
                .unwrap()
                .boot,
            [2; 16]
        );
    }

    #[test]
    fn signed_compile_scope_is_distinct_from_hitl_mode_and_unknown_never_inherits() {
        let (root, _, pairing) = fixture();
        let path = root.0.join("configuration.json");
        let (mut store, _) = Store::open(path.clone(), &pairing).unwrap();
        let flight = store.save(&pairing, Mode::Hitl, [1; 16], policy()).unwrap();
        let test_fire = store
            .save(
                &pairing,
                Mode::Hitl,
                [1; 16],
                Some(Policy {
                    signed_test_fire_scope: Some(true),
                    ..policy().unwrap()
                }),
            )
            .unwrap();
        assert_eq!(test_fire.generation, flight.generation + 1);
        assert_eq!(test_fire.signed_test_fire_scope, Some(true));
        let unknown = store.save(&pairing, Mode::Hitl, [2; 16], None).unwrap();
        assert_eq!(unknown.signed_test_fire_scope, None);
        assert_eq!(unknown.policies, [None; 4]);
        let (_, restored) = Store::open(path, &pairing).unwrap();
        assert_eq!(restored, Some(unknown));
    }

    #[test]
    fn corrupt_unknown_oversize_and_failed_disk_never_become_saved_proof_off() {
        let (root, _, pairing) = fixture();
        let path = root.0.join("configuration.json");
        std::fs::write(&path, b"{broken").unwrap();
        assert!(Store::open(path.clone(), &pairing).is_err());
        assert_eq!(read(&path).unwrap(), b"{broken");
        std::fs::write(&path, vec![0; MAX_BYTES as usize + 1]).unwrap();
        assert!(Store::open(path.clone(), &pairing).is_err());
        std::fs::remove_file(&path).unwrap();
        let (mut store, _) = Store::open(path.clone(), &pairing).unwrap();
        std::fs::create_dir(path.with_extension("config-pending")).unwrap();
        assert!(store.save(&pairing, Mode::Hitl, [1; 16], policy()).is_err());
        assert_eq!(store.generation, 1);
        assert!(store.saved.is_none());
        assert!(!path.exists());
        std::fs::remove_dir(path.with_extension("config-pending")).unwrap();
        let unknown = store.save(&pairing, Mode::Hitl, [1; 16], None).unwrap();
        assert_eq!(unknown.policies, [None; 4]);
        assert_eq!(unknown.source_revision, None);
        store.generation = u64::MAX;
        assert!(store.save(&pairing, Mode::Hitl, [2; 16], policy()).is_err());
    }
    #[tokio::test]
    async fn worker_restart_ttl_disconnect_and_save_failure_are_presentation_only() {
        if std::env::var_os("RETAINED_CONFIG_CHILD").is_none() {
            let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","command_proxy::retained_config::tests::worker_restart_ttl_disconnect_and_save_failure_are_presentation_only","--nocapture"]).env("RETAINED_CONFIG_CHILD","1").output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        let (root, _, pairing) = fixture();
        let pairing = Arc::new(pairing);
        let path = root.0.join("configuration.json");
        let mut worker = Worker::open_at(pairing.clone(), path.clone());
        worker.idle().await;
        assert_eq!(snapshot(Some(Mode::Hitl), true).status, "unknown");
        // Valid signed-observation format; actual signed CP acceptance integration
        // is covered separately by the cross-compiled producer/receiver fixture.
        let mut bytes = vec![0; 88];
        bytes[0] = 1;
        bytes[57] = 5;
        bytes[75..83].copy_from_slice(&9u64.to_le_bytes());
        bytes[83] = 0b111110;
        bytes[84] = 0;
        bytes[85..87].copy_from_slice(&u16::MAX.to_le_bytes());
        bytes[87] = 2;
        worker.accepted(Mode::Hitl, [1; 16], &bytes, 0);
        worker.idle().await;
        let current = snapshot(Some(Mode::Hitl), true);
        assert_eq!(current.status, "current");
        assert_eq!(current.signed_test_fire_scope, Some(false));
        assert_eq!(
            current.settings,
            [Some(false), Some(true), Some(true), Some(false)]
        );
        presentation().lock().unwrap().accepted =
            Some(Instant::now() - std::time::Duration::from_millis(1501));
        assert_eq!(snapshot(Some(Mode::Hitl), true).status, "last_known");
        disconnected();
        assert_eq!(snapshot(Some(Mode::Hitl), true).status, "disconnected");
        assert_eq!(snapshot(Some(Mode::Normal), true).settings, [None; 4]);
        worker.shutdown().await;
        let mut restarted = Worker::open_at(pairing.clone(), path.clone());
        restarted.idle().await;
        assert_eq!(snapshot(Some(Mode::Hitl), false).status, "disconnected");
        assert_eq!(snapshot(Some(Mode::Hitl), false).settings, current.settings);
        let state = crate::state::tests::test_app_state().await;
        assert!(
            crate::primary_mirror::capture_launch_origin(&state).is_none(),
            "historical metadata cannot create live launch authority"
        );
        std::fs::create_dir(path.with_extension("config-pending")).unwrap();
        bytes[83] = 0b111111;
        bytes[75..83].copy_from_slice(&10u64.to_le_bytes());
        restarted.accepted(Mode::Hitl, [1; 16], &bytes, 0);
        restarted.idle().await;
        let failed = snapshot(Some(Mode::Hitl), false);
        assert_eq!(failed.status, "unknown");
        assert_eq!(failed.settings, [None; 4]);
        assert!(failed.error.is_some());
        assert!(
            restarted.store.as_ref().unwrap().generation > current.record_generation.unwrap(),
            "actual failed write allocated a new local generation"
        );
        // Failed writes preserve the prior verified historical record.
        let (_, old) = Store::open(path, &pairing).unwrap();
        assert_eq!(old.unwrap().policies, current.settings);
        restarted.shutdown().await;
    }
}
