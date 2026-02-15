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
use crate::web::{emit_warning, emit_warning_db_only, FlightStateMsg};
use groundstation_shared::Board;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, Duration};

const TIMESYNC_PRIORITY: u64 = 50;
const TIMESYNC_SOURCE_TIMEOUT_MS: u64 = 5_000;
const TIMESYNC_ANNOUNCE_INTERVAL_MS: u64 = 1_000;
const TIMESYNC_REQUEST_INTERVAL_MS: u64 = 1_000;

static TIMESYNC_OFFSET_MS: AtomicI64 = AtomicI64::new(0);

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
    let mut radio_interval = interval(Duration::from_millis(2));
    let mut handle_interval = interval(Duration::from_millis(1));
    let mut router_interval = interval(Duration::from_millis(10));
    let mut heartbeat_interval = interval(Duration::from_millis(500));
    let mut timesync_interval = interval(Duration::from_millis(100));
    let mut heartbeat_failed = false;
    let timesync_state = Arc::new(Mutex::new(TimeSyncState::new()));

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
                            emit_warning_db_only(
                                &state,
                                "Warning: Ground Station heartbeat send failed",
                            );
                            heartbeat_failed = true;

                    }
                }
                _ = handle_interval.tick() => {
                    handle_packet(&state, &router, &timesync_state).await;
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

async fn insert_with_retry<F, Fut>(mut f: F) -> Result<(), sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error>>,
{
    let mut delay = DB_RETRY_DELAY_MS;
    let mut last_err: Option<sqlx::Error> = None;

    for _ in 0..=DB_RETRIES {
        match f().await {
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

pub async fn handle_packet(
    state: &Arc<AppState>,
    router: &Arc<sedsprintf_rs_2026::router::Router>,
    timesync_state: &Arc<Mutex<TimeSyncState>>,
) {
    // Keep raw packet in ring buffer if you still want it
    let pkt = {
        //get the most recent packet from the ring buffer
        let mut rb = state.ring_buffer.lock().unwrap();
        match rb.pop_oldest() {
            Some(pkt) => pkt,
            None => return, // No packet to process
        }
    };

    state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());

    if pkt.data_type() == DataType::Warning {
        if let Ok(msg) = pkt.data_as_string() {
            emit_warning(state, msg.to_string());
        } else {
            emit_warning(state, "Warning packet with invalid UTF-8 payload");
        }
        return;
    }

    if handle_timesync_packet(router, timesync_state, &pkt) {
        return;
    }

    if pkt.data_type() == DataType::FlightState {
        if !cfg!(feature = "testing") && !state.all_boards_seen() {
            return;
        }
        let pkt_data = match pkt.data_as_u8() {
            Ok(data) => *data.first().expect("index 0 does not exist"),
            Err(_) => return,
        };
        let new_flight_state = match u8_to_flight_state(pkt_data) {
            Some(flight_state) => flight_state,
            None => return,
        };
        {
            let mut fs = state.state.lock().unwrap();
            *fs = new_flight_state;
        }
        let ts_ms = get_current_timestamp_ms() as i64;
        if let Err(e) = insert_with_retry(|| {
            sqlx::query("INSERT INTO flight_state (timestamp_ms, f_state) VALUES (?, ?)")
                .bind(ts_ms)
                .bind(pkt_data as i64)
                .execute(&state.db)
        })
        .await
        {
            eprintln!("DB insert into flight_state failed after retry: {e}");
        }

        let _ = state.state_tx.send(FlightStateMsg {
            state: new_flight_state,
        });
        return;
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

                if let Err(e) = insert_with_retry(|| {
                    sqlx::query(
                        "INSERT INTO telemetry (timestamp_ms, data_type, values_json, payload_json) VALUES (?, ?, ?, ?)",
                    )
                        .bind(ts_ms)
                        .bind(VALVE_STATE_DATA_TYPE)
                        .bind(values_json.as_deref())
                        .bind(payload_json.as_str())
                        .execute(&state.db)
                })
                    .await
                {
                    eprintln!("DB insert into telemetry failed after retry: {e}");
                }

                let row = TelemetryRow {
                    timestamp_ms: ts_ms,
                    data_type: VALVE_STATE_DATA_TYPE.to_string(),
                    values: values_vec,
                };
                let _ = state.ws_tx.send(row);
            }
        }
        return;
    }

    let ts_ms = pkt.timestamp() as i64;
    let data_type_str = pkt.data_type().as_str().to_string();

    let payload_json = payload_json_from_pkt(&pkt);

    if let Ok(values) = pkt.data_as_f32() {
        let values_vec: Vec<Option<f32>> = values.into_iter().map(Some).collect();
        let values_json = serde_json::to_string(
            &values_vec.iter().map(|v| v.map(|n| n as f64)).collect::<Vec<_>>(),
        )
            .ok();

        if let Err(e) = insert_with_retry(|| {
            sqlx::query(
                "INSERT INTO telemetry (timestamp_ms, data_type, values_json, payload_json) VALUES (?, ?, ?, ?)",
            )
                .bind(ts_ms)
                .bind(&data_type_str)
                .bind(values_json.as_deref())
                .bind(payload_json.as_str())
                .execute(&state.db)
        })
            .await
        {
            eprintln!("DB insert into telemetry failed after retry: {e}");
        }

        let row = TelemetryRow {
            timestamp_ms: ts_ms,
            data_type: data_type_str,
            values: values_vec,
        };

        let _ = state.ws_tx.send(row);
    } else if let Err(e) = insert_with_retry(|| {
        sqlx::query(
            "INSERT INTO telemetry (timestamp_ms, data_type, values_json, payload_json) VALUES (?, ?, ?, ?)",
        )
            .bind(ts_ms)
            .bind(&data_type_str)
            .bind(Option::<String>::None)
            .bind(payload_json.as_str())
            .execute(&state.db)
    })
        .await
    {
        eprintln!("DB insert into telemetry failed after retry: {e}");
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
