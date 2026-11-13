//! Network download of board SD files into the offline SD recovery pipeline.
//!
//! Boards serve their SD card on SEDSnet P2P stream port 4511 (protocol v1, see
//! DAQ-Board `Core/Inc/sd_file_wire.h`). The station asks for one 512-byte block
//! at a time and appends it to a `.part` file, so a transfer can be paused,
//! cancelled and resumed at any byte. Transfers are bandwidth-bounded here and
//! paced again on the board, run one at a time, and are only admitted (and only
//! continue) in Idle, Landed or Recovery with no fill/launch sequence active.
//! Completed files are handed to `recording_sd::upload` unchanged, so the
//! existing import, provenance and "SD rows win" merge rules apply.
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
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

pub const SD_FILE_STREAM_PORT: u16 = 4511;
const OP_LIST: u8 = 0x21;
const OP_READ: u8 = 0x22;
const OP_CANCEL: u8 = 0x23;
const RESPONSE_FLAG: u8 = 0x80;
const STATUS_OK: u8 = 0;
const STATUS_BAD_STATE: u8 = 2;
const STATUS_BUSY: u8 = 3;
const STATUS_NOT_FOUND: u8 = 4;
const STATUS_END: u8 = 6;
const DATA_HEADER: usize = 11;
const LIST_HEADER: usize = 12;
const FLAG_LAST: u8 = 0x01;
const ENTRY_OPEN: u8 = 0x01;
const NAME_MAX: usize = 63;
pub const BLOCK_BYTES: u64 = 512;
pub const DIRECTORIES: [&str; 3] = ["LAUNCH", "UTC", "NO_UTC"];
const MAX_ENTRIES_PER_DIR: u16 = 4096;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const GATE_POLL: Duration = Duration::from_millis(100);
const MAX_RETRIES: u32 = 6;
const SOURCE_PORT_FIRST: u16 = 60_000;
const SOURCE_PORT_LAST: u16 = 64_999;

static MANAGER: LazyLock<Manager> = LazyLock::new(Manager::default);

/// Boards whose current firmware serves the SD download stream.
pub fn supports_sd_download(board: Board) -> bool {
    matches!(board, Board::DaqBoard)
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

type Gate = Arc<dyn Fn() -> Option<String> + Send + Sync>;

fn max_bytes_per_second() -> u64 {
    std::env::var("GS_SD_DOWNLOAD_MAX_BYTES_PER_SEC")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (512..=262_144).contains(value))
        .unwrap_or(16 * 1024)
}

// ---------------------------------------------------------------- wire codec

fn encode_list(dir: u8, index: u16) -> [u8; 4] {
    let i = index.to_le_bytes();
    [OP_LIST, dir, i[0], i[1]]
}

fn encode_read(offset: u32, dir: u8, name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(7 + name.len());
    out.push(OP_READ);
    out.extend_from_slice(&offset.to_le_bytes());
    out.push(dir);
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    out
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
struct DataFrame {
    status: u8,
    flags: u8,
    offset: u32,
    file_size: u32,
    data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Frame {
    List(ListFrame),
    Data(DataFrame),
    Cancel(u8),
}

fn u32_at(p: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(p[at..at + 4].try_into().expect("four bytes"))
}

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
        b if b == OP_READ | RESPONSE_FLAG => {
            if p.len() < DATA_HEADER {
                return None;
            }
            Some(Frame::Data(DataFrame {
                status: p[1],
                flags: p[2],
                offset: u32_at(p, 3),
                file_size: u32_at(p, 7),
                data: p[DATA_HEADER..].to_vec(),
            }))
        }
        b if b == OP_CANCEL | RESPONSE_FLAG && p.len() == 2 => Some(Frame::Cancel(p[1])),
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

fn dir_index(dir: &str) -> Option<u8> {
    DIRECTORIES
        .iter()
        .position(|d| d.eq_ignore_ascii_case(dir))
        .map(|i| i as u8)
}

fn status_text(status: u8) -> &'static str {
    match status {
        0 => "ok",
        1 => "bad message",
        2 => "board refused: not in Idle/Landed/Recovery or launch logging active",
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
struct Manager {
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

struct OwnedEvent {
    kind: P2pStreamEventKind,
    stream_id: P2pStreamId,
    peer: String,
    payload: Vec<u8>,
}

#[derive(Debug)]
enum StreamError {
    Gate(String),
    Cancelled,
    Other(String),
}

impl From<String> for StreamError {
    fn from(value: String) -> Self {
        Self::Other(value)
    }
}

struct BoardStream {
    router: Arc<Router>,
    board: &'static str,
    port: u16,
    stream: P2pStreamId,
    rx: mpsc::UnboundedReceiver<OwnedEvent>,
    open: bool,
}

impl BoardStream {
    async fn open(
        manager: &Manager,
        router: Arc<Router>,
        board: Board,
        gate: &Gate,
        cancel: &AtomicBool,
    ) -> Result<Self, StreamError> {
        let host = board.sender_id();
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
        let stream = match router.open_p2p_stream_to_hostname(host, SD_FILE_STREAM_PORT, port) {
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

    fn send(&self, payload: &[u8]) -> Result<(), StreamError> {
        self.router
            .send_p2p_stream(self.stream, payload)
            .map_err(|err| {
                StreamError::Other(format!("SD stream send to {} failed: {err}", self.board))
            })
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
    gate: &Gate,
    cancel: &AtomicBool,
) -> Result<Vec<RemoteFile>, StreamError> {
    let mut files = Vec::new();
    for (dir, dir_name) in DIRECTORIES.iter().enumerate() {
        let mut index = 0_u16;
        let mut attempts = 0_u32;
        while index < MAX_ENTRIES_PER_DIR {
            stream.send(&encode_list(dir as u8, index))?;
            let deadline = Instant::now() + RESPONSE_TIMEOUT;
            let frame = loop {
                match stream.next_frame(deadline, gate, cancel).await {
                    Ok(Frame::List(frame))
                        if frame.status != STATUS_OK
                            || (frame.dir == dir as u8 && frame.index == index) =>
                    {
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
                    if valid_name(&frame.name) {
                        files.push(RemoteFile {
                            dir: (*dir_name).into(),
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
                other => return Err(StreamError::Other(status_text(other).into())),
            }
        }
    }
    Ok(files)
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
    let dir = DIRECTORIES[dir_index(dir)? as usize];
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
        for dir in DIRECTORIES {
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
                        dir: dir.into(),
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

struct TransferRequest {
    id: u64,
    board: Board,
    dir: u8,
    name: String,
    root: PathBuf,
    max_bps: u64,
}

/// Reads one block answer starting at `offset`. Returns (bytes, file_size, end).
async fn read_block(
    stream: &mut BoardStream,
    req: &TransferRequest,
    offset: u64,
    gate: &Gate,
    cancel: &AtomicBool,
) -> Result<(Vec<u8>, u64, bool), StreamError> {
    let offset32 =
        u32::try_from(offset).map_err(|_| "file offset exceeds protocol limit".to_string())?;
    let mut attempts = 0_u32;
    'retry: loop {
        stream.send(&encode_read(offset32, req.dir, &req.name))?;
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        let mut data = Vec::with_capacity(BLOCK_BYTES as usize);
        loop {
            let frame = match stream.next_frame(deadline, gate, cancel).await {
                Ok(Frame::Data(frame)) => frame,
                Ok(_) => continue,
                Err(StreamError::Other(e)) if e.starts_with("timed out") => {
                    attempts += 1;
                    if attempts > MAX_RETRIES {
                        return Err(StreamError::Other(e));
                    }
                    continue 'retry;
                }
                Err(e) => return Err(e),
            };
            match frame.status {
                STATUS_OK => {
                    // Accept only the contiguous continuation; stale frames
                    // from an earlier retried request are ignored.
                    if u64::from(frame.offset) != offset + data.len() as u64 {
                        continue;
                    }
                    data.extend_from_slice(&frame.data);
                    if frame.flags & FLAG_LAST != 0 || data.len() as u64 >= BLOCK_BYTES {
                        return Ok((data, u64::from(frame.file_size), false));
                    }
                }
                STATUS_END => return Ok((data, u64::from(frame.file_size), true)),
                STATUS_BUSY => {
                    attempts += 1;
                    if attempts > 400 {
                        return Err(StreamError::Other("board stayed busy".into()));
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue 'retry;
                }
                STATUS_BAD_STATE => {
                    return Err(StreamError::Gate(status_text(STATUS_BAD_STATE).into()));
                }
                STATUS_NOT_FOUND => {
                    return Err(StreamError::Other(status_text(STATUS_NOT_FOUND).into()));
                }
                other => {
                    attempts += 1;
                    if attempts > MAX_RETRIES {
                        return Err(StreamError::Other(status_text(other).into()));
                    }
                    continue 'retry;
                }
            }
        }
    }
}

async fn transfer(
    manager: &Manager,
    router: Arc<Router>,
    req: &TransferRequest,
    gate: &Gate,
    cancel: &AtomicBool,
) -> Result<(), StreamError> {
    let dir_name = DIRECTORIES[req.dir as usize];
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
        s.message = format!("connecting to {}", req.board.sender_id());
    });
    let mut stream = BoardStream::open(manager, router, req.board, gate, cancel).await?;

    // Resume check: the block holding our last byte must still match, else
    // the card was replaced or rewritten and we restart from zero.
    if have > 0 {
        let check = (have - 1) / BLOCK_BYTES * BLOCK_BYTES;
        let (remote, _, _) = read_block(&mut stream, req, check, gate, cancel).await?;
        let local_len = (have - check) as usize;
        let mut local = vec![0_u8; local_len];
        {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let mut file = tokio::fs::File::open(&part_path)
                .await
                .map_err(|e| format!("cannot reopen partial file: {e}"))?;
            file.seek(std::io::SeekFrom::Start(check))
                .await
                .map_err(|e| e.to_string())?;
            file.read_exact(&mut local)
                .await
                .map_err(|e| e.to_string())?;
        }
        if remote.len() < local_len || remote[..local_len] != local[..] {
            have = 0;
            let _ = tokio::fs::remove_file(&part_path).await;
        }
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part_path)
        .await
        .map_err(|e| format!("cannot open partial file: {e}"))?;
    let resumed_from = have;
    manager.update(req.id, |s| {
        s.phase = Phase::Transferring;
        s.resumed_from = resumed_from;
        s.bytes_done = resumed_from;
        s.message = if resumed_from > 0 {
            format!("resuming at {resumed_from} bytes")
        } else {
            "transferring".into()
        };
    });

    let started = Instant::now();
    let mut sent_this_run = 0_u64;
    let mut last_disk_check = 0_u64;
    let mut offset = have;
    loop {
        // Bandwidth bound: never ask for more than max_bps on average.
        let due = started + Duration::from_secs_f64(sent_this_run as f64 / req.max_bps as f64);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            tokio::time::sleep(wait).await;
        }
        let (data, file_size, end) = read_block(&mut stream, req, offset, gate, cancel).await?;
        if !data.is_empty() {
            file.write_all(&data)
                .await
                .map_err(|e| format!("cannot write download: {e}"))?;
            offset += data.len() as u64;
            sent_this_run += data.len() as u64;
        }
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        manager.update(req.id, |s| {
            s.bytes_done = offset;
            s.total_bytes = file_size.max(offset);
            s.bytes_per_second = (sent_this_run as f64 / elapsed) as u64;
        });
        if offset - last_disk_check >= 4 * 1024 * 1024 {
            last_disk_check = offset;
            disk_ok(&req.root, 64 * 1024 * 1024)?;
        }
        if end || offset >= file_size {
            break;
        }
        if data.is_empty() {
            return Err(StreamError::Other("board returned an empty block".into()));
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    file.sync_all().await.map_err(|e| e.to_string())?;
    drop(file);
    stream.finish(false);
    tokio::fs::rename(&part_path, &final_path)
        .await
        .map_err(|e| format!("cannot finalize download: {e}"))?;
    manager.update(req.id, |s| {
        s.phase = Phase::Complete;
        s.message = "downloaded; ready to import".into();
    });
    Ok(())
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
    if !supports_sd_download(board) {
        return Err(format!("{} does not serve SD downloads", board.as_str()));
    }
    let dir_i = dir_index(dir).ok_or("unknown SD folder")?;
    if !valid_name(name) {
        return Err("invalid SD file name".into());
    }
    if let Some(reason) = gate() {
        return Err(reason);
    }
    let (id, cancel) = manager.create_job(board, DIRECTORIES[dir_i as usize], name)?;
    let req = TransferRequest {
        id,
        board,
        dir: dir_i,
        name: name.into(),
        root,
        max_bps: max_bytes_per_second(),
    };
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
        let files = list_on_stream(&mut stream, &gate, &never).await;
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
    match start_job(
        &MANAGER,
        router,
        board,
        &body.dir,
        &body.name,
        root,
        app_gate(&state),
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
        .route(
            "/api/recordings/{id}/sd/imports/{token}/download",
            post(import_downloaded),
        )
}

#[cfg(test)]
mod tests;
