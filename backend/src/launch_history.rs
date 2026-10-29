//! Durable clock identity and non-actuating recovery observations.
use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

pub mod api;
#[path = "launch_history/writer.rs"]
mod writer;
pub use writer::Writer;
const MAX_EVENTS: usize = 4096;
const MAX_SESSIONS: usize = 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClockRecord {
    pub authority: String,
    pub revision: u64,
    pub session: String,
    pub clock: LaunchClockMsg,
    pub t0_utc_ms: Option<i64>,
}
impl ClockRecord {
    fn fresh() -> Self {
        Self {
            authority: format!("{:032x}", rand::random::<u128>()),
            revision: 0,
            session: format!("{:032x}", rand::random::<u128>()),
            clock: LaunchClockMsg::idle(),
            t0_utc_ms: None,
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            [&self.authority, &self.session]
                .iter()
                .all(|value| value.len() == 32 && value.bytes().all(|c| c.is_ascii_hexdigit())),
            "Invalid clock identity"
        );
        ensure!(
            self.t0_utc_ms == t0(&self.clock),
            "Clock anchor does not match T0"
        );
        ensure!(
            self.clock.duration_ms.is_none_or(|d| d >= 0),
            "Invalid countdown duration"
        );
        ensure!(
            self.clock.kind == LaunchClockKind::Idle || self.t0_utc_ms.is_some(),
            "Clock has no T0"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryEvent {
    pub id: String,
    pub launch_session: String,
    pub authority: String,
    pub clock_revision: u64,
    pub t0_utc_ms: i64,
    pub reached_utc_ms: i64,
    pub t_relative_ms: i64,
    pub actor: String,
    pub auth_session: String,
    pub station: String,
    pub primary_verified: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Disk {
    version: u8,
    current: ClockRecord,
    past_sessions: Vec<ClockRecord>,
    events: Vec<RecoveryEvent>,
    daq_clock: [u64; 2],
    #[serde(default)]
    daq_stamp: u64,
    #[serde(default)]
    legacy_anchor_unknown: bool,
    #[serde(default)]
    awaiting_new_launch: bool,
    #[serde(default)]
    new_cycle_confirmed: bool,
}

pub struct Store {
    path: Option<PathBuf>,
    writer: Option<Arc<Writer>>,
    last_write: u64,
    closed: bool,
    last_reset: Option<(ClockRecord, ClockRecord)>,
    data: Disk,
    observer: bool,
    pub synchronized: bool,
    pub error: Option<String>,
    born: Instant,
    baseline_mono_ms: u64,
    baseline_elapsed_ms: Option<i64>,
}
pub type Shared = Arc<Mutex<Store>>;

fn t0(clock: &LaunchClockMsg) -> Option<i64> {
    match clock.kind {
        LaunchClockKind::Idle => None,
        LaunchClockKind::TPlus => clock.anchor_timestamp_ms,
        LaunchClockKind::TMinus => clock
            .anchor_timestamp_ms
            .map(|at| at.saturating_add(clock.duration_ms.unwrap_or(0))),
    }
}
fn atomic_save(path: &Path, data: &Disk) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("clock-pending-{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(data)?;
    ensure!(
        bytes.len() <= 4 * 1024 * 1024,
        "Launch history file is full; export/archive before recording more events"
    );
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
impl Store {
    pub fn open(
        path: PathBuf,
        observer: bool,
        cached_daq: Option<([u64; 2], u64)>,
        now: i64,
        mono: u64,
    ) -> Result<Self> {
        let mut data = if path.exists() {
            let mut bytes = Vec::new();
            fs::File::open(&path)
                .context("Open saved launch clock")?
                .take(4 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .context("Read saved launch clock")?;
            ensure!(
                bytes.len() <= 4 * 1024 * 1024,
                "Launch history exceeds its bounded file size"
            );
            serde_json::from_slice::<Disk>(&bytes).context("Decode saved launch clock")?
        } else {
            Disk {
                version: 1,
                current: ClockRecord::fresh(),
                past_sessions: Vec::new(),
                events: Vec::new(),
                daq_clock: cached_daq.map_or([0, 0], |value| value.0),
                daq_stamp: cached_daq.map_or(0, |value| value.1),
                legacy_anchor_unknown: cached_daq.is_some_and(|value| value.0[0] != 0),
                awaiting_new_launch: false,
                new_cycle_confirmed: false,
            }
        };
        ensure!(
            data.version == 1
                && data.events.len() <= MAX_EVENTS
                && data.past_sessions.len() <= MAX_SESSIONS,
            "Invalid launch history version or size"
        );
        data.current.validate()?;
        if let Some((cached, stamp)) = cached_daq {
            data.daq_clock = cached;
            data.daq_stamp = stamp;
        }
        let baseline_elapsed_ms = data.current.t0_utc_ms.map(|t0| now.saturating_sub(t0));
        let writer = Writer::start(path.clone(), false)?;
        Ok(Self {
            path: Some(path),
            writer: Some(writer),
            last_write: 0,
            closed: false,
            last_reset: None,
            data,
            observer,
            synchronized: !observer,
            error: None,
            born: Instant::now(),
            baseline_mono_ms: mono,
            baseline_elapsed_ms,
        })
    }
    pub fn ephemeral() -> Self {
        Self {
            path: None,
            writer: None,
            last_write: 0,
            closed: false,
            last_reset: None,
            data: Disk {
                version: 1,
                current: ClockRecord::fresh(),
                past_sessions: Vec::new(),
                events: Vec::new(),
                daq_clock: [0, 0],
                daq_stamp: 0,
                legacy_anchor_unknown: false,
                awaiting_new_launch: false,
                new_cycle_confirmed: false,
            },
            observer: false,
            synchronized: true,
            error: None,
            born: Instant::now(),
            baseline_mono_ms: 0,
            baseline_elapsed_ms: None,
        }
    }
    pub fn environment() -> Result<Self> {
        let role = crate::rrls::is_rrls();
        let path = std::env::var_os("GS_LAUNCH_CLOCK_STATE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let station = crate::station_pairing::public()
                    .map(|pairing| {
                        let digest = ring::digest::digest(
                            &ring::digest::SHA256,
                            pairing.station_id.as_bytes(),
                        );
                        format!(
                            "-{}",
                            digest.as_ref()[..8]
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>()
                        )
                    })
                    .unwrap_or_default();
                PathBuf::from("backend/data").join(format!(
                    "launch-clock-{}{station}.json",
                    if role { "rrls" } else { "primary" }
                ))
            });
        let now = crate::telemetry_task::procedure_clock::timestamp_ms();
        let mono = crate::simulation_clock_ms().unwrap_or(0);
        match Self::open(
            path.clone(),
            role,
            crate::network_variables::persisted_daq_log_clock_with_revision().or_else(|| {
                crate::network_variables::persisted_daq_log_clock().map(|pair| (pair, 0))
            }),
            now,
            mono,
        ) {
            Ok(store) => Ok(store),
            Err(error) => {
                let mut store = Self::ephemeral();
                store.writer = Some(Writer::start(path.clone(), true)?);
                store.path = Some(path);
                store.observer = role;
                store.synchronized = false;
                store.data.legacy_anchor_unknown = true;
                store.error = Some(error.to_string());
                Ok(store)
            }
        }
    }
    pub fn retry_save(&mut self) -> Result<()> {
        ensure!(!self.closed, "Launch journal is shutting down");
        if let Some(writer) = &self.writer
            && writer.error().is_some()
        {
            self.last_write = writer.enqueue(self.data.clone())?;
        }
        Ok(())
    }
    pub fn clock_known(&self) -> bool {
        !self.data.legacy_anchor_unknown && self.error.is_none()
    }
    pub fn snapshot(&self) -> ClockRecord {
        self.data.current.clone()
    }
    /// Shutdown linearization: no later clock/recovery commit can escape this ticket.
    pub fn close_and_waiter(&mut self) -> Option<(Arc<Writer>, u64)> {
        self.closed = true;
        self.waiter()
    }
    pub fn waiter(&self) -> Option<(Arc<Writer>, u64)> {
        self.writer
            .as_ref()
            .map(|writer| (writer.clone(), self.last_write))
    }
    pub fn persistence_error(&self) -> Option<String> {
        self.error
            .clone()
            .or_else(|| self.writer.as_ref().and_then(|writer| writer.error()))
    }
    pub fn set_reporter(&self, reporter: Arc<dyn Fn(String) + Send + Sync>) {
        if let Some(writer) = &self.writer {
            writer.set_reporter(reporter);
        }
    }

    pub fn daq_snapshot(&self) -> ([u64; 2], u64) {
        (self.data.daq_clock, self.data.daq_stamp)
    }
    #[cfg(test)]
    pub fn daq_clock(&self) -> [u64; 2] {
        self.data.daq_clock
    }
    pub fn events(&self) -> Vec<RecoveryEvent> {
        self.data.events.clone()
    }
    pub fn monotonic_ms(&self, model_ms: Option<u64>) -> u64 {
        model_ms.unwrap_or_else(|| self.born.elapsed().as_millis().min(u64::MAX as u128) as u64)
    }
    pub fn elapsed(&self, mono: u64) -> Option<i64> {
        self.baseline_elapsed_ms.map(|elapsed| {
            elapsed.saturating_add(
                mono.saturating_sub(self.baseline_mono_ms)
                    .min(i64::MAX as u64) as i64,
            )
        })
    }
    fn commit(&mut self, next: Disk, now: i64, mono: u64) -> Result<()> {
        ensure!(!self.closed, "Launch journal is shutting down");
        next.current.validate()?;
        if let Some(writer) = &self.writer {
            self.last_write = writer.enqueue(next.clone())?;
        }
        if next.current.t0_utc_ms != self.data.current.t0_utc_ms
            || next.current.session != self.data.current.session
        {
            self.baseline_elapsed_ms = next.current.t0_utc_ms.map(|t0| now.saturating_sub(t0));
            self.baseline_mono_ms = mono;
        }
        self.data = next;
        self.error = None;
        Ok(())
    }
    /// A received prelaunch phase or an accepted new operator launch is evidence
    /// of a new cycle. Boot's default local Idle and repeated Launch are not.
    pub fn confirm_new_launch_cycle(&mut self, now: i64, mono: u64) -> Result<()> {
        if self.observer || !self.data.awaiting_new_launch || self.data.new_cycle_confirmed {
            return Ok(());
        }
        let mut next = self.data.clone();
        next.new_cycle_confirmed = true;
        // The accepted new cycle changes the reset contract even before pilot/T0.
        // An old delayed reset cannot cancel its pending DAQ recording window.
        next.current.revision = next
            .current
            .revision
            .checked_add(1)
            .context("Clock revision exhausted")?;
        self.commit(next, now, mono)
    }
    pub fn update_primary(
        &mut self,
        clock: LaunchClockMsg,
        now: i64,
        mono: u64,
    ) -> Result<ClockRecord> {
        ensure!(!self.observer, "Only the primary authors the launch clock");
        ensure!(
            !self.data.legacy_anchor_unknown,
            "Existing DAQ session has no saved T0; explicit Reset T clock is required before a new clock"
        );
        if self.data.awaiting_new_launch
            && !(clock.kind == LaunchClockKind::TMinus && self.data.new_cycle_confirmed)
        {
            return Ok(self.snapshot());
        }
        let clock = crate::state::monotonic_launch_clock_update(&self.data.current.clock, clock);
        if self.data.current.clock == clock {
            return Ok(self.snapshot());
        }
        let mut next = self.data.clone();
        next.current.revision = next
            .current
            .revision
            .checked_add(1)
            .context("Clock revision exhausted")?;
        next.current.t0_utc_ms = t0(&clock);
        if clock.kind == LaunchClockKind::TMinus {
            next.awaiting_new_launch = false;
        }
        next.current.clock = clock;
        self.commit(next, now, mono)?;
        Ok(self.snapshot())
    }
    pub fn reset_primary(&mut self, now: i64, mono: u64) -> Result<ClockRecord> {
        ensure!(!self.observer, "Only the primary resets the launch clock");
        ensure!(
            self.data.past_sessions.len() < MAX_SESSIONS,
            "Launch history is full; export and archive it before reset"
        );
        let expected = self.snapshot();
        let mut next = self.data.clone();
        next.past_sessions.push(next.current.clone());
        next.current = ClockRecord {
            authority: next.current.authority.clone(),
            revision: next
                .current
                .revision
                .checked_add(1)
                .context("Clock revision exhausted")?,
            ..ClockRecord::fresh()
        };
        next.daq_clock = [0, 0];
        next.legacy_anchor_unknown = false;
        next.awaiting_new_launch = true;
        next.new_cycle_confirmed = false;
        self.commit(next, now, mono)?;
        let result = self.snapshot();
        self.last_reset = Some((expected, result.clone()));
        Ok(result)
    }
    pub fn reset_expected_primary(
        &mut self,
        expected: &ClockRecord,
        now: i64,
        mono: u64,
    ) -> Result<ClockRecord> {
        if &self.data.current == expected {
            return self.reset_primary(now, mono);
        }
        ensure!(
            self.last_reset
                .as_ref()
                .is_some_and(|(old, result)| old == expected && result == &self.data.current),
            "Primary clock changed; stale reset request was rejected"
        );
        self.retry_save()?;
        Ok(self.snapshot())
    }
    pub fn retain_daq(
        &mut self,
        snapshot: [u64; 2],
        stamp: u64,
        now: i64,
        mono: u64,
    ) -> Result<()> {
        if self.data.daq_clock == snapshot && self.data.daq_stamp == stamp {
            return Ok(());
        }
        let mut next = self.data.clone();
        next.daq_clock = snapshot;
        next.daq_stamp = stamp;
        self.commit(next, now, mono)
    }
    pub fn apply_primary(
        &mut self,
        record: ClockRecord,
        current_primary_verified: bool,
        now: i64,
        mono: u64,
    ) -> Result<bool> {
        ensure!(
            self.observer && current_primary_verified,
            "A fresh verified primary snapshot is required"
        );
        record.validate()?;
        ensure!(
            !(record.authority != self.data.current.authority
                && record.revision == 0
                && record.clock.kind == LaunchClockKind::Idle
                && self.data.current.t0_utc_ms.is_some()),
            "Primary has no durable reset/T0; retain the saved clock until an explicit primary reset"
        );
        if record.authority == self.data.current.authority
            && record.revision < self.data.current.revision
        {
            return Ok(false);
        }
        if record == self.data.current {
            self.retry_save()?;
            self.synchronized = true;
            return Ok(false);
        }
        if record.authority == self.data.current.authority
            && record.revision == self.data.current.revision
        {
            anyhow::bail!("Conflicting primary clock revision");
        }
        let mut next = self.data.clone();
        if record.session != next.current.session && !next.current.session.is_empty() {
            ensure!(
                next.past_sessions.len() < MAX_SESSIONS,
                "Launch history is full"
            );
            next.past_sessions.push(next.current.clone());
        }
        next.current = record;
        next.legacy_anchor_unknown = false;
        self.commit(next, now, mono)?;
        self.synchronized = true;
        Ok(true)
    }
    pub fn mark_reached(
        &mut self,
        actor: &str,
        auth_session: &str,
        station: &str,
        now: i64,
        mono: u64,
    ) -> Result<RecoveryEvent> {
        ensure!(
            !actor.is_empty()
                && actor.len() <= 128
                && auth_session.len() <= 128
                && station.len() <= 128,
            "Invalid recovery actor or session"
        );
        if let Some(event) = self
            .data
            .events
            .iter()
            .find(|event| {
                event.launch_session == self.data.current.session && event.station == station
            })
            .cloned()
        {
            if !self.closed
                && let Some(writer) = &self.writer
                && writer.error().is_some()
            {
                self.last_write = writer.enqueue(self.data.clone())?;
            }
            return Ok(event);
        }
        ensure!(
            self.data.events.len() < MAX_EVENTS,
            "Recovery history is full; export and archive it first"
        );
        let t0 = self
            .data
            .current
            .t0_utc_ms
            .context("Launch clock has no T0")?;
        let event = RecoveryEvent {
            id: format!("{:032x}", rand::random::<u128>()),
            launch_session: self.data.current.session.clone(),
            authority: self.data.current.authority.clone(),
            clock_revision: self.data.current.revision,
            t0_utc_ms: t0,
            reached_utc_ms: now,
            t_relative_ms: self
                .elapsed(mono)
                .context("Launch clock has no elapsed value")?,
            actor: actor.into(),
            auth_session: auth_session.into(),
            station: station.into(),
            primary_verified: self.synchronized,
        };
        let mut next = self.data.clone();
        next.events.push(event.clone());
        self.commit(next, now, mono)?;
        Ok(event)
    }
}

/// Await a captured persistence ticket off the async/RX thread, without holding clock locks.
pub async fn flush(waiter: Option<(Arc<Writer>, u64)>) -> Result<()> {
    if let Some((writer, ticket)) = waiter {
        tokio::task::spawn_blocking(move || writer.wait(ticket)).await??;
    }
    Ok(())
}

#[cfg(test)]
#[path = "launch_history/tests.rs"]
mod tests;
