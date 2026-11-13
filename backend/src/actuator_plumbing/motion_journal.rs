//! Durable original retraction intent. Disk effects belong only to the owned
//! blocking worker; this mutex is never held during filesystem IO.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
/// This is an original intent identity, not a wall-clock start estimate.
/// A record alone never authorizes Launch after process restart.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub(super) version: u8,
    pub(super) original_utc_ms: u64,
    pub(super) mode: u8,
    pub(super) mode_revision: u64,
    pub(super) boot: u64,
    pub(super) epoch: u32,
    pub(super) generation: u64,
    pub(super) manual_epoch: u64,
    pub(super) observed_manual_epoch: u64,
}
impl Record {
    pub(super) fn valid(&self) -> bool {
        self.version == 1
            && matches!(self.mode, 0 | 2)
            && !matches!(self.original_utc_ms, 0 | u64::MAX)
            && !matches!(self.boot, 0 | u64::MAX)
            && !matches!(self.epoch, 0 | u32::MAX)
            && !matches!(self.generation, 0 | u64::MAX)
            && self.manual_epoch != u64::MAX
            && self.observed_manual_epoch != u64::MAX
            && self.mode_revision != u64::MAX
    }
    pub(super) fn matches_motion(&self, status: super::motion_wire::MotionStatus) -> bool {
        self.valid()
            && status.completed_retract()
            && self.original_utc_ms == status.original_utc_ms
            && Some(self.mode) == status.setting.mode.map(|mode| mode as u8)
            && self.boot == status.setting.generation
            && self.epoch == status.setting.epoch
            && self.epoch == status.origin_epoch
    }
}
const MAX_BYTES: usize = 16 * 1024;
const MAX_ARCHIVES: usize = 8;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    sealed: bool,
    record: Option<Record>,
    sha256: String,
}
fn digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn encoded(record: Option<&Record>) -> Result<Vec<u8>, String> {
    if record.is_some_and(|r| !r.valid()) {
        return Err("Invalid original retraction intent".into());
    }
    let record = record.cloned();
    let bytes = serde_json::to_vec(&(1u8, true, &record)).map_err(|e| e.to_string())?;
    serde_json::to_vec(&Envelope {
        version: 1,
        sealed: true,
        record,
        sha256: digest(&bytes),
    })
    .map_err(|e| e.to_string())
}
fn decode(bytes: &[u8]) -> Result<Option<Record>, String> {
    let e: Envelope = serde_json::from_slice(bytes).map_err(|_| "Retraction history is invalid")?;
    let bytes = serde_json::to_vec(&(e.version, e.sealed, &e.record)).map_err(|e| e.to_string())?;
    if e.version != 1
        || !e.sealed
        || e.sha256 != digest(&bytes)
        || e.record.as_ref().is_some_and(|r| !r.valid())
    {
        return Err("Retraction history integrity check failed".into());
    }
    Ok(e.record)
}
fn read_bounded(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(
                "Retraction history cannot be read; restore readability before explicit reset"
                    .into(),
            );
        }
    };
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err("Retraction history exceeds its bounded archive size; preserve/export it before explicit reset".into());
    }
    Ok(Some(bytes))
}
fn fingerprint(bytes: Option<&[u8]>) -> String {
    let mut tagged = Vec::new();
    tagged.push(u8::from(bytes.is_some()));
    if let Some(bytes) = bytes {
        tagged.extend_from_slice(bytes);
    }
    digest(&tagged)
}
#[cfg(test)]
pub(super) fn load(path: &Path) -> Result<Option<Record>, String> {
    match read_bounded(path)? {
        Some(bytes) => decode(&bytes),
        None => Ok(None),
    }
}
#[cfg(test)]
fn cut(path: &Path, stage: u8) -> Result<(), String> {
    super::http_tests::motion_fault_cut(path, stage)
}
#[cfg(not(test))]
fn cut(_path: &Path, _stage: u8) -> Result<(), String> {
    Ok(())
}
pub(super) fn persist(path: &Path, record: Option<&Record>, nonce: u64) -> Result<(), String> {
    let bytes = encoded(record)?;
    #[cfg(test)]
    super::http_tests::save_hook(path, false)?;
    let directory = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let temporary = path.with_extension(format!("motion-{nonce:016x}.pending"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    cut(path, 3)?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    let observed = read_bounded(&temporary)?.ok_or("Prepared retraction history disappeared")?;
    if observed != bytes || decode(&observed)?.as_ref() != record {
        return Err("Prepared retraction history did not verify".into());
    }
    cut(path, 4)?;
    fs::rename(&temporary, path).map_err(|e| e.to_string())?;
    cut(path, 5)?;
    fs::File::open(directory)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| e.to_string())?;
    cut(path, 6)?;
    let observed = read_bounded(path)?.ok_or("Retraction history disappeared")?;
    if observed != bytes || decode(&observed)?.as_ref() != record {
        return Err("Retraction history readback did not match; Launch remains closed".into());
    }
    #[cfg(test)]
    super::http_tests::save_hook(path, true)?;
    Ok(())
}
struct State {
    record: Option<Record>,
    error: Option<String>,
    fingerprint: Option<String>,
    fault_revision: u64,
    bootstrap_unadmitted: bool,
}
pub(super) struct Journal {
    path: PathBuf,
    state: Mutex<State>,
    writer: Mutex<()>,
    pending: AtomicBool,
    sealed: AtomicBool,
}
#[derive(Clone, Serialize)]
pub(super) struct FaultContext {
    pub(super) fingerprint: Option<String>,
    pub(super) revision: u64,
    pub(super) resettable: bool,
}
impl Journal {
    pub(super) fn new(path: PathBuf) -> Self {
        let (raw, error) = match read_bounded(&path) {
            Ok(value) => (value, None),
            Err(error) => (None, Some(error)),
        };
        let present = raw.is_some();
        let (record, error) = if let Some(error) = error {
            (None, Some(error))
        } else if let Some(bytes) = raw.as_ref() {
            match decode(bytes) {
                Ok(record) => (record, None),
                Err(error) => (None, Some(error)),
            }
        } else {
            (None, None)
        };
        let fp = error
            .as_ref()
            .and(raw.as_ref().map(|bytes| fingerprint(Some(bytes))));
        let sealed = present || error.is_some();
        let revision = u64::from(error.is_some());
        Self {
            path,
            state: Mutex::new(State {
                record,
                error,
                fingerprint: fp,
                fault_revision: revision,
                bootstrap_unadmitted: false,
            }),
            writer: Mutex::new(()),
            pending: AtomicBool::new(false),
            sealed: AtomicBool::new(sealed),
        }
    }
    #[cfg(test)]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn sealed(&self) -> bool {
        self.sealed.load(Ordering::Acquire)
    }
    pub(super) fn reserve(&self) -> bool {
        if self.state.lock().unwrap().error.is_some() {
            return false;
        }
        self.pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub(super) fn reserve_repair(&self, revision: u64, fp: &str) -> bool {
        let state = self.state.lock().unwrap();
        if state.error.is_none()
            || state.fault_revision != revision
            || revision == u64::MAX
            || state.fingerprint.as_deref() != Some(fp)
        {
            return false;
        }
        self.pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub(super) fn finish(&self) {
        self.pending.store(false, Ordering::Release);
    }
    pub(super) fn pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
    pub(super) fn snapshot(&self) -> (Option<Record>, Option<String>) {
        let state = self.state.lock().unwrap();
        (state.record.clone(), state.error.clone())
    }
    pub(super) fn fault_context(&self) -> FaultContext {
        let state = self.state.lock().unwrap();
        FaultContext {
            fingerprint: state.fingerprint.clone(),
            revision: state.fault_revision,
            resettable: state.error.is_some()
                && state
                    .fingerprint
                    .as_ref()
                    .is_some_and(|fp| fp != &fingerprint(None) || state.bootstrap_unadmitted)
                && state.fault_revision != u64::MAX,
        }
    }
    pub(super) fn write_pending(&self, record: &Record) -> Result<(), String> {
        let _writer = self
            .writer
            .try_lock()
            .map_err(|_| "Retraction history writer busy")?;
        let was_unsealed = !self.sealed();
        let result = persist(&self.path, Some(record), rand::random::<u64>().max(1));
        let fp = if result.is_err() {
            read_bounded(&self.path)
                .ok()
                .map(|bytes| fingerprint(bytes.as_deref()))
        } else {
            None
        };
        self.sealed.store(true, Ordering::Release);
        let mut state = self.state.lock().unwrap();
        match &result {
            Ok(()) => state.record = Some(record.clone()),
            Err(error) => {
                state.error = Some(error.clone());
                state.fault_revision = state.fault_revision.saturating_add(1);
                state.bootstrap_unadmitted = was_unsealed
                    && state.record.is_none()
                    && fp.as_deref() == Some(&fingerprint(None));
                state.fingerprint = fp;
            }
        }
        result
    }
    /// Inspection never accepts externally changed data or clears a fault.
    pub(super) fn inspect_fault(&self) -> Result<(), String> {
        let _writer = self
            .writer
            .try_lock()
            .map_err(|_| "Retraction history writer busy")?;
        if self.state.lock().unwrap().error.is_none() {
            return Ok(());
        }
        let raw = read_bounded(&self.path)?;
        let fp = fingerprint(raw.as_deref());
        let mut state = self.state.lock().unwrap();
        state.fingerprint = Some(fp);
        state.fault_revision = state.fault_revision.saturating_add(1);
        Ok(())
    }
    /// Preserve/fsync bounded evidence before atomically replacing the original.
    /// The caller retains the owned reservation/lease and checks original Stop.
    pub(super) fn repair_closed(&self, revision: u64, expected: &str) -> Result<(), String> {
        let _writer = self
            .writer
            .try_lock()
            .map_err(|_| "Retraction history writer busy")?;
        let bootstrap = {
            let state = self.state.lock().unwrap();
            if state.error.is_none()
                || state.fault_revision != revision
                || state.fingerprint.as_deref() != Some(expected)
            {
                return Err("Retraction fault context changed; inspect and confirm again".into());
            }
            state.bootstrap_unadmitted
        };
        let raw = read_bounded(&self.path)?;
        if fingerprint(raw.as_deref()) != expected {
            return Err("Retraction history changed; inspect and confirm again".into());
        }
        if raw.is_none() && !bootstrap {
            return Err("Original retraction history is missing; restore preserved evidence before explicit reset".into());
        }
        let directory = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(directory).map_err(|e| e.to_string())?;
        let prefix = ".retraction-evidence-";
        let mut count = 0;
        let mut total = 0u64;
        for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_name().to_string_lossy().starts_with(prefix) {
                count += 1;
                total = total.saturating_add(entry.metadata().map_err(|e| e.to_string())?.len());
                if count >= MAX_ARCHIVES || total >= (MAX_ARCHIVES * MAX_BYTES) as u64 {
                    return Err("Retraction evidence retention is full; preserve/export it before another reset".into());
                }
            }
        }
        let archive = directory.join(format!(
            "{prefix}{:016x}{}",
            rand::random::<u64>(),
            if raw.is_none() {
                "-unadmitted-bootstrap"
            } else {
                ""
            }
        ));
        let mut evidence = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&archive)
            .map_err(|e| e.to_string())?;
        evidence.write_all(raw.as_deref().unwrap_or(b"No original record; this process proved zero SDK motion admission before the first failed write.")).map_err(|e|e.to_string())?;
        cut(&self.path, 0)?;
        evidence.sync_all().map_err(|e| e.to_string())?;
        drop(evidence);
        cut(&self.path, 1)?;
        fs::File::open(directory)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| e.to_string())?;
        cut(&self.path, 2)?;
        persist(&self.path, None, rand::random::<u64>().max(1))?;
        self.sealed.store(true, Ordering::Release);
        Ok(())
    }
    pub(super) fn publish_closed(&self) {
        let mut state = self.state.lock().unwrap();
        state.record = None;
        state.error = None;
        state.fingerprint = None;
        state.fault_revision = state.fault_revision.saturating_add(1);
        state.bootstrap_unadmitted = false;
        self.sealed.store(true, Ordering::Release);
    }
}
