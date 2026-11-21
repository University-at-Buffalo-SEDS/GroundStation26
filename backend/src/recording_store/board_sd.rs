//! Board SD launch files follow the GS launch capture automatically.
//!
//! FC (`irec_LAUNCH-*.csv`) and DAQ (`DAQ_LAUNCH_*.CSV`) launch files reach
//! the station by network download (DAQ, `sd_download`) or manual SD import
//! (`recording_sd`). Either way the file is matched to the GS launch capture
//! whose window overlaps its network-UTC rows, copied unchanged into
//! `<capture>/board_sd/<BOARD>/`, and listed in the capture's manifest.
//!
//! Boards switch to their launch file only when they learn of the launch
//! (the DAQ at its T0, the FC at the accepted Launch command), and neither has
//! RAM to buffer a minute of full-rate rows. The lead-up therefore comes from
//! the board's own ordinary file of the same boot: when it is available too,
//! the last [`LEAD_UP_MS`] before the launch file's first row is cut into
//! `<name>.leadup.csv` beside it.
use super::*;
use std::io::{BufRead, BufReader, Write};

/// Board lead-up kept before a launch file: covers arming, the countdown,
/// ignition and the pre-liftoff baseline (the GS capture keeps 10 min of the
/// network telemetry around it).
pub(crate) const LEAD_UP_MS: i64 = 60_000;
pub(crate) const BOARD_SD_DIR: &str = "board_sd";
/// Clock slack between a board's network UTC and the station's.
const MATCH_SLACK_MS: i64 = 5_000;
static MANIFEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSdEntry {
    pub board: String,
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
    /// First and last network-UTC row.
    pub data_from_ms: i64,
    pub data_to_ms: i64,
    pub rows: u64,
    /// `utc_window`: its network-UTC rows overlap the capture window.
    pub matched_by: String,
    /// `network_download` or `sd_import`.
    pub source: String,
    #[serde(default)]
    pub lead_up_file: Option<String>,
    #[serde(default)]
    pub lead_up_from_ms: Option<i64>,
    #[serde(default)]
    pub lead_up_rows: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoardFile {
    pub board: &'static str,
    pub launch: bool,
    pub first_utc: i64,
    pub last_utc: i64,
    /// Monotonic ms of the first/last network row (same boot when continuous).
    pub first_mono: i64,
    pub last_mono: i64,
    pub rows: u64,
}

/// FC files start with `irec`, DAQ files with `DAQ`; launch files carry
/// `_LAUNCH`. Anything else is not a board SD log.
pub(crate) fn classify_name(name: &str) -> Option<(&'static str, bool)> {
    let upper = name.to_ascii_uppercase();
    if !upper.ends_with(".CSV") {
        return None;
    }
    let board = if upper.starts_with("IREC") {
        "FC"
    } else if upper.starts_with("DAQ") {
        "DAQ"
    } else {
        return None;
    };
    Some((board, upper.contains("_LAUNCH")))
}

/// Fields 0 (`timestamp_ms`), 1 (`monotonic_ms`) and 7 (`time_source`) of a
/// board CSV row, honouring quoted fields.
fn row_fields(line: &str) -> Option<(i64, i64, bool)> {
    let mut fields: Vec<String> = Vec::with_capacity(8);
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                fields.push(std::mem::take(&mut current));
                if fields.len() == 8 {
                    break;
                }
            }
            _ => current.push(c),
        }
    }
    if fields.len() < 8 {
        fields.push(current);
    }
    let ts = fields.first()?.trim().parse::<i64>().ok()?;
    let mono = fields.get(1)?.trim().parse::<f64>().ok()? as i64;
    let network = fields.get(7).is_some_and(|f| f.trim() == "network");
    Some((ts, mono, network))
}

/// The network-UTC timestamp of one board CSV data row, if it has one.
pub(crate) fn network_time(line: &str) -> Option<i64> {
    if !data_line(line) {
        return None;
    }
    match row_fields(line) {
        Some((ts, _, true)) => Some(ts),
        _ => None,
    }
}

fn data_line(line: &str) -> bool {
    !line.is_empty() && !line.starts_with('#') && !line.starts_with("timestamp_ms")
}

/// The network-UTC span of a board CSV (None without network-UTC rows).
pub(crate) fn scan(path: &Path, name: &str) -> Option<BoardFile> {
    let (board, launch) = classify_name(name)?;
    let reader = BufReader::new(std::fs::File::open(path).ok()?);
    let mut span: Option<BoardFile> = None;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if !data_line(&line) {
            continue;
        }
        let Some((ts, mono, true)) = row_fields(&line) else { continue };
        let file = span.get_or_insert(BoardFile {
            board,
            launch,
            first_utc: ts,
            last_utc: ts,
            first_mono: mono,
            last_mono: mono,
            rows: 0,
        });
        file.first_utc = file.first_utc.min(ts);
        file.last_utc = file.last_utc.max(ts);
        file.last_mono = mono;
        file.rows += 1;
    }
    span
}

fn sha256_hex(path: &Path) -> std::io::Result<String> {
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
    Ok(context.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 96
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        && !name.starts_with('.')
}

/// The launch-type capture (launch or HITL, else ground test) whose window
/// best overlaps `[from, to]`.
pub(crate) fn matching_capture(root: &Path, from: i64, to: i64) -> Option<PathBuf> {
    let mut best: Option<(u8, i64, PathBuf)> = None;
    for (rank, kind) in [(2u8, EventKind::Launch), (2, EventKind::Hitl), (1, EventKind::GroundTest)] {
        for entry in std::fs::read_dir(events_dir(root).join(kind.id())).into_iter().flatten().flatten() {
            let folder = entry.path();
            let Some(manifest) = read_manifest(&folder) else { continue };
            let start = manifest.window_start_ms - MATCH_SLACK_MS;
            let end = manifest.window_end_ms + MATCH_SLACK_MS;
            let overlap = to.min(end) - from.max(start);
            if overlap < 0 {
                continue;
            }
            if best.as_ref().is_none_or(|(r, o, _)| (rank, overlap) > (*r, *o)) {
                best = Some((rank, overlap, folder));
            }
        }
    }
    best.map(|(_, _, folder)| folder)
}

/// Writes the rows of `ordinary` with network UTC in `[from, to)` (plus its
/// header and calibration lines) to `out`. Returns the row count.
fn write_lead_up(ordinary: &Path, out: &Path, from: i64, to: i64) -> std::io::Result<u64> {
    let reader = BufReader::new(std::fs::File::open(ordinary)?);
    let tmp = out.with_extension("partial");
    let mut writer = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    let mut rows = 0u64;
    for line in reader.lines() {
        let line = line?;
        if !data_line(&line) {
            writeln!(writer, "{line}")?;
            continue;
        }
        if let Some((ts, _, true)) = row_fields(&line)
            && ts >= from
            && ts < to
        {
            writeln!(writer, "{line}")?;
            rows += 1;
        }
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    std::fs::rename(tmp, out)?;
    Ok(rows)
}

fn update_manifest(folder: &Path, apply: impl FnOnce(&mut Manifest) -> bool) -> std::io::Result<bool> {
    let _guard = MANIFEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut manifest) = read_manifest(folder) else {
        return Err(std::io::Error::other("capture manifest missing"));
    };
    if !apply(&mut manifest) {
        return Ok(false);
    }
    write_atomic(&folder.join(MANIFEST_FILE), &serde_json::to_vec_pretty(&manifest).map_err(std::io::Error::other)?)?;
    Ok(true)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Saved into this capture folder (or already there).
    Attached(PathBuf),
    /// A lead-up was cut into this capture folder from an ordinary file.
    LeadUp(PathBuf),
    /// No launch capture covers the file's time (kept where it is).
    NoCapture,
    /// Not a board launch/ordinary file, or no network-UTC rows.
    Ignored,
}

/// Matches one board SD file (launch or ordinary) to the launch captures.
/// Blocking file work: call from `spawn_blocking`.
pub(crate) fn offer_file(root: &Path, path: &Path, name: &str, source: &str) -> std::io::Result<Vec<Outcome>> {
    if !safe_name(name) {
        return Ok(vec![Outcome::Ignored]);
    }
    let Some(file) = scan(path, name) else {
        return Ok(vec![Outcome::Ignored]);
    };
    if !file.launch {
        // An ordinary file supplies lead-ups for launch files already attached.
        let mut out = Vec::new();
        for folder in launch_folders(root) {
            if fill_lead_ups(&folder, file.board, Some((path, &file)))? {
                out.push(Outcome::LeadUp(folder));
            }
        }
        return Ok(if out.is_empty() { vec![Outcome::NoCapture] } else { out });
    }
    let Some(folder) = matching_capture(root, file.first_utc, file.last_utc) else {
        log::info!("board SD {name}: no launch capture covers {}..{}", file.first_utc, file.last_utc);
        return Ok(vec![Outcome::NoCapture]);
    };
    let board_dir = folder.join(BOARD_SD_DIR).join(file.board);
    std::fs::create_dir_all(&board_dir)?;
    let sha = sha256_hex(path)?;
    let target = board_dir.join(name);
    let already = read_manifest(&folder).is_some_and(|m| m.board_sd.iter().any(|e| e.sha256 == sha));
    if !already {
        let tmp = board_dir.join(format!(".{name}.partial"));
        std::fs::copy(path, &tmp)?;
        std::fs::File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, &target)?;
        let entry = BoardSdEntry {
            board: file.board.into(),
            file: name.into(),
            bytes: file_len(&target),
            sha256: sha.clone(),
            data_from_ms: file.first_utc,
            data_to_ms: file.last_utc,
            rows: file.rows,
            matched_by: "utc_window".into(),
            source: source.into(),
            lead_up_file: None,
            lead_up_from_ms: None,
            lead_up_rows: 0,
        };
        update_manifest(&folder, |m| {
            if m.board_sd.iter().any(|e| e.sha256 == sha) {
                return false;
            }
            m.board_sd.retain(|e| !(e.board == entry.board && e.file == entry.file));
            m.board_sd.push(entry);
            true
        })?;
        log::info!("board SD {name} attached to {}", folder.display());
    }
    // A lead-up from an ordinary file of the same board already on the station;
    // otherwise the DAQ's is fetched from its card in the background.
    fill_lead_ups(&folder, file.board, None)?;
    let has_lead_up = read_manifest(&folder)
        .is_some_and(|m| m.board_sd.iter().any(|e| e.board == file.board && e.file == name && e.lead_up_file.is_some()));
    if file.board == "DAQ" && !has_lead_up {
        crate::sd_download::leadup::enqueue(root, &folder, name, file.first_utc);
    }
    Ok(vec![Outcome::Attached(folder)])
}

fn launch_folders(root: &Path) -> Vec<PathBuf> {
    [EventKind::Launch, EventKind::Hitl, EventKind::GroundTest]
        .into_iter()
        .flat_map(|kind| std::fs::read_dir(events_dir(root).join(kind.id())).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|p| p.join(MANIFEST_FILE).exists())
        .collect()
}

/// Ordinary board files already on the station (network downloads).
fn local_ordinary_files(root: &Path, board: &str) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for dir in ["UTC", "NO_UTC"] {
        for entry in std::fs::read_dir(root.join("sd-downloads").join(board).join(dir)).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if classify_name(&name).is_some_and(|(b, launch)| b == board && !launch) {
                out.push((entry.path(), name));
            }
        }
    }
    out
}

/// For each attached launch file of `board` in `folder` without a lead-up,
/// cut one from `candidate` (or the station's local ordinary files) when it
/// covers the minute before the launch file on the same boot.
fn fill_lead_ups(folder: &Path, board: &str, candidate: Option<(&Path, &BoardFile)>) -> std::io::Result<bool> {
    let Some(manifest) = read_manifest(folder) else { return Ok(false) };
    let mut changed = false;
    for entry in manifest.board_sd.iter().filter(|e| e.board == board && e.lead_up_file.is_none()) {
        let from = entry.data_from_ms - LEAD_UP_MS;
        let to = entry.data_from_ms;
        let owned: Vec<(PathBuf, BoardFile)>;
        let sources: Vec<(&Path, &BoardFile)> = match candidate {
            Some(c) => vec![c],
            None => {
                let root = folder.ancestors().nth(4).map(Path::to_path_buf).unwrap_or_default();
                owned = local_ordinary_files(&root, board)
                    .into_iter()
                    .filter_map(|(p, n)| scan(&p, &n).map(|f| (p, f)))
                    .collect();
                owned.iter().map(|(p, f)| (p.as_path(), f)).collect()
            }
        };
        // Same boot: the ordinary file ends at or just before the launch file
        // starts, and its rows reach into the lead-up window.
        let Some((path, _)) = sources
            .into_iter()
            .find(|(_, f)| !f.launch && f.board == board && f.first_utc < to && f.last_utc >= from && f.last_utc <= to + MATCH_SLACK_MS)
        else {
            continue;
        };
        changed |= cut_lead_up(folder, board, &entry.file, path, from, to)? > 0;
    }
    Ok(changed)
}

/// Cuts the rows of `source` with network UTC in `[from, to)` into
/// `<launch file>.leadup.csv` beside the attached launch file and records it
/// in the manifest. `to` is the launch file's first row, so the lead-up and
/// the launch file never overlap. Returns the rows saved (an existing lead-up
/// is kept and its count returned).
pub(crate) fn cut_lead_up(folder: &Path, board: &str, launch_file: &str, source: &Path, from: i64, to: i64) -> std::io::Result<u64> {
    if let Some(existing) = read_manifest(folder)
        .and_then(|m| m.board_sd.into_iter().find(|e| e.board == board && e.file == launch_file))
        .filter(|e| e.lead_up_file.is_some())
    {
        return Ok(existing.lead_up_rows);
    }
    let lead_name = format!("{}.leadup.csv", launch_file.trim_end_matches(".csv").trim_end_matches(".CSV"));
    let out = folder.join(BOARD_SD_DIR).join(board).join(&lead_name);
    std::fs::create_dir_all(out.parent().unwrap_or(folder))?;
    let rows = write_lead_up(source, &out, from, to)?;
    if rows == 0 {
        let _ = std::fs::remove_file(&out);
        return Ok(0);
    }
    update_manifest(folder, |m| {
        let Some(e) = m.board_sd.iter_mut().find(|e| e.board == board && e.file == launch_file) else { return false };
        e.lead_up_file = Some(lead_name.clone());
        e.lead_up_from_ms = Some(from);
        e.lead_up_rows = rows;
        true
    })?;
    Ok(rows)
}

/// After a launch capture is saved: attach board launch files already on the
/// station (earlier network downloads) that fall in its window.
pub(crate) fn rescan_downloads(root: &Path) {
    for board in ["DAQ", "FC"] {
        for entry in std::fs::read_dir(root.join("sd-downloads").join(board).join("LAUNCH")).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Err(error) = offer_file(root, &entry.path(), &name, "network_download") {
                log::warn!("board SD {name}: {error}");
            }
        }
    }
}

/// Validated path of an attached board file (or its lead-up) in a capture.
pub(crate) fn attached_path(capture_db: &Path, board: &str, name: &str) -> Option<PathBuf> {
    if !matches!(board, "FC" | "DAQ") || !safe_name(name) {
        return None;
    }
    let path = capture_db.parent()?.join(BOARD_SD_DIR).join(board).join(name);
    std::fs::symlink_metadata(&path).ok().filter(|m| m.is_file()).map(|_| path)
}
