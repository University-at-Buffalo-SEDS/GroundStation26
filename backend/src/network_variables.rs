use crate::telemetry_task::get_current_timestamp_ms;
use crate::types::{Board, FlightState, NetworkFlightState};
use anyhow::{Context, Result};
use sedsnet::packet::Packet;
use sedsnet::router::{NetworkVariablePermissions, Router};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

// One durable desired revision, not a periodic route/clock refresh. An older
// in-flight submission must never clear or replace a newer T0/reset request.
#[derive(Default)]
struct PendingDaqClock {
    desired: Option<([u64; 2], u64)>,
    highest_revision: u64,
}
impl PendingDaqClock {
    fn record(&mut self, pair: [u64; 2], revision: u64) {
        if revision >= self.highest_revision {
            self.highest_revision = revision;
            self.desired = Some((pair, revision));
        }
    }
    fn accepted(&mut self, snapshot: ([u64; 2], u64)) {
        if self.desired == Some(snapshot) {
            self.desired = None;
        }
    }
}
static PENDING_DAQ_CLOCK: Mutex<PendingDaqClock> = Mutex::new(PendingDaqClock {
    desired: None,
    highest_revision: 0,
});
static DAQ_CLOCK_RETRY_FAILURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static DAQ_CLOCK_RETRY_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

const DEFAULT_CACHE_PATH: &str = "backend/data/network_variables.json";
const UNDERGLOW_TYPE: &str = "AV_BAY_UNDERGLOW";
const FLIGHT_BUZZER_TYPE: &str = "FLIGHT_BUZZER";
const FLIGHT_STATE_TYPE: &str = "FLIGHT_STATE";
const DAQ_CALIBRATION_TYPE: &str = "DAQ_LOADCELL_CALIBRATION";
const DAQ_KG50_CALIBRATION_TYPE: &str = "DAQ_KG50_CALIBRATION";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct PersistentVariables {
    #[serde(default)]
    av_bay_underglow: bool,
    #[serde(default)]
    flight_buzzer: bool,
    #[serde(default)]
    flight_state: u8,
    /// Original network timestamp; zero denotes an unknown legacy origin.
    #[serde(default)]
    flight_state_stamp: u64,
    #[serde(default)]
    daq_log_clock: [u64; 2],
    #[serde(default)]
    daq_log_clock_stamp: u64,
    #[serde(default)]
    daq_log_last_session: u64,
}

struct VariableStore {
    path: PathBuf,
    values: PersistentVariables,
    durable: bool,
    revision: u64,
    automatic_idle: Option<AutomaticLiveIdentity>,
}

impl VariableStore {
    fn new(path: PathBuf) -> Self {
        let persisted = read_persisted(&path);
        Self {
            path,
            values: persisted.unwrap_or_else(default_values),
            durable: persisted.is_some(),
            revision: 1,
            automatic_idle: None,
        }
    }

    fn commit(&mut self, values: PersistentVariables) -> Result<()> {
        // Mutation identity is cancellation only. Exhaustion retires ordinary
        // automatic publication, never existing critical durable writers.
        self.revision = self.revision.saturating_add(1);
        if self.durable && self.values == values {
            return Ok(());
        }
        persist(&self.path, values)?;
        self.values = values;
        self.durable = true;
        Ok(())
    }
    fn commit_flight_state(&mut self, values: PersistentVariables) -> Result<()> {
        self.commit(values)?;
        self.automatic_idle = None;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AutomaticIdleSnapshot {
    revision: u64,
    state: u8,
    stamp: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AutomaticIdleIdentity {
    pub(crate) packet_id: u64,
    pub(crate) timestamp: u64,
    pub(crate) generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutomaticLiveIdentity {
    Idle(AutomaticIdleIdentity),
    Phase(AutomaticPhaseIdentity),
}
impl AutomaticLiveIdentity {
    fn original(self) -> AutomaticIdleIdentity {
        match self { Self::Idle(identity) => identity, Self::Phase(identity) => identity.original }
    }
    fn phase(self) -> u8 { match self { Self::Idle(_) => 1, Self::Phase(identity) => identity.phase } }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AutomaticPhaseIdentity {
    pub(crate) original: AutomaticIdleIdentity,
    pub(crate) phase: u8,
}
#[derive(Clone, Copy)]
pub(crate) struct AutomaticPhaseSnapshot {
    revision: u64,
    state: u8,
    stamp: u64,
    live: Option<AutomaticLiveIdentity>,
}
pub(crate) fn capture_automatic_phase_store(expected: u8, generation: u64) -> Option<AutomaticPhaseSnapshot> {
    let guard = STORE.get()?.try_lock().ok()?;
    if guard.revision >= u64::MAX - 1 || expected > 6 || guard.values.flight_state > 6
        || guard.automatic_idle.is_some_and(|live| live.phase() != expected || live.original().generation != generation) {
        return None;
    }
    Some(AutomaticPhaseSnapshot { revision: guard.revision, state: guard.values.flight_state,
        stamp: guard.values.flight_state_stamp, live: guard.automatic_idle })
}
pub(crate) fn automatic_phase_packet(router: &Router, phase: u8) -> Result<Packet> {
    anyhow::ensure!((1..=6).contains(&phase), "invalid automatic prelaunch phase");
    let timestamp = crate::system_clock::required_control_network_utc_ms(router)?;
    flight_state_wire_packet(network_phase(phase)?, timestamp)
}
pub(crate) struct AutomaticIdleStoreGuard<'a>(std::sync::MutexGuard<'a, VariableStore>);
impl AutomaticIdleStoreGuard<'_> {
    pub(crate) fn matches_phase(&self, expected: AutomaticPhaseSnapshot) -> bool {
        expected.revision < u64::MAX - 1 && self.0.revision == expected.revision
            && (self.0.values.flight_state, self.0.values.flight_state_stamp) == (expected.state, expected.stamp)
            && self.0.automatic_idle == expected.live
    }
    pub(crate) fn admitted_phase(&mut self, identity: AutomaticPhaseIdentity) {
        self.0.revision += 1; // prevalidated by matches_phase; never wrap/allocate
        self.0.automatic_idle = Some(AutomaticLiveIdentity::Phase(identity));
    }

    pub(crate) fn matches(&self, expected: AutomaticIdleSnapshot) -> bool {
        expected.revision != u64::MAX && self.0.revision == expected.revision
            && (self.0.values.flight_state, self.0.values.flight_state_stamp) == (expected.state, expected.stamp)
            && expected.state == 0 && self.0.automatic_idle.is_none()
    }
    pub(crate) fn admitted(&mut self, identity: AutomaticIdleIdentity) {
        self.0.revision = self.0.revision.saturating_add(1);
        self.0.automatic_idle = Some(AutomaticLiveIdentity::Idle(identity));
    }
}
pub(crate) fn capture_automatic_idle_store() -> Option<AutomaticIdleSnapshot> {
    // Main initializes STORE before constructing AppState/workers. Missing
    // initialization is not permission to perform file IO in event capture.
    let guard = STORE.get()?.lock().ok()?;
    (guard.revision != u64::MAX && guard.values.flight_state == 0 && guard.automatic_idle.is_none()).then_some(AutomaticIdleSnapshot {
        revision: guard.revision, state: guard.values.flight_state, stamp: guard.values.flight_state_stamp,
    })
}
pub(crate) fn try_automatic_idle_store() -> Option<AutomaticIdleStoreGuard<'static>> {
    STORE.get()?.try_lock().ok().map(AutomaticIdleStoreGuard)
}
pub(crate) fn automatic_idle_packet(router: &Router) -> Result<Packet> {
    let timestamp = crate::system_clock::required_control_network_utc_ms(router)?;
    flight_state_wire_packet(NetworkFlightState::Idle, timestamp)
}
pub(crate) fn automatic_idle_batch_limits() -> sedsnet::router::ManagedBatchPreparationLimits {
    // Nine actual registered variables, one original, and a complete registry
    // headroom. No runtime growth/reservation or fallback admission.
    sedsnet::router::ManagedBatchPreparationLimits {
        max_batch_packets: 1, max_snapshot_types: 19, max_snapshot_entries: 19,
        max_original_payload_bytes: 1, max_snapshot_logical_bytes: 1024 * 1024,
    }
}

static STORE: OnceLock<Mutex<VariableStore>> = OnceLock::new();
static NETWORK_VARIABLE_NONCE: AtomicU16 = AtomicU16::new(1);

fn cache_path() -> PathBuf {
    std::env::var_os("GS_NETWORK_VARIABLE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(default_cache_path)
}

fn default_cache_path() -> PathBuf {
    #[cfg(not(test))]
    {
        PathBuf::from(DEFAULT_CACHE_PATH)
    }
    #[cfg(test)]
    {
        // A test binary may re-exec itself while the parent suite is still
        // writing. Its process-local STORE mutex cannot protect the parent's
        // JSON or rename temporary file, nor should tests touch operator data.
        static PATH: OnceLock<PathBuf> = OnceLock::new();
        PATH.get_or_init(|| {
            std::env::temp_dir()
                .join(format!(
                    "groundstation-network-variables-test-{}-{}",
                    std::process::id(),
                    rand::random::<u128>()
                ))
                .join("network_variables.json")
        })
        .clone()
    }
}

fn read_persisted(path: &Path) -> Option<PersistentVariables> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .filter(|values: &PersistentVariables| crate::types::u8_to_flight_state(values.flight_state).is_some())
}

#[cfg(test)]
fn load(path: &Path) -> PersistentVariables {
    read_persisted(path).unwrap_or_else(default_values)
}

fn default_values() -> PersistentVariables {
    PersistentVariables {
        av_bay_underglow: std::env::var("GS_AV_BAY_UNDERGLOW_DEFAULT")
            .ok()
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "on")),
        flight_buzzer: std::env::var("GS_FLIGHT_BUZZER_DEFAULT")
            .ok()
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "on")),
        flight_state: std::env::var("GS_FLIGHT_STATE_DEFAULT")
            .ok()
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|value| *value <= 15)
            .unwrap_or(0),
        daq_log_clock: [0, 0],
        flight_state_stamp: 0,
        daq_log_clock_stamp: 0,
        daq_log_last_session: 0,
    }
}

fn store() -> &'static Mutex<VariableStore> {
    STORE.get_or_init(|| Mutex::new(VariableStore::new(cache_path())))
}

fn persist(path: &Path, values: PersistentVariables) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "create network-variable cache directory {}",
                parent.display()
            )
        })?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&values)?)
        .with_context(|| format!("write network-variable cache {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("replace network-variable cache {}", path.display()))?;
    Ok(())
}

fn packet(data_type: &str, endpoint: &str, value: u8) -> Result<Packet> {
    packet_bytes(data_type, endpoint, Arc::from([value]))
}

fn packet_bytes(data_type: &str, endpoint: &str, payload: Arc<[u8]>) -> Result<Packet> {
    packet_bytes_at(data_type, endpoint, payload, get_current_timestamp_ms())
}

/// The single FLIGHT_STATE wire funnel. Only set (a) is ever built here; the
/// store keeps the GS-internal phase, and fill sub-phases leave as Fill.
fn flight_state_wire_packet(state: NetworkFlightState, timestamp: u64) -> Result<Packet> {
    // The simulator's set-(a) wire assertion reads this line.
    log::info!("FLIGHT_STATE wire value={} ({})", state as u8, state.label());
    packet_bytes_at(FLIGHT_STATE_TYPE, "FLIGHT_STATE", Arc::from([state as u8]), timestamp)
}
/// A stored (GS-internal) phase as it goes on the wire.
fn network_phase(phase: u8) -> Result<NetworkFlightState> {
    crate::types::u8_to_flight_state(phase)
        .map(FlightState::network)
        .ok_or_else(|| anyhow::anyhow!("invalid flight-state value {phase}"))
}

fn packet_bytes_at(
    data_type: &str,
    endpoint: &str,
    payload: Arc<[u8]>,
    timestamp: u64,
) -> Result<Packet> {
    Ok(Packet::new(
        crate::telemetry_schema::data_type(data_type),
        &[crate::telemetry_schema::endpoint(endpoint)],
        Board::GroundStation.sender_id(),
        timestamp,
        payload,
    )?
    .with_nonce(NETWORK_VARIABLE_NONCE.fetch_add(1, Ordering::Relaxed)))
}

pub fn initialize(router: &Router) -> Result<()> {
    initialize_for_role(router, crate::rrls::is_rrls())
}

/// Production bootstrap respects restored durable recovery closure. Register
/// services without synthesizing any cache/default write while CLOSED. An
/// ordinary open bootstrap owns the same actual producer lease through IO/SDK.
pub fn initialize_for_state(state: &crate::state::AppState, router: &Router) -> Result<()> {
    let claim = crate::gse::capture_operator_origin(state)
        .and_then(|generation| crate::gse::claim_operator_dispatch(state, generation));
    if claim.is_some() {
        initialize(router)
    } else {
        initialize_for_role_seed(router, crate::rrls::is_rrls(), false)
    }
}
fn initialize_for_role(router: &Router, observer: bool) -> Result<()> {
    initialize_for_role_seed(router, observer, true)
}
/// The managed variables a primary station owns (READ_WRITE) and answers
/// requests for itself. Every other node holds read-only replicas.
const STATION_VARIABLES: [&str; 9] = [
    UNDERGLOW_TYPE,
    FLIGHT_BUZZER_TYPE,
    FLIGHT_STATE_TYPE,
    DAQ_CALIBRATION_TYPE,
    DAQ_KG50_CALIBRATION_TYPE,
    "DAQ_LOG_CLOCK",
    "DAQ_THERMAL_CALIBRATION",
    "DAQ_FILTER_CALIBRATION",
    "DAQ_KG50_INPUT",
];
/// Whether this station owns and serves `ty`, so a request for it needs no
/// other node (an RRLS is a read-only observer and serves nothing).
pub(crate) fn station_serves(ty: sedsnet::config::DataType) -> bool {
    !crate::rrls::is_rrls() && STATION_VARIABLES.iter().any(|name| crate::telemetry_schema::data_type(name) == ty)
}
/// RRLS takeover: the cosmetic outputs (underglow, buzzer) become writable
/// while it owns the AV bay and read-only again when it hands back.
pub(crate) fn set_takeover_outputs_writable(router: &Router, writable: bool) -> Result<()> {
    for output in NetworkOutput::ALL {
        router.enable_network_variable(
            crate::telemetry_schema::data_type(output.data_type()),
            if writable {
                NetworkVariablePermissions::READ_WRITE
            } else {
                NetworkVariablePermissions::READ_ONLY
            },
        )?;
    }
    Ok(())
}
fn initialize_for_role_seed(router: &Router, observer: bool, seed: bool) -> Result<()> {
    for data_type in STATION_VARIABLES {
        router.enable_network_variable(
            crate::telemetry_schema::data_type(data_type),
            if observer {
                NetworkVariablePermissions::READ_ONLY
            } else {
                NetworkVariablePermissions::READ_WRITE
            },
        )?;
    }
    // Inbound values of the cosmetic variables, from any sender, are their
    // network readback. The router only calls this for inbound updates.
    for output in NetworkOutput::ALL {
        router.on_network_variable_update(
            crate::telemetry_schema::data_type(output.data_type()),
            move |packet| {
                record_network_value(output, packet);
                Ok(())
            },
        )?;
    }
    router.on_network_variable_update(
        crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE),
        move |packet| {
            // A per-board state report is not a write of the host's aggregate
            // desired state. SDK cache refreshes retain its original provenance;
            // only the authority's source can reach our durable GS seed.
            if !matches!(
                crate::types::canonical_sender_id(packet.sender()),
                "GS" | "FC"
            ) {
                return Ok(());
            }
            let wire =
                packet
                    .payload()
                    .first()
                    .copied()
                    .ok_or(sedsnet::TelemetryError::HandlerError(
                        "empty flight-state network variable",
                    ))?;
            // Set (a) only; a retired value from old firmware maps to its
            // nearest network value and is logged once.
            let Some(received) = crate::types::network_flight_state_from_wire(wire, packet.sender()) else {
                return Err(sedsnet::TelemetryError::HandlerError(
                    "out-of-range flight-state network variable",
                ));
            };
            let state = received as u8;
            if !observer && crate::types::canonical_sender_id(packet.sender()) == "GS" {
                // Explicit local publishers own their local/durable transition.
                // A reflected original packet is telemetry, never fresh intent.
                return Ok(());
            }
            if crate::types::canonical_sender_id(packet.sender()) == "FC" && state == 15 {
                return Ok(());
            }
            let mut guard = store()
                .lock()
                .expect("network-variable store lock poisoned");
            if crate::types::canonical_sender_id(packet.sender()) == "FC"
                && (state == 15
                    || guard.values.flight_state == 15
                    || (matches!(guard.values.flight_state, 7..=14 | 16) && state <= 6))
            {
                return Ok(());
            }
            // The store keeps the GS-internal phase: Fill received while the
            // store already holds a fill sub-phase is the same network value.
            let state = if network_phase(guard.values.flight_state).ok() == Some(received) {
                guard.values.flight_state
            } else {
                received.local() as u8
            };
            let mut next = guard.values;
            if next.flight_state != state || next.flight_state_stamp == 0 {
                next.flight_state_stamp = packet.timestamp();
            }
            next.flight_state = state;
            guard.commit_flight_state(next).map_err(|err| {
                log::warn!("failed to persist inbound flight-state network variable: {err}");
                sedsnet::TelemetryError::HandlerError(
                    "persist inbound flight-state network variable",
                )
            })
        },
    )?;
    // RRLS receives the primary's values. Seeding its local disk defaults as
    // GS-owned packets creates competing managed-variable refresh traffic,
    // even though the recovery transport ultimately discards those packets.
    if observer || !seed {
        return Ok(());
    }
    {
        let mut guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        if guard.values.daq_log_clock_stamp == 0 {
            let pair = guard.values.daq_log_clock;
            commit_daq_clock_pair(&mut guard, pair, get_current_timestamp_ms())?;
        } else if !guard.path.exists() {
            persist(&guard.path, guard.values)?;
            guard.durable = true;
        }
    }
    router.seed_managed_variable(packet(
        UNDERGLOW_TYPE,
        "AV_BAY_UNDERGLOW_OWNER",
        u8::from(underglow_enabled()),
    )?)?;
    router.seed_managed_variable(packet(
        FLIGHT_BUZZER_TYPE,
        "FLIGHT_CONTROLLER",
        u8::from(flight_buzzer_enabled()),
    )?)?;
    // Retained Launch is history, never a fresh launch intent on restart.
    // Unknown board proofs and partial admission must not execute it again.
    if let Some(packet) = retained_non_launch_packet()? {
        router.seed_managed_variable(packet)?;
    }
    let selected = crate::loadcell::load_or_default();
    router.seed_managed_variable(kg50_input_packet(&selected)?)?;
    let calibration = crate::loadcell::amp2_calibration(&selected);
    router.seed_managed_variable(calibration_packet(&calibration)?)?;
    router.seed_managed_variable(kg50_calibration_packet(&calibration)?)?;
    router.seed_managed_variable(cached_daq_log_clock_packet()?)?;
    router.seed_managed_variable(thermal_packet(&calibration)?)?;
    router.seed_managed_variable(filter_packet(&calibration)?)?;
    Ok(())
}

pub fn publish_bootstrap_for_state(
    state: &crate::state::AppState,
    router: &Router,
) -> Result<bool> {
    let Some(generation) = crate::gse::capture_operator_origin(state) else {
        return Ok(false);
    };
    let Some(_claim) = crate::gse::claim_operator_dispatch(state, generation) else {
        return Ok(false);
    };
    publish_current(router)?;
    Ok(true)
}

pub fn publish_current(router: &Router) -> Result<()> {
    let request = crate::qualification_requests::Request::begin("publish_current");
    publish_network_output(&request, router, NetworkOutput::Underglow, true, 0)?;
    publish_network_output(&request, router, NetworkOutput::FlightBuzzer, true, 0)?;
    if let Some(packet) = retained_non_launch_packet()? {
        request.submit(router, packet)?;
    }
    set_daq_calibration(router, &crate::loadcell::load_or_default())?;
    request.submit(router, cached_daq_log_clock_packet()?)?;
    Ok(())
}

/// The two cosmetic SEDSnet network variables. Neither has an owner board:
/// the GS owns writing them and confirmation comes from the network value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NetworkOutput {
    Underglow,
    FlightBuzzer,
}
impl NetworkOutput {
    const ALL: [Self; 2] = [Self::Underglow, Self::FlightBuzzer];
    fn index(self) -> usize {
        self as usize
    }
    fn data_type(self) -> &'static str {
        match self {
            Self::Underglow => UNDERGLOW_TYPE,
            Self::FlightBuzzer => FLIGHT_BUZZER_TYPE,
        }
    }
    fn endpoint(self) -> &'static str {
        match self {
            Self::Underglow => "AV_BAY_UNDERGLOW_OWNER",
            Self::FlightBuzzer => "FLIGHT_CONTROLLER",
        }
    }
    fn requested(self) -> bool {
        match self {
            Self::Underglow => underglow_enabled(),
            Self::FlightBuzzer => flight_buzzer_enabled(),
        }
    }
    fn commit_requested(self, enabled: bool) -> Result<()> {
        let mut guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        let mut next = guard.values;
        match self {
            Self::Underglow => next.av_bay_underglow = enabled,
            Self::FlightBuzzer => next.flight_buzzer = enabled,
        }
        guard.commit(next)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NetworkOutputView {
    /// Stored GS request; republished until the network carries it.
    pub(crate) requested: bool,
    /// The latest network value, only when it matches a fresh request.
    pub(crate) confirmed: Option<bool>,
    /// True once any network value for the variable has been seen.
    pub(crate) readback_seen: bool,
}

static NETWORK_CONFIRMATION: Mutex<[network_confirmation::Confirmation; 2]> =
    Mutex::new([network_confirmation::Confirmation::DEFAULT; 2]);
static NETWORK_RECONCILE_ARMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn network_confirmation() -> std::sync::MutexGuard<'static, [network_confirmation::Confirmation; 2]> {
    NETWORK_CONFIRMATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
static NETWORK_TEST_CLOCK: Mutex<Option<fn() -> std::time::Instant>> = Mutex::new(None);
/// Model-time tests drive retry horizons faster than wall time.
#[cfg(test)]
pub(crate) fn set_network_test_clock(clock: fn() -> std::time::Instant) {
    *NETWORK_TEST_CLOCK.lock().unwrap() = Some(clock);
}
fn network_now() -> std::time::Instant {
    #[cfg(test)]
    if let Some(clock) = *NETWORK_TEST_CLOCK.lock().unwrap() {
        return clock();
    }
    std::time::Instant::now()
}

pub(crate) fn network_output_view(output: NetworkOutput) -> NetworkOutputView {
    let requested = output.requested();
    let confirmation = network_confirmation();
    let entry = &confirmation[output.index()];
    NetworkOutputView {
        requested,
        confirmed: entry.shown(requested),
        readback_seen: entry.readback_seen(),
    }
}

/// Record an inbound network update of a cosmetic variable, from any sender
/// (a board, a relay or another station). This is the variable's readback.
fn record_network_value(output: NetworkOutput, packet: &Packet) {
    let Some(&byte) = packet.payload().first() else {
        return;
    };
    record_readback(output, byte != 0);
}

fn record_readback(output: NetworkOutput, value: bool) {
    let requested = output.requested();
    network_confirmation()[output.index()].network_value(value, requested, network_now());
    NETWORK_RECONCILE_ARMED.store(true, Ordering::Release);
    NETWORK_VALUE_CHANGED.store(true, Ordering::Release);
}

/// The FC's own report of the cosmetic outputs it applied, in BOARD_HEALTH
/// word 14 (sent every 5 s, one-way, so a lost end-to-end ACK no longer hides
/// a delivered write). Word 14 is the UART-drop counter on other boards
/// (word 1 flag 8), which the FC never sets; the FC marks its own use with
/// the tag in bits 16..31. Bits: 0 underglow on, 1 buzzer on, 2 the underglow
/// value came from a network write this boot, 3 likewise for the buzzer.
/// FC 13de2ca does not send it yet (proposed FC change); until it does,
/// nothing here fires.
pub(crate) const FC_OUTPUTS_TAG: u32 = 0x4F55;

/// [underglow, buzzer]: the value the FC reports it applied from a network
/// write, if it reports one.
pub(crate) fn fc_outputs_from_health(words: &[u32]) -> [Option<bool>; 2] {
    let tagged = words.len() == 24 && words[1] & 8 == 0 && words[14] >> 16 == FC_OUTPUTS_TAG;
    let word = if tagged { words[14] } else { 0 };
    let read = |value: u32, applied: u32| (tagged && word & applied != 0).then_some(word & value != 0);
    [read(1, 4), read(2, 8)]
}

/// An FC BOARD_HEALTH report: its applied output values are the variables'
/// readback, like an inbound network value.
pub(crate) fn observe_fc_health_outputs(words: &[u32]) {
    for (output, value) in NetworkOutput::ALL.into_iter().zip(fc_outputs_from_health(words)) {
        if let Some(value) = value {
            record_readback(output, value);
        }
    }
}

/// Set when an inbound value changed what the controls show, so the next
/// reconcile pass refreshes the action policy even with nothing to publish.
static NETWORK_VALUE_CHANGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Publish the stored request and start tracking its network delivery.
/// `fresh` marks a new request, which starts a new freshness epoch and earns
/// one immediate republish on loss.
fn publish_network_output(
    request: &crate::qualification_requests::Request,
    router: &Router,
    output: NetworkOutput,
    fresh: bool,
    reports: u64,
) -> Result<()> {
    if fresh {
        REPUBLISH_BACKOFF.lock().unwrap()[output.index()].reset();
    }
    let value = output.requested();
    let packet = packet(output.data_type(), output.endpoint(), u8::from(value))?;
    let id = packet.packet_id();
    let result = request.submit(router, packet);
    let now = network_now();
    let mut confirmation = network_confirmation();
    match &result {
        Ok(()) => confirmation[output.index()].published(id, value, now, fresh),
        Err(_) => confirmation[output.index()].publish_failed(now, reports, fresh),
    }
    NETWORK_RECONCILE_ARMED.store(true, Ordering::Release);
    result
}

/// Automatic republishes of one unconfirmed variable: 10 s, then 20 s, then
/// at most 30 s apart while the owner stays unreachable (bench S5=10: each
/// republish exhausted its ordered retries, 90 frames per variable in 75 s).
/// An operator request publishes at once and starts over, as does a
/// confirmation.
#[derive(Clone, Copy, Default)]
pub(crate) struct RepublishBackoff {
    republishes: u32,
    last: Option<std::time::Instant>,
}
impl RepublishBackoff {
    fn wait(&self) -> std::time::Duration {
        let step = network_confirmation::REPUBLISH_GAP;
        match self.republishes {
            0 => std::time::Duration::ZERO,
            n => step.saturating_mul(n.min(3)),
        }
    }
    pub(crate) fn due(&self, now: std::time::Instant) -> bool {
        self.last.is_none_or(|last| now.saturating_duration_since(last) >= self.wait())
    }
    pub(crate) fn republished(&mut self, now: std::time::Instant) {
        self.republishes = self.republishes.saturating_add(1);
        self.last = Some(now);
    }
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}
static REPUBLISH_BACKOFF: Mutex<[RepublishBackoff; 2]> = Mutex::new([RepublishBackoff { republishes: 0, last: None }; 2]);

/// Poll the SDK reliable rows for in-flight cosmetic variables and republish
/// the stored request when the network has not confirmed it. `network_reports`
/// counts packets heard from the network for each variable (any board; it
/// only paces retries and never gates confirmation). `claim` must return a
/// dispatch claim (held for the publish) or None when publication is closed.
pub(crate) fn reconcile_network_outputs<C>(
    router: &Router,
    network_reports: [u64; 2],
    claim: impl Fn() -> Option<C>,
) -> bool {
    let mut changed = NETWORK_VALUE_CHANGED.swap(false, Ordering::AcqRel);
    if !NETWORK_RECONCILE_ARMED.load(Ordering::Acquire) {
        return changed;
    }
    let tuning = sedsnet::config::runtime_tuning_config();
    let retransmit = std::time::Duration::from_millis(u64::from(tuning.reliable_retransmit_ms));
    let max_retries = tuning.reliable_max_retries;
    let now = network_now();
    let rows = {
        let confirmation = network_confirmation();
        confirmation
            .iter()
            .any(|c| c.inflight_id().is_some())
            .then(|| router.export_reliable_pending_stats())
    };
    let mut republish = Vec::new();
    {
        let mut confirmation = network_confirmation();
        for output in NetworkOutput::ALL {
            let entry = &mut confirmation[output.index()];
            let reports = network_reports[output.index()];
            let step = match (entry.inflight_id(), rows.as_ref()) {
                (Some(id), Some(rows)) => {
                    let row = rows.iter().find(|row| row.packet_id == id).map(|row| row.retries);
                    let queued = row.is_none()
                        && router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(
                            output.data_type(),
                        ));
                    entry.observe(row, queued, now, reports, retransmit, max_retries)
                }
                _ => entry.network_report(now, reports),
            };
            match step {
                network_confirmation::Step::Idle => {}
                network_confirmation::Step::Changed => changed = true,
                network_confirmation::Step::Republish => {
                    changed = true;
                    republish.push(output);
                }
            }
        }
        let mut backoff = REPUBLISH_BACKOFF.lock().unwrap();
        for output in NetworkOutput::ALL {
            if !confirmation[output.index()].needs_attention() {
                backoff[output.index()].reset();
            }
        }
        if !confirmation.iter().any(|c| c.needs_attention()) {
            NETWORK_RECONCILE_ARMED.store(false, Ordering::Release);
        }
    }
    for output in republish {
        let reports = network_reports[output.index()];
        {
            let mut backoff = REPUBLISH_BACKOFF.lock().unwrap();
            if !backoff[output.index()].due(now) {
                // Not yet: wait for more network traffic and the gap again.
                network_confirmation()[output.index()].publish_failed(now, reports, false);
                continue;
            }
            backoff[output.index()].republished(now);
        }
        let Some(_claim) = claim() else {
            network_confirmation()[output.index()].publish_failed(now, reports, false);
            continue;
        };
        let request = crate::qualification_requests::Request::begin("republish_unconfirmed");
        if let Err(error) = publish_network_output(&request, router, output, false, reports) {
            log::warn!("republish of unconfirmed {output:?} failed: {error}");
        }
    }
    changed
}

/// Production cadence wrapper: primary role (or an RRLS holding a takeover of
/// the AV bay) only, polled at most every 10 ms.
pub(crate) fn reconcile_network_outputs_for_state(
    state: &crate::state::AppState,
    router: &Router,
) -> bool {
    static NEXT_POLL: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    if (crate::rrls::is_rrls() && !crate::rrls::takeover_active())
        || !(NETWORK_RECONCILE_ARMED.load(Ordering::Acquire)
            || NETWORK_VALUE_CHANGED.load(Ordering::Acquire))
    {
        return false;
    }
    let now = std::time::Instant::now();
    {
        let mut next = NEXT_POLL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if next.is_some_and(|next| now < next) {
            return false;
        }
        *next = Some(now + std::time::Duration::from_millis(10));
    }
    // Any board heard from counts as network traffic; no owner board.
    let heard: u64 = state
        .board_status
        .lock()
        .unwrap()
        .values()
        .map(|status| status.packet_count)
        .fold(0, u64::saturating_add);
    reconcile_network_outputs(router, [heard; 2], || {
        crate::gse::capture_operator_origin(state)
            .and_then(|generation| crate::gse::claim_operator_dispatch(state, generation))
    })
}

fn set_network_output(router: &Router, output: NetworkOutput, enabled: bool, name: &'static str) -> Result<()> {
    let request = crate::qualification_requests::Request::begin(name);
    output.commit_requested(enabled)?;
    publish_network_output(&request, router, output, true, 0)
}

/// Toggle acts on the latest network value. Pressing again while the same
/// value is still being delivered keeps that delivery instead of flipping.
fn toggle_network_output(router: &Router, output: NetworkOutput, name: &'static str) -> Result<bool> {
    let requested = output.requested();
    let (target, delivering) = {
        let confirmation = network_confirmation();
        let entry = &confirmation[output.index()];
        let target = entry.toggle_target(requested);
        (target, entry.delivering(target))
    };
    if !(delivering && target == requested) {
        set_network_output(router, output, target, name)?;
    }
    Ok(target)
}

pub fn toggle_underglow(router: &Router) -> Result<bool> {
    toggle_network_output(router, NetworkOutput::Underglow, "set_underglow")
}

pub fn set_underglow(router: &Router, enabled: bool) -> Result<()> {
    set_network_output(router, NetworkOutput::Underglow, enabled, "set_underglow")
}

#[allow(dead_code)] // Used by the optional direct-command compatibility path.
pub fn toggle_flight_buzzer(router: &Router) -> Result<bool> {
    toggle_network_output(router, NetworkOutput::FlightBuzzer, "set_flight_buzzer")
}

pub fn set_flight_buzzer(router: &Router, enabled: bool) -> Result<()> {
    set_network_output(router, NetworkOutput::FlightBuzzer, enabled, "set_flight_buzzer")
}

pub fn set_flight_state(router: &Router, state: u8) -> Result<()> {
    let request = crate::qualification_requests::Request::begin("set_flight_state");
    let timestamp = crate::system_clock::required_control_network_utc_ms(router)?;
    let packet = flight_state_wire_packet(network_phase(state)?, timestamp)?;
    {
        let mut guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        let mut next = guard.values;
        next.flight_state = state;
        next.flight_state_stamp = timestamp;
        #[cfg(test)]
        if crate::runtime_mode::is_test_fire() {
            crate::telemetry_task::flight_state_timeline::event(router, "durable_commit_start", state, Some(packet.packet_id()), Some(timestamp), None);
        }
        #[cfg(test)]
        if crate::runtime_mode::is_test_fire() {
            crate::recovery_pending_probe::before_state_commit(state);
        }
        let committed = guard.commit_flight_state(next);
        #[cfg(test)]
        if crate::runtime_mode::is_test_fire() {
            crate::telemetry_task::flight_state_timeline::event(router, "durable_commit_end", state, Some(packet.packet_id()), Some(timestamp), Some(committed.is_ok()));
        }
        committed?;
    }
    #[cfg(test)]
    if crate::runtime_mode::is_test_fire() {
        crate::telemetry_task::flight_state_timeline::event(router, "sdk_submit_start", state, Some(packet.packet_id()), Some(timestamp), None);
    }
    #[cfg(test)]
    let submitted_packet_id = packet.packet_id();
    let submitted = request.submit(router, packet);
    #[cfg(test)]
    if crate::runtime_mode::is_test_fire() {
        crate::telemetry_task::flight_state_timeline::detail(router, "sdk_submit_end", state, Some(submitted_packet_id), Some(timestamp), Some(submitted.is_ok()), Some(format!("{submitted:?}")));
    }
    submitted?;
    Ok(())
}

/// FLIGHT_STATE is an ordered end-to-end stream: a write waits until every
/// owner has acknowledged the one before it, and when that earlier write runs
/// out of retries on one owner the SDK retires the stream and drops every
/// newer write queued behind it, unsent. GB and DAQ take an Abort only from
/// FLIGHT_STATE, so an Abort queued behind an Idle that RF never acknowledged
/// never reached them. `exhausted_heads` are the FLIGHT_STATE writes that
/// were being sent when the stream gave up. If the latest value is not one of
/// them and is no longer pending, it was dropped: send it again, read now so
/// a later Reset or Idle still wins. The copy keeps the original stamp (and
/// the durable record is untouched) but needs a fresh identity, since the
/// SDK will not resend a retired packet ID. Launch is left to its own
/// admission, which owns its exact packet. Returns whether it was re-sent.
pub(crate) fn resend_dropped_flight_state(router: &Router, exhausted_heads: &[u64]) -> Result<bool> {
    if exhausted_heads.is_empty() {
        return Ok(false);
    }
    resend_latest_flight_state(router, exhausted_heads, |_| true)
}

/// An operator Abort held because the AV bay had no route: once it has one,
/// send the retained Aborted FLIGHT_STATE again. The one written with the
/// Abort reached only the boards the station knew then (PB, which takes an
/// Abort only from FLIGHT_STATE, was not among them after a long radio
/// outage). Same stamp, fresh identity; skipped while a FLIGHT_STATE write is
/// pending or the latest value is not Aborted. Returns whether it was sent.
pub(crate) fn resend_aborted_flight_state(router: &Router) -> Result<bool> {
    resend_latest_flight_state(router, &[], |value| value == NetworkFlightState::Aborted)
}

fn resend_latest_flight_state(
    router: &Router,
    exhausted_heads: &[u64],
    wanted: impl Fn(NetworkFlightState) -> bool,
) -> Result<bool> {
    let ty = crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE);
    let Some(latest) = router.get_cached_network_variable(ty)? else {
        return Ok(false);
    };
    let ours = crate::types::canonical_sender_id(latest.sender())
        == crate::types::canonical_sender_id(router.sender().as_ref());
    if !ours
        || latest.payload() == [7]
        || exhausted_heads.contains(&latest.packet_id())
        || router.export_reliable_pending_stats().iter().any(|row| row.data_type == ty)
    {
        return Ok(false);
    }
    let Some(value) = latest.payload().first().copied().and_then(NetworkFlightState::from_u8)
        .filter(|value| latest.payload().len() == 1 && wanted(*value)) else {
        return Ok(false);
    };
    let again = flight_state_wire_packet(value, latest.timestamp())?;
    router.set_network_variable(again)?;
    Ok(true)
}

/// Pure launch preparation. No desired state, file, managed cache or SDK owner
/// is changed until the caller wins its original Stop/board admission boundary.
#[cfg_attr(test, derive(Clone))]
pub(crate) struct PreparedLaunchState {
    pub(crate) packet: Packet,
    expected: (u8, u64),
}
pub(crate) fn prepare_launch_state(router: &Router) -> Result<PreparedLaunchState> {
    let timestamp = crate::system_clock::required_control_network_utc_ms(router)?;
    let packet = flight_state_wire_packet(
        NetworkFlightState::Launch,
        timestamp,
    )?;
    let guard = store()
        .lock()
        .expect("network-variable store lock poisoned");
    Ok(PreparedLaunchState {
        packet,
        expected: (guard.values.flight_state, guard.values.flight_state_stamp),
    })
}
/// Post-admission CAS. Never overwrite a later reset/Abort desired revision.
/// No packet is submitted here: its EXACT original packet already owns SDK work.
pub(crate) fn commit_admitted_launch(prepared: &PreparedLaunchState) -> Result<()> {
    let mut guard = store()
        .lock()
        .expect("network-variable store lock poisoned");
    if guard.durable
        && guard.values.flight_state == 7
        && guard.values.flight_state_stamp == prepared.packet.timestamp()
    {
        // The real SDK managed callback may have durably consumed this exact
        // admitted packet first. Do not rewrite, enqueue, or freshen its stamp.
        return Ok(());
    }
    anyhow::ensure!(
        (guard.values.flight_state, guard.values.flight_state_stamp) == prepared.expected,
        "Desired flight state changed after launch preparation; admitted launch is uncertain, explicit recovery required"
    );
    let mut next = guard.values;
    next.flight_state = 7;
    next.flight_state_stamp = prepared.packet.timestamp();
    guard.commit_flight_state(next)
}

/// Coordinator-only staging: disk IO is outside DISPATCH/Stop/SDK locks.
/// The owner must subsequently admit this EXACT packet under its original
/// Stop fence and require fresh actual board readbacks before reopening.
pub(crate) fn operator_phase_packet(router:&Router,phase:u8)->Result<Packet> {
    anyhow::ensure!(matches!(crate::types::u8_to_flight_state(phase),Some(crate::types::FlightState::Startup|crate::types::FlightState::Idle|crate::types::FlightState::PreFill|crate::types::FlightState::FillTest|crate::types::FlightState::NitrogenFill|crate::types::FlightState::NitrousFill|crate::types::FlightState::Armed)),"Operator phase cannot enter Fire or flight");
    flight_state_wire_packet(network_phase(phase)?,crate::system_clock::required_control_network_utc_ms(router)?)
}

pub(crate) fn prepare_verified_recovery_startup(router: &Router) -> Result<Packet> {
    let timestamp = crate::system_clock::required_control_network_utc_ms(router)?;
    let packet = flight_state_wire_packet(NetworkFlightState::Startup, timestamp)?;
    let mut guard = store()
        .lock()
        .expect("network-variable store lock poisoned");
    let mut next = guard.values;
    next.flight_state = 0;
    next.flight_state_stamp = timestamp;
    guard.commit_flight_state(next)?;
    Ok(packet)
}

fn retained_non_launch_packet() -> Result<Option<Packet>> {
    let guard = store()
        .lock()
        .expect("network-variable store lock poisoned");
    // Unknown-origin defaults and legacy records are history, not a newly
    // observed authority state. Keep them on disk without publishing UTC zero
    // or minting a fresh operator intent during bootstrap/route refresh.
    if let Some(identity) = guard.automatic_idle {
        // Only the SDK owns this live exact original. Never replace it by the
        // older durable Startup seed, or manufacture a retained auto revision.
        let identity = identity.original();
        let _ = (identity.packet_id, identity.timestamp, identity.generation);
        return Ok(None);
    }
    if guard.values.flight_state == 7 || guard.values.flight_state_stamp == 0 {
        return Ok(None);
    }
    Ok(Some(flight_state_packet(guard.values)?))
}

fn flight_state_packet(values: PersistentVariables) -> Result<Packet> {
    // Reconnect must not make a retained Abort newer than a recovery cutoff.
    flight_state_wire_packet(network_phase(values.flight_state)?, values.flight_state_stamp)
}

fn filter_packet(cfg: &crate::loadcell::LoadcellCalibrationFile) -> Result<Packet> {
    crate::loadcell::validate_thermal(cfg).map_err(anyhow::Error::msg)?;
    let payload: Vec<u8> = ["KG1000", "KG50"]
        .iter()
        .flat_map(|sensor| {
            cfg.noise
                .get(*sensor)
                .map(|n| n.tau_ms)
                .unwrap_or(0.)
                .to_le_bytes()
        })
        .collect();
    packet_bytes("DAQ_FILTER_CALIBRATION", "SD_CARD", Arc::from(payload))
}

fn thermal_packet(cfg: &crate::loadcell::LoadcellCalibrationFile) -> Result<Packet> {
    crate::loadcell::validate_thermal(cfg).map_err(anyhow::Error::msg)?;
    let mut payload = Vec::with_capacity(16);
    for sensor in ["KG1000", "KG50"] {
        let t = cfg.thermal.get(sensor).cloned().unwrap_or_default();
        payload.extend_from_slice(&t.reference_c.to_le_bytes());
        payload.extend_from_slice(&t.raw_per_c.to_le_bytes());
    }
    packet_bytes("DAQ_THERMAL_CALIBRATION", "SD_CARD", Arc::from(payload))
}

fn calibration_packet(cfg: &crate::loadcell::LoadcellCalibrationFile) -> Result<Packet> {
    let values = [
        cfg.ch1.m.unwrap_or(1.0),
        cfg.ch1.b.unwrap_or(0.0)
            - cfg
                .ch1_zero_raw
                .map(|z| cfg.ch1.m.unwrap_or(1.0) * z + cfg.ch1.b.unwrap_or(0.0))
                .unwrap_or(0.0),
        cfg.iadc.m.unwrap_or(1.0),
        cfg.iadc.b.unwrap_or(0.0),
    ];
    anyhow::ensure!(
        values.iter().all(|value| value.is_finite()),
        "DAQ calibration contains a non-finite coefficient"
    );
    let mut payload = Vec::with_capacity(16);
    for value in values {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    packet_bytes(DAQ_CALIBRATION_TYPE, "SD_CARD", Arc::from(payload))
}

fn kg50_calibration_packet(cfg: &crate::loadcell::LoadcellCalibrationFile) -> Result<Packet> {
    let values = crate::loadcell::kg50_daq_coefficients(cfg).map_err(anyhow::Error::msg)?;
    let payload: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    packet_bytes(DAQ_KG50_CALIBRATION_TYPE, "SD_CARD", Arc::from(payload))
}

pub fn set_daq_calibration(
    router: &Router,
    cfg: &crate::loadcell::LoadcellCalibrationFile,
) -> Result<()> {
    let request = crate::qualification_requests::Request::begin("set_daq_calibration");
    // The input stands alone: unusable AMP2 coefficients must not hold back a
    // physical input selection, so it is sent even when they fail.
    let calibration = (|| -> Result<()> {
        let amp2 = crate::loadcell::amp2_calibration(cfg);
        let thermal = thermal_packet(&amp2)?;
        let filter = filter_packet(&amp2)?;
        let kg50 = kg50_calibration_packet(&amp2)?;
        request.submit(router, calibration_packet(&amp2)?)?;
        request.submit(router, kg50)?;
        request.submit(router, thermal)?;
        request.submit(router, filter)
    })();
    let input = request.submit(router, kg50_input_packet(cfg)?);
    calibration.and(input)
}

/// Stamped with the selection's revision rather than the time of sending. The
/// SDK drops a value older than the one a board has cached from this sender, so
/// a station clock stepped backwards would otherwise leave the DAQ stuck on the
/// old input. Re-sending the same revision is accepted; only the nonce differs.
fn kg50_input_packet(cfg: &crate::loadcell::LoadcellCalibrationFile) -> Result<Packet> {
    packet_bytes_at(
        "DAQ_KG50_INPUT",
        "SD_CARD",
        Arc::from([cfg.kg50_input.wire()]),
        match cfg.kg50_input_stamp_ms {
            0 => get_current_timestamp_ms(),
            stamp => stamp,
        },
    )
}

pub fn set_daq_kg50_input(
    router: &Router,
    cfg: &crate::loadcell::LoadcellCalibrationFile,
) -> Result<()> {
    crate::qualification_requests::Request::begin("set_daq_kg50_input")
        .submit(router, kg50_input_packet(cfg)?)
}

fn daq_log_clock_payload(
    session: u64,
    clock: &crate::telemetry_db::LaunchClockMsg,
    launch_now: i64,
    network_now: i64,
) -> Result<Vec<u8>> {
    use crate::telemetry_db::LaunchClockKind;
    let deadline = if clock.kind == LaunchClockKind::Idle {
        0
    } else {
        let anchor = clock
            .anchor_timestamp_ms
            .context("Launch clock has no anchor")?;
        let countdown = if clock.kind == LaunchClockKind::TMinus {
            clock.duration_ms.unwrap_or(0)
        } else {
            0
        };
        let deadline = network_now
            .saturating_add(anchor.saturating_sub(launch_now))
            .saturating_add(countdown)
            .saturating_add(20_000);
        anyhow::ensure!(
            (315_532_800_000..4_354_819_200_000).contains(&deadline),
            "Set GroundStation/network UTC before timestamped DAQ launch logging"
        );
        deadline as u64
    };
    let session = if deadline == 0 { 0 } else { session };
    anyhow::ensure!(
        deadline == 0 || session != 0,
        "DAQ launch session must be nonzero"
    );
    Ok([session.to_le_bytes(), deadline.to_le_bytes()].concat())
}

fn daq_log_clock_packet(pair: [u64; 2], stamp: u64) -> Result<Packet> {
    Ok(Packet::new(
        crate::telemetry_schema::data_type("DAQ_LOG_CLOCK"),
        &[crate::telemetry_schema::endpoint("SD_CARD")],
        Board::GroundStation.sender_id(),
        stamp,
        Arc::<[u8]>::from([pair[0].to_le_bytes(), pair[1].to_le_bytes()].concat()),
    )?
    .with_nonce(NETWORK_VARIABLE_NONCE.fetch_add(1, Ordering::Relaxed)))
}

/* A control revision, independent of UTC corrections. Commit it atomically
 * with the unchanged16-byte value; replays must never create a new revision. */
fn commit_daq_clock_pair(guard: &mut VariableStore, pair: [u64; 2], wall_ms: u64) -> Result<u64> {
    let mut next = guard.values;
    if next.daq_log_clock != pair || next.daq_log_clock_stamp == 0 {
        next.daq_log_clock_stamp = next
            .daq_log_clock_stamp
            .checked_add(1)
            .context("DAQ launch-clock revision exhausted")?
            .max(wall_ms);
    }
    next.daq_log_clock = pair;
    next.daq_log_last_session = next.daq_log_last_session.max(pair[0]);
    guard.commit(next)?;
    Ok(next.daq_log_clock_stamp)
}

fn cached_daq_log_clock_packet() -> Result<Packet> {
    let values = store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values;
    daq_log_clock_packet(values.daq_log_clock, values.daq_log_clock_stamp)
}

/// Exact retained payload/revision for the launch-clock journal. No disk I/O.
pub fn persisted_daq_log_clock_with_revision() -> Option<([u64; 2], u64)> {
    let values = store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values;
    (values.daq_log_clock_stamp != 0).then_some((values.daq_log_clock, values.daq_log_clock_stamp))
}

fn restore_daq_clock_snapshot(
    guard: &mut VariableStore,
    pair: [u64; 2],
    stamp: u64,
) -> Result<bool> {
    anyhow::ensure!(
        stamp != 0
            && ((pair[0] == 0 && pair[1] == 0)
                || (pair[0] != 0 && (315_532_800_000..4_354_819_200_000).contains(&pair[1]))),
        "Invalid retained DAQ launch-clock snapshot"
    );
    if guard.values.daq_log_clock_stamp != 0 {
        anyhow::ensure!(
            guard.values.daq_log_clock == pair && guard.values.daq_log_clock_stamp == stamp,
            "Existing DAQ launch-clock revision differs from the journal"
        );
        return Ok(false);
    }
    anyhow::ensure!(
        guard.values.daq_log_clock == [0, 0] || guard.values.daq_log_clock == pair,
        "Legacy DAQ launch-clock value differs from the journal"
    );
    let mut next = guard.values;
    next.daq_log_clock = pair;
    next.daq_log_clock_stamp = stamp;
    next.daq_log_last_session = next.daq_log_last_session.max(pair[0]);
    guard.commit(next)?;
    Ok(true)
}

/// Restore only a missing/unstamped matching cache. Preserve the literal stamp;
/// never refresh it from wall time, publish, or roll back a conflicting revision.
pub fn restore_persisted_daq_log_clock_snapshot(pair: [u64; 2], stamp: u64) -> Result<bool> {
    restore_daq_clock_snapshot(
        &mut store()
            .lock()
            .expect("network-variable store lock poisoned"),
        pair,
        stamp,
    )
}

pub fn next_daq_log_session(anchor_ms: i64) -> u64 {
    let values = store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values;
    let last = values.daq_log_last_session.max(values.daq_log_clock[0]);
    (anchor_ms.max(1) as u64).max(last.saturating_add(1))
}

fn usable_daq_utc(network_ms: Option<u64>, wall_ms: i64) -> i64 {
    network_ms
        .and_then(|ms| i64::try_from(ms).ok())
        .filter(|ms| (315_532_800_000..4_354_819_200_000).contains(ms))
        .unwrap_or(wall_ms)
}

pub fn set_daq_log_clock(
    router: &Router,
    session: u64,
    clock: &crate::telemetry_db::LaunchClockMsg,
) -> Result<()> {
    let request = crate::qualification_requests::Request::begin("set_daq_log_clock");
    let launch_now = crate::telemetry_task::procedure_clock::timestamp_ms();
    let network_now = usable_daq_utc(
        router.network_time().and_then(|t| t.unix_time_ms),
        launch_now,
    );

    let payload = daq_log_clock_payload(session, clock, launch_now, network_now)?;
    // Body and revision share one durable cache commit. UTC rollback and
    // primary restart cannot make an older launch/T0/reset appear newer.
    let pair = [
        u64::from_le_bytes(payload[..8].try_into().unwrap()),
        u64::from_le_bytes(payload[8..].try_into().unwrap()),
    ];
    let stamp = {
        let mut guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        let revision = commit_daq_clock_pair(&mut guard, pair, get_current_timestamp_ms())?;
        // Publish ordering follows the durable commit; no SDK lock is taken
        // while holding either application lock.
        PENDING_DAQ_CLOCK.lock().unwrap().record(pair, revision);
        revision
    };
    request.submit(router, daq_log_clock_packet(pair, stamp)?)?;
    PENDING_DAQ_CLOCK.lock().unwrap().accepted((pair, stamp));
    DAQ_CLOCK_RETRY_WARNED.store(false, Ordering::Relaxed);
    Ok(())
}

/// Retry only an unadmitted durable revision. Keep its original deadline/stamp.
/// Once admitted, SDK reliable delivery and managed reconnect replay own it.
/// Returns true only when an outstanding snapshot was admitted this call.
fn retry_pending_daq_log_clock(router: &Router) -> Result<bool> {
    if crate::rrls::is_rrls() || router.sender().as_ref() != "GS" {
        return Ok(false);
    }
    let Some((pair, stamp)) = PENDING_DAQ_CLOCK.lock().unwrap().desired else {
        return Ok(false);
    };
    let request = crate::qualification_requests::Request::begin("retry_pending_daq_log_clock");
    match request.submit(router, daq_log_clock_packet(pair, stamp)?) {
        Ok(()) => {
            PENDING_DAQ_CLOCK.lock().unwrap().accepted((pair, stamp));
            DAQ_CLOCK_RETRY_WARNED.store(false, Ordering::Relaxed);
            Ok(true)
        }
        Err(error) => {
            let failures = DAQ_CLOCK_RETRY_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
            if !DAQ_CLOCK_RETRY_WARNED.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "DAQ launch-clock publication remains pending after SDK admission failure ({failures} retry failures): {error}"
                );
            }
            Err(error)
        }
    }
}

#[cfg(test)]
fn toggle_persisted(store: &mut VariableStore) -> Result<bool> {
    let mut next = store.values;
    next.av_bay_underglow = !next.av_bay_underglow;
    store.commit(next)?;
    Ok(store.values.av_bay_underglow)
}

#[cfg(test)]
fn set_persisted_flight_state(store: &mut VariableStore, state: u8) -> Result<()> {
    anyhow::ensure!(state <= 15, "invalid flight-state value {state}");
    let mut next = store.values;
    next.flight_state = state;
    store.commit(next)
}

pub fn underglow_enabled() -> bool {
    store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values
        .av_bay_underglow
}

pub fn flight_buzzer_enabled() -> bool {
    store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values
        .flight_buzzer
}

pub fn flight_state() -> u8 {
    store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values
        .flight_state
}

/// Qualification uses the engine's completed-sample marker to distinguish a
/// fresh isolated boot from a process restart with intentionally retained data.
/// Production startup never calls this check or changes the cached state.
pub(crate) fn verify_normal_qualification_boot() -> Result<()> {
    let marker = std::env::var_os("FIRMWARE_SIM_SAMPLE_FILE")
        .context("normal qualification requires the engine sample marker")?;
    let completed = fs::read_to_string(marker)
        .context("read normal qualification sample marker")?
        .trim()
        .parse::<usize>()
        .context("parse normal qualification sample marker")?;
    verify_qualification_boot_cache(&cache_path(), completed, flight_state())
}

fn verify_qualification_boot_cache(path: &Path, completed: usize, actual: u8) -> Result<()> {
    let cached = read_persisted(path);
    if completed == 0 {
        anyhow::ensure!(
            cached.is_none(),
            "fresh normal qualification requires an isolated empty cache"
        );
        anyhow::ensure!(
            actual == 0,
            "fresh normal qualification must boot actual Startup0"
        );
    } else {
        let cached =
            cached.context("restarted normal qualification requires valid retained state")?;
        anyhow::ensure!(
            actual == cached.flight_state,
            "restarted normal qualification must restore the exact persisted flight state"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sedsnet::router::{EndpointHandler, RouterConfig};
    /// Bench 2026-11-11: after the failover the RRLS refused its own underglow
    /// toggle with PermissionDenied, because its outputs stayed read-only.
    #[test]
    fn a_takeover_makes_the_rrls_outputs_writable_and_hand_back_restores_read_only() {
        crate::telemetry_schema::initialize().unwrap();
        let router = Router::new(RouterConfig::new([]).with_sender("RRLS"));
        let output = NetworkOutput::Underglow;
        let ty = crate::telemetry_schema::data_type(output.data_type());
        router.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY).unwrap();
        let write = |r: &Router| r.set_network_variable(packet(output.data_type(), output.endpoint(), 1).unwrap());
        assert!(matches!(write(&router), Err(sedsnet::TelemetryError::PermissionDenied)));
        set_takeover_outputs_writable(&router, true).unwrap();
        assert!(write(&router).is_ok());
        set_takeover_outputs_writable(&router, false).unwrap();
        assert!(matches!(write(&router), Err(sedsnet::TelemetryError::PermissionDenied)));
    }

    #[test]
    fn qualification_requires_fresh_startup_zero_and_exact_restart_restoration() {
        let root = std::env::temp_dir().join(format!(
            "gs-qualified-boot-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let path = root.join("variables.json");
        assert!(verify_qualification_boot_cache(&path, 0, 0).is_ok());
        assert!(verify_qualification_boot_cache(&path, 0, 1).is_err());
        assert!(verify_qualification_boot_cache(&path, 4, 0).is_err());
        let mut first = VariableStore::new(path.clone());
        first
            .commit(PersistentVariables {
                flight_state: 1,
                ..PersistentVariables::default()
            })
            .unwrap();
        let restored = VariableStore::new(path.clone());
        assert!(verify_qualification_boot_cache(&path, 4, restored.values.flight_state).is_ok());
        assert!(verify_qualification_boot_cache(&path, 8, 1).is_ok());
        assert!(verify_qualification_boot_cache(&path, 4, 0).is_err());
        assert!(verify_qualification_boot_cache(&path, 0, 1).is_err());
        fs::write(&path, "invalid retained cache").unwrap();
        assert!(verify_qualification_boot_cache(&path, 4, 0).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_cache_is_owned_by_each_test_process_and_explicit_paths_are_preserved() {
        const CHILD: &str = "GS_NETWORK_CACHE_ISOLATION_TEST_CHILD";
        const NAME: &str = "network_variables::tests::default_cache_is_owned_by_each_test_process_and_explicit_paths_are_preserved";
        const MARKER: &str = "GS_NETWORK_CACHE_RESULT ";
        if let Ok(value) = std::env::var(CHILD) {
            let state = value.parse::<u8>().unwrap();
            let path = cache_path();
            // Fail before writing anything if the old shared production default is selected.
            assert_ne!(path, PathBuf::from(DEFAULT_CACHE_PATH));
            assert!(path.starts_with(std::env::temp_dir()));
            assert_eq!(
                cache_path(),
                path,
                "one process must retain one default store"
            );
            persist(
                &path,
                PersistentVariables {
                    flight_state: state,
                    ..Default::default()
                },
            )
            .unwrap();
            println!("{MARKER}{}", serde_json::json!({"path":path,"state":state}));
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "network-cache-owner-proof-{}-{}",
            std::process::id(),
            rand::random::<u128>()
        ));
        let explicit = root.join("explicit.json");
        let mut children = Vec::new();
        for state in [1u8, 2, 3] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD, state.to_string());
            if state == 3 {
                child.env("GS_NETWORK_VARIABLE_CACHE", &explicit);
            } else {
                child.env_remove("GS_NETWORK_VARIABLE_CACHE");
            }
            children.push(
                child
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        }
        let mut paths = Vec::new();
        let outputs = children
            .into_iter()
            .map(|child| child.wait_with_output().unwrap())
            .collect::<Vec<_>>();
        for (index, output) in outputs.into_iter().enumerate() {
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            let row = stdout
                .lines()
                .find_map(|line| line.split_once(MARKER).map(|(_, row)| row))
                .expect("child cache ownership report");
            let row: serde_json::Value = serde_json::from_str(row).unwrap();
            let path = PathBuf::from(row["path"].as_str().unwrap());
            assert_eq!(
                load(&path).flight_state,
                index as u8 + 1,
                "another test process must not replace this process's cache"
            );
            paths.push(path);
        }
        assert_ne!(
            paths[0], paths[1],
            "forked tests must not share json or json.tmp paths"
        );
        assert_eq!(
            paths[2], explicit,
            "explicit restart-test/config paths remain unchanged"
        );
        for path in &paths[..2] {
            fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn daq_clock_ignores_uptime_and_overflow_but_preserves_valid_network_utc() {
        let wall = 1_800_000_000_000;
        for invalid in [None, Some(0), Some(500_000), Some(u64::MAX)] {
            assert_eq!(usable_daq_utc(invalid, wall), wall);
        }
        assert_eq!(usable_daq_utc(Some(wall as u64 + 1234), wall), wall + 1234);
        let router = Router::new(RouterConfig::new([]));
        crate::telemetry_task::refresh_host_network_time(&router, wall as u64);
        let utc = router.network_time().unwrap().unix_time_ms.unwrap();
        assert!((wall as u64..wall as u64 + 1000).contains(&utc));
    }

    #[test]
    fn daq_deadline_uses_t_zero_not_launch_button_or_reconnect_time() {
        use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
        let now = 1_800_000_000_000;
        let clock = crate::state::launch_countdown_clock(now);
        let decode = |bytes: Vec<u8>| -> (u64, u64) {
            (
                u64::from_le_bytes(bytes[..8].try_into().unwrap()),
                u64::from_le_bytes(bytes[8..].try_into().unwrap()),
            )
        };
        let expected = (17, (now + 30_000) as u64);
        assert_eq!(
            decode(daq_log_clock_payload(17, &clock, now, now).unwrap()),
            expected
        );
        assert_eq!(
            decode(daq_log_clock_payload(17, &clock, now + 50_000, now + 50_000).unwrap()),
            expected
        );
        let pilot = LaunchClockMsg {
            kind: LaunchClockKind::TPlus,
            anchor_timestamp_ms: Some(now + 10_000),
            duration_ms: None,
        };
        assert_eq!(
            decode(daq_log_clock_payload(17, &pilot, now + 10_000, now + 10_000).unwrap()),
            expected
        );
        assert_eq!(
            decode(daq_log_clock_payload(17, &LaunchClockMsg::idle(), now, now).unwrap()),
            (0, 0)
        );
        assert!(daq_log_clock_payload(0, &clock, now, now).is_err());
        assert!(daq_log_clock_payload(17, &crate::state::launch_countdown_clock(0), 0, 0).is_err());
    }

    #[test]
    fn model_virtual_utc_deadline_is_stable_across_pause_and_late_ascent() {
        use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
        let epoch = 1_767_225_600_000i64;
        let start = epoch + 100_000;
        let clock = crate::state::launch_countdown_clock(start);
        let deadline = |clock: &LaunchClockMsg, now: i64| {
            let bytes = daq_log_clock_payload(17, clock, now, now).unwrap();
            u64::from_le_bytes(bytes[8..].try_into().unwrap())
        };
        let expected = (start + 30_000) as u64;
        assert_eq!(deadline(&clock, start), expected);
        assert_eq!(deadline(&clock, start + 833), expected);
        assert_eq!(deadline(&clock, start + 833), expected); // paused model
        let ascent = LaunchClockMsg {
            kind: LaunchClockKind::TPlus,
            anchor_timestamp_ms: Some(start + 10_000),
            duration_ms: None,
        };
        assert_eq!(deadline(&ascent, start + 10_000), expected);
        assert_eq!(deadline(&ascent, start + 12_500), expected);
    }

    #[test]
    fn daq_revision_survives_restart_wall_rollback_and_replay_without_restamping() {
        crate::telemetry_schema::initialize().unwrap();
        let path = std::env::temp_dir().join(format!(
            "daq-revisions-{}-{}.json",
            std::process::id(),
            rand::random::<u64>()
        ));
        let pair = [10, 1_800_000_030_000];
        let mut first = VariableStore::new(path.clone());
        let initial = commit_daq_clock_pair(&mut first, pair, 1_800_000_000_000).unwrap();
        let original = fs::read(&path).unwrap();
        let mut restarted = VariableStore::new(path.clone());
        assert_eq!(restarted.values.daq_log_clock_stamp, initial);
        let blocker = path.with_extension("json.tmp");
        fs::create_dir(&blocker).unwrap();
        assert_eq!(
            commit_daq_clock_pair(&mut restarted, pair, initial + 99999).unwrap(),
            initial
        );
        assert_eq!(fs::read(&path).unwrap(), original); // identical replay needs no write
        fs::remove_dir(&blocker).unwrap();
        let later_t0 = [10, pair[1] + 5000];
        let correction = commit_daq_clock_pair(&mut restarted, later_t0, initial - 10000).unwrap();
        assert_eq!(correction, initial + 1); // wall clock went backwards
        let reset = commit_daq_clock_pair(&mut restarted, [0, 0], 0).unwrap();
        assert_eq!(reset, correction + 1);
        let mut second_restart = VariableStore::new(path.clone());
        assert_eq!(second_restart.values.daq_log_last_session, 10);
        assert_eq!(second_restart.values.daq_log_clock, [0, 0]);
        let next = commit_daq_clock_pair(&mut second_restart, [11, pair[1] + 30000], 0).unwrap();
        assert_eq!(next, reset + 1);
        for _ in 0..3 {
            let replay = daq_log_clock_packet(second_restart.values.daq_log_clock, next).unwrap();
            assert_eq!(replay.timestamp(), next);
            assert_eq!(replay.payload().len(), 16);
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn daq_revision_upgrade_and_overflow_are_atomic() {
        let path = std::env::temp_dir().join(format!(
            "daq-revision-atomic-{}-{}.json",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::write(
            &path,
            r#"{"daq_log_clock":[7,1800000030000],"daq_log_last_session":7}"#,
        )
        .unwrap();
        let mut legacy = VariableStore::new(path.clone());
        assert_eq!(legacy.values.daq_log_clock_stamp, 0);
        assert_eq!(
            commit_daq_clock_pair(&mut legacy, [7, 1_800_000_030_000], 1234).unwrap(),
            1234
        );
        let mut maximum = legacy.values;
        maximum.daq_log_clock_stamp = u64::MAX;
        legacy.commit(maximum).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(commit_daq_clock_pair(&mut legacy, [0, 0], 0).is_err());
        assert_eq!(legacy.values, maximum);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            commit_daq_clock_pair(&mut legacy, maximum.daq_log_clock, 0).unwrap(),
            u64::MAX
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn journal_daq_snapshot_restore_preserves_literal_revision_and_refuses_conflict() {
        let path = std::env::temp_dir().join(format!(
            "daq-journal-restore-{}-{}.json",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut missing = VariableStore::new(path.clone());
        let pair = [17, 1_800_000_030_000];
        assert!(restore_daq_clock_snapshot(&mut missing, pair, 4321).unwrap());
        assert_eq!(missing.values.daq_log_clock_stamp, 4321);
        assert_eq!(VariableStore::new(path.clone()).values, missing.values);
        assert!(!restore_daq_clock_snapshot(&mut missing, pair, 4321).unwrap());
        assert!(restore_daq_clock_snapshot(&mut missing, [0, 0], 4322).is_err());
        assert!(restore_daq_clock_snapshot(&mut missing, pair, 4320).is_err());
        assert_eq!(missing.values.daq_log_clock, pair);
        assert!(restore_daq_clock_snapshot(&mut missing, pair, 0).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn daq_launch_cache_survives_groundstation_restart_without_extending_run() {
        let path = std::env::temp_dir().join(format!("gs-daq-clock-{}.json", std::process::id()));
        let values = PersistentVariables {
            daq_log_clock: [123, 1_800_000_130_000],
            ..Default::default()
        };
        persist(&path, values).unwrap();
        assert_eq!(load(&path).daq_log_clock, values.daq_log_clock);
        persist(&path, PersistentVariables::default()).unwrap();
        assert_eq!(load(&path).daq_log_clock, [0, 0]);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn daq_retained_launch_clock_recovers_after_either_router_restarts() {
        crate::telemetry_schema::initialize().unwrap();
        let ty = crate::telemetry_schema::data_type("DAQ_LOG_CLOCK");
        let ground_link: Arc<Mutex<Option<Arc<Router>>>> = Default::default();
        let daq_link: Arc<Mutex<Option<Arc<Router>>>> = Default::default();
        let observed: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let revisions: Arc<Mutex<Vec<u64>>> = Default::default();
        let saved_revision = std::sync::atomic::AtomicU64::new(1_800_000_000_000);
        let expected: Vec<u8> = [123u64.to_le_bytes(), 1_800_000_130_000u64.to_le_bytes()].concat();
        let make_ground = || {
            let r = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
            r.enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
                .unwrap();
            r.seed_managed_variable(
                daq_log_clock_packet(
                    [123, 1_800_000_130_000],
                    saved_revision.load(Ordering::Relaxed),
                )
                .unwrap(),
            )
            .unwrap();
            let link = daq_link.clone();
            r.add_side_packet("fill", move |p| {
                let receiver = link.lock().unwrap().clone();
                receiver.map_or(Ok(()), |r| r.rx_from_side(p, 0))
            });
            r
        };
        let make_daq = || {
            let r = Arc::new(Router::new(RouterConfig::new([]).with_sender("DAQ")));
            r.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
                .unwrap();
            let observations = observed.clone();
            let stamps = revisions.clone();
            r.on_network_variable_update(ty, move |p| {
                observations.lock().unwrap().push(p.payload().to_vec());
                stamps.lock().unwrap().push(p.timestamp());
                Ok(())
            })
            .unwrap();
            let link = ground_link.clone();
            r.add_side_packet("can", move |p| {
                let receiver = link.lock().unwrap().clone();
                receiver.map_or(Ok(()), |r| r.rx_from_side(p, 0))
            });
            r
        };
        let mut ground = make_ground();
        let mut daq = make_daq();
        for round in 0..3 {
            if round == 1 {
                *ground_link.lock().unwrap() = None;
                ground = make_ground();
            }
            if round == 2 {
                *daq_link.lock().unwrap() = None;
                daq = make_daq();
            }
            *ground_link.lock().unwrap() = Some(ground.clone());
            *daq_link.lock().unwrap() = Some(daq.clone());
            if round == 2 {
                observed.lock().unwrap().clear();
            }
            ground.announce_discovery().unwrap();
            daq.announce_discovery().unwrap();
            daq.request_managed_variable(ty).unwrap();
            for _ in 0..32 {
                ground.process_all_queues().unwrap();
                daq.process_all_queues().unwrap();
            }
            assert_eq!(
                observed.lock().unwrap().last(),
                Some(&expected),
                "restart round {round}"
            );
            assert_eq!(
                revisions.lock().unwrap().last().copied(),
                Some(saved_revision.load(Ordering::Relaxed)),
                "replay must keep the saved revision"
            );
            // Also prove the new GS instance can deliver a changed clock, not
            // merely that DAQ still remembers the value from before restart.
            let changed: Vec<u8> =
                [124u64.to_le_bytes(), 1_800_000_140_000u64.to_le_bytes()].concat();
            ground
                .set_network_variable(
                    daq_log_clock_packet(
                        [124, 1_800_000_140_000],
                        saved_revision.fetch_add(1, Ordering::Relaxed) + 1,
                    )
                    .unwrap(),
                )
                .unwrap();
            for _ in 0..32 {
                ground.process_all_queues().unwrap();
                daq.process_all_queues().unwrap();
            }
            assert_eq!(observed.lock().unwrap().last(), Some(&changed));
            std::thread::sleep(std::time::Duration::from_millis(2));
            ground
                .set_network_variable(
                    daq_log_clock_packet(
                        [123, 1_800_000_130_000],
                        saved_revision.fetch_add(1, Ordering::Relaxed) + 1,
                    )
                    .unwrap(),
                )
                .unwrap();
            for _ in 0..32 {
                ground.process_all_queues().unwrap();
                daq.process_all_queues().unwrap();
            }
        }
        // Release simulated cable endpoints; callbacks must not leak routers.
        *ground_link.lock().unwrap() = None;
        *daq_link.lock().unwrap() = None;
    }

    #[test]
    fn legacy_rate_cache_preserves_only_runtime_settings() {
        let values: PersistentVariables = serde_json::from_str(
            r#"{"av_bay_underglow":true,"flight_buzzer":true,"flight_state":6,"rf_telemetry_rate_hz":2,"fc_telemetry_rate_hz":4}"#,
        ).unwrap();
        assert!(values.av_bay_underglow);
        assert!(values.flight_buzzer);
        assert_eq!(values.flight_state, 6);
        let saved = serde_json::to_value(values).unwrap();
        assert!(saved.get("rf_telemetry_rate_hz").is_none());
        assert!(saved.get("fc_telemetry_rate_hz").is_none());
    }

    #[test]
    fn missing_cache_defaults_to_off() {
        let path = std::env::temp_dir().join(format!(
            "gs26-missing-network-vars-{}.json",
            std::process::id()
        ));
        assert!(!load(&path).av_bay_underglow);
    }

    #[test]
    fn persisted_cache_round_trips() {
        let path =
            std::env::temp_dir().join(format!("gs26-network-vars-{}.json", std::process::id()));
        persist(
            &path,
            PersistentVariables {
                av_bay_underglow: true,
                flight_buzzer: true,
                flight_state: 6,
                flight_state_stamp: 0,
                daq_log_clock: [0, 0],
                daq_log_clock_stamp: 0,
                daq_log_last_session: 0,
            },
        )
        .unwrap();
        assert!(load(&path).av_bay_underglow);
        assert!(load(&path).flight_buzzer);
        assert_eq!(load(&path).flight_state, 6);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn persisted_toggle_survives_two_process_style_reloads() {
        let path = std::env::temp_dir().join(format!(
            "gs26-network-vars-toggle-{}.json",
            std::process::id()
        ));
        persist(
            &path,
            PersistentVariables {
                av_bay_underglow: true,
                ..Default::default()
            },
        )
        .unwrap();

        let mut first_restart = VariableStore::new(path.clone());
        assert!(!toggle_persisted(&mut first_restart).unwrap());
        assert!(!load(&path).av_bay_underglow);

        let mut second_restart = VariableStore::new(path.clone());
        assert!(toggle_persisted(&mut second_restart).unwrap());
        assert!(load(&path).av_bay_underglow);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn flight_state_changes_survive_process_style_reloads() {
        let path = std::env::temp_dir().join(format!(
            "gs26-flight-state-restarts-{}.json",
            std::process::id()
        ));
        persist(&path, PersistentVariables::default()).unwrap();

        for expected in [1, 0, 1] {
            let mut restarted = VariableStore::new(path.clone());
            set_persisted_flight_state(&mut restarted, expected).unwrap();
            assert_eq!(load(&path).flight_state, expected);
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrupt_out_of_range_flight_state_fails_safe() {
        let path = std::env::temp_dir().join(format!(
            "gs26-invalid-flight-state-{}.json",
            std::process::id()
        ));
        fs::write(&path, br#"{"flight_state":255}"#).unwrap();
        assert_eq!(load(&path).flight_state, 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn network_variable_reaches_a_read_only_avionics_peer() {
        crate::telemetry_schema::initialize().unwrap();
        let source = Arc::new(Router::new(
            RouterConfig::new([]).with_sender(Board::GroundStation.sender_id()),
        ));
        let peer = Arc::new(Router::new(RouterConfig::new([]).with_sender("RF")));
        let ty = crate::telemetry_schema::data_type(UNDERGLOW_TYPE);
        source
            .enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
            .unwrap();
        peer.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
            .unwrap();

        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_callback = observed.clone();
        peer.on_network_variable_update(ty, move |packet| {
            observed_callback
                .lock()
                .unwrap()
                .push(packet.payload().to_vec());
            Ok(())
        })
        .unwrap();

        let peer_rx = peer.clone();
        source.add_side_packet("to-rf", move |packet| peer_rx.rx_from_side(packet, 0));
        let source_rx = source.clone();
        peer.add_side_packet("to-gs", move |packet| source_rx.rx_from_side(packet, 0));

        source
            .set_network_variable(packet(UNDERGLOW_TYPE, "AV_BAY_UNDERGLOW_OWNER", 1).unwrap())
            .unwrap();
        source.process_all_queues().unwrap();
        peer.process_all_queues().unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![vec![1]]);
    }

    #[test]
    fn cosmetic_variables_confirm_from_any_network_publisher_without_an_owner_board() {
        if crate::operator_queue::isolated_native_case(
            module_path!(),
            "cosmetic_variables_confirm_from_any_network_publisher_without_an_owner_board",
        ) {
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let gs = Arc::new(Router::new(
            RouterConfig::new([]).with_sender(Board::GroundStation.sender_id()),
        ));
        initialize_for_role_seed(&gs, false, false).unwrap();
        for output in NetworkOutput::ALL {
            output.commit_requested(true).unwrap();
            let view = network_output_view(output);
            assert_eq!(
                (view.requested, view.confirmed, view.readback_seen),
                (true, None, false),
                "{output:?}: no network value yet means waiting for network confirmation"
            );
        }
        // No owner board ever reports; a relay that is not the RF Board or
        // FC publishes the values the GS asked for.
        let relay = Arc::new(Router::new(RouterConfig::new([]).with_sender("GW")));
        for output in NetworkOutput::ALL {
            relay
                .enable_network_variable(
                    crate::telemetry_schema::data_type(output.data_type()),
                    NetworkVariablePermissions::READ_WRITE,
                )
                .unwrap();
        }
        let gs_side = gs.add_side_packet("to-gw", |_| Ok(()));
        let gs_rx = gs.clone();
        relay.add_side_packet("to-gs", move |packet| gs_rx.rx_from_side(packet, gs_side));
        let publish = |output: NetworkOutput, value: u8| {
            relay
                .set_network_variable(packet(output.data_type(), output.endpoint(), value).unwrap())
                .unwrap();
            relay.process_all_queues().unwrap();
            gs.process_all_queues().unwrap();
        };
        publish(NetworkOutput::Underglow, 0);
        let view = network_output_view(NetworkOutput::Underglow);
        assert_eq!((view.confirmed, view.readback_seen), (None, true), "different value is plain Unconfirmed");
        assert!(reconcile_network_outputs(&gs, [0; 2], || None::<()>), "inbound value refreshes the controls");
        for output in NetworkOutput::ALL {
            publish(output, 1);
            let view = network_output_view(output);
            assert_eq!((view.confirmed, view.readback_seen), (Some(true), true), "{output:?}");
        }
        // Requesting Off again is unconfirmed until the network carries Off.
        output_off_then_confirm(&gs, &publish);

        fn output_off_then_confirm(gs: &Router, publish: &dyn Fn(NetworkOutput, u8)) {
            let output = NetworkOutput::FlightBuzzer;
            let _ = set_network_output(gs, output, false, "set_flight_buzzer");
            assert_eq!(network_output_view(output).confirmed, None);
            publish(output, 0);
            assert_eq!(network_output_view(output).confirmed, Some(false));
        }
    }

    #[test]
    fn fc_health_outputs_are_read_only_from_the_tagged_word() {
        let mut words = [0u32; 24];
        words[0] = 1;
        words[4] = 1000;
        assert_eq!(fc_outputs_from_health(&words), [None, None], "FC 13de2ca: no tag, no readback");
        words[14] = (FC_OUTPUTS_TAG << 16) | 1 | 4 | 8;
        assert_eq!(fc_outputs_from_health(&words), [Some(true), Some(false)]);
        words[14] = (FC_OUTPUTS_TAG << 16) | 1 | 2;
        assert_eq!(fc_outputs_from_health(&words), [None, None], "a restored value is not a network write");
        words[14] = (FC_OUTPUTS_TAG << 16) | 2 | 8;
        words[1] = 8;
        assert_eq!(fc_outputs_from_health(&words), [None, None], "word 14 is a UART counter here");
        words[1] = 0;
        assert_eq!(fc_outputs_from_health(&words), [None, Some(true)]);
        assert_eq!(fc_outputs_from_health(&words[..23]), [None, None]);
    }

    /// Bench 2026-11-11: the FC applied every underglow write (ST-Link
    /// counters) but the GS confirmed none, because the end-to-end ACK was
    /// lost on the busy radio. The FC's own health report confirms it.
    #[test]
    fn cosmetic_variables_confirm_from_the_fc_health_report() {
        if crate::operator_queue::isolated_native_case(
            module_path!(),
            "cosmetic_variables_confirm_from_the_fc_health_report",
        ) {
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let gs = Arc::new(Router::new(
            RouterConfig::new([]).with_sender(Board::GroundStation.sender_id()),
        ));
        initialize_for_role_seed(&gs, false, false).unwrap();
        NetworkOutput::Underglow.commit_requested(true).unwrap();
        NetworkOutput::FlightBuzzer.commit_requested(false).unwrap();
        let _ = set_network_output(&gs, NetworkOutput::Underglow, true, "set_underglow");
        let _ = set_network_output(&gs, NetworkOutput::FlightBuzzer, false, "set_flight_buzzer");
        for output in NetworkOutput::ALL {
            assert_eq!(network_output_view(output).confirmed, None, "{output:?}: the ACK never came back");
        }
        let mut words = [0u32; 24];
        words[0] = 1;
        words[4] = 1000;
        words[14] = (FC_OUTPUTS_TAG << 16) | 1 | 4 | 8;
        observe_fc_health_outputs(&words);
        assert_eq!(network_output_view(NetworkOutput::Underglow).confirmed, Some(true));
        assert_eq!(network_output_view(NetworkOutput::FlightBuzzer).confirmed, Some(false));
        // The FC reports a different value: no longer confirmed.
        words[14] = (FC_OUTPUTS_TAG << 16) | 4;
        observe_fc_health_outputs(&words);
        assert_eq!(network_output_view(NetworkOutput::Underglow).confirmed, None);
    }

    #[test]
    fn flight_buzzer_targets_only_the_discovered_flight_controller() {
        crate::telemetry_schema::initialize().unwrap();
        let packet = packet(FLIGHT_BUZZER_TYPE, "FLIGHT_CONTROLLER", 1).unwrap();
        assert_eq!(
            packet.endpoints(),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")]
        );
        assert_eq!(packet.payload(), &[1]);
    }

    #[test]
    fn kg50_calibration_packet_has_separate_type_and_tare() {
        crate::telemetry_schema::initialize().unwrap();
        let mut cfg = crate::loadcell::LoadcellCalibrationFile::default();
        cfg.extra_channels.insert(
            "kg50".into(),
            crate::loadcell::GenericCalibrationChannel {
                linear: crate::loadcell::ChannelLinear {
                    m: Some(2.0),
                    b: Some(1.0),
                },
                zero_raw: Some(3.0),
                ..Default::default()
            },
        );
        let packet = kg50_calibration_packet(&cfg).unwrap();
        assert_eq!(
            packet.data_type(),
            crate::telemetry_schema::data_type("DAQ_KG50_CALIBRATION")
        );
        assert_eq!(
            packet.endpoints(),
            &[crate::telemetry_schema::endpoint("SD_CARD")]
        );
        let decoded: Vec<f32> = packet
            .payload()
            .as_chunks::<4>().0.iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        assert_eq!(decoded, vec![1.0, 2.0, 0.0, 0.0, 0.0, 0.0, 7.0]);
        assert_eq!(calibration_packet(&cfg).unwrap().payload().len(), 16);
    }

    #[test]
    fn daq_calibration_packet_contains_all_linear_coefficients() {
        crate::telemetry_schema::initialize().unwrap();
        let mut cfg = crate::loadcell::LoadcellCalibrationFile::default();
        cfg.ch1.m = Some(2.0);
        cfg.ch1.b = Some(-3.0);
        cfg.iadc.m = Some(4.5);
        cfg.iadc.b = Some(6.0);
        let packet = calibration_packet(&cfg).unwrap();
        let decoded: Vec<f32> = packet
            .payload()
            .as_chunks::<4>().0.iter()
            .map(|bytes| f32::from_le_bytes(*bytes))
            .collect();
        assert_eq!(decoded, vec![2.0, -3.0, 4.5, 6.0]);
    }

    #[test]
    fn rrls_startup_has_no_gs_owned_defaults_and_cannot_publish_controls() {
        crate::telemetry_schema::initialize().unwrap();
        let observer = Router::new(RouterConfig::new([]).with_sender("RRLS"));
        initialize_for_role(&observer, true).unwrap();
        for name in [UNDERGLOW_TYPE, FLIGHT_BUZZER_TYPE, FLIGHT_STATE_TYPE] {
            let ty = crate::telemetry_schema::data_type(name);
            assert!(observer.get_cached_network_variable(ty).unwrap().is_none());
        }
        assert!(
            observer
                .set_network_variable(packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", 0).unwrap())
                .is_err()
        );
        observer.process_all_queues().unwrap();
        assert!(
            observer
                .get_cached_network_variable(crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn flight_state_network_variable_reaches_a_read_only_avionics_peer() {
        crate::telemetry_schema::initialize().unwrap();
        let source = Arc::new(Router::new(
            RouterConfig::new([]).with_sender(Board::GroundStation.sender_id()),
        ));
        let peer = Arc::new(Router::new(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("FLIGHT_STATE"),
                |_packet| Ok(()),
            )])
            .with_sender("FC"),
        ));
        let ty = crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE);
        source
            .enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
            .unwrap();
        peer.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
            .unwrap();

        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_callback = observed.clone();
        peer.on_network_variable_update(ty, move |packet| {
            observed_callback.lock().unwrap().push(packet.payload()[0]);
            Ok(())
        })
        .unwrap();
        let peer_rx = peer.clone();
        source.add_side_packet("to-fc", move |packet| peer_rx.rx_from_side(packet, 0));
        let source_rx = source.clone();
        peer.add_side_packet("to-gs", move |packet| source_rx.rx_from_side(packet, 0));

        for state in [1, 0, 1] {
            std::thread::sleep(std::time::Duration::from_millis(2));
            source
                .set_network_variable(packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", state).unwrap())
                .unwrap();
            source.process_all_queues().unwrap();
            peer.process_all_queues().unwrap();
        }
        assert_eq!(*observed.lock().unwrap(), vec![1, 0, 1]);
    }
}

#[cfg(test)]
mod thermal_wire_tests {
    use super::*;
    #[test]
    fn thermal_payload_is_fixed_order_and_defaults_disabled() {
        crate::telemetry_schema::initialize().unwrap();
        let mut cfg = crate::loadcell::LoadcellCalibrationFile::default();
        for i in 0..6 {
            crate::loadcell::capture_thermal_zero(
                &mut cfg,
                "KG1000",
                10. + i as f32 * 0.5,
                20. + i as f32 * 2.,
            )
            .unwrap();
        }
        let p = thermal_packet(&cfg).unwrap();
        assert_eq!(
            p.data_type(),
            crate::telemetry_schema::data_type("DAQ_THERMAL_CALIBRATION")
        );
        let values: Vec<f32> = p
            .payload()
            .as_chunks::<4>().0.iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        assert_eq!(values, vec![20., 0.25, 0., 0.]);
    }
}

#[cfg(test)]
mod persistence_callback_tests {
    use super::*;
    /// A real source and DAQ-style read-only reader joined by discovery.
    fn kg50_input_pair(
        apply: impl Fn(u8) + Send + Sync + 'static,
    ) -> (Arc<Router>, Arc<Router>) {
        use sedsnet::router::RouterConfig;
        crate::telemetry_schema::initialize().unwrap();
        let ty = crate::telemetry_schema::data_type("DAQ_KG50_INPUT");
        let source = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
        source
            .enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
            .unwrap();
        let peer = Arc::new(Router::new(RouterConfig::new([]).with_sender("DAQ")));
        peer.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
            .unwrap();
        peer.on_network_variable_update(ty, move |packet| {
            apply(packet.payload()[0]);
            Ok(())
        })
        .unwrap();
        let target = peer.clone();
        source.add_side_packed("rocket_comms", move |bytes| {
            target.rx_packed_from_side(bytes, 0)
        });
        let target = source.clone();
        peer.add_side_packed("GS", move |bytes| target.rx_packed_from_side(bytes, 0));
        peer.announce_discovery().unwrap();
        source.announce_discovery().unwrap();
        kg50_input_pump(&source, &peer);
        (source, peer)
    }
    fn kg50_input_pump(source: &Router, peer: &Router) {
        for _ in 0..30 {
            source.process_all_queues().unwrap();
            peer.process_all_queues().unwrap();
        }
    }
    #[test]
    fn kg50_input_survives_a_station_clock_stepping_backwards() {
        const NAME: &str = "network_variables::persistence_callback_tests::kg50_input_survives_a_station_clock_stepping_backwards";
        if !isolated(NAME) {
            return;
        }
        let applied = Arc::new(Mutex::new(Vec::new()));
        let sink = applied.clone();
        let (source, peer) = kg50_input_pair(move |input| sink.lock().unwrap().push(input));
        let mut cfg = crate::loadcell::LoadcellCalibrationFile {
            kg50_input: crate::loadcell::Kg50Input::P6Differential,
            kg50_input_stamp_ms: u64::MAX / 2,
            ..Default::default()
        };
        set_daq_kg50_input(&source, &cfg).unwrap();
        kg50_input_pump(&source, &peer);
        assert_eq!(*applied.lock().unwrap(), vec![1]);
        // The wall clock is far behind this revision, as after a station reboot.
        // A value stamped with the clock is the failure this stamp prevents.
        source
            .set_network_variable(packet("DAQ_KG50_INPUT", "SD_CARD", 0).unwrap())
            .unwrap();
        kg50_input_pump(&source, &peer);
        assert_eq!(*applied.lock().unwrap(), vec![1]);
        // Re-sending the same revision still reaches the application.
        set_daq_kg50_input(&source, &cfg).unwrap();
        kg50_input_pump(&source, &peer);
        assert_eq!(*applied.lock().unwrap(), vec![1, 1]);
        cfg.kg50_input = crate::loadcell::Kg50Input::Amp2;
        set_daq_kg50_input(&source, &cfg).unwrap();
        kg50_input_pump(&source, &peer);
        assert_eq!(*applied.lock().unwrap(), vec![1, 1, 0]);
    }
    /// A board on the SDK's same-sender timestamp order (every DAQ firmware
    /// up to 8ce1685) that cached a stray newer AMP2 write from "GS" drops the
    /// station's older-stamped P6 selection, yet acknowledges it. Re-stamping
    /// with the wall clock converges only when the stray stamp is in the past.
    #[test]
    fn a_restamped_selection_outranks_a_stray_newer_value_only_from_the_past() {
        const NAME: &str = "network_variables::persistence_callback_tests::a_restamped_selection_outranks_a_stray_newer_value_only_from_the_past";
        if !isolated(NAME) {
            return;
        }
        let applied = Arc::new(Mutex::new(Vec::new()));
        let sink = applied.clone();
        let (source, peer) = kg50_input_pair(move |input| sink.lock().unwrap().push(input));
        let stray = crate::loadcell::LoadcellCalibrationFile {
            kg50_input: crate::loadcell::Kg50Input::Amp2,
            kg50_input_stamp_ms: 2_000_000,
            ..Default::default()
        };
        set_daq_kg50_input(&source, &stray).unwrap();
        kg50_input_pump(&source, &peer);
        let mut cfg = crate::loadcell::LoadcellCalibrationFile {
            kg50_input: crate::loadcell::Kg50Input::P6Differential,
            kg50_input_stamp_ms: 1_000_000,
            ..Default::default()
        };
        for _ in 0..3 {
            set_daq_kg50_input(&source, &cfg).unwrap();
            kg50_input_pump(&source, &peer);
        }
        assert_eq!(*applied.lock().unwrap(), vec![0], "older stamp never applies");
        // Station clock behind the stray stamp: the re-stamp still loses.
        cfg.kg50_input_stamp_ms = 1_999_999;
        set_daq_kg50_input(&source, &cfg).unwrap();
        kg50_input_pump(&source, &peer);
        assert_eq!(*applied.lock().unwrap(), vec![0]);
        // Wall clock past the stray stamp: the re-stamped selection applies.
        cfg.kg50_input_stamp_ms = 2_000_100;
        set_daq_kg50_input(&source, &cfg).unwrap();
        kg50_input_pump(&source, &peer);
        assert_eq!(*applied.lock().unwrap(), vec![0, 1]);
    }
    #[test]
    fn unusable_amp2_coefficients_do_not_hold_back_the_kg50_input() {
        const NAME: &str = "network_variables::persistence_callback_tests::unusable_amp2_coefficients_do_not_hold_back_the_kg50_input";
        if !isolated(NAME) {
            return;
        }
        let applied = Arc::new(Mutex::new(Vec::new()));
        let sink = applied.clone();
        let (source, peer) = kg50_input_pair(move |input| sink.lock().unwrap().push(input));
        for name in [
            DAQ_CALIBRATION_TYPE,
            DAQ_KG50_CALIBRATION_TYPE,
            "DAQ_THERMAL_CALIBRATION",
            "DAQ_FILTER_CALIBRATION",
        ] {
            source
                .enable_network_variable(
                    crate::telemetry_schema::data_type(name),
                    NetworkVariablePermissions::READ_WRITE,
                )
                .unwrap();
        }
        let mut cfg = crate::loadcell::LoadcellCalibrationFile {
            kg50_input_stamp_ms: 10,
            ..Default::default()
        };
        crate::loadcell::switch_kg50_input(&mut cfg, crate::loadcell::Kg50Input::P6Differential);
        // The stored AMP2 profile is what DAQ's SD calibration is built from.
        cfg.kg50_input_profiles.insert(
            "amp2".into(),
            serde_json::from_value(serde_json::json!({
                "channel": {"linear": {"m": 1.0, "b": 0.0}, "fit": {"type": "poly2", "a": null, "b": null, "c": null, "d": null, "e": null, "x0": null}},
                "thermal": null, "noise": null, "captures": []
            }))
            .unwrap(),
        );
        let error = set_daq_calibration(&source, &cfg).unwrap_err();
        assert!(error.to_string().contains("polynomial"), "{error}");
        kg50_input_pump(&source, &peer);
        assert_eq!(*applied.lock().unwrap(), vec![1]);
    }
    #[test]
    fn bootstrap_cache_without_discovered_readers_is_not_an_operational_receipt() {
        const NAME: &str = "network_variables::persistence_callback_tests::bootstrap_cache_without_discovered_readers_is_not_an_operational_receipt";
        if !isolated(NAME) {
            return;
        }
        use sedsnet::router::RouterConfig;
        use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
        crate::telemetry_schema::initialize().unwrap();
        let names = [
            FLIGHT_STATE_TYPE,
            UNDERGLOW_TYPE,
            FLIGHT_BUZZER_TYPE,
            DAQ_CALIBRATION_TYPE,
            DAQ_KG50_CALIBRATION_TYPE,
            "DAQ_LOG_CLOCK",
            "DAQ_THERMAL_CALIBRATION",
            "DAQ_FILTER_CALIBRATION",
            "DAQ_KG50_INPUT",
        ];
        let now = Arc::new(AtomicU64::new(0));
        let clock = now.clone();
        let source = Arc::new(Router::new_with_clock(
            RouterConfig::new([]).with_sender("GS"),
            Box::new(move || clock.load(Ordering::Relaxed)),
        ));
        initialize_for_role(&source, false).unwrap();
        let clock = now.clone();
        let peer = Arc::new(Router::new_with_clock(
            RouterConfig::new([]).with_sender("DAQ"),
            Box::new(move || clock.load(Ordering::Relaxed)),
        ));
        for name in names {
            peer.enable_network_variable(
                crate::telemetry_schema::data_type(name),
                NetworkVariablePermissions::READ_ONLY,
            )
            .unwrap();
        }
        // Phase0 matches real bootstrap: sides exist, no RX worker/discovery yet.
        let phase = Arc::new(AtomicU8::new(0));
        let held = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let delivery_phase = phase.clone();
        let received = held.clone();
        let target = peer.clone();
        source.add_side_packed("rocket_comms", move |bytes| {
            match delivery_phase.load(Ordering::Relaxed) {
                0 => Ok(()),
                1 => target.rx_packed_from_side(bytes, 0),
                _ => {
                    received.lock().unwrap().push(bytes.to_vec());
                    Ok(())
                }
            }
        });
        source.add_side_packed("umbilical_comms", |_| Ok(()));
        let target = source.clone();
        peer.add_side_packed("GS", move |bytes| target.rx_packed_from_side(bytes, 0));
        assert_eq!(source.export_runtime_stats().sides.len(), 2);
        assert!(
            source
                .export_topology()
                .routes
                .iter()
                .all(|route| route.announcers.is_empty())
        );
        crate::qualification_requests::prime_initial_cache(&source, || {
            let expected = if std::env::var("GS_SIM_POLICY_CORRECT_CONTROLS").as_deref() == Ok("1")
                && std::env::var("GS_SIM_TRACE_API_REQUESTS").as_deref() == Ok("1")
            {
                Some(1)
            } else {
                None
            };
            assert_eq!(
                crate::qualification_requests::bootstrap_phase_for_test(),
                expected
            );
            assert_eq!(
                std::thread::spawn(crate::qualification_requests::bootstrap_phase_for_test)
                    .join()
                    .unwrap(),
                None,
                "other threads must never inherit bootstrap exception"
            );
            publish_current(&source)
        })
        .unwrap();
        assert_eq!(
            crate::qualification_requests::bootstrap_phase_for_test(),
            None,
            "phase must seal before any operational setter"
        );
        let bootstrap: Vec<_> = names.iter().filter_map(|name| {
            source
                .get_cached_network_variable(crate::telemetry_schema::data_type(name))
                .unwrap()
                .map(|packet| (*name, packet))
        }).collect();
        assert_eq!(bootstrap.len(), 8);
        assert!(source.get_cached_network_variable(
            crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE)).unwrap().is_none(),
            "unknown-origin default must not create a managed state owner");
        assert!(source.export_reliable_pending_stats().is_empty());
        println!(
            "LIFECYCLE_BOOT cached8, unknown-origin state absent, packedSides2, knownAnnouncers0, API success, pendingOwners0; no end-to-end ACK implied"
        );
        phase.store(1, Ordering::Relaxed);
        now.store(1000, Ordering::Relaxed);
        peer.announce_discovery().unwrap();
        source.announce_discovery().unwrap();
        for _ in 0..30 {
            source.process_all_queues().unwrap();
            peer.process_all_queues().unwrap();
        }
        for (name, original) in &bootstrap {
            let ty = crate::telemetry_schema::data_type(name);
            peer.request_managed_variable(ty).unwrap();
            for _ in 0..8 {
                source.process_all_queues().unwrap();
                peer.process_all_queues().unwrap();
            }
            let value = peer.get_cached_network_variable(ty).unwrap().unwrap();
            assert_eq!(
                value.payload(),
                original.payload(),
                "bootstrap latest value {name}"
            );
            assert_eq!(
                value.packet_id(),
                original.packet_id(),
                "bootstrap identity retained {name}"
            );
        }
        assert!(
            source
                .export_topology()
                .routes
                .iter()
                .any(|route| !route.announcers.is_empty())
        );
        println!(
            "LIFECYCLE_DISCOVERY all8 known-origin/config peer caches converge to original boot PacketIDs via discovery/managed refresh; separate from operational500ms"
        );
        peer.request_managed_variable(crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE)).unwrap();
        for _ in 0..8 { source.process_all_queues().unwrap(); peer.process_all_queues().unwrap(); }
        assert!(peer.get_cached_network_variable(
            crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE)).unwrap().is_none(),
            "GET cannot manufacture authority for unknown retained state");
        phase.store(2, Ordering::Relaxed);
        now.store(1100, Ordering::Relaxed);
        crate::telemetry_task::refresh_host_network_time(&source, get_current_timestamp_ms());
        set_flight_state(&source, 1).unwrap();
        let pending = source.export_reliable_pending_stats();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].data_type,
            crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE)
        );
        assert_eq!(pending[0].pending_destinations.len(), 1);
        println!(
            "LIFECYCLE_OPERATIONAL one new State1 ID{} has one frozen owner; accepted UART-buffer bytes alone are not an ACK",
            pending[0].packet_id
        );
        phase.store(1, Ordering::Relaxed);
        for bytes in held.lock().unwrap().drain(..) {
            peer.rx_packed_from_side(&bytes, 0).unwrap();
        }
        for _ in 0..8 {
            source.process_all_queues().unwrap();
            peer.process_all_queues().unwrap();
        }
        assert!(source.export_reliable_pending_stats().is_empty());
    }
    #[test]
    fn policy_qualified_actual_sdk_all_owner_receipts() {
        const NAME: &str = "network_variables::persistence_callback_tests::policy_qualified_actual_sdk_all_owner_receipts";
        if !isolated(NAME) {
            return;
        }
        use sedsnet::router::{EndpointHandler, RouterConfig};
        crate::telemetry_schema::initialize().unwrap();
        let clock = || Box::new(|| crate::simulation_clock_ms().unwrap_or(0));
        let source = Arc::new(Router::new_with_clock(
            RouterConfig::new([]).with_sender("GS"),
            clock(),
        ));
        let ty = crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE);
        for name in [FLIGHT_STATE_TYPE, UNDERGLOW_TYPE, FLIGHT_BUZZER_TYPE] {
            source
                .enable_network_variable(
                    crate::telemetry_schema::data_type(name),
                    NetworkVariablePermissions::READ_WRITE,
                )
                .unwrap();
        }
        let mut peers = Vec::new();
        for (side, name) in ["RF", "FC"].into_iter().enumerate() {
            let cosmetic = if name == "RF" {
                "AV_BAY_UNDERGLOW_OWNER"
            } else {
                "FLIGHT_CONTROLLER"
            };
            let handlers = [
                EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoint("FLIGHT_STATE"),
                    |_| Ok(()),
                ),
                EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoint(cosmetic),
                    |_| Ok(()),
                ),
            ];
            let peer = Arc::new(Router::new_with_clock(
                RouterConfig::new(handlers).with_sender(name),
                clock(),
            ));
            for kind in [FLIGHT_STATE_TYPE, UNDERGLOW_TYPE, FLIGHT_BUZZER_TYPE] {
                peer.enable_network_variable(
                    crate::telemetry_schema::data_type(kind),
                    NetworkVariablePermissions::READ_ONLY,
                )
                .unwrap();
            }
            let remote = peer.clone();
            source.add_side_packed(name, move |bytes| remote.rx_packed_from_side(bytes, 0));
            let primary = source.clone();
            peer.add_side_packed("GS", move |bytes| primary.rx_packed_from_side(bytes, side));
            peers.push(peer);
        }
        for p in &peers {
            p.announce_discovery().unwrap();
        }
        source.announce_discovery().unwrap();
        for _ in 0..8 {
            source.process_all_queues().unwrap();
            for p in &peers {
                p.process_all_queues().unwrap();
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        let states =
            crate::qualification_requests::policy_states(crate::runtime_mode::is_hitl(), &[1, 0, 1])
                .unwrap();
        crate::telemetry_task::refresh_host_network_time(&source, get_current_timestamp_ms());
        for (round, state) in states.into_iter().enumerate() {
            crate::qualification_requests::in_controlled_round(round + 1, || {
                set_flight_state(&source, state).unwrap();
                set_underglow(&source, round != 1).unwrap();
                set_flight_buzzer(&source, round != 1).unwrap();
            });
            ids.insert(
                source
                    .get_cached_network_variable(ty)
                    .unwrap()
                    .unwrap()
                    .packet_id(),
            );
            for _ in 0..8 {
                source.process_all_queues().unwrap();
                for p in &peers {
                    p.process_all_queues().unwrap();
                }
            }
            assert!(source.export_reliable_pending_stats().is_empty());
            for p in &peers {
                assert_eq!(
                    p.get_cached_network_variable(ty)
                        .unwrap()
                        .unwrap()
                        .payload(),
                    &[state]
                );
            }
        }
        assert_eq!(ids.len(), 3);
    }
    #[test]
    fn policy_qualified_real_setters_publish_distinct_stable_idle_and_cosmetics() {
        const NAME: &str = "network_variables::persistence_callback_tests::policy_qualified_real_setters_publish_distinct_stable_idle_and_cosmetics";
        if !isolated(NAME) {
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let router = Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]).with_sender("GS"),
            Box::new(|| crate::simulation_clock_ms().unwrap_or(0)),
        );
        initialize_for_role(&router, false).unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let output = sent.clone();
        router.add_side_packet("captured-owner", move |packet| {
            output.lock().unwrap().push(packet.clone());
            Ok(())
        });
        router.clear_queues();
        sent.lock().unwrap().clear();
        let states =
            crate::qualification_requests::policy_states(crate::runtime_mode::is_hitl(), &[1, 0, 1])
                .unwrap();
        crate::telemetry_task::refresh_host_network_time(&router, get_current_timestamp_ms());
        for (round, (state, enabled)) in states.iter().zip([true, false, true]).enumerate() {
            crate::qualification_requests::in_controlled_round(round + 1, || {
                set_flight_state(&router, *state).unwrap();
                set_underglow(&router, enabled).unwrap();
                set_flight_buzzer(&router, enabled).unwrap();
            });
        }
        let packets = sent.lock().unwrap();
        for (name, expected) in [
            (FLIGHT_STATE_TYPE, states),
            (UNDERGLOW_TYPE, vec![1, 0, 1]),
            (FLIGHT_BUZZER_TYPE, vec![1, 0, 1]),
        ] {
            let selected = packets
                .iter()
                .filter(|p| p.data_type() == crate::telemetry_schema::data_type(name))
                .collect::<Vec<_>>();
            assert_eq!(selected.len(), 3);
            assert_eq!(
                selected.iter().map(|p| p.payload()[0]).collect::<Vec<_>>(),
                expected
            );
            let ids = selected
                .iter()
                .map(|p| p.packet_id())
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                ids.len(),
                3,
                "unchanged stableIdle must never coalesce explicit control IDs"
            );
        }
    }
    #[test]
    fn repeated_sdk_flight_state_refreshes_keep_the_successfully_persisted_cache() {
        const CHILD: &str = "GS_NETWORK_VARIABLE_PERSISTENCE_PROOF_CHILD";
        const NAME: &str = "network_variables::persistence_callback_tests::repeated_sdk_flight_state_refreshes_keep_the_successfully_persisted_cache";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .env_remove("GS_NETWORK_VARIABLE_CACHE")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            println!("{}", String::from_utf8_lossy(&output.stdout));
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let router = Router::new(sedsnet::router::RouterConfig::new([]).with_sender("RRLS"));
        initialize_for_role(&router, true).unwrap();
        let callbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = callbacks.clone();
        router
            .on_network_variable_update(
                crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE),
                move |_| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .unwrap();
        router
            .rx(&packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", 1).unwrap())
            .unwrap();
        let path = store().lock().unwrap().path.clone();
        assert_eq!(load(&path).flight_state, 1);
        let mut rewrites = 0;
        let mut callback_us = Vec::new();
        for _ in 0..8 {
            let mut tagged = fs::read(&path).unwrap();
            tagged.extend_from_slice(b"     ");
            fs::write(&path, &tagged).unwrap(); // Valid JSON whitespace detects an application replacement.
            let now = std::time::Instant::now();
            router
                .rx(&packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", 1).unwrap())
                .unwrap();
            callback_us.push(now.elapsed().as_micros());
            rewrites += usize::from(fs::read(&path).unwrap() != tagged);
        }
        println!(
            "PERSISTENCE_PROOF {}",
            serde_json::json!({"callbacks":callbacks.load(Ordering::SeqCst),"same_state_refreshes":8,"redundant_replacements":rewrites,"callback_us":callback_us})
        );
        assert_eq!(
            callbacks.load(Ordering::SeqCst),
            9,
            "exercise actual unchanged SDK notifications"
        );
        assert_eq!(
            rewrites, 0,
            "successfully durable same-state refreshes must not rewrite the JSON cache"
        );
        // A same-value durable refresh must not even attempt the write path.
        // A directory at the exact temporary-file name deterministically rejects writes.
        let temporary = path.with_extension("json.tmp");
        fs::create_dir(&temporary).unwrap();
        router
            .rx(&packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", 1).unwrap())
            .unwrap();
        let failed = router.rx(&packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", 2).unwrap());
        assert!(
            failed.is_err(),
            "changed update must retain the existing SDK persistence failure"
        );
        assert_eq!(
            flight_state(),
            1,
            "failed rename/write must not publish an uncommitted in-memory value"
        );
        assert_eq!(load(&path).flight_state, 1);
        fs::remove_dir(&temporary).unwrap();
        router
            .rx(&packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", 2).unwrap())
            .unwrap();
        assert_eq!(
            flight_state(),
            2,
            "same incoming value must retry after IO recovers"
        );
        assert_eq!(load(&path).flight_state, 2);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn isolated(name: &str) -> bool {
        const CHILD: &str = "GS_VARIABLE_COMMIT_ISOLATED_TEST";
        if std::env::var(CHILD).ok().as_deref() == Some(name) {
            return true;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, name)
            .env_remove("GS_NETWORK_VARIABLE_CACHE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }

    #[test]
    fn admitted_launch_managed_callback_before_cas_is_idempotent_and_never_overwrites_newer_state()
    {
        const NAME: &str = "network_variables::persistence_callback_tests::admitted_launch_managed_callback_before_cas_is_idempotent_and_never_overwrites_newer_state";
        if !isolated(NAME) {
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let router = Router::new(
            sedsnet::router::RouterConfig::new([
                sedsnet::router::EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoint("FLIGHT_STATE"),
                    |_| Ok(()),
                ),
            ])
            .with_sender("GS"),
        );
        crate::telemetry_task::refresh_host_network_time(&router, get_current_timestamp_ms());
        initialize_for_role(&router, false).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        let prepared = prepare_launch_state(&router).unwrap();
        let baseline = automatic_idle_test_snapshot();
        let before = router.queue_counters()[2];
        router.tx_queue(prepared.packet.clone()).unwrap();
        assert_eq!(
            router.queue_counters()[2],
            before + 1,
            "original packet acquired an actual queue owner"
        );
        router.process_all_queues_with_timeout(0).unwrap();
        // Processing really consumed/cached the SAME original, but a primary
        // own-GS reflection is observational, never a second durable writer.
        let cached = router.get_cached_network_variable(crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE)).unwrap().unwrap();
        assert_eq!(cached.packet_id(), prepared.packet.packet_id());
        assert_eq!(cached.timestamp(), prepared.packet.timestamp());
        assert_eq!(automatic_idle_test_snapshot(), baseline, "own-source callback leaves exact Store and bytes unchanged");
        // queue_counters[2] is current depth, not a cumulative accepted count.
        let admitted = router.queue_counters()[2];
        commit_admitted_launch(&prepared).expect("explicit admitted local publisher owns durable Launch CAS");
        let committed = automatic_idle_test_snapshot();
        let observed = store().lock().unwrap().values;
        assert_eq!((observed.flight_state, observed.flight_state_stamp), (7, prepared.packet.timestamp()));
        commit_admitted_launch(&prepared).expect("SECOND identical admitted commit is idempotent");
        assert_eq!(automatic_idle_test_snapshot(), committed, "duplicate local CAS never rewrites or changes revision");
        assert_eq!(
            router.queue_counters()[2],
            admitted,
            "post-admission publication must never enqueue again"
        );
        {
            let mut store = store().lock().unwrap();
            let mut next = store.values;
            next.flight_state = 15;
            next.flight_state_stamp = prepared.packet.timestamp() + 1;
            store.commit(next).unwrap();
        }
        assert!(commit_admitted_launch(&prepared).is_err());
        assert_eq!(flight_state(), 15);
    }
    #[test]
    fn board_abort_retained_sdk_refresh_never_becomes_a_gs_owned_abort() {
        const NAME: &str = "network_variables::persistence_callback_tests::board_abort_retained_sdk_refresh_never_becomes_a_gs_owned_abort";
        if !isolated(NAME) {
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let source = Arc::new(Router::new(
            sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        ));
        initialize_for_role(&source, false).unwrap();
        let desired = flight_state();
        let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
        let packet = Packet::new(
            ty,
            &[crate::telemetry_schema::endpoint("FLIGHT_STATE")],
            "AB",
            9000,
            Arc::from([15u8]),
        )
        .unwrap();
        let original_id = packet.packet_id();
        source.rx(&packet).unwrap();
        assert_eq!(flight_state(), desired);
        let peer = Arc::new(Router::new(
            sedsnet::router::RouterConfig::new([]).with_sender("RF"),
        ));
        peer.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
            .unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let capture = received.clone();
        peer.on_network_variable_update(ty, move |p| {
            capture.lock().unwrap().push(p.clone());
            Ok(())
        })
        .unwrap();
        let received_peer = peer.clone();
        source.add_side_packet("to-rf", move |p| received_peer.rx_from_side(p, 0));
        let received_source = source.clone();
        peer.add_side_packet("to-gs", move |p| received_source.rx_from_side(p, 0));
        for _ in 0..4 {
            source.process_all_queues_with_timeout(0).unwrap();
            peer.process_all_queues_with_timeout(0).unwrap();
        }
        peer.invalidate_network_variable_cache(ty);
        received.lock().unwrap().clear();
        assert!(peer.get_network_variable(ty, None).unwrap().is_none());
        for _ in 0..4 {
            peer.process_all_queues_with_timeout(0).unwrap();
            source.process_all_queues_with_timeout(0).unwrap();
        }
        let refreshed = peer
            .get_cached_network_variable(ty)
            .unwrap()
            .expect("actual managed request/reply must return the retained source packet");
        assert_eq!(refreshed.sender(), "AB");
        assert_eq!(refreshed.timestamp(), 9000);
        assert_eq!(refreshed.packet_id(), original_id);
        assert_eq!(refreshed.payload(), &[15]);
        assert_eq!(flight_state(), desired);
        let own = flight_state_packet(store().lock().unwrap().values).unwrap();
        assert_eq!(own.sender(), "GS");
        assert_eq!(own.payload(), &[desired]);
        assert_ne!(own.payload(), &[15]);
    }

    #[test]
    fn first_unchanged_sdk_state_still_creates_a_missing_durable_cache() {
        const NAME: &str = "network_variables::persistence_callback_tests::first_unchanged_sdk_state_still_creates_a_missing_durable_cache";
        if !isolated(NAME) {
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let router = Router::new(sedsnet::router::RouterConfig::new([]).with_sender("RRLS"));
        initialize_for_role(&router, true).unwrap();
        let path = store().lock().unwrap().path.clone();
        assert!(!path.exists());
        let current = flight_state();
        assert!(!store().lock().unwrap().durable);
        router
            .rx(&packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", current).unwrap())
            .unwrap();
        assert!(path.exists());
        assert_eq!(load(&path).flight_state, current);
        assert!(store().lock().unwrap().durable);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn equal_durable_local_commands_still_publish_distinct_fresh_packets() {
        const NAME: &str = "network_variables::persistence_callback_tests::equal_durable_local_commands_still_publish_distinct_fresh_packets";
        if !isolated(NAME) {
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let router = Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
        crate::telemetry_task::refresh_host_network_time(&router, get_current_timestamp_ms());
        initialize_for_role(&router, false).unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let output = sent.clone();
        let ty = crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE);
        router.add_side_packet("owner", move |packet| {
            if packet.data_type() == ty {
                output.lock().unwrap().push(packet.clone());
            }
            Ok(())
        });
        router.clear_queues();
        sent.lock().unwrap().clear();
        set_flight_state(&router, 1).unwrap();
        set_flight_state(&router, 1).unwrap();
        let packets = sent.lock().unwrap();
        assert_eq!(
            packets.len(),
            2,
            "disk suppression must never coalesce network commands"
        );
        assert_ne!(packets[0].packet_id(), packets[1].packet_id());
        assert_eq!(packets[0].payload(), &[1]);
        assert_eq!(packets[1].payload(), &[1]);
        assert!(packets[1].timestamp() >= packets[0].timestamp());
    }

    #[test]
    fn retained_flight_state_keeps_origin_through_disk_reload_and_sdk_seed() {
        crate::telemetry_schema::initialize().unwrap();
        let path =
            std::env::temp_dir().join(format!("gs-state-origin-{}.json", rand::random::<u128>()));
        let values = PersistentVariables {
            flight_state: 15,
            flight_state_stamp: 1000,
            ..Default::default()
        };
        persist(&path, values).unwrap();
        let restored = VariableStore::new(path.clone());
        let router = Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
        crate::telemetry_task::refresh_host_network_time(&router, get_current_timestamp_ms());
        let retained = flight_state_packet(restored.values).unwrap();
        assert_eq!(retained.timestamp(), 1000);
        assert_eq!(retained.payload(), &[15]);
        router
            .enable_network_variable(
                crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE),
                NetworkVariablePermissions::READ_WRITE,
            )
            .unwrap();
        router.seed_managed_variable(retained).unwrap();
        let cached = router
            .get_cached_network_variable(crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE))
            .unwrap()
            .unwrap();
        assert_eq!(
            cached.timestamp(),
            1000,
            "reseed must remain older than a later recovery cutoff"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn legacy_flight_state_origin_is_unknown_instead_of_freshened() {
        crate::telemetry_schema::initialize().unwrap();
        let values: PersistentVariables = serde_json::from_str(r#"{"flight_state":15}"#).unwrap();
        assert_eq!(flight_state_packet(values).unwrap().timestamp(), 0);
    }

    #[test]
    fn actual_primary_bootstrap_preserves_recorded_state_origin() {
        const NAME: &str = "network_variables::persistence_callback_tests::actual_primary_bootstrap_preserves_recorded_state_origin";
        if !isolated(NAME) {
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let values =
            serde_json::from_str(r#"{"flight_state":15,"flight_state_stamp":1000}"#).unwrap();
        persist(&cache_path(), values).unwrap();
        let router = Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
        initialize_for_role(&router, false).unwrap();
        let cached = router
            .get_cached_network_variable(crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE))
            .unwrap()
            .unwrap();
        assert_eq!(cached.payload(), &[15]);
        assert_eq!(
            cached.timestamp(),
            1000,
            "boot must not mint fresh time for a retained Abort"
        );
    }

    #[test]
    fn unsynchronized_state_change_does_not_modify_retained_state() {
        const NAME: &str = "network_variables::persistence_callback_tests::unsynchronized_state_change_does_not_modify_retained_state";
        if !isolated(NAME) {
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let router = Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
        let before = store().lock().unwrap().values;
        assert!(set_flight_state(&router, 15).is_err());
        assert_eq!(store().lock().unwrap().values, before);
    }

    #[test]
    fn failed_store_commit_is_atomic_for_every_cached_field_and_retries() {
        let path = std::env::temp_dir()
            .join(format!(
                "variable-atomic-{}-{}",
                std::process::id(),
                rand::random::<u128>()
            ))
            .join("cache.json");
        let original = PersistentVariables {
            av_bay_underglow: true,
            flight_buzzer: true,
            flight_state: 4,
            flight_state_stamp: 100,
            daq_log_clock: [7, 123456],
            daq_log_clock_stamp: 0,
            daq_log_last_session: 7,
        };
        persist(&path, original).unwrap();
        let mut state = VariableStore::new(path.clone());
        assert!(state.durable);
        let replacement = PersistentVariables {
            av_bay_underglow: false,
            flight_buzzer: false,
            flight_state: 5,
            flight_state_stamp: 200,
            daq_log_clock: [8, 123789],
            daq_log_clock_stamp: 0,
            daq_log_last_session: 8,
        };
        let temporary = path.with_extension("json.tmp");
        fs::create_dir(&temporary).unwrap();
        assert!(state.commit(replacement).is_err());
        assert_eq!(state.values, original);
        assert!(state.durable);
        assert_eq!(load(&path), original);
        fs::remove_dir(&temporary).unwrap();
        state.commit(replacement).unwrap();
        assert_eq!(state.values, replacement);
        assert_eq!(load(&path), replacement);
        // Also fail after a successful temporary-file write, at rename itself.
        let saved = path.with_extension("saved");
        fs::rename(&path, &saved).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(state.commit(original).is_err());
        assert_eq!(state.values, replacement);
        assert_eq!(load(&saved), replacement);
        fs::remove_dir(&path).unwrap();
        fs::rename(&saved, &path).unwrap();
        state.commit(original).unwrap();
        assert_eq!(state.values, original);
        assert_eq!(load(&path), original);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}

/// Read a legacy durable pair without stamping or inferring its launch T0.
pub fn persisted_daq_log_clock() -> Option<[u64; 2]> {
    let guard = store()
        .lock()
        .expect("network-variable store lock poisoned");
    guard.durable.then_some(guard.values.daq_log_clock)
}

mod network_confirmation;

#[cfg(test)]
#[path = "network_variables/daq_clock_tests.rs"]
mod daq_clock_tests;

/// Retry the current durable metadata only with a live host admission lease.
/// Closing admission pauses retries without changing their original revision.
pub(crate) fn retry_pending_daq_log_clock_for_state(
    state: &crate::state::AppState,
    router: &Router,
) -> Result<bool> {
    let generation = crate::gse::automatic_generation(state);
    let Some(_claim) = crate::gse::claim_operator_dispatch(state, generation) else {
        return Ok(false);
    };
    retry_pending_daq_log_clock(router)
}
#[cfg(test)]
mod clock_admission_tests;

#[cfg(test)]
#[path = "network_variables/unknown_origin_tests.rs"]
mod unknown_origin_tests;

#[cfg(test)]
mod automatic_idle_tests;

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AutomaticIdleTestSnapshot {
    values: PersistentVariables,
    revision: u64,
    volatile: Option<AutomaticLiveIdentity>,
    bytes: Option<Vec<u8>>,
}
#[cfg(test)]
pub(crate) fn automatic_idle_test_snapshot() -> AutomaticIdleTestSnapshot {
    let guard = store().lock().unwrap();
    AutomaticIdleTestSnapshot { values: guard.values, revision: guard.revision,
        volatile: guard.automatic_idle, bytes: fs::read(&guard.path).ok() }
}

#[cfg(test)]
impl AutomaticIdleTestSnapshot {
    #[cfg(not(feature = "firmware_sim"))]
    pub(crate) fn live_phase_identity(&self) -> Option<AutomaticPhaseIdentity> {
        self.volatile.map(|live| AutomaticPhaseIdentity { original: live.original(), phase: live.phase() })
    }
    pub(crate) fn durable_unchanged(&self, previous: &Self) -> bool {
        self.values == previous.values && self.bytes == previous.bytes
    }
}

#[cfg(test)]
mod republish_backoff_tests {
    use super::*;
    /// Faithful bench run (S5=10, downlink lost): AV_BAY_UNDERGLOW and
    /// FLIGHT_BUZZER republishes exhausted their ordered retries every
    /// 10-15 s (90 frames each in 75 s) with the FC unreachable. Automatic
    /// republishes now back off 10 -> 20 -> 30 s; an operator request
    /// publishes at once and starts over.
    #[test]
    fn automatic_republish_backs_off_while_unconfirmed_and_resets_on_a_fresh_request() {
        let second = std::time::Duration::from_secs(1);
        let mut t = std::time::Instant::now();
        let mut backoff = RepublishBackoff::default();
        assert!(backoff.due(t), "the first republish is not held");
        backoff.republished(t);
        for wait in [10u32, 20, 30, 30] {
            assert!(!backoff.due(t + second * wait - std::time::Duration::from_millis(1)), "{wait}");
            t += second * wait;
            assert!(backoff.due(t), "{wait}");
            backoff.republished(t);
        }
        backoff.reset();
        assert!(backoff.due(t), "a fresh operator request or a confirmation starts over");
    }
}
