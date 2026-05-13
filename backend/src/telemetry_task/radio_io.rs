use crate::comms::{CommsDevice, RadioWindowKind};
use crate::state::AppState;
use sedsprintf_rs_2026::config::{DataEndpoint, DataType};
use sedsprintf_rs_2026::packet::Packet;
use sedsprintf_rs_2026::router::{Router, RouterSideId};
use sedsprintf_rs_2026::serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Duration;

use super::{
    COMMS_ERROR_LOG_INTERVAL_MS, env_usize, get_current_timestamp_ms, log_telemetry_error,
    process_router_queues, timesync_enabled,
};

pub struct CommsWorkerHandle {
    pub name: &'static str,
    pub comms: Arc<Mutex<Box<dyn CommsDevice>>>,
    pub tx_comms: Option<Arc<Mutex<Box<dyn CommsDevice>>>>,
    pub side_id: RouterSideId,
    pub tx_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    pub legacy_single_worker: bool,
    pub prioritize_rx: bool,
    pub dedicated_radio_io: bool,
}

const COMMS_TX_BURST: usize = 1;
const COMMS_TX_GAP_MS: u64 = 10;
const COMMS_IDLE_SLEEP_MS: u64 = 1;
pub(super) fn spawn_comms_worker_threads(
    router: Arc<Router>,
    state: Arc<AppState>,
    mut comms_handle: CommsWorkerHandle,
) -> std::io::Result<Vec<thread::JoinHandle<()>>> {
    if comms_handle.dedicated_radio_io {
        return spawn_dedicated_radio_io_threads(router, state, comms_handle);
    }
    if comms_handle.legacy_single_worker {
        return spawn_legacy_comms_worker_thread(router, state, comms_handle);
    }
    if comms_handle.prioritize_rx {
        return spawn_rx_priority_comms_worker_thread(router, state, comms_handle);
    }

    let worker_name = comms_handle.name;
    let comms = comms_handle.comms;
    let tx_worker_state = state.clone();
    let tx_worker_comms = comms_handle
        .tx_comms
        .clone()
        .unwrap_or_else(|| comms.clone());
    let tx_worker = thread::Builder::new()
        .name(format!("{}_comms_tx", worker_name))
        .spawn(move || {
            let mut comms_shutdown_rx = tx_worker_state.shutdown_subscribe();
            let mut last_send_error_log_ms = 0;
            let mut suppressed_send_errors = 0;
            let mut next_tx_allowed_at = std::time::Instant::now();
            let mut command_backlog: VecDeque<Vec<u8>> = VecDeque::new();
            let mut telemetry_backlog: VecDeque<Vec<u8>> = VecDeque::new();
            loop {
                match comms_shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(broadcast::error::TryRecvError::Closed)
                    | Err(broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }

                let now = std::time::Instant::now();
                if now < next_tx_allowed_at {
                    thread::sleep(next_tx_allowed_at.saturating_duration_since(now));
                    continue;
                }

                loop {
                    match comms_handle.tx_rx.try_recv() {
                        Ok(payload) => {
                            if is_command_payload(&payload) {
                                command_backlog.push_back(payload);
                            } else {
                                telemetry_backlog.push_back(payload);
                            }
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => return,
                    }
                }

                let mut sent_any = false;
                for _ in 0..COMMS_TX_BURST {
                    let Some(payload) = command_backlog
                        .pop_front()
                        .or_else(|| telemetry_backlog.pop_front())
                    else {
                        break;
                    };
                    let mut comms = tx_worker_comms.lock().expect("failed to get lock");
                    match comms.send_data(&payload) {
                        Ok(()) => {
                            sent_any = true;
                            log_link_control_send(worker_name, &payload);
                            log_radio_command_event("radio TX sent", worker_name, &payload);
                            if suppressed_send_errors > 0 {
                                eprintln!(
                                    "{worker_name} comms worker send_data recovered after suppressing {suppressed_send_errors} repeated errors"
                                );
                                suppressed_send_errors = 0;
                                last_send_error_log_ms = 0;
                            }
                        }
                        Err(e) => {
                            log_repeated_worker_error(
                                &format!("{worker_name} comms worker send_data failed"),
                                &e.to_string(),
                                &mut last_send_error_log_ms,
                                &mut suppressed_send_errors,
                            );
                        }
                    }
                }

                if sent_any {
                    next_tx_allowed_at =
                        std::time::Instant::now() + Duration::from_millis(COMMS_TX_GAP_MS);
                    thread::yield_now();
                } else {
                    thread::sleep(Duration::from_millis(COMMS_IDLE_SLEEP_MS));
                }
            }
        })?;

    let rx_worker_state = state.clone();
    let rx_worker_router = router.clone();
    let rx_worker = thread::Builder::new()
        .name(format!("{}_comms_rx", worker_name))
        .spawn(move || {
            let mut comms_shutdown_rx = rx_worker_state.shutdown_subscribe();
            let mut last_recv_error_log_ms = 0;
            let mut suppressed_recv_errors = 0;
            loop {
                match comms_shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(broadcast::error::TryRecvError::Closed)
                    | Err(broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }

                let tap_state = rx_worker_state.clone();
                let mut packet_tap = |pkt: &Packet| {
                    tap_state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
                    tap_state.mark_packet_received(get_current_timestamp_ms());
                    let mut rb = tap_state.ring_buffer.lock().unwrap();
                    rb.push(pkt.clone());
                };

                let mut comms = comms.lock().expect("failed to get lock");
                match comms.recv_packet(&rx_worker_router, &mut packet_tap) {
                    Ok(_) => {}
                    Err(e) => {
                        if !handle_worker_recv_error(
                            &format!("{worker_name} comms worker recv_packet failed"),
                            &format!("{e:?}"),
                            &mut last_recv_error_log_ms,
                            &mut suppressed_recv_errors,
                        ) {
                            break;
                        }
                    }
                }
                drop(comms);
                thread::sleep(Duration::from_millis(COMMS_IDLE_SLEEP_MS));
            }
        })?;

    Ok(vec![tx_worker, rx_worker])
}

fn spawn_legacy_comms_worker_thread(
    router: Arc<Router>,
    state: Arc<AppState>,
    mut comms_handle: CommsWorkerHandle,
) -> std::io::Result<Vec<thread::JoinHandle<()>>> {
    let worker_name = comms_handle.name;
    let comms = comms_handle.comms;
    let worker = thread::Builder::new()
        .name(format!("{}_comms_worker", worker_name))
        .spawn(move || {
            let mut comms_shutdown_rx = state.shutdown_subscribe();
            let mut last_send_error_log_ms = 0;
            let mut suppressed_send_errors = 0;
            let mut last_recv_error_log_ms = 0;
            let mut suppressed_recv_errors = 0;
            loop {
                match comms_shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(broadcast::error::TryRecvError::Closed)
                    | Err(broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }

                let mut sent_any = false;
                let tap_state = state.clone();
                let mut packet_tap = |pkt: &Packet| {
                    tap_state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
                    tap_state.mark_packet_received(get_current_timestamp_ms());
                    let mut rb = tap_state.ring_buffer.lock().unwrap();
                    rb.push(pkt.clone());
                };
                let mut comms = comms.lock().expect("failed to get lock");
                for _ in 0..COMMS_TX_BURST {
                    match comms_handle.tx_rx.try_recv() {
                        Ok(payload) => {
                            sent_any = true;
                            match comms.send_data(&payload) {
                                Ok(()) => {
                                    log_link_control_send(worker_name, &payload);
                                    log_radio_command_event("radio TX sent", worker_name, &payload);
                                    if suppressed_send_errors > 0 {
                                        eprintln!(
                                            "{worker_name} comms worker send_data recovered after suppressing {suppressed_send_errors} repeated errors"
                                        );
                                        suppressed_send_errors = 0;
                                        last_send_error_log_ms = 0;
                                    }
                                }
                                Err(e) => {
                                    log_repeated_worker_error(
                                        &format!("{worker_name} comms worker send_data failed"),
                                        &e.to_string(),
                                        &mut last_send_error_log_ms,
                                        &mut suppressed_send_errors,
                                    );
                                }
                            }
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => return,
                    }
                }

                match comms.recv_packet(&router, &mut packet_tap) {
                    Ok(_) => {}
                    Err(e) => {
                        if !handle_worker_recv_error(
                            &format!("{worker_name} comms worker recv_packet failed"),
                            &format!("{e:?}"),
                            &mut last_recv_error_log_ms,
                            &mut suppressed_recv_errors,
                        ) {
                            break;
                        }
                    }
                }
                drop(comms);

                if sent_any {
                    thread::yield_now();
                } else {
                    thread::sleep(Duration::from_millis(COMMS_IDLE_SLEEP_MS));
                }
            }
        })?;

    Ok(vec![worker])
}

pub(super) fn spawn_dedicated_radio_io_threads(
    router: Arc<Router>,
    state: Arc<AppState>,
    mut comms_handle: CommsWorkerHandle,
) -> std::io::Result<Vec<thread::JoinHandle<()>>> {
    let worker_name = comms_handle.name;
    let side_id = comms_handle.side_id;
    let comms = comms_handle.comms;
    let (incoming_tx, incoming_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let io_state = state.clone();

    let io_thread = thread::Builder::new()
        .name(format!("{}_radio_io", worker_name))
        .spawn(move || {
            let mut comms_shutdown_rx = io_state.shutdown_subscribe();
            let mut last_send_error_log_ms = 0;
            let mut suppressed_send_errors = 0;
            let mut last_recv_error_log_ms = 0;
            let mut suppressed_recv_errors = 0;
            let radio_follow_timeout = Duration::from_millis(radio_follow_timeout_ms());
            let radio_rx_idle_poll = Duration::from_millis(radio_rx_poll_idle_ms());
            let radio_rx_uplink_poll = Duration::from_millis(radio_rx_poll_uplink_ms());
            let radio_rx_downlink_poll = Duration::from_millis(radio_rx_poll_downlink_ms());
            let radio_rx_idle_packets = radio_rx_packets_idle();
            let radio_rx_uplink_packets = radio_rx_packets_uplink();
            let radio_rx_downlink_packets = radio_rx_packets_downlink();
            let radio_tx_backlog_limit = radio_tx_backlog_limit();
            let radio_tx_window_packets = radio_tx_packets_per_window();
            let radio_tx_without_window = radio_tx_without_window();
            let radio_uplink_turnaround = Duration::from_millis(radio_uplink_turnaround_ms());
            let radio_uplink_tx_guard = Duration::from_millis(radio_uplink_tx_guard_ms());
            let mut tx_backlog: VecDeque<Vec<u8>> = VecDeque::new();
            let mut last_window_update_at: Option<std::time::Instant> = None;
            let mut follow_window_opened_at: Option<std::time::Instant> = None;
            let mut follow_window_until: Option<std::time::Instant> = None;
            let mut follow_window_is_uplink = false;
            let mut has_seen_window_update = false;
            let mut sent_in_current_uplink_window = 0usize;
            let mut last_uplink_log_at: Option<std::time::Instant> = None;

            loop {
                match comms_shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(broadcast::error::TryRecvError::Closed)
                    | Err(broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }

                loop {
                    match comms_handle.tx_rx.try_recv() {
                        Ok(payload) => {
                            if worker_name == "rocket_comms"
                                && is_fill_system_command_payload(&payload)
                            {
                                log_radio_command_event(
                                    "radio TX dropped fill-system command",
                                    worker_name,
                                    &payload,
                                );
                                continue;
                            }
                            log_radio_command_event("radio TX backlog", worker_name, &payload);
                            let repeats = if worker_name == "rocket_comms"
                                && is_flight_command_payload(&payload)
                            {
                                radio_flight_command_repeats()
                            } else {
                                1
                            };
                            for _ in 0..repeats {
                                tx_backlog.push_back(payload.clone());
                            }
                            while tx_backlog.len() > radio_tx_backlog_limit {
                                tx_backlog.pop_front();
                            }
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => return,
                    }
                }

                let mut comms = comms.lock().expect("failed to get lock");
                let now = std::time::Instant::now();
                let follow_mode_active = last_window_update_at
                    .is_some_and(|t| now.saturating_duration_since(t) <= radio_follow_timeout);
                let in_active_window =
                    has_seen_window_update && follow_mode_active && follow_window_until.is_some();
                let rx_poll_timeout = if follow_window_is_uplink {
                    radio_rx_uplink_poll
                } else if in_active_window {
                    radio_rx_downlink_poll
                } else {
                    radio_rx_idle_poll
                };
                let rx_packet_budget = if follow_window_is_uplink {
                    radio_rx_uplink_packets
                } else if in_active_window {
                    radio_rx_downlink_packets
                } else {
                    radio_rx_idle_packets
                };
                match comms.recv_serialized_packets_with_budget(
                    &mut |payload| {
                        let _ = incoming_tx.send(payload);
                    },
                    rx_poll_timeout,
                    rx_packet_budget,
                ) {
                    Ok(()) => {}
                    Err(e) => {
                        if !handle_worker_recv_error(
                            &format!("{worker_name} radio io recv_serialized_packets failed"),
                            &format!("{e:?}"),
                            &mut last_recv_error_log_ms,
                            &mut suppressed_recv_errors,
                        ) {
                            break;
                        }
                    }
                }
                while let Some(update) = comms.take_radio_window_update() {
                    let opened_at = std::time::Instant::now();
                    let deadline = opened_at + Duration::from_millis(update.duration_ms as u64);
                    has_seen_window_update = true;
                    last_window_update_at = Some(opened_at);
                    follow_window_opened_at = Some(opened_at);
                    follow_window_until = Some(deadline);
                    match update.kind {
                        RadioWindowKind::DownlinkOpen => {
                            // Window kinds are emitted from the RF-board perspective.
                            // RF-board downlink means the board is transmitting to GS.
                            follow_window_is_uplink = false;
                            sent_in_current_uplink_window = 0;
                        }
                        RadioWindowKind::UplinkOpen => {
                            // RF-board uplink means GS may transmit to the board.
                            follow_window_is_uplink = true;
                            sent_in_current_uplink_window = 0;
                            let uplink_log_now = std::time::Instant::now();
                            let should_log = !tx_backlog.is_empty()
                                && last_uplink_log_at
                                    .map(|last| {
                                        uplink_log_now.saturating_duration_since(last)
                                            >= Duration::from_millis(500)
                                    })
                                    .unwrap_or(true);
                            if should_log {
                                log_radio_uplink_available(
                                    worker_name,
                                    update.duration_ms,
                                    tx_backlog.len(),
                                );
                                last_uplink_log_at = Some(uplink_log_now);
                            }
                            let _ = send_while_uplink_window_open(
                                comms.as_mut(),
                                worker_name,
                                &mut tx_backlog,
                                radio_tx_window_packets,
                                radio_tx_without_window,
                                radio_follow_timeout,
                                has_seen_window_update,
                                last_window_update_at,
                                follow_window_opened_at,
                                radio_uplink_turnaround,
                                radio_uplink_tx_guard,
                                &mut follow_window_until,
                                &mut follow_window_is_uplink,
                                &mut sent_in_current_uplink_window,
                                &mut last_send_error_log_ms,
                                &mut suppressed_send_errors,
                            );
                        }
                    }
                }
                let _ = send_while_uplink_window_open(
                    comms.as_mut(),
                    worker_name,
                    &mut tx_backlog,
                    radio_tx_window_packets,
                    radio_tx_without_window,
                    radio_follow_timeout,
                    has_seen_window_update,
                    last_window_update_at,
                    follow_window_opened_at,
                    radio_uplink_turnaround,
                    radio_uplink_tx_guard,
                    &mut follow_window_until,
                    &mut follow_window_is_uplink,
                    &mut sent_in_current_uplink_window,
                    &mut last_send_error_log_ms,
                    &mut suppressed_send_errors,
                );
                drop(comms);
            }
        })?;

    let ingress_state = state.clone();
    let ingress_thread = thread::Builder::new()
        .name(format!("{}_radio_ingress", worker_name))
        .spawn(move || {
            let mut shutdown_rx = ingress_state.shutdown_subscribe();
            let mut last_ingress_error_log_ms = 0;
            let mut suppressed_ingress_errors = 0;
            loop {
                match shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(broadcast::error::TryRecvError::Closed)
                    | Err(broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }

                let payload = match incoming_rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(payload) => payload,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };

                if let Ok(pkt) = serialize::deserialize_packet(&payload) {
                    ingress_state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
                    ingress_state.mark_packet_received(get_current_timestamp_ms());
                    if matches!(
                        pkt.data_type(),
                        DataType::GpsData | DataType::GpsSatelliteNumber
                    ) && !pkt.endpoints().contains(&DataEndpoint::GroundStation)
                    {
                        let mut rb = ingress_state.ring_buffer.lock().unwrap();
                        rb.push(pkt);
                    }
                }

                if let Err(err) = router.rx_serialized_queue_from_side(&payload, side_id) {
                    log_repeated_worker_error(
                        &format!("{worker_name} radio ingress queue failed"),
                        &format!("{err:?}"),
                        &mut last_ingress_error_log_ms,
                        &mut suppressed_ingress_errors,
                    );
                }
            }
        })?;

    Ok(vec![io_thread, ingress_thread])
}

fn spawn_rx_priority_comms_worker_thread(
    router: Arc<Router>,
    state: Arc<AppState>,
    mut comms_handle: CommsWorkerHandle,
) -> std::io::Result<Vec<thread::JoinHandle<()>>> {
    let worker_name = comms_handle.name;
    let comms = comms_handle.comms;
    let worker = thread::Builder::new()
        .name(format!("{}_comms_worker", worker_name))
        .spawn(move || {
            let mut comms_shutdown_rx = state.shutdown_subscribe();
            let mut last_send_error_log_ms = 0;
            let mut suppressed_send_errors = 0;
            let mut last_recv_error_log_ms = 0;
            let mut suppressed_recv_errors = 0;
            let mut next_tx_allowed_at = std::time::Instant::now();

            loop {
                match comms_shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(broadcast::error::TryRecvError::Closed)
                    | Err(broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }

                let tap_state = state.clone();
                let mut packet_tap = |pkt: &Packet| {
                    tap_state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
                    tap_state.mark_packet_received(get_current_timestamp_ms());
                    let mut rb = tap_state.ring_buffer.lock().unwrap();
                    rb.push(pkt.clone());
                };

                let mut comms = comms.lock().expect("failed to get lock");
                let recv_result = comms.recv_packet(&router, &mut packet_tap);
                match recv_result {
                    Ok(()) => {
                        let now = std::time::Instant::now();
                        if now >= next_tx_allowed_at
                            && let Ok(payload) = comms_handle.tx_rx.try_recv()
                        {
                            match comms.send_data(&payload) {
                                Ok(()) => {
                                    log_radio_command_event("radio TX sent", worker_name, &payload);
                                    if suppressed_send_errors > 0 {
                                        eprintln!(
                                            "{worker_name} comms worker send_data recovered after suppressing {suppressed_send_errors} repeated errors"
                                        );
                                        suppressed_send_errors = 0;
                                        last_send_error_log_ms = 0;
                                    }
                                    next_tx_allowed_at = std::time::Instant::now()
                                        + Duration::from_millis(COMMS_TX_GAP_MS);
                                }
                                Err(e) => {
                                    log_repeated_worker_error(
                                        &format!("{worker_name} comms worker send_data failed"),
                                        &e.to_string(),
                                        &mut last_send_error_log_ms,
                                        &mut suppressed_send_errors,
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if !handle_worker_recv_error(
                            &format!("{worker_name} comms worker recv_packet failed"),
                            &format!("{e:?}"),
                            &mut last_recv_error_log_ms,
                            &mut suppressed_recv_errors,
                        ) {
                            break;
                        }
                    }
                }
                drop(comms);
                thread::sleep(Duration::from_millis(COMMS_IDLE_SLEEP_MS));
            }
        })?;

    Ok(vec![worker])
}

fn log_repeated_worker_error(
    context: &str,
    detail: &str,
    last_log_ms: &mut u64,
    suppressed_count: &mut u64,
) {
    let now_ms = get_current_timestamp_ms();
    if *last_log_ms == 0 || now_ms.saturating_sub(*last_log_ms) >= COMMS_ERROR_LOG_INTERVAL_MS {
        if *suppressed_count > 0 {
            eprintln!("{context}: {detail} (suppressed {suppressed_count} repeated errors)");
        } else {
            eprintln!("{context}: {detail}");
        }
        *last_log_ms = now_ms;
        *suppressed_count = 0;
    } else {
        *suppressed_count += 1;
    }
}

#[cfg(feature = "testing")]
fn testing_should_disable_rx_loop(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "no such file",
        "not found",
        "device not configured",
        "input/output error",
        "broken pipe",
        "disconnected",
        "timed out waiting",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
}

fn handle_worker_recv_error(
    context: &str,
    detail: &str,
    last_log_ms: &mut u64,
    suppressed_count: &mut u64,
) -> bool {
    #[cfg(feature = "testing")]
    {
        if testing_should_disable_rx_loop(detail) {
            gs_debug_println!("{context}: disabling RX loop in testing mode after {detail}");
            return false;
        }
        let _ = (context, detail, last_log_ms, suppressed_count);
        true
    }

    #[cfg(not(feature = "testing"))]
    {
        log_repeated_worker_error(context, detail, last_log_ms, suppressed_count);
        true
    }
}

pub(super) fn spawn_router_worker_thread(
    router: Arc<Router>,
    state: Arc<AppState>,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("router_worker".to_string())
        .spawn(move || {
            let mut shutdown_rx = state.shutdown_subscribe();
            loop {
                match shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(broadcast::error::TryRecvError::Closed)
                    | Err(broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }

                let mut did_work = false;
                match router.poll_discovery() {
                    Ok(queued) => {
                        did_work |= queued;
                    }
                    Err(e) => {
                        log_telemetry_error("router discovery polling failed", e);
                    }
                }
                if timesync_enabled()
                    && let Ok(queued) = router.poll_timesync()
                {
                    did_work |= queued;
                }
                if let Err(e) = process_router_queues(&router) {
                    log_telemetry_error("router queue processing failed", e);
                }
                state.mark_discovered_relays_seen();
                if !did_work {
                    thread::sleep(Duration::from_millis(COMMS_IDLE_SLEEP_MS));
                } else {
                    thread::yield_now();
                }
            }
        })
}
fn radio_follow_timeout_ms() -> u64 {
    static TIMEOUT_MS: OnceLock<u64> = OnceLock::new();
    *TIMEOUT_MS.get_or_init(|| env_usize("GS_RADIO_FOLLOW_TIMEOUT_MS", 1_500, 50, 10_000) as u64)
}

fn radio_rx_poll_idle_ms() -> u64 {
    static TIMEOUT_MS: OnceLock<u64> = OnceLock::new();
    *TIMEOUT_MS.get_or_init(|| env_usize("GS_RADIO_RX_POLL_IDLE_MS", 2, 0, 50) as u64)
}

fn radio_rx_poll_uplink_ms() -> u64 {
    static TIMEOUT_MS: OnceLock<u64> = OnceLock::new();
    *TIMEOUT_MS.get_or_init(|| env_usize("GS_RADIO_RX_POLL_UPLINK_MS", 0, 0, 10) as u64)
}

fn radio_rx_poll_downlink_ms() -> u64 {
    static TIMEOUT_MS: OnceLock<u64> = OnceLock::new();
    *TIMEOUT_MS.get_or_init(|| env_usize("GS_RADIO_RX_POLL_DOWNLINK_MS", 5, 0, 20) as u64)
}

fn radio_rx_packets_idle() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| env_usize("GS_RADIO_RX_PACKETS_IDLE", 4, 1, 128))
}

fn radio_rx_packets_uplink() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| env_usize("GS_RADIO_RX_PACKETS_UPLINK", 1, 1, 16))
}

fn radio_rx_packets_downlink() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| env_usize("GS_RADIO_RX_PACKETS_DOWNLINK", 16, 1, 256))
}

fn radio_tx_backlog_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| env_usize("GS_RADIO_TX_BACKLOG_LIMIT", 256, 1, 256))
}

fn radio_tx_packets_per_window() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| env_usize("GS_RADIO_TX_PACKETS_PER_WINDOW", 16, 1, 128))
}

fn radio_flight_command_repeats() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| env_usize("GS_RADIO_FLIGHT_COMMAND_REPEATS", 3, 1, 8))
}

fn radio_tx_without_window() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("GS_RADIO_TX_WITHOUT_WINDOW").ok().as_deref() == Some("1"))
}

fn radio_uplink_turnaround_ms() -> u64 {
    static DELAY_MS: OnceLock<u64> = OnceLock::new();
    *DELAY_MS.get_or_init(|| env_usize("GS_RADIO_UPLINK_TURNAROUND_MS", 250, 0, 1_000) as u64)
}

fn radio_uplink_tx_guard_ms() -> u64 {
    static DELAY_MS: OnceLock<u64> = OnceLock::new();
    *DELAY_MS.get_or_init(|| env_usize("GS_RADIO_UPLINK_TX_GUARD_MS", 100, 0, 1_000) as u64)
}

fn radio_air_bit_rate_bps() -> u64 {
    static BPS: OnceLock<u64> = OnceLock::new();
    *BPS.get_or_init(|| env_usize("GS_RADIO_AIR_BIT_RATE_BPS", 4_800, 1_200, 921_600) as u64)
}

fn radio_air_frame_overhead_bytes() -> usize {
    static OVERHEAD: OnceLock<usize> = OnceLock::new();
    *OVERHEAD.get_or_init(|| env_usize("GS_RADIO_AIR_FRAME_OVERHEAD_BYTES", 16, 0, 256))
}

fn raw_uart_air_duration(payload_len: usize) -> Duration {
    let frame_len = payload_len
        .saturating_add(4)
        .saturating_add(radio_air_frame_overhead_bytes());
    let air_bps = radio_air_bit_rate_bps();
    let air_ms = (((frame_len as u64) * 10 * 1_000) + air_bps - 1) / air_bps;
    Duration::from_millis(air_ms)
}

fn maybe_log_green_radio_command_send(worker_name: &str, payload: &[u8]) {
    if !crate::radio_diagnostics_enabled() {
        return;
    }
    let Ok(pkt) = serialize::deserialize_packet(payload) else {
        return;
    };
    if !matches!(pkt.data_type(), DataType::FlightCommand) {
        return;
    }
    let cmd_bytes = pkt.payload();
    let cmd_preview = cmd_bytes
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!(
        "\x1b[32m{worker_name} radio TX FlightCommand sender={} endpoints={:?} payload={}\x1b[0m",
        pkt.sender(),
        pkt.endpoints(),
        cmd_preview
    );
}

pub(super) fn radio_command_log_line(
    event: &str,
    worker_name: &str,
    payload: &[u8],
) -> Option<String> {
    let Ok(pkt) = serialize::deserialize_packet(payload) else {
        return None;
    };
    let is_command = matches!(
        pkt.data_type(),
        DataType::ValveCommand
            | DataType::FlightCommand
            | DataType::ActuatorCommand
            | DataType::FlightState
            | DataType::Abort
    );
    if !is_command {
        return None;
    }

    let payload_preview = pkt
        .payload()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!(
        "{worker_name}: {event} {:?} sender={} endpoints={:?} payload={payload_preview}",
        pkt.data_type(),
        pkt.sender(),
        pkt.endpoints(),
    ))
}

fn log_radio_command_event(event: &str, worker_name: &str, payload: &[u8]) {
    if !crate::radio_diagnostics_enabled() {
        return;
    }
    if let Some(message) = radio_command_log_line(event, worker_name, payload) {
        eprintln!("{message}");
    }
}

fn log_link_control_send(worker_name: &str, payload: &[u8]) {
    let Ok(pkt) = serialize::deserialize_packet(payload) else {
        return;
    };
    if !matches!(pkt.data_type(), DataType::FlightState) {
        return;
    }
    let payload_preview = pkt
        .payload()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    log::info!(
        "link tx sent side={worker_name} ty={:?} sender={} endpoints={:?} payload={payload_preview}",
        pkt.data_type(),
        pkt.sender(),
        pkt.endpoints(),
    );
}

fn log_radio_packet_event(event: &str, worker_name: &str, payload: &[u8]) {
    if !crate::radio_diagnostics_enabled() {
        return;
    }
    let Ok(pkt) = serialize::deserialize_packet(payload) else {
        return;
    };
    eprintln!(
        "{worker_name}: {event} {:?} sender={} endpoints={:?} payload_len={}",
        pkt.data_type(),
        pkt.sender(),
        pkt.endpoints(),
        pkt.payload().len(),
    );
}

fn log_radio_uplink_available(worker_name: &str, duration_ms: u16, backlog_len: usize) {
    if !crate::radio_diagnostics_enabled() {
        return;
    }
    eprintln!(
        "{worker_name}: radio uplink available window_ms={duration_ms} queued_commands={backlog_len}"
    );
}

fn is_command_payload(payload: &[u8]) -> bool {
    let Ok(pkt) = serialize::deserialize_packet(payload) else {
        return false;
    };
    matches!(
        pkt.data_type(),
        DataType::ValveCommand
            | DataType::FlightCommand
            | DataType::ActuatorCommand
            | DataType::FlightState
            | DataType::Abort
    )
}

pub(super) fn is_fill_system_command_payload(payload: &[u8]) -> bool {
    let Ok(pkt) = serialize::deserialize_packet(payload) else {
        return false;
    };
    matches!(
        pkt.data_type(),
        DataType::ValveCommand | DataType::ActuatorCommand | DataType::Abort
    )
}

fn is_flight_command_payload(payload: &[u8]) -> bool {
    let Ok(pkt) = serialize::deserialize_packet(payload) else {
        return false;
    };
    matches!(pkt.data_type(), DataType::FlightCommand)
}

fn send_while_uplink_window_open(
    comms: &mut dyn CommsDevice,
    worker_name: &str,
    tx_backlog: &mut VecDeque<Vec<u8>>,
    max_packets_per_window: usize,
    allow_without_window: bool,
    radio_follow_timeout: Duration,
    has_seen_window_update: bool,
    last_window_update_at: Option<std::time::Instant>,
    follow_window_opened_at: Option<std::time::Instant>,
    uplink_turnaround: Duration,
    uplink_tx_guard: Duration,
    follow_window_until: &mut Option<std::time::Instant>,
    follow_window_is_uplink: &mut bool,
    sent_in_current_uplink_window: &mut usize,
    last_send_error_log_ms: &mut u64,
    suppressed_send_errors: &mut u64,
) -> bool {
    let mut sent_any = false;
    loop {
        let now = std::time::Instant::now();
        let follow_mode_active = last_window_update_at
            .is_some_and(|t| now.saturating_duration_since(t) <= radio_follow_timeout);
        if !has_seen_window_update {
            if allow_without_window
                && let Some(payload) = tx_backlog.front()
                && is_command_payload(payload)
            {
                let payload = tx_backlog
                    .pop_front()
                    .expect("front payload should still exist");
                log_radio_packet_event("radio TX pop without window", worker_name, &payload);
                maybe_log_green_radio_command_send(worker_name, &payload);
                match comms.send_data(&payload) {
                    Ok(()) => {
                        log_radio_command_event(
                            "radio TX sent without window",
                            worker_name,
                            &payload,
                        );
                        sent_any = true;
                    }
                    Err(e) => {
                        log_radio_packet_event(
                            "radio TX send_data without window failed for",
                            worker_name,
                            &payload,
                        );
                        log_repeated_worker_error(
                            &format!("{worker_name} radio io send_data without window failed"),
                            &e.to_string(),
                            last_send_error_log_ms,
                            suppressed_send_errors,
                        );
                    }
                }
            }
            break;
        }
        if !follow_mode_active {
            *follow_window_until = None;
            *follow_window_is_uplink = false;
            *sent_in_current_uplink_window = 0;
            break;
        }
        let Some(deadline) = *follow_window_until else {
            break;
        };
        if now >= deadline {
            if crate::radio_diagnostics_enabled()
                && *follow_window_is_uplink
                && *sent_in_current_uplink_window == 0
                && !tx_backlog.is_empty()
            {
                eprintln!(
                    "{worker_name}: radio uplink window closed without TX queued_commands={}",
                    tx_backlog.len()
                );
            }
            *follow_window_until = None;
            *follow_window_is_uplink = false;
            *sent_in_current_uplink_window = 0;
            break;
        }
        if !*follow_window_is_uplink
            || tx_backlog.is_empty()
            || *sent_in_current_uplink_window >= max_packets_per_window
        {
            break;
        }
        if let Some(opened_at) = follow_window_opened_at {
            let earliest_tx_at = opened_at + uplink_turnaround;
            if now < earliest_tx_at {
                break;
            }
        }

        let Some(payload) = tx_backlog.front() else {
            break;
        };
        let latest_finish = deadline.checked_sub(uplink_tx_guard).unwrap_or(deadline);
        let air_done_at = now + raw_uart_air_duration(payload.len());
        if air_done_at > latest_finish {
            break;
        }
        let payload = tx_backlog
            .pop_front()
            .expect("front payload should still exist after budget check");
        log_radio_packet_event("radio TX pop", worker_name, &payload);
        maybe_log_green_radio_command_send(worker_name, &payload);
        match comms.send_data(&payload) {
            Ok(()) => {
                log_radio_command_event("radio TX sent", worker_name, &payload);
                *sent_in_current_uplink_window += 1;
                sent_any = true;
                if *suppressed_send_errors > 0 {
                    eprintln!(
                        "{worker_name} radio io send_data recovered after suppressing {suppressed_send_errors} repeated errors"
                    );
                    *suppressed_send_errors = 0;
                    *last_send_error_log_ms = 0;
                }
            }
            Err(e) => {
                log_radio_packet_event("radio TX send_data failed for", worker_name, &payload);
                log_repeated_worker_error(
                    &format!("{worker_name} radio io send_data failed"),
                    &e.to_string(),
                    last_send_error_log_ms,
                    suppressed_send_errors,
                );
                break;
            }
        }
    }
    sent_any
}
