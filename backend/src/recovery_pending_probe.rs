//! TEST ONLY: readiness-error native peer model and observational race pauses.
//! Never supplies SDK ACKs or claims actual MCU/NVM readiness.
use crate::{state::AppState, types::Board};
use sedsnet::packet::Packet;
use std::sync::{
    Arc, Condvar, Mutex, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Instant;
use tokio::sync::Notify;

pub(crate) struct SharedClock {
    pub(crate) born: Instant,
    pub(crate) divisor: u64,
}
impl SharedClock {
    pub(crate) fn sample(&self) -> u64 {
        self.born.elapsed().as_millis() as u64 / self.divisor
    }
}
pub(crate) struct ClockSource(pub(crate) Arc<SharedClock>);
impl sedsnet::router::Clock for ClockSource {
    fn now_ms(&self) -> u64 {
        self.0.sample()
    }
}
#[derive(Clone, Debug)]
pub(crate) struct Observation {
    pub(crate) id: u64,
    pub(crate) utc: u64,
    pub(crate) value: Option<u8>,
    pub(crate) manual: u64,
}
#[derive(Clone, Debug)]
pub(crate) struct WireObservation {
    pub(crate) sequence: usize,
    pub(crate) stage: &'static str,
    pub(crate) board: Board,
    pub(crate) bytes: Vec<u8>,
}
pub(crate) struct Probe {
    pub(crate) trace_wire: AtomicBool,
    pub(crate) hold_get: AtomicBool,
    pub(crate) held_get_id: Mutex<Option<u64>>,
    pub(crate) wire: Mutex<Vec<WireObservation>>,
    pub(crate) wire_dropped: AtomicU64,
    pub(crate) reject_gb: AtomicBool,
    pub(crate) refresh: AtomicBool,
    pub(crate) clock: Option<Arc<SharedClock>>,
    born: Instant,
    pub(crate) received: Mutex<Vec<Observation>>,
    pub(crate) observations_dropped: AtomicU64,
    pub(crate) refusal_errors: AtomicU64,
    pub(crate) entries: Mutex<Vec<(u64, u64, u64)>>,
    pub(crate) capture_entries: AtomicBool,
    pub(crate) entries_dropped: AtomicU64,
    pub(crate) before_auto: AtomicBool,
    pub(crate) auto_entered: Notify,
    pub(crate) auto_release: Notify,
    pub(crate) block_commit: AtomicBool,
    pub(crate) automatic_completed: AtomicBool,
    final_commit_outcomes: [AtomicU64;6],
    /// SDK refusals that were only lock contention (core or schema busy):
    /// the attempt returned at once without effect and a later fresh contact
    /// may qualify.
    sdk_contention: AtomicU64,
    pub(crate) sequence_ticks: AtomicU64,
    pub(crate) packet_worker_abort: Mutex<Option<tokio::task::AbortHandle>>,
    pub(crate) commit_entered: Notify,
    release_commit: Mutex<bool>,
    commit_cv: Condvar,
    last_get: AtomicU64,
    state: Mutex<Weak<AppState>>,
}
static NEXT: Mutex<Option<Arc<Probe>>> = Mutex::new(None);
static ACTIVE: Mutex<Option<Arc<Probe>>> = Mutex::new(None);
impl Probe {
    pub(crate) fn new(divisor: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            trace_wire: AtomicBool::new(false),
            hold_get: AtomicBool::new(false),
            held_get_id: Mutex::new(None),
            wire: Mutex::new(Vec::new()),
            wire_dropped: AtomicU64::new(0),
            reject_gb: AtomicBool::new(true),
            refresh: AtomicBool::new(false),
            clock: divisor.map(|divisor| {
                Arc::new(SharedClock {
                    born: Instant::now(),
                    divisor,
                })
            }),
            born: Instant::now(),
            received: Mutex::new(Vec::new()),
            observations_dropped: AtomicU64::new(0),
            refusal_errors: AtomicU64::new(0),
            entries: Mutex::new(Vec::new()),
            capture_entries: AtomicBool::new(false),
            entries_dropped: AtomicU64::new(0),
            before_auto: AtomicBool::new(false),
            auto_entered: Notify::new(),
            auto_release: Notify::new(),
            block_commit: AtomicBool::new(false),
            automatic_completed: AtomicBool::new(false),
            final_commit_outcomes: std::array::from_fn(|_|AtomicU64::new(0)),
            sdk_contention: AtomicU64::new(0),
            sequence_ticks: AtomicU64::new(0),
            packet_worker_abort: Mutex::new(None),
            commit_entered: Notify::new(),
            release_commit: Mutex::new(false),
            commit_cv: Condvar::new(),
            last_get: AtomicU64::new(0),
            state: Mutex::new(Weak::new()),
        })
    }
    pub(crate) fn final_commit_counts(&self)->[u64;6] {
        std::array::from_fn(|i|self.final_commit_outcomes[i].load(Ordering::Acquire))
    }
    pub(crate) fn sdk_contention_count(&self)->u64 {
        self.sdk_contention.load(Ordering::Acquire)
    }
    pub(crate) fn install(self: &Arc<Self>) {
        assert!(NEXT.lock().unwrap().replace(self.clone()).is_none());
    }
    pub(crate) fn bind(self: &Arc<Self>, state: &Arc<AppState>) {
        *self.state.lock().unwrap() = Arc::downgrade(state);
        *ACTIVE.lock().unwrap() = Some(self.clone());
    }
    pub(crate) fn observe(&self, board: Board, packet: &Packet) {
        if board != Board::GatewayBoard {
            return;
        }
        let manual = self
            .state
            .lock()
            .unwrap()
            .upgrade()
            .map_or(0, |s| s.manual_abort_intent.epoch());
        let mut rows = self.received.lock().unwrap();
        if rows.len() == 512 {
            self.observations_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        rows.push(Observation {
            id: packet.packet_id(),
            utc: packet.timestamp(),
            value: packet.payload().first().copied(),
            manual,
        });
    }
    // Pure bounded transport observation. No SDK decode, clock/provider call,
    // router lock or application callback is invoked from the TX callback.
    pub(crate) fn observe_wire(&self, stage: &'static str, board: Board, bytes: &[u8]) {
        if !self.trace_wire.load(Ordering::Acquire) || board != Board::GatewayBoard {
            return;
        }
        let mut rows = self.wire.lock().unwrap();
        let used: usize = rows.iter().map(|row| row.bytes.len()).sum();
        if rows.len() >= 256 || bytes.len() > 65_536 || used.saturating_add(bytes.len()) > 524_288 {
            self.wire_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let sequence = rows.len();
        rows.push(WireObservation { sequence, stage, board, bytes: bytes.to_vec() });
    }
    // Called by the native fixture BEFORE entering the actual GS SDK RX API,
    // outside router/SDK owners. The same original frame remains at queue head.
    pub(crate) fn should_hold_get(&self, board: Board, bytes: &[u8]) -> bool {
        if !self.trace_wire.load(Ordering::Acquire) || !self.hold_get.load(Ordering::Acquire)
            || board != Board::GatewayBoard {
            return false;
        }
        let Ok(packet) = sedsnet::wire_format::unpack_packet(bytes) else { return false; };
        if packet.data_type() != sedsnet::config::DataType::ManagedVariableRequest
            || sedsnet::discovery::decode_managed_variable_request(&packet).ok()
                != Some(crate::telemetry_schema::data_type("FLIGHT_STATE")) {
            return false;
        }
        let mut original = self.held_get_id.lock().unwrap();
        if let Some(id) = *original {
            assert_eq!(id, packet.packet_id(), "held GET original identity changed");
        } else {
            *original = Some(packet.packet_id());
        }
        true
    }
    pub(crate) fn get_due(&self) -> bool {
        if !self.refresh.load(Ordering::Acquire) {
            return false;
        }
        let now = self
            .clock
            .as_ref()
            .map_or_else(|| self.elapsed_wall(), |c| c.sample());
        let last = self.last_get.load(Ordering::Acquire);
        if now.saturating_sub(last) < 500 {
            return false;
        }
        self.last_get.store(now, Ordering::Release);
        true
    }
    fn elapsed_wall(&self) -> u64 {
        self.born.elapsed().as_millis() as u64
    }
    pub(crate) fn release(&self) {
        self.hold_get.store(false, Ordering::Release);
        self.auto_release.notify_one();
        *self.release_commit.lock().unwrap() = true;
        self.commit_cv.notify_all();
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.release();
    }
}
pub(crate) fn take_next() -> Option<Arc<Probe>> {
    NEXT.lock().unwrap().take()
}
fn for_state(state: &Arc<AppState>) -> Option<Arc<Probe>> {
    ACTIVE
        .lock()
        .unwrap()
        .as_ref()
        .filter(|p| {
            p.state
                .lock()
                .unwrap()
                .upgrade()
                .is_some_and(|s| Arc::ptr_eq(&s, state))
        })
        .cloned()
}
pub(crate) fn receive_entry(state: &Arc<AppState>, packet: &Packet) {
    if packet.data_type() != crate::telemetry_schema::data_type("HEARTBEAT") {
        return;
    }
    if let Some(p) = for_state(state) {
        // Board contacts only: the station's own heartbeat can land in the
        // capture window and is not a board entry.
        if !p.capture_entries.load(Ordering::Acquire)
            || state.canonical_network_sender_id(packet.sender()) == "GS"
        {
            return;
        }
        let mut entries = p.entries.lock().unwrap();
        if entries.len() < 64 {
            // Observation only: deliberately NOT used to fence unchanged code.
            entries.push((
                packet.packet_id(),
                crate::gse::automatic_generation(state),
                state.manual_abort_intent.epoch(),
            ));
        } else {
            p.entries_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
pub(crate) async fn before_automatic(state: &Arc<AppState>, packet: &Packet) {
    if packet.data_type() != crate::telemetry_schema::data_type("HEARTBEAT")
        || state.canonical_network_sender_id(packet.sender()) != "GB"
    {
        return;
    }
    if let Some(p) = for_state(state)
        && p.before_auto.swap(false, Ordering::AcqRel)
    {
        p.auto_entered.notify_one();
        p.auto_release.notified().await;
    }
}
pub(crate) fn before_state_commit(value: u8) {
    if value != 1 {
        return;
    }
    let p = ACTIVE.lock().unwrap().clone();
    if let Some(p) = p
        && p.block_commit.swap(false, Ordering::AcqRel)
    {
        p.commit_entered.notify_one();
        let mut released = p.release_commit.lock().unwrap();
        while !*released {
            released = p.commit_cv.wait(released).unwrap();
        }
    }
}
pub(crate) fn clear_for_state(state: &Arc<AppState>) {
    let mut slot = ACTIVE.lock().unwrap();
    if slot.as_ref().is_some_and(|p| {
        p.state
            .lock()
            .unwrap()
            .upgrade()
            .is_some_and(|s| Arc::ptr_eq(&s, state))
    }) && let Some(p) = slot.take() {
        let entries = p.entries_dropped.load(Ordering::Acquire);
        let observations = p.observations_dropped.load(Ordering::Acquire);
        if entries != 0 || observations != 0 {
            eprintln!("NATIVE_OBSERVER_LOSS entries={entries} GB={observations}");
        }
        p.release();
    }
}

pub(crate) fn peer_result<T>(probe: Option<&Arc<Probe>>, result: sedsnet::TelemetryResult<T>) {
    match result {
        Ok(_) => {}
        Err(sedsnet::TelemetryError::HandlerError("native GB strict-readiness refusal model"))
            if probe.is_some() =>
        {
            probe
                .unwrap()
                .refusal_errors
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => panic!("unexpected native peer transport/SDK error: {error:?}"),
    }
}

pub(crate) fn automatic_completed(state: &Arc<AppState>) {
    if let Some(probe) = for_state(state) {
        probe.automatic_completed.store(true, Ordering::Release);
    }
}

pub(crate) fn sequence_tick(state: &Arc<AppState>) {
    if let Some(probe) = for_state(state) {
        probe.sequence_ticks.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn packet_worker_registered(state: &Arc<AppState>, handle: tokio::task::AbortHandle) {
    if let Some(probe) = for_state(state) {
        assert!(
            probe
                .packet_worker_abort
                .lock()
                .unwrap()
                .replace(handle)
                .is_none(),
            "one actual packet owner per native fixture"
        );
    }
}

// Diagnostic-only: a contended observer is LOSS, never a fabricated outcome.
// This call is after try_commit has returned/dropped ALL final guards.
static FINAL_OUTCOME_OBSERVER_LOSSES:AtomicU64=AtomicU64::new(0);
pub(crate) fn final_outcome_observer_losses()->u64 {
    FINAL_OUTCOME_OBSERVER_LOSSES.load(Ordering::Acquire)
}
pub(crate) fn record_final_commit_outcome(
    state:&Arc<AppState>,result:&Result<(),crate::automatic_startup_idle::Error>,
) {
    let Ok(active)=ACTIVE.try_lock()else{
        FINAL_OUTCOME_OBSERVER_LOSSES.fetch_add(1,Ordering::Relaxed);return;
    };
    let Some(probe)=active.as_ref()else{return;};
    let Ok(owner)=probe.state.try_lock()else{
        FINAL_OUTCOME_OBSERVER_LOSSES.fetch_add(1,Ordering::Relaxed);return;
    };
    if !owner.upgrade().is_some_and(|s|Arc::ptr_eq(&s,state)){return;}
    use crate::automatic_startup_idle::Error;
    let at=match result {Ok(())=>0,Err(Error::Busy)=>1,Err(Error::Retired)=>2,
        Err(Error::StateChanged)=>3,Err(Error::StoreChanged)=>4,Err(Error::Sdk(_))=>5};
    use sedsnet::router::TryManagedBatchError as Sdk;
    if matches!(result,Err(Error::Sdk(Sdk::CoreBusy|Sdk::SchemaBusy))) {
        probe.sdk_contention.fetch_add(1,Ordering::Release);
    }
    probe.final_commit_outcomes[at].fetch_add(1,Ordering::Release);
}
