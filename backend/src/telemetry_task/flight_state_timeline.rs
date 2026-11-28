//! Bounded test-only trace for the unchanged 1500ms operator-state regression.
use sedsnet::{packet::Packet, router::Router};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, Weak,
};
use std::time::Instant;
const CAPACITY: usize = 1024;
const MAX_FRAME: usize = 2048;
static LOST_ACTIVE: AtomicU64 = AtomicU64::new(0);
#[derive(Clone, serde::Serialize)]
struct Event {
    ordinal: u64,
    thread: String,
    elapsed_us: u128,
    phase: &'static str,
    state: u8,
    packet_id: Option<u64>,
    source_ms: Option<u64>,
    ok: Option<bool>,
    detail: Option<String>,
    frame: Option<Vec<u8>>,
}
pub(crate) struct Trace {
    started: Instant,
    rows: Mutex<Vec<Event>>,
    next: AtomicU64,
    lost: AtomicU64,
}
static ACTIVE: Mutex<Option<(usize, Weak<Trace>)>> = Mutex::new(None);
impl Trace {
    pub(crate) fn start(router: &Router) -> Arc<Self> {
        LOST_ACTIVE.store(0, Ordering::Relaxed);
        let trace = Arc::new(Self {
            started: Instant::now(),
            rows: Mutex::new(Vec::with_capacity(CAPACITY)),
            next: AtomicU64::new(1),
            lost: AtomicU64::new(0),
        });
        *ACTIVE.lock().unwrap() = Some((router as *const Router as usize, Arc::downgrade(&trace)));
        trace
    }
    fn record(
        &self,
        phase: &'static str,
        state: u8,
        packet_id: Option<u64>,
        source_ms: Option<u64>,
        ok: Option<bool>,
        extra: (Option<String>, Option<&[u8]>),
    ) -> u64 {
        let (detail, frame) = extra;
        let ordinal = self.next.fetch_add(1, Ordering::Relaxed);
        let elapsed_us = self.started.elapsed().as_micros();
        let thread = format!("{:?}", std::thread::current().id());
        let Ok(mut rows) = self.rows.try_lock() else {
            self.lost.fetch_add(1, Ordering::Relaxed);
            return ordinal;
        };
        if rows.len() == CAPACITY || frame.is_some_and(|f| f.len() > MAX_FRAME) {
            self.lost.fetch_add(1, Ordering::Relaxed);
            return ordinal;
        }
        rows.push(Event {
            ordinal,
            thread,
            elapsed_us,
            phase,
            state,
            packet_id,
            source_ms,
            ok,
            detail,
            frame: frame.map(<[u8]>::to_vec),
        });
        ordinal
    }
    pub(crate) fn event(
        &self,
        phase: &'static str,
        state: u8,
        packet_id: Option<u64>,
        source_ms: Option<u64>,
        ok: Option<bool>,
    ) {
        self.record(phase, state, packet_id, source_ms, ok, (None, None));
    }
    pub(crate) fn wire_begin(&self, direction: &'static str, bytes: &[u8]) -> u64 {
        self.record(
            "packed_callback_begin",
            255,
            None,
            None,
            None,
            (Some(direction.into()), Some(bytes)),
        )
    }
    pub(crate) fn wire_end(&self, token: u64, result: &sedsnet::TelemetryResult<()>) {
        self.record(
            "packed_callback_end",
            255,
            None,
            None,
            Some(result.is_ok()),
            (Some(format!("begin={token}; result={result:?}")), None),
        );
    }
    pub(crate) fn packet(&self, pkt: &Packet, state: u8) {
        self.record(
            "peer_observed",
            state,
            Some(pkt.packet_id()),
            Some(pkt.timestamp()),
            None,
            (Some(format!(
                "type={:?}; sender={:?}; endpoints={:?}; payload={:?}",
                pkt.data_type(),
                pkt.sender(),
                pkt.endpoints(),
                pkt.payload()
            )), None),
        );
    }
    pub(crate) fn report(&self) -> serde_json::Value {
        let mut rows = self.rows.lock().unwrap().clone();
        rows.sort_by_key(|r| r.ordinal);
        // Decode only after the deadline/join, outside all trace and router locks.
        let decoded = rows.iter().filter_map(|r|r.frame.as_ref().map(|bytes| {
            let packet=sedsnet::wire_format::unpack_packet(bytes);
            serde_json::json!({"ordinal":r.ordinal,"frame_info":sedsnet::wire_format::peek_frame_info(bytes).map(|f|serde_json::json!({"envelope":format!("{:?}",f.envelope),"reliable":f.reliable.map(|r|serde_json::json!({"flags":r.flags,"seq":r.seq,"ack":r.ack}))})).map_err(|e|format!("{e:?}")),"packet":packet.map(|p|serde_json::json!({"id":p.packet_id(),"type":p.data_type().as_u32(),"sender":p.sender(),"source_ms":p.timestamp(),"endpoints":p.endpoints().iter().map(|e|e.as_u32()).collect::<Vec<_>>(),"payload":p.payload()})).map_err(|e|format!("{e:?}"))})
        })).collect::<Vec<_>>();
        serde_json::json!({"clock":"actual std::time::Instant since fixture bind","capacity":CAPACITY,"max_frame_bytes":MAX_FRAME,"capture_lost":self.lost.load(Ordering::Relaxed),"active_lookup_lost":LOST_ACTIVE.load(Ordering::Relaxed),"events":rows,"decoded_after_join":decoded})
    }
}
pub(crate) fn event(
    router: &Router,
    phase: &'static str,
    state: u8,
    packet_id: Option<u64>,
    source_ms: Option<u64>,
    ok: Option<bool>,
) {
    detail(router, phase, state, packet_id, source_ms, ok, None);
}
pub(crate) fn detail(
    router: &Router,
    phase: &'static str,
    state: u8,
    packet_id: Option<u64>,
    source_ms: Option<u64>,
    ok: Option<bool>,
    detail: Option<String>,
) {
    let Ok(active) = ACTIVE.try_lock() else {
        LOST_ACTIVE.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let trace = active.as_ref().and_then(|(owner, trace)| {
        (*owner == router as *const Router as usize)
            .then(|| trace.upgrade())
            .flatten()
    });
    drop(active);
    if let Some(trace) = trace {
        trace.record(phase, state, packet_id, source_ms, ok, (detail, None));
    }
}

/// Opt-in fixture evidence only. The caller runs this after the unchanged
/// deadline and actual telemetry-task shutdown/join. Never overwrite a file.
pub(crate) fn write_artifact_if_requested(report: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write;
    let Some(path) = std::env::var_os("GS_TEST_FSTATE_TRACE_PATH") else {
        return Ok(());
    };
    let path = std::path::PathBuf::from(path);
    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "trace artifact path must be absolute",
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    serde_json::to_writer(&mut file, report)?;
    file.write_all(b"\n")?;
    file.flush()
}
