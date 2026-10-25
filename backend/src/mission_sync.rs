//! Paired mission settings travel below live telemetry over the RRLS mesh.
//! Authentication is application-level and independent of TLS/proxy certificates.
use crate::{loadcell, state::AppState};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use paired_settings::{Document, coordinator::Coordinator, radio::RadioSync};
use ring::signature::KeyPair;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

#[path = "mission_sync/settings.rs"]
mod settings;
#[derive(Clone)]
struct Catalog {
    calibration: loadcell::LoadcellCalibrationFile,
    settings: Option<settings::Settings>,
}
impl Catalog {
    fn capture(state: &AppState) -> anyhow::Result<Self> {
        // Bounded disk read/JSON parsing precedes all sequence locks. A later
        // flight import still performs its own file CAS under the save mutex.
        let flight = crate::flight_setup::snapshot_saved().map_err(anyhow::Error::msg)?;
        let positions = crate::antenna::positions::saved_snapshot()?;
        // Lazy auto-zero preference I/O also precedes the sequence lock.
        let _ = state.auto_zero.lock().unwrap().settings();
        let gse = state.gse.lock().unwrap();
        let mut zero = state.auto_zero.lock().unwrap();
        let fill = state.fill_targets.lock().unwrap().clone();
        Ok(Self {
            calibration: state.loadcell_calibration.lock().unwrap().clone(),
            settings: Some(settings::Settings {
                fill,
                limits: gse.engine.config.clone(),
                flight,
                auto_zero: zero.settings(),
                positions,
            }),
        })
    }
    fn snapshot(&self) -> anyhow::Result<BTreeMap<String, Value>> {
        let mut snapshot = loadcell::mission::snapshot(&self.calibration)?;
        if let Some(settings) = &self.settings {
            snapshot.extend(settings.snapshot()?);
        }
        Ok(snapshot)
    }
    #[cfg(test)]
    fn calibration_only(cfg: &loadcell::LoadcellCalibrationFile) -> Self {
        Self {
            calibration: cfg.clone(),
            settings: None,
        }
    }
}
static RUNTIME: OnceLock<Arc<Mutex<Runtime>>> = OnceLock::new();
struct Runtime {
    installation: String,
    actors: Vec<String>,
    coordinator: Coordinator,
    radio: RadioSync,
    path: PathBuf,
    dirty: bool,
    needs_publication: bool,
    started: Instant,
    last_error: Option<String>,
}
impl Runtime {
    fn new(
        installation: String,
        station: String,
        peers: Vec<String>,
        snapshot: BTreeMap<String, Value>,
        bootstrap: bool,
        path: PathBuf,
    ) -> anyhow::Result<Self> {
        let mut actors = peers.clone();
        actors.push(station.clone());
        let (coordinator, dirty) = if path.exists() {
            (
                Coordinator::load(&path, &installation, &station, &actors)?,
                false,
            )
        } else {
            (
                Coordinator::new(
                    installation.clone(),
                    station.clone(),
                    snapshot,
                    bootstrap,
                    &actors,
                )?,
                true,
            )
        };
        let radio = RadioSync::new(installation.clone(), station, peers)?;
        Ok(Self {
            installation,
            actors,
            coordinator,
            radio,
            path,
            dirty,
            needs_publication: false,
            started: Instant::now(),
            last_error: None,
        })
    }
    fn checkpoint(&mut self) -> anyhow::Result<()> {
        if self.dirty {
            self.coordinator.save(&self.path)?;
            self.dirty = false;
        }
        Ok(())
    }
    fn validate(cfg: &Catalog, key: &str, document: &Document) -> anyhow::Result<()> {
        if !key.starts_with("calibration/") {
            return cfg
                .settings
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Unsupported mission setting"))?
                .validate_document(key, document);
        }
        let live = loadcell::mission::snapshot(&cfg.calibration)?;
        let expected = live
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("Unsupported calibration document"))?;
        for revision in &document.heads {
            loadcell::mission::replace(&cfg.calibration, key, expected, &revision.value)?;
        }
        Ok(())
    }
    fn observe(&mut self, cfg: &Catalog) -> anyhow::Result<()> {
        if self.coordinator.observe(&cfg.snapshot()?, &self.actors)? {
            self.dirty = true;
        }
        self.checkpoint()
    }
    fn receive(
        &mut self,
        cfg: &Catalog,
        wire: &[u8],
        now: u64,
        verify_key: &[u8],
    ) -> anyhow::Result<()> {
        if let Some((key, document)) =
            self.radio
                .receive(self.coordinator.store(), wire, now, verify_key)?
        {
            Self::validate(cfg, &key, &document)?;
            if self
                .coordinator
                .merge(&self.installation, &key, &document, &self.actors)?
            {
                self.dirty = true;
            }
            self.checkpoint()?;
        }
        Ok(())
    }
    fn resolve(&mut self, cfg: &Catalog, request: Resolve) -> anyhow::Result<()> {
        anyhow::ensure!(
            request.expected.heads.len() > 1,
            "Review a current conflict before resolving it"
        );
        let mut next = self.coordinator.clone();
        let document =
            next.resolve(&request.key, &request.expected, request.value, &self.actors)?;
        Self::validate(cfg, &request.key, &document)?;
        // A failed API save cannot leave a queued resolution that applies later.
        next.save(&self.path)?;
        self.coordinator = next;
        self.dirty = false;
        Ok(())
    }
}
#[derive(Serialize)]
struct Status {
    enabled: bool,
    link_available: bool,
    pending: usize,
    conflicts: usize,
    waiting_for_idle: bool,
    error: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolve {
    key: String,
    expected: Document,
    value: Value,
}

fn step(
    runtime: &mut Runtime,
    state: &Arc<AppState>,
    mesh: &crate::rrls::Shared,
    signer: &ring::signature::Ed25519KeyPair,
) -> anyhow::Result<()> {
    let now = runtime
        .started
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let current = Catalog::capture(state)?;
    runtime.observe(&current)?;
    let frames = {
        let mut link = mesh.lock().unwrap();
        (0..4)
            .filter_map(|_| link.take_settings())
            .collect::<Vec<_>>()
    };
    let mut reception_error = None;
    for wire in frames {
        match runtime.receive(&current, &wire, now, signer.public_key().as_ref()) {
            Ok(()) => {
                // Only authenticated, protocol-valid reception establishes presence.
                let mut link = mesh.lock().unwrap();
                let link_now = link.now();
                link.authenticated_settings_peer(link_now);
            }
            Err(error) => reception_error = Some(error),
        }
    }
    // A receive may have merged history before its first checkpoint failed.
    // Never apply or advertise that history until a durable retry succeeds.
    runtime.checkpoint()?;
    for pending in runtime.coordinator.pending() {
        // Same order as the sequence engine. Hold GSE and calibration locks
        // across the safe-state check, CAS and durable file replacement.
        let mut gse = state.gse.lock().unwrap();
        if !gse.calibration_import_allowed() {
            break;
        }
        let mut auto_zero = state.auto_zero.lock().unwrap();
        if auto_zero.pending() {
            break;
        }
        let mut publish_fill = false;
        if !pending.key.starts_with("calibration/") {
            let mut fill = state.fill_targets.lock().unwrap();
            let flight = current
                .settings
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Missing inert settings catalog"))?
                .flight
                .clone();
            let mut current = settings::Settings {
                fill: fill.clone(),
                limits: gse.engine.config.clone(),
                flight,
                auto_zero: auto_zero.settings(),
                positions: crate::antenna::positions::saved_snapshot()?,
            };
            let expected_flight = current.flight.clone();
            let expected_positions = current.positions.clone();
            current.apply_and_save(&pending, true, false, |key, next| {
                match key {
                    settings::FILL => {
                        crate::fill_targets::save(&next.fill).map_err(anyhow::Error::msg)?;
                        if fill.fill_source != next.fill.fill_source {
                            *state.latest_fill_mass_kg.lock().unwrap() = None;
                            gse.observe_mass(None);
                        }
                        *fill = next.fill.clone();
                        publish_fill = true;
                    }
                    settings::LIMITS => {
                        crate::gse::persist_settings(&next.limits).map_err(anyhow::Error::msg)?;
                        gse.engine.config = next.limits.clone();
                    }
                    settings::FLIGHT => {
                        crate::flight_setup::compare_and_save(&expected_flight, &next.flight)
                            .map_err(anyhow::Error::msg)?
                    }
                    settings::AUTO_ZERO => auto_zero
                        .configure(next.auto_zero)
                        .map_err(anyhow::Error::msg)?,
                    settings::POSITIONS => crate::antenna::positions::compare_and_save(
                        &expected_positions,
                        next.positions.clone(),
                    )?,
                    _ => anyhow::bail!("Unsupported mission setting"),
                }
                Ok(())
            })?;
            drop(fill);
            drop(auto_zero);
        } else {
            let mut calibration = state.loadcell_calibration.lock().unwrap();
            let preview = loadcell::mission::replace(
                &calibration,
                &pending.key,
                &pending.expected_local,
                &pending.revision.value,
            )?;
            let source_changed = preview.kg50_input != calibration.kg50_input;
            let recording = state.recording_status.lock().unwrap();
            if source_changed && recording.mode != crate::telemetry_db::RecordingModeWire::Idle {
                break;
            }
            loadcell::mission::apply_and_save(&mut calibration, &pending, loadcell::save)?;
            drop(calibration);
            drop(recording);
            drop(auto_zero);
            if source_changed {
                state.clear_kg50_samples();
                gse.observe_mass(None);
            }
        }
        drop(gse);
        if publish_fill {
            state.broadcast_fill_targets_snapshot();
        }
        runtime.coordinator.applied(&pending)?;
        runtime.dirty = true;
        runtime.needs_publication |=
            pending.key.starts_with("calibration/") && !crate::rrls::is_rrls();
        runtime.checkpoint()?;
    }
    if runtime.needs_publication {
        let calibration = state.loadcell_calibration.lock().unwrap().clone();
        match crate::web::publish_daq_calibration(state, &calibration) {
            Ok(()) => runtime.needs_publication = false,
            Err(error) => reception_error = Some(anyhow::anyhow!(error)),
        }
    }
    runtime
        .radio
        .poll(runtime.coordinator.store(), now, signer, |_, wire| {
            let mut link = mesh.lock().unwrap();
            let link_now = link.now();
            link.queue_settings(wire, link_now)
        })?;
    if let Some(error) = reception_error {
        return Err(error);
    }
    Ok(())
}

/// HITL retains its existing configuration and sequencing behavior.
pub fn start(state: Arc<AppState>) -> Option<tokio::task::JoinHandle<()>> {
    if cfg!(feature = "hitl_mode") {
        return None;
    }
    let pairing = match crate::station_pairing::Pairing::load() {
        Ok(Some(pairing)) => Arc::new(pairing),
        Ok(None) => return None,
        Err(error) => {
            log::warn!("Paired settings unavailable: {error}");
            return None;
        }
    };
    let Some(mesh) = crate::rrls::settings_transport() else {
        log::warn!("Paired settings require the primary or RRLS radio role");
        return None;
    };
    let snapshot = match Catalog::capture(&state).and_then(|catalog| catalog.snapshot()) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            log::warn!("Invalid local mission settings prevent paired settings: {error}");
            return None;
        }
    };
    let path = std::env::var_os("GS_MISSION_SETTINGS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("data").join(format!(
                "mission-settings-{}.json",
                pairing.public.station_id
            ))
        });
    let runtime = match Runtime::new(
        pairing.public.installation_id.clone(),
        pairing.public.station_id.clone(),
        pairing.public.peers.iter().map(|p| p.id.clone()).collect(),
        snapshot,
        !crate::rrls::is_rrls(),
        path,
    ) {
        Ok(runtime) => Arc::new(Mutex::new(runtime)),
        Err(error) => {
            log::warn!("Paired settings history unavailable: {error}");
            return None;
        }
    };
    if RUNTIME.set(runtime.clone()).is_err() {
        return None;
    }
    let mut shutdown = state.shutdown_subscribe();
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = interval.tick() => {
                    let runtime = runtime.clone(); let state = state.clone();
                    let mesh = mesh.clone(); let pairing = pairing.clone();
                    // File synchronization, serialization and signatures never
                    // block a Tokio network worker or run under the radio lock.
                    if let Err(error) = tokio::task::spawn_blocking(move || {
                        let mut runtime = runtime.lock().unwrap();
                        let result = step(&mut runtime, &state, &mesh, pairing.mission_signing_key());
                        let message = result.as_ref().err().map(|_| "Shared mission settings could not be synchronized".to_owned());
                        if runtime.last_error != message {
                            if let Err(error) = &result { log::warn!("Paired mission settings synchronization deferred: {error}"); }
                            runtime.last_error = message;
                        }
                    }).await { log::error!("Paired settings worker failed: {error}"); }
                }
            }
        }
    }))
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) =
        crate::web::authorize_headers(&state, &headers, crate::auth::Permission::ViewData).await
    {
        return error;
    }
    let Some(runtime) = RUNTIME.get() else {
        return Json(Status {
            enabled: false,
            link_available: false,
            pending: 0,
            conflicts: 0,
            waiting_for_idle: false,
            error: None,
        })
        .into_response();
    };
    let runtime = runtime.lock().unwrap();
    let pending = runtime.coordinator.pending().len();
    Json(Status {
        enabled: true,
        link_available: crate::rrls::settings_transport().is_some_and(|shared| {
            let mesh = shared.lock().unwrap();
            mesh.settings_available(mesh.now())
        }),
        pending,
        conflicts: runtime
            .coordinator
            .store()
            .documents()
            .values()
            .filter(|d| d.heads.len() > 1)
            .count(),
        waiting_for_idle: pending > 0 && !state.gse.lock().unwrap().calibration_import_allowed(),
        error: runtime.last_error.clone(),
    })
    .into_response()
}
async fn conflicts(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, crate::auth::Permission::ViewData)
            .await
        {
            Ok(p) => p,
            Err(e) => return e,
        };
    if !principal.permissions.view_data {
        return (StatusCode::FORBIDDEN, "View data permission required").into_response();
    }
    let Some(runtime) = RUNTIME.get() else {
        return Json(BTreeMap::<String, Document>::new()).into_response();
    };
    let runtime = runtime.lock().unwrap();
    Json(
        runtime
            .coordinator
            .store()
            .documents()
            .iter()
            .filter(|(key, d)| {
                d.heads.len() > 1
                    && settings::can_view(
                        key,
                        principal.permissions.view_data,
                        principal.calibration_access.view,
                    )
            })
            .map(|(k, d)| (k.clone(), d.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
    .into_response()
}
async fn resolve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<Resolve>,
) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, crate::auth::Permission::ViewData)
            .await
        {
            Ok(p) => p,
            Err(e) => return e,
        };
    if !settings::can_resolve(
        &request.key,
        principal.permissions.view_data,
        principal.calibration_access.edit,
        principal.permissions.send_commands,
        !principal.anonymous && principal.roles.contains(&crate::auth::StreamRole::Admin),
    ) {
        return (
            StatusCode::FORBIDDEN,
            "Edit permission for this setting is required",
        )
            .into_response();
    }
    let Some(runtime) = RUNTIME.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Paired settings are disabled",
        )
            .into_response();
    };
    let result = {
        let mut runtime = runtime.lock().unwrap();
        match Catalog::capture(&state) {
            Ok(catalog) => runtime.resolve(&catalog, request),
            Err(error) => Err(error),
        }
    };
    match result {
        Ok(()) => Json(serde_json::json!({"resolved":true})).into_response(),
        Err(error) => (StatusCode::CONFLICT, error.to_string()).into_response(),
    }
}
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/mission-settings", get(status))
        .route("/api/mission-settings/conflicts", get(conflicts))
        .route("/api/mission-settings/resolve", post(resolve))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rrls::{Mesh, Role};
    use ring::signature::Ed25519KeyPair;
    use serde_json::json;

    struct Station {
        id: &'static str,
        runtime: Runtime,
        calibration: loadcell::LoadcellCalibrationFile,
        mesh: Mesh,
        calibration_path: PathBuf,
    }
    fn station(id: &'static str, directory: &std::path::Path, mass: f32) -> Station {
        let calibration = loadcell::LoadcellCalibrationFile {
            full_mass_kg: Some(mass),
            ..Default::default()
        };
        let calibration_path = directory.join(format!("{id}-calibration.json"));
        loadcell::save_at(&calibration, &calibration_path).unwrap();
        let peer = if id == "gs" { "rrls" } else { "gs" };
        let runtime = Runtime::new(
            "launch".into(),
            id.into(),
            vec![peer.into()],
            loadcell::mission::snapshot(&calibration).unwrap(),
            id == "gs",
            directory.join(format!("{id}-history.json")),
        )
        .unwrap();
        Station {
            id,
            runtime,
            calibration,
            calibration_path,
            mesh: Mesh::new(
                if id == "gs" {
                    Role::Primary
                } else {
                    Role::Rrls
                },
                if id == "gs" { 7 } else { 8 },
            ),
        }
    }
    fn directory(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mission-runtime-{name}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }
    fn exchange(stations: &mut [Station; 2], key: &Ed25519KeyPair, base: u64) {
        let mut delayed = Vec::new();
        let mut sent = 0;
        let mut attempts = 0;
        for tick in 0..3000 {
            let now = base + tick * 100;
            // The main station's direct rocket path stays healthy; settings
            // synchronization must also work while RRLS is a standby observer.
            let _ = stations[0]
                .mesh
                .incoming(true, &[b'R', b'S', 1, 0, 0, 0, 0, 0, 0], now);
            for (direction, station) in stations.iter_mut().enumerate() {
                station
                    .runtime
                    .observe(&Catalog::calibration_only(&station.calibration))
                    .unwrap();
                station.mesh.tick(now);
                let runtime = &mut station.runtime;
                runtime
                    .radio
                    .poll(runtime.coordinator.store(), now, key, |_, wire| {
                        attempts += 1;
                        if attempts % 17 == 0 {
                            return false;
                        }
                        station.mesh.queue_settings(wire, now)
                    })
                    .unwrap();
                for _ in 0..4 {
                    let Some(frame) = station.mesh.pop(now) else {
                        break;
                    };
                    sent += 1;
                    if sent % 13 != 0 {
                        delayed.push((1 - direction, frame.command, frame.bytes));
                    }
                }
            }
            if tick % 3 == 0 {
                for (to, command, wire) in delayed.drain(..).rev() {
                    stations[to].mesh.incoming(command, &wire, now);
                    if tick % 7 == 0 {
                        stations[to].mesh.incoming(command, &wire, now);
                    }
                }
            }
            for station in &mut *stations {
                while let Some(wire) = station.mesh.take_settings() {
                    station
                        .runtime
                        .receive(
                            &Catalog::calibration_only(&station.calibration),
                            &wire,
                            now,
                            key.public_key().as_ref(),
                        )
                        .unwrap();
                    station.mesh.authenticated_settings_peer(now);
                }
                for pending in station.runtime.coordinator.pending() {
                    loadcell::mission::apply_and_save(&mut station.calibration, &pending, |cfg| {
                        loadcell::save_at(cfg, &station.calibration_path)
                    })
                    .unwrap();
                    station.runtime.coordinator.applied(&pending).unwrap();
                    station.runtime.dirty = true;
                    station.runtime.checkpoint().unwrap();
                }
            }
        }
        assert!(attempts > 17 && sent > 13);
        assert_eq!(
            stations[0].runtime.coordinator.store(),
            stations[1].runtime.coordinator.store()
        );
        assert!(!stations[0].mesh.active(base + 299_900));
        assert!(!stations[1].mesh.active(base + 299_900));
    }
    #[test]
    fn real_mesh_calibration_sync_retains_offline_conflicts_restart_and_explicit_resolution() {
        let dir = directory("paired");
        let key = Ed25519KeyPair::from_seed_unchecked(&[47; 32]).unwrap();
        let mut stations = [station("gs", &dir, 17.0), station("rrls", &dir, 10.0)];
        exchange(&mut stations, &key, 0);
        assert_eq!(stations[1].calibration.full_mass_kg, Some(17.0));
        for (station, mass) in stations.iter_mut().zip([20.0, 21.0]) {
            station.calibration.full_mass_kg = Some(mass);
            loadcell::save_at(&station.calibration, &station.calibration_path).unwrap();
            station
                .runtime
                .observe(&Catalog::calibration_only(&station.calibration))
                .unwrap();
        }
        exchange(&mut stations, &key, 400_000);
        let reviewed = stations[0]
            .runtime
            .coordinator
            .store()
            .document("calibration/full_mass_kg")
            .unwrap()
            .clone();
        assert_eq!(reviewed.heads.len(), 2);
        assert_eq!(stations[0].calibration.full_mass_kg, Some(20.0));
        assert_eq!(stations[1].calibration.full_mass_kg, Some(21.0));
        let rrls = &mut stations[1];
        assert_eq!(rrls.id, "rrls");
        rrls.calibration =
            serde_json::from_slice(&std::fs::read(&rrls.calibration_path).unwrap()).unwrap();
        rrls.runtime = Runtime::new(
            "launch".into(),
            "rrls".into(),
            vec!["gs".into()],
            loadcell::mission::snapshot(&rrls.calibration).unwrap(),
            false,
            dir.join("rrls-history.json"),
        )
        .unwrap();
        assert_eq!(
            rrls.runtime
                .coordinator
                .store()
                .document("calibration/full_mass_kg")
                .unwrap()
                .heads
                .len(),
            2
        );
        {
            let primary = &mut stations[0];
            let history_before = primary.runtime.coordinator.store().clone();
            let original_path = primary.runtime.path.clone();
            let blocked = dir.join("not-a-directory");
            std::fs::write(&blocked, b"block").unwrap();
            primary.runtime.path = blocked.join("history.json");
            assert!(
                primary
                    .runtime
                    .resolve(
                        &Catalog::calibration_only(&primary.calibration),
                        Resolve {
                            key: "calibration/full_mass_kg".into(),
                            expected: reviewed.clone(),
                            value: json!(21.0),
                        }
                    )
                    .is_err()
            );
            assert_eq!(primary.runtime.coordinator.store(), &history_before);
            primary.runtime.path = original_path;
        }
        let primary = &mut stations[0];
        primary
            .runtime
            .resolve(
                &Catalog::calibration_only(&primary.calibration),
                Resolve {
                    key: "calibration/full_mass_kg".into(),
                    expected: reviewed.clone(),
                    value: json!(21.0),
                },
            )
            .unwrap();
        let before = std::fs::read(&primary.runtime.path).unwrap();
        assert!(
            primary
                .runtime
                .resolve(
                    &Catalog::calibration_only(&primary.calibration),
                    Resolve {
                        key: "calibration/full_mass_kg".into(),
                        expected: reviewed,
                        value: json!(20.0),
                    }
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&primary.runtime.path).unwrap(), before);
        exchange(&mut stations, &key, 800_000);
        assert_eq!(stations[0].calibration.full_mass_kg, Some(21.0));
        assert_eq!(stations[1].calibration.full_mass_kg, Some(21.0));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn invalid_typed_document_is_rejected_without_checkpoint_or_live_mutation() {
        let dir = directory("invalid");
        let mut station = station("gs", &dir, 17.0);
        station.runtime.checkpoint().unwrap();
        let before = std::fs::read(&station.runtime.path).unwrap();
        let doc = Document {
            heads: vec![paired_settings::Revision {
                clock: BTreeMap::from([("rrls".into(), 1)]),
                value: json!({"not_a_mass":true}),
            }],
        };
        assert!(
            Runtime::validate(
                &Catalog::calibration_only(&station.calibration),
                "calibration/full_mass_kg",
                &doc
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&station.runtime.path).unwrap(), before);
        assert_eq!(station.calibration.full_mass_kg, Some(17.0));
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// Explicitly isolated process: fake config paths supplied by qualification runner.
    #[cfg(not(feature = "hitl_mode"))]
    #[tokio::test]
    #[ignore = "requires isolated fake settings directory; never use operator configuration"]
    async fn inert_worker_persists_saved_settings_without_outputs_or_qualification_changes() {
        let dir = PathBuf::from(
            std::env::var("GS_MISSION_ADAPTER_TEST_DIR")
                .expect("isolated fake settings directory required"),
        );
        for path in [
            crate::fill_targets::config_path(),
            crate::flight_setup::config_path(),
        ] {
            assert!(
                path.starts_with(&dir),
                "test must not access operator settings"
            );
        }
        for name in [
            "GS_GSE_CONFIG",
            "GS_AUTO_ZERO_CONFIG",
            "GS_LOADCELL_CALIBRATION_PATH",
            "GS_CALIBRATION_CONFIG_PATH",
            "GS_STATION_POSITIONS_PATH",
        ] {
            assert!(
                PathBuf::from(std::env::var(name).expect("explicit fake path required"))
                    .starts_with(&dir),
                "test must not read or write operator configuration"
            );
        }
        let mut state = Arc::try_unwrap(crate::state::tests::test_app_state().await)
            .ok()
            .unwrap();
        let (commands, mut received) = tokio::sync::mpsc::channel(8);
        state.cmd_tx = commands;
        let state = Arc::new(state);
        state
            .gse
            .lock()
            .unwrap()
            .engine
            .config
            .dry_self_test_confirmed = true;
        let before = serde_json::to_value(&state.gse.lock().unwrap().engine.status).unwrap();
        let current = Catalog::capture(&state).unwrap();
        let mut runtime = Runtime::new(
            "fake".into(),
            "gs".into(),
            vec!["rrls".into()],
            current.snapshot().unwrap(),
            true,
            dir.join("history.json"),
        )
        .unwrap();
        let mut desired = current.settings.as_ref().unwrap().clone();
        desired.fill.nitrous.target_mass_kg = 19.5;
        desired.flight.selected_profile_id = "wind_2".into();
        desired.limits.pressure_step_psi = 25.;
        desired.auto_zero.enabled = false;
        desired.positions.rrls = Some(crate::antenna::positions::Coordinate {
            latitude: 43.0,
            longitude: -78.8,
            altitude_m: None,
            accuracy_m: None,
            source: crate::antenna::positions::Source::Manual,
        });
        for (key, value) in desired.snapshot().unwrap() {
            let doc = Document {
                heads: vec![paired_settings::Revision {
                    clock: BTreeMap::from([("gs".into(), 1), ("rrls".into(), 1)]),
                    value,
                }],
            };
            Runtime::validate(&current, &key, &doc).unwrap();
            runtime
                .coordinator
                .merge("fake", &key, &doc, &runtime.actors)
                .unwrap();
            runtime.dirty = true;
        }
        let mesh = Arc::new(Mutex::new(Mesh::new(Role::Primary, 7)));
        let key = Ed25519KeyPair::from_seed_unchecked(&[88; 32]).unwrap();
        step(&mut runtime, &state, &mesh, &key).unwrap();
        assert!(
            matches!(
                received.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "no Apply, valve, QD, abort or other output command may be sent"
        );
        assert!(runtime.coordinator.pending().is_empty());
        let unchanged = runtime.coordinator.store().clone();
        let tick_start = Instant::now();
        for _ in 0..100 {
            step(&mut runtime, &state, &mesh, &key).unwrap();
        }
        assert_eq!(
            runtime.coordinator.store(),
            &unchanged,
            "unchanged ticks must not invent local revisions"
        );
        eprintln!(
            "warm_inert_catalog: ticks=100 elapsed_us={}",
            tick_start.elapsed().as_micros()
        );
        assert!(matches!(
            received.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(
            state.fill_targets.lock().unwrap().nitrous.target_mass_kg,
            19.5
        );
        assert_eq!(
            crate::flight_setup::snapshot_saved()
                .unwrap()
                .selected_profile_id,
            "wind_2"
        );
        assert!(!state.auto_zero.lock().unwrap().settings().enabled);
        assert_eq!(
            crate::antenna::positions::saved_snapshot().unwrap(),
            desired.positions
        );
        let gse = state.gse.lock().unwrap();
        assert!(gse.engine.config.dry_self_test_confirmed);
        assert!(gse.calibration_import_allowed());
        assert_eq!(serde_json::to_value(&gse.engine.status).unwrap(), before);
    }
    #[cfg(feature = "hitl_mode")]
    #[tokio::test]
    async fn hitl_start_remains_disabled_before_any_pairing_or_settings_worker() {
        let state = crate::state::tests::test_app_state().await;
        assert!(start(state).is_none());
    }
}
