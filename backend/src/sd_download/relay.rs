//! RRLS store-and-forward of the FC SD files, path selection and the
//! "AV bay can be powered off" readiness.
//!
//! **Why bounded.** All AV-bay egress leaves through the RF board's radio
//! turn (691 UART bytes per 200 ms, ~3.4 KB/s, RF board docs section 4.3),
//! whoever receives it. An RRLS closer to the landed rocket therefore cannot
//! fetch much faster than the GS: it gains only a cleaner link (fewer CRC
//! rejects and retransmissions) and, like the direct path, the adaptive rate
//! up to the radio ceiling (`fc::radio_ceiling`). A standby RRLS's radio is
//! silent by design (its router side has egress disabled so it never
//! collides with the primary); it fetches from the FC only while it owns the
//! AV bay (takeover), when the primary is not heard at all. A faster
//! post-landing path needs an RF-board firmware change (a larger turn burst
//! share after Landed that still leaves uplink time for recovery commands).
//!
//! **RRLS side.** A completed, CRC-verified FC file is copied into the
//! protected outbox `data/sd-downloads/FC/outbox/` with its SHA-256
//! (`outbox.json`); nothing there is ever deleted automatically. The outbox
//! is served on SEDSnet P2P port [`RELAY_STREAM_PORT`] with the board SD
//! wire (LIST dir 0, READ 512-byte blocks with the CRC-32 frame flag), plus a
//! virtual `<name>.sha256` sidecar per file holding its metadata, and one more
//! opcode: CONFIRM (`0x24`, SHA-256 + name) marks a file received by the GS.
//!
//! **GS side.** [`pull`] lists the RRLS outbox, reads each sidecar, downloads
//! the file (resumable by offset in `sd-downloads/FC/RRLS/<name>.part`),
//! verifies its SHA-256 end to end (a mismatch deletes the copy and fetches
//! it again; three mismatches refuse it), confirms it to the RRLS and
//! attaches it to the capture with `via=RRLS` provenance. Same pause rules
//! and adaptive rate as the direct path, one request in flight.
use super::*;
use super::fc::{FcJob, Stage};
use crate::recording_store::board_sd;

/// SEDSnet P2P port of the RRLS relay outbox.
pub const RELAY_STREAM_PORT: u16 = 4512;
pub(crate) const RRLS_HOST: &str = "RRLS";
const OUTBOX_FILE: &str = "outbox.json";
const RELAY_FILE: &str = "relay.json";
const SIDECAR: &str = ".sha256";
const MAX_SHA_MISMATCHES: u32 = 3;
const CONFIRM_OK: u8 = 0;
const CONFIRM_MISMATCH: u8 = 4;

// ------------------------------------------------------------ path choice

/// What the deciding station knows about both paths.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathMetrics {
    /// Age of the last direct AV-bay frame at the GS.
    pub gs_direct_age_ms: Option<u64>,
    /// Raw-UART frames (good, CRC-rejected) in the last minute at the GS.
    pub gs_good: u64,
    pub gs_crc: u64,
    /// The RRLS was heard recently (HEALTH / topology).
    pub rrls_online: bool,
    /// The RRLS hears the AV bay (it offers relay readiness only then).
    pub rrls_hears_av_bay: bool,
    /// Raw-UART frames at the RRLS, when known.
    pub rrls_good: u64,
    pub rrls_crc: u64,
    /// The RRLS may transmit to the FC (takeover); a standby RRLS stays silent.
    pub rrls_can_transmit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Path {
    Direct,
    Rrls,
}

fn poor(good: u64, crc: u64) -> bool {
    crc >= 10 && crc * 100 >= (good + crc) * 20
}

/// RRLS when it hears the AV bay over a clean link, may transmit, and the
/// direct link is lost (no AV-bay frame for 3 s) or poor; direct otherwise.
pub fn choose_path(m: &PathMetrics) -> (Path, &'static str) {
    let direct_lost = m.gs_direct_age_ms.is_none_or(|age| age > 3_000);
    let direct_poor = poor(m.gs_good, m.gs_crc);
    if !(direct_lost || direct_poor) {
        return (Path::Direct, "direct AV-bay link is good");
    }
    if !m.rrls_online || !m.rrls_hears_av_bay {
        return (Path::Direct, "RRLS not in range of the AV bay");
    }
    if poor(m.rrls_good, m.rrls_crc) {
        return (Path::Direct, "RRLS link to the AV bay is poor");
    }
    if !m.rrls_can_transmit {
        return (Path::Direct, "RRLS is in standby (radio silent)");
    }
    (Path::Rrls, if direct_lost { "direct link lost; RRLS hears the AV bay" } else { "direct link poor; RRLS link clean" })
}

// ------------------------------------------------------------ RRLS outbox

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutboxEntry {
    pub name: String,
    /// `launch` or `lead_up`.
    pub kind: String,
    pub launch_file: String,
    pub bytes: u64,
    pub sha256: String,
    /// READ answers re-requested on the FC->RRLS hop.
    pub crc_errors: u64,
    pub fetched_at_ms: u64,
    /// The launch file's lead-up is settled (saved, unavailable or not wanted).
    pub lead_up_resolved: bool,
    pub confirmed_at_ms: Option<u64>,
}

static OUTBOX_LOCK: Mutex<()> = Mutex::new(());

fn fc_dir(root: &FsPath) -> PathBuf {
    root.join("sd-downloads").join("FC")
}

pub(crate) fn load_outbox(root: &FsPath) -> Vec<OutboxEntry> {
    std::fs::read(fc_dir(root).join(OUTBOX_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn modify_outbox<T>(root: &FsPath, apply: impl FnOnce(&mut Vec<OutboxEntry>) -> T) -> T {
    let _guard = OUTBOX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries = load_outbox(root);
    let out = apply(&mut entries);
    let _ = std::fs::create_dir_all(fc_dir(root));
    let path = fc_dir(root).join(OUTBOX_FILE);
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&entries).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
    out
}

fn outbox_path(root: &FsPath, name: &str) -> PathBuf {
    fc_dir(root).join("outbox").join(name)
}

/// RRLS: keep a verified FC file for forwarding (copied, never auto-deleted).
pub(crate) fn register(root: &FsPath, source: &FsPath, name: &str, kind: &str, launch_file: &str, crc_errors: u64, lead_up_resolved: bool) -> std::io::Result<OutboxEntry> {
    if !valid_name(name) {
        return Err(std::io::Error::other("invalid name"));
    }
    let target = outbox_path(root, name);
    std::fs::create_dir_all(target.parent().expect("outbox"))?;
    let tmp = target.with_extension("partial");
    std::fs::copy(source, &tmp)?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, &target)?;
    let entry = OutboxEntry {
        name: name.into(),
        kind: kind.into(),
        launch_file: launch_file.into(),
        bytes: std::fs::metadata(&target)?.len(),
        sha256: board_sd::sha256_hex(&target)?,
        crc_errors,
        fetched_at_ms: now_ms(),
        lead_up_resolved,
        confirmed_at_ms: None,
    };
    modify_outbox(root, |entries| {
        entries.retain(|e| e.name != name);
        entries.push(entry.clone());
    });
    Ok(entry)
}

/// RRLS: the launch file's lead-up is settled (saved or unavailable).
pub(crate) fn resolve_lead_up(root: &FsPath, launch_file: &str) {
    modify_outbox(root, |entries| {
        for e in entries.iter_mut().filter(|e| e.kind == "launch" && e.launch_file == launch_file) {
            e.lead_up_resolved = true;
        }
    });
}

fn confirm_local(root: &FsPath, name: &str, sha: &str) -> bool {
    modify_outbox(root, |entries| {
        let Some(e) = entries.iter_mut().find(|e| e.name == name && e.sha256 == sha) else { return false };
        e.confirmed_at_ms.get_or_insert(now_ms());
        true
    })
}

/// Bytes of an outbox file or its sidecar.
fn served(root: &FsPath, name: &str) -> Option<Vec<u8>> {
    let entries = load_outbox(root);
    if let Some(stem) = name.strip_suffix(SIDECAR) {
        let entry = entries.into_iter().find(|e| e.name == stem)?;
        return serde_json::to_vec(&entry).ok();
    }
    entries.iter().find(|e| e.name == name)?;
    std::fs::read(outbox_path(root, name)).ok()
}

/// RRLS: serve the outbox to the GS on [`RELAY_STREAM_PORT`]. `corrupt`
/// (tests) flips the CRC of that many READ answers.
pub(crate) fn serve(router: &Arc<Router>, root: PathBuf, corrupt: Option<Arc<AtomicU64>>) -> Result<(), String> {
    let reply = router.clone();
    router
        .bind_p2p_stream_port(RELAY_STREAM_PORT, move |event| {
            if event.kind != P2pStreamEventKind::Data || event.payload.is_empty() {
                return Ok(());
            }
            // The primary is the only consumer.
            if !event.peer_hostname.eq_ignore_ascii_case("GS") {
                return reply.reset_p2p_stream(event.stream_id).map(|_| ());
            }
            for frame in answer(&root, event.payload, corrupt.as_deref()) {
                reply.send_p2p_stream(event.stream_id, &frame)?;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())
}

fn answer(root: &FsPath, p: &[u8], corrupt: Option<&AtomicU64>) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    match p[0] {
        OP_LIST if p.len() == 4 => {
            let index = u16::from_le_bytes([p[2], p[3]]) as usize;
            let names: Vec<(String, u64)> = load_outbox(root)
                .into_iter()
                .flat_map(|e| {
                    let meta = serde_json::to_vec(&e).map_or(0, |m| m.len() as u64);
                    [(e.name.clone(), e.bytes), (format!("{}{SIDECAR}", e.name), meta)]
                })
                .collect();
            let entry = (p[1] == 0).then(|| names.get(index)).flatten();
            let mut f = vec![OP_LIST | RESPONSE_FLAG, if entry.is_some() { STATUS_OK } else { STATUS_END }, p[1], p[2], p[3]];
            let (size, name) = entry.map_or((0, ""), |(n, s)| (*s as u32, n.as_str()));
            f.extend_from_slice(&size.to_le_bytes());
            f.extend_from_slice(&[0, name.len() as u8, 0]);
            f.extend_from_slice(name.as_bytes());
            frames.push(f);
        }
        OP_READ if p.len() > 7 => {
            let offset = u32::from_le_bytes(p[1..5].try_into().expect("four")) as usize;
            let name = std::str::from_utf8(&p[7..]).unwrap_or("");
            let Some(data) = served(root, name) else {
                let mut f = vec![OP_READ | RESPONSE_FLAG, STATUS_NOT_FOUND, FLAG_LAST];
                f.extend_from_slice(&[0; 8]);
                return vec![f];
            };
            let header = |status: u8, flags: u8, off: usize| {
                let mut f = vec![OP_READ | RESPONSE_FLAG, status, flags];
                f.extend_from_slice(&(off as u32).to_le_bytes());
                f.extend_from_slice(&(data.len() as u32).to_le_bytes());
                f
            };
            if offset >= data.len() {
                frames.push(header(STATUS_END, FLAG_LAST, offset));
            } else {
                let end = ((offset / 512 + 1) * 512).min(data.len());
                let mut crc = crc32(&data[offset..end]);
                if let Some(c) = corrupt
                    && c.load(Ordering::SeqCst) > 0
                {
                    c.fetch_sub(1, Ordering::SeqCst);
                    crc ^= 0x5a;
                }
                let mut at = offset;
                while at < end {
                    let to = (at + 100).min(end);
                    let last = to == end;
                    let mut f = header(STATUS_OK, if last { FLAG_LAST | FLAG_CRC } else { 0 }, at);
                    f.extend_from_slice(&data[at..to]);
                    if last {
                        f.extend_from_slice(&crc.to_le_bytes());
                    }
                    frames.push(f);
                    at = to;
                }
            }
        }
        OP_CONFIRM if p.len() > 34 => {
            let sha: String = p[1..33].iter().map(|b| format!("{b:02x}")).collect();
            let name = std::str::from_utf8(&p[34..]).unwrap_or("");
            let ok = confirm_local(root, name, &sha);
            frames.push(vec![OP_CONFIRM | RESPONSE_FLAG, if ok { CONFIRM_OK } else { CONFIRM_MISMATCH }]);
        }
        OP_CANCEL => frames.push(vec![OP_CANCEL | RESPONSE_FLAG, 0]),
        _ => {}
    }
    frames
}

// ----------------------------------------------------------------- GS pull

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Received {
    pub name: String,
    pub kind: String,
    pub launch_file: String,
    pub sha256: String,
    pub bytes: u64,
    pub received_at_ms: u64,
    pub confirmed: bool,
    pub capture: Option<String>,
}

/// GS view of the relay (persisted in `sd-downloads/FC/relay.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RelayState {
    /// The RRLS outbox as last listed (metadata from the sidecars).
    pub rrls_outbox: Vec<OutboxEntry>,
    pub listed_at_ms: u64,
    pub received: Vec<Received>,
    /// SHA-256 mismatches per file name; refused at [`MAX_SHA_MISMATCHES`].
    pub mismatches: HashMap<String, u32>,
    pub refused: Vec<String>,
    /// The file being forwarded now: (name, bytes done, total).
    pub forwarding: Option<(String, u64, u64)>,
    pub message: String,
}

static RELAY_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn load_state(root: &FsPath) -> RelayState {
    std::fs::read(fc_dir(root).join(RELAY_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn modify_state<T>(root: &FsPath, apply: impl FnOnce(&mut RelayState) -> T) -> T {
    let _guard = RELAY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load_state(root);
    let out = apply(&mut state);
    let _ = std::fs::create_dir_all(fc_dir(root));
    let path = fc_dir(root).join(RELAY_FILE);
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&state).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
    out
}

/// Everything one pull needs (production from `AppState`, tests from fakes).
#[derive(Clone)]
pub(crate) struct PullEnv {
    pub manager: &'static Manager,
    pub router: Arc<Router>,
    pub root: PathBuf,
    pub rate: u64,
    pub rate_fn: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
    /// Pause rules (commands, GPS, radio, operator stop...).
    pub gate: Gate,
    pub notify: Arc<dyn Fn(String) + Send + Sync>,
}

fn relay_request(env: &PullEnv, id: u64, name: &str) -> TransferRequest {
    let mut req = TransferRequest::new(id, Board::FlightComputer, 0, name, env.root.join("sd-downloads"), env.rate);
    req.peer = Some((RRLS_HOST, RELAY_STREAM_PORT));
    req.local_dir = Some("RRLS");
    req.attach = false;
    req.rate_fn = env.rate_fn.clone();
    req
}

fn received_path(root: &FsPath, name: &str) -> PathBuf {
    fc_dir(root).join("RRLS").join(name)
}

async fn read_small(stream: &mut BoardStream, req: &TransferRequest, gate: &Gate) -> Result<Vec<u8>, StreamError> {
    let never = AtomicBool::new(false);
    let mut out = Vec::new();
    loop {
        let (chunk, size, end) = read_block(stream, req, out.len() as u64, gate, &never).await?;
        out.extend_from_slice(&chunk);
        if end || chunk.is_empty() || out.len() as u64 >= size || out.len() > 64 * 1024 {
            return Ok(out);
        }
    }
}

async fn send_confirm(env: &PullEnv, name: &str, sha: &str) -> Result<bool, StreamError> {
    let never = AtomicBool::new(false);
    let mut stream = BoardStream::open_peer(env.manager, env.router.clone(), RRLS_HOST, RELAY_STREAM_PORT, &env.gate, &never).await?;
    let mut frame = vec![OP_CONFIRM];
    for i in 0..32 {
        frame.push(u8::from_str_radix(sha.get(2 * i..2 * i + 2).unwrap_or("00"), 16).unwrap_or(0));
    }
    frame.push(name.len() as u8);
    frame.extend_from_slice(name.as_bytes());
    stream.send(&frame)?;
    let deadline = Instant::now() + RESPONSE_TIMEOUT;
    loop {
        if let Frame::Confirm(status) = stream.next_frame(deadline, &env.gate, &never).await? {
            stream.finish(false);
            return Ok(status == CONFIRM_OK);
        }
    }
}

/// One pass over the RRLS outbox: forward every file the GS does not hold
/// verified yet. Errors leave partial copies for the next pass.
pub(crate) async fn pull(env: &PullEnv) -> Result<usize, StreamError> {
    if let Some(reason) = (env.gate)() {
        return Err(StreamError::Gate(reason));
    }
    if env.manager.active() {
        return Err(StreamError::Gate("another SD transfer is running".into()));
    }
    let never = AtomicBool::new(false);
    let mut stream = BoardStream::open_peer(env.manager, env.router.clone(), RRLS_HOST, RELAY_STREAM_PORT, &env.gate, &never).await?;
    let listing = list_dirs(&mut stream, &["OUTBOX"], false, &env.gate, &never).await?;
    let mut metas = Vec::new();
    for file in listing.iter().filter(|f| f.name.ends_with(SIDECAR)) {
        let req = relay_request(env, 0, &file.name);
        let bytes = read_small(&mut stream, &req, &env.gate).await?;
        if let Ok(meta) = serde_json::from_slice::<OutboxEntry>(&bytes) {
            metas.push(meta);
        }
    }
    stream.finish(false);
    modify_state(&env.root, |s| {
        s.rrls_outbox = metas.clone();
        s.listed_at_ms = now_ms();
    });
    let mut forwarded = 0;
    // Launch files first, then lead-ups (they attach beside them).
    metas.sort_by_key(|m| (m.kind != "launch", m.fetched_at_ms));
    for meta in metas {
        let state = load_state(&env.root);
        if state.refused.contains(&meta.name) {
            continue;
        }
        if let Some(have) = state.received.iter().find(|r| r.name == meta.name && r.sha256 == meta.sha256) {
            if !have.confirmed && send_confirm(env, &meta.name, &meta.sha256).await? {
                modify_state(&env.root, |s| s.received.iter_mut().filter(|r| r.name == meta.name).for_each(|r| r.confirmed = true));
            }
            continue;
        }
        forward_one(env, &meta).await?;
        forwarded += 1;
    }
    Ok(forwarded)
}

async fn forward_one(env: &PullEnv, meta: &OutboxEntry) -> Result<(), StreamError> {
    let (id, cancel) = env.manager.create_job(Board::FlightComputer, "RRLS", &meta.name).map_err(StreamError::Gate)?;
    let req = relay_request(env, id, &meta.name);
    modify_state(&env.root, |s| {
        s.forwarding = Some((meta.name.clone(), 0, meta.bytes));
        s.message = format!("forwarding {} from the RRLS", meta.name);
    });
    let result = transfer(env.manager, env.router.clone(), &req, &env.gate, &cancel).await;
    if result.is_err() {
        env.manager.update(id, |s| {
            if s.phase.is_active() {
                s.phase = Phase::Paused;
            }
        });
        let done = env.manager.get(id).map_or(0, |s| s.bytes_done);
        modify_state(&env.root, |s| s.forwarding = Some((meta.name.clone(), done, meta.bytes)));
    }
    result?;
    let path = received_path(&env.root, &meta.name);
    let sha = board_sd::sha256_hex(&path).map_err(|e| StreamError::Other(e.to_string()))?;
    let bytes = std::fs::metadata(&path).map_or(0, |m| m.len());
    if sha != meta.sha256 || bytes != meta.bytes {
        let _ = std::fs::remove_file(&path);
        let count = modify_state(&env.root, |s| {
            let n = s.mismatches.entry(meta.name.clone()).or_default();
            *n += 1;
            let n = *n;
            if n >= MAX_SHA_MISMATCHES {
                s.refused.push(meta.name.clone());
            }
            s.forwarding = None;
            n
        });
        log::warn!("RRLS forward of {}: SHA-256 mismatch ({count})", meta.name);
        if count >= MAX_SHA_MISMATCHES {
            (env.notify)(format!("FC SD file {} from the RRLS failed its SHA-256 check {count} times and was refused; import it from the card.", meta.name));
            return Ok(());
        }
        return Err(StreamError::Other(format!("{}: SHA-256 mismatch after the RRLS forward; fetching it again", meta.name)));
    }
    let confirmed = send_confirm(env, &meta.name, &meta.sha256).await.unwrap_or(false);
    let capture = attach(env, meta, &path, req.crc_errors.load(Ordering::Relaxed)).await;
    modify_state(&env.root, |s| {
        s.forwarding = None;
        s.received.retain(|r| r.name != meta.name);
        s.received.push(Received {
            name: meta.name.clone(),
            kind: meta.kind.clone(),
            launch_file: meta.launch_file.clone(),
            sha256: sha.clone(),
            bytes,
            received_at_ms: now_ms(),
            confirmed,
            capture: capture.clone(),
        });
        s.message = format!("{} received from the RRLS, SHA-256 verified", meta.name);
    });
    if meta.kind == "launch" {
        fc::relay_received(&env.root, &meta.name, &sha, capture.as_deref());
    }
    (env.notify)(format!(
        "FC SD {} {} received from the RRLS ({bytes} bytes, SHA-256 verified){}.",
        if meta.kind == "launch" { "launch file" } else { "lead-up" },
        meta.name,
        capture.as_deref().map_or(String::new(), |c| format!(", attached to {c}"))
    ));
    Ok(())
}

/// Attaches a forwarded file: a launch file like any download (`via=RRLS`),
/// a lead-up beside its launch file. Returns the capture folder.
async fn attach(env: &PullEnv, meta: &OutboxEntry, path: &FsPath, crc_errors: u64) -> Option<String> {
    let root = env.root.clone();
    let (meta, path) = (meta.clone(), path.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let relay = board_sd::RelayProvenance {
            via: "RRLS".into(),
            relay_id: RRLS_HOST.into(),
            hops: vec![
                board_sd::RelayHop { hop: "FC->RRLS".into(), check: "crc32_per_block".into(), sha256: meta.sha256.clone(), crc_errors: meta.crc_errors, completed_at_ms: meta.fetched_at_ms },
                board_sd::RelayHop { hop: "RRLS->GS".into(), check: "crc32_per_block+sha256".into(), sha256: meta.sha256.clone(), crc_errors, completed_at_ms: now_ms() },
            ],
        };
        let folder = if meta.kind == "launch" {
            let provenance = board_sd::Provenance { source: "network_download".into(), link: Some("radio".into()), relay: Some(relay) };
            board_sd::offer_file_with(&root, &path, &meta.name, &provenance).ok()?.into_iter().find_map(|o| match o {
                board_sd::Outcome::Attached(folder) => Some(folder),
                _ => None,
            })?
        } else {
            let folder = board_sd::folders_with(&root, "FC", &meta.launch_file).into_iter().next()?;
            let entry = crate::recording_store::read_manifest(&folder)?.board_sd.into_iter().find(|e| e.board == "FC" && e.file == meta.launch_file)?;
            board_sd::cut_lead_up(&folder, "FC", &meta.launch_file, &path, entry.data_from_ms - board_sd::LEAD_UP_MS, entry.data_from_ms).ok()?;
            folder
        };
        Some(folder.strip_prefix(&root).unwrap_or(&folder).to_string_lossy().into_owned())
    })
    .await
    .ok()
    .flatten()
}

/// GS: the RRLS outbox (as last listed) holds the newest job's launch file.
pub(crate) fn rrls_holds_launch(root: &FsPath) -> bool {
    let job = fc::load(root).jobs.into_iter().rev().find(|j| !j.done);
    let Some(job) = job else { return false };
    let state = load_state(root);
    state.rrls_outbox.iter().any(|e| {
        e.kind == "launch"
            && match &job.launch_file {
                Some(name) => &e.name == name,
                None => e.fetched_at_ms >= job.triggered_at_ms,
            }
    })
}

static PULLING: AtomicBool = AtomicBool::new(false);

/// GS: forward from the RRLS whenever it is reachable, every 10 s, resuming
/// partial copies; one loop per process.
pub(crate) fn spawn_pull_loop(state: &Arc<AppState>, root: &FsPath, router: Arc<Router>) {
    if PULLING.swap(true, Ordering::AcqRel) {
        return;
    }
    let env = fc::pull_env(state, root, router.clone());
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        struct Pulling;
        impl Drop for Pulling {
            fn drop(&mut self) {
                PULLING.store(false, Ordering::Release);
            }
        }
        let _pulling = Pulling;
        let mut backoff = 10u64;
        while weak.upgrade().is_some_and(|s| !s.shutdown_requested.load(Ordering::Acquire)) {
            if router.resolve_hostname(RRLS_HOST).is_none() {
                return;
            }
            match pull(&env).await {
                // Nothing new: list again in a minute (a LIST costs air time).
                Ok(0) => backoff = 60,
                Ok(_) => backoff = 10,
                Err(StreamError::Gate(reason)) => {
                    modify_state(&env.root, |s| s.message = format!("forward paused: {reason}"));
                }
                Err(error) => {
                    let text = match error {
                        StreamError::Other(e) => e,
                        _ => "cancelled".into(),
                    };
                    modify_state(&env.root, |s| s.message = format!("forward: {text}; retrying"));
                    backoff = (backoff * 2).min(60);
                }
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
        }
    });
}

// -------------------------------------------------------- power-off ready

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PowerOff {
    pub ready: bool,
    /// `GS` or `RRLS`: where the verified copy is.
    pub at: Option<String>,
    pub message: String,
}

/// What one station verifiably holds of this flight's FC files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Holding {
    /// The launch file complete with every block CRC-checked and its SHA-256 computed.
    pub launch_verified: bool,
    /// The lead-up is saved, or settled as unavailable.
    pub lead_up_resolved: bool,
}

/// The AV bay may be powered off once the GS or the RRLS holds the launch
/// file verified (and the lead-up settled when it is wanted). Complete at
/// the RRLS counts even before its forward to the GS has finished.
pub fn power_off_ready(lead_up_wanted: bool, gs: Holding, rrls: Holding) -> PowerOff {
    let ok = |h: Holding| h.launch_verified && (!lead_up_wanted || h.lead_up_resolved);
    let (ready, at) = if ok(gs) {
        (true, Some("GS"))
    } else if ok(rrls) {
        (true, Some("RRLS"))
    } else {
        (false, None)
    };
    let message = match at {
        Some(at) => format!("FC SD fully received by {at}, checksums verified: AV bay can be powered off"),
        None if gs.launch_verified || rrls.launch_verified => "FC launch file verified; waiting for its lead-up before the AV bay can be powered off".into(),
        None => "FC SD not yet collected: keep the AV bay powered".into(),
    };
    PowerOff { ready, at: at.map(Into::into), message }
}

/// Readiness for the newest FC job, from the GS job and the RRLS outbox.
pub(crate) fn power_off_for(job: Option<&FcJob>, lead_up_wanted: bool, relay: &RelayState) -> PowerOff {
    let Some(job) = job else { return PowerOff { message: "no FC landing yet".into(), ..PowerOff::default() } };
    let gs = Holding {
        launch_verified: job.sha256.is_some() && matches!(job.stage, Stage::LeadUp | Stage::Done),
        lead_up_resolved: job.stage == Stage::Done,
    };
    let launch = relay.rrls_outbox.iter().filter(|e| e.kind == "launch").find(|e| match &job.launch_file {
        Some(name) => &e.name == name,
        None => e.fetched_at_ms >= job.triggered_at_ms,
    });
    let rrls = Holding {
        launch_verified: launch.is_some_and(|e| !e.sha256.is_empty()),
        lead_up_resolved: launch.is_some_and(|e| e.lead_up_resolved),
    };
    power_off_ready(lead_up_wanted, gs, rrls)
}

#[cfg(test)]
#[path = "relay_tests.rs"]
mod tests;
