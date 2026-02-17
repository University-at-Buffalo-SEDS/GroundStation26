use crate::state::AppState;
use groundstation_shared::TelemetryRow;
use groundstation_shared::{u8_to_flight_state, TelemetryCommand};
use sedsprintf_rs_2026::config::DataType;
use sedsprintf_rs_2026::config::DEVICE_IDENTIFIER;
use sedsprintf_rs_2026::timesync::{
    compute_offset_delay, decode_timesync_request, decode_timesync_response, TimeSyncConfig, TimeSyncRole,
    TimeSyncTracker, TimeSyncUpdate,
};

use crate::gpio_panel::IGNITION_PIN;
use crate::radio::RadioDevice;
use crate::rocket_commands::{ActuatorBoardCommands, FlightCommands, ValveBoardCommands};
use crate::web::{emit_warning, FlightStateMsg};
use groundstation_shared::Board;
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::error::{TryRecvError as MpscTryRecvError, TrySendError};
use tokio::sync::{broadcast, mpsc, Notify};
use tokio::time::{interval, Duration};

const TIMESYNC_PRIORITY: u64 = 50;
const TIMESYNC_SOURCE_TIMEOUT_MS: u64 = 5_000;
const TIMESYNC_ANNOUNCE_INTERVAL_MS: u64 = 1_000;
const TIMESYNC_REQUEST_INTERVAL_MS: u64 = 1_000;
const PACKET_WORK_QUEUE_SIZE: usize = 8_192;
const PACKET_ENQUEUE_BURST: usize = 256;
const DB_WORK_QUEUE_SIZE: usize = 8_192;
const BACKPRESSURE_LOG_INTERVAL_MS: u64 = 10_000;
const DB_BATCH_MAX_DEFAULT: usize = 256;
const DB_BATCH_WAIT_MS_DEFAULT: u64 = 8;

static TIMESYNC_OFFSET_MS: AtomicI64 = AtomicI64::new(0);
static DB_BACKPRESSURE_LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
static DB_BACKPRESSURE_DROPPED: AtomicU64 = AtomicU64::new(0);
static DB_LAST_BUCKET_BY_TYPE: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
static DB_OVERFLOW_LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct DbOverflow {
    queue: Arc<Mutex<VecDeque<DbWrite>>>,
    notify: Arc<Notify>,
    running: Arc<AtomicBool>,
    max_entries: usize,
}

fn env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn drop_db_writes_on_backpressure() -> bool {
    static DROP: OnceLock<bool> = OnceLock::new();
    *DROP.get_or_init(|| std::env::var("GS_DB_DROP_ON_BACKPRESSURE").ok().as_deref() == Some("1"))
}

fn db_backpressure_log_interval_ms() -> u64 {
    static INTERVAL: OnceLock<u64> = OnceLock::new();
    *INTERVAL.get_or_init(|| {
        std::env::var("GS_DB_BACKPRESSURE_LOG_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60_000)
            .clamp(1_000, 3_600_000)
    })
}

fn db_backpressure_log_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GS_DB_BACKPRESSURE_LOG").ok().as_deref() != Some("0"))
}

fn db_bucket_ms() -> i64 {
    static BUCKET_MS: OnceLock<i64> = OnceLock::new();
    *BUCKET_MS.get_or_init(|| {
        std::env::var("GS_DB_BUCKET_MS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(20)
            .clamp(1, 1_000)
    })
}

fn should_persist_telemetry_sample(data_type: &str, ts_ms: i64) -> bool {
    let bucket_ms = db_bucket_ms();
    if bucket_ms <= 1 {
        return true;
    }
    let bucket_id = ts_ms.div_euclid(bucket_ms);
    let map = DB_LAST_BUCKET_BY_TYPE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut by_type = map.lock().unwrap();
    match by_type.get(data_type).copied() {
        Some(prev) if prev == bucket_id => false,
        _ => {
            by_type.insert(data_type.to_string(), bucket_id);
            true
        }
    }
}

enum DbWrite {
    FlightState {
        timestamp_ms: i64,
        state_code: i64,
    },
    Telemetry {
        timestamp_ms: i64,
        data_type: String,
        values_json: Option<String>,
        payload_json: String,
    },
}

pub struct TimeSyncState {
    tracker: TimeSyncTracker,
    next_seq: u64,
    pending: Option<(u64, u64)>,
    last_request_ms: u64,
    last_announce_ms: u64,
    last_offset_ms: Option<i64>,
    last_delay_ms: Option<u64>,
}

impl TimeSyncState {
    fn new() -> Self {
        Self {
            tracker: TimeSyncTracker::new(TimeSyncConfig {
                role: TimeSyncRole::Auto,
                priority: TIMESYNC_PRIORITY,
                source_timeout_ms: TIMESYNC_SOURCE_TIMEOUT_MS,
            }),
            next_seq: 1,
            pending: None,
            last_request_ms: 0,
            last_announce_ms: 0,
            last_offset_ms: None,
            last_delay_ms: None,
        }
    }

    fn mark_request(&mut self, seq: u64, t1_ms: u64, now_ms: u64) {
        self.pending = Some((seq, t1_ms));
        self.last_request_ms = now_ms;
    }

    fn clear_pending(&mut self) {
        self.pending = None;
    }
}

pub async fn telemetry_task(
    state: Arc<AppState>,
    router: Arc<sedsprintf_rs_2026::router::Router>,
    radio: Vec<Arc<Mutex<Box<dyn RadioDevice>>>>,
    mut rx: mpsc::Receiver<TelemetryCommand>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut radio_interval = interval(Duration::from_millis(10));
    let mut handle_interval = interval(Duration::from_millis(1));
    let mut router_interval = interval(Duration::from_millis(10));
    let mut heartbeat_interval = interval(Duration::from_millis(500));
    let mut timesync_interval = interval(Duration::from_millis(100));
    let mut heartbeat_failed = false;
    let mut last_backpressure_log_ms: u64 = 0;
    let timesync_state = Arc::new(Mutex::new(TimeSyncState::new()));
    let packet_work_queue_size = env_usize(
        "GS_PACKET_WORK_QUEUE_SIZE",
        PACKET_WORK_QUEUE_SIZE,
        1024,
        262_144,
    );
    let db_work_queue_size = env_usize("GS_DB_WORK_QUEUE_SIZE", DB_WORK_QUEUE_SIZE, 1024, 262_144);
    let packet_enqueue_burst = env_usize("GS_PACKET_ENQUEUE_BURST", PACKET_ENQUEUE_BURST, 32, 4096);
    let (packet_tx, mut packet_rx) = mpsc::channel::<TelemetryPacket>(packet_work_queue_size);
    let (db_tx, mut db_rx) = mpsc::channel::<DbWrite>(db_work_queue_size);
    let db_overflow = DbOverflow {
        queue: Arc::new(Mutex::new(VecDeque::new())),
        notify: Arc::new(Notify::new()),
        running: Arc::new(AtomicBool::new(true)),
        max_entries: env_usize("GS_DB_OVERFLOW_MAX", 250_000, 1024, 5_000_000),
    };

    let db_worker = {
        let db = state.db.clone();
        let db_batch_max = env_usize("GS_DB_BATCH_MAX", DB_BATCH_MAX_DEFAULT, 1, 4096);
        let db_batch_wait_ms = std::env::var("GS_DB_BATCH_WAIT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DB_BATCH_WAIT_MS_DEFAULT)
            .clamp(1, 250);
        tokio::spawn(async move {
            while let Some(first) = db_rx.recv().await {
                let mut batch: Vec<DbWrite> = Vec::with_capacity(db_batch_max);
                batch.push(first);
                let deadline =
                    tokio::time::Instant::now() + Duration::from_millis(db_batch_wait_ms);

                while batch.len() < db_batch_max {
                    match db_rx.try_recv() {
                        Ok(write) => batch.push(write),
                        Err(MpscTryRecvError::Disconnected) => break,
                        Err(MpscTryRecvError::Empty) => {
                            let now = tokio::time::Instant::now();
                            if now >= deadline {
                                break;
                            }
                            let remaining = deadline.saturating_duration_since(now);
                            match tokio::time::timeout(remaining, db_rx.recv()).await {
                                Ok(Some(write)) => batch.push(write),
                                Ok(None) => break,
                                Err(_) => break,
                            }
                        }
                    }
                }

                if let Err(e) = insert_db_batch_with_retry(&db, &batch).await {
                    eprintln!("DB insert failed after retry: {e}");
                }
            }
        })
    };

    let db_overflow_worker = {
        let db_tx = db_tx.clone();
        let db_overflow = db_overflow.clone();
        tokio::spawn(async move {
            while db_overflow.running.load(Ordering::Relaxed) {
                db_overflow.notify.notified().await;
                loop {
                    let next = {
                        let mut q = db_overflow.queue.lock().unwrap();
                        q.pop_front()
                    };
                    let Some(write) = next else {
                        break;
                    };
                    if db_tx.send(write).await.is_err() {
                        return;
                    }
                }
            }
        })
    };

    let packet_worker = {
        let state = state.clone();
        let router = router.clone();
        let timesync_state = timesync_state.clone();
        let db_tx = db_tx.clone();
        let db_overflow = db_overflow.clone();
        tokio::spawn(async move {
            while let Some(pkt) = packet_rx.recv().await {
                if let Some(row) =
                    handle_packet(&state, &router, &timesync_state, &db_tx, &db_overflow, pkt).await
                {
                    let _ = state.ws_tx.send(row);
                }
            }
        })
    };

    loop {
        tokio::select! {
                _ = radio_interval.tick() => {
                    for radio in &radio {
                        match radio.lock().expect("failed to get lock").recv_packet(&router){
                            Ok(_) => {
                                // Packet received and handled by router
                            }
                            Err(e) => {
                                log_telemetry_error("radio_task recv_packet failed", e);
                            }
                        }
                    }
                }
            _= router_interval.tick() => {
                    if let Err(e) = router.process_all_queues_with_timeout(20) {
                        log_telemetry_error("router queue processing failed", e);
                    }
                }
                Some(cmd) = rx.recv() => {
                    match cmd {
                        TelemetryCommand::Launch => {
                                if let Err(e) = router.log_queue(
                                    DataType::FlightCommand,
                                    &[FlightCommands::Launch as u8],
                                ) {
                                    log_telemetry_error("failed to log Launch command", e);
                                }
                                let gpio = &state.gpio;
                                gpio.write_output_pin(IGNITION_PIN, true).expect("failed to set gpio output");
                                println!("Launch command sent");

                            }
                        TelemetryCommand::Dump => {
                                let key = ValveBoardCommands::DumpOpen as u8;
                                let is_on = state.get_umbilical_valve_state(key).unwrap_or(false);
                                let cmd = if is_on {
                                    ValveBoardCommands::DumpClose
                                } else {
                                    ValveBoardCommands::DumpOpen
                                };
                                if let Err(e) = router.log_queue(
                                    DataType::ValveCommand,
                                    &[cmd as u8],
                                ) {
                                    log_telemetry_error("failed to log Dump command", e);
                                }
                                {
                                    let gpio = &state.gpio;
                                    gpio.write_output_pin(IGNITION_PIN, false).expect("failed to set gpio output");
                                }
                                println!("Dump command sent {:?}", cmd);
                            }
                        TelemetryCommand::Abort => {
                                if let Err(e) = router.log(
                                    DataType::Abort,
                                    "Manual Abort Command Issued".as_ref(),
                                ) {
                                    log_telemetry_error("failed to log Abort command", e);
                                }
                                println!("Abort command sent");
                            }
                        TelemetryCommand::Igniter => {
                                let key = ActuatorBoardCommands::IgniterOn as u8;
                                let is_on = state.get_umbilical_valve_state(key).unwrap_or(false);
                                let cmd = if is_on {
                                    ActuatorBoardCommands::IgniterOff
                                } else {
                                    ActuatorBoardCommands::IgniterOn
                                };
                                if let Err(e) = router.log_queue(
                                    DataType::ActuatorCommand,
                                    &[cmd as u8],
                                ) {
                                    log_telemetry_error("failed to log Igniter command", e);
                                }
                                println!("Igniter command sent {:?}", cmd);
                            }
                        TelemetryCommand::Pilot => {
                                let key = ValveBoardCommands::PilotOpen as u8;
                                let is_on = state.get_umbilical_valve_state(key).unwrap_or(false);
                                let cmd = if is_on {
                                    ValveBoardCommands::PilotClose
                                } else {
                                    ValveBoardCommands::PilotOpen
                                };
                                if let Err(e) = router.log_queue(
                                    DataType::ValveCommand,
                                    &[cmd as u8],
                                ) {
                                    log_telemetry_error("failed to log Pilot command", e);
                                }
                                println!("Pilot command sent {:?}", cmd);
                            }
                        TelemetryCommand::NormallyOpen => {
                                let key = ValveBoardCommands::NormallyOpenOpen as u8;
                                let is_on = state.get_umbilical_valve_state(key).unwrap_or(false);
                                let cmd = if is_on {
                                    ValveBoardCommands::NormallyOpenClose
                                } else {
                                    ValveBoardCommands::NormallyOpenOpen
                                };
                                if let Err(e) = router.log_queue(
                                    DataType::ValveCommand,
                                    &[cmd as u8],
                                ) {
                                    log_telemetry_error("failed to log NormallyOpen command", e);
                                }
                                println!("Tanks command sent {:?}", cmd);
                            }
                        TelemetryCommand::Nitrogen => {
                                let cmd_id = ActuatorBoardCommands::NitrogenOpen as u8;
                                let is_on = state.get_umbilical_valve_state(cmd_id).unwrap_or(false);
                                let cmd = if is_on {
                                    ActuatorBoardCommands::NitrogenClose
                                } else {
                                    ActuatorBoardCommands::NitrogenOpen
                                };
                                if let Err(e) = router.log_queue(
                                    DataType::ActuatorCommand,
                                    &[cmd as u8],
                                ) {
                                    log_telemetry_error("failed to log Nitrogen command", e);
                                }
                                println!("Nitrogen command sent {:?}", cmd);
                            }
                        TelemetryCommand::RetractPlumbing => {
                                if let Err(e) = router.log_queue(
                                    DataType::ActuatorCommand,
                                    &[ActuatorBoardCommands::RetractPlumbing as u8],
                                ) {
                                    log_telemetry_error("failed to log RetractPlumbing command", e);
                                }
                                println!("RetractPlumbing command sent");
                        }
                        TelemetryCommand::Nitrous => {
                                let cmd_id = ActuatorBoardCommands::NitrousOpen as u8;
                                let is_on = state.get_umbilical_valve_state(cmd_id).unwrap_or(false);
                                let cmd = if is_on {
                                    ActuatorBoardCommands::NitrousClose
                                } else {
                                    ActuatorBoardCommands::NitrousOpen
                                };
                                if let Err(e) = router.log_queue(
                                    DataType::ActuatorCommand,
                                    &[cmd as u8],
                                ) {
                                    log_telemetry_error("failed to log Nitrous command", e);
                                }
                                println!("Nitrous command sent: {:?}", cmd);
                        }
                    }
                }
                _ = heartbeat_interval.tick() => {
                    if router.log_queue::<u8>(DataType::Heartbeat, &[]).is_ok() {
                        state.mark_board_seen(
                            Board::GroundStation.sender_id(),
                            get_current_timestamp_ms(),
                        );
                        heartbeat_failed = false;
                    } else if !heartbeat_failed {
                            emit_warning(
                                &state,
                                "Warning: Ground Station heartbeat send failed",
                            );
                            heartbeat_failed = true;

                    }
                }
                _ = handle_interval.tick() => {
                    for _ in 0..packet_enqueue_burst {
                        match packet_tx.try_reserve() {
                            Ok(permit) => {
                                let pkt = {
                                    let mut rb = state.ring_buffer.lock().unwrap();
                                    rb.pop_oldest()
                                };
                                let Some(pkt) = pkt else {
                                    break;
                                };
                                permit.send(pkt);
                            }
                            Err(TrySendError::Full(_)) => {
                                let now_ms = get_current_timestamp_ms();
                                if now_ms.saturating_sub(last_backpressure_log_ms)
                                    >= BACKPRESSURE_LOG_INTERVAL_MS
                                {
                                    eprintln!(
                                        "Telemetry ingest backpressured: processing queue is full"
                                    );
                                    last_backpressure_log_ms = now_ms;
                                }
                                break;
                            }
                            Err(TrySendError::Closed(_)) => {
                                emit_warning(
                                    &state,
                                    "Warning: telemetry processing worker stopped unexpectedly",
                                );
                                break;
                            }
                        }
                    }
                }
                _ = timesync_interval.tick() => {
                    if timesync_enabled() {
                        handle_timesync_tick(&router, &timesync_state);
                    }
                }
                recv = shutdown_rx.recv() => {
                    match recv {
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
        }
    }

    let worker_shutdown_timeout = Duration::from_secs(10);

    // Stop intake first, then wait for packet worker to drain packet queue.
    drop(packet_tx);
    match tokio::time::timeout(worker_shutdown_timeout, packet_worker).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("Packet worker ended with error: {e}"),
        Err(_) => eprintln!(
            "Packet worker did not shut down within {:?}",
            worker_shutdown_timeout
        ),
    }

    db_overflow.running.store(false, Ordering::Relaxed);
    db_overflow.notify.notify_waiters();
    match tokio::time::timeout(worker_shutdown_timeout, db_overflow_worker).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("DB overflow worker ended with error: {e}"),
        Err(_) => eprintln!(
            "DB overflow worker did not shut down within {:?}",
            worker_shutdown_timeout
        ),
    }

    // Packet worker is done producing DB writes; now drain DB queue.
    drop(db_tx);
    match tokio::time::timeout(worker_shutdown_timeout, db_worker).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("DB worker ended with error: {e}"),
        Err(_) => eprintln!(
            "DB worker did not shut down within {:?}",
            worker_shutdown_timeout
        ),
    }
}

fn umbilical_state_key(cmd_id: u8, on: bool) -> Option<(u8, bool)> {
    use ActuatorBoardCommands as A;
    use ValveBoardCommands as V;

    match cmd_id {
        x if x == V::PilotOpen as u8 => Some((V::PilotOpen as u8, on)),
        x if x == V::PilotClose as u8 => Some((V::PilotOpen as u8, false)),
        x if x == V::NormallyOpenOpen as u8 => Some((V::NormallyOpenOpen as u8, on)),
        x if x == V::NormallyOpenClose as u8 => Some((V::NormallyOpenOpen as u8, false)),
        x if x == V::DumpOpen as u8 => Some((V::DumpOpen as u8, on)),
        x if x == V::DumpClose as u8 => Some((V::DumpOpen as u8, false)),
        x if x == A::IgniterOn as u8 => Some((A::IgniterOn as u8, on)),
        x if x == A::IgniterOff as u8 => Some((A::IgniterOn as u8, false)),
        x if x == A::NitrogenOpen as u8 => Some((A::NitrogenOpen as u8, on)),
        x if x == A::NitrogenClose as u8 => Some((A::NitrogenOpen as u8, false)),
        x if x == A::NitrousOpen as u8 => Some((A::NitrousOpen as u8, on)),
        x if x == A::NitrousClose as u8 => Some((A::NitrousOpen as u8, false)),
        x if x == A::RetractPlumbing as u8 => Some((A::RetractPlumbing as u8, on)),
        _ => None,
    }
}

const VALVE_STATE_DATA_TYPE: &str = "VALVE_STATE";

fn bool_to_f32(value: Option<bool>) -> Option<f32> {
    value.map(|v| if v { 1.0 } else { 0.0 })
}

fn valve_state_values(state: &AppState) -> [Option<f32>; 8] {
    use ActuatorBoardCommands as A;
    use ValveBoardCommands as V;

    [
        bool_to_f32(state.get_umbilical_valve_state(V::PilotOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(V::NormallyOpenOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(V::DumpOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(A::IgniterOn as u8)),
        bool_to_f32(state.get_umbilical_valve_state(A::NitrogenOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(A::NitrousOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(A::RetractPlumbing as u8)),
        None,
    ]
}

const DB_RETRIES: usize = 5;
const DB_RETRY_DELAY_MS: u64 = 50;

async fn insert_db_batch_once(
    db: &sqlx::SqlitePool,
    writes: &[DbWrite],
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    for write in writes {
        match write {
            DbWrite::FlightState {
                timestamp_ms,
                state_code,
            } => {
                sqlx::query("INSERT INTO flight_state (timestamp_ms, f_state) VALUES (?, ?)")
                    .bind(*timestamp_ms)
                    .bind(*state_code)
                    .execute(&mut *tx)
                    .await?;
            }
            DbWrite::Telemetry {
                timestamp_ms,
                data_type,
                values_json,
                payload_json,
            } => {
                sqlx::query(
                    "INSERT INTO telemetry (timestamp_ms, data_type, values_json, payload_json) VALUES (?, ?, ?, ?)",
                )
                    .bind(*timestamp_ms)
                    .bind(data_type.as_str())
                    .bind(values_json.as_deref())
                    .bind(payload_json.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    tx.commit().await
}

async fn insert_db_batch_with_retry(
    db: &sqlx::SqlitePool,
    writes: &[DbWrite],
) -> Result<(), sqlx::Error> {
    let mut delay = DB_RETRY_DELAY_MS;
    let mut last_err: Option<sqlx::Error> = None;

    for _ in 0..=DB_RETRIES {
        let result = insert_db_batch_once(db, writes).await;
        match result {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                delay = (delay * 2).min(1000);
            }
        }
    }

    Err(last_err.unwrap())
}

async fn queue_db_write(
    state: &AppState,
    db_tx: &mpsc::Sender<DbWrite>,
    db_overflow: &DbOverflow,
    write: DbWrite,
) {
    if drop_db_writes_on_backpressure() {
        match db_tx.try_send(write) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                DB_BACKPRESSURE_DROPPED.fetch_add(1, Ordering::Relaxed);
                if !db_backpressure_log_enabled() {
                    return;
                }
                let now_ms = get_current_timestamp_ms();
                let prev = DB_BACKPRESSURE_LAST_LOG_MS.load(Ordering::Relaxed);
                if now_ms.saturating_sub(prev) >= db_backpressure_log_interval_ms() {
                    DB_BACKPRESSURE_LAST_LOG_MS.store(now_ms, Ordering::Relaxed);
                    let dropped = DB_BACKPRESSURE_DROPPED.swap(0, Ordering::Relaxed);
                    eprintln!(
                        "Telemetry DB backpressured: dropped {} DB rows (realtime ingest preserved)",
                        dropped
                    );
                }
            }
            Err(TrySendError::Closed(_)) => {
                emit_warning(state, "Warning: telemetry DB worker stopped unexpectedly");
            }
        }
        return;
    }

    match db_tx.try_send(write) {
        Ok(()) => {}
        Err(TrySendError::Full(write)) => {
            let mut write_opt = Some(write);
            let mut queued_len = 0usize;
            let mut pushed = false;
            {
                let mut q = db_overflow.queue.lock().unwrap();
                if q.len() < db_overflow.max_entries {
                    q.push_back(write_opt.take().unwrap());
                    queued_len = q.len();
                    pushed = true;
                }
            }
            if pushed {
                db_overflow.notify.notify_one();
                let now_ms = get_current_timestamp_ms();
                let prev = DB_OVERFLOW_LAST_LOG_MS.load(Ordering::Relaxed);
                if now_ms.saturating_sub(prev) >= 60_000 {
                    DB_OVERFLOW_LAST_LOG_MS.store(now_ms, Ordering::Relaxed);
                    eprintln!(
                        "Telemetry DB overflow queue buffered {} pending rows (no drop mode)",
                        queued_len
                    );
                }
            } else if db_tx.send(write_opt.take().unwrap()).await.is_err() {
                emit_warning(state, "Warning: telemetry DB worker stopped unexpectedly");
            }
        }
        Err(TrySendError::Closed(_)) => {
            emit_warning(state, "Warning: telemetry DB worker stopped unexpectedly");
        }
    }
}

async fn handle_packet(
    state: &Arc<AppState>,
    router: &Arc<sedsprintf_rs_2026::router::Router>,
    timesync_state: &Arc<Mutex<TimeSyncState>>,
    db_tx: &mpsc::Sender<DbWrite>,
    db_overflow: &DbOverflow,
    pkt: TelemetryPacket,
) -> Option<TelemetryRow> {
    state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());

    if pkt.data_type() == DataType::Warning {
        if let Ok(msg) = pkt.data_as_string() {
            emit_warning(state, msg.to_string());
        } else {
            emit_warning(state, "Warning packet with invalid UTF-8 payload");
        }
        return None;
    }

    if handle_timesync_packet(router, timesync_state, &pkt) {
        return None;
    }

    if pkt.data_type() == DataType::FlightState {
        if !cfg!(feature = "testing") && !state.all_boards_seen() {
            return None;
        }
        let pkt_data = match pkt.data_as_u8() {
            Ok(data) => *data.first().expect("index 0 does not exist"),
            Err(_) => return None,
        };
        let new_flight_state = match u8_to_flight_state(pkt_data) {
            Some(flight_state) => flight_state,
            None => return None,
        };
        {
            let mut fs = state.state.lock().unwrap();
            *fs = new_flight_state;
        }
        let ts_ms = get_current_timestamp_ms() as i64;
        queue_db_write(
            state,
            db_tx,
            db_overflow,
            DbWrite::FlightState {
                timestamp_ms: ts_ms,
                state_code: pkt_data as i64,
            },
        )
            .await;

        let _ = state.state_tx.send(FlightStateMsg {
            state: new_flight_state,
        });
        return None;
    }

    if pkt.data_type() == DataType::UmbilicalStatus {
        if let Ok(data) = pkt.data_as_u8()
            && data.len() == 2
        {
            let cmd_id = data[0];
            let on = data[1] != 0;
            if let Some((key_cmd_id, key_on)) = umbilical_state_key(cmd_id, on) {
                state.set_umbilical_valve_state(key_cmd_id, key_on);

                let ts_ms = pkt.timestamp() as i64;
                let values = valve_state_values(state);
                let values_vec: Vec<Option<f32>> = values.into_iter().collect();
                let values_json = serde_json::to_string(
                    &values_vec
                        .iter()
                        .map(|v| v.map(|n| n as f64))
                        .collect::<Vec<_>>(),
                )
                    .ok();
                let payload_json = payload_json_from_pkt(&pkt);

                queue_db_write(
                    state,
                    db_tx,
                    db_overflow,
                    DbWrite::Telemetry {
                        timestamp_ms: ts_ms,
                        data_type: VALVE_STATE_DATA_TYPE.to_string(),
                        values_json,
                        payload_json,
                    },
                )
                    .await;

                let row = TelemetryRow {
                    timestamp_ms: ts_ms,
                    data_type: VALVE_STATE_DATA_TYPE.to_string(),
                    values: values_vec,
                };
                return Some(row);
            }
        }
        return None;
    }

    let ts_ms = pkt.timestamp() as i64;
    let data_type_str = pkt.data_type().as_str().to_string();

    let payload_json = payload_json_from_pkt(&pkt);

    if let Ok(values) = pkt.data_as_f32() {
        let values_vec: Vec<Option<f32>> = values.into_iter().map(Some).collect();
        let values_json = serde_json::to_string(
            &values_vec
                .iter()
                .map(|v| v.map(|n| n as f64))
                .collect::<Vec<_>>(),
        )
            .ok();

        if should_persist_telemetry_sample(&data_type_str, ts_ms) {
            queue_db_write(
                state,
                db_tx,
                db_overflow,
                DbWrite::Telemetry {
                    timestamp_ms: ts_ms,
                    data_type: data_type_str.clone(),
                    values_json,
                    payload_json: payload_json.clone(),
                },
            )
                .await;
        }

        let row = TelemetryRow {
            timestamp_ms: ts_ms,
            data_type: data_type_str,
            values: values_vec,
        };

        Some(row)
    } else {
        if should_persist_telemetry_sample(&data_type_str, ts_ms) {
            queue_db_write(
                state,
                db_tx,
                db_overflow,
                DbWrite::Telemetry {
                    timestamp_ms: ts_ms,
                    data_type: data_type_str,
                    values_json: None,
                    payload_json,
                },
            )
                .await;
        }
        None
    }
}

pub fn get_current_timestamp_ms() -> u64 {
    let raw = get_system_timestamp_ms();
    let offset = TIMESYNC_OFFSET_MS.load(Ordering::Relaxed);
    if offset >= 0 {
        raw.saturating_add(offset as u64)
    } else {
        raw.saturating_sub((-offset) as u64)
    }
}

fn get_system_timestamp_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now();
    let duration_since_epoch = now.duration_since(UNIX_EPOCH).unwrap();
    duration_since_epoch.as_millis() as u64
}

fn log_telemetry_error(context: &str, err: sedsprintf_rs_2026::TelemetryError) {
    eprintln!("{context}: {:?}", err);
}

fn payload_json_from_pkt(pkt: &sedsprintf_rs_2026::telemetry_packet::TelemetryPacket) -> String {
    let bytes = pkt.payload();
    serde_json::to_string(&bytes).unwrap_or_else(|_| "[]".to_string())
}

fn handle_timesync_tick(
    router: &Arc<sedsprintf_rs_2026::router::Router>,
    timesync_state: &Arc<Mutex<TimeSyncState>>,
) {
    let now_ms = get_system_timestamp_ms();
    let mut ts = timesync_state.lock().unwrap();

    if ts.tracker.refresh(now_ms) == TimeSyncUpdate::SourceChanged {
        ts.clear_pending();
    }

    if ts.tracker.should_announce(now_ms) {
        if now_ms.saturating_sub(ts.last_announce_ms) >= TIMESYNC_ANNOUNCE_INTERVAL_MS {
            let _ = queue_timesync_announce(
                router,
                ts.tracker.config().priority,
                get_current_timestamp_ms(),
            );
            ts.last_announce_ms = now_ms;
        }
        return;
    }

    if ts.tracker.current_source().is_some()
        && ts.pending.is_none()
        && now_ms.saturating_sub(ts.last_request_ms) >= TIMESYNC_REQUEST_INTERVAL_MS
    {
        let seq = ts.next_seq;
        ts.next_seq = ts.next_seq.wrapping_add(1);
        let t1_ms = get_system_timestamp_ms();
        if queue_timesync_request(router, seq, t1_ms).is_ok() {
            ts.mark_request(seq, t1_ms, now_ms);
        }
    }
}

fn handle_timesync_packet(
    router: &Arc<sedsprintf_rs_2026::router::Router>,
    timesync_state: &Arc<Mutex<TimeSyncState>>,
    pkt: &sedsprintf_rs_2026::telemetry_packet::TelemetryPacket,
) -> bool {
    if !timesync_enabled() {
        return false;
    }

    if pkt.sender() == DEVICE_IDENTIFIER {
        return true;
    }

    match pkt.data_type() {
        DataType::TimeSyncAnnounce => {
            let now_ms = get_system_timestamp_ms();
            let mut ts = timesync_state.lock().unwrap();
            if ts.tracker.handle_announce(pkt, now_ms).is_ok() {
                return true;
            }
            true
        }
        DataType::TimeSyncRequest => {
            let now_ms = get_system_timestamp_ms();
            let ts = timesync_state.lock().unwrap();
            if !ts.tracker.should_announce(now_ms) {
                return true;
            }
            let req = match decode_timesync_request(pkt) {
                Ok(req) => req,
                Err(_) => return true,
            };
            let t2_ms = get_current_timestamp_ms();
            let t3_ms = get_current_timestamp_ms();
            let _ = queue_timesync_response(router, req.seq, req.t1_ms, t2_ms, t3_ms);
            true
        }
        DataType::TimeSyncResponse => {
            let now_ms = get_system_timestamp_ms();
            let mut ts = timesync_state.lock().unwrap();
            let resp = match decode_timesync_response(pkt) {
                Ok(resp) => resp,
                Err(_) => return true,
            };
            let Some((pending_seq, t1_ms)) = ts.pending else {
                return true;
            };
            if pending_seq != resp.seq {
                return true;
            }
            if let Some(source) = ts.tracker.current_source() {
                if source.sender != pkt.sender() {
                    return true;
                }
            } else {
                return true;
            }
            let sample = compute_offset_delay(t1_ms, resp.t2_ms, resp.t3_ms, now_ms);
            TIMESYNC_OFFSET_MS.store(sample.offset_ms, Ordering::Relaxed);
            ts.last_offset_ms = Some(sample.offset_ms);
            ts.last_delay_ms = Some(sample.delay_ms);
            ts.clear_pending();
            true
        }
        _ => false,
    }
}

fn timesync_enabled() -> bool {
    if cfg!(feature = "testing") {
        return std::env::var("GROUNDSTATION_TIMESYNC").ok().as_deref() == Some("1");
    }
    true
}

fn queue_timesync_announce(
    router: &sedsprintf_rs_2026::router::Router,
    priority: u64,
    time_ms: u64,
) -> sedsprintf_rs_2026::TelemetryResult<()> {
    router.log_queue_ts(DataType::TimeSyncAnnounce, time_ms, &[priority, time_ms])
}

fn queue_timesync_request(
    router: &sedsprintf_rs_2026::router::Router,
    seq: u64,
    t1_ms: u64,
) -> sedsprintf_rs_2026::TelemetryResult<()> {
    router.log_queue_ts(DataType::TimeSyncRequest, t1_ms, &[seq, t1_ms])
}

fn queue_timesync_response(
    router: &sedsprintf_rs_2026::router::Router,
    seq: u64,
    t1_ms: u64,
    t2_ms: u64,
    t3_ms: u64,
) -> sedsprintf_rs_2026::TelemetryResult<()> {
    router.log_queue_ts(
        DataType::TimeSyncResponse,
        t3_ms,
        &[seq, t1_ms, t2_ms, t3_ms],
    )
}
