//! Low-priority fetch of a board's lead-up window after a launch.
//!
//! Written for the DAQ and shared with the Flight Computer collection
//! ([`super::fc`]): a job's `board` picks the card layout and its own jobs
//! file (`sd-downloads/<board>/leadup/jobs.json`); the FC worker drives its
//! jobs with [`step_with`] at the FC link rate and only ever reads a window.
//!
//! A DAQ launch file attached to a launch capture gets the minute before its
//! first row from the DAQ's regular session file of the same boot. Only that
//! window is read (protocol v2 OPENs any byte range): the file's last
//! kilobyte identifies the session that ended at the launch, a binary search
//! over 1 KiB probes finds the window start, and the window is then streamed
//! until it is covered. A small file is downloaded whole instead.
//!
//! The fetch never competes with live work: it runs only in the SD download
//! states (Idle, Landed, Recovery, no fill/launch sequence), pauses for any
//! other SD transfer and for [`QUIET_AFTER_COMMAND`] after any command
//! traffic, is rate-limited below the operator downloads, and keeps its state
//! in `sd-downloads/DAQ/leadup/jobs.json` so it resumes across link drops and
//! station restarts.
use super::*;
use crate::recording_store::board_sd;

/// A whole regular file at or below this size is downloaded and kept.
pub(crate) const SMALL_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// No lead-up traffic within this long of any command transmission.
pub(crate) const QUIET_AFTER_COMMAND: Duration = Duration::from_secs(10);
/// Rows can be queued slightly out of time order; start this much earlier.
const SEARCH_MARGIN_MS: i64 = 2_000;
const JOBS_FILE: &str = "jobs.json";
static LAST_COMMAND_MS: AtomicU64 = AtomicU64::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);
static JOBS_LOCK: Mutex<()> = Mutex::new(());

/// Called whenever command packets are flushed to the network.
pub(crate) fn note_command_traffic() {
    LAST_COMMAND_MS.store(now_ms(), Ordering::Release);
    super::fc::SIGNALS.note_command(now_ms());
}

#[cfg(test)]
pub(crate) static TEST_RATE: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
pub(crate) static TEST_SMALL_FILE_BYTES: AtomicU64 = AtomicU64::new(0);

fn small_file_bytes() -> u64 {
    #[cfg(test)]
    if TEST_SMALL_FILE_BYTES.load(Ordering::Relaxed) != 0 {
        return TEST_SMALL_FILE_BYTES.load(Ordering::Relaxed);
    }
    SMALL_FILE_BYTES
}

fn rate() -> u64 {
    #[cfg(test)]
    if TEST_RATE.load(Ordering::Relaxed) != 0 {
        return TEST_RATE.load(Ordering::Relaxed);
    }
    std::env::var("GS_SD_LEADUP_MAX_BYTES_PER_SEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| (512..=262_144).contains(v))
        .unwrap_or(8 * 1024)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// `DAQ` (default, older job files) or `FC`.
    #[serde(default = "daq_board")]
    pub board: String,
    /// Capture folder relative to the data directory.
    pub capture: String,
    pub launch_file: String,
    /// The lead-up window `[from_ms, to_ms)`; `to_ms` is the launch file's first row.
    pub from_ms: i64,
    pub to_ms: i64,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub source_dir: Option<String>,
    #[serde(default)]
    pub source_size: u64,
    #[serde(default)]
    pub start_offset: Option<u64>,
    #[serde(default)]
    pub fetched: u64,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub done: bool,
}

fn daq_board() -> String {
    "DAQ".into()
}

fn board_of(name: &str) -> Board {
    if name == "FC" { Board::FlightComputer } else { Board::DaqBoard }
}

fn dir(root: &FsPath, board: &str) -> PathBuf {
    root.join("sd-downloads").join(board).join("leadup")
}

/// The DAQ lead-up jobs (Recordings panel, DAQ worker).
pub(crate) fn load(root: &FsPath) -> Vec<Job> {
    load_board(root, "DAQ")
}

pub(crate) fn load_board(root: &FsPath, board: &str) -> Vec<Job> {
    std::fs::read(dir(root, board).join(JOBS_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(root: &FsPath, board: &str, jobs: &[Job]) {
    let path = dir(root, board).join(JOBS_FILE);
    let _ = std::fs::create_dir_all(dir(root, board));
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(jobs).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

fn update(root: &FsPath, board: &str, launch_file: &str, apply: impl FnOnce(&mut Job)) {
    let _guard = JOBS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut jobs = load_board(root, board);
    if let Some(job) = jobs.iter_mut().find(|j| j.launch_file == launch_file) {
        apply(job);
    }
    save(root, board, &jobs);
}

/// Queue the lead-up fetch for a DAQ launch file attached to `folder`.
pub(crate) fn enqueue(root: &FsPath, folder: &FsPath, launch_file: &str, first_row_ms: i64) {
    enqueue_board(root, "DAQ", folder, launch_file, first_row_ms);
}

/// Queue the lead-up fetch for `board`'s launch file attached to `folder`.
pub(crate) fn enqueue_board(root: &FsPath, board: &str, folder: &FsPath, launch_file: &str, first_row_ms: i64) {
    let Ok(capture) = folder.strip_prefix(root) else { return };
    let _guard = JOBS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut jobs = load_board(root, board);
    if jobs.iter().any(|j| j.launch_file == launch_file) {
        return;
    }
    jobs.push(Job {
        board: board.into(),
        capture: capture.to_string_lossy().into_owned(),
        launch_file: launch_file.into(),
        from_ms: first_row_ms - board_sd::LEAD_UP_MS,
        to_ms: first_row_ms,
        phase: "waiting".into(),
        message: "waiting until SD downloads are allowed".into(),
        ..Job::default()
    });
    save(root, board, &jobs);
}

pub(crate) fn pending(root: &FsPath) -> bool {
    load(root).iter().any(|j| !j.done)
}

/// A DAQ lead-up job is reading the card right now (not merely waiting).
pub(crate) fn transferring(root: &FsPath) -> bool {
    RUNNING.load(Ordering::Acquire)
        && load(root).iter().any(|j| !j.done && matches!(j.phase.as_str(), "searching" | "fetching" | "downloading"))
}

/// Some reason while command traffic was seen within [`QUIET_AFTER_COMMAND`].
pub(crate) fn command_quiet() -> Option<String> {
    let last = LAST_COMMAND_MS.load(Ordering::Acquire);
    (last != 0 && now_ms().saturating_sub(last) < QUIET_AFTER_COMMAND.as_millis() as u64)
        .then(|| "commands were sent recently".into())
}

/// Why the lead-up fetch must not use the link right now.
fn quiet_gate(state: &Arc<AppState>) -> Gate {
    let app = app_gate(state);
    let weak = Arc::downgrade(state);
    Arc::new(move || {
        if let Some(reason) = app() {
            return Some(reason);
        }
        if MANAGER.active() || MANAGER.listing_busy.load(Ordering::Acquire) {
            return Some("another SD transfer is running".into());
        }
        if super::launch_fetch_owed() {
            return Some("the DAQ launch file fetch goes first".into());
        }
        if let Some(state) = weak.upgrade()
            && super::fc::launch_file_owed(&crate::recording_export::directory(&state))
        {
            return Some("the FC launch file collection goes first".into());
        }
        command_quiet()
    })
}

/// First and last network-UTC timestamps of the complete rows in `bytes`
/// (the first partial line is skipped unless `from_start`).
pub(crate) fn block_times(bytes: &[u8], from_start: bool) -> (Option<i64>, Option<i64>) {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.split('\n');
    if !from_start {
        lines.next();
    }
    let mut first = None;
    let mut last = None;
    let complete: Vec<&str> = lines.collect();
    let n = complete.len();
    for (i, line) in complete.into_iter().enumerate() {
        // The last fragment may be cut by the block end.
        if i + 1 == n && !text.ends_with('\n') {
            break;
        }
        if let Some(ts) = board_sd::network_time(line.trim_end_matches('\r')) {
            first.get_or_insert(ts);
            last = Some(ts);
        }
    }
    (first, last)
}

struct Source<'a> {
    stream: &'a mut BoardStream,
    req: TransferRequest,
}

impl Source<'_> {
    /// `[offset, offset + length)` of the file (shorter at its end).
    async fn range(&mut self, offset: u64, length: u64, gate: &Gate) -> Result<Vec<u8>, StreamError> {
        read_range(self.stream, &self.req, offset, length, gate).await.map(|(bytes, _)| bytes)
    }
}

fn request(root: &FsPath, board: Board, dir_name: &str, name: &str, params: &Params) -> Option<TransferRequest> {
    let mut req = TransferRequest::new(0, board, dir_index(board, dir_name)?, name, root.join("sd-downloads"), params.rate);
    req.profile = params.profile;
    Some(req)
}

/// Bytes read by one binary-search probe (a few whole rows).
const PROBE_BYTES: u64 = 2 * BLOCK_BYTES;

/// Per-board fetch parameters.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Params {
    /// Reads are paced to this many DATA bytes per second.
    pub rate: u64,
    /// A whole ordinary file at or below this size is downloaded instead.
    pub whole_file_bytes: u64,
    /// Row bytes per millisecond at full rate (progress estimate only).
    pub bytes_per_ms: u64,
    pub profile: Profile,
}

impl Params {
    fn daq() -> Self {
        Self { rate: rate(), whole_file_bytes: small_file_bytes(), bytes_per_ms: 110, profile: Profile::WIRED }
    }
}

/// Runs every pending job once the link is quiet. One worker at a time.
pub(crate) async fn run(state: Arc<AppState>) {
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    struct Running;
    impl Drop for Running {
        fn drop(&mut self) {
            RUNNING.store(false, Ordering::Release);
        }
    }
    let _running = Running;
    let root = crate::recording_export::directory(&state);
    let gate = quiet_gate(&state);
    let deadline = Instant::now() + Duration::from_secs(6 * 3600);
    while Instant::now() < deadline {
        let Some(job) = load(&root).into_iter().find(|j| !j.done) else { return };
        if let Some(reason) = gate() {
            update(&root, "DAQ", &job.launch_file, |j| {
                j.phase = "paused".into();
                j.message = format!("paused: {reason}");
            });
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        let Some(router) = state.topology_router.get().cloned() else { return };
        match step(&MANAGER, &root, router, &job, &gate).await {
            Ok(()) => {}
            Err(StreamError::Gate(reason)) => {
                update(&root, "DAQ", &job.launch_file, |j| {
                    j.phase = "paused".into();
                    j.message = format!("paused: {reason}; resumes when allowed");
                });
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(error) => {
                let text = match error {
                    StreamError::Other(e) => e,
                    _ => "cancelled".into(),
                };
                update(&root, "DAQ", &job.launch_file, |j| {
                    j.phase = "retrying".into();
                    j.message = format!("{text}; retrying");
                });
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

/// One attempt at a DAQ job: find the source, then fetch (resuming) and finish.
pub(crate) async fn step(
    manager: &'static Manager,
    root: &FsPath,
    router: Arc<Router>,
    job: &Job,
    gate: &Gate,
) -> Result<(), StreamError> {
    step_with(manager, root, router, job, gate, Params::daq()).await
}

/// The ordinary files of the same card that may hold the lead-up, in the
/// order their tails are checked. The FC names its files by network UTC
/// seconds (`irec-<s>-<nnn>.csv`, uptime ms before UTC is known) and has one
/// per boot, so only the newest few candidates are read over the radio.
fn lead_up_candidates(board: Board, files: &[RemoteFile], to_ms: i64) -> Vec<RemoteFile> {
    use crate::recording_store::board_sd::classify_name;
    if board != Board::FlightComputer {
        return files.iter().filter(|f| f.dir != "LAUNCH" && f.size > 0).cloned().collect();
    }
    let mut out: Vec<(i64, usize, RemoteFile)> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.size > 0 && classify_name(&f.name) == Some(("FC", false)))
        .map(|(i, f)| {
            let utc_s = super::fc::name_utc_seconds(&f.name).filter(|s| *s * 1000 <= to_ms + 5_000);
            (utc_s.unwrap_or(-1), i, f.clone())
        })
        .collect();
    // Dated names newest first, then undated ones in reverse card order.
    out.sort_by_key(|e| std::cmp::Reverse((e.0, e.1)));
    out.into_iter().take(6).map(|(_, _, f)| f).collect()
}

/// One attempt at a job of any board.
pub(crate) async fn step_with(
    manager: &'static Manager,
    root: &FsPath,
    router: Arc<Router>,
    job: &Job,
    gate: &Gate,
    params: Params,
) -> Result<(), StreamError> {
    let board_name = job.board.clone();
    let board = board_of(&board_name);
    let label = board.sender_id();
    let folder = root.join(&job.capture);
    if !folder.join(crate::recording_store::MANIFEST_FILE).exists() {
        update(root, &board_name, &job.launch_file, |j| {
            j.done = true;
            j.phase = "dropped".into();
            j.message = "the capture was deleted".into();
        });
        return Ok(());
    }
    let mut job = job.clone();
    if job.source.is_none() {
        update(root, &board_name, &job.launch_file, |j| {
            j.phase = "searching".into();
            j.message = format!("finding the {label} session file that ended at the launch");
        });
        let listing = refresh_listing(manager, router.clone(), board, gate.clone())
            .await
            .map_err(StreamError::Other)?;
        let mut stream = BoardStream::open(manager, router.clone(), board, gate, &AtomicBool::new(false)).await?;
        let mut best: Option<(i64, RemoteFile)> = None;
        for file in lead_up_candidates(board, &listing.files, job.to_ms).iter() {
            let Some(req) = request(root, board, &file.dir, &file.name, &params) else { continue };
            let mut source = Source { stream: &mut stream, req };
            // The last kilobyte (to the end of the file as it is now).
            let tail = file.size.saturating_sub(2 * BLOCK_BYTES);
            let bytes = source.range(tail, 0, gate).await?;
            let (_, last) = block_times(&bytes, tail == 0);
            if let Some(last) = last
                && last >= job.from_ms
                && last <= job.to_ms + 5_000
                && best.as_ref().is_none_or(|(b, _)| last > *b)
            {
                best = Some((last, file.clone()));
            }
        }
        stream.finish(false);
        let Some((_, file)) = best else {
            update(root, &board_name, &job.launch_file, |j| {
                j.done = true;
                j.phase = "unavailable".into();
                j.message = format!("no {label} session file on the card covers the minute before launch");
            });
            return Ok(());
        };
        update(root, &board_name, &job.launch_file, |j| {
            j.source = Some(file.name.clone());
            j.source_dir = Some(file.dir.clone());
            j.source_size = file.size;
        });
        job.source = Some(file.name);
        job.source_dir = Some(file.dir);
        job.source_size = file.size;
    }
    let (Some(name), Some(dir_name)) = (job.source.clone(), job.source_dir.clone()) else { return Ok(()) };
    if job.source_size <= params.whole_file_bytes {
        // Small: the whole file, kept; the normal download path attaches it
        // (and its lead-up) to the capture when complete.
        update(root, &board_name, &job.launch_file, |j| {
            j.phase = "downloading".into();
            j.message = format!("downloading the whole {} ({} bytes)", name, j.source_size);
        });
        let downloads = root.join("sd-downloads");
        let started = start_job_at(manager, router, board, &dir_name, &name, downloads.clone(), gate.clone(), params.rate)
            .map_err(StreamError::Gate)?;
        while manager.get(started.id).is_some_and(|s| s.phase.is_active()) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let complete = completed_file(&downloads, label, &dir_name, &name);
        if complete.is_none() {
            return Err(StreamError::Gate("the whole-file download did not finish".into()));
        }
        finish(root, &job, &folder, complete.as_deref(), None)
    } else {
        fetch_window(manager, root, router, &job, &folder, &name, &dir_name, gate, params).await
    }
}

fn part_paths(root: &FsPath, board: &str, launch_file: &str) -> (PathBuf, PathBuf) {
    let base = dir(root, board).join(launch_file);
    (base.with_extension("header"), base.with_extension("window.part"))
}

#[expect(clippy::too_many_arguments, reason = "one job's owned transfer context")]
async fn fetch_window(
    manager: &'static Manager,
    root: &FsPath,
    router: Arc<Router>,
    job: &Job,
    folder: &FsPath,
    name: &str,
    dir_name: &str,
    gate: &Gate,
    params: Params,
) -> Result<(), StreamError> {
    let board_name = job.board.as_str();
    let board = board_of(board_name);
    let mut stream = BoardStream::open(manager, router, board, gate, &AtomicBool::new(false)).await?;
    let req = request(root, board, dir_name, name, &params).ok_or_else(|| StreamError::Other("bad SD folder".into()))?;
    let mut source = Source { stream: &mut stream, req };
    let (header_path, part_path) = part_paths(root, board_name, &job.launch_file);
    std::fs::create_dir_all(dir(root, board_name)).map_err(|e| e.to_string())?;
    // The CSV header and calibration lines that precede the first row.
    if !header_path.exists() {
        let mut header = String::new();
        let bytes = source.range(0, 16 * BLOCK_BYTES, gate).await?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        for line in text.split_inclusive('\n') {
            if !line.ends_with('\n') {
                break;
            }
            if board_sd::network_time(line.trim_end()).is_some() || (!line.starts_with('#') && !line.starts_with("timestamp_ms") && !line.trim().is_empty()) {
                break;
            }
            header.push_str(line);
        }
        std::fs::write(&header_path, header).map_err(|e| e.to_string())?;
    }
    // Binary search over single blocks for the window start.
    let start = match job.start_offset {
        Some(start) => start,
        None => {
            update(root, board_name, &job.launch_file, |j| {
                j.phase = "searching".into();
                j.message = format!("locating the lead-up window in {name}");
            });
            let target = job.from_ms - SEARCH_MARGIN_MS;
            let (mut lo, mut hi) = (0u64, job.source_size);
            while hi - lo > BLOCK_BYTES * 4 {
                let mid = (lo + (hi - lo) / 2) / BLOCK_BYTES * BLOCK_BYTES;
                let bytes = source.range(mid, PROBE_BYTES, gate).await?;
                match block_times(&bytes, mid == 0).0 {
                    Some(ts) if ts < target => lo = mid,
                    Some(_) => hi = mid,
                    // No dated row in this block: search the earlier half.
                    None => hi = mid,
                }
            }
            update(root, board_name, &job.launch_file, |j| j.start_offset = Some(lo));
            lo
        }
    };
    // Resume: the part file holds the bytes fetched so far from `start`.
    let mut have = std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
    if have != job.fetched {
        have = have.min(job.fetched);
        let file = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(&part_path).map_err(|e| e.to_string())?;
        file.set_len(have).map_err(|e| e.to_string())?;
    }
    let mut out = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part_path)
        .await
        .map_err(|e| e.to_string())?;
    let estimate = (job.to_ms - job.from_ms).max(1) as u64 * params.bytes_per_ms;
    // Stream from the kept offset until a row at or after the window end.
    let never = AtomicBool::new(false);
    let req = &source.req;
    if let Some(mut fetch) = Fetch::open(&mut *source.stream, req, start + have, 0, gate, &never).await? {
        loop {
            let bytes = match fetch.next().await {
                Ok(Some(bytes)) => bytes,
                Ok(None) => break,
                Err(error) => {
                    out.flush().await.map_err(|e| e.to_string())?;
                    update(root, board_name, &job.launch_file, |j| j.fetched = have);
                    return Err(error);
                }
            };
            out.write_all(&bytes).await.map_err(|e| e.to_string())?;
            have += bytes.len() as u64;
            let (_, last) = block_times(&bytes, false);
            let covered = last.is_some_and(|ts| ts >= job.to_ms);
            update(root, board_name, &job.launch_file, |j| {
                j.fetched = have;
                j.phase = "fetching".into();
                j.message = format!("{have} of about {estimate} bytes of the lead-up window");
            });
            if covered {
                break;
            }
        }
        fetch.close();
    }
    out.flush().await.map_err(|e| e.to_string())?;
    out.sync_all().await.map_err(|e| e.to_string())?;
    drop(out);
    stream.finish(false);
    // Header + window bytes form a CSV the lead-up cut reads like any file.
    let assembled = dir(root, board_name).join(format!("{}.window.csv", job.launch_file));
    {
        let mut file = std::fs::File::create(&assembled).map_err(|e| e.to_string())?;
        use std::io::Write;
        file.write_all(&std::fs::read(&header_path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        // Drop the partial first line of a mid-file start.
        let window = std::fs::read(&part_path).map_err(|e| e.to_string())?;
        let body = if start == 0 { &window[..] } else { window.iter().position(|b| *b == b'\n').map(|i| &window[i + 1..]).unwrap_or(&[]) };
        file.write_all(body).map_err(|e| e.to_string())?;
    }
    let result = finish(root, job, folder, Some(&assembled), Some((&header_path, &part_path)));
    let _ = std::fs::remove_file(&assembled);
    result
}

/// Cut the lead-up into the capture, update its manifest and close the job.
fn finish(
    root: &FsPath,
    job: &Job,
    folder: &FsPath,
    source: Option<&FsPath>,
    scratch: Option<(&FsPath, &FsPath)>,
) -> Result<(), StreamError> {
    let rows = match source {
        Some(path) => board_sd::cut_lead_up(folder, &job.board, &job.launch_file, path, job.from_ms, job.to_ms)
            .map_err(|e| StreamError::Other(e.to_string()))?,
        None => 0,
    };
    if let Some((header, part)) = scratch {
        let _ = std::fs::remove_file(header);
        let _ = std::fs::remove_file(part);
    }
    update(root, &job.board, &job.launch_file, |j| {
        j.done = true;
        j.phase = if rows > 0 { "complete" } else { "unavailable" }.into();
        j.message = if rows > 0 {
            format!("lead-up saved: {rows} rows")
        } else {
            "the session file holds no rows in the minute before launch".into()
        };
    });
    Ok(())
}
