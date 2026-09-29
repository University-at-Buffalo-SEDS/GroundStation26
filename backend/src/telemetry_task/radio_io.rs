use crate::comms::{CommsDevice, RadioWindowKind};
use crate::state::AppState;
use sedsnet::packet::Packet;
use sedsnet::router::{Router, RouterSideId};
use sedsnet::wire_format as serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Duration;

use super::{
    COMMS_ERROR_LOG_INTERVAL_MS, env_usize, get_current_timestamp_ms, log_telemetry_error,
    process_router_queues, timesync_enabled,
};

/// Bounds accepted bytes until the generic worker finishes the physical write.
/// The router retains ownership of frames rejected with backpressure.
pub struct TxQueueBudget {
    pending: AtomicUsize,
    limit: usize,
}

impl TxQueueBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            pending: AtomicUsize::new(0),
            limit,
        }
    }

    pub fn try_send(
        &self,
        sender: &mpsc::UnboundedSender<(u8, Vec<u8>)>,
        priority: u8,
        payload: &[u8],
    ) -> sedsnet::TelemetryResult<()> {
        // Side callbacks receive schema chunks individually. Once the first
        // chunk is admitted, finish that transfer even when it crosses the
        // normal queue target; otherwise every retry can stall at chunk two.
        // These are locally generated SDT v3 chunk envelopes (not RX input).
        let continuation = if payload.starts_with(b"SDT\x03") && payload.len() >= 16 {
            let index = u16::from_le_bytes([payload[8], payload[9]]) as usize;
            let total = u16::from_le_bytes([payload[10], payload[11]]) as usize;
            if total == 0 || total > 64 || index >= total || payload.len() > 1024 {
                return Err(sedsnet::TelemetryError::HandlerError(
                    "radio chunk transfer exceeds budget",
                ));
            }
            index != 0
        } else {
            false
        };
        let ceiling = self.limit + if continuation { 64 * 1024 } else { 0 };
        self.pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                pending
                    .checked_add(payload.len())
                    .filter(|total| *total <= ceiling)
            })
            .map_err(|_| sedsnet::TelemetryError::HandlerError("radio transport backpressure"))?;
        if sender.send((priority, payload.to_vec())).is_err() {
            self.complete(payload.len());
            return Err(sedsnet::TelemetryError::HandlerError(
                "radio tx queue closed",
            ));
        }
        Ok(())
    }

    fn complete(&self, len: usize) {
        self.pending.fetch_sub(len, Ordering::AcqRel);
    }
}

pub struct CommsWorkerHandle {
    pub name: &'static str,
    /// Used by the generic worker; legacy/dedicated workers leave this None.
    pub tx_budget: Option<Arc<TxQueueBudget>>,
    pub comms: Arc<Mutex<Box<dyn CommsDevice>>>,
    pub tx_comms: Option<Arc<Mutex<Box<dyn CommsDevice>>>>,
    pub side_id: RouterSideId,
    pub tx_rx: mpsc::UnboundedReceiver<(u8, Vec<u8>)>,
    pub legacy_single_worker: bool,
    pub prioritize_rx: bool,
    pub dedicated_radio_io: bool,
}

const COMMS_TX_BURST: usize = 1;
const COMMS_TX_GAP_MS: u64 = 10;
const GENERAL_COMMS_TX_BURST: usize = 16;
const COMMS_IDLE_SLEEP_MS: u64 = 1;

type PriorityBacklog = BTreeMap<u8, VecDeque<Vec<u8>>>;

fn backlog_push(backlog: &mut PriorityBacklog, priority: u8, payload: Vec<u8>, front: bool) {
    let queue = backlog.entry(priority).or_default();
    if front {
        queue.push_front(payload);
    } else {
        queue.push_back(payload);
    }
}

// Discovery and variable refreshes use priorities 254/255. Reserve one turn
// after 1 KiB of control traffic for queued application traffic; otherwise a
// continuous control backlog can outlive an ordered command's retry window.
const MAX_CONTROL_BYTES: usize = 1024;
fn backlog_pop(backlog: &mut PriorityBacklog, control_bytes: &mut usize) -> Option<(u8, Vec<u8>)> {
    let highest = backlog.last_key_value().map(|(priority, _)| *priority)?;
    let priority = if *control_bytes >= MAX_CONTROL_BYTES {
        backlog
            .range(..254)
            .next_back()
            .map(|(priority, _)| *priority)
            .unwrap_or(highest)
    } else {
        highest
    };
    let queue = backlog.get_mut(&priority).expect("priority just selected");
    let payload = queue.pop_front();
    if priority >= 254 {
        *control_bytes = control_bytes.saturating_add(payload.as_ref().map_or(0, Vec::len));
    } else {
        *control_bytes = 0;
    }
    if queue.is_empty() {
        backlog.remove(&priority);
    }
    payload.map(|payload| (priority, payload))
}

#[cfg(test)]
mod priority_tests {
    use super::*;

    #[test]
    fn radio_budget_stays_charged_until_write_completion() {
        let budget = TxQueueBudget::new(2048);
        let (tx, mut rx) = mpsc::unbounded_channel();
        budget.try_send(&tx, 255, &[0; 1024]).unwrap();
        budget.try_send(&tx, 255, &[1; 1024]).unwrap();
        let (_, first) = rx.try_recv().unwrap();
        assert!(budget.try_send(&tx, 0, &[2; 32]).is_err());
        // Moving to a worker backlog or retrying a failed write frees nothing.
        assert_eq!(budget.pending.load(Ordering::Acquire), 2048);
        budget.complete(first.len());
        budget.try_send(&tx, 0, &[2; 32]).unwrap();
        assert_eq!(budget.pending.load(Ordering::Acquire), 1056);
    }

    #[test]
    fn radio_budget_finishes_a_schema_larger_than_the_queue_target() {
        use sedsnet::config::{DataEndpoint, DataType};
        use sedsnet::router::{RouterConfig, RouterSideOptions};
        let budget = Arc::new(TxQueueBudget::new(2048));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let router = Router::new_with_clock(RouterConfig::default(), Box::new(|| 0));
        let callback_budget = budget.clone();
        let callback_tx = tx.clone();
        router.add_side_packed_with_options(
            "test_uart",
            move |frame| callback_budget.try_send(&callback_tx, 255, frame),
            RouterSideOptions {
                max_frame_bytes: 1024,
                ..Default::default()
            },
        );
        let mut seed = 123u32;
        let body: Vec<u8> = (0..3600)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        let packet = Packet::new(
            DataType::DiscoverySchema,
            &[DataEndpoint::Discovery],
            "GS",
            1,
            Arc::from(body),
        )
        .unwrap();
        router.tx(packet).unwrap();
        assert!(budget.pending.load(Ordering::Acquire) > 3600);
        assert!(budget.try_send(&tx, 255, &[0; 100]).is_err());
        let mut frames = 0;
        while let Ok((_, payload)) = rx.try_recv() {
            budget.complete(payload.len());
            frames += 1;
        }
        assert_eq!(frames, 4);
        assert_eq!(budget.pending.load(Ordering::Acquire), 0);
    }

    #[test]
    fn radio_budget_rejects_oversize_and_rolls_back_closed_channel() {
        let budget = TxQueueBudget::new(2048);
        let (tx, rx) = mpsc::unbounded_channel();
        assert!(budget.try_send(&tx, 0, &[0; 2049]).is_err());
        assert_eq!(budget.pending.load(Ordering::Acquire), 0);
        drop(rx);
        assert!(budget.try_send(&tx, 0, &[0; 128]).is_err());
        assert_eq!(budget.pending.load(Ordering::Acquire), 0);
    }

    #[test]
    fn control_flood_cannot_starve_queued_commands() {
        let mut backlog = PriorityBacklog::new();
        let mut streak = 0;
        backlog_push(&mut backlog, 0, vec![42], false);
        for _ in 0..MAX_CONTROL_BYTES / 128 {
            backlog_push(&mut backlog, 254, vec![1; 128], false);
            backlog_push(&mut backlog, 255, vec![2; 128], false);
            assert_eq!(backlog_pop(&mut backlog, &mut streak).unwrap().0, 255);
        }
        backlog_push(&mut backlog, 255, vec![3], false);
        assert_eq!(backlog_pop(&mut backlog, &mut streak), Some((0, vec![42])));
        assert_eq!(backlog_pop(&mut backlog, &mut streak).unwrap().0, 255);
    }

    #[test]
    fn one_large_control_frame_yields_to_waiting_command() {
        let mut backlog = PriorityBacklog::new();
        let mut bytes = 0;
        backlog_push(&mut backlog, 0, vec![42], false);
        backlog_push(&mut backlog, 254, vec![1; 1024], false);
        backlog_push(&mut backlog, 254, vec![2; 1024], false);
        assert_eq!(backlog_pop(&mut backlog, &mut bytes).unwrap().0, 254);
        assert_eq!(backlog_pop(&mut backlog, &mut bytes), Some((0, vec![42])));
    }

    #[test]
    fn priority_backlog_preserves_fifo_within_each_band() {
        let mut backlog = PriorityBacklog::new();
        let mut control_bytes = 0;
        backlog.entry(5).or_default().extend([vec![1], vec![2]]);
        backlog.entry(255).or_default().push_back(vec![3]);
        backlog.entry(254).or_default().push_back(vec![4]);

        assert_eq!(
            backlog_pop(&mut backlog, &mut control_bytes),
            Some((255, vec![3]))
        );
        assert_eq!(
            backlog_pop(&mut backlog, &mut control_bytes),
            Some((254, vec![4]))
        );
        assert_eq!(
            backlog_pop(&mut backlog, &mut control_bytes),
            Some((5, vec![1]))
        );
        assert_eq!(
            backlog_pop(&mut backlog, &mut control_bytes),
            Some((5, vec![2]))
        );
        assert_eq!(backlog_pop(&mut backlog, &mut control_bytes), None);
    }
}
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
    // UART reads can wait for incoming bytes while a write drains a long
    // frame. Independent handles keep either direction from holding up the
    // other; I2C and other shared-bus transports retain their single lock.
    let tx_worker_comms = if let Some(writer) = comms_handle.tx_comms.clone() {
        writer
    } else {
        let writer = comms.lock().expect("failed to get lock").try_clone_tx()?;
        writer
            .map(|writer| Arc::new(Mutex::new(writer)))
            .unwrap_or_else(|| comms.clone())
    };
    let tx_worker = thread::Builder::new()
        .name(format!("{}_comms_tx", worker_name))
        .spawn(move || {
            let mut comms_shutdown_rx = tx_worker_state.shutdown_subscribe();
            let mut last_send_error_log_ms = 0;
            let mut suppressed_send_errors = 0;
            let mut next_tx_allowed_at = std::time::Instant::now();
            let mut backlog = PriorityBacklog::new();
        let mut control_bytes = 0;
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

                let mut sent_any = false;
                for _ in 0..GENERAL_COMMS_TX_BURST {
                    // Pick up newly queued commands before every frame, not
                    // after transmitting an entire burst of large schemas.
                    // Bound draining so a busy producer cannot starve writes.
                    for _ in 0..256 {
                        match comms_handle.tx_rx.try_recv() {
                            Ok((priority, payload)) => backlog_push(&mut backlog, priority, payload, false),
                            Err(mpsc::error::TryRecvError::Empty) => break,
                            Err(mpsc::error::TryRecvError::Disconnected) => return,
                        }
                    }
                    let Some((priority, payload)) = backlog_pop(&mut backlog, &mut control_bytes) else {
                        break;
                    };
                    let mut comms = tx_worker_comms.lock().expect("failed to get lock");
                    match comms.send_data(&payload) {
                        Ok(()) => {
                            if let Some(budget) = &comms_handle.tx_budget {
                                budget.complete(payload.len());
                            }
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
                            // A transport error means the packet never reached
                            // Pico-Fi. Preserve queue ordering and retry the
                            // entire logical packet from a new START slot.
                            backlog_push(&mut backlog, priority, payload, true);
                            next_tx_allowed_at = std::time::Instant::now()
                                + Duration::from_millis(COMMS_TX_GAP_MS);
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
                    // send_data drains the OS/device transport. An additional
                    // per-packet delay only lowers throughput and increases
                    // discovery/network-variable latency.
                    next_tx_allowed_at = std::time::Instant::now();
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
                let busy = comms.receive_budget_exhausted();
                drop(comms);
                // Release the shared I2C lock between bounded bursts so TX can
                // compete. Do not add a millisecond of latency to busy RX.
                if busy {
                    thread::yield_now();
                } else {
                    thread::sleep(Duration::from_millis(COMMS_IDLE_SLEEP_MS));
                }
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
                        Ok((_priority, payload)) => {
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
            let radio_window_max_age = Duration::from_millis(radio_window_max_age_ms());
            let radio_tx_backlog_limit = radio_tx_backlog_limit();
            let radio_tx_window_packets = radio_tx_packets_per_window();
            let radio_uplink_window = Duration::from_millis(radio_uplink_window_ms());
            let radio_uplink_turnaround = Duration::from_millis(radio_uplink_turnaround_ms());
            let radio_uplink_tx_guard = Duration::from_millis(radio_uplink_tx_guard_ms());
            let radio_uplink_yield_grace = Duration::from_millis(radio_uplink_yield_grace_ms());
            let mut command_backlog: VecDeque<Vec<u8>> = VecDeque::new();
            let mut telemetry_backlog: VecDeque<Vec<u8>> = VecDeque::new();
            let mut last_window_update_at: Option<std::time::Instant> = None;
            let mut follow_window_opened_at: Option<std::time::Instant> = None;
            let mut follow_window_until: Option<std::time::Instant> = None;
            let mut follow_window_is_uplink = false;
            let mut follow_window_seq = 0u8;
            let mut follow_window_credit = radio_tx_window_packets;
            let mut follow_window_turnaround = radio_uplink_turnaround;
            let mut has_seen_window_update = false;
            let mut sent_in_current_uplink_window = 0usize;
            let mut sent_uplink_yield = false;
            let mut last_uplink_tx_at: Option<std::time::Instant> = None;
            let mut next_uplink_tx_at: Option<std::time::Instant> = None;
            let mut last_uplink_log_at: Option<std::time::Instant> = None;

            loop {
                match comms_shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(broadcast::error::TryRecvError::Closed)
                    | Err(broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }

                if !drain_radio_tx_queue(
                    &mut comms_handle.tx_rx,
                    worker_name,
                    &mut command_backlog,
                    &mut telemetry_backlog,
                    radio_tx_backlog_limit,
                ) {
                    return;
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
                if follow_window_is_uplink && !command_backlog.is_empty() {
                    let _ = send_while_uplink_window_open(
                        comms.as_mut(),
                        worker_name,
                        &mut command_backlog,
                        &mut telemetry_backlog,
                        follow_window_credit,
                        radio_follow_timeout,
                        has_seen_window_update,
                        last_window_update_at,
                        follow_window_opened_at,
                        follow_window_turnaround,
                        radio_uplink_tx_guard,
                        &mut follow_window_until,
                        &mut follow_window_is_uplink,
                        &mut sent_in_current_uplink_window,
                        &mut last_uplink_tx_at,
                        &mut next_uplink_tx_at,
                        &mut last_send_error_log_ms,
                        &mut suppressed_send_errors,
                    );
                }
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
                if !drain_radio_tx_queue(
                    &mut comms_handle.tx_rx,
                    worker_name,
                    &mut command_backlog,
                    &mut telemetry_backlog,
                    radio_tx_backlog_limit,
                ) {
                    return;
                }
                while let Some(update) = comms.take_radio_window_update() {
                    if !drain_radio_tx_queue(
                        &mut comms_handle.tx_rx,
                        worker_name,
                        &mut command_backlog,
                        &mut telemetry_backlog,
                        radio_tx_backlog_limit,
                    ) {
                        return;
                    }
                    let now = std::time::Instant::now();
                    if now.saturating_duration_since(update.received_at) > radio_window_max_age {
                        if crate::radio_diagnostics_enabled() {
                            eprintln!(
                                "{worker_name}: radio scheduler dropped stale {:?} seq={} age_ms={}",
                                update.kind,
                                update.seq,
                                now.saturating_duration_since(update.received_at).as_millis()
                            );
                        }
                        continue;
                    }
                    let opened_at = update.received_at;
                    let deadline = opened_at
                        + if matches!(update.kind, RadioWindowKind::UplinkOpen) {
                            radio_follow_timeout.min(radio_uplink_window)
                        } else {
                            radio_follow_timeout
                        };
                    has_seen_window_update = true;
                    last_window_update_at = Some(opened_at);
                    follow_window_opened_at = Some(opened_at);
                    follow_window_until = Some(deadline);
                    follow_window_seq = update.seq;
                    follow_window_credit = update.credit.min(radio_tx_window_packets).max(1);
                    follow_window_turnaround =
                        radio_uplink_turnaround.max(Duration::from_millis(update.turnaround_ms));
                    sent_uplink_yield = false;
                    match update.kind {
                        RadioWindowKind::DownlinkOpen => {
                            // Window kinds are emitted from the RF-board perspective.
                            // RF-board downlink means the board is transmitting to GS.
                            follow_window_is_uplink = false;
                            sent_in_current_uplink_window = 0;
                            sent_uplink_yield = false;
                            last_uplink_tx_at = None;
                            next_uplink_tx_at = None;
                        }
                        RadioWindowKind::UplinkOpen => {
                            // RF-board uplink means GS may transmit to the board.
                            follow_window_is_uplink = true;
                            sent_in_current_uplink_window = 0;
                            last_uplink_tx_at = None;
                            next_uplink_tx_at = None;
                            let uplink_log_now = std::time::Instant::now();
                            let should_log = !(command_backlog.is_empty()
                                && telemetry_backlog.is_empty())
                                && last_uplink_log_at
                                    .map(|last| {
                                        uplink_log_now.saturating_duration_since(last)
                                            >= Duration::from_millis(500)
                                    })
                                    .unwrap_or(true);
                            if should_log {
                                log_radio_uplink_available(
                                    worker_name,
                                    update.credit,
                                    command_backlog.len() + telemetry_backlog.len(),
                                );
                                last_uplink_log_at = Some(uplink_log_now);
                            }
                            let _ = send_while_uplink_window_open(
                                comms.as_mut(),
                                worker_name,
                                &mut command_backlog,
                                &mut telemetry_backlog,
                                follow_window_credit,
                                radio_follow_timeout,
                                has_seen_window_update,
                                last_window_update_at,
                                follow_window_opened_at,
                                follow_window_turnaround,
                                radio_uplink_tx_guard,
                                &mut follow_window_until,
                                &mut follow_window_is_uplink,
                                &mut sent_in_current_uplink_window,
                                &mut last_uplink_tx_at,
                                &mut next_uplink_tx_at,
                                &mut last_send_error_log_ms,
                                &mut suppressed_send_errors,
                            );
                        }
                    }
                }
                if !drain_radio_tx_queue(
                    &mut comms_handle.tx_rx,
                    worker_name,
                    &mut command_backlog,
                    &mut telemetry_backlog,
                    radio_tx_backlog_limit,
                ) {
                    return;
                }
                let _ = send_while_uplink_window_open(
                    comms.as_mut(),
                    worker_name,
                    &mut command_backlog,
                    &mut telemetry_backlog,
                    follow_window_credit,
                    radio_follow_timeout,
                    has_seen_window_update,
                    last_window_update_at,
                    follow_window_opened_at,
                    follow_window_turnaround,
                    radio_uplink_tx_guard,
                    &mut follow_window_until,
                    &mut follow_window_is_uplink,
                    &mut sent_in_current_uplink_window,
                    &mut last_uplink_tx_at,
                    &mut next_uplink_tx_at,
                    &mut last_send_error_log_ms,
                    &mut suppressed_send_errors,
                );
                let uplink_turnaround_elapsed = follow_window_opened_at
                    .map(|opened_at| {
                        std::time::Instant::now()
                            >= opened_at
                                .checked_add(follow_window_turnaround)
                                .unwrap_or(opened_at)
                    })
                    .unwrap_or(true);
                let uplink_yield_grace_elapsed =
                    if sent_in_current_uplink_window >= follow_window_credit {
                        true
                    } else {
                        last_uplink_tx_at
                            .or(follow_window_opened_at)
                            .map(|started_at| {
                                std::time::Instant::now()
                                    >= started_at
                                        .checked_add(radio_uplink_yield_grace)
                                        .unwrap_or(started_at)
                            })
                            .unwrap_or(true)
                    };
                let uplink_airtime_elapsed = next_uplink_tx_at
                    .map(|next_tx_at| std::time::Instant::now() >= next_tx_at)
                    .unwrap_or(true);
                if follow_window_is_uplink
                    && !sent_uplink_yield
                    && uplink_turnaround_elapsed
                    && uplink_yield_grace_elapsed
                    && uplink_airtime_elapsed
                    && (command_backlog.is_empty() && telemetry_backlog.is_empty()
                        || sent_in_current_uplink_window >= follow_window_credit)
                {
                    match comms.send_radio_scheduler_status(
                        follow_window_seq,
                        !(command_backlog.is_empty() && telemetry_backlog.is_empty()),
                    ) {
                        Ok(()) => {
                            sent_uplink_yield = true;
                            follow_window_is_uplink = false;
                            follow_window_until =
                                Some(std::time::Instant::now() + radio_follow_timeout);
                            sent_in_current_uplink_window = 0;
                            last_uplink_tx_at = None;
                            next_uplink_tx_at = None;
                        }
                        Err(e) => {
                            log_repeated_worker_error(
                                &format!("{worker_name} radio io scheduler yield failed"),
                                &e.to_string(),
                                &mut last_send_error_log_ms,
                                &mut suppressed_send_errors,
                            );
                        }
                    }
                }
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

                if let Ok(pkt) = serialize::unpack_packet(&payload) {
                    ingress_state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
                    ingress_state.mark_packet_received(get_current_timestamp_ms());
                    if pkt.data_type() == crate::telemetry_schema::data_type("GPS_SATELLITE_NUMBER")
                    {
                        let mut rb = ingress_state.ring_buffer.lock().unwrap();
                        rb.push(pkt);
                    }
                }

                if let Err(err) = router.rx_packed_queue_from_side(&payload, side_id) {
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
                            && let Ok((_priority, payload)) = comms_handle.tx_rx.try_recv()
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
            let mut last_clock_refresh = std::time::Instant::now();
            loop {
                match shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(broadcast::error::TryRecvError::Closed)
                    | Err(broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }

                let mut did_work = false;
                if std::env::var("GS_SIM_DISABLE_PERIODIC_DISCOVERY")
                    .ok()
                    .as_deref()
                    != Some("1")
                {
                    match router.poll_discovery() {
                        Ok(queued) => {
                            did_work |= queued;
                        }
                        Err(e) => {
                            log_telemetry_error("router discovery polling failed", e);
                        }
                    }
                }
                if timesync_enabled() {
                    if last_clock_refresh.elapsed() >= Duration::from_secs(1) {
                        super::refresh_host_network_time(&router, get_current_timestamp_ms());
                        last_clock_refresh = std::time::Instant::now();
                    }
                    if let Ok(queued) = router.poll_timesync() {
                        did_work |= queued;
                    }
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
    *TIMEOUT_MS.get_or_init(|| env_usize("GS_RADIO_FOLLOW_TIMEOUT_MS", 4_500, 50, 20_000) as u64)
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
    *TIMEOUT_MS.get_or_init(|| env_usize("GS_RADIO_RX_POLL_DOWNLINK_MS", 1, 0, 20) as u64)
}

fn radio_rx_packets_idle() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| env_usize("GS_RADIO_RX_PACKETS_IDLE", 16, 1, 128))
}

fn radio_rx_packets_uplink() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| env_usize("GS_RADIO_RX_PACKETS_UPLINK", 1, 1, 16))
}

fn radio_rx_packets_downlink() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| env_usize("GS_RADIO_RX_PACKETS_DOWNLINK", 16, 1, 256))
}

fn radio_window_max_age_ms() -> u64 {
    static TIMEOUT_MS: OnceLock<u64> = OnceLock::new();
    *TIMEOUT_MS.get_or_init(|| env_usize("GS_RADIO_WINDOW_MAX_AGE_MS", 1_000, 50, 5_000) as u64)
}

fn radio_tx_backlog_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| env_usize("GS_RADIO_TX_BACKLOG_LIMIT", 256, 1, 256))
}

fn radio_tx_packets_per_window() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| env_usize("GS_RADIO_TX_PACKETS_PER_WINDOW", 5, 1, 128))
}

fn radio_uplink_window_ms() -> u64 {
    static DELAY_MS: OnceLock<u64> = OnceLock::new();
    *DELAY_MS.get_or_init(|| env_usize("GS_RADIO_UPLINK_WINDOW_MS", 4_000, 500, 20_000) as u64)
}

fn radio_flight_command_repeats() -> usize {
    1
}

fn radio_uplink_turnaround_ms() -> u64 {
    static DELAY_MS: OnceLock<u64> = OnceLock::new();
    *DELAY_MS.get_or_init(|| env_usize("GS_RADIO_UPLINK_TURNAROUND_MS", 0, 0, 5_000) as u64)
}

fn radio_uplink_tx_guard_ms() -> u64 {
    static DELAY_MS: OnceLock<u64> = OnceLock::new();
    *DELAY_MS.get_or_init(|| env_usize("GS_RADIO_UPLINK_TX_GUARD_MS", 150, 0, 1_000) as u64)
}

fn radio_uplink_yield_grace_ms() -> u64 {
    static DELAY_MS: OnceLock<u64> = OnceLock::new();
    *DELAY_MS.get_or_init(|| env_usize("GS_RADIO_UPLINK_YIELD_GRACE_MS", 75, 0, 1_000) as u64)
}

fn radio_air_bit_rate_bps() -> u64 {
    static BPS: OnceLock<u64> = OnceLock::new();
    /* The RFD900x UART and air link are configured for 57,600 bit/s. The old
     * LoRa-era 9,600 bit/s default artificially held each packet in the host
     * scheduler six times too long, so its backlog and command latency grew
     * continuously even though the physical radio still had capacity. */
    *BPS.get_or_init(|| env_usize("GS_RADIO_AIR_BIT_RATE_BPS", 57_600, 300, 115_200) as u64)
}

fn radio_air_frame_overhead_bytes() -> u64 {
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| env_usize("GS_RADIO_AIR_FRAME_OVERHEAD_BYTES", 16, 0, 128) as u64)
}

fn radio_tx_cooldown_ms() -> u64 {
    static DELAY_MS: OnceLock<u64> = OnceLock::new();
    *DELAY_MS.get_or_init(|| env_usize("GS_RADIO_TX_COOLDOWN_MS", 25, 0, 1_000) as u64)
}

fn radio_air_time_for_payload(payload_len: usize) -> Duration {
    let framed_len = payload_len as u64 + 4;
    let bytes = framed_len + radio_air_frame_overhead_bytes();
    let bps = radio_air_bit_rate_bps().max(1);
    let air_ms = ((bytes * 10 * 1_000) + bps - 1) / bps;
    Duration::from_millis(air_ms.saturating_add(radio_tx_cooldown_ms()))
}

fn maybe_log_green_radio_command_send(worker_name: &str, payload: &[u8]) {
    if !crate::radio_diagnostics_enabled() {
        return;
    }
    let Ok(pkt) = serialize::unpack_packet(payload) else {
        return;
    };
    if pkt.data_type() != crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
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
    let Ok(pkt) = serialize::unpack_packet(payload) else {
        return None;
    };
    let ty = pkt.data_type();
    let is_command = [
        "VALVE_COMMAND",
        "FLIGHT_COMMAND",
        "ACTUATOR_COMMAND",
        "FLIGHT_STATE",
        "ABORT",
    ]
    .into_iter()
    .any(|name| ty == crate::telemetry_schema::data_type(name));
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
    let Ok(pkt) = serialize::unpack_packet(payload) else {
        return;
    };
    if pkt.data_type() != crate::telemetry_schema::data_type("FLIGHT_STATE") {
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
    let Ok(pkt) = serialize::unpack_packet(payload) else {
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

fn log_radio_uplink_available(worker_name: &str, credit: usize, backlog_len: usize) {
    if !crate::radio_diagnostics_enabled() {
        return;
    }
    eprintln!("{worker_name}: radio uplink grant credit={credit} queued_commands={backlog_len}");
}

fn is_command_payload(payload: &[u8]) -> bool {
    let Ok(pkt) = serialize::unpack_packet(payload) else {
        return false;
    };
    let ty = pkt.data_type();
    [
        "VALVE_COMMAND",
        "FLIGHT_COMMAND",
        "ACTUATOR_COMMAND",
        "FLIGHT_STATE",
        "ABORT",
    ]
    .into_iter()
    .any(|name| ty == crate::telemetry_schema::data_type(name))
}

fn is_flight_command_payload(payload: &[u8]) -> bool {
    let Ok(pkt) = serialize::unpack_packet(payload) else {
        return false;
    };
    pkt.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND")
}

fn drain_radio_tx_queue(
    tx_rx: &mut mpsc::UnboundedReceiver<(u8, Vec<u8>)>,
    worker_name: &str,
    command_backlog: &mut VecDeque<Vec<u8>>,
    telemetry_backlog: &mut VecDeque<Vec<u8>>,
    radio_tx_backlog_limit: usize,
) -> bool {
    loop {
        match tx_rx.try_recv() {
            Ok((_priority, payload)) => {
                log_radio_command_event("radio TX backlog", worker_name, &payload);
                let repeats =
                    if worker_name == "rocket_comms" && is_flight_command_payload(&payload) {
                        radio_flight_command_repeats()
                    } else {
                        1
                    };
                for _ in 0..repeats {
                    if is_command_payload(&payload) {
                        command_backlog.push_back(payload.clone());
                    } else {
                        telemetry_backlog.push_back(payload.clone());
                    }
                }
                while telemetry_backlog.len() > radio_tx_backlog_limit {
                    telemetry_backlog.pop_front();
                }
            }
            Err(mpsc::error::TryRecvError::Empty) => return true,
            Err(mpsc::error::TryRecvError::Disconnected) => return false,
        }
    }
}

fn send_while_uplink_window_open(
    comms: &mut dyn CommsDevice,
    worker_name: &str,
    command_backlog: &mut VecDeque<Vec<u8>>,
    telemetry_backlog: &mut VecDeque<Vec<u8>>,
    max_packets_per_window: usize,
    radio_follow_timeout: Duration,
    has_seen_window_update: bool,
    last_window_update_at: Option<std::time::Instant>,
    follow_window_opened_at: Option<std::time::Instant>,
    uplink_turnaround: Duration,
    uplink_tx_guard: Duration,
    follow_window_until: &mut Option<std::time::Instant>,
    follow_window_is_uplink: &mut bool,
    sent_in_current_uplink_window: &mut usize,
    last_uplink_tx_at: &mut Option<std::time::Instant>,
    next_uplink_tx_at: &mut Option<std::time::Instant>,
    last_send_error_log_ms: &mut u64,
    suppressed_send_errors: &mut u64,
) -> bool {
    let mut sent_any = false;
    loop {
        let now = std::time::Instant::now();
        let follow_mode_active = last_window_update_at
            .is_some_and(|t| now.saturating_duration_since(t) <= radio_follow_timeout);
        if !has_seen_window_update {
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
                && !(command_backlog.is_empty() && telemetry_backlog.is_empty())
            {
                eprintln!(
                    "{worker_name}: radio uplink window closed without TX queued_commands={}",
                    command_backlog.len() + telemetry_backlog.len()
                );
            }
            *follow_window_until = None;
            *follow_window_is_uplink = false;
            *sent_in_current_uplink_window = 0;
            break;
        }
        if !*follow_window_is_uplink
            || (command_backlog.is_empty() && telemetry_backlog.is_empty())
            || *sent_in_current_uplink_window >= max_packets_per_window
        {
            break;
        }
        let mut tx_start = now;
        if let Some(opened_at) = follow_window_opened_at {
            let earliest_tx_at = opened_at + uplink_turnaround;
            if earliest_tx_at > tx_start {
                tx_start = earliest_tx_at;
            }
        }
        if let Some(next_tx_at) = *next_uplink_tx_at
            && next_tx_at > tx_start
        {
            tx_start = next_tx_at;
        }
        if now < tx_start {
            break;
        }

        let sending_command = !command_backlog.is_empty();
        let Some(payload) = (if sending_command {
            command_backlog.front()
        } else {
            telemetry_backlog.front()
        }) else {
            break;
        };
        let air_time = radio_air_time_for_payload(payload.len());
        let latest_finish = deadline.checked_sub(uplink_tx_guard).unwrap_or(deadline);
        if now.checked_add(air_time).unwrap_or(now) > latest_finish {
            break;
        }

        let Some(payload) = (if sending_command {
            command_backlog.pop_front()
        } else {
            telemetry_backlog.pop_front()
        }) else {
            break;
        };
        log_radio_packet_event("radio TX pop", worker_name, &payload);
        maybe_log_green_radio_command_send(worker_name, &payload);
        match comms.send_data(&payload) {
            Ok(()) => {
                log_radio_command_event("radio TX sent", worker_name, &payload);
                *sent_in_current_uplink_window += 1;
                let sent_at = std::time::Instant::now();
                *last_uplink_tx_at = Some(sent_at);
                *next_uplink_tx_at = Some(sent_at.checked_add(air_time).unwrap_or(sent_at));
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
                if sending_command {
                    command_backlog.push_front(payload);
                }
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
