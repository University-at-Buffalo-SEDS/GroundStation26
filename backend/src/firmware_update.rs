use crate::types::Board;
use sedsnet::router::{P2pStreamEventKind, P2pStreamId, Router};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

pub const OTA_STREAM_PORT: u16 = 4510;
pub const OTA_MAX_CHUNK: usize = 120;
pub const MAX_UPLOAD_BYTES: usize = 16 * 1024 * 1024;

const LAUNCHCORE_DELTA_MAGIC: u32 = 0x4C43_4450;
const LAUNCHCORE_IMAGE_MAGIC: u32 = 0x4C43_494D;
const LAUNCHCORE_DELTA_VERSION: u16 = 1;
const LAUNCHCORE_DELTA_HEADER_SIZE: usize = 100;

const OTA_OP_BEGIN_DELTA: u8 = 0x01;
const OTA_OP_CHUNK: u8 = 0x02;
const OTA_OP_FINISH: u8 = 0x03;
const OTA_OP_ABORT: u8 = 0x04;
const OTA_RESPONSE_FLAG: u8 = 0x80;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const SOURCE_PORT_FIRST: u16 = 49_152;
const SOURCE_PORT_LAST: u16 = 65_535;

static MANAGER: LazyLock<FirmwareUpdateManager> = LazyLock::new(FirmwareUpdateManager::new);

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FirmwareUpdatePhase {
    Queued,
    Connecting,
    Beginning,
    Transferring,
    Finishing,
    Rebooting,
    Completed,
    Cancelling,
    Cancelled,
    Failed,
}

impl FirmwareUpdatePhase {
    fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::Connecting
                | Self::Beginning
                | Self::Transferring
                | Self::Finishing
                | Self::Rebooting
                | Self::Cancelling
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FirmwareUpdateStatus {
    pub id: u64,
    pub board: String,
    pub board_label: String,
    pub filename: String,
    pub artifact_kind: &'static str,
    pub phase: FirmwareUpdatePhase,
    pub bytes_sent: usize,
    pub total_bytes: usize,
    pub progress_percent: f32,
    pub board_max_bytes: Option<u32>,
    pub message: String,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone)]
struct JobEntry {
    status: FirmwareUpdateStatus,
    cancel: Arc<AtomicBool>,
}

struct FirmwareUpdateManager {
    jobs: Mutex<HashMap<u64, JobEntry>>,
    next_id: AtomicU64,
    next_source_port: AtomicU16,
}

#[derive(Debug, Clone)]
struct OwnedStreamEvent {
    kind: P2pStreamEventKind,
    stream_id: P2pStreamId,
    peer_hostname: String,
    payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
struct OtaResponse {
    opcode: u8,
    status: u32,
    expected_offset: u32,
    max_patch_size: u32,
}

impl FirmwareUpdateManager {
    fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            next_source_port: AtomicU16::new(SOURCE_PORT_FIRST),
        }
    }

    fn next_source_port(&self) -> u16 {
        self.next_source_port
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |port| {
                Some(if port == SOURCE_PORT_LAST {
                    SOURCE_PORT_FIRST
                } else {
                    port + 1
                })
            })
            .unwrap_or(SOURCE_PORT_FIRST)
    }

    fn create_job(
        &self,
        board: Board,
        filename: String,
        total_bytes: usize,
    ) -> Result<(u64, Arc<AtomicBool>), String> {
        let mut jobs = self.jobs.lock().expect("firmware update jobs poisoned");
        if let Some(active) = jobs.values().find(|job| job.status.phase.is_active()) {
            return Err(format!(
                "firmware update {} for {} is already active",
                active.status.id, active.status.board
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let now_ms = now_ms();
        let cancel = Arc::new(AtomicBool::new(false));
        jobs.insert(
            id,
            JobEntry {
                status: FirmwareUpdateStatus {
                    id,
                    board: board.sender_id().to_string(),
                    board_label: board.as_str().to_string(),
                    filename,
                    artifact_kind: "launchcore_delta",
                    phase: FirmwareUpdatePhase::Queued,
                    bytes_sent: 0,
                    total_bytes,
                    progress_percent: 0.0,
                    board_max_bytes: None,
                    message: "queued".to_string(),
                    started_at_ms: now_ms,
                    updated_at_ms: now_ms,
                },
                cancel: cancel.clone(),
            },
        );
        Ok((id, cancel))
    }

    fn update(
        &self,
        id: u64,
        phase: FirmwareUpdatePhase,
        bytes_sent: usize,
        board_max_bytes: Option<u32>,
        message: impl Into<String>,
    ) {
        let mut jobs = self.jobs.lock().expect("firmware update jobs poisoned");
        let Some(job) = jobs.get_mut(&id) else {
            return;
        };
        job.status.phase = phase;
        job.status.bytes_sent = bytes_sent.min(job.status.total_bytes);
        job.status.progress_percent = if job.status.total_bytes == 0 {
            0.0
        } else {
            job.status.bytes_sent as f32 * 100.0 / job.status.total_bytes as f32
        };
        if board_max_bytes.is_some() {
            job.status.board_max_bytes = board_max_bytes;
        }
        job.status.message = message.into();
        job.status.updated_at_ms = now_ms();
    }

    fn get(&self, id: u64) -> Option<FirmwareUpdateStatus> {
        self.jobs
            .lock()
            .expect("firmware update jobs poisoned")
            .get(&id)
            .map(|entry| entry.status.clone())
    }

    fn list(&self) -> Vec<FirmwareUpdateStatus> {
        let mut statuses = self
            .jobs
            .lock()
            .expect("firmware update jobs poisoned")
            .values()
            .map(|entry| entry.status.clone())
            .collect::<Vec<_>>();
        statuses.sort_by_key(|status| std::cmp::Reverse(status.id));
        statuses
    }

    fn cancel(&self, id: u64) -> Result<FirmwareUpdateStatus, String> {
        let mut jobs = self.jobs.lock().expect("firmware update jobs poisoned");
        let Some(job) = jobs.get_mut(&id) else {
            return Err("firmware update not found".to_string());
        };
        if !job.status.phase.is_active() {
            return Err("firmware update is no longer active".to_string());
        }
        job.cancel.store(true, Ordering::Relaxed);
        job.status.phase = FirmwareUpdatePhase::Cancelling;
        job.status.message = "cancellation requested".to_string();
        job.status.updated_at_ms = now_ms();
        Ok(job.status.clone())
    }
}

pub fn start_update(
    router: Arc<Router>,
    board: Board,
    filename: String,
    firmware: Vec<u8>,
) -> Result<FirmwareUpdateStatus, String> {
    let filename = validate_firmware_filename(&filename)?;
    if board == Board::GroundStation {
        return Err("the ground station cannot be an OTA target".to_string());
    }
    if !supports_live_ota(board) {
        return Err(format!(
            "{} does not expose a live SEDSnet OTA receiver in its current firmware",
            board.as_str()
        ));
    }
    if firmware.is_empty() {
        return Err("firmware upload is empty".to_string());
    }
    if firmware.len() > MAX_UPLOAD_BYTES {
        return Err(format!(
            "firmware upload exceeds the {} byte server limit",
            MAX_UPLOAD_BYTES
        ));
    }
    if firmware.len() > u32::MAX as usize {
        return Err("firmware upload is too large for the OTA protocol".to_string());
    }
    validate_live_ota_artifact(&firmware)?;
    if router.resolve_hostname(board.sender_id()).is_none() {
        return Err(format!(
            "{} ({}) is not present in the SEDSnet address book",
            board.as_str(),
            board.sender_id()
        ));
    }

    let (id, cancel) = MANAGER.create_job(board, filename, firmware.len())?;
    let status = MANAGER.get(id).expect("new firmware update must exist");
    tokio::spawn(run_update(id, router, board, firmware, cancel));
    Ok(status)
}

pub fn update_status(id: u64) -> Option<FirmwareUpdateStatus> {
    MANAGER.get(id)
}

pub fn update_history() -> Vec<FirmwareUpdateStatus> {
    MANAGER.list()
}

pub fn cancel_update(id: u64) -> Result<FirmwareUpdateStatus, String> {
    MANAGER.cancel(id)
}

async fn run_update(
    id: u64,
    router: Arc<Router>,
    board: Board,
    firmware: Vec<u8>,
    cancel: Arc<AtomicBool>,
) {
    if let Err(err) = run_update_inner(id, &router, board, &firmware, &cancel).await {
        let phase = if cancel.load(Ordering::Relaxed) {
            FirmwareUpdatePhase::Cancelled
        } else {
            FirmwareUpdatePhase::Failed
        };
        let sent = MANAGER.get(id).map_or(0, |status| status.bytes_sent);
        MANAGER.update(id, phase, sent, None, err);
    }
}

async fn run_update_inner(
    id: u64,
    router: &Arc<Router>,
    board: Board,
    firmware: &[u8],
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let source_port = MANAGER.next_source_port();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<OwnedStreamEvent>();
    router
        .bind_p2p_stream_port(source_port, move |event| {
            event_tx
                .send(OwnedStreamEvent {
                    kind: event.kind,
                    stream_id: event.stream_id,
                    peer_hostname: event.peer_hostname.to_string(),
                    payload: event.payload.to_vec(),
                })
                .map_err(|_| sedsnet::TelemetryError::HandlerError("OTA event receiver closed"))
        })
        .map_err(|err| format!("failed to bind OTA response stream: {err}"))?;

    let mut active_stream = None;
    let mut update_started = false;
    let result = async {
        MANAGER.update(
            id,
            FirmwareUpdatePhase::Connecting,
            0,
            None,
            format!(
                "connecting to {} on stream port {OTA_STREAM_PORT}",
                board.sender_id()
            ),
        );
        let stream_id = router
            .open_p2p_stream_to_hostname(board.sender_id(), OTA_STREAM_PORT, source_port)
            .map_err(|err| format!("failed to open OTA stream to {}: {err}", board.sender_id()))?;
        active_stream = Some(stream_id);
        wait_for_connected(&mut event_rx, stream_id, board.sender_id()).await?;
        check_cancel(cancel)?;

        MANAGER.update(
            id,
            FirmwareUpdatePhase::Beginning,
            0,
            None,
            "starting delta update",
        );
        let mut begin = [0_u8; 5];
        begin[0] = OTA_OP_BEGIN_DELTA;
        begin[1..].copy_from_slice(&(firmware.len() as u32).to_le_bytes());
        send_ota(router, stream_id, board.sender_id(), &begin)?;
        let begin_response = wait_for_response(
            &mut event_rx,
            stream_id,
            board.sender_id(),
            OTA_OP_BEGIN_DELTA,
        )
        .await?;
        ensure_ota_ok(begin_response)?;
        update_started = true;
        if begin_response.expected_offset != 0 {
            return Err(format!(
                "{} began the update at offset {}; expected 0",
                board.sender_id(),
                begin_response.expected_offset
            ));
        }
        if firmware.len() > begin_response.max_patch_size as usize {
            return Err(format!(
                "firmware is {} bytes but {} accepts at most {} bytes",
                firmware.len(),
                board.sender_id(),
                begin_response.max_patch_size
            ));
        }
        MANAGER.update(
            id,
            FirmwareUpdatePhase::Transferring,
            0,
            Some(begin_response.max_patch_size),
            "transferring firmware",
        );

        let mut offset = 0_usize;
        while offset < firmware.len() {
            if cancel.load(Ordering::Relaxed) {
                MANAGER.update(
                    id,
                    FirmwareUpdatePhase::Cancelling,
                    offset,
                    None,
                    "aborting update on board",
                );
                let _ = router.send_p2p_stream(stream_id, &[OTA_OP_ABORT]);
                let _ = router.reset_p2p_stream(stream_id);
                return Err("firmware update cancelled".to_string());
            }
            let end = (offset + OTA_MAX_CHUNK).min(firmware.len());
            let mut chunk = Vec::with_capacity(5 + end - offset);
            chunk.push(OTA_OP_CHUNK);
            chunk.extend_from_slice(&(offset as u32).to_le_bytes());
            chunk.extend_from_slice(&firmware[offset..end]);
            send_ota(router, stream_id, board.sender_id(), &chunk)?;
            let response =
                wait_for_response(&mut event_rx, stream_id, board.sender_id(), OTA_OP_CHUNK)
                    .await?;
            ensure_ota_ok(response)?;
            if response.expected_offset != end as u32 {
                return Err(format!(
                    "{} acknowledged offset {}, expected {}",
                    board.sender_id(),
                    response.expected_offset,
                    end
                ));
            }
            offset = end;
            MANAGER.update(
                id,
                FirmwareUpdatePhase::Transferring,
                offset,
                Some(response.max_patch_size),
                format!("transferred {offset}/{} bytes", firmware.len()),
            );
        }

        check_cancel(cancel)?;
        MANAGER.update(
            id,
            FirmwareUpdatePhase::Finishing,
            firmware.len(),
            None,
            "validating and committing firmware",
        );
        send_ota(router, stream_id, board.sender_id(), &[OTA_OP_FINISH])?;
        let finish =
            wait_for_response(&mut event_rx, stream_id, board.sender_id(), OTA_OP_FINISH).await?;
        ensure_ota_ok(finish)?;
        update_started = false;
        MANAGER.update(
            id,
            FirmwareUpdatePhase::Rebooting,
            firmware.len(),
            Some(finish.max_patch_size),
            "firmware accepted; board is rebooting",
        );
        let _ = router.close_p2p_stream(stream_id);
        MANAGER.update(
            id,
            FirmwareUpdatePhase::Completed,
            firmware.len(),
            Some(finish.max_patch_size),
            "firmware accepted and reboot requested",
        );
        Ok(())
    }
    .await;

    if result.is_err()
        && let Some(stream_id) = active_stream
    {
        if update_started {
            let _ = router.send_p2p_stream(stream_id, &[OTA_OP_ABORT]);
        }
        let _ = router.reset_p2p_stream(stream_id);
    }
    router.clear_p2p_stream_port(source_port);
    result
}

fn send_ota(
    router: &Router,
    stream_id: P2pStreamId,
    board: &str,
    payload: &[u8],
) -> Result<(), String> {
    router
        .send_p2p_stream(stream_id, payload)
        .map_err(|err| format!("failed to send OTA message to {board}: {err}"))
}

async fn wait_for_connected(
    event_rx: &mut mpsc::UnboundedReceiver<OwnedStreamEvent>,
    stream_id: P2pStreamId,
    board: &str,
) -> Result<(), String> {
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        loop {
            let event = event_rx.recv().await?;
            if event.stream_id == stream_id && event.peer_hostname == board {
                match event.kind {
                    P2pStreamEventKind::Connected => return Some(Ok(())),
                    P2pStreamEventKind::Closed | P2pStreamEventKind::Reset => {
                        return Some(Err("OTA stream closed while connecting".to_string()));
                    }
                    _ => {}
                }
            }
        }
    })
    .await
    .map_err(|_| format!("timed out connecting to OTA service on {board}"))?
    .ok_or_else(|| "OTA event channel closed while connecting".to_string())?
}

async fn wait_for_response(
    event_rx: &mut mpsc::UnboundedReceiver<OwnedStreamEvent>,
    stream_id: P2pStreamId,
    board: &str,
    opcode: u8,
) -> Result<OtaResponse, String> {
    tokio::time::timeout(RESPONSE_TIMEOUT, async {
        loop {
            let event = event_rx
                .recv()
                .await
                .ok_or_else(|| "OTA event channel closed".to_string())?;
            if event.stream_id != stream_id || event.peer_hostname != board {
                continue;
            }
            match event.kind {
                P2pStreamEventKind::Data => {
                    let response = parse_response(&event.payload)?;
                    if response.opcode == opcode | OTA_RESPONSE_FLAG {
                        return Ok(response);
                    }
                }
                P2pStreamEventKind::Closed | P2pStreamEventKind::Reset => {
                    return Err(format!("OTA stream to {board} closed unexpectedly"));
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for OTA response from {board}"))?
}

fn parse_response(payload: &[u8]) -> Result<OtaResponse, String> {
    if payload.len() != 13 {
        return Err(format!(
            "invalid OTA response length {}; expected 13",
            payload.len()
        ));
    }
    Ok(OtaResponse {
        opcode: payload[0],
        status: u32::from_le_bytes(payload[1..5].try_into().expect("four-byte status")),
        expected_offset: u32::from_le_bytes(payload[5..9].try_into().expect("four-byte offset")),
        max_patch_size: u32::from_le_bytes(payload[9..13].try_into().expect("four-byte maximum")),
    })
}

fn ensure_ota_ok(response: OtaResponse) -> Result<(), String> {
    if response.status == 0 {
        return Ok(());
    }
    let reason = match response.status {
        1 => "bad message",
        2 => "bad state",
        3 => "bad offset",
        4 => "no space",
        5 => "storage error",
        6 => "bad image",
        7 => "internal error",
        _ => "unknown error",
    };
    Err(format!(
        "board rejected OTA opcode 0x{:02x}: {reason} (status {})",
        response.opcode & !OTA_RESPONSE_FLAG,
        response.status
    ))
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("firmware update cancelled".to_string())
    } else {
        Ok(())
    }
}

fn sanitize_filename(filename: &str) -> String {
    let filename = filename.trim();
    let leaf = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let safe = leaf
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
        .take(128)
        .collect::<String>();
    if safe.is_empty() {
        "firmware.seds".to_string()
    } else {
        safe
    }
}

/// Sanitizes an upload name and requires the SEDS firmware artifact extension.
pub fn validate_firmware_filename(filename: &str) -> Result<String, String> {
    if filename.trim().is_empty() {
        return Err("firmware filename is required".to_string());
    }
    let filename = sanitize_filename(filename);
    if !filename.to_ascii_lowercase().ends_with(".seds") {
        return Err("firmware filename must use the .seds extension".to_string());
    }
    Ok(filename)
}

/// Returns whether the checked-in firmware for a board implements the live OTA stream service.
pub fn supports_live_ota(board: Board) -> bool {
    // Audited against the sibling 2026 firmware repositories. These applications expose
    // `Core/Src/ota_stream.c` on stream port 4510. The flight computer is intentionally kept
    // unavailable until its application wires in the same receiver.
    matches!(
        board,
        Board::RFBoard
            | Board::PowerBoard
            | Board::ValveBoard
            | Board::GatewayBoard
            | Board::ActuatorBoard
            | Board::DaqBoard
    )
}

/// Rejects recovery/full-image `.seds` files and malformed deltas before opening a board stream.
fn validate_live_ota_artifact(firmware: &[u8]) -> Result<(), String> {
    if firmware.len() < 4 {
        return Err(".seds artifact is too small to contain a LaunchCore header".to_string());
    }
    let magic = u32::from_le_bytes(firmware[0..4].try_into().expect("four-byte magic"));
    if magic == LAUNCHCORE_IMAGE_MAGIC {
        return Err(
            "this .seds file is a full-image recovery artifact; the current board workflow requires UART bootloader recovery"
                .to_string(),
        );
    }
    if magic != LAUNCHCORE_DELTA_MAGIC {
        return Err(".seds artifact is not a LaunchCore delta".to_string());
    }
    if firmware.len() < LAUNCHCORE_DELTA_HEADER_SIZE {
        return Err("LaunchCore delta header is truncated".to_string());
    }
    let version = u16::from_le_bytes(firmware[4..6].try_into().expect("two-byte version"));
    let header_size =
        u16::from_le_bytes(firmware[6..8].try_into().expect("two-byte header size")) as usize;
    let total_size =
        u32::from_le_bytes(firmware[8..12].try_into().expect("four-byte total size")) as usize;
    let erase_size = u32::from_le_bytes(firmware[12..16].try_into().expect("four-byte erase size"));
    let record_count =
        u32::from_le_bytes(firmware[24..28].try_into().expect("four-byte record count"));
    let records_offset = u32::from_le_bytes(
        firmware[28..32]
            .try_into()
            .expect("four-byte records offset"),
    ) as usize;
    if version != LAUNCHCORE_DELTA_VERSION
        || header_size != LAUNCHCORE_DELTA_HEADER_SIZE
        || records_offset != header_size
        || total_size != firmware.len()
        || erase_size == 0
        || record_count == 0
    {
        return Err(".seds artifact has an invalid LaunchCore delta header".to_string());
    }
    let expected_crc =
        u32::from_le_bytes(firmware[96..100].try_into().expect("four-byte header CRC"));
    if crc32(&firmware[..96]) != expected_crc {
        return Err(".seds artifact has an invalid LaunchCore delta header CRC".to_string());
    }
    Ok(())
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn parse_board_target(raw: &str) -> Option<Board> {
    let normalized = raw.trim().to_ascii_lowercase().replace(['-', '_', ' '], "");
    match normalized.as_str() {
        "fc" | "flightcomputer" => Some(Board::FlightComputer),
        "rf" | "rfboard" => Some(Board::RFBoard),
        "pb" | "powerboard" => Some(Board::PowerBoard),
        "vb" | "valveboard" => Some(Board::ValveBoard),
        "gb" | "gw" | "gateway" | "gatewayboard" => Some(Board::GatewayBoard),
        "ab" | "actuator" | "actuatorboard" => Some(Board::ActuatorBoard),
        "daq" | "daqb" | "daqboard" => Some(Board::DaqBoard),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sedsnet::router::{P2pStreamEvent, RouterConfig};

    #[test]
    fn parses_ota_response() {
        let mut bytes = [0_u8; 13];
        bytes[0] = OTA_OP_CHUNK | OTA_RESPONSE_FLAG;
        bytes[5..9].copy_from_slice(&240_u32.to_le_bytes());
        bytes[9..13].copy_from_slice(&24_576_u32.to_le_bytes());
        let response = parse_response(&bytes).expect("valid response");
        assert_eq!(response.opcode, 0x82);
        assert_eq!(response.status, 0);
        assert_eq!(response.expected_offset, 240);
        assert_eq!(response.max_patch_size, 24_576);
    }

    #[test]
    fn maps_all_remote_board_targets() {
        for (raw, expected) in [
            ("FC", Board::FlightComputer),
            ("rf-board", Board::RFBoard),
            ("Power Board", Board::PowerBoard),
            ("VB", Board::ValveBoard),
            ("GW", Board::GatewayBoard),
            ("actuator_board", Board::ActuatorBoard),
            ("DAQB", Board::DaqBoard),
        ] {
            assert_eq!(parse_board_target(raw), Some(expected));
        }
    }

    #[test]
    fn strips_paths_and_unsafe_filename_characters() {
        assert_eq!(
            validate_firmware_filename("../../build/fw update.seds").unwrap(),
            "fwupdate.seds"
        );
    }

    #[test]
    fn rejects_non_seds_firmware_artifacts() {
        for filename in ["firmware.bin", "firmware.delta", "firmware.seds.zip", ""] {
            assert!(validate_firmware_filename(filename).is_err(), "{filename}");
        }
        assert_eq!(
            validate_firmware_filename("FIRMWARE.SEDS").unwrap(),
            "FIRMWARE.SEDS"
        );
    }

    #[test]
    fn exposes_every_checked_in_live_ota_receiver() {
        for board in [
            Board::RFBoard,
            Board::PowerBoard,
            Board::ValveBoard,
            Board::GatewayBoard,
            Board::ActuatorBoard,
            Board::DaqBoard,
        ] {
            assert!(supports_live_ota(board), "{}", board.as_str());
        }
        assert!(!supports_live_ota(Board::GroundStation));
        assert!(!supports_live_ota(Board::FlightComputer));
    }

    #[test]
    fn validates_launchcore_delta_artifacts_and_rejects_recovery_images() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let mut delta = vec![0_u8; LAUNCHCORE_DELTA_HEADER_SIZE + 1];
        delta[0..4].copy_from_slice(&LAUNCHCORE_DELTA_MAGIC.to_le_bytes());
        delta[4..6].copy_from_slice(&LAUNCHCORE_DELTA_VERSION.to_le_bytes());
        delta[6..8].copy_from_slice(&(LAUNCHCORE_DELTA_HEADER_SIZE as u16).to_le_bytes());
        let delta_size = delta.len() as u32;
        delta[8..12].copy_from_slice(&delta_size.to_le_bytes());
        delta[12..16].copy_from_slice(&2048_u32.to_le_bytes());
        delta[24..28].copy_from_slice(&1_u32.to_le_bytes());
        delta[28..32].copy_from_slice(&(LAUNCHCORE_DELTA_HEADER_SIZE as u32).to_le_bytes());
        let header_crc = crc32(&delta[..96]);
        delta[96..100].copy_from_slice(&header_crc.to_le_bytes());
        validate_live_ota_artifact(&delta).unwrap();

        let mut recovery = vec![0_u8; 256];
        recovery[0..4].copy_from_slice(&LAUNCHCORE_IMAGE_MAGIC.to_le_bytes());
        assert!(
            validate_live_ota_artifact(&recovery)
                .unwrap_err()
                .contains("UART bootloader recovery")
        );

        delta[8..12].copy_from_slice(&999_u32.to_le_bytes());
        assert!(validate_live_ota_artifact(&delta).is_err());
    }

    #[tokio::test]
    async fn transfers_firmware_over_a_routed_p2p_stream() {
        #[derive(Default)]
        struct MockOta {
            declared_size: usize,
            bytes: Vec<u8>,
            finished: bool,
        }

        fn response(opcode: u8, status: u32, offset: usize) -> [u8; 13] {
            let mut response = [0_u8; 13];
            response[0] = opcode | OTA_RESPONSE_FLAG;
            response[1..5].copy_from_slice(&status.to_le_bytes());
            response[5..9].copy_from_slice(&(offset as u32).to_le_bytes());
            response[9..13].copy_from_slice(&4096_u32.to_le_bytes());
            response
        }

        let client = Arc::new(Router::new(
            RouterConfig::default()
                .with_hostname("GS")
                .with_static_address(0x1001),
        ));
        let server = Arc::new(Router::new(
            RouterConfig::default()
                .with_hostname("AB")
                .with_static_address(0x1002),
        ));
        let server_rx = server.clone();
        client.add_side_packet("to-ab", move |packet| server_rx.rx_from_side(packet, 0));
        let client_rx = client.clone();
        server.add_side_packet("to-gs", move |packet| client_rx.rx_from_side(packet, 0));

        client.announce_discovery().expect("client discovery");
        server.announce_discovery().expect("server discovery");
        for _ in 0..3 {
            client.process_all_queues().expect("client queues");
            server.process_all_queues().expect("server queues");
        }
        assert!(client.resolve_hostname("AB").is_some());

        let received = Arc::new(Mutex::new(MockOta::default()));
        let received_for_handler = received.clone();
        let server_for_handler = server.clone();
        server
            .bind_p2p_stream_port(OTA_STREAM_PORT, move |event: P2pStreamEvent<'_>| {
                if event.kind != P2pStreamEventKind::Data || event.payload.is_empty() {
                    return Ok(());
                }
                let opcode = event.payload[0];
                let mut state = received_for_handler.lock().expect("mock OTA state");
                let status = match opcode {
                    OTA_OP_BEGIN_DELTA if event.payload.len() == 5 => {
                        state.declared_size = u32::from_le_bytes(
                            event.payload[1..5].try_into().expect("declared size"),
                        ) as usize;
                        state.bytes.clear();
                        0
                    }
                    OTA_OP_CHUNK if event.payload.len() >= 6 => {
                        let offset = u32::from_le_bytes(
                            event.payload[1..5].try_into().expect("chunk offset"),
                        ) as usize;
                        if offset != state.bytes.len() {
                            3
                        } else {
                            state.bytes.extend_from_slice(&event.payload[5..]);
                            0
                        }
                    }
                    OTA_OP_FINISH
                        if state.bytes.len() == state.declared_size && event.payload.len() == 1 =>
                    {
                        state.finished = true;
                        0
                    }
                    OTA_OP_ABORT => 0,
                    _ => 1,
                };
                let reply = response(opcode, status, state.bytes.len());
                drop(state);
                server_for_handler.send_p2p_stream(event.stream_id, &reply)
            })
            .expect("bind mock OTA service");

        let pumping = Arc::new(AtomicBool::new(true));
        let pumping_task = pumping.clone();
        let pump_client = client.clone();
        let pump_server = server.clone();
        let pump = tokio::spawn(async move {
            while pumping_task.load(Ordering::Relaxed) {
                pump_client.process_all_queues().expect("client pump");
                pump_server.process_all_queues().expect("server pump");
                tokio::task::yield_now().await;
            }
        });

        let firmware = (0..250_u16).map(|value| value as u8).collect::<Vec<_>>();
        run_update_inner(
            u64::MAX,
            &client,
            Board::ActuatorBoard,
            &firmware,
            &Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("OTA transfer should complete");
        pumping.store(false, Ordering::Relaxed);
        pump.await.expect("router pump task");

        let received = received.lock().expect("mock OTA result");
        assert_eq!(received.bytes, firmware);
        assert!(received.finished);
    }
}
