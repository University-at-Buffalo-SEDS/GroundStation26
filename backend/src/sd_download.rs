//! Network download of board SD files into the offline SD recovery pipeline.
//!
//! Boards serve their SD card on SEDSnet P2P stream port 4511 with the SD
//! transfer protocol v2 (`docs/backend/sd-transfer.md`, [`xfer`]): the station
//! OPENs a byte range, the board streams it as independent 16 KiB blocks in
//! SDX ([`sdx`], lossless binary rows, about 5-6x smaller than the CSV) paced
//! to the rate and window the station sets, and the station acknowledges
//! selectively, verifies each block's length and CRC-32, appends the verified
//! prefix to a `.part` file and finally compares the board's SHA-256 of the
//! file with its own copy's. A transfer can be paused (rate 0), cancelled and
//! resumed at any committed byte. Transfers run one at a time and are only
//! admitted (and only continue) in Idle, Landed or Recovery with no
//! fill/launch sequence active. Completed files are handed to
//! `recording_sd::upload` unchanged, so the existing import, provenance and
//! "SD rows win" merge rules apply.
//!
//! The DAQ serves `/LAUNCH`, `/UTC` and `/NO_UTC` (directories 0..2); the
//! Flight Computer serves its card root (directory 0, `ROOT` here, listing
//! `irec*.csv`), and its ground-test image also the IGT replay dataset
//! (directory 1, `/IGT`, read by [`fc_igt_dataset`] for provenance only). The
//! FC is collected automatically after landing by [`fc`].
use crate::{
    auth::Permission,
    state::AppState,
    types::{Board, FlightState},
};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use sedsnet::router::{P2pStreamEventKind, P2pStreamId, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

pub const SD_FILE_STREAM_PORT: u16 = 4511;
use xfer::{OP_CANCEL, OP_CONFIRM, OP_LIST, RESPONSE as RESPONSE_FLAG, STATUS_BUSY, STATUS_END, STATUS_OK};
const STATUS_BAD_MESSAGE: u8 = 1;
const STATUS_BAD_STATE: u8 = 2;
const LIST_HEADER: usize = 12;
const ENTRY_OPEN: u8 = 0x01;
const NAME_MAX: usize = 63;
/// Unit of the lead-up's tail reads and binary-search probes.
pub const BLOCK_BYTES: u64 = 512;
pub const DIRECTORIES: [&str; 3] = ["LAUNCH", "UTC", "NO_UTC"];
/// The FC serves its card root only (directory 0).
pub const FC_DIRECTORIES: [&str; 1] = ["ROOT"];
const MAX_ENTRIES_PER_DIR: u16 = 4096;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const GATE_POLL: Duration = Duration::from_millis(100);
const MAX_RETRIES: u32 = 6;
const SOURCE_PORT_FIRST: u16 = 60_000;
const SOURCE_PORT_LAST: u16 = 64_999;

static MANAGER: LazyLock<Manager> = LazyLock::new(Manager::default);
/// Transfer ids differ between streams too (a stale DATA of an earlier
/// stream can never match).
static NEXT_XFER: AtomicU8 = AtomicU8::new(1);

/// Boards whose current firmware serves the SD download stream.
pub fn supports_sd_download(board: Board) -> bool {
    matches!(board, Board::DaqBoard | Board::FlightComputer)
}

/// The directory table a board serves, by wire index.
pub fn directories(board: Board) -> &'static [&'static str] {
    match board {
        Board::FlightComputer => &FC_DIRECTORIES,
        _ => &DIRECTORIES,
    }
}

/// Downloads run only while nothing is being filled, armed or flown.
pub fn download_allowed(state: FlightState) -> bool {
    matches!(
        state,
        FlightState::Idle | FlightState::Landed | FlightState::Recovery
    )
}

fn station_gate(state: &AppState) -> Option<String> {
    let flight = state.local_flight_state_snapshot();
    if !download_allowed(flight) {
        return Some(format!(
            "SD downloads run only in Idle, Landed or Recovery (now {flight:?})"
        ));
    }
    if state.operator_queue.fill_launch_active() {
        return Some("SD downloads are paused while a fill/launch sequence is active".into());
    }
    None
}

pub(crate) type Gate = Arc<dyn Fn() -> Option<String> + Send + Sync>;

fn max_bytes_per_second() -> u64 {
    std::env::var("GS_SD_DOWNLOAD_MAX_BYTES_PER_SEC")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (512..=262_144).contains(value))
        .unwrap_or(16 * 1024)
}

/// Operator downloads: the FC is usually reached over the AV-bay radio, so
/// its rate follows the FC collection's link rule (radio 512 B/s).
fn max_bytes_per_second_for(state: &AppState, board: Board) -> u64 {
    match board {
        Board::FlightComputer => fc::rate(fc::link(state)),
        _ => max_bytes_per_second(),
    }
}

// ---------------------------------------------------------------- wire codec

fn encode_list(dir: u8, index: u16) -> [u8; 4] {
    let i = index.to_le_bytes();
    [OP_LIST, dir, i[0], i[1]]
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListFrame {
    status: u8,
    dir: u8,
    index: u16,
    size: u32,
    flags: u8,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Frame {
    List(ListFrame),
    Open(xfer::OpenAnswer),
    Hash(xfer::HashAnswer),
    Cancel(u8),
    Confirm(u8),
}

fn u32_at(p: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(p[at..at + 4].try_into().expect("four bytes"))
}

/// Answers other than DATA (which [`Fetch`] reads itself).
fn decode_frame(p: &[u8]) -> Option<Frame> {
    match *p.first()? {
        b if b == OP_LIST | RESPONSE_FLAG => {
            if p.len() < LIST_HEADER {
                return None;
            }
            let name_len = p[10] as usize;
            if p.len() != LIST_HEADER + name_len {
                return None;
            }
            let name = std::str::from_utf8(&p[LIST_HEADER..]).ok()?.to_owned();
            Some(Frame::List(ListFrame {
                status: p[1],
                dir: p[2],
                index: u16::from_le_bytes([p[3], p[4]]),
                size: u32_at(p, 5),
                flags: p[9],
                name,
            }))
        }
        b if b == xfer::OP_OPEN | RESPONSE_FLAG => xfer::OpenAnswer::decode(p).map(Frame::Open),
        b if b == xfer::OP_HASH | RESPONSE_FLAG => xfer::HashAnswer::decode(p).map(Frame::Hash),
        b if b == OP_CANCEL | RESPONSE_FLAG && p.len() == 2 => Some(Frame::Cancel(p[1])),
        b if b == OP_CONFIRM | RESPONSE_FLAG && p.len() == 2 => Some(Frame::Confirm(p[1])),
        _ => None,
    }
}

/// Same leaf-name rule as the firmware: no separators, not dot-only.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        && name.bytes().any(|b| b != b'.')
}

fn dir_index(board: Board, dir: &str) -> Option<u8> {
    directories(board)
        .iter()
        .position(|d| d.eq_ignore_ascii_case(dir))
        .map(|i| i as u8)
}

fn status_text(status: u8) -> &'static str {
    match status {
        0 => "ok",
        1 => "bad message",
        2 => "board refused: not in an SD download state (DAQ: Idle/Landed/Recovery without launch logging; FC: Landed/Recovery)",
        3 => "board busy",
        4 => "file not found on SD card",
        5 => "SD read error",
        6 => "end of file",
        7 => "bad offset",
        _ => "unknown board status",
    }
}

// ------------------------------------------------------------- job manager

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Queued,
    Connecting,
    Transferring,
    Complete,
    Paused,
    Cancelling,
    Cancelled,
    Failed,
}

impl Phase {
    fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Connecting | Self::Transferring | Self::Cancelling
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub id: u64,
    pub board: String,
    pub dir: String,
    pub name: String,
    pub phase: Phase,
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub resumed_from: u64,
    pub bytes_per_second: u64,
    pub message: String,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
    /// READ answers whose CRC-32 did not match and were requested again.
    #[serde(default)]
    pub crc_errors: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RemoteFile {
    pub dir: String,
    pub name: String,
    pub size: u64,
    pub open: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    pub board: String,
    pub listed_at_ms: u64,
    pub files: Vec<RemoteFile>,
}

struct JobEntry {
    status: JobStatus,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
pub(crate) struct Manager {
    jobs: Mutex<HashMap<u64, JobEntry>>,
    listings: Mutex<HashMap<String, Listing>>,
    listing_busy: AtomicBool,
    next_id: AtomicU64,
    next_port: AtomicU16,
}

impl Manager {
    fn create_job(
        &self,
        board: Board,
        dir: &str,
        name: &str,
    ) -> Result<(u64, Arc<AtomicBool>), String> {
        let mut jobs = self.jobs.lock().expect("sd download jobs");
        if let Some(active) = jobs.values().find(|job| job.status.phase.is_active()) {
            return Err(format!(
                "SD download {} ({}) is already active",
                active.status.id, active.status.name
            ));
        }
        if self.listing_busy.load(Ordering::Acquire) {
            return Err("SD file listing is already active".into());
        }
        // Keep a bounded history.
        if jobs.len() >= 32
            && let Some(oldest) = jobs.keys().min().copied()
        {
            jobs.remove(&oldest);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let now = now_ms();
        let cancel = Arc::new(AtomicBool::new(false));
        jobs.insert(
            id,
            JobEntry {
                status: JobStatus {
                    id,
                    board: board.sender_id().into(),
                    dir: dir.into(),
                    name: name.into(),
                    phase: Phase::Queued,
                    bytes_done: 0,
                    total_bytes: 0,
                    resumed_from: 0,
                    bytes_per_second: 0,
                    message: "queued".into(),
                    started_at_ms: now,
                    updated_at_ms: now,
                    crc_errors: 0,
                },
                cancel: cancel.clone(),
            },
        );
        Ok((id, cancel))
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut JobStatus)) {
        if let Some(job) = self.jobs.lock().expect("sd download jobs").get_mut(&id) {
            f(&mut job.status);
            job.status.updated_at_ms = now_ms();
        }
    }

    fn get(&self, id: u64) -> Option<JobStatus> {
        self.jobs
            .lock()
            .expect("sd download jobs")
            .get(&id)
            .map(|job| job.status.clone())
    }

    fn list(&self) -> Vec<JobStatus> {
        let mut out = self
            .jobs
            .lock()
            .expect("sd download jobs")
            .values()
            .map(|job| job.status.clone())
            .collect::<Vec<_>>();
        out.sort_by_key(|job| std::cmp::Reverse(job.id));
        out
    }

    fn cancel(&self, id: u64) -> Result<JobStatus, String> {
        let mut jobs = self.jobs.lock().expect("sd download jobs");
        let job = jobs.get_mut(&id).ok_or("SD download not found")?;
        if !job.status.phase.is_active() {
            return Err("SD download is no longer active".into());
        }
        job.cancel.store(true, Ordering::Release);
        job.status.phase = Phase::Cancelling;
        job.status.message = "cancelling".into();
        job.status.updated_at_ms = now_ms();
        Ok(job.status.clone())
    }

    fn active(&self) -> bool {
        self.listing_busy.load(Ordering::Acquire)
            || self
                .jobs
                .lock()
                .expect("sd download jobs")
                .values()
                .any(|job| job.status.phase.is_active())
    }

    fn next_source_port(&self) -> u16 {
        let span = u32::from(SOURCE_PORT_LAST - SOURCE_PORT_FIRST) + 1;
        let n = u32::from(self.next_port.fetch_add(1, Ordering::Relaxed)) % span;
        SOURCE_PORT_FIRST + n as u16
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ----------------------------------------------------------- board stream

pub(crate) struct OwnedEvent {
    kind: P2pStreamEventKind,
    stream_id: P2pStreamId,
    peer: String,
    payload: Vec<u8>,
}

#[derive(Debug)]
pub(crate) enum StreamError {
    Gate(String),
    Cancelled,
    Other(String),
}

impl From<String> for StreamError {
    fn from(value: String) -> Self {
        Self::Other(value)
    }
}

pub(crate) struct BoardStream {
    router: Arc<Router>,
    board: &'static str,
    port: u16,
    stream: P2pStreamId,
    rx: mpsc::UnboundedReceiver<OwnedEvent>,
    open: bool,
    /// The last transfer id used on this stream.
    xfer: u8,
    /// CANCELs sent on this stream whose answer has not come back yet.
    unanswered_cancels: u32,
}

impl BoardStream {
    async fn open(
        manager: &Manager,
        router: Arc<Router>,
        board: Board,
        gate: &Gate,
        cancel: &AtomicBool,
    ) -> Result<Self, StreamError> {
        Self::open_peer(manager, router, board.sender_id(), SD_FILE_STREAM_PORT, gate, cancel).await
    }

    /// A stream to any SD file service (a board on 4511, the RRLS relay
    /// outbox on `relay::RELAY_STREAM_PORT`).
    async fn open_peer(
        manager: &Manager,
        router: Arc<Router>,
        host: &'static str,
        service_port: u16,
        gate: &Gate,
        cancel: &AtomicBool,
    ) -> Result<Self, StreamError> {
        if router.resolve_hostname(host).is_none() {
            return Err(StreamError::Other(format!(
                "{host} is not present in the SEDSnet address book"
            )));
        }
        let (tx, rx) = mpsc::unbounded_channel::<OwnedEvent>();
        let mut port = 0;
        for _ in 0..8 {
            let candidate = manager.next_source_port();
            let tx = tx.clone();
            if router
                .bind_p2p_stream_port(candidate, move |event| {
                    tx.send(OwnedEvent {
                        kind: event.kind,
                        stream_id: event.stream_id,
                        peer: event.peer_hostname.to_string(),
                        payload: event.payload.to_vec(),
                    })
                    .map_err(|_| sedsnet::TelemetryError::HandlerError("SD stream receiver closed"))
                })
                .is_ok()
            {
                port = candidate;
                break;
            }
        }
        if port == 0 {
            return Err(StreamError::Other("no free SD download source port".into()));
        }
        let stream = match router.open_p2p_stream_to_hostname(host, service_port, port) {
            Ok(stream) => stream,
            Err(err) => {
                router.clear_p2p_stream_port(port);
                return Err(StreamError::Other(format!(
                    "failed to open SD stream to {host}: {err}"
                )));
            }
        };
        let mut this = Self {
            router,
            board: host,
            port,
            stream,
            rx,
            open: true,
            xfer: NEXT_XFER.fetch_add(97, Ordering::Relaxed),
            unanswered_cancels: 0,
        };
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            let event = this.next_event(deadline, gate, cancel).await?;
            match event.kind {
                P2pStreamEventKind::Connected => return Ok(this),
                P2pStreamEventKind::Closed | P2pStreamEventKind::Reset => {
                    this.open = false;
                    return Err(StreamError::Other(format!(
                        "{host} refused the SD stream (not in Idle/Landed/Recovery, or firmware without SD download)"
                    )));
                }
                _ => {}
            }
        }
    }

    /// Next event for this stream, honouring the deadline, the state gate and
    /// cancellation at least every 100 ms.
    async fn next_event(
        &mut self,
        deadline: Instant,
        gate: &Gate,
        cancel: &AtomicBool,
    ) -> Result<OwnedEvent, StreamError> {
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(StreamError::Cancelled);
            }
            if let Some(reason) = gate() {
                return Err(StreamError::Gate(reason));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(StreamError::Other(format!(
                    "timed out waiting for {}",
                    self.board
                )));
            }
            let wait = (deadline - now).min(GATE_POLL);
            match tokio::time::timeout(wait, self.rx.recv()).await {
                Ok(Some(event)) if event.stream_id == self.stream && event.peer == self.board => {
                    return Ok(event);
                }
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => return Err(StreamError::Other("SD stream channel closed".into())),
            }
        }
    }

    async fn next_frame(
        &mut self,
        deadline: Instant,
        gate: &Gate,
        cancel: &AtomicBool,
    ) -> Result<Frame, StreamError> {
        loop {
            let event = self.next_event(deadline, gate, cancel).await?;
            match event.kind {
                P2pStreamEventKind::Data => {
                    self.note_answer(&event.payload);
                    if let Some(frame) = decode_frame(&event.payload) {
                        return Ok(frame);
                    }
                }
                P2pStreamEventKind::Closed | P2pStreamEventKind::Reset => {
                    self.open = false;
                    return Err(StreamError::Other(format!(
                        "{} closed the SD stream (state change or reboot)",
                        self.board
                    )));
                }
                _ => {}
            }
        }
    }

    /// A new transfer id (1..255), never the previous one on this stream.
    fn next_xfer(&mut self) -> u8 {
        self.xfer = self.xfer.wrapping_add(1).max(1);
        self.xfer
    }

    /// The next DATA payload within `wait`; `None` when nothing arrived.
    /// A closed or reset stream is an error.
    async fn recv(&mut self, wait: Duration) -> Result<Option<Vec<u8>>, StreamError> {
        let deadline = Instant::now() + wait;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            match tokio::time::timeout(deadline - now, self.rx.recv()).await {
                Ok(Some(event)) if event.stream_id == self.stream && event.peer == self.board => match event.kind {
                    P2pStreamEventKind::Data => {
                        self.note_answer(&event.payload);
                        return Ok(Some(event.payload));
                    }
                    P2pStreamEventKind::Closed | P2pStreamEventKind::Reset => {
                        self.open = false;
                        return Err(StreamError::Other(format!("{} closed the SD stream (state change or reboot)", self.board)));
                    }
                    _ => {}
                },
                Ok(Some(_)) => {}
                Err(_) => return Ok(None),
                Ok(None) => return Err(StreamError::Other("SD stream channel closed".into())),
            }
        }
    }

    fn send(&mut self, payload: &[u8]) -> Result<(), StreamError> {
        self.router
            .send_p2p_stream(self.stream, payload)
            .map_err(|err| {
                StreamError::Other(format!("SD stream send to {} failed: {err}", self.board))
            })?;
        if payload == [OP_CANCEL] {
            self.unanswered_cancels += 1;
        }
        Ok(())
    }

    fn note_answer(&mut self, payload: &[u8]) {
        if matches!(payload, [b, _] if *b == OP_CANCEL | RESPONSE_FLAG) {
            self.unanswered_cancels = self.unanswered_cancels.saturating_sub(1);
        }
    }

    /// Waits (until `deadline` at most) for the board to answer every CANCEL
    /// sent on this stream. The SDK does not keep two sends on one stream in
    /// order: a frame refused by a busy side is queued for retry and the next
    /// one can go out ahead of it. A request that must reach the board after
    /// a CANCEL is sent only once the CANCEL has been answered.
    async fn settle_cancels(&mut self, deadline: Instant, gate: &Gate, cancel: &AtomicBool) -> Result<(), StreamError> {
        while self.unanswered_cancels > 0 {
            match self.next_frame(deadline, gate, cancel).await {
                Ok(_) => {}
                Err(StreamError::Other(e)) if e.starts_with("timed out") => {
                    // An answer lost on the link: carry on as before.
                    self.unanswered_cancels = 0;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Clean close: CANCEL drops any queued answer on the board, then FIN.
    fn finish(mut self, abort: bool) {
        if self.open {
            let _ = self.router.send_p2p_stream(self.stream, &[OP_CANCEL]);
            if abort {
                let _ = self.router.reset_p2p_stream(self.stream);
            } else {
                let _ = self.router.close_p2p_stream(self.stream);
            }
            self.open = false;
        }
        self.router.clear_p2p_stream_port(self.port);
    }
}

impl Drop for BoardStream {
    fn drop(&mut self) {
        if self.open {
            let _ = self.router.reset_p2p_stream(self.stream);
            self.router.clear_p2p_stream_port(self.port);
        }
    }
}

async fn list_on_stream(
    stream: &mut BoardStream,
    board: Board,
    gate: &Gate,
    cancel: &AtomicBool,
) -> Result<Vec<RemoteFile>, StreamError> {
    list_dirs(stream, directories(board), board == Board::FlightComputer, gate, cancel).await
}

async fn list_dirs(
    stream: &mut BoardStream,
    dirs: &[&str],
    fc_names_only: bool,
    gate: &Gate,
    cancel: &AtomicBool,
) -> Result<Vec<RemoteFile>, StreamError> {
    let mut files = Vec::new();
    for (dir, dir_name) in dirs.iter().enumerate() {
        // The FC lists only `irec*.csv`; anything else is ignored.
        let keep = |name: &str| {
            !fc_names_only
                || crate::recording_store::board_sd::classify_name(name).is_some_and(|(b, _)| b == "FC")
        };
        match list_dir(stream, dir as u8, dir_name, keep, gate, cancel).await? {
            Some(entries) => files.extend(entries),
            None => return Err(StreamError::Other(status_text(STATUS_BAD_MESSAGE).into())),
        }
    }
    Ok(files)
}

/// One directory's entries whose names `keep` accepts. `Ok(None)`: the board
/// refused the directory as a bad message (it does not serve it).
async fn list_dir(
    stream: &mut BoardStream,
    dir: u8,
    dir_name: &str,
    keep: impl Fn(&str) -> bool,
    gate: &Gate,
    cancel: &AtomicBool,
) -> Result<Option<Vec<RemoteFile>>, StreamError> {
    let mut files = Vec::new();
    let mut index = 0_u16;
    let mut attempts = 0_u32;
    while index < MAX_ENTRIES_PER_DIR {
        stream.send(&encode_list(dir, index))?;
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        let frame = loop {
            match stream.next_frame(deadline, gate, cancel).await {
                Ok(Frame::List(frame)) if frame.status != STATUS_OK || (frame.dir == dir && frame.index == index) => {
                    break Ok(frame);
                }
                Ok(_) => {}
                Err(StreamError::Other(e)) if e.starts_with("timed out") => break Err(e),
                Err(e) => return Err(e),
            }
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(e) => {
                attempts += 1;
                if attempts > MAX_RETRIES {
                    return Err(StreamError::Other(e));
                }
                continue;
            }
        };
        match frame.status {
            STATUS_OK => {
                if keep(&frame.name) && valid_name(&frame.name) {
                    files.push(RemoteFile {
                        dir: dir_name.into(),
                        name: frame.name,
                        size: u64::from(frame.size),
                        open: frame.flags & ENTRY_OPEN != 0,
                    });
                }
                index += 1;
                attempts = 0;
            }
            STATUS_END => break,
            STATUS_BUSY => {
                attempts += 1;
                if attempts > 400 {
                    return Err(StreamError::Other("board stayed busy while listing".into()));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            STATUS_BAD_STATE => {
                return Err(StreamError::Gate(status_text(STATUS_BAD_STATE).into()));
            }
            STATUS_BAD_MESSAGE => return Ok(None),
            other => return Err(StreamError::Other(status_text(other).into())),
        }
    }
    Ok(Some(files))
}

// ------------------------------------------------------- FC IGT dataset

/// FC directory 1: the integrated ground test's replay dataset (`/IGT`),
/// served read-only by the ground-test image only.
pub(crate) const FC_IGT_DIR: u8 = 1;

/// One file of the FC's `/IGT` replay dataset as the FC reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgtDatasetFile {
    pub name: String,
    pub size: u64,
    /// The FC's SHA-256 of the file on the card (hex).
    pub sha256: String,
}

/// The replay's file names: `IMU|BARO|GPS<digit>.CSV`.
pub(crate) fn igt_dataset_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["IMU", "BARO", "GPS"].iter().any(|stem| {
        upper
            .strip_prefix(stem)
            .and_then(|rest| rest.strip_suffix(".CSV"))
            .is_some_and(|digit| digit.len() == 1 && digit.as_bytes()[0].is_ascii_digit())
    })
}

/// Lists the FC's `/IGT` dataset (directory 1) and asks the FC for each
/// file's SHA-256 (39-byte answers; nothing is downloaded). `Ok(None)`: the
/// FC image does not serve the directory (a flight image).
pub(crate) async fn fc_igt_dataset(
    manager: &'static Manager,
    router: Arc<Router>,
    gate: Gate,
) -> Result<Option<Vec<IgtDatasetFile>>, StreamError> {
    if let Some(reason) = gate() {
        return Err(StreamError::Gate(reason));
    }
    if manager.listing_busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err(StreamError::Other("SD file listing is already active".into()));
    }
    struct Busy(&'static Manager);
    impl Drop for Busy {
        fn drop(&mut self) {
            self.0.listing_busy.store(false, Ordering::Release);
        }
    }
    let _busy = Busy(manager);
    let never = AtomicBool::new(false);
    let mut stream = BoardStream::open(manager, router, Board::FlightComputer, &gate, &never).await?;
    let result = async {
        let Some(files) = list_dir(&mut stream, FC_IGT_DIR, "IGT", igt_dataset_name, &gate, &never).await? else {
            return Ok(None);
        };
        let mut dataset = Vec::with_capacity(files.len());
        for file in files {
            let req = TransferRequest::new(0, Board::FlightComputer, FC_IGT_DIR, &file.name, PathBuf::new(), xfer::RATE_UNIT);
            let answer = board_hash(&mut stream, &req, &gate, &never).await?;
            dataset.push(IgtDatasetFile {
                name: file.name,
                size: u64::from(answer.size),
                sha256: answer.sha256.iter().map(|b| format!("{b:02x}")).collect(),
            });
        }
        Ok(Some(dataset))
    }
    .await;
    stream.finish(result.is_err());
    result
}

// ------------------------------------------------------------ local files

pub(crate) fn download_root(state: &AppState) -> PathBuf {
    crate::recording_export::directory(state).join("sd-downloads")
}

fn local_paths(root: &FsPath, board: Board, dir: &str, name: &str) -> (PathBuf, PathBuf) {
    let folder = root.join(board.sender_id()).join(dir);
    (folder.join(name), folder.join(format!("{name}.part")))
}

/// A completed local copy; `None` if missing, partial or unsafe.
pub(crate) fn completed_file(root: &FsPath, board: &str, dir: &str, name: &str) -> Option<PathBuf> {
    let board = Board::from_sender_id(board).filter(|b| supports_sd_download(*b))?;
    let dir = directories(board)[dir_index(board, dir)? as usize];
    if !valid_name(name) {
        return None;
    }
    let (path, _) = local_paths(root, board, dir, name);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    (meta.is_file() && !meta.file_type().is_symlink()).then_some(path)
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalFile {
    pub board: String,
    pub dir: String,
    pub name: String,
    pub bytes: u64,
    pub complete: bool,
}

fn local_files(root: &FsPath) -> Vec<LocalFile> {
    let mut out = Vec::new();
    for board in Board::ALL
        .iter()
        .copied()
        .filter(|b| supports_sd_download(*b))
    {
        for dir in directories(board) {
            let Ok(entries) = std::fs::read_dir(root.join(board.sender_id()).join(dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let file = entry.file_name().to_string_lossy().into_owned();
                let (name, complete) = match file.strip_suffix(".part") {
                    Some(stem) => (stem.to_owned(), false),
                    None => (file, true),
                };
                if valid_name(&name) {
                    out.push(LocalFile {
                        board: board.sender_id().into(),
                        dir: (*dir).into(),
                        name,
                        bytes: meta.len(),
                        complete,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| (&a.dir, &a.name).cmp(&(&b.dir, &b.name)));
    out
}

fn disk_ok(path: &FsPath, needed: u64) -> Result<(), String> {
    let free =
        crate::recording_sd::free_disk(path).map_err(|e| format!("disk check failed: {e}"))?;
    if free.saturating_sub(crate::recording_sd::reserve_bytes()) < needed {
        return Err("not enough free disk for this SD download (reserve kept)".into());
    }
    Ok(())
}

// --------------------------------------------------------------- transfer

/// DATA message size on the AV-bay radio. SEDSnet carries a DATA message in
/// one P2P packet split into 192-byte radio frames; with the P2P stream and
/// packet headers a 460-byte message fills three frames (measured by
/// `wire_bytes_per_payload_byte_on_an_instant_link`), so no frame is sent
/// half empty and one lost frame costs at most 460 bytes.
pub(crate) const RADIO_MSG_BYTES: u16 = 460;
/// Wired links (bench, umbilical, CAN, DAQ fill link): the board clamps it
/// to what it serves (FC 512, DAQ 1024).
pub(crate) const WIRED_MSG_BYTES: u16 = 1024;
/// Raw block size the GS asks for (KiB).
pub(crate) const BLOCK_KIB: u8 = 16;

/// DATA message size, window and compression for one link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Profile {
    pub msg: u16,
    pub window: u8,
    /// Ask for zstd (only the RRLS outbox has it).
    pub zstd: bool,
}

impl Profile {
    /// Radio: 6 messages in flight. P2P stream messages are ordered and
    /// reliable in SEDSnet, so each unacknowledged DATA message (~520 B with
    /// its SEDSnet overhead) stays in the FC router's 8 KiB queue budget
    /// until acknowledged; 6 keep the transfer near 3 KB of it.
    pub(crate) const RADIO: Self = Self { msg: RADIO_MSG_BYTES, window: 6, zstd: false };
    /// FC on a wire: the FC answers at most 512-byte messages; 8 of them
    /// stay within the same queue budget.
    pub(crate) const FC_WIRED: Self = Self { msg: WIRED_MSG_BYTES, window: 8, zstd: false };
    /// DAQ (fill link): 1 KiB messages, 16 in flight.
    pub(crate) const WIRED: Self = Self { msg: WIRED_MSG_BYTES, window: 16, zstd: false };
}

/// A pacing rate that may change during the transfer (FC adaptive rate).
/// Called about every 500 ms with the gaps and bad blocks seen since the
/// last call; returns DATA bytes per second.
pub(crate) type RateFn = Arc<dyn Fn(u32) -> u64 + Send + Sync>;
/// Told when an open transfer pauses (`Some(reason)`) and resumes (`None`).
pub(crate) type PauseHook = Arc<dyn Fn(Option<&str>) + Send + Sync>;

pub(crate) struct TransferRequest {
    id: u64,
    board: Board,
    dir: u8,
    name: String,
    root: PathBuf,
    /// DATA bytes per second (whole messages, headers included).
    max_bps: u64,
    profile: Profile,
    /// Offer the finished file to the launch captures (operator and DAQ
    /// launch downloads). The FC collection attaches it itself.
    attach: bool,
    /// Blocks that failed their length or CRC-32 and were requested again.
    crc_errors: AtomicU64,
    /// Another SD file service than the board itself (host, P2P port).
    peer: Option<(&'static str, u16)>,
    /// Local folder under `<root>/<board>/` instead of the wire directory.
    local_dir: Option<&'static str>,
    rate_fn: Option<RateFn>,
    on_pause: Option<PauseHook>,
}

impl TransferRequest {
    fn new(id: u64, board: Board, dir: u8, name: &str, root: PathBuf, max_bps: u64) -> Self {
        Self {
            id,
            board,
            dir,
            name: name.into(),
            root,
            max_bps: max_bps.max(xfer::RATE_UNIT),
            profile: Profile::WIRED,
            attach: true,
            crc_errors: AtomicU64::new(0),
            peer: None,
            local_dir: None,
            rate_fn: None,
            on_pause: None,
        }
    }

    fn rate(&self, losses: u32) -> u64 {
        self.rate_fn.as_ref().map_or(self.max_bps, |f| f(losses)).max(xfer::RATE_UNIT)
    }
}

#[cfg(not(test))]
mod timing {
    use std::time::Duration;
    /// An OPEN or HASH whose answer did not come is sent again after this.
    pub const OPEN_RETRY: Duration = Duration::from_secs(3);
    pub const HASH_RETRY: Duration = Duration::from_secs(10);
    /// A board answering BUSY to a HASH is asked again this often (no CANCEL:
    /// it is usually still hashing the file, and a CANCEL would restart that).
    pub const HASH_BUSY_POLL: Duration = Duration::from_secs(1);
    /// BUSY for this long: CANCEL whatever the board still holds, then ask again.
    pub const HASH_BUSY_CANCEL: Duration = Duration::from_secs(60);
    /// No new DATA for this long while not paused: OPEN again from the
    /// committed offset (the board may have dropped the transfer).
    pub const STALL: Duration = Duration::from_secs(8);
    /// A pause holds the open transfer (rate 0, ACK every 5 s) this long; a
    /// longer one closes the stream and resumes later by a new OPEN.
    pub const PAUSE_HOLD: Duration = Duration::from_secs(15);
}
#[cfg(test)]
mod timing {
    use std::time::Duration;
    pub const OPEN_RETRY: Duration = Duration::from_millis(1500);
    pub const HASH_RETRY: Duration = Duration::from_secs(2);
    pub const HASH_BUSY_POLL: Duration = Duration::from_millis(200);
    pub const HASH_BUSY_CANCEL: Duration = Duration::from_secs(30);
    pub const STALL: Duration = Duration::from_secs(4);
    pub const PAUSE_HOLD: Duration = Duration::from_secs(3);
}
use timing::PAUSE_HOLD;
const KEEPALIVE: Duration = Duration::from_secs(5);
const ACK_EVERY: Duration = Duration::from_secs(1);
const ACK_GAP_EVERY: Duration = Duration::from_millis(500);
const RATE_CHECK: Duration = Duration::from_millis(500);
const HASH_TIMEOUT: Duration = Duration::from_secs(180);

/// Sends OPEN until the board answers it (retrying lost requests, waiting
/// while it is busy). `END` comes back as an answer, other refusals as errors.
async fn send_open(
    stream: &mut BoardStream,
    req: &TransferRequest,
    offset: u64,
    length: u64,
    rate: u64,
    gate: &Gate,
    cancel: &AtomicBool,
) -> Result<xfer::OpenAnswer, StreamError> {
    let offset32 = u32::try_from(offset).map_err(|_| "file offset exceeds protocol limit".to_string())?;
    let length32 = u32::try_from(length).map_err(|_| "range exceeds protocol limit".to_string())?;
    let mut attempts = 0u32;
    let mut busy = 0u32;
    loop {
        let open = xfer::Open {
            xfer: stream.next_xfer(),
            dir: req.dir,
            flags: xfer::OPEN_SDX | if req.profile.zstd { xfer::OPEN_ZSTD } else { 0 },
            offset: offset32,
            length: length32,
            rate: xfer::rate_units(rate),
            msg: req.profile.msg,
            window: req.profile.window,
            block_kib: BLOCK_KIB,
            name: req.name.clone(),
        };
        stream.send(&open.encode())?;
        let deadline = Instant::now() + timing::OPEN_RETRY;
        let answer = loop {
            match stream.next_frame(deadline, gate, cancel).await {
                Ok(Frame::Open(a)) if a.xfer == open.xfer || a.status != STATUS_OK => break Some(a),
                Ok(_) => {}
                Err(StreamError::Other(e)) if e.starts_with("timed out") => break None,
                Err(e) => return Err(e),
            }
        };
        let Some(answer) = answer else {
            attempts += 1;
            if attempts > MAX_RETRIES {
                return Err(StreamError::Other(format!("timed out waiting for {} to open {}", stream.board, req.name)));
            }
            continue;
        };
        match answer.status {
            STATUS_OK | STATUS_END => return Ok(answer),
            STATUS_BUSY => {
                busy += 1;
                if busy > 400 {
                    return Err(StreamError::Other("board stayed busy".into()));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            STATUS_BAD_STATE => return Err(StreamError::Gate(status_text(STATUS_BAD_STATE).into())),
            other => return Err(StreamError::Other(status_text(other).into())),
        }
    }
}

/// One streamed range read: OPEN, then verified raw bytes in file order.
/// Pauses (the gate) send rate 0 and hold the transfer for up to
/// [`PAUSE_HOLD`]; nothing is handed out while paused. Gaps are acknowledged
/// selectively, stalls and bad blocks re-OPEN from the committed offset.
pub(crate) struct Fetch<'a> {
    stream: &'a mut BoardStream,
    req: &'a TransferRequest,
    gate: &'a Gate,
    cancel: &'a AtomicBool,
    rx: xfer::Receiver,
    /// The file size at the latest OPEN.
    pub size: u64,
    rate: u64,
    window: u8,
    losses: u32,
    new_since_ack: u32,
    dup_since_ack: bool,
    last_ack: Instant,
    last_rate_check: Instant,
    last_fresh: Instant,
    stalls: u32,
    paused: Option<(Instant, String)>,
    failed: Option<StreamError>,
    final_acked: bool,
    /// The gate is polled every [`GATE_POLL`] (it may read files).
    gate_at: Option<Instant>,
    gate_reason: Option<String>,
}

impl<'a> Fetch<'a> {
    /// Opens `[offset, offset + length)` (`length` 0: to the end of the
    /// file). `None` when `offset` is the end of the file.
    pub(crate) async fn open(
        stream: &'a mut BoardStream,
        req: &'a TransferRequest,
        offset: u64,
        length: u64,
        gate: &'a Gate,
        cancel: &'a AtomicBool,
    ) -> Result<Option<Fetch<'a>>, StreamError> {
        let rate = req.rate(0);
        let answer = send_open(stream, req, offset, length, rate, gate, cancel).await?;
        if answer.status == STATUS_END {
            return Ok(None);
        }
        let start = u64::from(answer.offset);
        let now = Instant::now();
        Ok(Some(Self {
            rx: xfer::Receiver::new(answer.xfer, start, start + u64::from(answer.length)),
            stream,
            req,
            gate,
            cancel,
            size: u64::from(answer.size),
            rate,
            window: req.profile.window,
            losses: 0,
            new_since_ack: 0,
            dup_since_ack: false,
            last_ack: now,
            last_rate_check: now,
            last_fresh: now,
            stalls: 0,
            paused: None,
            failed: None,
            final_acked: false,
            gate_at: None,
            gate_reason: None,
        }))
    }

    fn send_ack(&mut self) -> Result<(), StreamError> {
        let rate = if self.paused.is_some() { 0 } else { xfer::rate_units(self.rate) };
        let ack = self.rx.ack(rate, self.window);
        self.stream.send(&ack.encode())?;
        self.last_ack = Instant::now();
        self.new_since_ack = 0;
        self.dup_since_ack = false;
        Ok(())
    }

    /// OPEN again at the committed offset (same range end); verified blocks
    /// already held are kept.
    async fn reopen(&mut self) -> Result<(), StreamError> {
        let at = self.rx.committed;
        let length = self.rx.range_end - at;
        let answer = send_open(self.stream, self.req, at, length, self.rate, self.gate, self.cancel).await?;
        let end = if answer.status == STATUS_END { at } else { u64::from(answer.offset) + u64::from(answer.length) };
        self.rx.restart(answer.xfer, end.min(self.rx.range_end));
        self.size = u64::from(answer.size);
        self.last_fresh = Instant::now();
        self.last_ack = Instant::now();
        Ok(())
    }

    /// The next verified bytes, in order; `None` at the end of the range.
    pub(crate) async fn next(&mut self) -> Result<Option<Vec<u8>>, StreamError> {
        loop {
            if self.failed.is_some() {
                // Hand out what is verified before reporting the stop.
                if let Some(bytes) = self.rx.take_ready() {
                    return Ok(Some(bytes));
                }
                return Err(self.failed.take().expect("failure"));
            }
            if self.cancel.load(Ordering::Acquire) {
                let _ = self.stream.send(&[OP_CANCEL]);
                self.failed = Some(StreamError::Cancelled);
                continue;
            }
            let now = Instant::now();
            if self.gate_at.is_none_or(|at| now.duration_since(at) >= GATE_POLL) {
                self.gate_at = Some(now);
                self.gate_reason = (self.gate)();
            }
            match (self.gate_reason.clone(), self.paused.as_ref()) {
                (Some(reason), None) => {
                    self.paused = Some((now, reason.clone()));
                    self.send_ack()?;
                    if let Some(hook) = &self.req.on_pause {
                        hook(Some(&reason));
                    }
                }
                (Some(reason), Some((since, _))) => {
                    if now.duration_since(*since) >= PAUSE_HOLD {
                        let _ = self.stream.send(&[OP_CANCEL]);
                        self.failed = Some(StreamError::Gate(reason));
                        continue;
                    }
                }
                (None, Some(_)) => {
                    self.paused = None;
                    self.rate = self.req.rate(0);
                    self.last_rate_check = now;
                    self.last_fresh = now;
                    self.send_ack()?;
                    if let Some(hook) = &self.req.on_pause {
                        hook(None);
                    }
                }
                (None, None) => {}
            }
            if self.paused.is_none() {
                if let Some(bytes) = self.rx.take_ready() {
                    return Ok(Some(bytes));
                }
                if self.rx.done() {
                    if !self.final_acked {
                        self.final_acked = true;
                        self.send_ack()?;
                    }
                    return Ok(None);
                }
                if now.duration_since(self.last_rate_check) >= RATE_CHECK {
                    self.last_rate_check = now;
                    let rate = self.req.rate(std::mem::take(&mut self.losses));
                    if xfer::rate_units(rate) != xfer::rate_units(self.rate) {
                        self.rate = rate;
                        self.send_ack()?;
                    }
                }
                if now.duration_since(self.last_fresh) >= timing::STALL {
                    self.stalls += 1;
                    if self.stalls > MAX_RETRIES {
                        return Err(StreamError::Other(format!("timed out waiting for {}", self.stream.board)));
                    }
                    self.reopen().await?;
                    continue;
                }
            }
            let since_ack = now.duration_since(self.last_ack);
            let due = since_ack >= KEEPALIVE
                || (self.paused.is_none()
                    && (u64::from(self.new_since_ack) >= u64::from(self.window / 2).max(1)
                        || (self.rx.has_gap() && since_ack >= ACK_GAP_EVERY)
                        || (self.new_since_ack > 0 && since_ack >= ACK_EVERY)
                        || (self.dup_since_ack && since_ack >= Duration::from_millis(200))));
            if due {
                self.send_ack()?;
            }
            let event = match self.stream.recv(Duration::from_millis(25)).await? {
                Some(event) => event,
                None => continue,
            };
            let Some(data) = xfer::Data::decode(&event) else { continue };
            let arrival = self.rx.on_data(&data);
            if arrival.fresh {
                self.new_since_ack += 1;
                self.last_fresh = Instant::now();
                self.stalls = 0;
            }
            if arrival.duplicate {
                self.dup_since_ack = true;
            }
            if arrival.gap {
                self.losses += 1;
            }
            if let Some(offset) = arrival.bad_block {
                self.req.crc_errors.fetch_add(1, Ordering::Relaxed);
                self.losses += 1;
                log::warn!(
                    "{} SD block at {offset} of {} failed its length or CRC-32; requesting it again",
                    self.req.board.sender_id(),
                    self.req.name
                );
                self.reopen().await?;
            }
        }
    }

    /// Ends the transfer on the board (CANCEL; harmless if it already ended).
    pub(crate) fn close(self) {
        let _ = self.stream.send(&[OP_CANCEL]);
    }
}

/// Reads `[offset, offset + length)` whole (tails, probes, small files).
/// Returns the bytes (shorter at the end of the file) and the file size.
pub(crate) async fn read_range(
    stream: &mut BoardStream,
    req: &TransferRequest,
    offset: u64,
    length: u64,
    gate: &Gate,
) -> Result<(Vec<u8>, u64), StreamError> {
    let never = AtomicBool::new(false);
    let Some(mut fetch) = Fetch::open(stream, req, offset, length, gate, &never).await? else {
        return Ok((Vec::new(), offset));
    };
    let mut out = Vec::new();
    while let Some(bytes) = fetch.next().await? {
        out.extend_from_slice(&bytes);
    }
    let size = fetch.size;
    fetch.close();
    Ok((out, size))
}

/// The board's size and SHA-256 of the file as it is now.
///
/// A board hashes a large file over many service turns (the DAQ: 4 KB a turn,
/// tens of seconds for an 18 MB launch file in the r7 simulation) and answers
/// BUSY to requests meanwhile. Only the first request is preceded by a CANCEL
/// (a transfer the board still holds would make it BUSY); a retry or a BUSY
/// answer asks again without one, since a CANCEL abandons the hash in progress
/// and the board starts over: with a CANCEL on every retry a file that takes
/// longer than HASH_RETRY to hash never verified, and DAQ-Board's abandoned
/// hash also left its request slot BUSY ("SHA-256 refused: board busy").
async fn board_hash(stream: &mut BoardStream, req: &TransferRequest, gate: &Gate, cancel: &AtomicBool) -> Result<xfer::HashAnswer, StreamError> {
    let deadline = Instant::now() + HASH_TIMEOUT;
    let mut cancel_first = true;
    let mut busy_since: Option<Instant> = None;
    loop {
        if std::mem::take(&mut cancel_first) {
            stream.send(&[OP_CANCEL])?;
        }
        // A HASH that overtook the CANCEL would have its hash abandoned by it.
        stream.settle_cancels((Instant::now() + timing::OPEN_RETRY).min(deadline), gate, cancel).await?;
        stream.send(&xfer::encode_hash(req.dir, &req.name))?;
        let retry = (Instant::now() + timing::HASH_RETRY).min(deadline);
        let answer = loop {
            match stream.next_frame(retry, gate, cancel).await {
                Ok(Frame::Hash(answer)) => break Some(answer),
                Ok(_) => {}
                Err(StreamError::Other(e)) if e.starts_with("timed out") => break None,
                Err(e) => return Err(e),
            }
        };
        match answer {
            Some(a) if a.status == STATUS_OK => return Ok(a),
            Some(a) if a.status == STATUS_BUSY && Instant::now() < deadline => {
                let since = *busy_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= timing::HASH_BUSY_CANCEL {
                    cancel_first = true;
                    busy_since = None;
                }
                tokio::time::sleep(timing::HASH_BUSY_POLL).await;
            }
            Some(a) if a.status == STATUS_BAD_STATE => return Err(StreamError::Gate(status_text(a.status).into())),
            Some(a) => return Err(StreamError::Other(format!("SHA-256 of {} refused: {}", req.name, status_text(a.status)))),
            None if Instant::now() >= deadline => return Err(StreamError::Other(format!("timed out waiting for the SHA-256 of {}", req.name))),
            None => {}
        }
    }
}

fn sha256_file(path: &FsPath) -> std::io::Result<[u8; 32]> {
    use std::io::Read;
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut file = std::fs::File::open(path)?;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        context.update(&buffer[..n]);
    }
    Ok(context.finish().as_ref().try_into().expect("32 bytes"))
}

/// Bytes of the local copy compared with the card when resuming.
const RESUME_CHECK_BYTES: u64 = 512;

async fn transfer(
    manager: &Manager,
    router: Arc<Router>,
    req: &TransferRequest,
    gate: &Gate,
    cancel: &AtomicBool,
) -> Result<(), StreamError> {
    let dir_name = match req.local_dir {
        Some(dir) => dir,
        None => directories(req.board)
            .get(req.dir as usize)
            .copied()
            .ok_or_else(|| "unknown SD folder".to_string())?,
    };
    let (final_path, part_path) = local_paths(&req.root, req.board, dir_name, &req.name);
    tokio::fs::create_dir_all(part_path.parent().expect("download folder"))
        .await
        .map_err(|e| format!("cannot create download folder: {e}"))?;
    // A finished copy of a file that was still being written is refreshed by
    // resuming after its last byte (the tail check below still applies).
    if tokio::fs::metadata(&part_path).await.is_err() && tokio::fs::metadata(&final_path).await.is_ok() {
        tokio::fs::rename(&final_path, &part_path)
            .await
            .map_err(|e| format!("cannot reopen finished copy: {e}"))?;
    }
    let mut have = match tokio::fs::metadata(&part_path).await {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    };
    manager.update(req.id, |s| {
        s.phase = Phase::Connecting;
        s.message = format!("connecting to {}", req.peer.map_or(req.board.sender_id(), |p| p.0));
    });
    let (host, service_port) = req.peer.unwrap_or((req.board.sender_id(), SD_FILE_STREAM_PORT));
    let mut stream = BoardStream::open_peer(manager, router, host, service_port, gate, cancel).await?;

    // Resume check: our last bytes must still match the card, else the card
    // was replaced or rewritten and we restart from zero.
    if have > 0 {
        let check = have.saturating_sub(RESUME_CHECK_BYTES);
        let (remote, _) = read_range(&mut stream, req, check, have - check, gate).await?;
        let mut local = vec![0_u8; (have - check) as usize];
        {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let mut file = tokio::fs::File::open(&part_path)
                .await
                .map_err(|e| format!("cannot reopen partial file: {e}"))?;
            file.seek(std::io::SeekFrom::Start(check)).await.map_err(|e| e.to_string())?;
            file.read_exact(&mut local).await.map_err(|e| e.to_string())?;
        }
        if remote != local {
            have = 0;
            let _ = tokio::fs::remove_file(&part_path).await;
        }
    }
    let resumed_from = have;
    manager.update(req.id, |s| {
        s.phase = Phase::Transferring;
        s.resumed_from = resumed_from;
        s.bytes_done = resumed_from;
        s.message = if resumed_from > 0 { format!("resuming at {resumed_from} bytes") } else { "transferring".into() };
    });
    let started = Instant::now();
    let mut received = 0_u64;
    let mut last_disk_check = have;
    let mut hash_mismatches = 0;
    loop {
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part_path)
            .await
            .map_err(|e| format!("cannot open partial file: {e}"))?;
        let mut fetch = Fetch::open(&mut stream, req, have, 0, gate, cancel).await?;
        if let Some(fetch) = fetch.as_mut() {
            let total = fetch.size;
            loop {
                let next = fetch.next().await;
                let bytes = match next {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => break,
                    Err(error) => {
                        file.flush().await.map_err(|e| e.to_string())?;
                        return Err(error);
                    }
                };
                file.write_all(&bytes).await.map_err(|e| format!("cannot write download: {e}"))?;
                have += bytes.len() as u64;
                received += bytes.len() as u64;
                let elapsed = started.elapsed().as_secs_f64().max(0.001);
                let crc_errors = req.crc_errors.load(Ordering::Relaxed);
                manager.update(req.id, |s| {
                    s.bytes_done = have;
                    s.total_bytes = total.max(have);
                    s.bytes_per_second = (received as f64 / elapsed) as u64;
                    s.crc_errors = crc_errors;
                    if s.message.starts_with("paused") || s.message.starts_with("resuming") {
                        s.message = "transferring".into();
                    }
                });
                if have - last_disk_check >= 4 * 1024 * 1024 {
                    last_disk_check = have;
                    disk_ok(&req.root, 64 * 1024 * 1024)?;
                }
            }
        }
        if let Some(fetch) = fetch {
            fetch.close();
        }
        file.flush().await.map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
        drop(file);
        // End to end: the board's SHA-256 of the file against our copy's.
        let hash = board_hash(&mut stream, req, gate, cancel).await?;
        let size = u64::from(hash.size);
        if size > have {
            // Still being written: fetch what was added.
            continue;
        }
        let path = part_path.clone();
        let local = tokio::task::spawn_blocking(move || sha256_file(&path))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        if size == have && local == hash.sha256 {
            break;
        }
        hash_mismatches += 1;
        log::warn!("{} SD {}: SHA-256 differs from the card ({have} of {size} bytes); fetching it again", req.board.sender_id(), req.name);
        let _ = tokio::fs::remove_file(&part_path).await;
        have = 0;
        if hash_mismatches >= 2 {
            return Err(StreamError::Other(format!("{}: the copy's SHA-256 differs from the card's", req.name)));
        }
    }
    stream.finish(false);
    tokio::fs::rename(&part_path, &final_path)
        .await
        .map_err(|e| format!("cannot finalize download: {e}"))?;
    manager.update(req.id, |s| {
        s.phase = Phase::Complete;
        s.bytes_done = have;
        s.total_bytes = have;
        s.message = "downloaded and SHA-256 verified; ready to import".into();
    });
    // A board launch file follows its GS launch capture; an ordinary file can
    // supply the lead-up of one already attached.
    if req.attach
        && let Some(data_root) = req.root.parent().map(FsPath::to_path_buf)
    {
        let name = req.name.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(error) = crate::recording_store::board_sd::offer_file(&data_root, &final_path, &name, "network_download") {
                log::warn!("board SD {name}: could not attach to its launch capture: {error}");
            }
        })
        .await;
    }
    Ok(())
}

static LAUNCH_FETCH: AtomicBool = AtomicBool::new(false);
/// The launch fetch is running (waiting for a safe state or transferring).
static LAUNCH_FETCH_RUNNING: AtomicBool = AtomicBool::new(false);
/// A launch capture was saved: fetch the DAQ's launch files once allowed.
pub(crate) fn schedule_launch_fetch() {
    LAUNCH_FETCH.store(true, Ordering::Release);
}
/// A DAQ launch-file fetch is scheduled or running: the low-priority lead-up
/// fetch yields the link to it.
pub(crate) fn launch_fetch_owed() -> bool {
    LAUNCH_FETCH.load(Ordering::Acquire) || LAUNCH_FETCH_RUNNING.load(Ordering::Acquire)
}
pub(crate) fn take_launch_fetch() -> bool {
    LAUNCH_FETCH.swap(false, Ordering::AcqRel)
}

/// Downloads the DAQ's `/LAUNCH` files the station does not have yet, one at
/// a time, as soon as downloads are allowed (Idle, Landed or Recovery, no
/// sequence, no other transfer). Each completed file attaches itself to its
/// launch capture (see `transfer`). Gives up after an hour of waiting.
/// Wait between DAQ launch-file listing attempts.
const LAUNCH_LIST_RETRY: Duration = Duration::from_secs(15);
/// DAQ launch-file listing attempts before the fetch gives up.
const LAUNCH_LIST_ATTEMPTS: u32 = 4;

pub(crate) async fn fetch_launch_files(state: Arc<AppState>) {
    struct Running;
    impl Drop for Running {
        fn drop(&mut self) {
            LAUNCH_FETCH_RUNNING.store(false, Ordering::Release);
        }
    }
    LAUNCH_FETCH_RUNNING.store(true, Ordering::Release);
    let _running = Running;
    let deadline = Instant::now() + Duration::from_secs(3600);
    loop {
        if station_gate(&state).is_none() && !MANAGER.active() {
            break;
        }
        if Instant::now() >= deadline {
            log::warn!("DAQ launch files were not fetched: downloads stayed unavailable for an hour");
            return;
        }
        // Start as soon as the station is safe (the burn report waits on it).
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let Some(router) = state.topology_router.get().cloned() else { return };
    // A listing that times out (the DAQ busy closing its launch file, a lost answer) is retried a few times: the
    // burn report's final version depends on it. Not indefinitely: the FC SD collection waits behind this fetch,
    // and a DAQ that refuses its SD stream (its flight state not Idle, Landed or Recovery) never answers.
    let mut attempts = 0;
    let listing = loop {
        attempts += 1;
        match refresh_listing(&MANAGER, router.clone(), Board::DaqBoard, app_gate(&state)).await {
            Ok(listing) => break listing,
            Err(error) if attempts < LAUNCH_LIST_ATTEMPTS && Instant::now() + LAUNCH_LIST_RETRY < deadline => {
                log::warn!("DAQ launch files: listing failed: {error}; retrying in {} s", LAUNCH_LIST_RETRY.as_secs());
                tokio::time::sleep(LAUNCH_LIST_RETRY).await;
                while station_gate(&state).is_some() || MANAGER.active() {
                    if Instant::now() >= deadline {
                        log::warn!("DAQ launch files were not fetched: downloads stayed unavailable for an hour");
                        return;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            Err(error) => {
                log::warn!("DAQ launch files: listing failed: {error}");
                return;
            }
        }
    };
    let root = download_root(&state);
    if std::fs::create_dir_all(&root).is_err() {
        return;
    }
    for file in listing.files.iter().filter(|f| f.dir == "LAUNCH" && !f.open) {
        let local = completed_file(&root, "DAQ", "LAUNCH", &file.name);
        if local.as_ref().is_some_and(|p| std::fs::metadata(p).is_ok_and(|m| m.len() >= file.size)) {
            continue;
        }
        if disk_ok(&root, file.size.saturating_add(64 * 1024 * 1024)).is_err() {
            log::warn!("DAQ launch files: not enough free disk for {}", file.name);
            return;
        }
        let job = match start_job(&MANAGER, router.clone(), Board::DaqBoard, "LAUNCH", &file.name, root.clone(), app_gate(&state)) {
            Ok(job) => job,
            Err(error) => {
                log::warn!("DAQ launch file {}: {error}", file.name);
                return;
            }
        };
        log::info!("fetching DAQ launch file {} for its launch capture", file.name);
        while MANAGER.get(job.id).is_some_and(|s| s.phase.is_active()) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn run_job(
    manager: &'static Manager,
    router: Arc<Router>,
    req: TransferRequest,
    gate: Gate,
    cancel: Arc<AtomicBool>,
) {
    let result = transfer(manager, router, &req, &gate, &cancel).await;
    if let Err(error) = result {
        let (phase, message) = match error {
            StreamError::Cancelled => (
                Phase::Cancelled,
                "cancelled; partial data kept for resume".to_string(),
            ),
            StreamError::Gate(reason) => (
                Phase::Paused,
                format!("paused: {reason}; resume when allowed"),
            ),
            StreamError::Other(e) => (Phase::Failed, format!("{e}; partial data kept for resume")),
        };
        manager.update(req.id, |s| {
            s.phase = phase;
            s.message = message;
        });
    }
}

fn start_job(
    manager: &'static Manager,
    router: Arc<Router>,
    board: Board,
    dir: &str,
    name: &str,
    root: PathBuf,
    gate: Gate,
) -> Result<JobStatus, String> {
    start_job_at(manager, router, board, dir, name, root, gate, max_bytes_per_second())
}

#[expect(clippy::too_many_arguments, reason = "one job's owned transfer context")]
fn start_job_at(
    manager: &'static Manager,
    router: Arc<Router>,
    board: Board,
    dir: &str,
    name: &str,
    root: PathBuf,
    gate: Gate,
    max_bps: u64,
) -> Result<JobStatus, String> {
    if !supports_sd_download(board) {
        return Err(format!("{} does not serve SD downloads", board.as_str()));
    }
    let dir_i = dir_index(board, dir).ok_or("unknown SD folder")?;
    if !valid_name(name) {
        return Err("invalid SD file name".into());
    }
    if let Some(reason) = gate() {
        return Err(reason);
    }
    let (id, cancel) = manager.create_job(board, directories(board)[dir_i as usize], name)?;
    let req = TransferRequest::new(id, board, dir_i, name, root, max_bps);
    tokio::spawn(run_job(manager, router, req, gate, cancel));
    Ok(manager.get(id).expect("new SD download job"))
}

async fn refresh_listing(
    manager: &'static Manager,
    router: Arc<Router>,
    board: Board,
    gate: Gate,
) -> Result<Listing, String> {
    if !supports_sd_download(board) {
        return Err(format!("{} does not serve SD downloads", board.as_str()));
    }
    if let Some(reason) = gate() {
        return Err(reason);
    }
    if manager.active() {
        return Err("an SD download or listing is already active".into());
    }
    if manager
        .listing_busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err("SD file listing is already active".into());
    }
    struct Busy(&'static Manager);
    impl Drop for Busy {
        fn drop(&mut self) {
            self.0.listing_busy.store(false, Ordering::Release);
        }
    }
    let _busy = Busy(manager);
    let never = AtomicBool::new(false);
    let result = async {
        let mut stream = BoardStream::open(manager, router, board, &gate, &never).await?;
        let files = list_on_stream(&mut stream, board, &gate, &never).await;
        stream.finish(files.is_err());
        files
    }
    .await
    .map_err(|e| match e {
        StreamError::Gate(reason) => reason,
        StreamError::Cancelled => "cancelled".into(),
        StreamError::Other(e) => e,
    })?;
    let listing = Listing {
        board: board.sender_id().into(),
        listed_at_ms: now_ms(),
        files: result,
    };
    manager
        .listings
        .lock()
        .expect("sd listings")
        .insert(listing.board.clone(), listing.clone());
    Ok(listing)
}

// ------------------------------------------------------------------- HTTP

#[derive(Deserialize)]
pub struct StartRequest {
    dir: String,
    name: String,
}

fn parse_board(raw: &str) -> Option<Board> {
    crate::firmware_update::parse_board_target(raw).filter(|b| supports_sd_download(*b))
}

#[expect(
    clippy::result_large_err,
    reason = "Axum handlers return the rejection Response directly"
)]
async fn command_principal(state: &Arc<AppState>, headers: &HeaderMap) -> Result<(), Response> {
    let principal = crate::web::authorize_headers(state, headers, Permission::SendCommands).await?;
    if !principal.permissions.send_commands || !principal.allows_command_name("SdDownload") {
        return Err((
            StatusCode::FORBIDDEN,
            "SD download requires command permission",
        )
            .into_response());
    }
    if state.shutdown_requested.load(Ordering::Acquire) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "ground station is shutting down",
        )
            .into_response());
    }
    Ok(())
}

fn app_gate(state: &Arc<AppState>) -> Gate {
    let weak = Arc::downgrade(state);
    Arc::new(move || match weak.upgrade() {
        Some(state) if state.shutdown_requested.load(Ordering::Acquire) => {
            Some("ground station is shutting down".into())
        }
        Some(state) => station_gate(&state),
        None => Some("ground station stopped".into()),
    })
}

/// File listings, jobs and local copies for the SD download panel.
pub async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    let root = download_root(&state);
    let listings = MANAGER
        .listings
        .lock()
        .expect("sd listings")
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let blocked = station_gate(&state);
    Json(serde_json::json!({
        "boards": Board::ALL.iter().filter(|b| supports_sd_download(**b)).map(|b| b.sender_id()).collect::<Vec<_>>(),
        "allowed": blocked.is_none(),
        "blocked_reason": blocked,
        "max_bytes_per_second": max_bytes_per_second(),
        "listings": listings,
        "jobs": MANAGER.list(),
        "local": local_files(&root),
        "fc_collect": fc::status(&state),
    }))
    .into_response()
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(board): Path<String>,
) -> Response {
    if let Err(e) = command_principal(&state, &headers).await {
        return e;
    }
    let Some(board) = parse_board(&board) else {
        return (StatusCode::BAD_REQUEST, "board does not serve SD downloads").into_response();
    };
    if let Some(reason) = station_gate(&state) {
        return (StatusCode::CONFLICT, reason).into_response();
    }
    let Some(router) = state.topology_router.get().cloned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "router unavailable").into_response();
    };
    match refresh_listing(&MANAGER, router, board, app_gate(&state)).await {
        Ok(listing) => Json(listing).into_response(),
        Err(e) if e.contains("already active") => (StatusCode::CONFLICT, e).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e).into_response(),
    }
}

pub async fn start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(board): Path<String>,
    Json(body): Json<StartRequest>,
) -> Response {
    if let Err(e) = command_principal(&state, &headers).await {
        return e;
    }
    let Some(board) = parse_board(&board) else {
        return (StatusCode::BAD_REQUEST, "board does not serve SD downloads").into_response();
    };
    if let Some(reason) = station_gate(&state) {
        return (StatusCode::CONFLICT, reason).into_response();
    }
    let Some(router) = state.topology_router.get().cloned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "router unavailable").into_response();
    };
    let root = download_root(&state);
    if let Err(e) = std::fs::create_dir_all(&root)
        .map_err(|e| e.to_string())
        .and_then(|_| disk_ok(&root, 64 * 1024 * 1024))
    {
        return (StatusCode::INSUFFICIENT_STORAGE, e).into_response();
    }
    match start_job_at(
        &MANAGER,
        router,
        board,
        &body.dir,
        &body.name,
        root,
        app_gate(&state),
        max_bytes_per_second_for(&state, board),
    ) {
        Ok(job) => (StatusCode::ACCEPTED, Json(job)).into_response(),
        Err(e) if e.contains("already active") => (StatusCode::CONFLICT, e).into_response(),
        Err(e) if e.starts_with("SD downloads") => (StatusCode::CONFLICT, e).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

pub async fn cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    if let Err(e) = command_principal(&state, &headers).await {
        return e;
    }
    match MANAGER.cancel(id) {
        Ok(job) => Json(job).into_response(),
        Err(e) if e.contains("not found") => (StatusCode::NOT_FOUND, e).into_response(),
        Err(e) => (StatusCode::CONFLICT, e).into_response(),
    }
}

#[derive(Deserialize)]
pub struct ImportSource {
    board: String,
    dir: String,
    name: String,
}

/// Feeds a completed download into the unchanged SD import (same admin rule,
/// limits, staging, worker and merge) instead of a browser file upload.
pub async fn import_downloaded(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, token)): Path<(String, String)>,
    Json(source): Json<ImportSource>,
) -> Response {
    if let Err(e) = crate::recording_sd::admin(&state, &headers).await {
        return e;
    }
    let root = download_root(&state);
    let Some(path) = completed_file(&root, &source.board, &source.dir, &source.name) else {
        return (
            StatusCode::NOT_FOUND,
            "downloaded SD file not found or incomplete",
        )
            .into_response();
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let body = futures::stream::try_unfold(file, |mut file| async move {
        use tokio::io::AsyncReadExt;
        let mut buffer = vec![0_u8; 64 * 1024];
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        buffer.truncate(n);
        Ok(Some((axum::body::Bytes::from(buffer), file)))
    });
    crate::recording_sd::upload(
        State(state),
        headers,
        Path((id, token)),
        axum::body::Body::from_stream(body),
    )
    .await
}

pub fn routes() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/sd_download", get(status))
        .route("/api/sd_download/{board}/list", post(list))
        .route("/api/sd_download/{board}/start", post(start))
        .route("/api/sd_download/jobs/{id}/cancel", post(cancel))
        .route("/api/sd_download/fc", get(fc::status_route))
        .route("/api/sd_download/fc/stop", post(fc::stop_route))
        .route("/api/sd_download/fc/resume", post(fc::resume_route))
        .route(
            "/api/recordings/{id}/sd/imports/{token}/download",
            post(import_downloaded),
        )
}

#[path = "sd_download/leadup.rs"]
pub(crate) mod leadup;

#[path = "sd_download/fc.rs"]
pub(crate) mod fc;

#[path = "sd_download/relay.rs"]
pub(crate) mod relay;

#[path = "sd_download/sdx.rs"]
pub(crate) mod sdx;

#[path = "sd_download/xfer.rs"]
pub(crate) mod xfer;

#[cfg(test)]
#[path = "sd_download/test_data.rs"]
pub(crate) mod test_data;

#[cfg(test)]
mod tests;
