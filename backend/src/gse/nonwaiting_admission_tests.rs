//! Actual production enqueue controls; only the deterministic pauses are test-only.
use crate::state::AppState;
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

const TRACE_CAPACITY: usize = 64;
static TRACE_LOOKUP_MISSES: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
#[repr(u64)]
pub(crate) enum TraceEvent {
    SdkWindowStarted = 1,
    SdkOfferCalled,
    SdkOfferReturned,
    HolderSpawnRequested,
    HolderStarted,
    CoreEnteredObserved,
    BusyObserved,
    Sleep20Started,
    Sleep20Returned,
    ReleaseRequested,
    CoreReleaseNotified,
    HolderCompleted,
    CallerReturned,
    HolderClockCallbackReturning,
}

pub(crate) struct Trace {
    origin: Instant,
    enabled: AtomicBool,
    next: AtomicUsize,
    lost: AtomicUsize,
    release_seen: AtomicBool,
    codes: [AtomicU64; TRACE_CAPACITY],
    nanos: [AtomicU64; TRACE_CAPACITY],
    aux: [AtomicU64; TRACE_CAPACITY],
}
impl Trace {
    fn new() -> Self {
        Self {
            origin: Instant::now(), enabled: AtomicBool::new(false),
            next: AtomicUsize::new(0), lost: AtomicUsize::new(0),
            release_seen: AtomicBool::new(false),
            codes: std::array::from_fn(|_| AtomicU64::new(0)),
            nanos: std::array::from_fn(|_| AtomicU64::new(0)),
            aux: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
    pub(crate) fn enable(&self, enabled: bool) { self.enabled.store(enabled, Ordering::Release); }
    fn at(&self, event: TraceEvent, at: Instant, aux: u64) {
        if !self.enabled.load(Ordering::Acquire) { return; }
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        if index >= TRACE_CAPACITY { self.lost.fetch_add(1, Ordering::Relaxed); return; }
        let Some(elapsed) = at.checked_duration_since(self.origin) else {
            self.lost.fetch_add(1, Ordering::Relaxed); return;
        };
        self.nanos[index].store(elapsed.as_nanos().min(u128::from(u64::MAX)) as u64, Ordering::Relaxed);
        self.aux[index].store(aux, Ordering::Relaxed);
        self.codes[index].store(event as u64, Ordering::Release);
    }
    pub(crate) fn mark(&self, event: TraceEvent, aux: u64) {
        if self.enabled.load(Ordering::Acquire) { self.at(event, Instant::now(), aux); }
    }
    pub(crate) fn snapshot(&self) -> serde_json::Value {
        let mut incomplete = 0;
        let rows = (0..self.next.load(Ordering::Acquire).min(TRACE_CAPACITY)).map(|index| {
            let code = self.codes[index].load(Ordering::Acquire);
            if code == 0 { incomplete += 1; }
            let event = match code {
                1 => "sdk_window_started_existing_instant", 2 => "sdk_offer_called",
                3 => "sdk_offer_return_observed", 4 => "holder_spawn_requested",
                5 => "holder_thread_started", 6 => "core_entered_observed_by_parent",
                7 => "zero_owner_busy_observed_by_parent", 8 => "original_sleep20_started",
                9 => "original_sleep20_returned", 10 => "release_requested",
                11 => "release_flag_written_and_notified_observed", 12 => "holder_seed_function_completed",
                13 => "caller_function_returned", 14 => "holder_clock_callback_return_sample",
                _ => "uncommitted_or_unknown",
            };
            serde_json::json!({"index":index,"event":event,
                "ns_since_trace_install":self.nanos[index].load(Ordering::Relaxed),
                "aux":self.aux[index].load(Ordering::Relaxed)})
        }).collect::<Vec<_>>();
        serde_json::json!({"capacity":TRACE_CAPACITY,"lost":self.lost.load(Ordering::Relaxed),
            "lookup_misses":TRACE_LOOKUP_MISSES.load(Ordering::Relaxed),"incomplete":incomplete,"events":rows,
            "clock":"wall monotonic; window origin copies existing production Instant; not packet UTC",
            "aux_contract":"SDK return: accepted_before*4 + (0 success,1 CoreBusy,2 other error); SDK called: ordinal; caller returned: Ok count or u64MAX for Err"})
    }
}

fn trace_for(owner: usize) -> Option<Arc<Trace>> {
    let slot = HOOK.get()?;
    let Ok(guard) = slot.try_lock() else {
        TRACE_LOOKUP_MISSES.fetch_add(1, Ordering::Relaxed); return None;
    };
    guard.as_ref().filter(|h| h.owner == owner).map(|h| h.trace.clone())
}


struct Hook {
    owner: usize,
    prepared: mpsc::Sender<()>,
    proceed: Mutex<mpsc::Receiver<()>>,
    core_entered: mpsc::Sender<()>,
    busy: mpsc::Sender<()>,
    packet: Arc<Mutex<Option<sedsnet::packet::Packet>>>,
    core_release: Arc<(Mutex<bool>, Condvar)>,
    launch_packets:Arc<Mutex<Vec<sedsnet::packet::Packet>>>,
    launch_calls:Arc<std::sync::atomic::AtomicUsize>,
    retry_gate:Arc<(Mutex<bool>,Condvar)>,
    trace: Arc<Trace>,
}
static HOOK: OnceLock<Mutex<Option<Arc<Hook>>>> = OnceLock::new();
pub(crate) struct Pause {
    pub(crate) prepared: mpsc::Receiver<()>,
    pub(crate) proceed: mpsc::Sender<()>,
    pub(crate) core_entered: mpsc::Receiver<()>,
    pub(crate) busy:mpsc::Receiver<()>,
    pub(crate) packet:Arc<Mutex<Option<sedsnet::packet::Packet>>>,
    release: Arc<(Mutex<bool>, Condvar)>,
    pub(crate) launch_packets:Arc<Mutex<Vec<sedsnet::packet::Packet>>>,
    pub(crate) launch_calls:Arc<std::sync::atomic::AtomicUsize>,
    retry_gate:Arc<(Mutex<bool>,Condvar)>,
    pub(crate) trace: Arc<Trace>,
}
impl Pause {
    pub(crate) fn install(state: &AppState) -> Self {
        TRACE_LOOKUP_MISSES.store(0, Ordering::Relaxed);
        let trace = Arc::new(Trace::new());
        let (prepared, received) = mpsc::channel();
        let (proceed, continued) = mpsc::channel();
        let (core_entered, core_received) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (busy,busy_received)=mpsc::channel();
        let packet=Arc::new(Mutex::new(None));
        let launch_packets=Arc::new(Mutex::new(Vec::new()));
        let launch_calls=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retry_gate=Arc::new((Mutex::new(false),Condvar::new()));
        *HOOK.get_or_init(Default::default).lock().unwrap() = Some(Arc::new(Hook {
            owner: state as *const AppState as usize,
            prepared,
            proceed: Mutex::new(continued),
            core_entered,
            busy,
            packet:packet.clone(),
            core_release: release.clone(),
            launch_packets:launch_packets.clone(),launch_calls:launch_calls.clone(),retry_gate:retry_gate.clone(),
            trace: trace.clone(),
        }));
        Self {
            prepared: received,
            proceed,
            core_entered: core_received,
            busy:busy_received,
            packet,
            release,
            launch_packets,launch_calls,retry_gate,trace,
        }
    }
    pub(crate) fn release_core(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
        if !self.trace.release_seen.swap(true, Ordering::Relaxed) {
            self.trace.mark(TraceEvent::CoreReleaseNotified, 0);
        }
    }
    pub(crate) fn hold_retry(&self){*self.retry_gate.0.lock().unwrap()=true;}
    pub(crate) fn resume_retry(&self){*self.retry_gate.0.lock().unwrap()=false;self.retry_gate.1.notify_all();}
    pub(crate) fn release(&self) {
        self.release_core();self.resume_retry();
        let _ = self.proceed.send(());
    }
}
impl Drop for Pause {
    fn drop(&mut self) {
        self.release();
        *HOOK.get().unwrap().lock().unwrap() = None;
    }
}
fn hook(owner: usize) -> Option<Arc<Hook>> {
    HOOK.get()?
        .lock()
        .unwrap()
        .as_ref()
        .filter(|h| h.owner == owner)
        .cloned()
}
pub(crate) fn after_prepared(state: &AppState) {
    if let Some(hook) = hook(state as *const AppState as usize) {
        hook.prepared.send(()).unwrap();
        hook.proceed
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }
}
pub(crate) fn clock_pause(owner: usize) {
    if std::thread::current().name() != Some("production-sdk-core-holder") {
        return;
    }
    if let Some(hook) = hook(owner) {
        let _ = hook.core_entered.send(());
        let released = hook.core_release.0.lock().unwrap();
        let (released, _) = hook
            .core_release
            .1
            .wait_timeout_while(released, Duration::from_secs(5), |released| !*released)
            .unwrap();
        assert!(
            *released,
            "RAII must release the actual public SDK core holder"
        );
    }
}

pub(crate) fn launch_ticket(state: &AppState, revision: u64) -> super::LaunchAdmission {
    super::LaunchAdmission::capture_for_launch(state, Some(revision)).unwrap()
}

pub(crate) fn core_busy_refused(state:&AppState) {
    if let Some(hook)=hook(state as *const AppState as usize){let _=hook.busy.send(());}
}
pub(crate) fn record_prepared_packet(state: &AppState, packet: &sedsnet::packet::Packet) {
    if let Some(hook) = hook(state as *const AppState as usize) { *hook.packet.lock().unwrap() = Some(packet.clone()); }
}
pub(crate) fn after_prepared_packet(state:&AppState,packet:&sedsnet::packet::Packet) {
    record_prepared_packet(state, packet);
    after_prepared(state);
}

pub(crate) fn record_launch_packets(state:&AppState,packets:&[sedsnet::packet::Packet]){
    if let Some(h)=hook(state as *const AppState as usize){*h.launch_packets.lock().unwrap()=packets.to_vec();}
}
pub(crate) fn record_launch_sdk_call(state:&AppState, first_sdk_attempt: Option<Instant>){
    state.ignition_observer.launch_sdk_offers.fetch_add(1, Ordering::Relaxed);
    if let Some(h)=hook(state as *const AppState as usize){
        let ordinal = h.launch_calls.fetch_add(1, Ordering::Relaxed) + 1;
        if ordinal == 1 && let Some(at) = first_sdk_attempt {
            h.trace.at(TraceEvent::SdkWindowStarted, at, 0);
        }
        h.trace.mark(TraceEvent::SdkOfferCalled, ordinal as u64);
    }
}
pub(crate) fn record_holder_clock_return(owner: usize, sampled_at: Instant, original_clock_ms: u64) {
    if std::thread::current().name() != Some("production-sdk-core-holder") { return; }
    if let Some(trace) = trace_for(owner) {
        // Reuse the ONE clock sample already taken by the original closure.
        // This may run under SDK Core: try-only lookup, fixed atomic stores.
        trace.at(TraceEvent::HolderClockCallbackReturning, sampled_at, original_clock_ms);
    }
}
pub(crate) fn record_launch_sdk_return(state: &AppState, accepted: usize, error: Option<&sedsnet::TelemetryError>) {
    if let Some(trace) = trace_for(state as *const AppState as usize) {
        let class = match error {
            None => 0,
            Some(sedsnet::TelemetryError::Io("router core busy")) => 1,
            Some(_) => 2,
        };
        trace.mark(TraceEvent::SdkOfferReturned, (accepted as u64).saturating_mul(4).saturating_add(class));
    }
}

pub(crate) fn launch_retry_boundary(state:&AppState){
    if let Some(h)=hook(state as *const AppState as usize){
        let held=h.retry_gate.0.lock().unwrap();
        let(held,_)=h.retry_gate.1.wait_timeout_while(held,Duration::from_secs(5),|v|*v).unwrap();
        assert!(!*held,"owned testretry gate must be released outsideallAppguards");
    }
}
