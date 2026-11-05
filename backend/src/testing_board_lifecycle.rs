//! Pure-Testing application owners. This is not MCU NVM or a hardware fallback.
use crate::types::Board;
use ring::rand::{SecureRandom, SystemRandom};
use sedsnet::{
    TelemetryResult,
    packet::Packet,
    router::{EndpointHandler, Router, RouterConfig, RouterSideId, RouterSideOptions},
};
use std::{
    array,
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Instant,
};

pub(crate) const BOARDS: [Board; 7] = [
    Board::RFBoard,
    Board::FlightComputer,
    Board::PowerBoard,
    Board::ActuatorBoard,
    Board::ValveBoard,
    Board::GatewayBoard,
    Board::DaqBoard,
];
const FRESH_MS: u64 = 5_000;
fn at(board: Board) -> usize {
    BOARDS
        .iter()
        .position(|&b| b == board)
        .expect("model board")
}
fn physical(board: Board) -> bool {
    matches!(
        board,
        Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer
    )
}
pub(crate) fn endpoint(board: Board) -> &'static str {
    match board {
        Board::RFBoard => "BOARD_RECOVERY_RF",
        Board::FlightComputer => "BOARD_RECOVERY_FC",
        Board::PowerBoard => "BOARD_RECOVERY_PB",
        Board::ActuatorBoard => "BOARD_RECOVERY_AB",
        Board::ValveBoard => "BOARD_RECOVERY_VB",
        Board::GatewayBoard => "BOARD_RECOVERY_GB",
        Board::DaqBoard => "BOARD_RECOVERY_DAQ",
        Board::GroundStation => unreachable!(),
    }
}
#[derive(Debug)]
struct Protocol {
    request: sedsnet::config::DataType,
    status: sedsnet::config::DataType,
    abort: sedsnet::config::DataType,
    state: sedsnet::config::DataType,
    actuator: sedsnet::config::DataType,
    valve: sedsnet::config::DataType,
    flight: sedsnet::config::DataType,
    endpoints: [sedsnet::config::DataEndpoint; 7],
}
fn protocol() -> &'static Protocol {
    static VALUE: std::sync::OnceLock<Protocol> = std::sync::OnceLock::new();
    VALUE.get_or_init(|| {
        let ty = crate::telemetry_schema::data_type;
        Protocol {
            request: ty("BOARD_RECOVERY_REQUEST"),
            status: ty("BOARD_STATE_STATUS"),
            abort: ty("ABORT"),
            state: ty("FLIGHT_STATE"),
            actuator: ty("ACTUATOR_COMMAND"),
            valve: ty("VALVE_COMMAND"),
            flight: ty("FLIGHT_COMMAND"),
            endpoints: array::from_fn(|i| crate::telemetry_schema::endpoint(endpoint(BOARDS[i]))),
        }
    })
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    State,
    Actuator,
    Valve,
    Flight,
    AutomaticActuator,
    AutomaticValve,
}
pub(crate) fn status_packet(board: Board, wire: [u8; 40], now: u64) -> TelemetryResult<Packet> {
    Packet::new(
        protocol().status,
        &[crate::telemetry_schema::endpoints::GROUND_STATION],
        board.sender_id(),
        now,
        Arc::from(wire),
    )
}
pub(crate) fn eligible() -> bool {
    crate::flight_sim::sim_mode_enabled() && !crate::rrls::is_rrls()
}
static SELECTED_DUMMY_LINKS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
pub(crate) fn bind_selected(rocket: bool, fill: bool) {
    SELECTED_DUMMY_LINKS.store(
        u8::from(rocket) | (u8::from(fill) << 1),
        std::sync::atomic::Ordering::SeqCst,
    );
}
pub(crate) fn enabled() -> bool {
    eligible() && selected_links_present()
}
/// Scalar-only final admission check; no eligibility/provider callbacks.
pub(crate) fn selected_links_present() -> bool {
    SELECTED_DUMMY_LINKS.load(std::sync::atomic::Ordering::SeqCst) == 3
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClockSample {
    utc: Option<u64>,
    taken: Instant,
}
impl ClockSample {
    pub(crate) fn new(utc: Option<u64>) -> Self {
        Self {
            utc,
            taken: Instant::now(),
        }
    }
    pub(crate) fn current(self) -> u64 {
        let age = self.taken.elapsed();
        if age.as_secs() >= 1 {
            return 0;
        }
        self.utc
            .filter(|v| *v != 0 && *v != u64::MAX)
            .and_then(|v| v.checked_add(age.as_nanos().div_ceil(1_000_000) as u64))
            .unwrap_or(0)
    }
}

// Only exact packets created by this model may carry this local provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReportOrigin {
    board: Board,
    boot: u64,
    epoch: u32,
    sequence: u64,
}
struct RegisteredReport {
    id: u64,
    bytes: Vec<u8>,
    origin: ReportOrigin,
    ambiguous: bool,
    anchor: Packet,
}
fn registered_origin(packet: &Packet) -> Option<ReportOrigin> {
    if packet.endpoints().is_empty() {
        return None;
    }
    let bytes = sedsnet::wire_format::pack_packet(packet);
    let registry = report_registry().try_lock().ok()?;
    registry
        .iter()
        .find(|v| {
            !v.ambiguous
                && v.id == packet.packet_id()
                && v.bytes.as_slice() == bytes.as_ref()
                && v.anchor.endpoints().as_ptr() == packet.endpoints().as_ptr()
        })
        .map(|v| v.origin)
}
fn report_registry() -> &'static Mutex<VecDeque<RegisteredReport>> {
    static VALUE: std::sync::OnceLock<Mutex<VecDeque<RegisteredReport>>> =
        std::sync::OnceLock::new();
    VALUE.get_or_init(|| Mutex::new(VecDeque::new()))
}
pub(crate) fn register_model_report(packet: &Packet, origin: ReportOrigin) {
    if packet.endpoints().is_empty()
        || !enabled()
        || origin.board != Board::FlightComputer
        || packet.data_type() != protocol().state
        || packet.payload() != [15]
        || packet.sender() != Board::FlightComputer.sender_id()
    {
        return;
    }
    let bytes = sedsnet::wire_format::pack_packet(packet).to_vec();
    let Ok(mut registry) = report_registry().try_lock() else {
        return;
    };
    let id = packet.packet_id();
    if let Some(old) = registry.iter_mut().find(|v| v.id == id) {
        if old.bytes != bytes
            || old.origin != origin
            || old.anchor.endpoints().as_ptr() != packet.endpoints().as_ptr()
        {
            old.ambiguous = true;
        }
        return;
    }
    if registry.len() == 256 {
        registry.pop_front();
    }
    registry.push_back(RegisteredReport {
        id,
        bytes,
        origin,
        ambiguous: false,
        anchor: packet.clone(),
    });
}
pub(crate) fn retired_model_abort_state(board: Board, packet: &Packet) -> bool {
    if !enabled()
        || board != Board::FlightComputer
        || packet.data_type() != protocol().state
        || packet.payload() != [15]
    {
        return false;
    }
    // Retained Packet::clone pins the nonempty Arc endpoint allocation. A fresh
    // decode with identical wire bytes/ID cannot acquire this local lineage.
    // Registry ownership is released before trying the model owner.
    let origin = registered_origin(packet);
    origin.is_some_and(|origin| crate::flight_sim::model_report_is_old(origin, packet.timestamp()))
}

fn fresh(source: u64, now: u64) -> bool {
    source != 0 && source != u64::MAX && now.checked_sub(source).is_some_and(|age| age <= FRESH_MS)
}
fn new_boot() -> Option<u64> {
    let mut bytes = [0; 8];
    SystemRandom::new().fill(&mut bytes).ok()?;
    let value = u64::from_le_bytes(bytes);
    (value != 0 && value < u64::MAX - 1).then_some(value)
}
#[derive(Clone, Debug)]
struct Query {
    nonce: u64,
    issuer: u64,
    epoch: u32,
    boot: u64,
    source: u64,
}
#[derive(Clone, Debug)]
pub(crate) struct Intent {
    wire: [u8; 32],
    pub(crate) source: u64,
    pub(crate) desired: u8,
    epoch: u32,
    boot: u64,
    nonce: u64,
    issuer: u64,
    result: u8,
    finished_epoch: Option<u32>,
}
#[derive(Clone, Debug)]
struct Node {
    boot: u64,
    epoch: u32,
    state: Option<u8>,
    blocked: bool,
    aborted: bool,
    abort_utc: u64,
    floor: u64,
    attempted: u64,
    completed: bool,
    completed_epoch: u32,
    launched: bool,
    accepted_launch: bool,
    query: Option<Query>,
    last: Option<Intent>,
    born: Instant,
}
#[derive(Debug)]
pub(crate) struct Owner {
    protocol: &'static Protocol,
    observed_clock: Option<(u64, Instant)>,
    clock_high: u64,
    boot_sequence: u64,
    initialized: bool,
    nodes: [Node; 7],
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum Action {
    None,
    Stop,
    Ordinary(Kind, u8),
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Reply {
    pub(crate) result: u8,
    nonce: u64,
    issuer: u64,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Decision {
    pub(crate) action: Action,
    pub(crate) reply: Option<Reply>,
}
impl Decision {
    fn none() -> Self {
        Self {
            action: Action::None,
            reply: None,
        }
    }
}
impl Owner {
    pub(crate) fn new() -> Self {
        Self {
            protocol: protocol(),
            observed_clock: None,
            clock_high: 0,
            boot_sequence: 1,
            initialized: false,
            nodes: array::from_fn(|_| Node {
                boot: new_boot().unwrap_or(0),
                epoch: 1,
                state: None,
                blocked: true,
                aborted: false,
                abort_utc: 0,
                floor: 0,
                attempted: 0,
                completed: false,
                completed_epoch: 0,
                launched: false,
                accepted_launch:false,
                query: None,
                last: None,
                born: Instant::now(),
            }),
        }
    }
    /// Existing host history is the durability authority. A fresh model identity
    /// cannot erase an unclean/closed/pending host hold or an observed board Abort.
    pub(crate) fn initialize(&mut self, hold_mask: u8, abort_mask: u8, floor: u64) {
        if self.initialized {
            return;
        }
        for (i, node) in self.nodes.iter_mut().enumerate() {
            node.blocked = hold_mask & (1 << i) != 0 || node.boot == 0;
            node.aborted = abort_mask & (1 << i) != 0;
            node.blocked |= node.aborted;
            node.state = if node.aborted {
                Some(15)
            } else if node.blocked {
                None
            } else {
                Some(
                    if matches!(BOARDS[i], Board::ActuatorBoard | Board::ValveBoard) {
                        1
                    } else {
                        0
                    },
                )
            };
            node.floor = if floor == u64::MAX { 0 } else { floor };
        }
        self.initialized = true;
    }
    pub(crate) fn initialize_boot(&mut self, hold: u8, actual_abort: u8, unknown: u8, floor: u64) {
        if self.initialized {
            return;
        }
        self.initialize(hold, actual_abort, floor);
        for (i, node) in self.nodes.iter_mut().enumerate() {
            if unknown & (1 << i) != 0 && !node.blocked {
                node.state = Some(0);
            }
        }
    }
    pub(crate) fn observe_clock(&mut self, utc: Option<u64>) {
        let Some(value) = utc.filter(|v| *v != 0 && *v != u64::MAX && *v >= self.clock_high) else {
            self.observed_clock = None;
            return;
        };
        self.clock_high = value;
        self.observed_clock = Some((value, Instant::now()));
    }
    pub(crate) fn original_event_stamp(&self) -> u64 {
        let Some((value, taken)) = self.observed_clock else {
            return 0;
        };
        if taken.elapsed().as_secs() >= 1
            || self.nodes.iter().any(|n| n.completed && value < n.floor)
        {
            0
        } else {
            value
        }
    }
    pub(crate) fn reboot(&mut self) {
        self.observed_clock = None;
        self.clock_high = 0;
        self.boot_sequence = self.boot_sequence.checked_add(1).unwrap_or(0);
        for (i, node) in self.nodes.iter_mut().enumerate() {
            node.boot = new_boot().unwrap_or(0);
            node.epoch = 1;
            node.query = None;
            node.blocked |= node.last.as_ref().is_some_and(|i| i.result == 1) || node.boot == 0;
            node.completed = false;
            node.completed_epoch = 0;
            node.last = None;
            node.launched = false;
            node.accepted_launch=false;
            node.state = if node.aborted {
                Some(15)
            } else if node.blocked {
                None
            } else {
                Some(
                    if matches!(BOARDS[i], Board::ActuatorBoard | Board::ValveBoard) {
                        1
                    } else {
                        0
                    },
                )
            };
            node.born = Instant::now();
        }
    }
    pub(crate) fn manual_stop(&mut self, board: Board) {
        let abort_utc = self.original_event_stamp();
        let n = &mut self.nodes[at(board)];
        n.accepted_launch=false;
        n.blocked = true;
        n.aborted = true;
        n.abort_utc = abort_utc;
        n.state = Some(15);
        n.epoch = n
            .epoch
            .checked_add(1)
            .filter(|e| *e < u32::MAX - 1)
            .unwrap_or(u32::MAX);
        // Old completed B remains history; this fresh Stop is independently held.
    }
    pub(crate) fn blocked(&self, board: Board) -> bool {
        !self.initialized || self.nodes[at(board)].blocked
    }
    pub(crate) fn source_allowed(&self, board: Board, source: u64, now: u64) -> bool {
        let n = &self.nodes[at(board)];
        self.initialized && !n.blocked && fresh(source, now) && source > n.floor
    }
    pub(crate) fn note_state(&mut self, board: Board, state: u8) {
        let n = &mut self.nodes[at(board)];
        if !n.blocked {
            n.state = Some(state);
        }
    }
    pub(crate) fn publication_state(&self) -> Option<u8> {
        self.nodes[at(Board::FlightComputer)].state
    }
    pub(crate) fn publication_source(&self, fallback: u64) -> u64 {
        let n = &self.nodes[at(Board::FlightComputer)];
        if n.state == Some(15) {
            n.abort_utc
        } else {
            fallback
        }
    }
    pub(crate) fn note_launch(&mut self, board: Board) {
        self.nodes[at(board)].launched = true;
        self.nodes[at(board)].accepted_launch = true;
    }
    pub(crate) fn receive(&mut self, board: Board, packet: &Packet, now: u64) -> Decision {
        let request_ty = self.protocol.request;
        if packet.data_type() != request_ty {
            return self.ordinary(board, packet, now);
        }
        let p = packet.payload();
        if p.len() != 32
            || packet.sender() != "GS"
            || packet.endpoints() != [self.protocol.endpoints[at(board)]]
        {
            return Decision::none();
        }
        let nonce = u64::from_le_bytes(p[4..12].try_into().unwrap());
        let issuer = u64::from_le_bytes(p[24..32].try_into().unwrap());
        if nonce == 0 || issuer == 0 {
            return Decision::none();
        }
        let response = |result| Decision {
            action: Action::None,
            reply: Some(Reply {
                result,
                nonce,
                issuer,
            }),
        };
        if p[0] != 1 || p[3] != 0 || p[1] > 1 || (p[1] == 0 && p[2] != 0) {
            return response(5);
        }
        let n = &mut self.nodes[at(board)];
        if p[1] == 0 {
            // No model effects, floor mutation, epoch advancement or recovery.
            n.query = Some(Query {
                nonce,
                issuer,
                epoch: n.epoch,
                boot: n.boot,
                source: packet.timestamp(),
            });
            return response(if !self.initialized || n.blocked && !n.aborted {
                7
            } else {
                0
            });
        }
        // Firmware parity: a delayed preLaunch Recover is not fresh intent to
        // revoke an already accepted Launch/igniter owner. Query remains read-only.
        // ManualStop independently retires this RAM owner; historical launched
        // stays latched until a later verified Recovery completes.
        if n.accepted_launch {return response(4);}
        if let Some(old) = &n.last
            && old.nonce == nonce
            && old.issuer == issuer
        {
            if old.wire.as_slice() != p || old.source != packet.timestamp() {
                return response(6);
            }
            let result = if old.finished_epoch.is_some_and(|e| e != n.epoch)
                || old.result == 1 && old.epoch != n.epoch
            {
                4
            } else {
                old.result
            };
            return response(result);
        }
        if n.last
            .as_ref()
            .is_some_and(|old| old.result == 1 && old.epoch == n.epoch)
        {
            return response(3);
        }
        let boot = u64::from_le_bytes(p[12..20].try_into().unwrap());
        let epoch = u32::from_le_bytes(p[20..24].try_into().unwrap());
        let intent = Intent {
            wire: p.try_into().unwrap(),
            source: packet.timestamp(),
            desired: p[2],
            epoch,
            boot,
            nonce,
            issuer,
            result: 7,
            finished_epoch: None,
        };
        n.last = Some(intent);
        let result = if !self.initialized || n.boot == 0 || n.epoch >= u32::MAX - 1 {
            7
        } else if p[2] != 0
            && !(p[2] == 1 && matches!(board, Board::ActuatorBoard | Board::ValveBoard))
        {
            2
        } else if boot != n.boot {
            6
        } else if epoch != n.epoch
            || !fresh(packet.timestamp(), now)
            || packet.timestamp() <= n.floor
            || packet.timestamp() <= n.attempted
            || !n.query.as_ref().is_some_and(|q| {
                q.boot == boot
                    && q.epoch == epoch
                    && q.issuer == issuer
                    && q.nonce != nonce
                    && fresh(q.source, now)
                    && q.source > n.floor
                    && q.source > n.attempted
            })
        {
            4
        } else {
            1
        };
        n.last.as_mut().unwrap().result = result;
        if result != 1 {
            return response(result);
        }
        n.attempted = packet.timestamp();
        n.blocked = true;
        n.aborted = true;
        n.abort_utc = 0;
        n.state = Some(15);
        Decision {
            action: Action::Stop,
            reply: Some(Reply {
                result: 1,
                nonce,
                issuer,
            }),
        }
    }
    fn ordinary(&mut self, board: Board, packet: &Packet, now: u64) -> Decision {
        use crate::telemetry_schema::endpoints as ep;
        let ty = packet.data_type();
        let n = &self.nodes[at(board)];
        let shape = |required, extras: &[sedsnet::config::DataEndpoint]| {
            let values = packet.endpoints();
            values.contains(&required)
                && values.iter().enumerate().all(|(i, e)| {
                    !values[..i].contains(e) && (*e == required || extras.contains(e))
                })
        };
        let station = matches!(packet.sender(), "GS" | "RRLS");
        if ty == self.protocol.abort {
            if !physical(board)
                || !shape(
                    ep::ABORT,
                    &[
                        ep::GROUND_STATION,
                        ep::FLIGHT_CONTROLLER,
                        ep::VALVE_BOARD,
                        ep::ACTUATOR_BOARD,
                        ep::FLIGHT_STATE,
                        ep::SD_CARD,
                    ],
                )
            {
                return Decision::none();
            }
            let p = packet.payload();
            let automatic = p.starts_with(b"SEDS_AUTO_ABORT_V1:");
            if board == Board::FlightComputer {
                if !station || !(p == b"SEDS_MANUAL_ABORT_V1:" || p == b"SEDS_MANUAL_ABORT_V1:K") {
                    return Decision::none();
                }
            } else if automatic {
                let producer = station
                    || Board::from_sender_id(packet.sender())
                        .is_some_and(|b| b != Board::GroundStation);
                let heartbeat = p.windows(b"heartbeat".len()).any(|w| w == b"heartbeat");
                let minimum = if heartbeat { 2 } else { 1 };
                if !producer
                    || n.launched
                    || !n.state.is_some_and(|s| s >= minimum && s < 7)
                    || !self.source_allowed(board, packet.timestamp(), now)
                {
                    return Decision::none();
                }
            }
            if !automatic
                && p == b"SEDS_MANUAL_ABORT_V1:K"
                && station
                && n.completed
                && packet.timestamp() != 0
                && packet.timestamp() != u64::MAX
                && packet.timestamp() < n.floor
            {
                return Decision::none();
            }
            self.manual_stop(board);
            return Decision {
                action: Action::Stop,
                reply: None,
            };
        }
        if !station
            || !self.source_allowed(board, packet.timestamp(), now)
            || packet.payload().len() != 1
        {
            return Decision::none();
        }
        let kind = if ty == self.protocol.state
            && packet.sender() == "GS"
            && shape(ep::FLIGHT_STATE, &[ep::GROUND_STATION, ep::SD_CARD])
        {
            Kind::State
        } else if board == Board::ActuatorBoard
            && ty == self.protocol.actuator
            && shape(ep::ACTUATOR_BOARD, &[ep::GROUND_STATION, ep::SD_CARD])
        {
            Kind::Actuator
        } else if board == Board::ValveBoard
            && ty == self.protocol.valve
            && shape(ep::VALVE_BOARD, &[ep::GROUND_STATION, ep::SD_CARD])
        {
            Kind::Valve
        } else if board == Board::FlightComputer
            && ty == self.protocol.flight
            && shape(ep::FLIGHT_CONTROLLER, &[ep::GROUND_STATION, ep::SD_CARD])
        {
            Kind::Flight
        } else {
            return Decision::none();
        };
        let raw = packet.payload()[0];
        if kind==Kind::Actuator && raw==14 && n.accepted_launch {return Decision::none();}
        let mut kind = kind;
        let mut value = raw;
        if raw & 0x80 != 0 && matches!(kind, Kind::Actuator | Kind::Valve) {
            value = raw & 0x7f;
            let allowed = match kind {
                Kind::Actuator => matches!(value, 9 | 10 | 12 | 13),
                Kind::Valve => value <= 5,
                _ => false,
            };
            if !allowed || n.launched {
                return Decision::none();
            }
            kind = if kind == Kind::Actuator {
                Kind::AutomaticActuator
            } else {
                Kind::AutomaticValve
            };
        }
        if kind == Kind::State && value == 15 && n.launched {
            return Decision::none();
        }
        Decision {
            action: Action::Ordinary(kind, value),
            reply: None,
        }
    }
    pub(crate) fn report_origin(&self, board: Board) -> Option<ReportOrigin> {
        let n = &self.nodes[at(board)];
        (self.initialized && n.boot != 0 && self.boot_sequence != 0).then_some(ReportOrigin {
            board,
            boot: n.boot,
            epoch: n.epoch,
            sequence: self.boot_sequence,
        })
    }
    pub(crate) fn report_is_old(&self, origin: ReportOrigin, source: u64) -> bool {
        let n = &self.nodes[at(origin.board)];
        self.initialized
            && n.completed
            && n.floor > 0
            && n.floor != u64::MAX
            && self.boot_sequence != 0
            && (source == 0 || (source != u64::MAX && source < n.floor))
            && (origin.sequence < self.boot_sequence
                || (origin.sequence == self.boot_sequence
                    && origin.boot == n.boot
                    && origin.epoch < n.completed_epoch))
    }
    pub(crate) fn ready_jobs(&self) -> Vec<(Board, Intent)> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| {
                n.last
                    .as_ref()
                    .filter(|v| v.result == 1)
                    .map(|v| (BOARDS[i], v.clone()))
            })
            .collect()
    }
    pub(crate) fn ticket_current(&self, board: Board, intent: &Intent, now: u64) -> bool {
        let n = &self.nodes[at(board)];
        !n.accepted_launch && fresh(intent.source, now)
            && n.boot == intent.boot
            && n.epoch == intent.epoch
            && n.last.as_ref().is_some_and(|v| {
                v.wire == intent.wire && v.source == intent.source && v.result == 1
            })
    }
    pub(crate) fn finish(&mut self, board: Board, intent: &Intent, now: u64) -> Reply {
        let n = &mut self.nodes[at(board)];
        let valid = !n.accepted_launch && fresh(intent.source, now)
            && n.boot == intent.boot
            && n.epoch == intent.epoch
            && n.last.as_ref().is_some_and(|v| {
                v.wire == intent.wire && v.source == intent.source && v.result == 1
            });
        let result = if valid {
            0
        } else if now == 0 || now == u64::MAX {
            7
        } else {
            4
        };
        if valid {
            n.epoch += 1;
            n.state = Some(intent.desired);
            n.aborted = false;
            n.blocked = false;
            // Same verified ticket/epoch/freshness and actual safe owner apply.
            // Query, Pending, failure and a raced Stop never reach this clear.
            n.launched = false;
            n.accepted_launch=false;
            n.floor = intent.source;
            n.completed = true;
            n.completed_epoch = n.epoch;
        }
        if let Some(last) = &mut n.last
            && last.wire == intent.wire
            && last.source == intent.source
        {
            last.result = result;
            last.finished_epoch = (result == 0).then_some(n.epoch);
        }
        Reply {
            result,
            nonce: intent.nonce,
            issuer: intent.issuer,
        }
    }
    pub(crate) fn status(&self, board: Board, reply: Reply, outputs: u32, _now: u64) -> [u8; 40] {
        let n = &self.nodes[at(board)];
        let mut wire = [0; 40];
        wire[0] = 1;
        wire[1] = reply.result;
        wire[2] = n.state.unwrap_or(255);
        let mut flags = if self.initialized && n.boot != 0 {
            4 | 32
        } else {
            0
        };
        if n.state.is_some() {
            flags |= 8;
        }
        if physical(board) {
            flags |= 16 | 64;
            if outputs == if board == Board::ValveBoard { 6 } else { 0 } {
                flags |= 2;
            }
            if n.aborted {
                flags |= 1;
            }
        }
        wire[24..28].copy_from_slice(
            &(n.born.elapsed().as_millis().min(u32::MAX as u128) as u32).to_le_bytes(),
        );
        wire[3] = flags;
        wire[4..12].copy_from_slice(&reply.nonce.to_le_bytes());
        wire[12..20].copy_from_slice(&n.boot.to_le_bytes());
        wire[20..24].copy_from_slice(&n.epoch.to_le_bytes());
        wire[28..32].copy_from_slice(&if physical(board) { outputs } else { 0 }.to_le_bytes());
        wire[32..40].copy_from_slice(&reply.issuer.to_le_bytes());
        wire
    }
}

type BridgeQueue = Arc<Mutex<VecDeque<(RouterSideId, Vec<u8>)>>>;

/// Actual SDK peers are reachable only through an explicitly selected dummy
/// link. They retain SDK delivery/dedup ownership independently from app state.
pub(crate) struct Link {
    bridge: Router,
    outer: RouterSideId,
    peers: Vec<Peer>,
    output: Arc<Mutex<VecDeque<Vec<u8>>>>,
    to_bridge: BridgeQueue,
    last_discovery: Instant,
}
struct Peer {
    board: Board,
    router: Router,
    side: RouterSideId,
    inbox: Arc<Mutex<VecDeque<Packet>>>,
    incoming: Arc<Mutex<VecDeque<Vec<u8>>>>,
    endpoints: Vec<sedsnet::config::DataEndpoint>,
}
impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PureTestingBoardLink")
            .field("peers", &self.peers.len())
            .finish()
    }
}
fn command_endpoint(board: Board) -> Option<&'static str> {
    match board {
        Board::ActuatorBoard => Some("ACTUATOR_BOARD"),
        Board::ValveBoard => Some("VALVE_BOARD"),
        Board::FlightComputer => Some("FLIGHT_CONTROLLER"),
        _ => None,
    }
}
impl Link {
    pub(crate) fn new(name: &str) -> Option<Self> {
        if !eligible() {
            return None;
        }
        let rocket = match name {
            "Rocket Comms" => true,
            "Umbilical Comms" => false,
            _ => return None,
        };
        let output = Arc::new(Mutex::new(VecDeque::new()));
        let to_bridge = Arc::new(Mutex::new(VecDeque::new()));
        let sender = if rocket { "SIM_AV" } else { "SIM_FILL" };
        let bridge = Router::new(RouterConfig::new([]).with_sender(sender));
        let now = crate::telemetry_task::get_current_timestamp_ms();
        bridge
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(now))
            .ok()?;
        let capture = output.clone();
        let outer = bridge.add_side_packed_with_options(
            "actual-dummy-host",
            move |wire| {
                capture.lock().unwrap().push_back(wire.to_vec());
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                ..Default::default()
            }
            .with_small_packet_transport(if rocket { 192 } else { 1024 }),
        );
        bridge
            .rx_from_side(
                &sedsnet::discovery::build_discovery_announce(
                    "GS",
                    now,
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                )
                .ok()?,
                outer,
            )
            .ok()?;
        let mut peers = Vec::new();
        for board in BOARDS {
            if rocket
                != matches!(
                    board,
                    Board::RFBoard | Board::FlightComputer | Board::PowerBoard
                )
            {
                continue;
            }
            let inbox = Arc::new(Mutex::new(VecDeque::new()));
            let mut endpoints = vec![
                crate::telemetry_schema::endpoint(endpoint(board)),
                crate::telemetry_schema::endpoints::FLIGHT_STATE,
            ];
            if physical(board) {
                endpoints.push(crate::telemetry_schema::endpoints::ABORT);
            }
            if board == Board::FlightComputer {
                endpoints.push(crate::telemetry_schema::endpoints::SD_CARD);
            }
            if let Some(command) = command_endpoint(board) {
                endpoints.push(crate::telemetry_schema::endpoint(command));
            }
            let handlers = endpoints
                .iter()
                .map(|&ep| {
                    let q = inbox.clone();
                    EndpointHandler::new_packet_handler(ep, move |packet| {
                        let protocol = protocol();
                        let ty = packet.data_type();
                        use crate::telemetry_schema::endpoints as e;
                        let control = (ep == protocol.endpoints[at(board)]
                            && ty == protocol.request)
                            || (ep == e::ABORT && ty == protocol.abort)
                            || (ep == e::FLIGHT_STATE && ty == protocol.state)
                            || (board == Board::ActuatorBoard
                                && ep == e::ACTUATOR_BOARD
                                && ty == protocol.actuator)
                            || (board == Board::ValveBoard
                                && ep == e::VALVE_BOARD
                                && ty == protocol.valve)
                            || (board == Board::FlightComputer
                                && ep == e::FLIGHT_CONTROLLER
                                && ty == protocol.flight);
                        if control {
                            q.lock().unwrap().push_back(packet.clone());
                        }
                        Ok(())
                    })
                })
                .collect::<Vec<_>>();
            let router = Router::new(RouterConfig::new(handlers).with_sender(board.sender_id()));
            router
                .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                    now,
                ))
                .ok()?;
            let incoming = Arc::new(Mutex::new(VecDeque::new()));
            let capture = incoming.clone();
            let bridge_side = bridge.add_side_packed_with_options(
                board.sender_id(),
                move |wire| {
                    capture.lock().unwrap().push_back(wire.to_vec());
                    Ok(())
                },
                RouterSideOptions {
                    reliable_enabled: true,
                    ..Default::default()
                },
            );
            let bus = to_bridge.clone();
            let side = router.add_side_packed_with_options(
                sender,
                move |wire| {
                    bus.lock().unwrap().push_back((bridge_side, wire.to_vec()));
                    Ok(())
                },
                RouterSideOptions {
                    reliable_enabled: true,
                    ..Default::default()
                },
            );
            router
                .rx_from_side(
                    &sedsnet::discovery::build_discovery_announce(
                        sender,
                        now,
                        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                    )
                    .ok()?,
                    side,
                )
                .ok()?;
            let announce =
                sedsnet::discovery::build_discovery_announce(board.sender_id(), now, &endpoints)
                    .ok()?;
            bridge.rx_from_side(&announce, bridge_side).ok()?;
            // This is the actual registered peer's SDK discovery, not a
            // coordinator capability override or an unknown real-device proof.
            bridge.tx_queue(announce).ok()?;
            peers.push(Peer {
                board,
                router,
                side,
                inbox,
                incoming,
                endpoints,
            });
        }
        Some(Self {
            bridge,
            outer,
            peers,
            output,
            to_bridge,
            last_discovery: Instant::now(),
        })
    }
    pub(crate) fn send(&mut self, wire: &[u8]) -> TelemetryResult<()> {
        self.bridge.rx_packed_queue_from_side(wire, self.outer)
    }
    pub(crate) fn poll(&mut self, observed_utc: Option<u64>) -> TelemetryResult<Option<Vec<u8>>> {
        let sample = ClockSample::new(observed_utc);
        crate::flight_sim::observe_model_clock(observed_utc);
        let now = sample.current();
        if self.last_discovery.elapsed().as_millis() >= 2000 {
            for peer in &self.peers {
                self.bridge
                    .tx_queue(sedsnet::discovery::build_discovery_announce(
                        peer.board.sender_id(),
                        now,
                        &peer.endpoints,
                    )?)?;
            }
            self.last_discovery = Instant::now();
        }
        for (board, packet) in crate::flight_sim::service_model_recovery(
            sample,
            &self.peers.iter().map(|p| p.board).collect::<Vec<_>>(),
        )? {
            if let Some(p) = self.peers.iter().find(|p| p.board == board) {
                p.router.tx_queue(packet)?;
            }
        }
        self.bridge.process_all_queues()?;
        for peer in &self.peers {
            let incoming: Vec<_> = peer.incoming.lock().unwrap().drain(..).collect();
            for wire in incoming {
                peer.router.rx_packed_queue_from_side(&wire, peer.side)?;
            }
            peer.router.process_rx_queue()?;
            let packets: Vec<_> = peer.inbox.lock().unwrap().drain(..).collect();
            for packet in packets {
                if let Some(reply) =
                    crate::flight_sim::receive_model_packet(peer.board, &packet, sample)?
                {
                    peer.router.tx_queue(reply)?;
                }
            }
            peer.router.process_tx_queue()?;
        }
        let incoming: Vec<_> = self.to_bridge.lock().unwrap().drain(..).collect();
        for (side, wire) in incoming {
            self.bridge.rx_packed_queue_from_side(&wire, side)?;
        }
        self.bridge.process_all_queues()?;
        Ok(self.output.lock().unwrap().pop_front())
    }
}

#[cfg(test)]
#[path = "testing_board_lifecycle/tests.rs"]
mod tests;
