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

// Managed values retain their application type on wire but use discovery's
// scheduling priority (254). Only self-contained application frames can safely
// bypass the schema FIFO. This is a host transport tag, not a wire/API change.
fn radio_transport_priority(priority: u8, payload: &[u8]) -> u8 {
    if priority == 254
        && !payload.starts_with(b"SDT")
        && serialize::peek_frame_info_unchecked(payload)
            .is_ok_and(|info| !sedsnet::discovery::is_discovery_type(info.envelope.ty))
    {
        255
    } else {
        priority
    }
}

fn is_delivery_receipt(payload: &[u8]) -> bool {
    !payload.starts_with(b"SDT")
        && serialize::peek_frame_info_unchecked(payload).is_ok_and(|info| {
            matches!(
                info.envelope.ty,
                sedsnet::config::DataType::ReliableAck
                    | sedsnet::config::DataType::ReliablePartialAck
                    | sedsnet::config::DataType::ReliablePacketRequest
            )
        })
}

// Only delivery/application controls may consume the reserved command bytes.
// Discovery's priority-255 address/topology snapshots must not exhaust them.
fn uses_command_reserve(priority: u8, payload: &[u8]) -> bool {
    priority >= 200
        && priority != 254
        && !payload.starts_with(b"SDT")
        && !serialize::peek_frame_info_unchecked(payload)
            .is_ok_and(|info| sedsnet::discovery::is_discovery_type(info.envelope.ty))
}

// A linked simulator runs MCU UARTs and modem budgets in virtual time. Using
// host wall time for this one pacer floods the simulated radio when emulation
// slows down, creating artificial receive backlogs and ACK latency. Normal
// GroundStation operation continues to use Instant, including ordinary HITL.
#[derive(Clone, Copy)]
struct TxDeadline {
    wall: std::time::Instant,
    simulated_ms: Option<u64>,
}
impl TxDeadline {
    fn now() -> Self {
        Self {
            wall: std::time::Instant::now(),
            simulated_ms: crate::simulation_clock_ms(),
        }
    }
    fn after(self, delay: Duration) -> Self {
        Self {
            wall: self.wall + delay,
            simulated_ms: self
                .simulated_ms
                .map(|now| now.saturating_add(delay.as_millis() as u64)),
        }
    }
    fn ready_at(self, wall: std::time::Instant, simulated_ms: Option<u64>) -> bool {
        match (self.simulated_ms, simulated_ms) {
            (Some(deadline), Some(now)) => now >= deadline,
            _ => wall >= self.wall,
        }
    }
    fn ready(self) -> bool {
        self.ready_at(std::time::Instant::now(), crate::simulation_clock_ms())
    }
}

/// Bounds accepted bytes until the generic worker finishes the physical write.
/// The router retains ownership of frames rejected with backpressure.
pub struct TxQueueBudget {
    pending: AtomicUsize,
    pending_control: AtomicUsize,
    limit: usize,
}

impl TxQueueBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            pending: AtomicUsize::new(0),
            pending_control: AtomicUsize::new(0),
            limit,
        }
    }

    pub fn try_send(
        &self,
        sender: &mpsc::UnboundedSender<(u8, Vec<u8>)>,
        priority: u8,
        payload: &[u8],
    ) -> sedsnet::TelemetryResult<()> {
        let priority = radio_transport_priority(priority, payload);
        // Side callbacks receive schema chunks individually. Once the first
        // chunk is admitted, finish that transfer even when it crosses the
        // normal queue target; otherwise every retry can stall at chunk two.
        // These are locally generated SDT v3 chunk envelopes (not RX input).
        let continuation = if payload.starts_with(b"SDT\x03") && payload.len() >= 16 {
            let index = u16::from_le_bytes([payload[8], payload[9]]) as usize;
            let total = u16::from_le_bytes([payload[10], payload[11]]) as usize;
            // Smaller airtime chunks must not reduce the existing 64 KiB
            // byte allowance for a dynamic schema. Keep a separate bounded
            // frame count; the aggregate byte ceiling below still applies.
            if total == 0 || total > 512 || index >= total || payload.len() > 1024 {
                return Err(sedsnet::TelemetryError::HandlerError(
                    "radio chunk transfer exceeds budget",
                ));
            }
            index != 0
        } else {
            false
        };
        // Discovery uses priority 254. ACKs (255) and board commands have
        // their own bounded allowance, even while accepted schema chunks
        // exceed the ordinary target. Ownership still lasts until the write.
        let control = uses_command_reserve(priority, payload);
        if control {
            // Receipts are essential but must leave room for a new operator
            // command. Keep the same aggregate bound and retain refused frames
            // in the router; never discard an accepted receipt.
            let allowance = if is_delivery_receipt(payload) {
                self.limit.saturating_sub((self.limit / 4).min(256))
            } else {
                self.limit
            };
            self.pending_control
                .try_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                    pending
                        .checked_add(payload.len())
                        .filter(|total| *total <= allowance)
                })
                .map_err(|_| sedsnet::TelemetryError::Io("side tx busy"))?;
        }
        let ceiling = if control {
            self.limit.saturating_mul(2).saturating_add(64 * 1024)
        } else {
            self.limit
                .saturating_add(if continuation { 64 * 1024 } else { 0 })
        };
        let reserved = self
            .pending
            .try_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                pending
                    .checked_add(payload.len())
                    .filter(|total| *total <= ceiling)
            })
            .map_err(|_| sedsnet::TelemetryError::Io("side tx busy"));
        if let Err(error) = reserved {
            if control {
                self.pending_control
                    .fetch_sub(payload.len(), Ordering::AcqRel);
            }
            return Err(error);
        }
        if sender.send((priority, payload.to_vec())).is_err() {
            self.complete(priority, &payload);
            return Err(sedsnet::TelemetryError::HandlerError(
                "radio tx queue closed",
            ));
        }
        if std::env::var_os("GS_SIM_TRACE_RELIABLE").is_some() {
            log::info!(
                "full-bay transport admitted sim={:?} priority={} len={} type={:?}",
                crate::simulation_clock_ms(),
                priority,
                payload.len(),
                serialize::peek_frame_info_unchecked(payload).map(|info| info.envelope.ty)
            );
        }
        Ok(())
    }

    fn complete(&self, priority: u8, payload: &[u8]) {
        let len = payload.len();
        self.pending.fetch_sub(len, Ordering::AcqRel);
        if uses_command_reserve(priority, payload) {
            self.pending_control.fetch_sub(len, Ordering::AcqRel);
        }
    }
}

/// Accepted software TX ownership, including queue-pop and active-write windows.
/// This does not assert drain/receipt inside an external modem or remote board.
#[derive(Default)]
pub struct TransportTxActivity {
    pending: AtomicUsize,
}
impl TransportTxActivity {
    pub fn try_enqueue(&self, send: impl FnOnce() -> sedsnet::TelemetryResult<()>) -> sedsnet::TelemetryResult<()> {
        self.pending.try_update(Ordering::AcqRel, Ordering::Acquire, |count| count.checked_add(1))
            .map_err(|_| sedsnet::TelemetryError::Io("side tx busy"))?;
        if let Err(error) = send() {
            self.complete();
            return Err(error);
        }
        Ok(())
    }
    #[cfg(any(not(feature = "hitl_mode"), test))]
    pub fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) != 0
    }
    fn complete(&self) {
        // Underflow retains a fail-closed nonzero count even in release builds.
        let previous = self.pending.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0, "unowned software TX completion");
    }
}

pub struct CommsWorkerHandle {
    pub transport_activity: Option<Arc<TransportTxActivity>>,
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

// Stateful side-transport frames retain callback order. Self-contained control
// packets can bypass that stream: they neither install nor reference templates.
struct WireBacklog {
    ordered: VecDeque<(u8, Vec<u8>)>,
    routing: VecDeque<(u8, Vec<u8>)>,
    receipts: VecDeque<(u8, Vec<u8>)>,
    application_bytes: usize,
    ordered_metadata_next: bool,
    priority_bytes: usize,
    priorities: PriorityBacklog,
    preserve_order: bool,
    control_bytes: usize,
}
impl WireBacklog {
    fn new(preserve_order: bool) -> Self {
        Self {
            ordered: VecDeque::new(),
            routing: VecDeque::new(),
            receipts: VecDeque::new(),
            application_bytes: 0,
            ordered_metadata_next: false,
            priority_bytes: 0,
            priorities: PriorityBacklog::new(),
            preserve_order,
            control_bytes: 0,
        }
    }
    fn push(&mut self, priority: u8, payload: Vec<u8>, front: bool) {
        let priority = radio_transport_priority(priority, &payload);
        // These bytes come from the local router callback. SDT envelopes may
        // depend on earlier dictionary/chunk frames; canonical controls do not.
        let independent_control = priority >= 16 && priority != 254 && !payload.starts_with(b"SDT");
        // Route advertisements and application controls both carry priority
        // 255. A FIFO in that band put a newly requested state change behind
        // seconds of startup advertisements. Preserve every accepted route
        // update, but service it separately from independent commands/ACKs.
        let routing = priority == 255
            && !payload.starts_with(b"SDT")
            && serialize::peek_frame_info_unchecked(&payload)
                .is_ok_and(|info| sedsnet::discovery::is_discovery_type(info.envelope.ty));
        if self.preserve_order && is_delivery_receipt(&payload) {
            if front {
                self.receipts.push_front((priority, payload));
            } else {
                self.receipts.push_back((priority, payload));
            }
        } else if self.preserve_order && routing {
            if front {
                self.routing.push_front((priority, payload));
            } else {
                self.routing.push_back((priority, payload));
            }
        } else if self.preserve_order && !independent_control {
            if front {
                self.ordered.push_front((priority, payload));
            } else {
                self.ordered.push_back((priority, payload));
            }
        } else {
            backlog_push(&mut self.priorities, priority, payload, front);
        }
    }
    fn selection_state(&self) -> (usize, bool, usize, usize) {
        (
            self.application_bytes,
            self.ordered_metadata_next,
            self.priority_bytes,
            self.control_bytes,
        )
    }
    fn restore_selection(&mut self, state: (usize, bool, usize, usize)) {
        (
            self.application_bytes,
            self.ordered_metadata_next,
            self.priority_bytes,
            self.control_bytes,
        ) = state;
    }
    fn restore_selected(
        &mut self,
        state: (usize, bool, usize, usize),
        first: (u8, Vec<u8>),
        second: Option<(u8, Vec<u8>)>,
    ) {
        self.restore_selection(state);
        if let Some((priority, payload)) = second {
            self.push(priority, payload, true);
        }
        self.push(first.0, first.1, true);
    }
    /// Apply ordinary selection/fairness first. Only a command that would be
    /// selected next anyway may join the already selected command.
    fn batch_follower(&mut self, comms: &dyn CommsDevice, first: (u8, &[u8])) -> Option<(u8, Vec<u8>)> {
        let first_bytes = comms.command_batch_frame_len(first.0, first.1)?;
        self.batch_follower_after_bytes(comms, first_bytes)
    }
    fn batch_follower_after_bytes(&mut self, comms: &dyn CommsDevice, first_bytes: usize) -> Option<(u8, Vec<u8>)> {
        let state = self.selection_state();
        let Some(second) = self.pop() else {
            self.restore_selection(state);
            return None;
        };
        let eligible = comms
            .command_batch_frame_len(second.0, &second.1)
            .is_some_and(|size| size + first_bytes <= crate::comms::RADIO_COMMAND_BATCH_MAX_BYTES);
        if eligible {
            Some(second)
        } else {
            self.restore_selection(state);
            self.push(second.0, second.1, true);
            None
        }
    }
    fn has_eligible_critical(&self) -> bool {
        !self.receipts.is_empty()
            || !self.routing.is_empty()
            || self.priorities.iter().any(|(priority, queue)| {
                queue.iter().any(|bytes| {
                    *priority >= 254 || crate::comms::closes_radio_packet_batch(*priority, bytes)
                })
            })
    }
    fn pop_critical(&mut self) -> Option<(u8, Vec<u8>)> {
        // Canonical controls are independent of the ordered SDT dictionaries.
        // Routine heartbeat/GPS/discovery must not hide one eligible loan.
        let application = self.priorities.iter().rev().find_map(|(priority, queue)| {
            queue
                .iter()
                .position(|bytes| crate::comms::closes_radio_packet_batch(*priority, bytes))
                .map(|index| (*priority, index))
        });
        let receipt = self.receipts.front().is_some_and(|(priority, bytes)| {
            crate::comms::closes_radio_packet_batch(*priority, bytes)
        });
        let packet = if let Some((priority, index)) =
            application.filter(|_| !receipt || self.application_bytes < 256)
        {
            let queue = self.priorities.get_mut(&priority).unwrap();
            let bytes = queue.remove(index).unwrap();
            if queue.is_empty() {
                self.priorities.remove(&priority);
            }
            self.application_bytes = self.application_bytes.saturating_add(bytes.len());
            Some((priority, bytes))
        } else if receipt {
            self.application_bytes = 0;
            self.receipts.pop_front()
        } else {
            None
        };
        if let Some((_, bytes)) = &packet {
            self.control_bytes = self.control_bytes.saturating_add(bytes.len());
        }
        packet
    }
    fn pop(&mut self) -> Option<(u8, Vec<u8>)> {
        if self.preserve_order {
            // Bound control bursts so schema/bootstrap still makes progress.
            if (!self.priorities.is_empty() || !self.receipts.is_empty())
                && (self.control_bytes < MAX_CONTROL_BYTES
                    || (self.ordered.is_empty() && self.routing.is_empty()))
            {
                // Separate receipt and application FIFOs: a new managed value
                // shares priority 255 with ACKs and otherwise sits behind the
                // whole ACK backlog. Bound application bursts too, so neither
                // side can starve the other. Stateful SDT order stays intact.
                let packet = if !self.priorities.is_empty()
                    && (self.receipts.is_empty() || self.application_bytes < 256)
                {
                    let packet = backlog_pop(&mut self.priorities, &mut self.priority_bytes);
                    self.application_bytes = self
                        .application_bytes
                        .saturating_add(packet.as_ref().map_or(0, |(_, bytes)| bytes.len()));
                    packet
                } else {
                    self.application_bytes = 0;
                    self.receipts.pop_front()
                };
                self.control_bytes = self
                    .control_bytes
                    .saturating_add(packet.as_ref().map_or(0, |(_, payload)| payload.len()));
                return packet;
            }
            self.control_bytes = 0;
            // Alternate route repair and the stateful schema stream. Neither
            // may starve even under a sustained independent-control workload.
            if !self.routing.is_empty() && (self.ordered.is_empty() || !self.ordered_metadata_next)
            {
                self.ordered_metadata_next = true;
                self.routing.pop_front()
            } else {
                self.ordered_metadata_next = false;
                self.ordered.pop_front()
            }
        } else {
            backlog_pop(&mut self.priorities, &mut self.control_bytes)
        }
    }
}

#[cfg(test)]
mod priority_tests {
    use super::*;
    #[test]
    fn ordered_and_piggyback_controls_keep_individual_admissions() {
        crate::telemetry_schema::initialize().unwrap();
        let packet = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GS",
            1,
            Arc::from([1u8]),
        )
        .unwrap();
        let normal = serialize::pack_packet(&packet);
        assert!(crate::comms::is_radio_batch_command(255, &normal));
        for header in [
            serialize::ReliableHeader {
                flags: 0,
                seq: 7,
                ack: 0,
            },
            serialize::ReliableHeader {
                flags: serialize::RELIABLE_FLAG_UNORDERED,
                seq: 7,
                ack: 0,
            },
            serialize::ReliableHeader {
                flags: serialize::RELIABLE_FLAG_UNSEQUENCED,
                seq: 7,
                ack: 0,
            },
            serialize::ReliableHeader {
                flags: serialize::RELIABLE_FLAG_UNSEQUENCED,
                seq: 0,
                ack: 11,
            },
        ] {
            let bytes = serialize::pack_packet_with_reliable(&packet, header);
            assert_eq!(
                serialize::peek_frame_info(&bytes).unwrap().reliable,
                Some(header)
            );
            assert!(serialize::unpack_packet(&bytes).is_ok());
            assert!(
                !crate::comms::is_radio_batch_command(255, &bytes),
                "sequenced or piggyback receipt header must not share command admission: {header:?}"
            );
        }
    }

    struct BatchTestComms;
    impl CommsDevice for BatchTestComms {
        fn command_batch_frame_len(&self, priority:u8, payload:&[u8]) -> Option<usize> {
            crate::comms::is_radio_batch_command(priority,payload).then_some(payload.len()+12)
        }
        fn send_data(&mut self,_:&[u8])->Result<(),Box<dyn std::error::Error+Send+Sync>> { unreachable!() }
        fn set_side_id(&mut self,_:RouterSideId) {}
        fn recv_packet(&mut self,_:&Router,_:&mut dyn FnMut(&Packet))->sedsnet::TelemetryResult<()> { unreachable!() }
    }
    fn batch_test_command(name: &str, payload: &[u8]) -> Vec<u8> {
        crate::telemetry_schema::initialize().unwrap();
        serialize::pack_packet(
            &Packet::new(
                crate::telemetry_schema::data_type(name),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "GS",
                1,
                Arc::from(payload),
            )
            .unwrap(),
        )
        .to_vec()
    }
    #[test]
    fn three_command_batch_keeps_selection_and_retry_order() {
        let packets = ["FLIGHT_STATE", "AV_BAY_UNDERGLOW", "FLIGHT_BUZZER"]
            .map(|name| batch_test_command(name, &[1]));
        let mut q = WireBacklog::new(true);
        for packet in &packets { q.push(255, packet.clone(), false); }
        let before = q.selection_state();
        let first = q.pop().unwrap();
        let second = q.batch_follower(&BatchTestComms, (first.0, &first.1)).unwrap();
        let bytes = BatchTestComms.command_batch_frame_len(first.0, &first.1).unwrap()
            + BatchTestComms.command_batch_frame_len(second.0, &second.1).unwrap();
        let third = q.batch_follower_after_bytes(&BatchTestComms, bytes).unwrap();
        assert!(q.pop().is_none());
        // Any partial physical write retains all three original owners/IDs.
        q.push(third.0, third.1, true);
        q.restore_selected(before, first, Some(second));
        assert_eq!(q.selection_state(), before);
        for packet in packets { assert_eq!(q.pop().unwrap().1, packet); }
        assert!(q.pop().is_none());
    }

    #[test]
    fn existing_worker_backlog_coavailable_commands_form_bounded_pair_without_waiting() {
        let first = batch_test_command("AV_BAY_UNDERGLOW", &[1]);
        let second = batch_test_command("FLIGHT_COMMAND", &[1]);
        let mut q = WireBacklog::new(true);
        q.push(255, first.clone(), false);
        q.push(255, second.clone(), false);
        let selected = q.pop().unwrap();
        assert_eq!(selected.1, first);
        assert_eq!(
            q.batch_follower(&BatchTestComms, (selected.0, &selected.1))
                .unwrap()
                .1,
            second
        );
        assert!(q.pop().is_none());
        assert!(
            q.batch_follower(&BatchTestComms, (selected.0, &selected.1))
                .is_none(),
            "never wait for future producer callbacks"
        );
    }
    #[test]
    fn command_pair_does_not_bypass_metadata_fair_turn_or_consume_invalid_follower() {
        let command = batch_test_command("FLIGHT_COMMAND", &[1]);
        for follower in [
            b"SDT\x03opaque".to_vec(),
            serialize::pack_packet(
                &Packet::new(
                    sedsnet::config::DataType::ReliableAck,
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                    "RF",
                    1,
                    Arc::from(1u64.to_le_bytes()),
                )
                .unwrap(),
            )
            .to_vec(),
            serialize::pack_packet(
                &Packet::new(
                    crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
                    &std::array::from_fn::<_, 32, _>(|i| {
                        sedsnet::config::DataEndpoint(u32::MAX - i as u32)
                    }),
                    "GS",
                    1,
                    Arc::from([1u8]),
                )
                .unwrap(),
            )
            .to_vec(),
            {
                let mut b = command.clone();
                *b.last_mut().unwrap() ^= 1;
                b
            },
        ] {
            let mut q = WireBacklog::new(true);
            q.push(255, command.clone(), false);
            q.push(255, follower.clone(), false);
            let first = q.pop().unwrap();
            let counters = q.selection_state();
            assert!(
                q.batch_follower(&BatchTestComms, (first.0, &first.1))
                    .is_none()
            );
            assert_eq!(q.selection_state(), counters);
            assert_eq!(q.pop().unwrap().1, follower);
        }
        let mut q = WireBacklog::new(true);
        q.push(255, command.clone(), false);
        q.push(255, command.clone(), false);
        q.push(253, b"SDT\x03metadata".to_vec(), false);
        q.control_bytes = MAX_CONTROL_BYTES - 1;
        let first = q.pop().unwrap();
        assert!(q.control_bytes >= MAX_CONTROL_BYTES);
        assert!(
            q.batch_follower(&BatchTestComms, (first.0, &first.1))
                .is_none()
        );
        assert_eq!(q.pop().unwrap().1, b"SDT\x03metadata");
        assert_eq!(q.pop().unwrap().1, command);
    }


    #[test]
    fn failed_command_pair_restores_both_original_owners_and_fairness_checkpoint() {
        let a = batch_test_command("AV_BAY_UNDERGLOW", &[1]);
        let b = batch_test_command("FLIGHT_COMMAND", &[1]);
        let mut q = WireBacklog::new(true);
        q.push(255, a.clone(), false);
        q.push(255, b.clone(), false);
        let before = q.selection_state();
        let first = q.pop().unwrap();
        let second = q.batch_follower(&BatchTestComms, (first.0, &first.1));
        assert!(second.is_some());
        q.restore_selected(before, first, second);
        assert_eq!(q.selection_state(), before);
        assert_eq!(q.pop().unwrap().1, a);
        assert_eq!(q.pop().unwrap().1, b);
        assert!(q.pop().is_none());
    }
    #[test]
    fn continuous_command_pairs_preserve_existing_metadata_service_turn() {
        let command = batch_test_command("FLIGHT_COMMAND", &[1]);
        let mut q = WireBacklog::new(true);
        let mut metadata = 0;
        let mut commands = 0;
        let mut since_metadata = 0;
        for _ in 0..1000 {
            while q.priorities.values().map(VecDeque::len).sum::<usize>() < 4 {
                q.push(255, command.clone(), false);
            }
            if q.ordered.is_empty() {
                q.push(253, b"SDT\x03owned".to_vec(), false);
            }
            let first = q.pop().unwrap();
            if first.1.starts_with(b"SDT") {
                metadata += 1;
                since_metadata = 0;
            } else {
                since_metadata += first.1.len();
                commands += 1;
                if let Some(second) = q.batch_follower(&BatchTestComms, (first.0, &first.1)) {
                    since_metadata += second.1.len();
                    commands += 1;
                }
                assert!(
                    since_metadata <= MAX_CONTROL_BYTES + command.len(),
                    "batch cannot add an extra command after the existing fair turn becomes due"
                );
            }
        }
        assert!(commands > 1000 && metadata > 20);
    }

    #[test]
    fn rf_receive_dictionary_retains_interleaved_control_flows() {
        use sedsnet::router::{EndpointHandler, RouterConfig, RouterSideOptions};
        crate::telemetry_schema::initialize().unwrap();
        fn run(capacity: usize) -> usize {
            let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = delivered.clone();
            let endpoint = crate::telemetry_schema::endpoint("GROUND_STATION");
            let receiver = Router::new_with_clock(
                RouterConfig::new([EndpointHandler::new_packet_handler(
                    endpoint,
                    move |_: &Packet| {
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                )])
                .with_reliable_enabled(false),
                Box::new(|| 0),
            );
            let side = receiver.add_side_packed_with_options(
                "radio",
                |_| Ok(()),
                RouterSideOptions {
                    header_template_enabled: true,
                    max_side_transport_templates: capacity,
                    ..Default::default()
                },
            );
            let frames = Arc::new(Mutex::new(Vec::new()));
            let output = frames.clone();
            let sender = Router::new_with_clock(RouterConfig::default(), Box::new(|| 0));
            sender.add_side_packed_with_options(
                "radio",
                move |bytes| {
                    output.lock().unwrap().push(bytes.to_vec());
                    Ok(())
                },
                RouterSideOptions {
                    header_template_enabled: true,
                    max_side_transport_templates: 16,
                    ..Default::default()
                },
            );
            for round in 0..2 {
                for flow in 0..8 {
                    let packet = Packet::new(
                        crate::telemetry_schema::data_type("FUEL_FLOW"),
                        &[endpoint],
                        &format!("FLOW{flow}"),
                        round * 8 + flow,
                        Arc::from(12.0_f32.to_le_bytes()),
                    )
                    .unwrap()
                    .with_nonce((round * 8 + flow + 1) as u16);
                    sender.tx(packet).unwrap();
                }
                for frame in frames.lock().unwrap().drain(..) {
                    receiver.rx_packed_from_side(&frame, side).unwrap();
                }
            }
            delivered.load(Ordering::SeqCst)
        }
        assert!(
            run(4) < 16,
            "old RF cache must reproduce silent compact frame loss"
        );
        assert_eq!(run(16), 16);
    }

    #[test]
    fn radio_wire_order_survives_priority_bursts_and_failed_writes() {
        let mut backlog = WireBacklog::new(true);
        // A low-priority template followed by control/chunk traffic must not
        // be overtaken after its callback has accepted the encoded bytes.
        backlog.push(0, b"SDT\x01template".to_vec(), false);
        backlog.push(255, b"SDT\x03chunk".to_vec(), false);
        backlog.push(254, b"SDT\x02compact".to_vec(), false);
        let (priority, first) = backlog.pop().unwrap();
        assert_eq!(first, b"SDT\x01template");
        backlog.push(priority, first, true);
        backlog.push(255, b"SDT\x01new".to_vec(), false);
        for expected in [
            b"SDT\x01template".as_slice(),
            b"SDT\x03chunk",
            b"SDT\x02compact",
            b"SDT\x01new",
        ] {
            assert_eq!(backlog.pop().unwrap().1, expected);
        }
        assert!(backlog.pop().is_none());
        let mut unpaced = WireBacklog::new(false);
        unpaced.push(0, vec![0], false);
        unpaced.push(200, vec![1], false);
        assert_eq!(unpaced.pop().unwrap().1, vec![1]);
    }

    #[test]
    fn canonical_variable_update_bypasses_schema_backlog_without_reordering_templates() {
        crate::telemetry_schema::initialize().unwrap();
        let packet = Packet::new(
            crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
            &[crate::telemetry_schema::endpoint("AV_BAY_UNDERGLOW_OWNER")],
            "GS",
            123,
            Arc::from([1_u8]),
        )
        .unwrap();
        let control = serialize::pack_packet(&packet).to_vec();
        let mut backlog = WireBacklog::new(true);
        // At a conservative 25 ms/frame, 60 queued schema frames previously
        // delayed a one-byte state change by 1.5 seconds.
        let frames: Vec<_> = (0_u8..60)
            .map(|index| vec![b'S', b'D', b'T', 3, index])
            .collect();
        for frame in &frames {
            backlog.push(254, frame.clone(), false);
        }
        backlog.push(16, control.clone(), false);
        assert_eq!(backlog.pop(), Some((16, control.clone())));
        // A failed write must retry the complete canonical packet.
        backlog.push(16, control.clone(), true);
        assert_eq!(backlog.pop(), Some((16, control)));
        for expected in frames {
            assert_eq!(backlog.pop(), Some((254, expected)));
        }
        assert!(backlog.pop().is_none());
    }

    #[test]
    fn actual_managed_variable_update_is_independent_of_queued_templates() {
        use sedsnet::router::{NetworkVariablePermissions, RouterConfig, RouterSideOptions};
        crate::telemetry_schema::initialize().unwrap();
        let source = Arc::new(Router::new_with_clock(
            RouterConfig::new([]).with_sender("GS"),
            Box::new(|| 0),
        ));
        let peer = Router::new_with_clock(RouterConfig::new([]).with_sender("RF"), Box::new(|| 0));
        let ty = crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW");
        source
            .enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
            .unwrap();
        peer.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
            .unwrap();
        let frames = Arc::new(Mutex::new(Vec::<(u8, Vec<u8>)>::new()));
        let captured = frames.clone();
        source.add_side_packed_with_priority_and_options(
            "radio",
            move |bytes, priority| {
                captured.lock().unwrap().push((priority, bytes.to_vec()));
                Ok(())
            },
            RouterSideOptions::default().with_small_packet_transport(1024),
        );
        let destination = source.clone();
        peer.add_side_packet("ground", move |packet| destination.rx_from_side(packet, 0));
        peer.announce_discovery().unwrap();
        peer.process_all_queues_with_timeout(0).unwrap();
        source.process_all_queues_with_timeout(0).unwrap();
        frames.lock().unwrap().clear();
        source
            .set_network_variable(
                Packet::new(
                    ty,
                    &[crate::telemetry_schema::endpoint("AV_BAY_UNDERGLOW_OWNER")],
                    "GS",
                    123,
                    Arc::from([1_u8]),
                )
                .unwrap(),
            )
            .unwrap();
        source.process_all_queues_with_timeout(0).unwrap();
        let (priority, control) = frames
            .lock()
            .unwrap()
            .iter()
            .find(|(_, bytes)| {
                serialize::unpack_packet(bytes).is_ok_and(|packet| packet.data_type() == ty)
            })
            .cloned()
            .expect("Actual managed-variable update must be self-contained on the radio side");
        assert_eq!(
            priority,
            sedsnet::transport_priority(sedsnet::config::DataType::ManagedVariableValue)
        );
        let scheduled_priority = radio_transport_priority(priority, &control);
        assert_eq!(scheduled_priority, 255);
        // A full schema allowance cannot reject a small application update.
        let budget = TxQueueBudget::new(2048);
        let (tx, mut rx) = mpsc::unbounded_channel();
        budget.try_send(&tx, 254, &[0; 2048]).unwrap();
        budget.try_send(&tx, priority, &control).unwrap();
        let (metadata_priority, metadata) = rx.try_recv().unwrap();
        let (app_priority, application) = rx.try_recv().unwrap();
        assert_eq!(app_priority, 255);
        budget.complete(app_priority, &application);
        budget.complete(metadata_priority, &metadata);
        assert_eq!(budget.pending_control.load(Ordering::Acquire), 0);
        assert_eq!(budget.pending.load(Ordering::Acquire), 0);
        let mut backlog = WireBacklog::new(true);
        backlog.push(254, b"SDT\x03pending-schema".to_vec(), false);
        backlog.push(priority, control.clone(), false);
        assert_eq!(backlog.pop(), Some((scheduled_priority, control)));
        assert_eq!(backlog.pop().unwrap().1, b"SDT\x03pending-schema");
    }

    #[test]
    fn startup_route_backlog_cannot_delay_independent_managed_controls() {
        crate::telemetry_schema::initialize().unwrap();
        let route = serialize::pack_packet(
            &Packet::new(
                sedsnet::config::DataType::DiscoveryAnnounce,
                &[sedsnet::config::DataEndpoint::TelemetryError],
                "GS",
                1,
                Arc::from([]),
            )
            .unwrap(),
        )
        .to_vec();
        let control = serialize::pack_packet(
            &Packet::new(
                crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
                &[crate::telemetry_schema::endpoint("AV_BAY_UNDERGLOW_OWNER")],
                "GS",
                2,
                Arc::from([1_u8]),
            )
            .unwrap(),
        )
        .to_vec();
        let budget = TxQueueBudget::new(route.len() * 4);
        let (tx, mut rx) = mpsc::unbounded_channel();
        for _ in 0..4 {
            budget.try_send(&tx, 255, &route).unwrap();
        }
        assert_eq!(budget.pending_control.load(Ordering::Acquire), 0);
        budget.try_send(&tx, 254, &control).unwrap();
        while let Ok((priority, payload)) = rx.try_recv() {
            budget.complete(priority, &payload);
        }
        assert_eq!(budget.pending.load(Ordering::Acquire), 0);
        assert_eq!(budget.pending_control.load(Ordering::Acquire), 0);
        let mut backlog = WireBacklog::new(true);
        for _ in 0..32 {
            backlog.push(255, route.clone(), false);
        }
        backlog.push(254, b"SDT\x03schema-first".to_vec(), false);
        backlog.push(254, b"SDT\x03schema-second".to_vec(), false);
        backlog.push(254, control.clone(), false);
        assert_eq!(backlog.pop(), Some((255, control)));
        // Accepted route updates are retained, and schema order is unchanged.
        assert_eq!(backlog.pop(), Some((255, route.clone())));
        assert_eq!(backlog.pop().unwrap().1, b"SDT\x03schema-first");
        assert_eq!(backlog.pop(), Some((255, route.clone())));
        assert_eq!(backlog.pop().unwrap().1, b"SDT\x03schema-second");
        for _ in 0..30 {
            assert_eq!(backlog.pop(), Some((255, route.clone())));
        }
        assert!(backlog.pop().is_none());
    }

    #[test]
    fn ack_flood_yields_to_commands_as_well_as_both_metadata_streams() {
        let mut backlog = WireBacklog::new(true);
        backlog.push(254, b"SDT\x03schema".to_vec(), false);
        for _ in 0..100 {
            backlog.push(255, vec![0; 128], false);
        }
        backlog.push(200, vec![42], false);
        let mut command_seen = false;
        let mut schema_seen = false;
        for _ in 0..(MAX_CONTROL_BYTES / 128 + 3) {
            let (priority, payload) = backlog.pop().unwrap();
            command_seen |= priority == 200 && payload == [42];
            schema_seen |= payload == b"SDT\x03schema";
        }
        assert!(command_seen, "ACKs must not starve lower-priority commands");
        assert!(
            schema_seen,
            "independent controls must yield to the schema stream"
        );
    }

    #[test]
    fn real_ack_backlog_preserves_command_admission_and_bounded_bidirectional_service() {
        crate::telemetry_schema::initialize().unwrap();
        let ep = crate::telemetry_schema::endpoint("AV_BAY_UNDERGLOW_OWNER");
        let ack = serialize::pack_packet(
            &Packet::new(
                sedsnet::config::DataType::ReliableAck,
                &[ep],
                "GS",
                1,
                Arc::from(42_u64.to_le_bytes()),
            )
            .unwrap(),
        )
        .to_vec();
        let command = serialize::pack_packet(
            &Packet::new(
                crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
                &[ep],
                "GS",
                2,
                Arc::from([1_u8]),
            )
            .unwrap(),
        )
        .to_vec();
        assert!(is_delivery_receipt(&ack));
        assert!(!is_delivery_receipt(&command));
        let budget = TxQueueBudget::new(2048);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut accepted = 0;
        while budget.try_send(&tx, 255, &ack).is_ok() {
            accepted += 1;
        }
        assert!(accepted > 0);
        budget
            .try_send(&tx, 254, &command)
            .expect("ACK backlog must leave command space");
        let mut backlog = WireBacklog::new(true);
        while let Ok((priority, bytes)) = rx.try_recv() {
            backlog.push(priority, bytes, false);
        }
        assert_eq!(backlog.pop(), Some((255, command.clone())));
        budget.complete(255, &command);
        let mut delivered = 0;
        while let Some((priority, bytes)) = backlog.pop() {
            assert_eq!(bytes, ack);
            budget.complete(priority, &bytes);
            delivered += 1;
        }
        assert_eq!(delivered, accepted);
        assert_eq!(budget.pending.load(Ordering::Acquire), 0);
        assert_eq!(budget.pending_control.load(Ordering::Acquire), 0);

        // Sustained commands must still yield to accepted ACKs and metadata.
        backlog.push(255, ack.clone(), false);
        backlog.push(254, b"SDT\x03first".to_vec(), false);
        backlog.push(254, b"SDT\x03second".to_vec(), false);
        let mut receipt_seen = false;
        let mut metadata = Vec::new();
        for _ in 0..(2 * MAX_CONTROL_BYTES / command.len() + 20) {
            backlog.push(254, command.clone(), false);
            let (_, bytes) = backlog.pop().unwrap();
            receipt_seen |= bytes == ack;
            if bytes.starts_with(b"SDT") {
                metadata.push(bytes);
            }
            if receipt_seen && metadata.len() == 2 {
                break;
            }
        }
        assert!(receipt_seen);
        assert_eq!(
            metadata,
            [b"SDT\x03first".to_vec(), b"SDT\x03second".to_vec()]
        );
    }

    #[test]
    fn continuous_canonical_controls_cannot_starve_the_stateful_stream() {
        let mut backlog = WireBacklog::new(true);
        backlog.push(254, b"SDT\x03schema".to_vec(), false);
        for _ in 0..100 {
            backlog.push(16, vec![0; 128], false);
        }
        let mut controls = 0;
        loop {
            let (priority, _) = backlog.pop().unwrap();
            if priority == 254 {
                break;
            }
            controls += 1;
            assert!(controls <= MAX_CONTROL_BYTES / 128);
        }
        assert_eq!(controls, MAX_CONTROL_BYTES / 128);
    }

    #[test]
    fn radio_budget_stays_charged_until_write_completion() {
        crate::telemetry_schema::initialize().unwrap();
        let frame = serialize::pack_packet(
            &Packet::new(
                crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
                &[crate::telemetry_schema::endpoint("AV_BAY_UNDERGLOW_OWNER")],
                "GS",
                1,
                Arc::from([1_u8]),
            )
            .unwrap(),
        )
        .to_vec();
        let size = frame.len();
        let budget = TxQueueBudget::new(2 * size);
        let (tx, mut rx) = mpsc::unbounded_channel();
        budget.try_send(&tx, 255, &frame).unwrap();
        budget.try_send(&tx, 255, &frame).unwrap();
        let (priority, first) = rx.try_recv().unwrap();
        assert!(matches!(
            budget.try_send(&tx, 0, &[2]),
            Err(sedsnet::TelemetryError::Io("side tx busy"))
        ));
        // Moving to a worker backlog or retrying a failed write frees nothing.
        assert_eq!(budget.pending.load(Ordering::Acquire), 2 * size);
        budget.complete(priority, &first);
        budget.try_send(&tx, 0, &[2]).unwrap();
        assert_eq!(budget.pending.load(Ordering::Acquire), size + 1);
    }

    #[test]
    fn radio_backpressure_retains_update_until_transport_drains() {
        use sedsnet::config::{
            register_data_type_with_description, register_endpoint_with_description,
        };
        use sedsnet::{MessageClass, MessageDataType, MessageElement, ReliableMode};
        let endpoint =
            register_endpoint_with_description("RADIO_BACKPRESSURE_SINK", "", false).unwrap();
        let ty = register_data_type_with_description(
            "RADIO_BACKPRESSURE_VALUE",
            "",
            MessageElement::Static(2, MessageDataType::UInt8, MessageClass::Data),
            &[endpoint],
            ReliableMode::None,
            200,
        )
        .unwrap();
        use sedsnet::router::{RouterConfig, RouterSideOptions};
        let budget = Arc::new(TxQueueBudget::new(2048));
        let (tx, mut rx) = mpsc::unbounded_channel();
        budget.try_send(&tx, 0, &[0; 2048]).unwrap();
        let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let tick = clock.clone();
        let router = Router::new_with_clock(
            RouterConfig::default(),
            Box::new(move || tick.load(Ordering::Acquire)),
        );
        let callback_budget = budget.clone();
        let side = router.add_side_packed_with_options(
            "busy",
            // Exercise ordinary best-effort backpressure here; actual managed
            // updates have their protected reserve checked above.
            move |frame| callback_budget.try_send(&tx, 0, frame),
            RouterSideOptions::default(),
        );
        // Discovery snapshots are regenerable/coalesced control state. Use an
        // application frame to verify the transport's accepted-update contract.
        router
            .rx_from_side(
                &sedsnet::discovery::build_discovery_announce(
                    "RADIO_BACKPRESSURE_PEER",
                    0,
                    &[endpoint],
                )
                .unwrap(),
                side,
            )
            .unwrap();
        let packet = Packet::new(ty, &[endpoint], "GS", 1, Arc::from([10u8, 11u8])).unwrap();
        router.tx(packet.clone()).unwrap();
        let (priority, filler) = rx.try_recv().unwrap();
        assert_eq!(filler.len(), 2048);
        assert!(rx.try_recv().is_err());
        budget.complete(priority, &filler);
        // Temporary refusals retry on the next millisecond instead of spinning
        // repeatedly on a transport that has just reported itself full.
        clock.store(1, Ordering::Release);
        router.process_tx_queue().unwrap();
        let mut found = false;
        while let Ok((priority, bytes)) = rx.try_recv() {
            budget.complete(priority, &bytes);
            if let Ok(decoded) = serialize::unpack_packet(&bytes) {
                // Sender names may resolve to a numeric address until discovery.
                // The wire receipt identity must survive that normalization.
                found |= decoded.packet_id() == packet.packet_id()
                    && decoded.data_type() == packet.data_type()
                    && decoded.payload() == packet.payload();
            }
        }
        assert!(
            found,
            "accepted update must survive temporary transport saturation"
        );
    }

    #[test]
    fn schema_burst_cannot_consume_ack_and_command_allowance() {
        let budget = TxQueueBudget::new(2048);
        let (tx, mut rx) = mpsc::unbounded_channel();
        for index in 0u16..8 {
            let mut chunk = vec![0u8; 1024];
            chunk[..4].copy_from_slice(b"SDT\x03");
            chunk[8..10].copy_from_slice(&index.to_le_bytes());
            chunk[10..12].copy_from_slice(&8u16.to_le_bytes());
            budget.try_send(&tx, 254, &chunk).unwrap();
        }
        assert!(budget.try_send(&tx, 0, &[0; 32]).is_err());
        budget.try_send(&tx, 255, &[1; 32]).unwrap();
        budget.try_send(&tx, 245, &[2; 32]).unwrap();
        assert_eq!(budget.pending_control.load(Ordering::Acquire), 64);
        assert!(budget.try_send(&tx, 255, &[0; 2048]).is_err());
        while let Ok((priority, payload)) = rx.try_recv() {
            budget.complete(priority, &payload);
        }
        assert_eq!(budget.pending.load(Ordering::Acquire), 0);
        assert_eq!(budget.pending_control.load(Ordering::Acquire), 0);
        drop(rx);
        assert!(budget.try_send(&tx, 255, &[0; 32]).is_err());
        assert_eq!(budget.pending.load(Ordering::Acquire), 0);
        assert_eq!(budget.pending_control.load(Ordering::Acquire), 0);
    }

    #[test]
    fn short_schema_chunks_keep_the_existing_byte_budget() {
        let budget = TxQueueBudget::new(2048);
        let (tx, mut rx) = mpsc::unbounded_channel();
        for index in 0u16..200 {
            let mut chunk = vec![0; 256];
            chunk[..4].copy_from_slice(b"SDT\x03");
            chunk[8..10].copy_from_slice(&index.to_le_bytes());
            chunk[10..12].copy_from_slice(&200u16.to_le_bytes());
            budget.try_send(&tx, 254, &chunk).unwrap();
        }
        budget.try_send(&tx, 255, &[1; 32]).unwrap();
        assert_eq!(budget.pending.load(Ordering::Acquire), 51_232);
        while let Ok((priority, payload)) = rx.try_recv() {
            budget.complete(priority, &payload);
        }
        assert_eq!(budget.pending.load(Ordering::Acquire), 0);
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
            move |frame| callback_budget.try_send(&callback_tx, 254, frame),
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
        assert!(budget.try_send(&tx, 0, &[0; 100]).is_err());
        let mut frames = 0;
        while let Ok((priority, payload)) = rx.try_recv() {
            budget.complete(priority, &payload);
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
            if worker_name == "rocket_comms" {
                // No modem turn scheduling: this only bounds bytes offloaded
                // at UART speed into a slower finite-capacity radio modem.
                tx_worker_comms.lock().unwrap().enable_radio_capacity_control();
            }
            let mut comms_shutdown_rx = tx_worker_state.shutdown_subscribe();
            let mut last_send_error_log_ms = 0;
            let mut suppressed_send_errors = 0;
            let mut next_tx_allowed_at = TxDeadline::now();
            // Pico-Fi and radio sides both use stateful SDT dictionaries.
            // Preserve their relative order on every generic packed link.
            let mut backlog = WireBacklog::new(true);
            loop {
                match comms_shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(broadcast::error::TryRecvError::Closed)
                    | Err(broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }

                if !next_tx_allowed_at.ready() {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }

                let mut sent_any = false;
                for _ in 0..GENERAL_COMMS_TX_BURST {
                    // Pick up newly queued commands before every frame, not
                    // after transmitting an entire burst of large schemas.
                    // Bound draining so a busy producer cannot starve writes.
                    for _ in 0..256 {
                        match comms_handle.tx_rx.try_recv() {
                            Ok((priority, payload)) => backlog.push(priority, payload, false),
                            Err(mpsc::error::TryRecvError::Empty) => break,
                            Err(mpsc::error::TryRecvError::Disconnected) => return,
                        }
                    }
                    let mut comms = tx_worker_comms.lock().expect("failed to get lock");
                    // Mesh HEALTH/READY must not depend on receiving another
                    // radio byte. Service protected egress before SDK queue
                    // ownership/priority selection, even when SDK is idle.
                    if let Err(error) = comms.service_transmit() {
                        log_repeated_worker_error(
                            &format!("{worker_name} comms worker transport service failed"),
                            &error.to_string(), &mut last_send_error_log_ms,
                            &mut suppressed_send_errors,
                        );
                        break;
                    }
                    if !backlog.has_eligible_critical()
                        && let Err(error) = comms.service_background_mode()
                    {
                        log_repeated_worker_error(
                            &format!("{worker_name} comms worker background mode service failed"),
                            &error.to_string(),
                            &mut last_send_error_log_ms,
                            &mut suppressed_send_errors,
                        );
                        break;
                    }
                    let selection_state = (
                        backlog.application_bytes,
                        backlog.ordered_metadata_next,
                        backlog.priority_bytes,
                        backlog.control_bytes,
                    );
                    let selected = if comms.transmit_ready() {
                        backlog.pop()
                    } else {
                        backlog.pop_critical()
                    };
                    let Some((priority, payload)) = selected else {
                        break;
                    };
                    if !comms.transmit_ready_for(priority, &payload) {
                        (
                            backlog.application_bytes,
                            backlog.ordered_metadata_next,
                            backlog.priority_bytes,
                            backlog.control_bytes,
                        ) = selection_state;
                        backlog.push(priority, payload, true);
                        break;
                    }
                    if std::env::var_os("GS_SIM_TRACE_RELIABLE").is_some() {
                        log::info!("full-bay transport selected side={worker_name} sim={:?} priority={priority} len={} type={:?}", crate::simulation_clock_ms(), payload.len(), serialize::peek_frame_info_unchecked(&payload).map(|info| info.envelope.ty));
                    }
                    let follower = backlog.batch_follower(&**comms, (priority, &payload));
                    let third = follower.as_ref().and_then(|(next_priority, next_payload)| {
                        let bytes = comms.command_batch_frame_len(priority, &payload)?
                            .checked_add(comms.command_batch_frame_len(*next_priority, next_payload)?)?;
                        backlog.batch_follower_after_bytes(&**comms, bytes)
                    });
                    let result = if let (Some((p2, b2)), Some((p3, b3))) = (follower.as_ref(), third.as_ref()) {
                        comms.send_command_batch(&[(priority, &payload), (*p2, b2), (*p3, b3)])
                    } else if let Some((next_priority, next_payload)) = follower.as_ref() {
                        comms.send_command_batch(&[(priority, &payload), (*next_priority, next_payload)])
                    } else {
                        comms.send_data_with_priority(&payload, priority)
                    };
                    match result {
                        Ok(()) => {
                            if let Some(budget) = &comms_handle.tx_budget {
                                budget.complete(priority, &payload);
                            }
                            if let Some(activity) = &comms_handle.transport_activity {
                                activity.complete();
                            }
                            log_link_control_send(worker_name, &payload);
                            log_radio_command_event("radio TX sent", worker_name, &payload);
                            for (next_priority, next_payload) in follower.iter().chain(third.iter()) {
                                if let Some(budget) = &comms_handle.tx_budget {
                                    budget.complete(*next_priority, next_payload);
                                }
                                if let Some(activity) = &comms_handle.transport_activity {
                                    activity.complete();
                                }
                                log_link_control_send(worker_name, next_payload);
                                log_radio_command_event("radio TX sent", worker_name, next_payload);
                            }
                            sent_any = true;
                            if suppressed_send_errors > 0 {
                                eprintln!(
                                    "{worker_name} comms worker send_data recovered after suppressing {suppressed_send_errors} repeated errors"
                                );
                                suppressed_send_errors = 0;
                                last_send_error_log_ms = 0;
                            }
                            // RFD firmware owns MAC/turn scheduling. The UART
                            // device enforces shared capacity before UART offload;
                            // no LoRa cooldown, AUX or receive window is used.
                            next_tx_allowed_at = TxDeadline::now();
                        }
                        Err(e) => {
                            // Logical completion is unconfirmed: a partial UART
                            // write may already be on wire. Preserve every owner
                            // and retry their original IDs in the original order.
                            if let Some((third_priority, third_payload)) = third {
                                backlog.push(third_priority, third_payload, true);
                            }
                            backlog.restore_selected(selection_state, (priority, payload), follower);
                            next_tx_allowed_at = TxDeadline::now().after(Duration::from_millis(COMMS_TX_GAP_MS));
                            log_repeated_worker_error(
                                &format!("{worker_name} comms worker send_data failed"),
                                &e.to_string(),
                                &mut last_send_error_log_ms,
                                &mut suppressed_send_errors,
                            );
                            break;
                        }
                    }
                }

                if sent_any {
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
            let mut pending_tx = None;
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
                    let selected = pending_tx.take().map(Ok).unwrap_or_else(|| comms_handle.tx_rx.try_recv());
                    match selected {
                        Ok((priority, payload)) => {
                            sent_any = true;
                            match comms.send_data_with_priority(&payload, priority) {
                                Ok(()) => {
                                    if let Some(budget) = &comms_handle.tx_budget { budget.complete(priority, &payload); }
                                    if let Some(activity) = &comms_handle.transport_activity { activity.complete(); }
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
                                    pending_tx = Some((priority, payload));
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
            let mut command_backlog: VecDeque<(u8, Vec<u8>)> = VecDeque::new();
            let mut telemetry_backlog: VecDeque<(u8, Vec<u8>)> = VecDeque::new();
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

                    comms_handle.transport_activity.as_deref(),
                    comms_handle.tx_budget.as_deref(),
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

                    comms_handle.transport_activity.as_deref(),
                    comms_handle.tx_budget.as_deref(),
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

                    comms_handle.transport_activity.as_deref(),
                    comms_handle.tx_budget.as_deref(),
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

                    comms_handle.transport_activity.as_deref(),
                    comms_handle.tx_budget.as_deref(),
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

                    comms_handle.transport_activity.as_deref(),
                    comms_handle.tx_budget.as_deref(),
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

                    comms_handle.transport_activity.as_deref(),
                    comms_handle.tx_budget.as_deref(),
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

                    comms_handle.transport_activity.as_deref(),
                    comms_handle.tx_budget.as_deref(),
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

            let mut pending_tx = None;
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
                            && let Some((priority, payload)) = pending_tx.take().or_else(|| comms_handle.tx_rx.try_recv().ok())
                        {
                            match comms.send_data_with_priority(&payload, priority) {
                                Ok(()) => {
                                    if let Some(budget) = &comms_handle.tx_budget { budget.complete(priority, &payload); }
                                    if let Some(activity) = &comms_handle.transport_activity { activity.complete(); }
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
                                    pending_tx = Some((priority, payload));
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
    // User-confirmed ATS2=64. UART remains 115200; RF firmware uses the same budget.
    *BPS.get_or_init(|| env_usize("GS_RADIO_AIR_BIT_RATE_BPS", 64_000, 300, 250_000) as u64)
}

fn radio_air_frame_overhead_bytes() -> u64 {
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| env_usize("GS_RADIO_AIR_FRAME_OVERHEAD_BYTES", 16, 0, 128) as u64)
}

fn radio_tx_cooldown_ms() -> u64 {
    static DELAY_MS: OnceLock<u64> = OnceLock::new();
    *DELAY_MS.get_or_init(|| env_usize("GS_RADIO_TX_COOLDOWN_MS", 0, 0, 1_000) as u64)
}

fn radio_air_budget(
    payload_len: usize,
    air_bps: u64,
    share_percent: u64,
    overhead: u64,
) -> Duration {
    let bytes = payload_len as u128 + 4 + overhead as u128;
    let bps = (air_bps as u128 * share_percent.clamp(1, 100) as u128 / 100).max(1);
    let ms = (bytes * 8 * 1_000).div_ceil(bps).min(u64::MAX as u128);
    Duration::from_millis(ms as u64)
}

fn radio_air_time_for_payload(payload_len: usize) -> Duration {
    // Each end may consume 40% of the shared half-duplex link. Keep packets in
    // the bounded priority queue rather than flooding the modem without CTS.
    // With optional RRLS voice, two hosts may share the radio with RF.
    // 25% + 25% + RF's 40% leaves 10% for additional modem overhead.
    let voice_enabled = std::env::var("GS_RRLS_VOICE").as_deref() == Ok("1");
    let share = env_usize(
        "GS_RADIO_AIR_SHARE_PERCENT",
        if voice_enabled { 25 } else { 40 },
        1,
        100,
    ) as u64;
    radio_air_budget(
        payload_len,
        radio_air_bit_rate_bps(),
        share,
        radio_air_frame_overhead_bytes(),
    )
    .saturating_add(Duration::from_millis(radio_tx_cooldown_ms()))
}

#[cfg(test)]
mod rfd_air_budget_tests {
    use super::*;
    #[test]
    fn radio_chunk_leaves_control_latency_headroom() {
        assert!(
            radio_air_budget(crate::RADIO_SIDE_MAX_FRAME_BYTES, 64_000, 40, 16)
                <= Duration::from_millis(70)
        );
    }
    #[test]
    fn budgets_shared_air_link_instead_of_uart_baud() {
        assert_eq!(
            radio_air_budget(100, 64_000, 40, 16),
            Duration::from_millis(38)
        );
        assert_eq!(
            radio_air_budget(1024, 64_000, 40, 16),
            Duration::from_millis(327)
        );
        assert_eq!(
            radio_air_budget(256, 64_000, 40, 16),
            Duration::from_millis(87)
        );
        assert_eq!(
            radio_air_budget(100, 64_000, 80, 16),
            Duration::from_millis(19)
        );
        assert!(radio_air_budget(usize::MAX, 0, 0, 128).as_millis() > 0);
    }
    #[test]
    fn voice_frame_fits_a_quarter_share_without_using_the_rocket_reservation() {
        assert_eq!(
            radio_air_budget(79, 64_000, 25, 16),
            Duration::from_millis(50)
        );
        assert!(radio_air_budget(79, 64_000, 25, 16) < Duration::from_millis(60));
    }
    #[test]
    fn slow_emulation_cannot_spend_virtual_radio_airtime_using_wall_time() {
        let wall = std::time::Instant::now();
        let deadline = TxDeadline {
            wall,
            simulated_ms: Some(1000),
        }
        .after(Duration::from_millis(87));
        assert!(!deadline.ready_at(wall + Duration::from_secs(10), Some(1086)));
        assert!(deadline.ready_at(wall, Some(1087)));
        let real = TxDeadline {
            wall,
            simulated_ms: None,
        }
        .after(Duration::from_millis(87));
        assert!(!real.ready_at(wall + Duration::from_millis(86), None));
        assert!(real.ready_at(wall + Duration::from_millis(87), None));
    }
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
    if std::env::var_os("GS_SIM_TRACE_RELIABLE").is_some()
        && ["FLIGHT_STATE", "AV_BAY_UNDERGLOW", "FLIGHT_BUZZER"]
            .iter()
            .any(|name| pkt.data_type() == crate::telemetry_schema::data_type(name))
    {
        log::info!(
            "full-bay control transport TX side={worker_name} sim={:?} type={:?} source_ms={} packet_id={:#x} owners={:?}",
            crate::simulation_clock_ms(),
            pkt.data_type(),
            pkt.timestamp(),
            pkt.packet_id(),
            serialize::peek_envelope(payload).map(|envelope| envelope.target_senders)
        );
    }
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
    // Success here means the transport accepted the packet; an RRLS observer
    // may deliberately discard it before any serial write.
    log::debug!(
        "link tx accepted side={worker_name} ty={:?} sender={} endpoints={:?} payload={payload_preview}",
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
    command_backlog: &mut VecDeque<(u8, Vec<u8>)>,
    telemetry_backlog: &mut VecDeque<(u8, Vec<u8>)>,
    radio_tx_backlog_limit: usize,
    transport_activity: Option<&TransportTxActivity>,
    tx_budget: Option<&TxQueueBudget>,
) -> bool {
    loop {
        match tx_rx.try_recv() {
            Ok((priority, payload)) => {
                log_radio_command_event("radio TX backlog", worker_name, &payload);
                let repeats =
                    if worker_name == "rocket_comms" && is_flight_command_payload(&payload) {
                        radio_flight_command_repeats()
                    } else {
                        1
                    };
                for _ in 0..repeats {
                    if is_command_payload(&payload) {
                        command_backlog.push_back((priority, payload.clone()));
                    } else {
                        telemetry_backlog.push_back((priority, payload.clone()));
                    }
                }
                while telemetry_backlog.len() > radio_tx_backlog_limit {
                    if let Some((priority, payload)) = telemetry_backlog.pop_front() {
                        if let Some(budget) = tx_budget { budget.complete(priority, &payload); }
                        if let Some(activity) = transport_activity { activity.complete(); }
                    }
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
    command_backlog: &mut VecDeque<(u8, Vec<u8>)>,
    telemetry_backlog: &mut VecDeque<(u8, Vec<u8>)>,
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
    transport_activity: Option<&TransportTxActivity>,
    tx_budget: Option<&TxQueueBudget>,
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
        let Some((_, payload)) = (if sending_command {
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

        let Some((priority, payload)) = (if sending_command {
            command_backlog.pop_front()
        } else {
            telemetry_backlog.pop_front()
        }) else {
            break;
        };
        log_radio_packet_event("radio TX pop", worker_name, &payload);
        maybe_log_green_radio_command_send(worker_name, &payload);
        match comms.send_data_with_priority(&payload, priority) {
            Ok(()) => {
                if let Some(budget) = tx_budget { budget.complete(priority, &payload); }
                if let Some(activity) = transport_activity { activity.complete(); }
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
                    command_backlog.push_front((priority, payload));
                } else {
                    telemetry_backlog.push_front((priority, payload));
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
#[cfg(test)]
mod rfd_worker_tests {
    use super::*;
    use std::error::Error;

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn primary_actual_uart_worker_mode_reply_progresses_under_sdt_and_defers_to_command() {
        use serialport::SerialPort;
        use std::io::Read;
        crate::telemetry_schema::initialize().unwrap();
        let command = serialize::pack_packet(
            &Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
                &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
                "GS",
                1,
                Arc::from([1u8]),
            )
            .unwrap(),
        )
        .to_vec();
        let bulk = include_bytes!("../test-fixtures/mode-background-captured-sdt.bin").to_vec();
        let shared = Arc::new(Mutex::new(crate::rrls::Mesh::new(
            crate::rrls::Role::Primary,
            42,
        )));
        let mut reply = b"OM\x02\x01".to_vec();
        reply.extend(42u32.to_le_bytes());
        reply.extend(123u128.to_le_bytes());
        reply.push(0);
        assert!(shared.lock().unwrap().queue_operating_mode_reply(&reply, 0));
        let wrapped = {
            let mut mesh = shared.lock().unwrap();
            let owner = mesh.begin_mode_reply_transmission(0).unwrap();
            let bytes = owner.frame().bytes.clone();
            mesh.finish_transmission(owner, false);
            bytes
        };
        let mut expected_wire = vec![0xA6, 0x5B];
        expected_wire.extend((wrapped.len() as u16).to_le_bytes());
        expected_wire.extend(wrapped);

        let (mut peer, port) = serialport::TTYPort::pair().unwrap();
        peer.set_timeout(Duration::from_millis(10)).unwrap();
        let mut uart = crate::comms::UartComms::test_mesh_pty(port, shared.clone());
        uart.set_side_id(0);
        let tx = uart.try_clone_tx().unwrap().unwrap();
        let state = crate::state::tests::test_app_state().await;
        let (sender, receiver) = mpsc::unbounded_channel();
        let budget = Arc::new(TxQueueBudget::new(32768));
        budget.try_send(&sender, 255, &command).unwrap();
        for _ in 0..64 {
            budget.try_send(&sender, 0, &bulk).unwrap();
        }
        let workers = spawn_comms_worker_threads(
            Arc::new(Router::new(Default::default())),
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                name: "rocket_comms",
                tx_budget: Some(budget.clone()),
                comms: Arc::new(Mutex::new(Box::new(uart))),
                tx_comms: Some(Arc::new(Mutex::new(tx))),
                side_id: 0,
                tx_rx: receiver,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: false,
            },
        )
        .unwrap();
        let expected_frame = expected_wire.clone();
        let (captured, _peer_guard) = tokio::task::spawn_blocking(move || {
            let start = std::time::Instant::now();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 2048];
            while start.elapsed() < Duration::from_secs(2) {
                if let Ok(n) = peer.read(&mut chunk) {
                    bytes.extend_from_slice(&chunk[..n]);
                    if bytes
                        .windows(expected_frame.len())
                        .any(|w| w == expected_frame)
                    {
                        break;
                    }
                }
            }
            (bytes, peer)
        })
        .await
        .unwrap();
        let pending = budget.pending.load(Ordering::Acquire);
        state.request_shutdown();
        drop(sender);
        for worker in workers {
            worker.join().unwrap();
        }
        let mode = captured
            .windows(reply.len())
            .position(|w| w == reply)
            .expect("mode reply starved behind sustained SDK SDT backlog");
        let control = captured
            .windows(command.len())
            .position(|w| w == command)
            .expect("canonical command missing");
        let frame_start = captured
            .windows(expected_wire.len())
            .position(|w| w == expected_wire)
            .expect("exact production Mesh envelope and CRC inside raw UART frame");
        assert_eq!(mode, frame_start + 8);
        assert_eq!(&captured[frame_start..frame_start + 2], &[0xA6, 0x5B]);
        let framed_length = 4 + usize::from(u16::from_le_bytes(
            captured[frame_start + 2..frame_start + 4]
                .try_into()
                .unwrap(),
        ));
        assert_eq!(
            framed_length,
            reply.len() + 12,
            "charged wire includes raw4+Mesh8 overhead"
        );
        assert_eq!(framed_length, 37);
        assert!(
            captured.len() >= frame_start + framed_length,
            "full owned frame including MeshCRC accepted"
        );
        println!(
            "MODE_BACKGROUND_ACTUAL_UART wire_bytes={framed_length} payload_bytes={} sdk_pending_bytes={pending}",
            reply.len()
        );
        assert!(
            control < mode,
            "eligible SDK critical command precedes background mode"
        );
        assert!(
            pending > 0,
            "mode progresses while ordinary SDK work remains pending"
        );
    }
    struct CaptureDevice {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        done: std::sync::mpsc::Sender<()>,
        fail_first: Arc<std::sync::atomic::AtomicBool>,
    }
    impl CommsDevice for CaptureDevice {
        fn recv_packet(
            &mut self,
            _: &Router,
            _: &mut dyn FnMut(&Packet),
        ) -> sedsnet::TelemetryResult<()> {
            Ok(())
        }
        fn send_data(&mut self, bytes: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
            if self.fail_first.swap(false, Ordering::SeqCst) {
                return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock).into());
            }
            let mut sent = self.sent.lock().unwrap();
            sent.push(bytes.to_vec());
            if sent.len() == 32 {
                let _ = self.done.send(());
            }
            Ok(())
        }
        fn set_side_id(&mut self, _: RouterSideId) {}
    }

    async fn run_rfd_worker(fail: bool) {
        let state = crate::state::tests::test_app_state().await;
        let router = Arc::new(Router::new(Default::default()));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (done, complete) = std::sync::mpsc::channel();
        let fail_first = Arc::new(std::sync::atomic::AtomicBool::new(fail));
        let device = || {
            Box::new(CaptureDevice {
                sent: sent.clone(),
                done: done.clone(),
                fail_first: fail_first.clone(),
            }) as Box<dyn CommsDevice>
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        crate::telemetry_schema::initialize().unwrap();
        let mut expected = Vec::new();
        for marker in 0..32u8 {
            let packet = Packet::new(
                crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
                &[crate::telemetry_schema::endpoint("AV_BAY_UNDERGLOW_OWNER")],
                "GS",
                u64::from(marker),
                Arc::from([marker]),
            )
            .unwrap();
            let bytes = serialize::pack_packet(&packet).to_vec();
            sender.send((16, bytes.clone())).unwrap();
            expected.push(bytes);
        }
        let workers = spawn_comms_worker_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                name: "rocket_comms",
                tx_budget: None,
                comms: Arc::new(Mutex::new(device())),
                tx_comms: Some(Arc::new(Mutex::new(device()))),
                side_id: 0,
                tx_rx: receiver,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: false,
            },
        )
        .unwrap();
        // A frozen simulator clock cannot satisfy an artificial air deadline.
        // A real UART would supply its own DMA/driver readiness instead.
        let received =
            tokio::task::spawn_blocking(move || complete.recv_timeout(Duration::from_secs(2)))
                .await
                .unwrap();
        state.request_shutdown();
        drop(sender);
        for worker in workers {
            worker.join().unwrap();
        }
        received.expect("RFD TX worker did not drain after device capacity became ready");
        let captured = sent.lock().unwrap();
        assert_eq!(*captured, expected);
    }
    #[tokio::test]
    async fn rfd_worker_respects_device_capacity_without_lora_turn_windows() {
        run_rfd_worker(false).await;
    }
    #[tokio::test]
    async fn rfd_worker_preserves_order_after_transient_write_failure() {
        run_rfd_worker(true).await;
    }

    struct MeshPriorityDevice {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        credit: Arc<AtomicUsize>,
        pending_health: Arc<std::sync::atomic::AtomicBool>,
    }
    impl CommsDevice for MeshPriorityDevice {
        fn service_transmit(&mut self) -> sedsnet::TelemetryResult<()> {
            if self.credit.load(Ordering::Acquire) > 0
                && self.pending_health.swap(false, Ordering::AcqRel)
            {
                self.credit.fetch_sub(1, Ordering::AcqRel);
                self.sent.lock().unwrap().push(b"HEALTH".to_vec());
            }
            Ok(())
        }
        fn transmit_ready(&self) -> bool {
            self.credit.load(Ordering::Acquire) > 0
        }
        fn recv_packet(
            &mut self,
            _: &Router,
            _: &mut dyn FnMut(&Packet),
        ) -> sedsnet::TelemetryResult<()> {
            Ok(())
        }
        fn send_data(&mut self, bytes: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
            assert!(self.credit.fetch_sub(1, Ordering::AcqRel) > 0);
            self.sent.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn set_side_id(&mut self, _: RouterSideId) {}
    }
    #[tokio::test]
    async fn queued_mesh_health_gets_capacity_before_sdk_and_keeps_its_reservation() {
        let state = crate::state::tests::test_app_state().await;
        let sent = Arc::new(Mutex::new(Vec::new()));
        let credit = Arc::new(AtomicUsize::new(1));
        let pending_health = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let device = || {
            Box::new(MeshPriorityDevice {
                sent: sent.clone(),
                credit: credit.clone(),
                pending_health: pending_health.clone(),
            }) as Box<dyn CommsDevice>
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        let frame = vec![0xa1, 0xb2];
        let budget = Arc::new(TxQueueBudget::new(4096));
        budget.try_send(&sender, 16, &frame).unwrap();
        let workers = spawn_comms_worker_threads(
            Arc::new(Router::new(Default::default())),
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                name: "rocket_comms",
                tx_budget: Some(budget.clone()),
                comms: Arc::new(Mutex::new(device())),
                tx_comms: Some(Arc::new(Mutex::new(device()))),
                side_id: 0,
                tx_rx: receiver,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: false,
            },
        )
        .unwrap();
        let result = async {
            tokio::time::timeout(Duration::from_secs(2), async {
                while sent.lock().unwrap().is_empty() {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .map_err(|_| "worker stalled")?;
            if sent.lock().unwrap()[0] != b"HEALTH" {
                return Err("SDK bulk took capacity before queued mesh health");
            }
            if budget.pending.load(Ordering::Acquire) != frame.len() {
                return Err("SDK reservation released before UART offload");
            }
            credit.store(1, Ordering::Release);
            tokio::time::timeout(Duration::from_secs(2), async {
                while sent.lock().unwrap().len() < 2 {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .map_err(|_| "SDK data never resumed")?;
            Ok(())
        }
        .await;
        state.request_shutdown();
        drop(sender);
        for worker in workers {
            worker.join().unwrap();
        }
        result.unwrap();
        assert_eq!(*sent.lock().unwrap(), vec![b"HEALTH".to_vec(), frame]);
        assert_eq!(budget.pending.load(Ordering::Acquire), 0);
    }

    struct ReadinessDevice {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        credit: Arc<AtomicUsize>,
    }
    #[tokio::test]
    async fn critical_head_is_inspected_before_ordinary_capacity_refusal() {
        struct LoanDevice {
            sent: Arc<Mutex<Vec<Vec<u8>>>>,
            unused: Arc<std::sync::atomic::AtomicBool>,
        }
        impl CommsDevice for LoanDevice {
            fn transmit_ready(&self) -> bool {
                false
            }
            fn transmit_ready_for(&self, priority: u8, payload: &[u8]) -> bool {
                self.unused.load(Ordering::Acquire)
                    && crate::comms::closes_radio_packet_batch(priority, payload)
            }
            fn recv_packet(
                &mut self,
                _: &Router,
                _: &mut dyn FnMut(&Packet),
            ) -> sedsnet::TelemetryResult<()> {
                Ok(())
            }
            fn set_side_id(&mut self, _: RouterSideId) {}
            fn send_data(&mut self, _: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
                panic!("trusted priority lost")
            }
            fn send_data_with_priority(
                &mut self,
                bytes: &[u8],
                priority: u8,
            ) -> Result<(), Box<dyn Error + Send + Sync>> {
                assert!(crate::comms::closes_radio_packet_batch(priority, bytes));
                assert!(self.unused.swap(false, Ordering::AcqRel));
                self.sent.lock().unwrap().push(bytes.to_vec());
                Ok(())
            }
        }
        crate::telemetry_schema::initialize().unwrap();
        let packet = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            "GS",
            1,
            Arc::from([1]),
        )
        .unwrap();
        let command = serialize::pack_packet(&packet).to_vec();
        let bulk = b"SDT opaque schema retains FIFO ownership".to_vec();
        let state = crate::state::tests::test_app_state().await;
        let sent = Arc::new(Mutex::new(Vec::new()));
        let unused = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let budget = Arc::new(TxQueueBudget::new(4096));
        let (sender, receiver) = mpsc::unbounded_channel();
        let heartbeat = Packet::new(
            crate::telemetry_schema::data_type("HEARTBEAT"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GS",
            1,
            Arc::from([]),
        )
        .unwrap();
        let heartbeat = serialize::pack_packet(&heartbeat).to_vec();
        budget.try_send(&sender, 253, &heartbeat).unwrap();
        budget.try_send(&sender, 0, &bulk).unwrap();
        budget.try_send(&sender, 200, &command).unwrap();
        let device = || {
            Box::new(LoanDevice {
                sent: sent.clone(),
                unused: unused.clone(),
            }) as Box<dyn CommsDevice>
        };
        let workers = spawn_comms_worker_threads(
            Arc::new(Router::new(Default::default())),
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                name: "rocket_comms",
                tx_budget: Some(budget.clone()),
                comms: Arc::new(Mutex::new(device())),
                tx_comms: Some(Arc::new(Mutex::new(device()))),
                side_id: 0,
                tx_rx: receiver,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: false,
            },
        )
        .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            while sent.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
        state.request_shutdown();
        drop(sender);
        for worker in workers {
            worker.join().unwrap();
        }
        result.expect("ordinary readiness hid the eligible critical head");
        assert_eq!(*sent.lock().unwrap(), vec![command]);
        assert_eq!(
            budget.pending.load(Ordering::Acquire),
            bulk.len() + heartbeat.len(),
            "refused bulk keeps its exact reservation"
        );
    }
    impl CommsDevice for ReadinessDevice {
        fn transmit_ready(&self) -> bool {
            self.credit.load(Ordering::Acquire) > 0
        }
        fn recv_packet(
            &mut self,
            _: &Router,
            _: &mut dyn FnMut(&Packet),
        ) -> sedsnet::TelemetryResult<()> {
            Ok(())
        }
        fn send_data(&mut self, bytes: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
            assert!(self.credit.fetch_sub(1, Ordering::AcqRel) > 0);
            self.sent.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn set_side_id(&mut self, _: RouterSideId) {}
    }

    #[tokio::test]
    async fn uart_backpressure_retains_budget_and_selects_new_controls_before_bulk() {
        let state = crate::state::tests::test_app_state().await;
        let router = Arc::new(Router::new(Default::default()));
        crate::telemetry_schema::initialize().unwrap();
        let frames = (0..3u8)
            .map(|marker| {
                let packet = Packet::new(
                    crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
                    &[crate::telemetry_schema::endpoint("AV_BAY_UNDERGLOW_OWNER")],
                    "GS",
                    u64::from(marker),
                    Arc::from([marker]),
                )
                .unwrap();
                serialize::pack_packet(&packet).to_vec()
            })
            .collect::<Vec<_>>();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let credit = Arc::new(AtomicUsize::new(1));
        let device = || {
            Box::new(ReadinessDevice {
                sent: sent.clone(),
                credit: credit.clone(),
            }) as Box<dyn CommsDevice>
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        let budget = Arc::new(TxQueueBudget::new(4096));
        budget.try_send(&sender, 16, &frames[0]).unwrap();
        budget.try_send(&sender, 16, &frames[1]).unwrap();
        let workers = spawn_comms_worker_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                name: "rocket_comms",
                tx_budget: Some(budget.clone()),
                comms: Arc::new(Mutex::new(device())),
                tx_comms: Some(Arc::new(Mutex::new(device()))),
                side_id: 0,
                tx_rx: receiver,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: false,
            },
        )
        .unwrap();
        let observations = async {
            for count in 1..=3 {
                tokio::time::timeout(Duration::from_secs(2), async {
                    while sent.lock().unwrap().len() < count
                        || (count == 1 && budget.pending.load(Ordering::Acquire) != frames[1].len())
                    {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                })
                .await
                .map_err(|_| "UART worker failed to advance or retain transport ownership")?;
                if count == 1 {
                    if budget.pending.load(Ordering::Acquire) != frames[1].len() {
                        return Err("transport budget was released before the write");
                    }
                    budget.try_send(&sender, 255, &frames[2]).unwrap();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    if sent.lock().unwrap().len() != 1 {
                        return Err("worker wrote while UART readiness was false");
                    }
                }
                if count < 3 {
                    credit.store(1, Ordering::Release);
                }
            }
            Ok(())
        }
        .await;
        state.request_shutdown();
        drop(sender);
        for worker in workers {
            worker.join().unwrap();
        }
        observations.unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            vec![frames[0].clone(), frames[2].clone(), frames[1].clone()]
        );
        assert_eq!(budget.pending.load(Ordering::Acquire), 0);
        assert_eq!(budget.pending_control.load(Ordering::Acquire), 0);
    }
}

#[cfg(test)]
mod transport_pending_tests {
    use super::*;
    use std::error::Error;
    struct BlockThenRetry { expected_priority:u8, attempts: Arc<AtomicUsize>, release: Arc<AtomicUsize>, entered: std::sync::mpsc::Sender<usize> }
    impl CommsDevice for BlockThenRetry {
        fn recv_packet(&mut self, _: &Router, _: &mut dyn FnMut(&Packet)) -> sedsnet::TelemetryResult<()> { Ok(()) }
        fn send_data(&mut self, _: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
            let attempt=self.attempts.fetch_add(1,Ordering::AcqRel);self.entered.send(attempt).unwrap();
            while self.release.load(Ordering::Acquire)<=attempt { thread::sleep(Duration::from_millis(1)); }
            if attempt==0 { return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock).into()); } Ok(())
        }
        fn send_data_with_priority(&mut self, bytes:&[u8],priority:u8)->Result<(),Box<dyn Error+Send+Sync>> {
            assert_eq!(priority,self.expected_priority,"worker lost the selected priority across retry");
            self.send_data(bytes)
        }
        fn set_side_id(&mut self, _: RouterSideId) {}
    }
    #[test]
    fn rejected_enqueue_does_not_leave_phantom_transport_ownership() {
        let activity=TransportTxActivity::default();
        assert!(activity.try_enqueue(||Err(sedsnet::TelemetryError::Io("side tx busy"))).is_err());assert!(!activity.has_pending());
        activity.try_enqueue(||{assert!(activity.has_pending(),"ownership must precede handoff");Ok(())}).unwrap();
        assert!(activity.has_pending());activity.complete();assert!(!activity.has_pending());
    }
    #[tokio::test]
    async fn software_owner_survives_queue_pop_active_write_and_retry_in_all_workers() {
        for (legacy,prioritize_rx) in [(false,false),(true,false),(false,true)] {
            let state=crate::state::tests::test_app_state().await;let activity=state.transport_tx_activity.clone();
            let attempts=Arc::new(AtomicUsize::new(0));let release=Arc::new(AtomicUsize::new(0));
            let (entered,observed)=std::sync::mpsc::channel();let (sender,receiver)=mpsc::unbounded_channel();
            activity.try_enqueue(||sender.send((16,vec![1,2,3])).map_err(|_|sedsnet::TelemetryError::Io("test queue closed"))).unwrap();
            assert!(state.has_pending_transport_tx(),"queued work must block clear");
            let workers=spawn_comms_worker_threads(Arc::new(Router::new(Default::default())),state.clone(),CommsWorkerHandle {
                transport_activity:Some(activity.clone()),name:"pending_fixture",tx_budget:None,
                comms:Arc::new(Mutex::new(Box::new(BlockThenRetry {expected_priority:16,attempts:attempts.clone(),release:release.clone(),entered}))),
                tx_comms:None,side_id:0,tx_rx:receiver,legacy_single_worker:legacy,prioritize_rx,dedicated_radio_io:false,
            }).unwrap();
            let result=(||->Result<(),&'static str>{
                if observed.recv_timeout(Duration::from_secs(2))!=Ok(0){return Err("first write not reached");}
                if !state.has_pending_transport_tx(){return Err("ownership disappeared during active write");}
                release.store(1,Ordering::Release);
                if observed.recv_timeout(Duration::from_secs(2))!=Ok(1){return Err("failed write dropped instead of retried");}
                if !state.has_pending_transport_tx(){return Err("ownership disappeared across retry");}
                release.store(2,Ordering::Release);
                for _ in 0..2000 {if !activity.has_pending(){return Ok(());}thread::sleep(Duration::from_millis(1));}
                Err("successful write did not release ownership")
            })();
            release.store(usize::MAX,Ordering::Release);state.request_shutdown();drop(sender);
            for worker in workers {worker.join().unwrap();}
            assert!(result.is_ok(),"legacy={legacy} rx_priority={prioritize_rx}: {result:?}");assert!(!activity.has_pending());
        }
    }
    #[test]
    fn dedicated_retry_retains_software_owner_and_exact_capacity_priority() {
        let activity=TransportTxActivity::default();let budget=TxQueueBudget::new(2048);
        let (sender,mut receiver)=mpsc::unbounded_channel();
        activity.try_enqueue(||budget.try_send(&sender,0,&[1,2,3])).unwrap();
        let mut commands=VecDeque::new();let mut telemetry=VecDeque::new();
        assert!(drain_radio_tx_queue(&mut receiver,"fixture",&mut commands,&mut telemetry,8,Some(&activity),Some(&budget)));
        let (entered,_observed)=std::sync::mpsc::channel();
        let mut device=BlockThenRetry {expected_priority:0,attempts:Arc::new(AtomicUsize::new(0)),release:Arc::new(AtomicUsize::new(usize::MAX)),entered};
        let now=std::time::Instant::now();let mut until=Some(now+Duration::from_secs(4));let mut uplink=true;
        let mut sent=0;let mut last=None;let mut next=None;let mut error_time=0;let mut suppressed=0;
        for round in 0..2 {
            let wrote=send_while_uplink_window_open(&mut device,"fixture",&mut commands,&mut telemetry,8,Duration::from_secs(5),true,Some(now),Some(now),Duration::ZERO,Duration::ZERO,&mut until,&mut uplink,&mut sent,&mut last,&mut next,&mut error_time,&mut suppressed,Some(&activity),Some(&budget));
            assert_eq!(wrote,round==1);
            assert_eq!(activity.has_pending(),round==0);
            assert_eq!(budget.pending.load(Ordering::Acquire),if round==0 {3}else{0});
            assert_eq!(telemetry.len(),if round==0 {1}else{0});
        }
    }
    #[test]
    fn dedicated_backlog_eviction_releases_only_discarded_telemetry() {
        let activity=TransportTxActivity::default();let (sender,mut receiver)=mpsc::unbounded_channel();
        for byte in 1..=3 {activity.try_enqueue(||sender.send((0,vec![byte])).map_err(|_|sedsnet::TelemetryError::Io("closed"))).unwrap();}
        let mut commands=VecDeque::new();let mut telemetry=VecDeque::new();
        assert!(drain_radio_tx_queue(&mut receiver,"fixture",&mut commands,&mut telemetry,1,Some(&activity),None));
        assert_eq!(telemetry.len(),1);assert!(activity.has_pending());activity.complete();assert!(!activity.has_pending());
    }
}
