mod abort_input;
mod abort_retirement;
mod admin;
mod daq_launch_log;
mod daq_sd;
mod radio_voice;
mod recording_replay;
// main.rs

macro_rules! gs_debug_println {
    ($($arg:tt)*) => {
        if crate::debug_prints_enabled() {
            std::println!($($arg)*);
        }
    };
}

mod actuator_plumbing;
mod alert_condition_journal;
mod alert_condition_producers;
mod alert_condition_runtime;
mod alert_conditions;
mod alert_history;
mod antenna;
mod auth;
mod auto_zero;
mod board_abort;
mod board_abort_history;
mod final_proof_clock;
mod history_startup_path;
mod board_abort_history_writer;
mod board_recovery;
mod igniter_sense;
mod ignition_pair;
mod checklists;
mod client_settings;
mod command_probe;
mod command_proxy;
mod comms;
mod comms_config;
mod control_ingress;
mod core_busy;
mod deferred_abort;
mod dummy_packets;
mod ethernet_autoconfig;
mod fill_delivery_probe;
mod fill_recording;
mod fill_targets;
mod firmware_update;
mod flight_setup;
mod flight_sim;
mod flight_sim_runtime;
mod flight_maxima;
mod gpio;
mod gpio_panel;
mod gse;
mod history_policy;
mod host_temperature;
mod i18n;
mod launch_analytics;
mod launch_clock_sync;
mod launch_history;
mod launch_mode_gate;
mod layout;
mod kg50_input_sync;
mod loadcell;
mod loadcell_zero;
mod log_redaction;
mod logger;
mod manual_abort_intent;
mod manual_abort_resend;
mod manual_abort_utc;
mod map;
mod media;
mod media_runtime;
mod mission_phase;
mod valve_report_health;
mod mission_sync;
mod network_health;
mod network_traffic;
mod network_variables;
mod operator_queue;
mod operator_command_resend;
mod operator_phase;
mod primary_mirror;
mod qualification_requests;
mod recording_export;
mod recovery_events;
mod recording_range;
mod recording_report;
mod recording_sd;
mod sd_download;
mod ring_buffer;
mod rocket_commands;
mod rrls;
mod runtime_mode;
mod rrls_radio;
mod rrls_sim_primary;
mod rrls_topology;
mod safety_task;
mod sequence_progress;
mod sequence_progress_producer;
mod sequence_progress_projection;
mod sequence_progress_service;
mod sequence_progress_stage;
mod sequence_stream;
mod safety_catalog;
mod sequences;
mod sim_daq_cadence;
mod sim_freshness;
mod state;
mod station_mode;
mod station_mode_switch;
mod station_pairing;
mod system_clock;
mod telemetry_db;
mod telemetry_provenance;
mod telemetry_schema;
mod telemetry_task;
mod test_fire_csv;
mod testing_board_lifecycle;
mod thermal_fit;
mod thermal_settling;
mod types;
mod voice;
mod web;
mod web_server;

use crate::map::{ensure_map_data, selected_map_region};
use crate::ring_buffer::RingBuffer;
use crate::rocket_commands::{FlightComputerCommands, ValveBoardCommands};
use crate::safety_task::safety_supervisor;
use crate::sequences::{default_action_policy, start_sequence_task};
use crate::state::{AppState, BoardStatus};
use crate::telemetry_db::{
    DEFAULT_TELEMETRY_DB_FILENAME, DbQueueItem, RecordingModeWire, RecordingStatusMsg,
    apply_sqlite_pragmas, close_and_finalize_sqlite, delete_sqlite_if_empty, ensure_sqlite_db_file,
    open_in_memory_telemetry_db, recover_sqlite_sidecars_in_dir,
};
use crate::telemetry_task::{
    CommsWorkerHandle, TxQueueBudget, flush_command_tx, get_current_timestamp_ms,
    queue_locally_routed_flight_command, set_network_time_router, telemetry_task,
};

use crate::comms::DummyComms;
use crate::comms::{CommsDevice, link_description, open_link, startup_failure_hint};
use crate::comms_config::CommsLinkConfig;
use crate::types::{Board, FlightState as FlightStateMode};
use axum::Router;
use sedsnet::TelemetryError;
use sedsnet::packet::Packet;
use sedsnet::router::{EndpointHandler, RouterSideOptions};
use sedsnet::timesync::{TimeSyncConfig, TimeSyncRole};
use sqlx::Row;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::time::{Duration, Instant};

use crate::web::AlertAckStateMsg;
use tokio::sync::{broadcast, mpsc};

fn env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn network_delivery_tuning() -> sedsnet::config::RuntimeTuningConfig {
    // Radio schema frames are bounded to 256 bytes. At the configured
    // 40% share of a 64 kbps link, one framed chunk takes under 90 ms.
    // Retry at 250 ms so one lost receipt can recover inside a 500 ms target;
    // 18 attempts retain the previous 4.5-second total delivery window.
    sedsnet::config::RuntimeTuningConfig {
        // Embedded peers omit the zstd decoder. Header templates still reduce
        // link overhead, but canonical payloads must remain decodable there.
        payload_compress_threshold: usize::MAX,
        reliable_retransmit_ms: env_usize("GS_RELIABLE_RETRANSMIT_MS", 250, 100, 5000) as u32,
        reliable_max_retries: 17,
        // The end-to-end reliable window is ONE pool shared by every side.
        // At the SDK default (32), commands to slow or unreachable radio
        // boards, each held for the 4.5 s delivery window, filled it and
        // starved the wired umbilical boards: their commands and recovery
        // queries waited for slots and answered only in bursts, every 4–8 s.
        // A larger pool keeps room for the umbilical while the radio backs
        // up; each entry stays bounded by the network memory budget below.
        reliable_max_end_to_end_pending: env_usize("GS_RELIABLE_MAX_E2E_PENDING", 1024, 32, 8192),
        ..Default::default()
    }
}

fn network_memory_config() -> sedsnet::config::RuntimeMemoryConfig {
    // This is a ceiling, not a preallocation. Keep physical transport queues
    // short so a stalled radio cannot turn extra host RAM into command latency.
    let budget = env_usize(
        "GS_NETWORK_MEMORY_BUDGET_BYTES",
        2 * 1024 * 1024 * 1024,
        1024 * 1024,
        2 * 1024 * 1024 * 1024,
    );
    sedsnet::config::RuntimeMemoryConfig::new(budget, 4096, 4096, 1.25)
        .expect("bounded host network memory configuration")
}

const RADIO_SIDE_MAX_FRAME_BYTES: usize = 192;

fn network_router_time_divisor() -> u64 {
    std::env::var("GS_SIM_ROUTER_TIME_DIVISOR")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1)
}

static SIMULATION_CLOCK_PATH: std::sync::OnceLock<Option<std::path::PathBuf>> =
    std::sync::OnceLock::new();
static SIMULATION_CLOCK_LAST: AtomicU64 = AtomicU64::new(0);
static SIMULATION_CLOCK_VALID: AtomicBool = AtomicBool::new(false);
static SIMULATION_CLOCK_SAMPLE: AtomicU64 = AtomicU64::new(0);
// A configured model source is one coherent observation, including its existing
// SDK clamp and diagnostic publication. Physical/no-file clocks never take this.
static SIMULATION_CLOCK_OBSERVATION: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// No filesystem access: used only after the existing SDK clock sampled its source.
fn cached_simulation_clock_sample() -> Option<(Option<u64>, u64)> {
    match SIMULATION_CLOCK_PATH.get()? {
        None => Some((None, 0)),
        Some(_) if SIMULATION_CLOCK_VALID.load(Ordering::Acquire) => Some((
            Some(SIMULATION_CLOCK_LAST.load(Ordering::Acquire)),
            SIMULATION_CLOCK_SAMPLE.load(Ordering::Acquire),
        )),
        Some(_) => None,
    }
}
fn simulation_clock_ms() -> Option<u64> {
    let path = SIMULATION_CLOCK_PATH
        .get_or_init(|| {
            // RUNTIME-MODE-BOOT: the clock source is chosen once, at the first
            // sample (during start), from the boot mode. A station clock must not
            // change source in process; GS_SIM_CLOCK_FILE is set only by the
            // simulator qualification harness, which never switches modes.
            if crate::runtime_mode::is_hitl() || cfg!(feature = "firmware_sim") {
                std::env::var_os("GS_SIM_CLOCK_FILE").map(std::path::PathBuf::from)
            } else {
                None
            }
        })
        .as_ref()?;
    // Serialize only an explicitly configured simulator source. An older parsed
    // value must not race a newer fetch_max and impersonate source rollback.
    // A path is configured only in HITL or simulator builds (see above).
    let (_source_guard, source_poisoned) = match SIMULATION_CLOCK_OBSERVATION.lock() {
        Ok(guard) => (guard, false),
        Err(error) => (error.into_inner(), true),
    };
    // The guard remains held through the complete source/clamp/cache/finish
    // observation. Poison never clears: return the existing clamp but invalidate
    // this observation, preserving the original fail-closed recovery fence.
    let final_observation = final_proof_clock::begin_model_observation();
    let (value, valid, raw) = if source_poisoned {
        (SIMULATION_CLOCK_LAST.load(Ordering::Relaxed), false, None)
    } else {
        read_simulation_clock_details(path, &SIMULATION_CLOCK_LAST)
    };
    SIMULATION_CLOCK_VALID.store(valid, Ordering::Release);
    let _ = SIMULATION_CLOCK_SAMPLE.try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        value.checked_add(1)
    });
    final_observation.finish(value, raw);
    Some(value)
}

#[cfg(test)]
fn read_simulation_clock(path: &std::path::Path, last: &std::sync::atomic::AtomicU64) -> u64 {
    read_simulation_clock_observed(path, last).0
}
#[cfg(test)]
fn read_simulation_clock_observed(
    path: &std::path::Path,
    last: &std::sync::atomic::AtomicU64,
) -> (u64, bool) {
    let (value, valid, _) = read_simulation_clock_details(path, last);
    (value, valid)
}
fn read_simulation_clock_details(
    path: &std::path::Path,
    last: &std::sync::atomic::AtomicU64,
) -> (u64, bool, Option<u64>) {
    #[cfg(test)]
    if crate::runtime_mode::is_hitl() || cfg!(feature = "firmware_sim") {
        final_proof_clock::tests::before_source_read(path);
    }
    // Preserve the SDK clock's existing read/parse/monotonic clamp exactly.
    // Raw input is retained ONLY for final recovery-proof eligibility.
    if let Ok(text) = std::fs::read_to_string(path)
        && let Ok(value) = text.trim().parse::<u64>() {
            last.fetch_max(value, Ordering::Relaxed);
            return (last.load(Ordering::Relaxed), true, Some(value));
        }
    (last.load(Ordering::Relaxed), false, None)
}

#[derive(Clone, Copy)]
struct ValidationInstant {
    wall: Instant,
    simulated_ms: Option<u64>,
}

impl ValidationInstant {
    fn now() -> Self {
        Self {
            wall: Instant::now(),
            simulated_ms: simulation_clock_ms(),
        }
    }
}

fn validation_elapsed_ms(started: ValidationInstant) -> u64 {
    if let (Some(start), Some(now)) = (started.simulated_ms, simulation_clock_ms()) {
        return now.saturating_sub(start);
    }
    u64::try_from(started.wall.elapsed().as_millis()).unwrap_or(u64::MAX)
        / network_router_time_divisor()
}

fn validation_latency_limit_ms(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

async fn wait_for_validation_idle(router: &sedsnet::router::Router) -> bool {
    let started = ValidationInstant::now();
    loop {
        let stats = router.export_runtime_stats();
        if stats.reliable.end_to_end_pending_destination_count == 0 {
            log::info!(
                "full-bay prior reliable traffic drained before managed-variable measurement: {} ms",
                validation_elapsed_ms(started)
            );
            return true;
        }
        if validation_elapsed_ms(started) >= 30_000 {
            log::error!(
                "full-bay prior reliable traffic did not drain: tx={}, pending={}",
                stats.queues.tx_len,
                stats.reliable.end_to_end_pending_destination_count
            );
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_validation_reliable_delivery(
    router: &sedsnet::router::Router,
    label: &str,
    tx_before: &[(sedsnet::config::DataType, u64)],
) -> bool {
    let started = ValidationInstant::now();
    let latency_limit_ms =
        validation_latency_limit_ms("GS_SIM_MANAGED_VARIABLE_MAX_LATENCY_MS", 2_500);
    let timeout_ms = std::env::var("GS_SIM_RELIABLE_ACK_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30_000);
    let minimum_observation_ms = std::env::var("GS_SIM_MANAGED_VARIABLE_SETTLE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1_000);
    // A 100 ms wall poll can report a sub-500 ms model receipt after its
    // deadline. Sample more closely in simulation; keep the source clock,
    // minimum observation period and delivery limit unchanged.
    let poll_interval = if started.simulated_ms.is_some() {
        Duration::from_millis(10)
    } else {
        Duration::from_millis(100)
    };
    let mut delivery_ms = None;
    let mut last_pending = Vec::new();
    loop {
        let stats = router.export_runtime_stats();
        let details = router.export_reliable_pending_stats();
        let pending: usize = details
            .iter()
            .filter(|entry| tx_before.iter().any(|(ty, _)| *ty == entry.data_type))
            .map(|entry| entry.pending_destinations.len())
            .sum();
        if std::env::var_os("GS_SIM_TRACE_RELIABLE").is_some()
            && details != last_pending {
                log::info!("full-bay reliable pending for {label}: {details:?}");
                last_pending.clone_from(&details);
            }
        let transmission_counts = tx_before
            .iter()
            .map(|(expected, before)| {
                let now = stats
                    .sides
                    .iter()
                    .flat_map(|side| side.data_types.iter())
                    .filter(|kind| kind.data_type == *expected)
                    .map(|kind| kind.tx_packets)
                    .sum::<u64>();
                (*expected, *before, now)
            })
            .collect::<Vec<_>>();
        let elapsed_ms = validation_elapsed_ms(started);
        // Test the round's actual transmissions and destination ACKs. A busy
        // telemetry/schema queue is not a missing ACK, and need not be empty
        // for controls to complete under continuous network load.
        let delivered = pending == 0
            && transmission_counts
                .iter()
                .all(|(_, before, after)| after > before);
        if delivered {
            delivery_ms.get_or_insert(elapsed_ms);
        }
        // An empty pending table is not proof of an ACK: retry exhaustion
        // removes entries too. Fail at the delivery deadline while the
        // outstanding destinations are still observable, before retry expiry
        // can be mistaken for successful completion.
        if delivery_ms.is_none() && elapsed_ms > latency_limit_ms {
            log::error!(
                "full-bay reliable delivery missed latency deadline for {label}: {elapsed_ms} ms > {latency_limit_ms} ms, {pending} destination ACK(s) pending, per-type tx before/after={transmission_counts:?}, pending deliveries={details:?}"
            );
            return false;
        }
        // Completion is an event: once all transmissions and their ACKs have
        // finished, later heartbeats/schema work cannot undo that event or
        // turn its latency into the length of an unrelated quiet interval.
        if elapsed_ms >= minimum_observation_ms
            && let Some(delivery_ms) = delivery_ms {
            if delivery_ms > latency_limit_ms {
                log::error!(
                    "full-bay reliable delivery exceeded latency bound for {label}: {delivery_ms} ms > {latency_limit_ms} ms"
                );
                return false;
            }
            log::info!(
                "full-bay managed-variable latency within bound: {delivery_ms} ms <= {latency_limit_ms} ms ({label}); observed for {elapsed_ms} ms; first completion recorded; per-type tx before/after={transmission_counts:?}"
            );
            return true;
        }
        if validation_elapsed_ms(started) >= timeout_ms {
            log::error!(
                "full-bay reliable delivery timed out for {label}: {pending} destination ACK(s) pending, tx queue depth {}, per-type tx before/after={transmission_counts:?}",
                stats.queues.tx_len,
            );
            return false;
        }
        tokio::time::sleep(poll_interval).await;
    }
}

pub(crate) fn debug_prints_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GS_DEBUG_PRINTS")
            .ok()
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

pub(crate) fn radio_diagnostics_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GS_RADIO_DIAGNOSTICS")
            .ok()
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

pub(crate) fn ws_diagnostics_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GS_WS_DIAGNOSTICS")
            .ok()
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

fn router_hop_reliable_enabled(link: &CommsLinkConfig) -> bool {
    match link {
        // Pico-Fi's polled I2C bridge owns delivery and cannot sustain
        // SEDSNet's bidirectional hop-ACK traffic without starving Gateway.
        CommsLinkConfig::I2c { .. } | CommsLinkConfig::PicoFiTcp { .. } => false,
        // RF disables hop sequencing on its RFD900x side. Match that policy:
        // enabling it only here leaves unacknowledged frames in hop history.
        // Router end-to-end command delivery remains enabled independently.
        CommsLinkConfig::Serial { serial }
        | CommsLinkConfig::RaspberryPiGpioUart { serial }
        | CommsLinkConfig::CustomSerial { serial } => {
            serial.protocol != crate::comms_config::SerialProtocol::RawUart
        }
        CommsLinkConfig::Spi { .. } | CommsLinkConfig::Can { .. } => true,
    }
}

/// Discovery schema snapshots and schema requests never go out on the radio.
/// Every AV-bay board's compiled schema is a subset of the station's
/// (`av_bay_schema_contract` keeps that a test), and the SDK keeps a compiled
/// definition over any received one, so an exchange over the radio teaches
/// neither end anything. At 192 B frames it was a 26-chunk transfer each way
/// that rarely completed under radio loss and was then requested again, about
/// 1.4 kB/s of air. The radio side carries both the direct AV-bay link and
/// the RRLS relay (which forwards the station's own frames), so this one deny
/// covers both. Schema still flows on every wired side.
pub(crate) fn keep_schema_exchange_off_the_radio(
    router: &sedsnet::router::Router,
    radio: sedsnet::router::RouterSideId,
    wired: &[sedsnet::router::RouterSideId],
) -> sedsnet::TelemetryResult<()> {
    let sources = [None, Some(radio)].into_iter().chain(wired.iter().copied().map(Some));
    for src in sources {
        for ty in [sedsnet::config::DataType::DiscoverySchema, sedsnet::config::DataType::DiscoverySchemaRequest] {
            router.set_typed_route(src, ty, radio, false)?;
            // A typed override for (src, ty) makes every unlisted destination
            // closed, so the wired sides are opened explicitly.
            for &side in wired.iter().filter(|side| src != Some(**side)) {
                router.set_typed_route(src, ty, side, true)?;
            }
        }
    }
    Ok(())
}

fn rocket_router_side_options(link: &CommsLinkConfig, observer: bool) -> RouterSideOptions {
    RouterSideOptions {
        reliable_enabled: router_hop_reliable_enabled(link),
        // RRLS forwards original frames through Mesh, not its local SDK.
        // Suppress local discovery/managed queries before they enter TX queues.
        egress_enabled: !observer,
        ..Default::default()
    }
    .with_small_packet_transport(RADIO_SIDE_MAX_FRAME_BYTES)
}

struct ValvePowerFreshness {
    clock: ValidationInstant,
    last_source: [Option<u64>; 2],
    last_received: [Option<u64>; 2],
    stale_reported: [bool; 2],
}
impl Default for ValvePowerFreshness {
    fn default() -> Self {
        Self {
            clock: ValidationInstant::now(),
            last_source: [None; 2],
            last_received: [None; 2],
            stale_reported: [false; 2],
        }
    }
}
impl ValvePowerFreshness {
    fn observe(&mut self, index: usize, source: u64, now: u64) -> Option<u64> {
        if self.last_source[index] == Some(source) {
            return None;
        }
        let gap = self.check(index, now);
        self.last_source[index] = Some(source);
        self.last_received[index] = Some(now);
        self.stale_reported[index] = false;
        gap
    }
    fn check(&mut self, index: usize, now: u64) -> Option<u64> {
        let gap = now.saturating_sub(self.last_received[index]?);
        if gap > 3_000 && !self.stale_reported[index] {
            self.stale_reported[index] = true;
            Some(gap)
        } else {
            None
        }
    }
}

#[derive(Default)]
struct StreamRateProbe {
    // Source cadence is independent of unordered transport arrival order.
    // Keep only the bounded window required by the existing cadence gates.
    timestamps: [u64; 6],
    count: usize,
}

impl StreamRateProbe {
    fn observe(
        &mut self,
        timestamp_ms: u64,
        minimum_interval_ms: u64,
        maximum_interval_ms: u64,
        required_intervals: u32,
    ) -> bool {
        let needed = required_intervals as usize + 1;
        if needed < 2 || needed > self.timestamps.len() {
            return false;
        }
        if self.count > needed {
            self.timestamps
                .copy_within(self.count - needed..self.count, 0);
            self.count = needed;
        }
        if self.timestamps[..self.count].contains(&timestamp_ms) {
            return false;
        }
        if self.count == needed {
            if timestamp_ms < self.timestamps[0] {
                return false;
            }
            self.timestamps.copy_within(1..needed, 0);
            self.count -= 1;
        }
        let position = self.timestamps[..self.count].partition_point(|&t| t < timestamp_ms);
        self.timestamps
            .copy_within(position..self.count, position + 1);
        self.timestamps[position] = timestamp_ms;
        self.count += 1;
        self.count == needed
            && self.timestamps[..needed].windows(2).all(|pair| {
                (minimum_interval_ms..=maximum_interval_ms).contains(&(pair[1] - pair[0]))
            })
    }
}

/// Source-cadence expectation for the simulator observer only. This never
/// changes acquisition/publication rates or transport/receipt/freshness gates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DaqLoadcellCadence {
    hz: u32,
    minimum_interval_ms: u64,
    maximum_interval_ms: u64,
}

impl DaqLoadcellCadence {
    const ENV: &'static str = "GS_SIM_DAQ_LOADCELL_HZ";

    fn parse(raw: Option<&str>) -> anyhow::Result<Self> {
        let hz = match raw {
            None => 250,
            Some(value) => value.parse::<u32>().map_err(|_| {
                anyhow::anyhow!("{} must be an integer from 1 through 1000", Self::ENV)
            })?,
        };
        anyhow::ensure!(
            (1..=1000).contains(&hz),
            "{} must be from 1 through 1000",
            Self::ENV
        );
        // The source timestamp is an integer number of milliseconds. Retain
        // the original +/-50% period bounds (250Hz =>2..6ms), quantized down.
        Ok(Self {
            hz,
            minimum_interval_ms: 500 / u64::from(hz),
            maximum_interval_ms: 1500 / u64::from(hz),
        })
    }

    fn from_env() -> anyhow::Result<Self> {
        let raw = match std::env::var(Self::ENV) {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(anyhow::anyhow!("{}: {error}", Self::ENV)),
        };
        Self::parse(raw.as_deref())
    }
}

// Both DAQ wire formats are observed only inside the explicit
// GS_SIM_VALIDATE_TELEMETRY_RETURN block. Packet source time stays original.
fn validation_loadcell_source_timestamp(
    board: Option<Board>,
    pkt: &sedsnet::packet::Packet,
) -> Option<u64> {
    sim_daq_cadence::captured_loadcell(board, pkt)
        .map(|(_, source_timestamp)| source_timestamp)
}

fn request_startup_topology(router: &sedsnet::router::Router) -> sedsnet::TelemetryResult<()> {
    router.request_topology()
}

/// The SDK's runtime tuning is process-wide. Tests that change it hold this
/// lock, so a model that depends on retry timing never sees another test's
/// tuning mid-run.
#[cfg(test)]
pub(crate) static RUNTIME_TUNING_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Installs the station's delivery tuning (retry span 250 ms x 18) for one
/// test and restores the previous tuning when dropped.
#[cfg(test)]
pub(crate) struct StationDeliveryTuning {
    previous: sedsnet::config::RuntimeTuningConfig,
    _serial: std::sync::MutexGuard<'static, ()>,
}
#[cfg(test)]
pub(crate) fn network_delivery_tuning_for_test() -> StationDeliveryTuning {
    let serial = RUNTIME_TUNING_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = sedsnet::config::runtime_tuning_config();
    sedsnet::config::set_runtime_tuning_config(network_delivery_tuning()).unwrap();
    StationDeliveryTuning { previous, _serial: serial }
}
#[cfg(test)]
impl Drop for StationDeliveryTuning {
    fn drop(&mut self) {
        sedsnet::config::set_runtime_tuning_config(self.previous).unwrap();
    }
}

#[cfg(test)]
mod router_link_policy_tests {
    #[test]
    fn embedded_network_packets_do_not_require_a_zstd_decoder() {
        let _tuning = crate::RUNTIME_TUNING_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = sedsnet::config::runtime_tuning_config();
        sedsnet::config::set_runtime_tuning_config(super::network_delivery_tuning()).unwrap();
        let payload: std::sync::Arc<[u8]> = vec![0; 4096].into();
        let packet = sedsnet::packet::Packet::new(
            sedsnet::config::DataType::DiscoverySchema,
            &[sedsnet::config::DataEndpoint::Discovery],
            "GS",
            1,
            payload.clone(),
        )
        .unwrap();
        let wire = sedsnet::wire_format::pack_packet(&packet);
        sedsnet::config::set_runtime_tuning_config(previous).unwrap();
        assert_eq!(
            wire[0] & 0x01,
            0,
            "wire packet requires an embedded zstd decoder"
        );
        assert_eq!(
            sedsnet::wire_format::unpack_packet(&wire)
                .unwrap()
                .payload(),
            payload.as_ref()
        );
    }

    /// Discovery types (the test above) are never compressed whatever the
    /// threshold, so that test cannot catch a station that compresses board
    /// traffic. Every board builds SEDSnet with zstd and so advertises the
    /// payload-compression link capability, yet none has the heap to run it
    /// (decompression needs a ~96 KiB context; the largest board heap is
    /// ~14 KiB). A compressed command is not decoded, not acknowledged and is
    /// retransmitted until retries run out. Send an ordinary, compressible
    /// board-bound command over the station's radio side options to a
    /// codec-capable peer and require plain payloads on the air.
    #[test]
    fn board_bound_commands_stay_plain_to_a_codec_capable_board() {
        use std::collections::VecDeque;
        let _tuning = crate::RUNTIME_TUNING_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = sedsnet::config::runtime_tuning_config();
        sedsnet::config::set_runtime_tuning_config(super::network_delivery_tuning()).unwrap();

        let ty = super::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST");
        let endpoint = super::telemetry_schema::endpoint("BOARD_RECOVERY_RF");
        let delivered = Arc::new(AtomicUsize::new(0));
        let seen = delivered.clone();
        let now = Arc::new(AtomicU64::new(1_000));
        let (gs_clock, rf_clock) = (now.clone(), now.clone());
        let to_rf: Arc<Mutex<VecDeque<Vec<u8>>>> = Default::default();
        let to_gs: Arc<Mutex<VecDeque<Vec<u8>>>> = Default::default();
        let gs = sedsnet::router::Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]).with_sender("GS"),
            Box::new(move || gs_clock.load(Ordering::Relaxed)),
        );
        let rf = sedsnet::router::Router::new_with_clock(
            sedsnet::router::RouterConfig::new([EndpointHandler::new_packet_handler(
                endpoint,
                move |_packet: &Packet| {
                    seen.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                },
            )])
            .with_sender("RF"),
            Box::new(move || rf_clock.load(Ordering::Relaxed)),
        );
        let link = || {
            RouterSideOptions {
                reliable_enabled: false,
                ..Default::default()
            }
            .with_small_packet_transport(RADIO_SIDE_MAX_FRAME_BYTES)
        };
        let queue = to_rf.clone();
        let gs_side = gs.add_side_packed_with_options(
            "radio",
            move |frame: &[u8]| {
                queue.lock().unwrap().push_back(frame.to_vec());
                Ok(())
            },
            link(),
        );
        let queue = to_gs.clone();
        let rf_side = rf.add_side_packed_with_options(
            "radio",
            move |frame: &[u8]| {
                queue.lock().unwrap().push_back(frame.to_vec());
                Ok(())
            },
            link(),
        );
        let mut command_frames = Vec::new();
        let mut pump = |watch: bool| {
            for _ in 0..8 {
                let frames: Vec<_> = to_rf.lock().unwrap().drain(..).collect();
                for frame in frames {
                    if watch
                        && sedsnet::wire_format::peek_frame_info(&frame)
                            .is_ok_and(|info| info.envelope.ty == ty)
                    {
                        command_frames.push(frame.clone());
                    }
                    let _ = rf.rx_packed_from_side(&frame, rf_side);
                    let _ = rf.process_all_queues();
                }
                let frames: Vec<_> = to_gs.lock().unwrap().drain(..).collect();
                for frame in frames {
                    let _ = gs.rx_packed_from_side(&frame, gs_side);
                    let _ = gs.process_all_queues();
                }
                let _ = gs.periodic(0);
                let _ = rf.periodic(0);
            }
        };
        // Both learn each other's link capabilities, zstd included.
        for _ in 0..40 {
            let _ = gs.announce_discovery();
            let _ = rf.announce_discovery();
            pump(false);
            now.fetch_add(50, Ordering::Relaxed);
        }
        // 32 B, all zero: zstd shrinks it, so only the station's policy keeps it plain.
        let packet = Packet::new(ty, &[endpoint], "GS", now.load(Ordering::Relaxed), vec![0u8; 32].into())
            .unwrap();
        gs.tx(packet).unwrap();
        for _ in 0..20 {
            pump(true);
            now.fetch_add(50, Ordering::Relaxed);
        }
        sedsnet::config::set_runtime_tuning_config(previous).unwrap();

        assert!(!command_frames.is_empty(), "the command never reached the radio side");
        for frame in &command_frames {
            assert_eq!(frame[0] & 0x01, 0, "board-bound command was zstd-compressed on the air");
        }
        assert_eq!(command_frames.len(), 1, "a plain command is delivered and acknowledged once");
        assert_eq!(delivered.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn no_explicit_clock_preserves_wall_runtime_in_an_isolated_process() {
        const NAME: &str = "router_link_policy_tests::no_explicit_clock_preserves_wall_runtime_in_an_isolated_process";
        if std::env::var_os("GS_NO_MODEL_CLOCK_CHILD").is_some() {
            assert_eq!(super::simulation_clock_ms(), None);
            let before = super::telemetry_task::get_current_timestamp_ms();
            let purpose = super::telemetry_task::procedure_clock::now();
            let after = super::telemetry_task::get_current_timestamp_ms();
            assert!(purpose.utc_ms() >= before && purpose.utc_ms() <= after);
            assert_eq!(purpose.model_monotonic_ms(), None);
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--test-threads=1"])
            .env("GS_NO_MODEL_CLOCK_CHILD", "1")
            .env_remove("GS_SIM_CLOCK_FILE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn simulation_clock_survives_partial_writes_and_board_reset() {
        let path = std::env::temp_dir().join(format!("gs-clock-test-{}", std::process::id()));
        let last = std::sync::atomic::AtomicU64::new(0);
        for (contents, expected) in [
            ("1000", 1000),
            ("", 1000),
            ("10", 1000),
            ("invalid", 1000),
            ("1010\n", 1010),
            ("0", 1010),
            ("1020", 1020),
        ] {
            std::fs::write(&path, contents).unwrap();
            assert_eq!(super::read_simulation_clock(&path, &last), expected);
        }
        std::fs::remove_file(&path).unwrap();
        assert_eq!(super::read_simulation_clock(&path, &last), 1020);
    }
    #[test]
    fn radio_delivery_window_exceeds_a_schema_transfer() {
        let tuning = super::network_delivery_tuning();
        tuning.validate().unwrap();
        if std::env::var_os("GS_RELIABLE_RETRANSMIT_MS").is_none() {
            assert_eq!(tuning.reliable_retransmit_ms, 250);
            assert_eq!(tuning.reliable_max_retries, 17);
            assert!(
                u64::from(tuning.reliable_retransmit_ms)
                    * u64::from(tuning.reliable_max_retries + 1)
                    >= 4500
            );
        }
    }

    use super::*;

    #[test]
    fn hosted_router_retains_a_burst_larger_than_embedded_default() {
        let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = delivered.clone();
        let endpoint = telemetry_schema::endpoint("GROUND_STATION");
        let handler = EndpointHandler::new_packet_handler(endpoint, move |_packet: &Packet| {
            observed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        let router = sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([handler])
                .with_sender("GS")
                .with_memory_config(network_memory_config())
                .unwrap(),
        );
        // A brief host worker pause must not silently evict the early readings.
        // This burst exceeds the old 100 KiB shared state ceiling.
        for i in 0..2048 {
            let packet = Packet::new(
                telemetry_schema::data_type("BATTERY_VOLTAGE"),
                &[endpoint],
                "AB",
                i,
                15.0f32.to_le_bytes().to_vec().into(),
            )
            .unwrap();
            router.rx_queue(packet).unwrap();
        }
        assert!(router.export_runtime_stats().queues.shared_queue_bytes_used > 100 * 1024);
        router.process_rx_queue_with_timeout(0).unwrap();
        assert_eq!(delivered.load(Ordering::Relaxed), 2048);
    }

    #[test]
    fn rrls_observer_does_not_accumulate_local_sdk_transmissions() {
        use sedsnet::router::{NetworkVariablePermissions, Router, RouterConfig};
        use std::sync::atomic::{AtomicU64, AtomicUsize};
        telemetry_schema::initialize().unwrap();
        let clock = Arc::new(AtomicU64::new(0));
        let now = clock.clone();
        let observer = Router::new_with_clock(
            RouterConfig::new([]).with_sender("RRLS"),
            Box::new(move || now.load(Ordering::SeqCst)),
        );
        let primary = rrls::Mesh::new(rrls::Role::Primary, 1);
        let mut relay = rrls::Mesh::new(rrls::Role::Rrls, 2);
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();
        let link: CommsLinkConfig = serde_json::from_value(serde_json::json!({
            "interface":"serial", "port":"/dev/test-radio", "protocol":"raw_uart"
        }))
        .unwrap();
        let side = observer.add_side_packed_with_priority_and_options(
            "radio",
            move |_, _| {
                seen.fetch_add(1, Ordering::SeqCst);
                Err(sedsnet::TelemetryError::Io(
                    "observer has no local SDK egress",
                ))
            },
            rocket_router_side_options(&link, true),
        );
        let ty = telemetry_schema::data_type("FLIGHT_STATE");
        observer
            .enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
            .unwrap();
        for tick in (0..30_000).step_by(100) {
            clock.store(tick, Ordering::SeqCst);
            // A live observer must learn a GS value while remaining silent.
            if tick % 1000 == 0 {
                let packet = Packet::new(
                    ty,
                    &[telemetry_schema::endpoint("FLIGHT_STATE")],
                    "GS",
                    tick + 1,
                    vec![2].into(),
                )
                .unwrap();
                let wire = sedsnet::wire_format::pack_packet(&packet);
                let frame = primary.local_tx(false, &wire, tick).unwrap();
                let observed = relay
                    .incoming(frame.command, &frame.bytes, tick)
                    .expect("RRLS must expose primary radio data to its read-only observer");
                observer.rx_packed_from_side(&observed.bytes, side).unwrap();
                observer.request_managed_variable(ty).unwrap();
            }
            observer.process_all_queues_with_timeout(0).unwrap();
        }
        assert!(observer.get_cached_network_variable(ty).unwrap().is_some());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            0,
            "RRLS must suppress local SDK work before its radio queue, not after pacing"
        );
        let stats = observer.export_runtime_stats();
        assert_eq!(stats.queues.tx_len, 0);
        assert_eq!(stats.total_handler_retries, 0);
    }

    #[test]
    fn raw_uart_radio_matches_rf_hop_policy() {
        let link = CommsLinkConfig::Serial {
            serial: crate::comms_config::SerialLinkConfig {
                hardware_flow_control: false,
                rfd_profile: None,
                port: "sim://av-bay".to_owned(),
                baud_rate: 57_600,
                protocol: crate::comms_config::SerialProtocol::RawUart,
            },
        };

        assert!(!router_hop_reliable_enabled(&link));
        let mut framed = link.clone();
        if let CommsLinkConfig::Serial { serial } = &mut framed {
            serial.protocol = crate::comms_config::SerialProtocol::PacketFramed;
        }
        assert!(router_hop_reliable_enabled(&framed));
    }

    #[test]
    fn pico_fi_i2c_uses_transport_delivery_without_hop_retries() {
        let link = CommsLinkConfig::I2c {
            i2c: crate::comms_config::I2cLinkConfig {
                protocol_version: 1,
                bus: 1,
                addr: 0x17,
                chunk_delay_ms: 1,
                initial_wait_ms: 0,
            },
        };

        assert!(!router_hop_reliable_enabled(&link));
    }

    #[test]
    fn daq_source_cadence_preserves250_default_and_requires_five_intervals() {
        let legacy = super::DaqLoadcellCadence::parse(None).unwrap();
        assert_eq!(
            (
                legacy.hz,
                legacy.minimum_interval_ms,
                legacy.maximum_interval_ms
            ),
            (250, 2, 6)
        );
        for (raw, period) in [("40", 25), ("250", 4)] {
            let expected = super::DaqLoadcellCadence::parse(Some(raw)).unwrap();
            let mut probe = super::StreamRateProbe::default();
            for index in 0..=5 {
                assert_eq!(
                    probe.observe(
                        index * period,
                        expected.minimum_interval_ms,
                        expected.maximum_interval_ms,
                        5
                    ),
                    index == 5
                );
            }
        }
    }

    #[test]
    fn daq_source_cadence_rejects_the_other_publication_rate() {
        for (expected_hz, wrong_period) in [("40", 4), ("250", 25)] {
            let expected = super::DaqLoadcellCadence::parse(Some(expected_hz)).unwrap();
            let mut probe = super::StreamRateProbe::default();
            for index in 0..100 {
                assert!(!probe.observe(
                    index * wrong_period,
                    expected.minimum_interval_ms,
                    expected.maximum_interval_ms,
                    5
                ));
            }
        }
    }

    #[test]
    fn daq_source_cadence_rejects_invalid_configuration() {
        for raw in ["", "0", "-1", "1001", "NaN", "1.5", "forty", " 40"] {
            assert!(
                super::DaqLoadcellCadence::parse(Some(raw)).is_err(),
                "accepted {raw:?}"
            );
        }
    }

    #[test]
    fn stream_rate_probe_requires_consecutive_timestamp_intervals() {
        let mut probe = StreamRateProbe::default();
        assert!(!probe.observe(1_000, 750, 1_250, 2));
        assert!(!probe.observe(2_010, 750, 1_250, 2));
        assert!(probe.observe(3_020, 750, 1_250, 2));
        assert!(!probe.observe(5_000, 750, 1_250, 2));
    }

    #[test]
    fn source_cadence_accepts_reordering_but_not_missing_samples_or_replays() {
        let mut probe = StreamRateProbe::default();
        // Unordered telemetry arrives late without changing its source cadence.
        for timestamp in [1000, 1400, 1200, 1800, 1600] {
            assert!(!probe.observe(timestamp, 150, 250, 5));
        }
        assert!(probe.observe(2000, 150, 250, 5));
        assert!(!probe.observe(1400, 150, 250, 5));
        assert!(!probe.observe(3000, 150, 250, 5));
        let mut missing = StreamRateProbe::default();
        for timestamp in [1000, 1200, 1600, 1800, 2000, 1000, 1200, 1600, 1800, 2000] {
            assert!(!missing.observe(timestamp, 150, 250, 5));
        }
    }

    #[test]
    fn captured_unordered_flight_streams_retain_their_source_cadence() {
        let mut probe = StreamRateProbe::default();
        let mut passed = false;
        for timestamp in [
            2185, 2385, 2987, 2586, 2787, 3387, 3987, 3787, 5196, 4595, 6002, 6405, 6605, 6805,
            7205, 7005, 7605, 7405, 8005, 8595, 8395, 8795, 9796, 9997, 10627, 9396, 10417, 10837,
            11244, 11444, 11044, 11645, 11846, 12046, 12256, 12664, 12865, 12464, 13065, 13465,
            13265, 13665, 14064, 13865, 14254, 14444, 14843, 14643, 15043,
        ] {
            passed |= probe.observe(timestamp, 150, 250, 5);
        }
        assert!(
            passed,
            "IMU_DATA source cadence was lost by arrival ordering"
        );
        let mut probe = StreamRateProbe::default();
        let mut passed = false;
        for timestamp in [
            2393, 2837, 3072, 2632, 3688, 3889, 3473, 4359, 4119, 4559, 5160, 5399, 6240, 5839,
            6440, 7040, 6640, 6840, 7241, 7682, 8346, 8144, 7922, 8546, 9147, 9386, 9786, 9987,
            10405, 10616, 10195, 11112, 10868, 11312, 11512, 11712, 11912, 12117, 12570, 12368,
            12770, 12972, 13410, 13611, 14051, 14279, 14469, 13210, 14908, 14708, 15109, 15348,
            15548,
        ] {
            passed |= probe.observe(timestamp, 150, 250, 5);
        }
        assert!(
            passed,
            "BAROMETER_DATA source cadence was lost by arrival ordering"
        );
    }

    #[test]
    fn flight_stream_gate_requires_five_hz_not_one_hz() {
        let mut probe = StreamRateProbe::default();
        for timestamp in [0, 1000, 2000, 3000, 4000, 5000] {
            assert!(!probe.observe(timestamp, 150, 250, 5));
        }
        for timestamp in [5211, 5422, 5633, 5844] {
            assert!(!probe.observe(timestamp, 150, 250, 5));
        }
        assert!(probe.observe(6055, 150, 250, 5));
        assert!(!probe.observe(7055, 150, 250, 5));
    }

    #[test]
    fn daq_stream_gate_accepts_250_hz_and_rejects_old_50_hz_cadence() {
        let mut probe = StreamRateProbe::default();
        for timestamp in [0, 20, 40, 60, 80, 100] {
            assert!(!probe.observe(timestamp, 2, 6, 5));
        }
        for timestamp in [104, 108, 112, 116] {
            assert!(!probe.observe(timestamp, 2, 6, 5));
        }
        assert!(probe.observe(120, 2, 6, 5));
        assert!(!probe.observe(140, 2, 6, 5));
    }

    #[test]
    fn startup_topology_request_is_sent_on_every_physical_side() {
        let router = sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]));
        let left = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let right = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let left_tx = left.clone();
        let right_tx = right.clone();
        router.add_side_packed("rocket", move |bytes| {
            left_tx.lock().unwrap().push(bytes.to_vec());
            Ok(())
        });
        router.add_side_packed("fill", move |bytes| {
            right_tx.lock().unwrap().push(bytes.to_vec());
            Ok(())
        });

        request_startup_topology(&router).unwrap();
        router.process_all_queues().unwrap();

        for frames in [&left, &right] {
            let frames = frames.lock().unwrap();
            assert!(frames.iter().any(|frame| {
                sedsnet::wire_format::unpack_packet(frame).is_ok_and(|packet| {
                    packet.data_type() == sedsnet::config::DataType::DiscoveryTopologyRequest
                })
            }));
        }
    }
}

/// Creates or upgrades the auth session table used by token-based login.
async fn ensure_auth_sessions_table(db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS auth_sessions (
            token             TEXT PRIMARY KEY,
            username          TEXT NOT NULL,
            session_type      TEXT NOT NULL,
            can_view_data     INTEGER NOT NULL,
            can_send_commands INTEGER NOT NULL,
            allowed_commands_json TEXT NOT NULL DEFAULT '[]',
            created_at_ms     INTEGER NOT NULL,
            expires_at_ms     INTEGER NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    let session_columns = sqlx::query("PRAGMA table_info(auth_sessions);")
        .fetch_all(db)
        .await?;
    let has_allowed_commands_json = session_columns.iter().any(|row| {
        row.get::<String, _>("name")
            .eq_ignore_ascii_case("allowed_commands_json")
    });
    if !has_allowed_commands_json {
        sqlx::query(
            "ALTER TABLE auth_sessions ADD COLUMN allowed_commands_json TEXT NOT NULL DEFAULT '[]';",
        )
            .execute(db)
            .await?;
    }

    Ok(())
}

/// Waits for process termination signals and then fan-outs the app-wide shutdown request.
async fn shutdown_signal(state: Arc<AppState>) {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            log::error!("failed to install Ctrl+C handler: {err}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => {
                log::error!("failed to install SIGTERM handler: {err}");
            }
        }
    };

    // An accepted station mode switch takes the same clean shutdown path.
    #[cfg(unix)]
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
        _ = station_mode_switch::restart_requested() => {
            log::info!("station mode switch requested a clean restart");
        }
    }

    #[cfg(not(unix))]
    tokio::select! {
        _ = ctrl_c => {}
        _ = station_mode_switch::restart_requested() => {}
    }

    if state.request_shutdown() {
        log::info!("shutdown requested from signal handler");
    }
}

fn open_primary_link(link: &CommsLinkConfig) -> anyhow::Result<Box<dyn CommsDevice>> {
    let device=open_link(link)?;
    if matches!(link,CommsLinkConfig::Serial{..}|CommsLinkConfig::RaspberryPiGpioUart{..}|CommsLinkConfig::CustomSerial{..}) {
        Ok(Box::new(rrls_radio::RecoveryRadio::from_open(link.clone(),device)?))
    } else { Ok(device) }
}

/// The AV-bay radio. The RFD modem check runs once, before the first open.
fn open_av_bay_primary_link(link: &CommsLinkConfig) -> anyhow::Result<Box<dyn CommsDevice>> {
    let device = rrls_radio::open_av_bay_link(link)?;
    if matches!(link,CommsLinkConfig::Serial{..}|CommsLinkConfig::RaspberryPiGpioUart{..}|CommsLinkConfig::CustomSerial{..}) {
        Ok(Box::new(rrls_radio::RecoveryRadio::from_open_av_bay(link.clone(),device)?))
    } else { Ok(device) }
}

fn open_rocket_comms(link: &CommsLinkConfig) -> (Arc<Mutex<Box<dyn CommsDevice>>>, bool) {
    if rrls::is_rrls() {
        return (
            Arc::new(Mutex::new(Box::new(rrls_radio::RecoveryRadio::new_av_bay(
                link.clone(),
            )))),
            false,
        );
    }
    match open_av_bay_primary_link(link) {
        Ok(r) => {
            gs_debug_println!("Rocket comms online");
            (Arc::new(Mutex::new(r)), true)
        }
        Err(e) => {
            gs_debug_println!("Rocket comms missing, using DummyComms: {}", e);
            log::warn!(
                "AV bay link unavailable: {e}. Setup hint: {}",
                startup_failure_hint(link)
            );
            // RUNTIME-MODE-BOOT: links open once at start. Normal refuses to
            // start without them; the other modes use a stand-in, and a later
            // switch into Normal is refused while a stand-in exists
            // (comms::dummy_links).
            if runtime_mode::is_normal() {
                panic!("Rocket comms missing and testing mode not enabled")
            }
            (
                Arc::new(Mutex::new(Box::new(DummyComms::new("Rocket Comms")))),
                false,
            )
        }
    }
}

fn open_umbilical_comms(link: &CommsLinkConfig) -> (Arc<Mutex<Box<dyn CommsDevice>>>, bool) {
    match open_primary_link(link) {
        Ok(r) => {
            gs_debug_println!("Umbilical comms online");
            (Arc::new(Mutex::new(r)), true)
        }
        Err(e) => {
            gs_debug_println!("Umbilical comms missing, using DummyComms: {}", e);
            log::warn!(
                "Fill box link unavailable: {e}. Setup hint: {}",
                startup_failure_hint(link)
            );
            // RUNTIME-MODE-BOOT: links open once at start. Normal refuses to
            // start without them; the other modes use a stand-in, and a later
            // switch into Normal is refused while a stand-in exists
            // (comms::dummy_links).
            if runtime_mode::is_normal() {
                panic!("Umbilical comms missing and testing mode not enabled")
            }
            (
                Arc::new(Mutex::new(Box::new(DummyComms::new("Umbilical Comms")))),
                false,
            )
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let boot_mode = runtime_mode::init_boot();
    eprintln!("Station mode at start: {}", runtime_mode::label(boot_mode));
    flight_sim_runtime::validate_startup()?;
    rrls_sim_primary::validate_startup()?;
    let daq_loadcell_cadence = DaqLoadcellCadence::from_env()?;
    telemetry_schema::initialize()?;
    sedsnet::config::set_runtime_tuning_config(network_delivery_tuning())?;

    logger::init()?;
    let _host_temperature = host_temperature::start();
    // The flag names are kept from the per-mode builds: simulator tooling
    // parses this line.
    log::info!(
        "groundstation backend starting features testing={} hitl_mode={} test_fire_mode={} mode={} firmware_sim={}",
        boot_mode == station_mode::Mode::Testing,
        boot_mode == station_mode::Mode::Hitl,
        boot_mode == station_mode::Mode::TestFire,
        runtime_mode::label(boot_mode),
        cfg!(feature = "firmware_sim")
    );

    if flight_sim_runtime::enabled() {
        log::info!("FlightSim execution_mode=flight_sim output_domain=simulated; physical transports/proxy disabled");
    }

    // Initialize GPIO
    let gpio = gpio::GpioPins::new();

    // Ensure offline map tiles
    if let Err(e) = ensure_map_data(&selected_map_region()).await {
        log::warn!("failed to ensure map tiles: {e:#}");
        // you can choose to return Err(e) instead if tiles are mandatory
    }

    // --- DB path ---
    let recordings_dir = PathBuf::from("./data");
    let placeholder_db_path = recordings_dir.join(DEFAULT_TELEMETRY_DB_FILENAME);
    recover_sqlite_sidecars_in_dir(&recordings_dir).await?;
    if placeholder_db_path.exists() {
        let _ = delete_sqlite_if_empty(&placeholder_db_path.to_string_lossy()).await;
    }
    let db = open_in_memory_telemetry_db().await?;
    let auth_db_path = recordings_dir.join("users.db");
    let auth_db_path_str = ensure_sqlite_db_file(&auth_db_path)?;
    let auth_db = sqlx::SqlitePool::connect(&format!("sqlite://{}", auth_db_path_str)).await?;
    apply_sqlite_pragmas(&auth_db).await;

    ensure_auth_sessions_table(&auth_db).await?;

    // --- Channels ---
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let db_work_queue_size = env_usize("GS_DB_WORK_QUEUE_SIZE", 8_192, 1024, 262_144);
    let (db_queue_tx, db_queue_rx) = mpsc::channel::<DbQueueItem>(db_work_queue_size);
    let ws_broadcast_capacity = env_usize("GS_WS_BROADCAST_CAPACITY", 8192, 512, 262_144);
    let board_status_capacity = env_usize("GS_BOARD_STATUS_BROADCAST_CAPACITY", 256, 64, 4096);
    let alerts_capacity = env_usize("GS_ALERTS_BROADCAST_CAPACITY", 1024, 128, 8192);
    let notifications_capacity = env_usize("GS_NOTIFICATIONS_BROADCAST_CAPACITY", 64, 16, 2048);
    let actions_capacity = env_usize("GS_ACTION_POLICY_BROADCAST_CAPACITY", 64, 16, 2048);
    let launch_clock_capacity = env_usize("GS_LAUNCH_CLOCK_BROADCAST_CAPACITY", 32, 8, 1024);
    let recording_status_capacity =
        env_usize("GS_RECORDING_STATUS_BROADCAST_CAPACITY", 32, 8, 1024);
    let (ws_tx, _ws_rx) = broadcast::channel(ws_broadcast_capacity);
    let (board_status_tx, _board_status_rx) = broadcast::channel(board_status_capacity);
    let (dashboard_reset_tx, _dashboard_reset_rx) = broadcast::channel(16);
    let (notifications_tx, _notifications_rx) = broadcast::channel(notifications_capacity);
    let (messages_tx, _messages_rx) = broadcast::channel(notifications_capacity);
    let (action_policy_tx, _action_policy_rx) = broadcast::channel(actions_capacity);
    let (fill_targets_tx, _fill_targets_rx) = broadcast::channel(actions_capacity);
    let (launch_clock_tx, _launch_clock_rx) = broadcast::channel(launch_clock_capacity);
    let (recording_status_tx, _recording_status_rx) = broadcast::channel(recording_status_capacity);
    let (shutdown_tx, _shutdown_rx) = broadcast::channel(8);

    // --- Shared state ---
    let mut board_status = HashMap::new();
    for board in Board::ALL {
        board_status.insert(
            *board,
            BoardStatus {
                packet_count: 0,
                last_seen_ms: None,
                last_seen_model_ms: None,
                last_seen_instant: None,
                ema_gap_ms: None,
                warned: false,
            },
        );
    }

    let ring_buffer_capacity = env_usize("GS_RING_BUFFER_CAPACITY", 65_536, 1024, 1_000_000);
    let loadcell_calibration = loadcell::load_or_default();
    let comms_links = if flight_sim_runtime::enabled() {
        comms_config::CommsLinksConfig::default()
    } else if rrls_sim_primary::enabled() {
        rrls_sim_primary::comms_links()?
    } else {
        comms_config::load_or_default()
    };
    if !flight_sim_runtime::enabled() {
        comms::rfd_profile::initialize(&comms_links.av_bay)?;
        comms::rfd_modem::remember_av_bay(&comms_links.av_bay);
        if let comms_config::CommsLinkConfig::PicoFiTcp { tcp } = &comms_links.fill_box {
            ethernet_autoconfig::prepare(tcp.discovery_interface.clone()).await;
        } else if rrls::is_rrls() {
            ethernet_autoconfig::prepare(None).await;
        }
        rrls::configure(&comms_links.av_bay)?;
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let users_path = std::env::var_os("GS_USERS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("users").join("users.json"));
    let legacy_users_path = manifest_dir.join("data").join("users.json");
    if std::env::var_os("GS_USERS_FILE").is_none()
        && !users_path.exists()
        && legacy_users_path.exists()
    {
        if let Some(parent) = users_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&legacy_users_path, &users_path)?;
    }
    let auth = Arc::new(auth::AuthManager::new(users_path));
    auth.ensure_file()
        .map_err(|e| anyhow::anyhow!("failed to initialize users.json: {e}"))?;
    let _ = auth.cleanup_expired_sessions(&auth_db).await;
    if std::env::var("GS_SIM_POLICY_CORRECT_CONTROLS").as_deref() == Ok("1") {
        assert!(
            simulation_clock_ms().is_some(),
            "qualification requires actual simulator clock"
        );
        assert_eq!(
            std::env::var("GS_SIM_TRACE_API_REQUESTS").as_deref(),
            Ok("1")
        );
        assert_eq!(std::env::var("SEDSNET_TRACE_RELIABLE").as_deref(), Ok("1"));
        qualification_requests::trace_scope();
        // Qualification boots run in one fixed mode; the check runs at start
        // under the boot mode, exactly as the old non-HITL builds did.
        if !runtime_mode::is_hitl() {
            network_variables::verify_normal_qualification_boot()?;
        }
    }
    let history_directory = history_startup_path::environment()?;
    let launch_history = Arc::new(Mutex::new(launch_history::Store::environment()?));
    let paired_identity = station_pairing::Pairing::load_or_unpaired("abort history").map(|pairing| -> [u8; 16] {
        let digest = ring::digest::digest(&ring::digest::SHA256, pairing.public.installation_id.as_bytes());
        digest.as_ref()[..16].try_into().unwrap()
    });
    let history_identity: [u8;16] = if let Some(identity) = paired_identity
        .filter(|identity| !history_directory.exists() || board_abort_history::Store::anchor_identity(&history_directory).ok() == Some(*identity))
    {
        identity
    } else if history_directory.exists() {
        // History written before this pairing (or under another one) keeps its
        // own anchor: pairing from the UI must not stop the station starting.
        if paired_identity.is_some() {
            log::warn!("Abort history predates the current station pairing; keeping its existing identity");
        }
        board_abort_history::Store::anchor_identity(&history_directory)?
    } else {
        rand::random()
    };
    let history_changed = Arc::new(tokio::sync::Notify::new());
    let (history_restored, history_publisher, abort_history_writer) =
        board_abort_history_writer::Owner::open(
            &history_directory,
            history_identity,
            history_changed.clone(),
        )?;
    let board_abort = Arc::new(board_abort::Runtime::restored(
        history_restored,
        history_publisher,
        history_changed,
    ));

    // Restore closed admission before constructing or starting any producer.
    let (restored_recovery_owner, restored_generation) =
        launch_history.lock().unwrap().restored_recovery_barrier();
    let restored_clock = launch_history.lock().unwrap().snapshot().clock;
    let (restored_daq, restored_stamp) = launch_history.lock().unwrap().daq_snapshot();
    if network_variables::persisted_daq_log_clock_with_revision().is_none() && restored_stamp != 0 {
        network_variables::restore_persisted_daq_log_clock_snapshot(restored_daq, restored_stamp)?;
    }
    let state = Arc::new(AppState {
        fill_recording: Default::default(),
        control_ingress: Arc::new(Default::default()),
        ring_buffer: Arc::new(Mutex::new(RingBuffer::new(ring_buffer_capacity))),
        cmd_tx,
        ws_tx,
        warnings_tx: broadcast::channel(alerts_capacity).0,
        errors_tx: broadcast::channel(alerts_capacity).0,
        alert_ack_state: Arc::new(Mutex::new(AlertAckStateMsg::default())),
            alert_conditions: Arc::new(crate::alert_condition_runtime::Runtime::new()),
        alert_ack_tx: broadcast::channel(16).0,
        dashboard_reset_tx,
        db: Arc::new(Mutex::new(db)),
        db_path: Arc::new(Mutex::new("sqlite::memory:".to_string())),
        placeholder_db_path: placeholder_db_path.to_string_lossy().to_string(),
        db_queue_tx,
        db_write_runtime: tokio::runtime::Handle::current(),
        auth_db,
        state: Arc::new(Mutex::new(
            crate::types::u8_to_flight_state(network_variables::flight_state())
                .unwrap_or(FlightStateMode::Startup),
        )),
        state_tx: broadcast::channel(16).0,
        gpio,
        board_status: Arc::new(Mutex::new(board_status)),
        board_status_tx,
        last_board_status_broadcast_ms: Arc::new(AtomicU64::new(0)),
        last_packet_rx_ms: Arc::new(AtomicU64::new(0)),
        umbilical_valve_states: Arc::new(Mutex::new(HashMap::new())),
        pending_umbilical_valve_states: Arc::new(Mutex::new(HashMap::new())),
        latest_fuel_tank_pressure: Arc::new(Mutex::new(None)),
        gse: Arc::new(Mutex::new(crate::gse::Runtime::default())),
        phase_control_selected: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        phase_control_revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        phase_control_public: Arc::new(Mutex::new(None)),
        sequence_progress: Arc::new(crate::sequence_progress_service::Shared::new(
            rand::random::<u128>(),
        )),
        automatic_generation: Arc::new(AtomicU64::new(restored_generation)),
        operator_admission_barrier: Arc::new(std::sync::atomic::AtomicU64::new(
            restored_recovery_owner,
        )),
        operator_queue: Arc::new(crate::operator_queue::Control::default()),
        operator_dispatch_inflight: Arc::new(AtomicUsize::new(0)),
        manual_abort_intent: Arc::new(crate::manual_abort_intent::ManualAbortIntent::default()),
        abort_resend: Arc::new(crate::manual_abort_resend::AbortResend::default()),
        deferred_abort: Arc::new(crate::deferred_abort::Inbox::default()),
        igniter_sense: Arc::new(std::sync::Mutex::new(igniter_sense::Tracker::default())),
        board_recovery: Arc::new(crate::board_recovery::Runtime::default()),
        board_abort,
        launch_dedup_revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        board_abort_tx: broadcast::channel(16).0,
        plumbing_drive: Arc::new(crate::actuator_plumbing::Runtime::default()),
        latest_fill_mass_kg: Arc::new(Mutex::new(None)),
        loadcell_processing: Arc::new(crate::loadcell_zero::Service::default()),
        network_traffic: Arc::new(Mutex::new(crate::network_traffic::Sampler::default())),
        network_health: Arc::new(Mutex::new(crate::network_health::Tracker::default())),
        loadcell_calibration: Arc::new(Mutex::new(loadcell_calibration)),
        auto_zero: Default::default(),
        daq_log_session: Arc::new(Mutex::new(restored_daq[0])),
        shutdown_tx,
        shutdown_requested: Arc::new(AtomicBool::new(false)),
        pending_db_writes: Arc::new(AtomicUsize::new(0)),
        ignition_observer: Arc::new(Default::default()),
        transport_tx_activity: Arc::new(Default::default()),
        radio_tx_activity: Arc::new(Default::default()),
        db_write_notify: Arc::new(Notify::new()),
        notifications: Arc::new(Mutex::new(Vec::new())),
        notifications_tx,
        next_notification_id: Arc::new(AtomicU64::new(0)),
        messages: Arc::new(Mutex::new(Vec::new())),
        messages_tx,
        next_message_id: Arc::new(AtomicU64::new(0)),
        action_policy: Arc::new(Mutex::new(default_action_policy())),
        sequence_policy_state: Arc::new(Mutex::new(
            crate::sequences::SequencePolicyState::default(),
        )),
        action_policy_tx,
        fill_targets: Arc::new(Mutex::new(fill_targets::load_or_default())),
        fill_targets_tx,
        launch_history: launch_history.clone(),
        launch_clock: Arc::new(Mutex::new(restored_clock)),
        launch_clock_tx,
        launch_sequence_command_pending: Arc::new(AtomicBool::new(false)),
        launch_mode_gate: Default::default(),
        launch_indicator_latched: Arc::new(AtomicBool::new(false)),
        abort_indicator_latched: Arc::new(AtomicBool::new(false)),
        hitl_button_interlock_enabled: Arc::new(AtomicBool::new(false)),
        hitl_launch_interlock_enabled: Arc::new(AtomicBool::new(false)),
        hitl_safety_interlocks: state::default_hitl_safety_interlocks(),
        hitl_physical_launch_uses_ground_station: Arc::new(AtomicBool::new(false)),
        recording_status: Arc::new(Mutex::new(RecordingStatusMsg {
            mode: RecordingModeWire::Idle,
            db_path: None,
        })),
        recording_status_tx,
        last_command_ms: Arc::new(Mutex::new(HashMap::new())),
        fill_sequence_continue_requests: Arc::new(AtomicU64::new(0)),
        recent_telemetry_cache: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        latest_gps_fix_by_sender: Arc::new(Mutex::new(HashMap::new())),
        latest_gps_satellites_by_sender: Arc::new(Mutex::new(HashMap::new())),
        recent_alerts_cache: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        av_bay_comms_connected: Arc::new(AtomicBool::new(false)),
        fill_comms_connected: Arc::new(AtomicBool::new(false)),
        topology_router: Arc::new(std::sync::OnceLock::new()),
        kg50_input_sync: Default::default(),
        auth,
    });
    // The RFD modem check may enter command mode only outside flight and
    // with no launch or sequence running. Seed it from the restored state
    // before the radio opens, then keep it current.
    comms::rfd_modem::set_gate(comms::rfd_modem::station_block_reason(&state));
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                comms::rfd_modem::set_gate(comms::rfd_modem::station_block_reason(&state));
                comms::rfd_modem::run_deferred_startup_check();
                if let Some(notice) = comms::rfd_modem::take_notice() {
                    web::emit_notification_warning(&state, notice);
                }
            }
        });
    }

    // The modeled testing boards exist only in a Testing+simulation boot
    // (testing_board_lifecycle::eligible), which never switches mode in
    // process (a simulation change restarts), so this stays a start-up step.
    if runtime_mode::is_testing() {
        flight_sim::initialize_model_owners(
            &state,
            history_restored.snapshot.actual_abort,
            history_restored.snapshot.unknown,
        );
    }
    state.board_abort.publish_initial_history();
    state.set_messages_snapshot(Vec::new());

    if !rrls::is_rrls() {
        gpio_panel::setup_gpio_panel(state.clone()).expect("failed to setup gpio panel");
    }
    let sequence_shutdown_rx = state.shutdown_subscribe();
    let mut sequence_task = start_sequence_task(state.clone(), sequence_shutdown_rx);

    // --- Router endpoint handlers ---
    let ground_station_handler_state_clone = state.clone();
    let pilot_open_ack_generation = Arc::new(AtomicU64::new(0));
    let pilot_open_ack_generation_handler = pilot_open_ack_generation.clone();
    let command_status_probe = Arc::new(Mutex::new(command_probe::CommandProbe::default()));
    let command_status_probe_handler = command_status_probe.clone();
    let rf_gps_rate = Arc::new(Mutex::new(StreamRateProbe::default()));
    let rf_gps_rate_handler = rf_gps_rate.clone();
    let rf_gps_rate_reported = Arc::new(AtomicBool::new(false));
    let rf_gps_rate_reported_handler = rf_gps_rate_reported.clone();
    let fc_sensor_rate = Arc::new(Mutex::new(StreamRateProbe::default()));
    let fc_sensor_rate_handler = fc_sensor_rate.clone();
    let fc_sensor_rate_reported = Arc::new(AtomicBool::new(false));
    let fc_sensor_rate_reported_handler = fc_sensor_rate_reported.clone();
    let fc_barometer_rate = Mutex::new(StreamRateProbe::default());
    let fc_barometer_rate_reported = AtomicBool::new(false);
    let power_sensor_rate = Arc::new(Mutex::new(StreamRateProbe::default()));
    let power_sensor_rate_handler = power_sensor_rate.clone();
    let power_seen = Arc::new(AtomicBool::new(false));
    let power_seen_handler = power_seen.clone();
    let power_rate_reported = Arc::new(AtomicBool::new(false));
    let power_rate_reported_handler = power_rate_reported.clone();
    let valve_power_freshness = Arc::new(Mutex::new(ValvePowerFreshness::default()));
    let valve_power_rates = Mutex::new([StreamRateProbe::default(), StreamRateProbe::default()]);
    let valve_power_reported = [AtomicBool::new(false), AtomicBool::new(false)];
    if std::env::var_os("GS_SIM_VALIDATE_VALVE_POWER").is_some() {
        let freshness = valve_power_freshness.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let mut probe = freshness.lock().unwrap();
                let now = validation_elapsed_ms(probe.clock);
                for index in 0..2 {
                    if let Some(gap) = probe.check(index, now) {
                        log::error!(
                            "full-bay Valve power telemetry stale: channel={index}, gap={gap} ms"
                        );
                    }
                }
            }
        });
    }
    let fill_delivery_clock = ValidationInstant::now();
    let fill_delivery = std::env::var_os("GS_SIM_VALIDATE_FILL_DELIVERY")
        .map(|_| Arc::new(Mutex::new(fill_delivery_probe::Probe::default())));
    if let Some(probe) = fill_delivery.clone() {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let now = validation_elapsed_ms(fill_delivery_clock);
                let window = probe.lock().unwrap().take_window(now);
                if let Some(w) = window {
                    let detail = serde_json::json!({"start_ms":w.start_ms,"elapsed_ms":w.elapsed_ms,
                        "channels":fill_delivery_probe::NAMES,"received":w.received,
                        "max_gap_ms":w.max_gap_ms,"passed":w.passed});
                    if w.passed {
                        log::info!("sustained fill delivery passed {detail}");
                    } else {
                        log::error!("sustained fill delivery failed {detail}");
                    }
                }
            }
        });
    }
    let daq_loadcell_rate = Arc::new(Mutex::new(StreamRateProbe::default()));
    let daq_loadcell_rate_handler = daq_loadcell_rate.clone();
    let daq_loadcell_rate_reported = Arc::new(AtomicBool::new(false));
    let daq_loadcell_rate_reported_handler = daq_loadcell_rate_reported.clone();
    let fill_telemetry_seen = Arc::new(Mutex::new(HashSet::<Board>::new()));
    let fill_telemetry_seen_handler = fill_telemetry_seen.clone();
    let abort_handler_state_clone = state.clone();
    let flight_state_handler_state_clone = state.clone();
    let heartbeat_handler_state_clone = state.clone();

    let ground_station_handler = EndpointHandler::new_packet_handler(
        telemetry_schema::endpoint("GROUND_STATION"),
        move |pkt: &Packet| {
            let ingress_generation = gse::automatic_generation(&ground_station_handler_state_clone);
            crate::board_abort::observe_raw(&ground_station_handler_state_clone, pkt);
            let umbilical_capture =
                if pkt.data_type() == telemetry_schema::data_type("UMBILICAL_STATUS") {
                    match telemetry_task::capture_umbilical_status_at_ingress(
                        &ground_station_handler_state_clone,
                        pkt,
                    ) {
                        Some(capture) => Some(capture),
                        None => return Ok(()),
                    }
                } else {
                    None
                };
            let captured_owner = umbilical_capture.as_ref().map(|capture| capture.owner());
            if let Some(probe) = &fill_delivery {
                let board = captured_owner.or_else(|| {
                    ground_station_handler_state_clone.board_from_network_sender(pkt.sender())
                });
                let ty = pkt.data_type();
                let channel = if board == Some(Board::DaqBoard) {
                    if ty == telemetry_schema::data_type("KG1000")
                        || ty == telemetry_schema::data_type("DAQ_LOADCELL_CHAMBER")
                    {
                        Some(0)
                    } else {
                        let input = ground_station_handler_state_clone
                            .loadcell_calibration
                            .lock()
                            .unwrap()
                            .kg50_input
                            .wire();
                        pkt.data_as_f32()
                            .is_ok_and(|values| {
                                fill_delivery_probe::selected_kg50(
                                    input,
                                    ty == telemetry_schema::data_type("KG50"),
                                    ty == telemetry_schema::data_type("DAQ_KG50_SELECTED"),
                                    &values,
                                )
                            })
                            .then_some(1)
                    }
                } else if board == Some(Board::ValveBoard) {
                    if ty == telemetry_schema::data_type("BATTERY_VOLTAGE") {
                        Some(2)
                    } else if ty == telemetry_schema::data_type("BATTERY_CURRENT") {
                        Some(3)
                    } else if ty == telemetry_schema::data_type("FUEL_TANK_PRESSURE") {
                        Some(4)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(channel) = channel
                    && pkt
                        .data_as_f32()
                        .is_ok_and(|v| !v.is_empty() && v.iter().all(|x| x.is_finite()))
                    {
                        probe.lock().unwrap().observe(
                            channel,
                            pkt.packet_id(),
                            validation_elapsed_ms(fill_delivery_clock),
                        );
                    }
            }
            if std::env::var_os("GS_SIM_VALIDATE_TELEMETRY_RETURN").is_some() {
                let board = captured_owner.or_else(|| {
                    ground_station_handler_state_clone.board_from_network_sender(pkt.sender())
                });
                if pkt.data_type() == telemetry_schema::data_type("GPS_SATELLITE_NUMBER")
                    || pkt.data_type() == telemetry_schema::data_type("IMU_DATA")
                    || pkt.data_type() == telemetry_schema::data_type("BAROMETER_DATA")
                    || pkt.data_type() == telemetry_schema::data_type("BATTERY_VOLTAGE")
                {
                    log::info!(
                        "full-bay telemetry candidate sender={} resolved={board:?} type={:?} timestamp={}",
                        pkt.sender(),
                        pkt.data_type(),
                        pkt.timestamp()
                    );
                }
                if board == Some(Board::RFBoard)
                    && pkt.data_type() == telemetry_schema::data_type("GPS_SATELLITE_NUMBER")
                {
                    let valid =
                        rf_gps_rate_handler
                            .lock()
                            .unwrap()
                            .observe(pkt.timestamp(), 750, 1_250, 2);
                    if valid && !rf_gps_rate_reported_handler.swap(true, Ordering::AcqRel) {
                        log::info!("full-bay RF GPS 1 Hz stream reached GroundStation");
                    }
                }
                if board == Some(Board::FlightComputer)
                    && pkt.data_type() == telemetry_schema::data_type("IMU_DATA")
                {
                    let valid = fc_sensor_rate_handler.lock().unwrap().observe(
                        pkt.timestamp(),
                        150,
                        250,
                        5,
                    );
                    if valid && !fc_sensor_rate_reported_handler.swap(true, Ordering::AcqRel) {
                        log::info!("full-bay Flight sensor 5 Hz stream reached GroundStation");
                    }
                }
                if board == Some(Board::FlightComputer)
                    && pkt.data_type() == telemetry_schema::data_type("BAROMETER_DATA")
                {
                    let valid =
                        fc_barometer_rate
                            .lock()
                            .unwrap()
                            .observe(pkt.timestamp(), 150, 250, 5);
                    if valid && !fc_barometer_rate_reported.swap(true, Ordering::AcqRel) {
                        log::info!("full-bay Flight barometer 5 Hz stream reached GroundStation");
                    }
                }
                if std::env::var_os("GS_SIM_VALIDATE_VALVE_POWER").is_some()
                    && board == Some(Board::ValveBoard)
                {
                    let index = if pkt.data_type() == telemetry_schema::data_type("BATTERY_VOLTAGE")
                    {
                        Some(0)
                    } else if pkt.data_type() == telemetry_schema::data_type("BATTERY_CURRENT") {
                        Some(1)
                    } else {
                        None
                    };
                    if let Some(index) = index
                        && pkt
                            .data_as_f32()
                            .is_ok_and(|values| values.len() == 1 && values[0].is_finite())
                        {
                            let mut probe = valve_power_freshness.lock().unwrap();
                            let now = validation_elapsed_ms(probe.clock);
                            if let Some(gap) = probe.observe(index, pkt.timestamp(), now) {
                                log::error!(
                                    "full-bay Valve power telemetry stale: channel={index}, gap={gap} ms"
                                );
                            }
                            drop(probe);
                            if valve_power_rates.lock().unwrap()[index].observe(
                                pkt.timestamp(),
                                700,
                                1500,
                                2,
                            ) && !valve_power_reported[index].swap(true, Ordering::AcqRel)
                            {
                                log::info!(
                                    "full-bay Valve {} 1 Hz stream reached GroundStation",
                                    if index == 0 { "battery" } else { "current" }
                                );
                            }
                        }
                }
                if board == Some(Board::PowerBoard)
                    && pkt.data_type() == telemetry_schema::data_type("BATTERY_VOLTAGE")
                {
                    if !power_seen_handler.swap(true, Ordering::AcqRel) {
                        log::info!("full-bay Power telemetry reached GroundStation");
                    }
                    let valid = power_sensor_rate_handler.lock().unwrap().observe(
                        pkt.timestamp(),
                        4_000,
                        6_000,
                        1,
                    );
                    if valid && !power_rate_reported_handler.swap(true, Ordering::AcqRel) {
                        log::info!("full-bay Power 5-second stream reached GroundStation");
                    }
                }
                // The simulator observes incoming source packets, never the
                // derived KG1000 row created from the combined pressure format.
                let daq_loadcell_timestamp = validation_loadcell_source_timestamp(board, pkt);
                if let Some(source_timestamp) = daq_loadcell_timestamp {
                    let valid = daq_loadcell_rate_handler.lock().unwrap().observe(
                        source_timestamp,
                        daq_loadcell_cadence.minimum_interval_ms,
                        daq_loadcell_cadence.maximum_interval_ms,
                        5,
                    );
                    if valid && !daq_loadcell_rate_reported_handler.swap(true, Ordering::AcqRel) {
                        log::info!(
                            "full-bay DAQ raw loadcell {} Hz stream reached GroundStation",
                            daq_loadcell_cadence.hz
                        );
                    }
                }
                if let Some(
                    board @ (Board::GatewayBoard
                    | Board::ActuatorBoard
                    | Board::ValveBoard
                    | Board::DaqBoard),
                ) = board
                {
                    let mut seen = fill_telemetry_seen_handler.lock().unwrap();
                    if seen.insert(board) {
                        log::info!(
                            "full-bay fill telemetry reached GroundStation from {}",
                            board.sender_id()
                        );
                    }
                }
            }
            if pkt.data_type() == telemetry_schema::data_type("AB_FILL_MOTION_STATUS_V2") {
                crate::actuator_plumbing::receive_motion_status(
                    &ground_station_handler_state_clone,
                    pkt,
                );
                return Ok(());
            }
            if pkt.data_type() == telemetry_schema::data_type("AB_FILL_DRIVE_STATUS") {
                crate::actuator_plumbing::receive_status(&ground_station_handler_state_clone, pkt);
                return Ok(());
            }
            if pkt.data_type() == telemetry_schema::data_type("BOARD_STATE_STATUS") {
                // Correlated actual board readbacks bypass DB/GSE work.
                crate::board_recovery::receive_status(&ground_station_handler_state_clone, pkt);
                return Ok(());
            }
            if pkt.data_type() == telemetry_schema::data_type("UMBILICAL_STATUS") {
                log::debug!(
                    "umbilical status packet received sender={} endpoints={:?} payload={:02x?}",
                    pkt.sender(),
                    pkt.endpoints(),
                    pkt.payload()
                );
                let source = captured_owner.or_else(|| {
                    ground_station_handler_state_clone.board_from_network_sender(pkt.sender())
                });
                if let Some(source) = source {
                    command_status_probe_handler
                        .lock()
                        .unwrap()
                        .observe(source.sender_id(), pkt.payload());
                }
                if source == Some(Board::ValveBoard)
                    && pkt.payload() == [ValveBoardCommands::PilotOpen as u8, 1].as_slice()
                {
                    pilot_open_ack_generation_handler.fetch_add(1, Ordering::Relaxed);
                }
            }
            if let Some(capture) = umbilical_capture {
                ground_station_handler_state_clone.mark_packet_received(capture.received_ms());
                if let Some(row) = telemetry_task::apply_captured_umbilical_status(
                    &ground_station_handler_state_clone,
                    capture,
                ) {
                    ground_station_handler_state_clone.cache_recent_telemetry(row.clone());
                    let _ = ground_station_handler_state_clone.ws_tx.send(row.clone());
                    ground_station_handler_state_clone
                        .ring_buffer
                        .lock()
                        .unwrap()
                        .push(crate::ring_buffer::ReceivedPacket::AppliedStatus {
                            packet: pkt.clone(),
                            row,
                        });
                }
                return Ok(());
            }
            ground_station_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
            let mut rb = ground_station_handler_state_clone
                .ring_buffer
                .lock()
                .unwrap();
            rb.push(crate::ring_buffer::ReceivedPacket::Telemetry { packet: pkt.clone(), ingress_generation });
            Ok(())
        },
    );

    let flight_state_handler = EndpointHandler::new_packet_handler(
        telemetry_schema::endpoint("FLIGHT_STATE"),
        move |pkt: &Packet| {
            let ingress_generation = gse::automatic_generation(&flight_state_handler_state_clone);
            crate::board_abort::observe_raw(&flight_state_handler_state_clone, pkt);
            flight_state_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
            let mut rb = flight_state_handler_state_clone.ring_buffer.lock().unwrap();
            rb.push(crate::ring_buffer::ReceivedPacket::Telemetry { packet: pkt.clone(), ingress_generation });
            Ok(())
        },
    );

    let abort_handler = EndpointHandler::new_packet_handler(
        telemetry_schema::endpoint("ABORT"),
        move |pkt: &Packet| {
            crate::deferred_abort::observe(
                &abort_handler_state_clone,
                pkt,
                crate::gse::automatic_generation(&abort_handler_state_clone),
            );
            Ok(())
        },
    );

    let heartbeat_handler = EndpointHandler::new_packet_handler(
        telemetry_schema::endpoint("HEART_BEAT"),
        move |pkt: &Packet| {
            let ingress_generation = gse::automatic_generation(&heartbeat_handler_state_clone);
            heartbeat_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
            let mut rb = heartbeat_handler_state_clone.ring_buffer.lock().unwrap();
            rb.push(crate::ring_buffer::ReceivedPacket::Telemetry { packet: pkt.clone(), ingress_generation });
            Ok(())
        },
    );

    let telemetry_error_handler = EndpointHandler::new_packet_handler(
        sedsnet::config::DataEndpoint::TelemetryError,
        |pkt: &Packet| {
            log::error!(
                "remote SEDSNet error from {}: {}",
                pkt.sender(),
                String::from_utf8_lossy(pkt.payload())
            );
            Ok(())
        },
    );

    let mut cfg = sedsnet::router::RouterConfig::new([
        ground_station_handler,
        abort_handler,
        flight_state_handler,
        heartbeat_handler,
        telemetry_error_handler,
    ])
    .with_sender(if rrls::is_rrls() {
        "RRLS"
    } else {
        Board::GroundStation.sender_id()
    })
    .with_preferred_discovery_master(Board::GroundStation.sender_id())
    .with_memory_config(network_memory_config())?;
    if telemetry_task::timesync_enabled() {
        cfg = cfg.with_timesync(TimeSyncConfig {
            role: TimeSyncRole::Source,
            priority: 50,
            ..TimeSyncConfig::default()
        });
    }

    // --- Radios ---
    gs_debug_println!("AV bay config: {}", link_description(&comms_links.av_bay));
    gs_debug_println!(
        "Fill box config: {}",
        link_description(&comms_links.fill_box)
    );

    let force_sim_comms = runtime_mode::is_testing() && flight_sim::sim_mode_enabled();

    let (rocket_comms, av_bay_comms_connected): (Arc<Mutex<Box<dyn CommsDevice>>>, bool) =
        if force_sim_comms {
            gs_debug_println!("Testing simulator mode enabled; using DummyComms for rocket comms");
            (
                Arc::new(Mutex::new(Box::new(DummyComms::new("Rocket Comms")))),
                false,
            )
        } else {
            open_rocket_comms(&comms_links.av_bay)
        };

    let (umbilical_comms, fill_comms_connected) = if rrls::is_rrls() {
        (None, false)
    } else {
        let (comms, connected): (Arc<Mutex<Box<dyn CommsDevice>>>, bool) = if force_sim_comms {
            gs_debug_println!(
                "Testing simulator mode enabled; using DummyComms for umbilical comms"
            );
            (
                Arc::new(Mutex::new(Box::new(DummyComms::new("Umbilical Comms")))),
                false,
            )
        } else {
            open_umbilical_comms(&comms_links.fill_box)
        };
        (Some(comms), connected)
    };
    // The modeled board links exist only in a Testing+simulation boot.
    if runtime_mode::is_testing() {
        crate::testing_board_lifecycle::bind_selected(
            rocket_comms.lock().unwrap().testing_model_link(),
            umbilical_comms
                .as_ref()
                .is_some_and(|link| link.lock().unwrap().testing_model_link()),
        );
    }

    state
        .av_bay_comms_connected
        .store(av_bay_comms_connected, Ordering::Relaxed);
    state
        .fill_comms_connected
        .store(fill_comms_connected, Ordering::Relaxed);

    let router = Arc::new(if simulation_clock_ms().is_some() {
        sedsnet::router::Router::new_with_clock(
            cfg,
            Box::new(|| simulation_clock_ms().unwrap_or(0)),
        )
    } else if network_router_time_divisor() == 1 {
        sedsnet::router::Router::new(cfg)
    } else {
        // Renode advances several MCU instances cooperatively and can run much
        // slower than wall time. Keep discovery expiry on the simulated time
        // scale so valid firmware routes do not disappear only because the
        // host process runs natively. Production defaults to a divisor of one.
        let divisor = network_router_time_divisor();
        let started = std::time::Instant::now();
        sedsnet::router::Router::new_with_clock(
            cfg,
            Box::new(move || {
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX) / divisor
            }),
        )
    });
    if telemetry_task::timesync_enabled() {
        telemetry_task::refresh_host_network_time(&router, get_current_timestamp_ms());
    }
    network_variables::initialize_for_state(&state, &router)?;
    set_network_time_router(router.clone());
    let _ = state.topology_router.set(router.clone());
    let (mut abort_observations, abort_observations_stop) = crate::deferred_abort::spawn(state.clone());
    // Retires stale automatic ABORTs; stops with the station.
    drop(crate::abort_retirement::spawn(state.clone(), router.clone()));

    let (rocket_tx, rocket_rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    let (umbilical_tx, umbilical_rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();

    // Bound queued radio work independently of the much faster Pico link.
    let rocket_tx_budget = Arc::new(if comms::rfd_profile::active().is_some() {
        TxQueueBudget::new_radio(2048, 256)
    } else {
        TxQueueBudget::new(2048)
    });
    telemetry_task::register_radio_tx_budget(rocket_tx_budget.clone());
    let (rocket_side, rocket_retry_tracker) = {
        let rocket_tx = rocket_tx.clone();
        let budget = rocket_tx_budget.clone();
        let activity = state.radio_tx_activity.clone();
        let opts = rocket_router_side_options(&comms_links.av_bay, rrls::is_rrls());
        router.add_side_packed_leased_with_priority_and_options(
            "rocket_comms",
            move |pkt, priority| {
                budget.try_send_leased_tracked(&rocket_tx,priority,pkt,&activity)
            },
            opts,
            sedsnet::router::RetryTrackerLimits::default(),
        )?
    };

    let umbilical_side = if umbilical_comms.is_none() {
        None
    } else {
        Some({
            let umbilical_tx = umbilical_tx.clone();
            let activity = state.transport_tx_activity.clone();
            let recovery_classifier=telemetry_task::RecoveryFrameClassifier::default();
            let opts = RouterSideOptions {
                reliable_enabled: router_hop_reliable_enabled(&comms_links.fill_box),
                ..Default::default()
            }
            .with_small_packet_transport(1024);
            router.add_side_packed_with_priority_and_options(
                "umbilical_comms",
                move |pkt, priority| {
                    activity.try_enqueue_owned(recovery_classifier.classify(pkt),pkt.to_vec(), |owned| {
                        umbilical_tx.send((priority, owned)).map_err(|_| {
                            TelemetryError::HandlerError("umbilical_comms tx queue closed")
                        })
                    })
                },
                opts,
            )
        })
    };

    rocket_comms
        .lock()
        .unwrap()
        .bind_retry_tracker(rocket_retry_tracker)?;
    rocket_comms
        .lock()
        .expect("failed to get rocket comms lock")
        .set_side_id(rocket_side);
    if let (Some(comms), Some(side)) = (&umbilical_comms, umbilical_side) {
        comms
            .lock()
            .expect("failed to get umbilical comms lock")
            .set_side_id(side);
    }
    keep_schema_exchange_off_the_radio(&router, rocket_side, umbilical_side.as_slice())?;

    if !rrls::is_rrls() {
        qualification_requests::prime_initial_cache(&router, || {
            network_variables::publish_bootstrap_for_state(&state, &router).map(|_| ())
        })?;
    }
    // The full-system simulator uses the production toggle path while each
    // phase starts a fresh GroundStation process. Keeping this behind an
    // explicit validation-only environment flag avoids altering deployment
    // startup behavior.
    if std::env::var("GS_SIM_TOGGLE_UNDERGLOW_ON_START")
        .ok()
        .as_deref()
        == Some("1")
    {
        let enabled = network_variables::toggle_underglow(&router)?;
        log::info!("full-bay validation toggled AV bay underglow: {enabled}");
    }

    let initial_discovery = if rrls::is_rrls() {
        Ok(())
    } else if std::env::var("GS_SIM_COMPACT_INITIAL_DISCOVERY")
        .ok()
        .as_deref()
        == Some("1")
    {
        /* Every linked-validation firmware image embeds this same schema.
         * Send the normal lightweight topology/address refresh and omit the
         * redundant multi-kilobyte hosted schema transfer. */
        router.poll_discovery().map(|_| ())
    } else {
        router.announce_discovery()
    };
    if let Err(err) = initial_discovery {
        eprintln!("WARNING: failed to queue initial discovery announce: {err}");
    }
    // Announcing only teaches adjacent routers about GroundStation. It does
    // not make a freshly restarted process recover topology that peers are
    // advertising at their slow steady-state cadence. Request one bounded
    // snapshot on every side at startup; discovered routers answer with their
    // current transitive graph, so fill and AV-bay routes recover without a
    // recurring broadcast/fanout workaround.
    if let Err(err) = if rrls::is_rrls() {
        Ok(())
    } else {
        request_startup_topology(&router)
    } {
        eprintln!("WARNING: failed to request startup network topology: {err}");
    }

    // --- Background tasks ---
    let telemetry_shutdown_rx = state.shutdown_subscribe();
    let safety_shutdown_rx = state.shutdown_subscribe();
    let mut comms_workers = vec![CommsWorkerHandle {
        transport_activity: Some(state.radio_tx_activity.clone()),
        name: "rocket_comms",
        tx_budget: Some(rocket_tx_budget.clone()),
        comms: rocket_comms,
        tx_comms: None,
        side_id: rocket_side,
        tx_rx: rocket_rx,
        legacy_single_worker: false,
        prioritize_rx: false,
        dedicated_radio_io: false,
    }];
    if let (Some(comms), Some(side_id)) = (umbilical_comms, umbilical_side) {
        comms_workers.push(CommsWorkerHandle {
            transport_activity: Some(state.transport_tx_activity.clone()),
            name: "umbilical_comms",
            tx_budget: None,
            comms,
            tx_comms: None,
            side_id,
            tx_rx: umbilical_rx,
            legacy_single_worker: false,
            prioritize_rx: false,
            dedicated_radio_io: false,
        });
    }
    let mut tt = tokio::spawn(telemetry_task(
        state.clone(),
        router.clone(),
        comms_workers,
        cmd_rx,
        db_queue_rx,
        telemetry_shutdown_rx,
    ));
    // A bounded retained E-stop publisher is independent of the ordinary cmd
    // channel. Local SDK callbacks retain observations and never wait on GSE.
    let stop_state = state.clone();
    let stop_router = router.clone();
    let _board_abort_publications = tokio::spawn(crate::board_abort::publish_service(
        state.clone(),
        state.shutdown_subscribe(),
    ));
    let _board_abort_queries = tokio::spawn(crate::board_abort::query_service(
        state.clone(),
        state.shutdown_subscribe(),
    ));
    let _mission_phase = tokio::spawn(crate::mission_phase::service(
        state.clone(),
        state.shutdown_subscribe(),
    ));
    let _valve_report_health = tokio::spawn(crate::valve_report_health::service(
        state.clone(),
        state.shutdown_subscribe(),
    ));
    let _kg50_input_sync = tokio::spawn(crate::kg50_input_sync::service(
        state.clone(),
        state.shutdown_subscribe(),
    ));
    let _abort_resend = tokio::spawn(crate::manual_abort_resend::service(
        state.clone(),
        router.clone(),
        state.shutdown_subscribe(),
    ));
    let _manual_utc_reservations = tokio::spawn(crate::manual_abort_utc::service(
        state.clone(),
        router.clone(),
        state.shutdown_subscribe(),
    ));
    let _manual_stop_publisher = tokio::spawn(crate::manual_abort_intent::service_with_claim(
        state.manual_abort_intent.clone(),
        state.shutdown_subscribe(),
        move |claim| match crate::manual_abort_utc::publish_local_claim(&stop_state, &stop_router, claim)
        {
            Ok(true) => {
                stop_state.board_abort.request_abort_confirmation();
                if let Some(wire) = claim.wire() {
                    let recovery = crate::board_recovery::coordinator::recovery_activity(&stop_state).flatten();
                    stop_state.abort_resend.arm(claim.epoch(), wire, recovery);
                }
                log::info!(
                    "manual Abort accepted by SDK epoch={} current={} pending={:?}",
                    claim.epoch(),
                    stop_state.manual_abort_intent.epoch(),
                    stop_state.manual_abort_intent.pending_epoch()
                );
                true
            }
            // Logger WriteMode::Direct can wait on disk. A refused admission
            // keeps the bounded RAM intent and retries without synchronous I/O.
            Ok(false) | Err(_) => false,
        },
    ));
    let full_bay_discovery_ready = Arc::new(AtomicBool::new(false));
    // No allocation or command-path change outside simulator validation. Keep
    // the boot PilotOpen test from racing the soak's first PilotClose.
    let valve_startup_ack = (std::env::var("GS_SIM_VALIDATE_VALVE_ROUNDTRIP")
        .ok()
        .as_deref()
        == Some("1"))
    .then(|| Arc::new(command_probe::StartupAck::new()));
    if let Ok(expected) = std::env::var("GS_SIM_EXPECT_DISCOVERY_NODES") {
        let validation_router = router.clone();
        let validation_state = state.clone();
        let validation_generation = gse::capture_operator_origin(&validation_state).unwrap_or(0);
        let validation_discovery_ready = full_bay_discovery_ready.clone();
        let validation_pilot_ack_generation = pilot_open_ack_generation.clone();
        let validate_valve_roundtrip = std::env::var("GS_SIM_VALIDATE_VALVE_ROUNDTRIP")
            .ok()
            .as_deref()
            == Some("1");
        let control_step_ms = std::env::var("GS_SIM_CONTROL_STEP_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(750);
        tokio::spawn(async move {
            let expected = expected
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect::<BTreeSet<_>>();
            let mut discovery_waits = 0u32;
            loop {
                let topology = validation_router.export_topology();
                let mut discovered = BTreeSet::new();
                for route in topology.routes {
                    for announcer in route.announcers {
                        discovered.insert(announcer.sender_id);
                        for board in announcer.routers {
                            discovered.insert(board.sender_id);
                        }
                    }
                }
                if expected.is_subset(&discovered) {
                    assert_eq!(
                        validation_router.preferred_discovery_master().as_deref(),
                        Some("GS")
                    );
                    log::info!(
                        "full-bay preferred discovery master GS verified after named discovery"
                    );
                    validation_discovery_ready.store(true, Ordering::Release);
                    log::info!(
                        "full-bay named discovery ready: {}",
                        discovered.into_iter().collect::<Vec<_>>().join(",")
                    );
                    if validate_valve_roundtrip {
                        let ack_started = ValidationInstant::now();
                        while validation_pilot_ack_generation.load(Ordering::Acquire) == 0 {
                            if validation_elapsed_ms(ack_started) >= 30_000 {
                                log::warn!(
                                    "full-bay managed-variable validation is continuing without an early Valve command acknowledgement"
                                );
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                    if std::env::var("GS_SIM_VALIDATE_FLIGHT_COMMAND")
                        .ok()
                        .as_deref()
                        == Some("1")
                    {
                        let Some(_claim) =
                            gse::claim_operator_dispatch(&validation_state, validation_generation)
                        else {
                            return;
                        };
                        let command = FlightComputerCommands::EvaluationRelax as u8;
                        match queue_locally_routed_flight_command(
                            &validation_state,
                            &validation_router,
                            "full-bay Flight Computer validation",
                            &[command],
                        ) {
                            Ok(()) => {
                                flush_command_tx(
                                    &validation_router,
                                    "full-bay Flight Computer validation tx",
                                );
                                log::info!("full-bay Flight Computer command queued: {command}");
                            }
                            Err(error) => {
                                log::error!("full-bay Flight Computer command failed: {error}");
                                return;
                            }
                        }
                    }
                    let flight_states = std::env::var("GS_SIM_FLIGHT_STATE_SEQUENCE")
                        .ok()
                        .map(|sequence| {
                            sequence
                                .split(',')
                                .filter_map(|value| match value.trim().parse::<u8>() {
                                    Ok(state) => Some(state),
                                    Err(_) => {
                                        log::error!(
                                            "full-bay flight-state sequence contains invalid value {value:?}"
                                        );
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    // Only this explicit qualification path substitutes a stable normal
                    // policy value. HITL still exercises the original state sequence.
                    let flight_states = if std::env::var("GS_SIM_POLICY_CORRECT_CONTROLS")
                        .as_deref()
                        == Ok("1")
                    {
                        if !runtime_mode::is_hitl() {
                            let boot_started = ValidationInstant::now();
                            while validation_state.local_flight_state_snapshot()
                                != types::FlightState::Idle
                            {
                                assert!(
                                    validation_elapsed_ms(boot_started) < 30_000,
                                    "normal boot did not actually advance Startup0 to Idle1"
                                );
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                            assert!(
                                wait_for_validation_idle(&validation_router).await,
                                "normal boot accepted obligations must drain within original bound"
                            );
                        }
                        qualification_requests::policy_states(
                            runtime_mode::is_hitl(),
                            &flight_states,
                        )
                        .expect("qualification must use nonactuating prelaunch states")
                    } else {
                        flight_states
                    };
                    let underglow = std::env::var("GS_SIM_UNDERGLOW_SEQUENCE")
                        .ok()
                        .map(|sequence| {
                            sequence
                                .split(',')
                                .map(|value| matches!(value.trim(), "1" | "true" | "on"))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let flight_buzzer = std::env::var("GS_SIM_FLIGHT_BUZZER_SEQUENCE")
                        .ok()
                        .map(|sequence| {
                            sequence
                                .split(',')
                                .map(|value| matches!(value.trim(), "1" | "true" | "on"))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let rounds = flight_states
                        .len()
                        .max(underglow.len())
                        .max(flight_buzzer.len());
                    for round in 0..rounds {
                        tokio::time::sleep(Duration::from_millis(control_step_ms)).await;
                        log::info!(
                            "full-bay managed-variable topology before round {}: {:?}",
                            round + 1,
                            validation_router.export_topology().routes
                        );
                        // The pending counter is global. Drain the preceding FC
                        // command and startup calibration first so their ACKs
                        // cannot be mislabeled as managed-variable latency.
                        if !wait_for_validation_idle(&validation_router).await {
                            break;
                        }
                        let expected_types = [
                            flight_states
                                .get(round)
                                .map(|_| telemetry_schema::data_type("FLIGHT_STATE")),
                            underglow
                                .get(round)
                                .map(|_| telemetry_schema::data_type("AV_BAY_UNDERGLOW")),
                            flight_buzzer
                                .get(round)
                                .map(|_| telemetry_schema::data_type("FLIGHT_BUZZER")),
                        ]
                        .into_iter()
                        .flatten()
                        .map(|data_type| {
                            let count = validation_router
                                .export_runtime_stats()
                                .sides
                                .iter()
                                .flat_map(|side| side.data_types.iter())
                                .filter(|kind| kind.data_type == data_type)
                                .map(|kind| kind.tx_packets)
                                .sum::<u64>();
                            (data_type, count)
                        })
                        .collect::<Vec<_>>();
                        let Some(round_claim) =
                            gse::claim_operator_dispatch(&validation_state, validation_generation)
                        else {
                            return;
                        };
                        if let Some(&state) = flight_states.get(round) {
                            if let Err(error) =
                                qualification_requests::in_controlled_round(round + 1, || {
                                    network_variables::set_flight_state(&validation_router, state)
                                })
                            {
                                log::error!("full-bay flight-state sequence failed: {error}");
                                break;
                            }
                            log::info!("full-bay validation set flight state: {state}");
                        }
                        if let Some(&enabled) = underglow.get(round) {
                            if let Err(error) =
                                qualification_requests::in_controlled_round(round + 1, || {
                                    network_variables::set_underglow(&validation_router, enabled)
                                })
                            {
                                log::error!("full-bay underglow sequence failed: {error}");
                                break;
                            }
                            log::info!("full-bay validation set AV bay underglow: {enabled}");
                        }
                        if let Some(&enabled) = flight_buzzer.get(round) {
                            if let Err(error) =
                                qualification_requests::in_controlled_round(round + 1, || {
                                    network_variables::set_flight_buzzer(
                                        &validation_router,
                                        enabled,
                                    )
                                })
                            {
                                log::error!("full-bay Flight buzzer sequence failed: {error}");
                                break;
                            }
                            log::info!("full-bay validation set Flight buzzer: {enabled}");
                        }
                        flush_command_tx(
                            &validation_router,
                            "full-bay managed-variable validation tx",
                        );
                        drop(round_claim);
                        if std::env::var("GS_SIM_POLICY_CORRECT_CONTROLS").as_deref() == Ok("1") {
                            // This is only a bounded global-settling barrier, never ACK
                            // evidence (retry expiry can also remove a pending entry).
                            // The mandatory audit_obligations.py gate charges every
                            // accepted ID from its own API request to ALL owner receipts.
                            if !wait_for_validation_idle(&validation_router).await {
                                break;
                            }
                            log::info!(
                                "full-bay qualified round {} settled; per-ID500ms ALL-owner audit remains REQUIRED, no latency pass inferred",
                                round + 1
                            );
                        } else if !wait_for_validation_reliable_delivery(
                            &validation_router,
                            "managed-variable round",
                            &expected_types,
                        )
                        .await
                        {
                            break;
                        }
                    }
                    let traffic_started = ValidationInstant::now();
                    loop {
                        /* client_stats is link/announcer oriented: downstream
                         * boards nested inside a router's topology legitimately
                         * have no direct side counters. Use the application's
                         * canonical per-sender packet observations here so this
                         * assertion proves real board payloads reached GS rather
                         * than crediting every board with its relay side total. */
                        let status =
                            validation_state.board_status_snapshot(get_current_timestamp_ms());
                        let attributed = status
                            .boards
                            .into_iter()
                            .filter(|entry| {
                                expected.contains(&entry.sender_id) && entry.packet_count > 0
                            })
                            .map(|entry| (entry.sender_id, entry.packet_count))
                            .collect::<Vec<_>>();
                        let graph =
                            validation_state.network_topology_snapshot(get_current_timestamp_ms());
                        let graph_attributed = expected
                            .iter()
                            .filter_map(|sender| {
                                let board = Board::from_sender_id(sender)?;
                                let node = graph.nodes.iter().find(|node| {
                                    node.sender_id.as_deref() == Some(sender.as_str())
                                })?;
                                let packets = node.stats?.packets_received;
                                (node.label == board.as_str() && packets > 0)
                                    .then(|| format!("{sender}={}:rx={packets}", node.label))
                            })
                            .collect::<Vec<_>>();
                        if attributed.len() == expected.len()
                            && graph_attributed.len() == expected.len()
                        {
                            log::info!(
                                "full-bay per-board traffic attribution ready: {}",
                                attributed
                                    .iter()
                                    .map(|(sender, packets)| format!("{sender}={packets}"))
                                    .collect::<Vec<_>>()
                                    .join(",")
                            );
                            log::info!(
                                "full-bay network graph attribution ready: {}",
                                graph_attributed.join(",")
                            );
                            break;
                        }
                        if validation_elapsed_ms(traffic_started) >= 30_000 {
                            let observed = validation_state
                                .board_status_snapshot(get_current_timestamp_ms())
                                .boards
                                .into_iter()
                                .filter(|entry| entry.packet_count > 0)
                                .map(|entry| entry.sender_id)
                                .collect::<BTreeSet<_>>();
                            let missing =
                                expected.difference(&observed).cloned().collect::<Vec<_>>();
                            let graph_missing = expected
                                .iter()
                                .filter(|sender| {
                                    let Some(board) = Board::from_sender_id(sender) else {
                                        return true;
                                    };
                                    !graph.nodes.iter().any(|node| {
                                        node.sender_id.as_deref() == Some(sender.as_str())
                                            && node.label == board.as_str()
                                            && node
                                                .stats
                                                .is_some_and(|stats| stats.packets_received > 0)
                                    })
                                })
                                .cloned()
                                .collect::<Vec<_>>();
                            log::error!(
                                "full-bay per-board traffic attribution timed out; missing={missing:?} graph_missing={graph_missing:?}"
                            );
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    break;
                }
                discovery_waits += 1;
                if discovery_waits.is_multiple_of(100) {
                    let missing = expected.difference(&discovered).collect::<Vec<_>>();
                    let routes = validation_router.export_topology().routes;
                    log::info!(
                        "full-bay named discovery pending: missing={missing:?}; routes={routes:?}"
                    );
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }
    if std::env::var_os("GS_SIM_EXPECT_DISCOVERY_NODES").is_none()
        && let Ok(sequence) = std::env::var("GS_SIM_UNDERGLOW_SEQUENCE")
    {
        let validation_router = router.clone();
        let validation_state = state.clone();
        let validation_generation = gse::capture_operator_origin(&validation_state).unwrap_or(0);
        tokio::spawn(async move {
            for value in sequence.split(',') {
                tokio::time::sleep(Duration::from_millis(750)).await;
                let enabled = matches!(value.trim(), "1" | "true" | "on");
                let Some(_claim) =
                    gse::claim_operator_dispatch(&validation_state, validation_generation)
                else {
                    return;
                };
                match network_variables::set_underglow(&validation_router, enabled) {
                    Ok(()) => log::info!("full-bay validation set AV bay underglow: {enabled}"),
                    Err(error) => {
                        log::error!("full-bay underglow sequence failed: {error}");
                        break;
                    }
                }
            }
        });
    }
    if std::env::var_os("GS_SIM_EXPECT_DISCOVERY_NODES").is_none()
        && let Ok(sequence) = std::env::var("GS_SIM_FLIGHT_BUZZER_SEQUENCE")
    {
        let validation_router = router.clone();
        let validation_state = state.clone();
        let validation_generation = gse::capture_operator_origin(&validation_state).unwrap_or(0);
        tokio::spawn(async move {
            for value in sequence.split(',') {
                tokio::time::sleep(Duration::from_millis(750)).await;
                let enabled = matches!(value.trim(), "1" | "true" | "on");
                let Some(_claim) =
                    gse::claim_operator_dispatch(&validation_state, validation_generation)
                else {
                    return;
                };
                match network_variables::set_flight_buzzer(&validation_router, enabled) {
                    Ok(()) => {
                        log::info!("full-bay validation set Flight buzzer: {enabled}")
                    }
                    Err(error) => {
                        log::error!("full-bay Flight buzzer sequence failed: {error}");
                        break;
                    }
                }
            }
        });
    }
    if std::env::var("GS_SIM_VALIDATE_VALVE_ROUNDTRIP")
        .ok()
        .as_deref()
        == Some("1")
    {
        let validation_router = router.clone();
        let validation_state = state.clone();
        let validation_generation = gse::capture_operator_origin(&validation_state).unwrap_or(0);
        let validation_pilot_ack_generation = pilot_open_ack_generation.clone();
        let validation_discovery_ready = full_bay_discovery_ready.clone();
        let mut startup_run = valve_startup_ack
            .as_ref()
            .expect("validation flag has startup barrier")
            .begin();
        tokio::spawn(async move {
            let discovery_started = ValidationInstant::now();
            let valve_endpoint = telemetry_schema::endpoint("VALVE_BOARD");
            // Command validation must exercise learned discovery routing. The
            // linked simulator advances virtual MCU time more slowly than host
            // wall time, so a fixed host-side timeout can fire before the
            // Valve discovery announcement crosses CAN, Gateway, Pico-Fi, and
            // I2C. Wait for the actual route instead of injecting a fanout.
            let mut waits = 0u32;
            loop {
                if validation_discovery_ready.load(Ordering::Acquire)
                    && validation_router
                        .export_topology()
                        .routes
                        .iter()
                        .any(|route| {
                            route.side_name == "umbilical_comms"
                                && route.reachable_endpoints.contains(&valve_endpoint)
                        })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                waits += 1;
                if waits.is_multiple_of(100) {
                    log::info!("full-bay validation waiting for Valve discovery route");
                }
            }
            let discovery_elapsed_ms = validation_elapsed_ms(discovery_started);
            let discovery_limit_ms =
                validation_latency_limit_ms("GS_SIM_DISCOVERY_MAX_LATENCY_MS", 5_000);
            if discovery_elapsed_ms > discovery_limit_ms {
                log::error!(
                    "full-bay discovery exceeded latency bound: {discovery_elapsed_ms} ms > {discovery_limit_ms} ms"
                );
            } else {
                log::info!(
                    "full-bay discovery latency within bound: {discovery_elapsed_ms} ms <= {discovery_limit_ms} ms"
                );
            }
            log::info!("full-bay valve discovery route is ready");
            log::info!(
                "full-bay Valve discovery topology: {:?}",
                validation_router.export_topology().routes
            );

            // Incremental discovery propagates additions in both directions.
            // Do not force a full topology refresh before each command: that
            // would hide discovery regressions and waste bandwidth.
            let settle_ms = std::env::var("GS_SIM_VALVE_ROUTE_SETTLE_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(500);
            tokio::time::sleep(Duration::from_millis(settle_ms)).await;

            let command_type = telemetry_schema::data_type("VALVE_COMMAND");
            /* Exercise the same runtime-schema path as the UI. log_queue uses
             * the VALVE_COMMAND endpoint ownership learned by discovery;
             * manually constructing a packet here bypassed that schema lookup
             * and could leave it local to the GroundStation. */
            let ack_generation_before = validation_pilot_ack_generation.load(Ordering::Relaxed);
            let command_started = ValidationInstant::now();
            let ack_latency_limit_ms =
                validation_latency_limit_ms("GS_SIM_VALVE_ACK_MAX_LATENCY_MS", 2_500);
            let Some(command_claim) =
                gse::claim_operator_dispatch(&validation_state, validation_generation)
            else {
                return;
            };
            let open_result =
                validation_router.log_queue(command_type, &[ValveBoardCommands::PilotOpen as u8]);
            if let Err(err) = open_result {
                log::error!("full-bay valve-open validation command failed: {err}");
                return;
            }
            flush_command_tx(&validation_router, "full-bay valve-open validation tx");
            drop(command_claim);
            log::info!(
                "full-bay router stats after valve command: {:?}",
                validation_router.export_runtime_stats().sides
            );
            log::info!("full-bay valve-open validation command queued");

            // The linked firmware simulator advances seven MCUs cooperatively;
            // a packet that is near-instantaneous on hardware can take tens of
            // seconds of host time to traverse both emulated serial bridges.
            loop {
                if validation_pilot_ack_generation.load(Ordering::Relaxed) > ack_generation_before
                    && validation_state
                        .get_umbilical_valve_state(ValveBoardCommands::PilotOpen as u8)
                        == Some(true)
                {
                    let elapsed_ms = validation_elapsed_ms(command_started);
                    if elapsed_ms > ack_latency_limit_ms {
                        log::error!(
                            "full-bay valve ACK exceeded latency bound: {elapsed_ms} ms > {ack_latency_limit_ms} ms"
                        );
                        return;
                    }
                    log::info!(
                        "full-bay valve ACK latency within bound: {elapsed_ms} ms <= {ack_latency_limit_ms} ms"
                    );
                    if !startup_run.pass() {
                        log::error!(
                            "full-bay startup validation already failed; soak remains blocked"
                        );
                        return;
                    }
                    log::info!("full-bay valve ACK reached GroundStation");
                    return;
                }
                if validation_elapsed_ms(command_started) > ack_latency_limit_ms {
                    log::error!(
                        "full-bay valve ACK timed out: generation before={}, generation now={}, app state={:?}, topology={:?}, router stats={:?}",
                        ack_generation_before,
                        validation_pilot_ack_generation.load(Ordering::Relaxed),
                        validation_state
                            .get_umbilical_valve_state(ValveBoardCommands::PilotOpen as u8),
                        validation_router.export_topology().routes,
                        validation_router.export_runtime_stats().sides,
                    );
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }
    if std::env::var("GS_SIM_VALIDATE_SOAK_COMMANDS")
        .ok()
        .as_deref()
        == Some("1")
        && let Ok(sample_path) = std::env::var("FIRMWARE_SIM_SAMPLE_FILE")
    {
        let soak_router = router.clone();
        let soak_state = state.clone();
        let soak_generation = gse::capture_operator_origin(&soak_state).unwrap_or(0);
        let soak_status = command_status_probe.clone();
        let soak_discovery_ready = full_bay_discovery_ready.clone();
        let soak_startup_ack = valve_startup_ack.clone();
        let command_samples = std::env::var("GS_SIM_SOAK_COMMAND_SAMPLES")
            .ok()
            .map(|value| {
                value
                    .split(',')
                    .filter_map(|sample| sample.trim().parse::<u32>().ok())
                    .collect::<Vec<_>>()
            })
            .filter(|samples| !samples.is_empty())
            .unwrap_or_else(|| (1..12).collect());
        tokio::spawn(async move {
            // Capture before discovery: late discovery must not silently skip a
            // required round. A host restart resumes the global test schedule.
            let startup_sample = fs::read_to_string(&sample_path)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
                .unwrap_or(0);
            let startup_wait = ValidationInstant::now();
            loop {
                match command_probe::startup_state(soak_startup_ack.as_deref()) {
                    command_probe::StartupState::Passed
                        if soak_discovery_ready.load(Ordering::Acquire) =>
                    {
                        break;
                    }
                    command_probe::StartupState::Failed => {
                        log::error!("full-bay soak blocked: initial Valve validation failed");
                        return;
                    }
                    _ => {}
                }
                if validation_elapsed_ms(startup_wait) > 30_000 {
                    if let Some(barrier) = &soak_startup_ack {
                        barrier.fail();
                    }
                    log::error!(
                        "full-bay soak startup validation timed out; required rounds remain unacknowledged"
                    );
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let rounds = command_samples.len();
            for (round_index, target_sample) in
                command_probe::remaining_rounds(&command_samples, startup_sample)
            {
                loop {
                    let observed_sample = fs::read_to_string(&sample_path)
                        .ok()
                        .and_then(|value| value.trim().parse::<u32>().ok())
                        .unwrap_or(0);
                    if observed_sample >= target_sample {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }

                // Alternate states: periodic reports of the unchanged open
                // state cannot stand in for execution of every command.
                let open = round_index % 2 != 0;
                if std::env::var("GS_SIM_VALIDATE_SOAK_AV_BAY").ok().as_deref() == Some("1") {
                    if !wait_for_validation_idle(&soak_router).await {
                        return;
                    }
                    let before = ["AV_BAY_UNDERGLOW", "FLIGHT_COMMAND"].map(|name| {
                        let ty = telemetry_schema::data_type(name);
                        let count = soak_router
                            .export_runtime_stats()
                            .sides
                            .iter()
                            .flat_map(|side| side.data_types.iter())
                            .filter(|kind| kind.data_type == ty)
                            .map(|kind| kind.tx_packets)
                            .sum::<u64>();
                        (ty, count)
                    });
                    // This path is simulation-only and never exercises a
                    // deployment output. Require actual round transmissions
                    // and their destination receipts throughout the soak.
                    let Some(command_claim) =
                        gse::claim_operator_dispatch(&soak_state, soak_generation)
                    else {
                        return;
                    };
                    let command = FlightComputerCommands::EvaluationRelax as u8;
                    if let Err(error) = network_variables::set_underglow(&soak_router, open)
                        .and_then(|_| {
                            queue_locally_routed_flight_command(
                                &soak_state,
                                &soak_router,
                                "full-bay RF soak",
                                &[command],
                            )
                            .map_err(anyhow::Error::from)
                        })
                    {
                        log::error!(
                            "full-bay RF soak failed to queue round {}: {error}",
                            round_index + 1
                        );
                        return;
                    }
                    flush_command_tx(&soak_router, "full-bay RF soak tx");
                    drop(command_claim);
                    if !wait_for_validation_reliable_delivery(
                        &soak_router,
                        "RF soak round",
                        &before,
                    )
                    .await
                    {
                        return;
                    }
                    log::info!(
                        "full-bay RF soak control acknowledged: round {}/{rounds}, sample {target_sample}",
                        round_index + 1
                    );
                }
                for (slot, board, ty, command) in command_probe::validation_commands(open) {
                    let generation_before = soak_status.lock().unwrap().generation(slot, open);
                    let started = ValidationInstant::now();
                    let Some(command_claim) =
                        gse::claim_operator_dispatch(&soak_state, soak_generation)
                    else {
                        return;
                    };
                    if let Err(error) =
                        soak_router.log_queue(telemetry_schema::data_type(ty), &[command])
                    {
                        log::error!(
                            "full-bay soak {board} command round {}/{rounds} at sample {target_sample} failed to queue: {error}",
                            round_index + 1
                        );
                        return;
                    }
                    flush_command_tx(&soak_router, "full-bay soak valve command tx");
                    drop(command_claim);
                    let latency_limit_ms = validation_latency_limit_ms(
                        "GS_SIM_SOAK_COMMAND_ACK_MAX_LATENCY_MS",
                        2_500,
                    );
                    loop {
                        if soak_status.lock().unwrap().generation(slot, open) > generation_before {
                            let elapsed_ms = validation_elapsed_ms(started);
                            if elapsed_ms > latency_limit_ms {
                                log::error!(
                                    "full-bay soak {board} command round {}/{rounds} at sample {target_sample} ACK exceeded latency bound: {elapsed_ms} ms > {latency_limit_ms} ms",
                                    round_index + 1
                                );
                                return;
                            }
                            log::info!(
                                "full-bay soak valve command acknowledged: board={board}, state={open}, round {}/{rounds}, sample {target_sample}, latency {elapsed_ms} ms",
                                round_index + 1
                            );
                            break;
                        }
                        if validation_elapsed_ms(started) > latency_limit_ms {
                            log::error!(
                                "full-bay soak {board} command round {}/{rounds} at sample {target_sample} ACK timed out",
                                round_index + 1
                            );
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
            }
        });
    }
    // The safety task runs in every mode and idles in HITL and test fire (whose
    // old builds had none), starting over when the mode changes.
    let mut st = {
        let state = state.clone();
        let router = router.clone();
        tokio::spawn(async move {
            if !rrls::is_rrls() {
                safety_supervisor(state, router, safety_shutdown_rx).await;
            }
        })
    };

    let _antenna_gpsd = antenna::start_gpsd(state.shutdown_subscribe());

    let weak_state = Arc::downgrade(&state);
    state
        .launch_history
        .lock()
        .unwrap()
        .set_reporter(Arc::new(move |error| {
            if let Some(state) = weak_state.upgrade() {
                state.add_notification(error);
            }
        }));
    if let Some(error) = state.launch_history.lock().unwrap().persistence_error() {
        state.add_notification(format!("Saved T clock needs attention: {error}"));
    }
    let _mission_sync = mission_sync::start(state.clone());
    let _station_mode = station_mode::start(state.clone());
    station_mode_switch::start();
    let _launch_clock_sync = launch_clock_sync::start(state.clone());
    let _command_proxy = command_proxy::start(state.clone());
    let _station_pairing = station_pairing::manage::start(state.clone());

    // --- Webserver ---
    let mut video_runtime = media_runtime::VideoRuntime::start();
    let app: Router = web::router(state.clone(), video_runtime.password.clone());

    web_server::serve(
        app,
        web_server::Config::from_env()?,
        shutdown_signal(state.clone()),
    )
    .await?;

    video_runtime.stop().await;
    // Ensure background tasks are signaled even if server exits unexpectedly.
    if state.request_shutdown() {
        log::info!("shutdown requested; draining background tasks");
    } else {
        log::info!("shutdown already in progress; draining background tasks");
    }

    let telemetry_shutdown_timeout = Duration::from_secs(20);
    let task_shutdown_timeout = Duration::from_secs(5);
    state.control_ingress.seal();
    let control_ingress_terminal=state.control_ingress.wait_terminal(task_shutdown_timeout).await;
    let firmware_terminal=firmware_update::seal_shutdown_and_is_quiescent();
    let mut shutdown_producers_quiesced = control_ingress_terminal && firmware_terminal;
    let mut receiver_producers_quiesced = false;
    // Prevent late authenticated HTTP from replacing the retained owner after
    // extraction. The admitted operation keeps its original actor/journal/UTC.
    let coordinator_owner = board_recovery::seal_shutdown_and_take_owner(&state);
    if let Some(mut owner) = coordinator_owner {
        match tokio::time::timeout(telemetry_shutdown_timeout, &mut owner).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                shutdown_producers_quiesced = false;
                log::error!("Recovery owner shutdown failed: {error}");
            }
            Err(_) => {
                shutdown_producers_quiesced = false;
                log::error!("Recovery owner still running; leave abort history Active");
            }
        }
        // A timed-out admitted owner remains alive; never cancel its durable
        // application protocol merely to fabricate a clean shutdown receipt.
    }
    shutdown_producers_quiesced &= board_recovery::shutdown_clean_eligible(&state);
    // These owners include RRLS authenticated mirror ingress and control
    // context changes. Retain their handles; an underscore binding is not a
    // shutdown receipt. No application mutex is held while awaiting a join.
    for (name, owner) in [
        ("command_proxy", _command_proxy),
        ("mission_sync", _mission_sync),
        ("station_mode", _station_mode),
        ("launch_clock_sync", _launch_clock_sync),
        ("board_abort_queries", Some(_board_abort_queries)),
        ("board_abort_publications", Some(_board_abort_publications)),
        ("manual_utc_reservations", Some(_manual_utc_reservations)),
        ("manual_stop_publisher", Some(_manual_stop_publisher)),
    ] {
        let Some(mut owner) = owner else { continue };
        match tokio::time::timeout(task_shutdown_timeout, &mut owner).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                shutdown_producers_quiesced = false;
                log::error!("{name} shutdown join failed: {error}");
            }
            Err(_) => {
                shutdown_producers_quiesced = false;
                log::error!("{name} did not stop before its shutdown deadline");
            }
        }
        if !owner.is_finished() {
            owner.abort();
            let _ = owner.await;
        }
    }
    match tokio::time::timeout(task_shutdown_timeout, &mut st).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            shutdown_producers_quiesced = false;
            log::error!("safety task ended with error: {e}");
        }
        Err(_) => {
            shutdown_producers_quiesced = false;
            log::error!(
                "safety task did not shut down within {:?}",
                task_shutdown_timeout
            );
        }
    }
    if !st.is_finished() {
        st.abort();
        let _ = st.await;
    }
    match tokio::time::timeout(task_shutdown_timeout, &mut sequence_task).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            shutdown_producers_quiesced = false;
            log::error!("sequence task ended with error: {e}");
        }
        Err(_) => {
            shutdown_producers_quiesced = false;
            log::error!(
                "sequence task did not shut down within {:?}",
                task_shutdown_timeout
            );
        }
    }
    if !sequence_task.is_finished() {
        sequence_task.abort();
        let _ = sequence_task.await;
    }

    match tokio::time::timeout(telemetry_shutdown_timeout, &mut tt).await {
        Ok(Ok(outcome)) => {
            receiver_producers_quiesced = outcome.ingress_quiesced;
            shutdown_producers_quiesced &= outcome.ingress_quiesced;
        }
        Ok(Err(e)) => {
            shutdown_producers_quiesced = false;
            log::error!("telemetry task ended with error: {e}");
        }
        Err(_) => {
            shutdown_producers_quiesced = false;
            log::error!(
                "telemetry task did not shut down within {:?}",
                telemetry_shutdown_timeout
            );
        }
    }
    if !tt.is_finished() {
        tt.abort();
        let _ = tt.await;
    }
    // Keep manual observation apply alive until receiver intake is actually
    // stopped. An ambiguous receiver never earns a stop/clean receipt.
    let mut abort_observations_stop = Some(abort_observations_stop);
    if receiver_producers_quiesced {
        let _ = abort_observations_stop.take().unwrap().send(());
    }
    match tokio::time::timeout(task_shutdown_timeout, &mut abort_observations).await {
        Ok(Ok(outcome)) => {
            shutdown_producers_quiesced &= receiver_producers_quiesced && outcome.quiesced
        }
        Ok(Err(error)) => {
            shutdown_producers_quiesced = false;
            log::error!("Abort observation apply owner failed: {error}");
        }
        Err(_) => {
            shutdown_producers_quiesced = false;
            log::error!("Abort observation owner still active; leave history Active");
        }
    }
    // No abort/cancel of an admitted blocking manual apply to obtain Clean.
    // History composition point: ONLY if this is true may the adapter freeze
    // the register, take its final snapshot, drain the owned writer, and attempt
    // checked Clean sealing. Any failure must leave the durable session Active.
    if !shutdown_producers_quiesced {
        log::error!(
            "Abort history cannot be clean-sealed: producer quiescence was not established"
        );
    }

    // Late HTTP claims must be gone, not merely hidden by server completion.
    shutdown_producers_quiesced &= state.operator_dispatch_inflight.load(Ordering::Acquire) == 0;
    // Active/Unknown protects a later Launch; it is not a promise of physical
    // Stop delivery across process death. No replay/cancel/restamp for Clean.
    if shutdown_producers_quiesced {
        shutdown_producers_quiesced = board_abort::shutdown_stop_quiesced(&state);
    }
    let final_history = if shutdown_producers_quiesced {
        state.board_abort.freeze_history_after_quiescence()
    } else {
        state.board_abort.history_snapshot()
    };
    match tokio::task::spawn_blocking(move || {
        abort_history_writer.finish(final_history, shutdown_producers_quiesced)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::error!("Abort history shutdown failed; no clean receipt: {error}"),
        Err(error) => log::error!("Abort history writer join failed; no clean receipt: {error}"),
    }

    let clock_flush = state.launch_history.lock().unwrap().close_and_waiter();
    if let Err(error) = launch_history::flush(clock_flush).await {
        log::error!("Launch history shutdown flush failed: {error}");
    }

    let db_drain_timeout = Duration::from_secs(10);
    if !state.wait_for_db_writes(db_drain_timeout).await {
        log::error!(
            "Timed out waiting for DB writes. Pending writes remaining: {}",
            state.pending_db_write_count()
        );
    }

    let telemetry_db = state.telemetry_db_pool();
    let telemetry_db_path = state.telemetry_db_path();
    if telemetry_db_path == "sqlite::memory:" {
        telemetry_db.close().await;
    } else {
        close_and_finalize_sqlite(telemetry_db, &telemetry_db_path).await;
    }

    close_and_finalize_sqlite(state.auth_db.clone(), &auth_db_path_str).await;
    if let Err(err) = state.gpio.reset_outputs_low() {
        log::error!("failed to reset GPIO outputs low during shutdown: {err}");
    }
    log::info!("groundstation backend shutdown complete");
    if let Some(code) = station_mode_switch::exit_code() {
        log::info!("exiting with status {code} so the supervisor starts the selected station mode");
        log::logger().flush();
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod valve_power_freshness_tests {
    use super::*;
    #[test]
    fn duplicate_packets_cannot_hide_stale_reports_and_recovery_resets_the_alarm() {
        let mut probe = ValvePowerFreshness::default();
        assert_eq!(probe.observe(0, 10, 0), None);
        assert_eq!(probe.observe(0, 10, 2500), None);
        assert_eq!(probe.check(0, 3001), Some(3001));
        assert_eq!(probe.check(0, 4000), None);
        assert_eq!(probe.observe(0, 11, 4100), None);
        assert_eq!(probe.check(0, 7101), Some(3001));
    }
    #[test]
    fn a_late_packet_cannot_erase_a_gap_before_the_watchdog_runs() {
        let mut probe = ValvePowerFreshness::default();
        probe.observe(1, 1, 100);
        assert_eq!(probe.observe(1, 2, 4100), Some(4000));
        assert_eq!(probe.check(0, 99999), None);
    }
}

#[cfg(test)]
mod validation_loadcell_observer_tests {
    use super::*;
    use sedsnet::packet::Packet;
    use std::sync::Arc;

    fn packet(name: &str, values: &[f32], source_timestamp: u64) -> Packet {
        let payload: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        Packet::new(
            telemetry_schema::data_type(name),
            &[telemetry_schema::endpoint("GROUND_STATION")],
            "DAQ",
            source_timestamp,
            Arc::from(payload),
        )
        .unwrap()
    }
    #[test]
    fn actual_captured_combined_type_is_seen_by_validation_with_original_source_identity() {
        // Actual received three-component example. This source timestamp is
        // SYNTHETIC test input, never inferred from the API's receipt clock.
        let values = [
            f32::from_bits(0x3c8d798b),
            f32::from_bits(0x4590606e),
            f32::from_bits(0x41b8cd5a),
        ];
        let p = packet("DAQ_LOADCELL_CHAMBER", &values, 987_654);
        let id = p.packet_id();
        let bytes = p.payload().to_vec();
        assert_eq!(
            validation_loadcell_source_timestamp(Some(Board::DaqBoard), &p),
            Some(987_654)
        );
        assert_eq!(p.packet_id(), id);
        assert_eq!(p.payload(), bytes);
        assert_eq!(
            validation_loadcell_source_timestamp(Some(Board::ValveBoard), &p),
            None
        );
        assert_eq!(validation_loadcell_source_timestamp(None, &p), None);
    }
    #[test]
    fn original_scalar_and_wrong_owner_controls_still_hold() {
        let p = packet("KG1000", &[1.25], 987_654);
        assert_eq!(
            validation_loadcell_source_timestamp(Some(Board::DaqBoard), &p),
            Some(987_654)
        );
        assert_eq!(
            validation_loadcell_source_timestamp(Some(Board::ValveBoard), &p),
            None
        );
        assert_eq!(validation_loadcell_source_timestamp(None, &p), None);
    }
}

#[cfg(test)]
mod recovery_pending_probe;
#[cfg(test)]
mod contract_fixtures;
#[cfg(test)]
mod radio_schema_route_tests;
#[cfg(test)]
mod av_bay_schema_contract;

mod automatic_flight_state;
#[cfg(all(test, not(feature = "firmware_sim")))]
mod normal_publication_probe;
mod automatic_startup_idle;
