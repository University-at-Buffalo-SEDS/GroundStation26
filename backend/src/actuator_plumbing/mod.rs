//! Persistent desired drive plus nonce-correlated board confirmation.
//! Delivery ACKs and timed electrical cycles never certify mechanical position.
#[cfg(test)]
pub(crate) mod http_tests;
mod motion_journal;
mod motion_wire;
mod wire;

use crate::{
    auth::Permission,
    state::AppState,
    types::{Board, FlightState},
};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use sedsnet::packet::Packet;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};
use wire::{Mode, Request, Status};

const REQUEST_TYPE: &str = "AB_FILL_DRIVE_SETTINGS";
const STATUS_TYPE: &str = "AB_FILL_DRIVE_STATUS";
const FRESH_MS: u64 = 3_000;
const REQUEST_TTL: u64 = 5_000;

#[derive(Clone, Copy)]
struct Moment {
    wall: Instant,
    model: Option<u64>,
}
impl Moment {
    fn now() -> Self {
        Self {
            wall: Instant::now(),
            model: crate::simulation_clock_ms(),
        }
    }
    fn age(self) -> Option<u64> {
        let now = Self::now();
        match (now.model, self.model) {
            (Some(now), Some(then)) => now.checked_sub(then),
            (None, None) => now
                .wall
                .checked_duration_since(self.wall)
                .map(|d| d.as_millis().min(u64::MAX as u128) as u64),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Desired {
    version: u8,
    mode: u8,
    revision: u64,
    source_ms: u64,
}
impl Default for Desired {
    fn default() -> Self {
        Self {
            version: 1,
            mode: 0,
            revision: 0,
            source_ms: 0,
        }
    }
}
impl Desired {
    fn load(path: &Path) -> Result<Option<Self>, String> {
        match fs::read(path) {
            Ok(bytes) => {
                let value: Self = serde_json::from_slice(&bytes)
                    .map_err(|_| "Saved drive configuration is invalid")?;
                if value.version != 1 || Mode::decode(value.mode).is_none() {
                    return Err("Saved drive configuration is unsupported".into());
                }
                Ok(Some(value))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("Saved drive configuration could not be read".into()),
        }
    }
    fn save(self, path: &Path, nonce: u64) -> Result<(), String> {
        #[cfg(test)]
        http_tests::save_hook(path, false)?;
        let parent = path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent).map_err(|_| "Cannot create drive configuration directory")?;
        let temporary = path.with_extension(format!("{nonce:016x}.tmp"));
        let result = (|| {
            let bytes = serde_json::to_vec_pretty(&self)
                .map_err(|_| "Cannot encode drive configuration")?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| "Cannot create drive configuration")?;
            file.write_all(&bytes)
                .map_err(|_| "Cannot write drive configuration")?;
            file.sync_all()
                .map_err(|_| "Cannot persist drive configuration")?;
            fs::rename(&temporary, path).map_err(|_| "Cannot replace drive configuration")?;
            #[cfg(test)]
            http_tests::save_hook(path, true)?;
            #[cfg(unix)]
            fs::File::open(parent)
                .and_then(|dir| dir.sync_all())
                .map_err(|_| "Cannot persist drive configuration directory")?;
            Ok::<(), String>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

struct Context {
    request: Request,
    sent: Moment,
    complete: bool,
    recovery_generation: u64,
    desired_revision: u64,
}
struct Observation {
    value: Status,
    after: Moment,
    recovery_generation: u64,
}
struct MotionQuery {
    nonce: u64,
    sent: Moment,
    complete: bool,
    generation: u64,
    mode_revision: u64,
}
struct MotionObservation {
    value: motion_wire::MotionStatus,
    after: Moment,
    received: Instant,
    generation: u64,
    mode_revision: u64,
}
#[cfg(feature = "testing")]
#[derive(Clone, Copy)]
struct ModeledRetraction {
    source: u64,
    started: Instant,
    generation: u64,
    manual_epoch: u64,
    observed_manual_epoch: u64,
    mode_revision: u64,
}
struct Inner {
    desired: Desired,
    #[cfg(feature = "testing")]
    modeled_retraction: Option<ModeledRetraction>,
    durable: bool,
    storage_error: Option<String>,
    requests: VecDeque<Context>,
    observed: Option<Observation>,
    motion_queries: VecDeque<MotionQuery>,
    motion_observed: Option<MotionObservation>,
    post_motion_query_record: Option<motion_journal::Record>,
    settings_busy: bool,
    pending_setting: Option<u64>,
    last_motion_stamp: u64,
    message: String,
}
pub(crate) struct Runtime {
    issuer: u64,
    path: PathBuf,
    inner: Mutex<Inner>,
    // Only settings workers take this mutex. SDK ingress and snapshots never
    // wait on filesystem IO, and all inner locks are released before IO.
    settings_gate: Mutex<()>,
    motion_journal: motion_journal::Journal,
}
impl Default for Runtime {
    fn default() -> Self {
        let issuer = rand::random::<u64>().max(1);
        let path = std::env::var_os("GS_ACTUATOR_PLUMBING_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if cfg!(test) {
                    std::env::temp_dir().join(format!(
                        "gs-plumbing-{}-{issuer:016x}/drive.json",
                        std::process::id()
                    ))
                } else {
                    PathBuf::from("backend/data/actuator_plumbing.json")
                }
            });
        let loaded = Desired::load(&path);
        let (desired, durable, storage_error) = match loaded {
            Ok(Some(value)) => (value, true, None),
            Ok(None) => (Desired::default(), false, None),
            Err(error) => (Desired::default(), false, Some(error)),
        };
        Self {
            issuer,
            motion_journal: motion_journal::Journal::new(path.with_extension("motion.json")),
            path,
            settings_gate: Mutex::new(()),
            inner: Mutex::new(Inner {
                desired,
                #[cfg(feature = "testing")]
                modeled_retraction: None,
                durable,
                storage_error,
                requests: VecDeque::new(),
                observed: None,
                motion_queries: VecDeque::new(),
                motion_observed: None,
                post_motion_query_record: None,
                settings_busy: false,
                pending_setting: None,
                last_motion_stamp: 0,
                message: "Query the actuator for its saved drive selection.".into(),
            }),
        }
    }
}

impl Runtime {
    pub(crate) fn motion_worker_pending(&self) -> bool {
        self.motion_journal.pending()
    }
    pub(crate) fn begin_settings(&self) {
        self.inner.lock().unwrap().settings_busy = true;
    }
}
struct SettingsBusy<'a>(&'a Runtime);
impl Drop for SettingsBusy<'_> {
    fn drop(&mut self) {
        self.0.inner.lock().unwrap().settings_busy = false;
    }
}

/// Local application provenance; this is never an SDK packet field.
#[derive(Clone, Copy, Debug)]
pub struct MotionOrigin {
    pub(crate) source_ms: Option<u64>,
    fallback_ms: u64,
    revision: u64,
    backend: u64,
}
pub(crate) fn capture_motion_origin(
    state: &AppState,
    command: &crate::types::TelemetryCommand,
) -> Option<MotionOrigin> {
    if !matches!(command, crate::types::TelemetryCommand::RetractPlumbing) {
        return None;
    }
    // Freeze the selected-mode context before the existing SDK clock call.
    // A mode commit while that call waits must invalidate this original intent.
    let revision = state.plumbing_drive.inner.lock().unwrap().desired.revision;
    let source_ms = state
        .topology_router
        .get()
        .and_then(|r| crate::system_clock::required_control_network_utc_ms(r).ok());
    Some(MotionOrigin {
        source_ms,
        fallback_ms: source_ms.unwrap_or_else(crate::telemetry_task::get_current_timestamp_ms),
        revision,
        backend: state.plumbing_drive.issuer,
    })
}
pub(crate) fn capture_remote_motion_origin(state: &AppState, source_ms: u64) -> MotionOrigin {
    let revision = state.plumbing_drive.inner.lock().unwrap().desired.revision;
    MotionOrigin {
        source_ms: Some(source_ms),
        fallback_ms: source_ms,
        revision,
        backend: state.plumbing_drive.issuer,
    }
}
/// Prepare original motion provenance outside final Stop/board admission.
/// Settings publish their busy flag under the shared RAM admission boundary.
fn motion_stamp(
    state: &AppState,
    router: &sedsnet::router::Router,
    origin: Option<MotionOrigin>,
) -> Result<u64, String> {
    let current = crate::system_clock::required_control_network_utc_ms(router).ok();
    let fallback = crate::system_clock::control_packet_timestamp_ms(router);
    let stamp = {
        let inner = state.plumbing_drive.inner.lock().unwrap();
        motion_ready_inner(state, &inner)?;
        let explicit = inner.desired.revision != 0 || inner.desired.mode != 0 || inner.durable;
        if let Some(context) = origin {
            if context.backend != state.plumbing_drive.issuer
                || context.revision != inner.desired.revision
            {
                return Err(
                    "Drive changed after this motion was requested; submit a fresh motion command."
                        .into(),
                );
            }
        } else if explicit {
            return Err("This motion lacks original drive context; use an updated primary or recovery station and submit a fresh command.".into());
        }
        let stamp = origin
            .and_then(|context| context.source_ms)
            .or_else(|| (!explicit).then(|| origin.map_or(fallback, |context| context.fallback_ms)))
            .ok_or("Original network UTC is required for the selected drive.")?;
        if explicit
            && (stamp <= inner.desired.source_ms
                || current.is_none_or(|now| {
                    now.saturating_sub(stamp) > REQUEST_TTL || stamp.saturating_sub(now) > 1_000
                }))
        {
            return Err(
                "Motion predates the drive selection or has expired; submit a fresh command."
                    .into(),
            );
        }
        stamp
    };
    Ok(stamp)
}
#[cfg(feature = "testing")]
pub(crate) fn validated_motion_stamp(
    state: &AppState,
    router: &sedsnet::router::Router,
    origin: Option<MotionOrigin>,
) -> Result<u64, String> {
    motion_stamp(state, router, origin)
}
pub(crate) fn validate_motion_origin(
    state: &AppState,
    origin: Option<MotionOrigin>,
) -> Result<(), String> {
    let router = state
        .topology_router
        .get()
        .ok_or("Telemetry router unavailable for plumbing motion")?;
    motion_stamp(state, router, origin).map(|_| ())
}
struct MotionWorkerReservation(Arc<AppState>);
impl Drop for MotionWorkerReservation {
    fn drop(&mut self) {
        self.0.plumbing_drive.motion_journal.finish();
        self.0.board_abort.changed.notify_one();
    }
}
/// Scheduling is bounded RAM admission. Only the owned blocking worker writes
/// the journal, and durable readback precedes any SDK motion ownership.
pub(crate) fn schedule_motion_under_dispatch(
    state: &Arc<AppState>,
    router: &sedsnet::router::Router,
    origin: Option<MotionOrigin>,
    logged: bool,
    ticket: crate::gse::LaunchAdmission,
) -> Result<(), String> {
    let handle = tokio::runtime::Handle::try_current().map_err(|_| "Motion worker unavailable")?;
    let source = motion_stamp(state, router, origin)?;
    let (mode, mode_revision, boot, epoch) =
        {
            let inner = state.plumbing_drive.inner.lock().unwrap();
            let proof = inner.motion_observed.as_ref().filter(|row|
            row.after.wall.elapsed().as_millis() <= FRESH_MS as u128
            && row.generation == crate::gse::automatic_generation(state)
            && row.mode_revision == inner.desired.revision
            && row.value.setting.ready() && !row.value.setting.busy
            && row.value.setting.mode.map(|m| m as u8) == Some(inner.desired.mode)
        ).ok_or("Check launch readiness for fresh actuator MotionV2 identity before retracting.")?;
            (
                inner.desired.mode,
                inner.desired.revision,
                proof.value.setting.generation,
                proof.value.setting.epoch,
            )
        };
    let safe = crate::board_recovery::actuator_cached_motion_safe_proof(state)
        .filter(|proof| {
            proof.boot == boot
                && proof.epoch == epoch
                && match mode {
                    0 => proof.outputs & !16 == 0,
                    2 => proof.outputs == 0,
                    _ => false,
                }
        })
        .ok_or("Fresh same-boot actuator safe-state proof is required before retraction.")?;
    let (generation, manual_epoch, observed_manual_epoch) = ticket.original_ownership();
    let record = motion_journal::Record {
        version: 1,
        original_utc_ms: source,
        mode,
        mode_revision,
        boot: safe.boot,
        epoch: safe.epoch,
        generation,
        manual_epoch,
        observed_manual_epoch,
    };
    let origin = origin.or(Some(MotionOrigin {
        source_ms: Some(source),
        fallback_ms: source,
        revision: mode_revision,
        backend: state.plumbing_drive.issuer,
    }));
    if !record.valid() {
        return Err("Retraction requires finite original UTC, identity and Stop epochs".into());
    }
    if !crate::gse::reserve_plumbing_worker_under_dispatch(state, ticket, || {
        state.plumbing_drive.motion_journal.reserve()
    }) {
        return Err("Retraction journal is closed/busy or original admission changed. Inspect status before an explicit retry.".into());
    }
    let state = state.clone();
    let reservation = MotionWorkerReservation(state.clone());
    handle.spawn_blocking(move || {
        let _reservation = reservation;
        let result = (|| -> Result<(),String> {
            let _claim = crate::gse::claim_operator_dispatch(&state,generation)
                .ok_or("Recovery closed admission before the retraction worker started")?;
            crate::gse::with_plumbing_admission(&state,ticket,||()).map_err(str::to_owned)?;
            state.plumbing_drive.motion_journal.write_pending(&record)?;
            let router=state.topology_router.get().ok_or("Telemetry router unavailable")?;
            // Sender, clock/schema lookup, endpoints, allocation and Packet ID
            // are prepared outside every final Stop/board admission mutex.
            let prepared = prepare_motion_packet(&state,router,origin,logged)?;
            if prepared.packet.timestamp()!=record.original_utc_ms {
                return Err("Original retraction timestamp changed; no motion sent".into());
            }
            crate::gse::with_prepared_motion_admission(&state,ticket,|| {
                if state.local_flight_state_snapshot() as u8 >= 7 {
                    return Err("Launch superseded the retraction worker; no motion sent".into());
                }
                {
                    let inner=state.plumbing_drive.inner.lock().unwrap();
                    if inner.desired.revision!=record.mode_revision || inner.desired.mode!=record.mode
                        ||inner.settings_busy||inner.pending_setting.is_some()||inner.storage_error.is_some()
                        ||Instant::now()>prepared.deadline
                    {return Err("Original drive context expired before SDK admission; no motion sent".into());}
                }
                router.try_tx_queue(prepared.packet).map_err(|error|error.to_string())
            }).map_err(str::to_owned)??;
            {
                let mut inner=state.plumbing_drive.inner.lock().unwrap();
                inner.last_motion_stamp=inner.last_motion_stamp.max(record.original_utc_ms);
            }
            crate::telemetry_task::flush_command_tx(router,"Durable original retraction intent");
            schedule_post_motion_queries(state.clone(),record.clone());
            Ok(())
        })();
        if let Err(error)=result {
            crate::web::emit_notification_warning(&state,format!("Retraction was not confirmed: {error}. Launch remains inhibited; inspect status before an explicit retry."));
        }
    });
    Ok(())
}
/// At most two read-only observations per admitted original intent. No lease
/// survives either sleep, and neither a reply nor an estimate queues motion.
fn schedule_post_motion_queries(state: Arc<AppState>, record: motion_journal::Record) {
    {
        let mut inner = state.plumbing_drive.inner.lock().unwrap();
        if inner.post_motion_query_record.as_ref() == Some(&record) {
            return;
        }
        inner.post_motion_query_record = Some(record.clone());
    }
    tokio::spawn(async move {
        for _ in 0..2 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if !post_motion_query_current(&state, &record) {
                return;
            }
            let Some(_claim) = crate::gse::claim_operator_dispatch(&state, record.generation)
            else {
                return;
            };
            if !post_motion_query_current(&state, &record) {
                return;
            }
            let _ = request_motion_query(&state);
        }
    });
}
fn post_motion_query_current(state: &AppState, record: &motion_journal::Record) -> bool {
    if state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::Acquire)
        || record.generation != crate::gse::automatic_generation(state)
        || record.manual_epoch != state.manual_abort_intent.epoch()
        || record.observed_manual_epoch != state.deferred_abort.manual_epoch()
        || crate::gse::manual_stop_pending(state)
    {
        return false;
    }
    let inner = state.plumbing_drive.inner.lock().unwrap();
    inner.post_motion_query_record.as_ref() == Some(record)
        && inner.desired.revision == record.mode_revision
        && inner.desired.mode == record.mode
        && !inner.settings_busy
        && inner.pending_setting.is_none()
        && inner.storage_error.is_none()
}
struct PreparedMotion {
    packet: Packet,
    deadline: Instant,
}
fn prepare_motion_packet(
    state: &AppState,
    router: &sedsnet::router::Router,
    origin: Option<MotionOrigin>,
    logged: bool,
) -> Result<PreparedMotion, String> {
    let stamp = motion_stamp(state, router, origin)?;
    let now =
        crate::system_clock::required_control_network_utc_ms(router).map_err(|e| e.to_string())?;
    if matches!(stamp, 0 | u64::MAX)
        || now.saturating_sub(stamp) > REQUEST_TTL
        || stamp.saturating_sub(now) > 1_000
    {
        return Err("Original retraction UTC expired; submit a fresh explicit request".into());
    }
    let kind = crate::telemetry_schema::data_type("ACTUATOR_COMMAND");
    let endpoints: Arc<[sedsnet::config::DataEndpoint]> = if logged {
        sedsnet::endpoints_from_datatype(kind)
    } else {
        Arc::from([
            crate::telemetry_schema::endpoint("GROUND_STATION"),
            crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
        ])
    };
    let packet = Packet::new(
        kind,
        &endpoints,
        &router.sender(),
        stamp,
        Arc::from([crate::rocket_commands::ActuatorBoardCommands::RetractPlumbing as u8]),
    )
    .map_err(|e| e.to_string())?;
    let remaining = REQUEST_TTL.saturating_sub(now.saturating_sub(stamp));
    Ok(PreparedMotion {
        packet,
        deadline: Instant::now() + std::time::Duration::from_millis(remaining),
    })
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error":message.into()}))).into_response()
}
fn primary(state: &AppState) -> bool {
    !crate::rrls::is_rrls()
        && state
            .topology_router
            .get()
            .is_some_and(|r| r.sender().as_ref() == "GS")
}
fn fresh(state: &AppState, observation: &Observation) -> bool {
    observation.recovery_generation == crate::gse::automatic_generation(state)
        && observation.after.age().is_some_and(|age| age <= FRESH_MS)
        && !matches!(observation.value.generation, 0 | u64::MAX)
        && !matches!(observation.value.epoch, 0 | u32::MAX)
}
fn view(state: &AppState, commands: bool) -> Value {
    let is_primary = primary(state);
    let idle = state.local_flight_state_snapshot() == FlightState::Idle;
    let proof = crate::board_recovery::actuator_idle_proof(state);
    let retraction = retraction_gate(state);
    let history_fault = state.plumbing_drive.motion_journal.fault_context();
    let inner = state.plumbing_drive.inner.lock().unwrap();
    let observed = inner.observed.as_ref();
    let valid = observed.is_some_and(|row| fresh(state, row));
    let board = observed.map(|row| {let s=row.value;json!({
        "mode":s.mode.map(Mode::name),"generation":format!("{:016x}",s.generation),"epoch":s.epoch,
        "fresh":fresh(state,row),"ready":s.ready(),"age_ms":row.after.age(),"result":s.result_name(),"busy":s.busy,
        "active_action":(["none","retract","extend"][s.active_action as usize]),"remaining_ms":s.remaining_ms,
        "capabilities":{"stepper":s.capabilities&1!=0,"h_bridge":false,"auxiliary":s.capabilities&4!=0,"extend":false}
    })});
    json!({"primary":is_primary,"can_configure":is_primary&&commands&&idle&&valid&&proof.is_some_and(|p|observed.is_some_and(|r|p.boot==r.value.generation&&p.epoch==r.value.epoch)&&p.outputs&!16==0)&&!inner.settings_busy&&inner.storage_error.is_none()&&observed.is_some_and(|row|!row.value.busy&&matches!(row.value.result,0|6)),
        "backend_instance":format!("{:016x}",state.plumbing_drive.issuer),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(state)),
        "desired":{"mode":Mode::decode(inner.desired.mode).map(Mode::name),"revision":inner.desired.revision,"durable":inner.durable},
        "board":board,"pending":inner.pending_setting.is_some()||!valid||observed.is_none_or(|row|!row.value.ready()||row.value.mode.map(|m|m as u8)!=Some(inner.desired.mode)),
        "retraction_interlock":retraction,
        "retraction_history_fault":history_fault,
        "motion":motion_view(state,&inner),
        "settings_inflight":inner.settings_busy,"message":inner.storage_error.as_ref().unwrap_or(&inner.message)})
}

/// Explicit read-only demand only. A failed/legacy peer never becomes ready.
pub(crate) fn request_motion_query(state: &AppState) -> Result<(), String> {
    if !primary(state) {
        return Err("Motion provenance checks require the primary GS".into());
    }
    let router = state
        .topology_router
        .get()
        .ok_or("Telemetry router unavailable")?;
    let nonce = rand::random::<u64>().max(1);
    let payload =
        motion_wire::query(nonce, state.plumbing_drive.issuer).ok_or("Invalid motion query")?;
    let stamp = crate::system_clock::control_packet_timestamp_ms(router);
    let packet = Packet::new(
        crate::telemetry_schema::data_type(REQUEST_TYPE),
        &[crate::telemetry_schema::endpoint("ACTUATOR_BOARD")],
        &router.sender(),
        stamp,
        Arc::from(payload),
    )
    .map_err(|e| e.to_string())?;
    {
        let mut inner = state.plumbing_drive.inner.lock().unwrap();
        if inner
            .motion_queries
            .back()
            .is_some_and(|q| q.sent.age().is_some_and(|age| age < 1_000))
        {
            return Ok(());
        }
        if inner.motion_queries.len() >= 4 {
            inner.motion_queries.pop_front();
        }
        let mode_revision = inner.desired.revision;
        inner.motion_queries.push_back(MotionQuery {
            nonce,
            sent: Moment::now(),
            complete: false,
            generation: crate::gse::automatic_generation(state),
            mode_revision,
        });
    }
    if let Err(error) = router.tx_queue(packet) {
        state
            .plumbing_drive
            .inner
            .lock()
            .unwrap()
            .motion_queries
            .retain(|q| q.nonce != nonce);
        return Err(error.to_string());
    }
    Ok(())
}
pub(crate) fn receive_motion_status(state: &AppState, packet: &Packet) {
    if packet.data_type() != crate::telemetry_schema::data_type(motion_wire::TYPE)
        || state.board_from_network_sender(packet.sender()) != Some(Board::ActuatorBoard)
        || packet.endpoints() != [crate::telemetry_schema::endpoint("GROUND_STATION")]
    {
        return;
    }
    let Some(value) = motion_wire::MotionStatus::decode(packet.payload()) else {
        return;
    };
    if value.setting.issuer != state.plumbing_drive.issuer {
        return;
    }
    let mut inner = state.plumbing_drive.inner.lock().unwrap();
    let Some(query) = inner
        .motion_queries
        .iter_mut()
        .find(|q| q.nonce == value.setting.nonce)
    else {
        return;
    };
    if query.complete
        || query.generation != crate::gse::automatic_generation(state)
        || query.sent.age().is_none_or(|age| age > REQUEST_TTL)
    {
        return;
    }
    query.complete = true;
    let after = query.sent;
    let generation = query.generation;
    let mode_revision = query.mode_revision;
    if mode_revision != inner.desired.revision
        || inner
            .motion_observed
            .as_ref()
            .is_some_and(|old| old.after.wall >= after.wall)
    {
        return;
    }
    let mut value = value;
    if let Some(old) = inner.motion_observed.as_ref()
        && old.value.original_utc_ms == value.original_utc_ms
        && old.value.origin_epoch == value.origin_epoch
        && old.value.setting.generation == value.setting.generation
        && old.value.accepted_action == value.accepted_action
        && (value.minimum_delay_remaining_ms > old.value.minimum_delay_remaining_ms
            || old.value.lifecycle == 2 && value.lifecycle == 1)
    {
        // A known operation cannot restart its accepted monotonic interval.
        // An inconsistent new proof closes readiness rather than extending a
        // countdown or borrowing the previous successful proof.
        value.lifecycle = 4;
    }
    inner.motion_observed = Some(MotionObservation {
        value,
        after,
        received: Instant::now(),
        generation,
        mode_revision,
    });
    drop(inner);
    state.board_abort.changed.notify_one();
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RetractionGate {
    pub(crate) configured: bool,
    pub(crate) enforced: bool,
    pub(crate) allowed: bool,
    pub(crate) phase: String,
    pub(crate) remaining_ms: Option<u32>,
    pub(crate) message: String,
}
/// RAM-only final admission check. Fresh query ages use local monotonic time;
/// network UTC remains identity only, and disk/SDK work never enters this gate.
#[cfg(feature = "testing")]
pub(crate) fn record_modeled_retraction(
    state: &AppState,
    source: u64,
    ticket: crate::gse::LaunchAdmission,
) -> bool {
    let (generation, manual_epoch, observed_manual_epoch) = ticket.original_ownership();
    let mut inner = state.plumbing_drive.inner.lock().unwrap();
    if inner.modeled_retraction.is_some_and(|old| {
        old.source == source
            && old.generation == generation
            && old.manual_epoch == manual_epoch
            && old.observed_manual_epoch == observed_manual_epoch
            && old.mode_revision == inner.desired.revision
    }) {
        return false;
    }
    inner.modeled_retraction = Some(ModeledRetraction {
        source,
        started: Instant::now(),
        generation,
        manual_epoch,
        observed_manual_epoch,
        mode_revision: inner.desired.revision,
    });
    true
}
#[cfg(feature = "testing")]
pub(crate) fn invalidate_modeled_retraction(state: &AppState) {
    state.board_abort.change_launch_context_if(|| {
        state
            .plumbing_drive
            .inner
            .lock()
            .unwrap()
            .modeled_retraction
            .take()
            .is_some()
    });
}
#[cfg(feature = "testing")]
fn modeled_remaining(started: Instant, now: Instant) -> u32 {
    45_000u32.saturating_sub(
        now.saturating_duration_since(started)
            .as_millis()
            .min(u32::MAX as u128) as u32,
    )
}
pub(crate) fn retraction_gate(state: &AppState) -> RetractionGate {
    let (record, error) = state.plumbing_drive.motion_journal.snapshot();
    let pending = state.plumbing_drive.motion_journal.pending();
    let sealed = state.plumbing_drive.motion_journal.sealed();
    let (desired, durable, config_error, settings_busy, pending_setting, motion) = {
        let inner = state.plumbing_drive.inner.lock().unwrap();
        (
            inner.desired,
            inner.durable,
            inner.storage_error.clone(),
            inner.settings_busy,
            inner.pending_setting.is_some(),
            inner.motion_observed.as_ref().map(|row| {
                (
                    row.value,
                    row.after.wall,
                    row.received,
                    row.generation,
                    row.mode_revision,
                )
            }),
        )
    };
    #[cfg(feature = "testing")]
    let modeled = state
        .plumbing_drive
        .inner
        .lock()
        .unwrap()
        .modeled_retraction;
    #[cfg(feature = "testing")]
    let modeled_configured = crate::flight_sim::sim_mode_enabled() && modeled.is_some();
    #[cfg(not(feature = "testing"))]
    let modeled_configured = false;
    let configured = modeled_configured
        || durable
        || config_error.is_some()
        || record.is_some()
        || error.is_some()
        || state.plumbing_drive.motion_journal.sealed()
        || pending
        || settings_busy
        || pending_setting;
    #[cfg(feature = "hitl_mode")]
    let enforced = configured && state.hitl_safety_interlock_enabled(0);
    #[cfg(not(feature = "hitl_mode"))]
    let enforced = configured;
    let make = |allowed, phase: &str, remaining_ms, message: &str| RetractionGate {
        configured,
        enforced,
        allowed,
        phase: phase.into(),
        remaining_ms,
        message: message.into(),
    };
    if !configured {
        return make(
            true,
            "not_configured",
            None,
            "Fill plumbing is not configured; its retraction delay is inactive.",
        );
    }
    if config_error.is_some() || error.is_some() {
        return make(
            false,
            "fault",
            None,
            "Plumbing configuration or retraction history needs explicit authorized attention. Launch remains inhibited.",
        );
    }
    if pending || settings_busy || pending_setting {
        return make(
            false,
            "pending",
            None,
            "A plumbing owner or setting is pending. Wait for its actual terminal outcome before Launch.",
        );
    }
    if sealed && record.is_none() {
        return make(false,"needs_check",None,"Retraction history is CLOSED. A fresh durable retraction intent and actual checks are required; optional delay settings do not bypass uncertainty.");
    }
    #[cfg(feature = "testing")]
    if crate::flight_sim::sim_mode_enabled() {
        let Some(model) = modeled else {
            return make(
                false,
                "needs_check",
                None,
                "Explicitly trigger modeled retraction before the Testing minimum-delay check.",
            );
        };
        if model.mode_revision != desired.revision
            || model.generation != crate::gse::automatic_generation(state)
            || model.manual_epoch != state.manual_abort_intent.epoch()
            || model.observed_manual_epoch != state.deferred_abort.manual_epoch()
        {
            return make(
                false,
                "needs_check",
                None,
                "The modeled retraction belongs to an older Stop/reset or drive context. An explicit new model intent is required.",
            );
        }
        let remaining = modeled_remaining(model.started, Instant::now());
        return if remaining == 0 {
            make(
                true,
                "ready",
                Some(0),
                "Testing model: 45 monotonic seconds elapsed after accepted modeled retraction; no hardware or physical-disconnection proof is claimed.",
            )
        } else {
            make(
                false,
                "countdown",
                Some(remaining),
                "Testing model: Launch waits 45 monotonic seconds from the explicitly accepted modeled retraction.",
            )
        };
    }
    let Some(record) = record else {
        return if enforced {
            make(
                false,
                "needs_check",
                None,
                "Retract the configured fill plumbing, then check actual completion before Launch.",
            )
        } else {
            make(
                true,
                "hitl_bypassed",
                None,
                "HITL retraction minimum-delay interlock is disabled.",
            )
        };
    };
    if !record.valid()
        || record.mode != desired.mode
        || record.mode_revision != desired.revision
        || record.original_utc_ms <= desired.source_ms
        || record.generation != crate::gse::automatic_generation(state)
        || record.manual_epoch != state.manual_abort_intent.epoch()
        || record.observed_manual_epoch != state.deferred_abort.manual_epoch()
        || crate::gse::manual_stop_pending(state)
    {
        return make(
            false,
            "needs_check",
            None,
            "The retained retraction belongs to an older drive or Stop/reset context. Make a fresh explicit retraction after resolving interlocks.",
        );
    }
    let Some((proof, after, received, generation, revision)) = motion else {
        return make(
            false,
            "needs_check",
            None,
            "Check actual retraction status; retained intent alone does not prove motion or elapsed time.",
        );
    };
    if generation != record.generation
        || revision != record.mode_revision
        || !proof.setting.ready()
        || proof.setting.mode.map(|mode| mode as u8) != Some(record.mode)
        || proof.setting.generation != record.boot
        || proof.setting.epoch != record.epoch
        || proof.origin_epoch != record.epoch
        || proof.original_utc_ms != record.original_utc_ms
        || proof.accepted_action != 1
        || !matches!(proof.lifecycle, 1 | 2)
    {
        return make(
            false,
            "needs_check",
            None,
            "Fresh same-operation retraction proof is required. Check readiness, then make a separate Launch request.",
        );
    }
    if after.elapsed().as_millis() > FRESH_MS as u128 {
        let remaining = (proof.minimum_delay_remaining_ms != 0).then(|| {
            proof
                .minimum_delay_remaining_ms
                .saturating_sub(received.elapsed().as_millis().min(u32::MAX as u128) as u32)
        });
        return make(
            false,
            if remaining.is_some() {
                "countdown"
            } else {
                "needs_check"
            },
            remaining,
            "Retraction timing is an estimate until fresh same-operation completion is checked. Launch remains locked.",
        );
    }
    if !enforced {
        // The known board-owned motion is distinct from a delayed host worker.
        // HITL may bypass min45; uncertainty/Stop/recovery still never bypasses.
        return make(
            true,
            "hitl_bypassed",
            None,
            "HITL minimum-delay interlock is disabled; actual retraction identity is known.",
        );
    }
    if proof.minimum_delay_remaining_ms != 0 {
        let remaining = proof
            .minimum_delay_remaining_ms
            .saturating_sub(received.elapsed().as_millis().min(u32::MAX as u128) as u32);
        return make(
            false,
            "countdown",
            Some(remaining),
            "Launch is locked until at least 45 seconds after the actuator actually accepted retraction. Check fresh completion when the countdown ends.",
        );
    }
    if !record.matches_motion(proof) {
        return make(
            false,
            "needs_check",
            Some(0),
            "The minimum interval alone is insufficient: actual retraction must complete without busy, Stop or fault.",
        );
    }
    let Some(safe) = crate::board_recovery::actuator_cached_motion_safe_proof(state) else {
        return make(
            false,
            "needs_check",
            Some(0),
            "Fresh actual actuator safe-state readback is required before Launch.",
        );
    };
    if safe.boot != record.boot
        || safe.epoch != record.epoch
        || match record.mode {
            0 => safe.outputs & !16 != 0,
            2 => safe.outputs != 0,
            _ => true,
        }
    {
        return make(
            false,
            "needs_check",
            Some(0),
            "Actuator identity or commanded outputs do not match completed retraction readiness.",
        );
    }
    make(
        true,
        "ready",
        Some(0),
        "The actual retraction minimum interval and completion checks passed; other Launch interlocks still apply. This is not physical-disconnection proof.",
    )
}
fn motion_view(state: &AppState, inner: &Inner) -> Value {
    let Some(row) = inner.motion_observed.as_ref() else {
        return json!({"known":false,"ready":false,"message":"Check actual accepted retraction provenance; legacy status is insufficient."});
    };
    let current = row.generation == crate::gse::automatic_generation(state)
        && row.mode_revision == inner.desired.revision
        && row.after.age().is_some_and(|age| age <= FRESH_MS);
    let (intent, journal_error) = state.plumbing_drive.motion_journal.snapshot();
    let identity_matches = intent
        .as_ref()
        .is_some_and(|intent| intent.matches_motion(row.value));
    json!({"known":true,"fresh":current,"ready":false,
        "durable_identity_matches":identity_matches,"journal_error":journal_error,
        "worker_pending":state.plumbing_drive.motion_journal.pending(),
        "completed_minimum_delay_proof":current&&row.value.completed_retract(),
        "boot":format!("{:016x}",row.value.setting.generation),"epoch":row.value.setting.epoch,
        "origin_epoch":row.value.origin_epoch,"source_utc_ms":row.value.original_utc_ms,
        "accepted_action":row.value.accepted_action,"lifecycle":row.value.lifecycle,
        "busy":row.value.setting.busy,"minimum_delay_remaining_ms":row.value.minimum_delay_remaining_ms,
        "message":"Actuator proof is not durable host intent or mechanical position proof."})
}
pub(crate) fn receive_status(state: &AppState, packet: &Packet) {
    if packet.data_type() != crate::telemetry_schema::data_type(STATUS_TYPE)
        || state.board_from_network_sender(packet.sender()) != Some(Board::ActuatorBoard)
        || packet.endpoints() != [crate::telemetry_schema::endpoint("GROUND_STATION")]
    {
        return;
    }
    let Some(value) = Status::decode(packet.payload()) else {
        return;
    };
    if value.issuer != state.plumbing_drive.issuer {
        return;
    }
    let mut inner = state.plumbing_drive.inner.lock().unwrap();
    let Some(context) = inner
        .requests
        .iter_mut()
        .find(|c| c.request.nonce == value.nonce)
    else {
        return;
    };
    if context.complete
        || context.recovery_generation != crate::gse::automatic_generation(state)
        || context.sent.age().is_none_or(|age| age > REQUEST_TTL)
    {
        return;
    }
    if value.result == 1 {
        return;
    } // Pending is not a completed, fresh proof.
    let after = context.sent;
    let recovery_generation = context.recovery_generation;
    let desired_revision = context.desired_revision;
    context.complete = true;
    let setting = context.request.mode;
    let expected = (context.request.generation, context.request.epoch);
    if setting.is_some()
        && value.result == 0
        && (setting != value.mode || expected != (value.generation, value.epoch))
    {
        return;
    }
    if setting.is_some() && inner.pending_setting == Some(value.nonce) {
        inner.pending_setting = None;
    }
    // A fresh QUERY challenge can reconcile a lost SET reply or a GS restart.
    // Matching persisted board value is still distinct from delivery ACK.
    if setting.is_none()
        && desired_revision == inner.desired.revision
        && value.ready()
        && value.mode.map(|mode| mode as u8) == Some(inner.desired.mode)
    {
        inner.pending_setting = None;
    }
    inner.message = if value.result == 0 {
        "Actual actuator readback received; physical position is not verified.".into()
    } else {
        format!("Actuator drive operation returned {}.", value.result_name())
    };
    inner.observed = Some(Observation {
        value,
        after,
        recovery_generation,
    });
}

struct PreparedRequest {
    request: Request,
    packet: Packet,
    sent: Moment,
}
fn prepare_request(
    state: &AppState,
    request: Request,
    stamp: u64,
) -> Result<PreparedRequest, String> {
    let router = state
        .topology_router
        .get()
        .ok_or("Telemetry router unavailable")?;
    let bytes = request.encode().ok_or("Invalid drive request identity")?;
    let packet = Packet::new(
        crate::telemetry_schema::data_type(REQUEST_TYPE),
        &[crate::telemetry_schema::endpoint("ACTUATOR_BOARD")],
        &router.sender(),
        stamp,
        Arc::from(bytes),
    )
    .map_err(|_| "Cannot prepare drive request")?;
    Ok(PreparedRequest {
        request,
        packet,
        sent: Moment::now(),
    })
}
fn queue(state: &AppState, request: Request, stamp: u64) -> Result<(), String> {
    let prepared = prepare_request(state, request, stamp)?;
    queue_prepared_request(state, prepared, false)
}
fn queue_prepared_request(
    state: &AppState,
    prepared: PreparedRequest,
    nonwaiting: bool,
) -> Result<(), String> {
    let PreparedRequest {
        request,
        packet,
        sent,
    } = prepared;
    let router = state
        .topology_router
        .get()
        .ok_or("Telemetry router unavailable")?;
    {
        let mut inner = state.plumbing_drive.inner.lock().unwrap();
        if inner
            .requests
            .iter()
            .any(|context| context.request.nonce == request.nonce)
        {
            return Err("Repeated request nonce".into());
        }
        if inner.requests.len() == 4 {
            inner.requests.pop_front();
        }
        let desired_revision = inner.desired.revision;
        inner.requests.push_back(Context {
            request,
            sent,
            complete: false,
            recovery_generation: crate::gse::automatic_generation(state),
            desired_revision,
        });
        if request.mode.is_some() {
            inner.pending_setting = Some(request.nonce);
        }
    }
    // SDK ingress can take inner; never call into the router while holding it.
    let admission = if nonwaiting {
        router.try_tx_queue(packet)
    } else {
        router.tx_queue(packet)
    };
    if let Err(error) = admission {
        state
            .plumbing_drive
            .inner
            .lock()
            .unwrap()
            .requests
            .retain(|c| c.request.nonce != request.nonce);
        // Desired remains pending; only a new correlated query can reconcile it.
        return Err(format!("Drive request was not admitted: {error}"));
    }
    Ok(())
}

pub(crate) fn routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/api/actuator/plumbing/status", get(status))
        .route("/api/actuator/plumbing/query", post(query))
        .route("/api/actuator/plumbing/settings", post(settings))
        .route(
            "/api/actuator/plumbing/retraction-history/inspect",
            post(inspect_history_fault),
        )
        .route(
            "/api/actuator/plumbing/retraction-history/reset",
            post(reset_history_fault),
        )
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
            Ok(p) => p,
            Err(r) => return r,
        };
    Json(view(
        &state,
        principal.permissions.send_commands && principal.allows_command_name("RetractPlumbing"),
    ))
    .into_response()
}
async fn query(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
            Ok(p) => p,
            Err(r) => return r,
        };
    if !primary(&state) {
        return error(
            StatusCode::CONFLICT,
            "Drive queries require the current primary GS.",
        );
    }
    {
        let inner = state.plumbing_drive.inner.lock().unwrap();
        if inner
            .requests
            .back()
            .is_some_and(|c| c.sent.age().is_some_and(|age| age < 1000))
        {
            drop(inner);
            return Json(view(
                &state,
                principal.permissions.send_commands
                    && principal.allows_command_name("RetractPlumbing"),
            ))
            .into_response();
        }
    }
    let router = state.topology_router.get().unwrap();
    let request = Request {
        mode: None,
        nonce: rand::random::<u64>().max(1),
        generation: 0,
        epoch: 0,
        issuer: state.plumbing_drive.issuer,
    };
    if let Err(message) = queue(
        &state,
        request,
        crate::system_clock::control_packet_timestamp_ms(router),
    ) {
        return error(StatusCode::SERVICE_UNAVAILABLE, message);
    }
    Json(view(
        &state,
        principal.permissions.send_commands && principal.allows_command_name("RetractPlumbing"),
    ))
    .into_response()
}

#[derive(Deserialize)]
struct SettingsRequest {
    mode: String,
    expected_generation: String,
    expected_epoch: u32,
    backend_instance: String,
    expected_revision: u64,
    expected_reset_epoch: String,
    #[serde(default)]
    release_stepper: bool,
}
async fn settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<SettingsRequest>,
) -> Response {
    let generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
    let admission = crate::gse::plumbing_admission_ticket(&state);
    let origin = state
        .topology_router
        .get()
        .and_then(|router| crate::system_clock::required_control_network_utc_ms(router).ok());
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(r) => return r,
        };
    if !principal.permissions.send_commands || !principal.allows_command_name("RetractPlumbing") {
        return error(
            StatusCode::FORBIDDEN,
            "Fill-line command permission required.",
        );
    }
    let Some(origin) = origin else {
        return error(
            StatusCode::CONFLICT,
            "Network UTC is required before changing drives.",
        );
    };
    let worker = state.clone();
    match tokio::task::spawn_blocking(move || {
        apply_settings(&worker, body, generation, origin, admission)
    })
    .await
    {
        Ok(Ok(changed)) => (
            if changed {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            },
            Json(view(&state, true)),
        )
            .into_response(),
        Ok(Err((status, message))) => error(status, message),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Drive settings worker failed; query actual state before retrying.",
        ),
    }
}

fn apply_settings(
    state: &AppState,
    body: SettingsRequest,
    generation: u64,
    origin: u64,
    admission: crate::gse::LaunchAdmission,
) -> Result<bool, (StatusCode, String)> {
    let fail = |message: &str| (StatusCode::CONFLICT, message.to_string());
    let mode = Mode::from_name(&body.mode).ok_or_else(|| fail("Invalid drive selection."))?;
    let expected = u64::from_str_radix(&body.expected_generation, 16)
        .map_err(|_| fail("Invalid board generation."))?;
    let _settings = state.plumbing_drive.settings_gate.lock().unwrap();
    if body.expected_reset_epoch != format!("{generation:016x}") {
        return Err(fail("Recovery context changed; query again."));
    }
    let _owner = crate::gse::claim_plumbing_settings_dispatch(state, generation)
        .ok_or_else(|| fail("Drive change cancelled by recovery."))?;
    let _busy = SettingsBusy(&state.plumbing_drive);
    crate::gse::with_plumbing_admission(state, admission, || ())
        .map_err(fail)?;
    if !primary(state) || state.local_flight_state_snapshot() != FlightState::Idle {
        return Err(fail("Drive changes require idle primary GS."));
    }
    let current_time = state
        .topology_router
        .get()
        .and_then(|router| crate::system_clock::required_control_network_utc_ms(router).ok())
        .ok_or_else(|| fail("Network UTC is unavailable."))?;
    if current_time.saturating_sub(origin) > 5_000 || origin.saturating_sub(current_time) > 1_000 {
        return Err(fail(
            "Drive request expired during authorization; submit a new request.",
        ));
    }
    if body.backend_instance != format!("{:016x}", state.plumbing_drive.issuer) {
        return Err(fail("Backend changed; query again."));
    }
    let board_proof = crate::board_recovery::actuator_idle_proof(state).ok_or_else(||fail("Fresh actual actuator Idle/output readback is required. Query both board and drive state."))?;
    let desired = {
        let mut inner = state.plumbing_drive.inner.lock().unwrap();
        if inner.storage_error.is_some() {
            return Err(fail(
                "Saved configuration requires repair before changing drives.",
            ));
        }
        if inner.desired.revision != body.expected_revision {
            return Err(fail("Saved drive changed; query again."));
        }
        let observed = inner
            .observed
            .as_ref()
            .filter(|row| fresh(state, row))
            .ok_or_else(|| fail("Fresh actuator query is required."))?;
        let current = observed.value;
        if !matches!(current.result, 0 | 6)
            || current.busy
            || current.generation != expected
            || current.epoch != body.expected_epoch
        {
            return Err(fail("Actuator changed or is moving; query again."));
        }
        if board_proof.boot != current.generation || board_proof.epoch != current.epoch {
            return Err(fail(
                "Actuator board and drive readbacks disagree; query both again.",
            ));
        }
        if current.mode == Some(Mode::HBridge) || (current.mode.is_none() && current.result != 6) {
            return Err(fail(
                "Drive metadata is unsupported; query actual state before changing drives.",
            ));
        }
        if body.release_stepper && current.mode == Some(Mode::Auxiliary) {
            return Err(fail(
                "Holding torque release is only valid when explicitly switching from legacy or unknown metadata to auxiliary.",
            ));
        }
        if mode == Mode::Auxiliary
            && current.ready()
            && current.mode == Some(mode)
            && inner.desired.mode == mode as u8
            && inner.pending_setting.is_none()
            && board_proof.outputs == 0
            && !body.release_stepper
        {
            inner.message =
                "Auxiliary drive is already confirmed; no new setting was saved.".into();
            return Ok(false);
        }
        if mode == Mode::Stepper
            && inner.pending_setting.is_none()
            && current.ready()
            && current.mode == Some(Mode::Stepper)
            && board_proof.outputs & !16 == 0
        {
            if inner.desired.mode == 0 && !body.release_stepper {
                inner.message="Legacy stepper is already selected and holding torque is unchanged; no new setting was saved.".into();
                return Ok(false);
            }
            return Err(fail(
                "Stepper holding torque is active; no changed desired setting was saved.",
            ));
        }
        let release = mode == Mode::Auxiliary
            && current.mode != Some(Mode::Auxiliary)
            && body.release_stepper;
        if board_proof.outputs & !16 != 0 || (board_proof.outputs & 16 != 0 && !release) {
            return Err(fail(
                "All actuator outputs must be off; only explicit legacy-to-aux release may release idle stepper holding torque.",
            ));
        }
        if body.release_stepper && mode != Mode::Auxiliary {
            return Err(fail(
                "Holding torque release is only valid for an explicit switch to auxiliary.",
            ));
        }
        if !mode.supported(current.capabilities) {
            return Err(fail(
                "Selected drive is unavailable on this board revision.",
            ));
        }
        if current.mode != Some(Mode::Auxiliary) && mode == Mode::Auxiliary && !body.release_stepper
        {
            return Err(fail(
                "Explicit release of idle stepper holding torque is required.",
            ));
        }
        if origin <= inner.desired.source_ms || origin <= inner.last_motion_stamp {
            return Err(fail("Drive request predates the saved selection."));
        }
        Desired {
            version: 1,
            mode: mode as u8,
            revision: inner
                .desired
                .revision
                .checked_add(1)
                .ok_or_else(|| fail("Drive revision exhausted."))?,
            source_ms: origin,
        }
    };
    let nonce = rand::random::<u64>().max(1);
    let saved = desired.save(&state.plumbing_drive.path, nonce);
    // The reload may also perform IO; keep it outside every SDK-ingress lock.
    let reloaded = if saved.is_err() {
        Desired::load(&state.plumbing_drive.path).ok().flatten()
    } else {
        None
    };
    {
        let mut inner = state.plumbing_drive.inner.lock().unwrap();
        if let Err(message) = saved {
            // Rename may have succeeded before a directory flush failed. Keep
            // the visible value honest, inhibit apply, and never queue SET.
            if let Some(actual) = reloaded {
                inner.desired = actual;
            }
            inner.durable = false;
            inner.storage_error = Some(message.clone());
            return Err((StatusCode::INSUFFICIENT_STORAGE, message));
        }
        inner.desired = desired;
        inner.durable = true;
        inner.pending_setting = Some(nonce);
        inner.message = "Desired drive saved; actual actuator confirmation is pending.".into();
        if inner.observed.as_ref().is_none_or(|row| {
            !fresh(state, row)
                || row.value.busy
                || row.value.generation != expected
                || row.value.epoch != body.expected_epoch
        }) {
            return Err(fail(
                "Desired drive saved, but actuator changed; no SET was queued. Query again.",
            ));
        }
    }
    let final_proof = crate::board_recovery::actuator_idle_proof(state);
    if state.local_flight_state_snapshot() != FlightState::Idle
        || final_proof.is_none_or(|p| {
            p.boot != expected
                || p.epoch != body.expected_epoch
                || p.outputs & !16 != 0
                || (p.outputs & 16 != 0 && !(mode == Mode::Auxiliary && body.release_stepper))
        })
    {
        return Err(fail(
            "Desired drive saved, but actual actuator Idle/output proof changed; no SET was queued. Query again.",
        ));
    }
    let now = state
        .topology_router
        .get()
        .and_then(|r| crate::system_clock::required_control_network_utc_ms(r).ok());
    if now.is_none_or(|now| {
        now.saturating_sub(origin) > REQUEST_TTL || origin.saturating_sub(now) > 1_000
    }) {
        return Err(fail(
            "Desired drive saved, but its original request expired before admission. Query and explicitly retry.",
        ));
    }
    let prepared = prepare_request(
        state,
        Request {
            mode: Some(mode),
            nonce,
            generation: expected,
            epoch: body.expected_epoch,
            issuer: state.plumbing_drive.issuer,
        },
        origin,
    )
    .map_err(|message| (StatusCode::SERVICE_UNAVAILABLE, message))?;
    crate::gse::with_plumbing_admission(state, admission, || {
        queue_prepared_request(state, prepared, true).map(|()| {
            // A fresh selected-mode intent must not inherit the old mode's
            // software dedup. Keep accepted history and all unrelated entries.
            state
                .last_command_ms
                .lock()
                .unwrap()
                .remove("dedup:RetractPlumbing");
        })
    })
    .map_err(fail)?
    .map(|()| true)
    .map_err(|message| (StatusCode::SERVICE_UNAVAILABLE, message))
}

/// Legacy firmware retains its existing behavior until an explicit selection.
/// A selected auxiliary/bridge drive needs fresh matching actual readback.
#[cfg(any(test, not(feature = "hitl_mode")))]
pub(crate) fn motion_ready(state: &AppState) -> Result<(), String> {
    let inner = state.plumbing_drive.inner.lock().unwrap();
    motion_ready_inner(state, &inner)
}
fn motion_ready_inner(state: &AppState, inner: &Inner) -> Result<(), String> {
    if inner.settings_busy || inner.pending_setting.is_some() || inner.storage_error.is_some() {
        return Err("Drive configuration is not ready.".into());
    }
    if inner.desired.mode == 0
        && inner.desired.revision == 0
        && !inner.durable
        && inner.observed.is_none()
    {
        return Ok(());
    }
    let observed = inner
        .observed
        .as_ref()
        .filter(|row| fresh(state, row))
        .ok_or("Fresh actuator drive readback is required.")?;
    if !observed.value.ready()
        || observed.value.busy
        || observed.value.mode.map(|mode| mode as u8) != Some(inner.desired.mode)
    {
        return Err("Actuator is moving or drive confirmation is pending.".into());
    }
    Ok(())
}

#[cfg(any(test, not(feature = "hitl_mode")))]
pub(crate) fn completion_timeout_ms(state: &AppState) -> u64 {
    let inner = state.plumbing_drive.inner.lock().unwrap();
    if inner.observed.is_some() {
        if inner.desired.mode == 0 {
            605_000
        } else {
            50_000
        }
    } else {
        5_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(state: &AppState, nonce: u64, mode: u8, result: u8) -> [u8; 40] {
        let mut bytes = [0; 40];
        bytes[0] = 1;
        bytes[1] = result;
        bytes[2] = mode;
        bytes[3] = 5;
        bytes[4..12].copy_from_slice(&nonce.to_le_bytes());
        bytes[12..20].copy_from_slice(&7u64.to_le_bytes());
        bytes[20..24].copy_from_slice(&3u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&state.plumbing_drive.issuer.to_le_bytes());
        bytes
    }
    fn context(state: &AppState, nonce: u64, mode: Option<Mode>) {
        state
            .plumbing_drive
            .inner
            .lock()
            .unwrap()
            .requests
            .push_back(Context {
                request: Request {
                    mode,
                    nonce,
                    generation: 7,
                    epoch: 3,
                    issuer: state.plumbing_drive.issuer,
                },
                sent: Moment::now(),
                complete: false,
                recovery_generation: crate::gse::automatic_generation(state),
                desired_revision: 0,
            });
    }
    fn deliver(state: &AppState, sender: &str, bytes: &[u8]) {
        let packet = Packet::new(
            crate::telemetry_schema::data_type(STATUS_TYPE),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            sender,
            1,
            Arc::from(bytes),
        )
        .unwrap();
        receive_status(state, &packet);
    }
    #[tokio::test]
    async fn wrong_sender_nonce_or_payload_never_confirms_a_selection() {
        let state = crate::state::tests::test_app_state().await;
        context(&state, 9, None);
        deliver(&state, "VB", &frame(&state, 9, 0, 0));
        deliver(&state, "AB", &frame(&state, 8, 0, 0));
        assert!(
            Packet::new(
                crate::telemetry_schema::data_type(STATUS_TYPE),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "AB",
                1,
                Arc::from(&frame(&state, 9, 0, 0)[..39])
            )
            .is_err()
        );
        assert!(
            state
                .plumbing_drive
                .inner
                .lock()
                .unwrap()
                .observed
                .is_none()
        );
        deliver(&state, "AB", &frame(&state, 9, 0, 0));
        let value = view(&state, true);
        assert_eq!(value["board"]["mode"], "stepper");
        assert_eq!(value["board"]["fresh"], true);
        assert_eq!(value["board"]["capabilities"]["h_bridge"], false);
    }
    #[tokio::test]
    async fn duplicate_reply_cannot_slide_freshness_or_replace_actual_state() {
        let state = crate::state::tests::test_app_state().await;
        context(&state, 9, None);
        deliver(&state, "AB", &frame(&state, 9, 0, 0));
        {
            let mut inner = state.plumbing_drive.inner.lock().unwrap();
            let observed = inner.observed.as_mut().unwrap();
            observed.after.wall -= std::time::Duration::from_millis(4_000);
            if let Some(model) = observed.after.model.as_mut() {
                *model = model.checked_sub(4_000).unwrap_or(u64::MAX);
            }
        }
        deliver(&state, "AB", &frame(&state, 9, 2, 0));
        let value = view(&state, true);
        assert_eq!(value["board"]["fresh"], false);
        assert_eq!(value["board"]["mode"], "stepper");
        assert!(motion_ready(&state).is_err());
    }
    #[tokio::test]
    async fn pending_or_superseded_setting_cannot_enable_motion_and_fresh_query_can_reconcile() {
        let state = crate::state::tests::test_app_state().await;
        {
            let mut inner = state.plumbing_drive.inner.lock().unwrap();
            inner.desired.mode = 2;
            inner.pending_setting = Some(9);
        }
        context(&state, 9, Some(Mode::Auxiliary));
        deliver(&state, "AB", &frame(&state, 9, 2, 1));
        assert_eq!(view(&state, true)["pending"], true);
        assert!(motion_ready(&state).is_err());
        let mut wrong_epoch = frame(&state, 9, 2, 0);
        wrong_epoch[20..24].copy_from_slice(&4u32.to_le_bytes());
        deliver(&state, "AB", &wrong_epoch);
        assert!(motion_ready(&state).is_err());
        context(&state, 10, None);
        deliver(&state, "AB", &frame(&state, 10, 2, 0));
        assert_eq!(view(&state, true)["pending"], false);
        assert!(motion_ready(&state).is_ok());
        assert_eq!(completion_timeout_ms(&state), 50_000);
    }
    #[tokio::test]
    async fn io_metadata_allows_explicit_retry_context_but_never_motion_or_automatic_repair() {
        let state = crate::state::tests::test_app_state().await;
        context(&state, 9, None);
        deliver(&state, "AB", &frame(&state, 9, 255, 6));
        let value = view(&state, true);
        assert_eq!(value["board"]["fresh"], true);
        assert_eq!(value["board"]["ready"], false);
        assert!(motion_ready(&state).is_err());
        assert!(!state.plumbing_drive.path.exists());
    }
    #[tokio::test]
    async fn stale_recovery_generation_refuses_before_any_settings_file_mutation() {
        let state = crate::state::tests::test_app_state().await;
        let generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
        let body = SettingsRequest {
            mode: "auxiliary".into(),
            expected_generation: "0000000000000007".into(),
            expected_epoch: 3,
            backend_instance: format!("{:016x}", state.plumbing_drive.issuer),
            expected_revision: 0,
            expected_reset_epoch: format!("{:016x}", generation + 1),
            release_stepper: true,
        };
        let error = apply_settings(
            &state,
            body,
            generation + 1,
            1000,
            crate::gse::plumbing_admission_ticket(&state),
        )
        .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert!(!state.plumbing_drive.path.exists());
        assert_eq!(
            state
                .operator_dispatch_inflight
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
    }
    #[test]
    fn desired_selection_round_trips_and_invalid_storage_never_becomes_a_default_write() {
        let path = std::env::temp_dir().join(format!(
            "gs-plumbing-file-test-{:016x}/settings.json",
            rand::random::<u64>()
        ));
        assert!(Desired::load(&path).unwrap().is_none());
        let value = Desired {
            version: 1,
            mode: 2,
            revision: 8,
            source_ms: 123456,
        };
        value.save(&path, 3).unwrap();
        let loaded = Desired::load(&path).unwrap().unwrap();
        assert_eq!(
            (loaded.mode, loaded.revision, loaded.source_ms),
            (2, 8, 123456)
        );
        fs::write(&path, b"not a config").unwrap();
        assert!(Desired::load(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"not a config");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}

async fn inspect_history_fault(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
            Ok(p) => p,
            Err(r) => return r,
        };
    if !primary(&state) {
        return error(
            StatusCode::CONFLICT,
            "Inspect retraction history at the primary GS.",
        );
    }
    let worker = state.clone();
    match tokio::task::spawn_blocking(move || worker.plumbing_drive.motion_journal.inspect_fault())
        .await
    {
        Ok(Ok(())) => Json(view(
            &state,
            principal.permissions.send_commands && principal.allows_command_name("RetractPlumbing"),
        ))
        .into_response(),
        Ok(Err(message)) => error(StatusCode::CONFLICT, message),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Retraction history inspection failed; original evidence remains unchanged.",
        ),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryReset {
    backend_instance: String,
    reset_epoch: String,
    fault_fingerprint: String,
    fault_revision: u64,
    confirmation: String,
}
async fn reset_history_fault(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<HistoryReset>,
) -> Response {
    let generation = crate::gse::capture_operator_origin(&state);
    let ticket = crate::gse::plumbing_admission_ticket(&state);
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(r) => return r,
        };
    if !principal.permissions.send_commands || !principal.allows_command_name("RetractPlumbing") {
        return error(
            StatusCode::FORBIDDEN,
            "Fill-line command permission is required to reset retraction history.",
        );
    }
    if !primary(&state) {
        return error(
            StatusCode::CONFLICT,
            "Retraction history reset requires the primary GS.",
        );
    }
    if generation.is_none()
        || body.reset_epoch != format!("{:016x}", generation.unwrap())
        || body.backend_instance != format!("{:016x}", state.plumbing_drive.issuer)
        || body.confirmation != "RESET RETRACTION JOURNAL"
        || body.fault_fingerprint.len() != 64
        || !body
            .fault_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return error(
            StatusCode::CONFLICT,
            "Retraction history confirmation changed; inspect and confirm again.",
        );
    }
    let fault = state.plumbing_drive.motion_journal.fault_context();
    if !fault.resettable
        || fault.revision != body.fault_revision
        || fault.fingerprint.as_deref() != Some(&body.fault_fingerprint)
    {
        return error(
            StatusCode::CONFLICT,
            "Retraction fault context changed or original evidence is unreadable; inspect and confirm again.",
        );
    }
    let generation = generation.unwrap();
    if !crate::gse::reserve_plumbing_worker(&state, ticket, || {
        state
            .plumbing_drive
            .motion_journal
            .reserve_repair(body.fault_revision, &body.fault_fingerprint)
    }) {
        return error(
            StatusCode::CONFLICT,
            "Original admission changed or retraction history has an owner. No reset was admitted.",
        );
    }
    let worker = state.clone();
    let reservation = MotionWorkerReservation(worker.clone());
    match tokio::task::spawn_blocking(move || -> Result<(), (StatusCode, String)> {
        let _reservation = reservation;
        let _claim = crate::gse::claim_operator_dispatch(&worker, generation).ok_or((
            StatusCode::CONFLICT,
            "Recovery closed admission before history reset".into(),
        ))?;
        crate::gse::with_plumbing_admission(&worker, ticket, || ())
            .map_err(|e| (StatusCode::CONFLICT, e.into()))?;
        worker
            .plumbing_drive
            .motion_journal
            .repair_closed(body.fault_revision, &body.fault_fingerprint)
            .map_err(|e| (StatusCode::INSUFFICIENT_STORAGE, e))?;
        crate::gse::with_plumbing_admission(&worker, ticket, || {
            worker.plumbing_drive.motion_journal.publish_closed()
        })
        .map_err(|e| {
            (
                StatusCode::CONFLICT,
                format!("{e}; durable history remains CLOSED, no motion was sent"),
            )
        })?;
        Ok(())
    })
    .await
    {
        Ok(Ok(())) => (StatusCode::OK, Json(view(&state, true))).into_response(),
        Ok(Err((status, message))) => error(status, message),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Retraction history reset worker failed; Launch remains inhibited.",
        ),
    }
}
