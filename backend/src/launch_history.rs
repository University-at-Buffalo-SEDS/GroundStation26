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

/// Durable fail-closed host recovery intent. Never contains an auth token.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryPending {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_policy: Option<ResetProofPolicy>,
    pub owner: u64,
    pub backend: String,
    pub generation: u64,
    pub manual_epoch: u64,
    pub observed_manual_epoch: u64,
    pub origin_utc_ms: u64,
    pub prior_automatic_actions_inhibited: bool,
    pub targets: Vec<RecoveryTarget>,
    pub verified: bool,
    pub published: bool,
}
/// Application journal evidence only; never restored live board readback.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResetProofPolicy {
    pub mode: crate::station_mode::Mode,
    pub required_mask: u8,
    pub verified_mask: u8,
}
impl ResetProofPolicy {
    fn valid_for(&self, targets: &[RecoveryTarget]) -> bool {
        // Journals from before the available-boards reset still carry a nonzero mask.
        self.required_mask & !0x7f == 0
            && self.verified_mask & !0x7f == 0
            && self.verified_mask & self.required_mask == self.required_mask
            && targets.len() == self.verified_mask.count_ones() as usize
            && targets.iter().all(|t| t.desired == 0 && board_bit(&t.board)
                .is_some_and(|bit| self.verified_mask & bit != 0))
    }
}
fn board_bit(board: &str) -> Option<u8> {
    ["RF", "FC", "PB", "AB", "VB", "GB", "DAQ"].iter()
        .position(|b| *b == board).map(|i| 1 << i)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryIntent {
    pub body: [u8; 32],
    pub source_utc_ms: u64,
    pub backend: String,
    pub principal_context: String,
}
impl RecoveryIntent {
    pub(crate) fn boot(&self) -> u64 {
        u64::from_le_bytes(self.body[12..20].try_into().unwrap())
    }
    pub(crate) fn nonce(&self) -> u64 {
        u64::from_le_bytes(self.body[4..12].try_into().unwrap())
    }
    fn validate(&self, board: &str) -> Result<()> {
        let epoch = u32::from_le_bytes(self.body[20..24].try_into().unwrap());
        ensure!(
            self.backend.len() == 32
                && self.principal_context.len() == 32
                && self.backend.bytes().all(|byte| byte.is_ascii_hexdigit())
                && self
                    .principal_context
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "Invalid sealed intent actor/backend identity"
        );
        let mut issuer_bytes = [0u8; 8];
        for (at, byte) in issuer_bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&self.backend[at * 2..at * 2 + 2], 16)?;
        }
        ensure!(
            self.body[24..32] == issuer_bytes,
            "Sealed intent issuer differs from original backend identity"
        );
        // RECOVER (1) targets Startup, or Idle on AB/VB. RESYNC (2) targets a
        // fill phase or Armed on AB/VB only, from a known boot generation.
        let resync = self.body[1] == 2
            && matches!(board, "AB" | "VB")
            && (2..=6).contains(&self.body[2])
            && self.boot() != u64::MAX;
        ensure!(
            self.body[0] == 1
                && (self.body[1] == 1 || resync)
                && self.body[3] == 0
                && (resync || self.body[2] == 0 || self.body[2] == 1 && matches!(board, "AB" | "VB"))
                && self.source_utc_ms != 0
                && self.nonce() != 0
                && self.boot() != 0
                && epoch != 0
                && epoch < u32::MAX - 1
                && u64::from_le_bytes(self.body[24..32].try_into().unwrap()) != 0,
            "Invalid sealed recovery request"
        );
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecoveryIntentKind {
    Normal,
    First,
    ReconcileFirst,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryTarget {
    pub board: String,
    pub desired: u8,
    pub boot: u64,
    pub epoch: u32,
    pub intent: RecoveryIntent,
    pub kind: RecoveryIntentKind,
    pub original_first: Option<RecoveryIntent>,
    pub uncertain_first: Option<RecoveryIntent>,
}
impl RecoveryPending {
    pub(crate) fn verified_startup_policy(&self) -> bool {
        self.verified && self.targets.iter().all(|t| t.desired == 0)
            && self.reset_policy.as_ref().map_or(self.targets.len() == 7,
                |p| p.mode == crate::station_mode::local_mode() && p.valid_for(&self.targets))
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !matches!(self.owner, 0 | u64::MAX)
                && !matches!(self.generation, 0 | u64::MAX)
                && self.manual_epoch != u64::MAX
                && self.observed_manual_epoch != u64::MAX
                && self.origin_utc_ms != 0
                && self.backend.len() == 32
                && self.backend.bytes().all(|c| c.is_ascii_hexdigit())
                && (!self.published || self.verified)
                && (self.reset_policy.as_ref().map_or((1..=7).contains(&self.targets.len()), |p| p.valid_for(&self.targets))),
            "Invalid pending recovery identity"
        );
        let mut seen = std::collections::HashSet::new();
        for target in &self.targets {
            ensure!(
                matches!(
                    target.board.as_str(),
                    "RF" | "FC" | "PB" | "AB" | "VB" | "GB" | "DAQ"
                ) && seen.insert(&target.board)
                    && target.boot != 0
                    && target.epoch != 0
                    && target.epoch < u32::MAX - 1
                    && (target.desired == 0
                        || (target.desired == 1 && matches!(target.board.as_str(), "AB" | "VB"))
                        || (target.intent.body[1] == 2 && target.kind == RecoveryIntentKind::Normal
                            && matches!(target.board.as_str(), "AB" | "VB"))),
                "Invalid pending recovery target"
            );
            target.intent.validate(&target.board)?;
            ensure!(
                target.intent.boot() == target.boot
                    && target.intent.body[2] == target.desired
                    && u32::from_le_bytes(target.intent.body[20..24].try_into().unwrap())
                        == target.epoch
                    && target.intent.source_utc_ms == self.origin_utc_ms
                    && target.intent.backend == self.backend,
                "Pending intent differs from frozen target/boundary"
            );
            for first in [&target.original_first, &target.uncertain_first]
                .into_iter()
                .flatten()
            {
                first.validate(&target.board)?;
                ensure!(
                    first.boot() == u64::MAX && first.source_utc_ms <= self.origin_utc_ms,
                    "Invalid retained FIRST lineage"
                );
            }
            ensure!(
                match target.kind {
                    RecoveryIntentKind::Normal => target.boot != u64::MAX,
                    RecoveryIntentKind::First =>
                        target.boot == u64::MAX && target.original_first.is_some(),
                    RecoveryIntentKind::ReconcileFirst =>
                        target.boot == u64::MAX
                            && target.original_first.is_some()
                            && target
                                .uncertain_first
                                .as_ref()
                                .is_some_and(|first| first.source_utc_ms < self.origin_utc_ms
                                    && first.nonce() != target.intent.nonce()),
                },
                "Invalid recovery lineage kind"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Disk {
    version: u8,
    #[serde(default, skip_serializing_if = "is_zero_manual_ceiling")]
    manual_abort_utc_ceiling: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovery_pending: Option<RecoveryPending>,
    /// Sticky across clock resets; only explicit board/startup recovery may clear it.
    #[serde(default)]
    automatic_actions_inhibited: bool,
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

fn is_zero_manual_ceiling(value: &u64) -> bool {
    *value == 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ManualUtcKey {
    pub owner: u64,
    pub generation: u64,
    pub boundary: u64,
    pub boots: [u64; 3], // AB, VB, FC
    pub epochs: [u32; 3],
    pub ceiling: u64,
    pub ticket: u64,
}
pub(crate) struct ManualUtcReceipt {
    key: ManualUtcKey,
    path: PathBuf,
    marker: RecoveryPending,
    writer: Arc<Writer>,
}
impl ManualUtcReceipt {
    pub(crate) fn wait_and_verify(&self) -> Result<()> {
        self.writer.wait(self.key.ticket)?;
        let mut bytes = Vec::new();
        fs::File::open(&self.path)?
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 4 * 1024 * 1024, "Journal exceeds limit");
        let disk: Disk = serde_json::from_slice(&bytes)?;
        ensure!(
            disk.version == 1
                && disk.manual_abort_utc_ceiling >= self.key.ceiling
                && disk.manual_abort_utc_ceiling != u64::MAX
                && disk.recovery_pending.as_ref() == Some(&self.marker),
            "Manual UTC reservation readback changed"
        );
        disk.current.validate()?;
        self.marker.validate()
    }
    pub(crate) fn key(&self) -> ManualUtcKey {
        self.key
    }
}

pub struct Store {
    analytics: crate::launch_analytics::Journal,
    path: Option<PathBuf>,
    writer: Option<Arc<Writer>>,
    last_write: u64,
    closed: bool,
    recovery_owner: u64,
    recovery_revision_staged: bool,
    recovery_activation_pending: bool,
    last_reset: Option<(ClockRecord, ClockRecord)>,
    data: Disk,
    observer: bool,
    pub synchronized: bool,
    pub error: Option<String>,
    born: Instant,
    baseline_mono_ms: u64,
    baseline_elapsed_ms: Option<i64>,
}
/// Captures the actual durable primary journal prefix and RAM close owner.
/// This fence alone is never permission to reset physical boards.
#[derive(Clone)]
pub(crate) struct RecoveryFence {
    owner: u64,
    ticket: u64,
    clock: ClockRecord,
    writer: Arc<Writer>,
    revision_staged: bool,
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
                manual_abort_utc_ceiling: 0,
                recovery_pending: None,
                automatic_actions_inhibited: false,
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
                && data.manual_abort_utc_ceiling != u64::MAX
                && data.events.len() <= MAX_EVENTS
                && data.past_sessions.len() <= MAX_SESSIONS,
            "Invalid launch history version or size"
        );
        data.current.validate()?;
        if let Some(pending) = &data.recovery_pending {
            pending.validate()?;
            ensure!(
                pending.published || data.automatic_actions_inhibited,
                "Pending recovery without inhibition"
            );
        }
        if let Some((cached, stamp)) = cached_daq {
            data.daq_clock = cached;
            data.daq_stamp = stamp;
        }
        let baseline_elapsed_ms = data.current.t0_utc_ms.map(|t0| now.saturating_sub(t0));
        let restored_owner = data
            .recovery_pending
            .as_ref()
            .filter(|p| !p.published)
            .map_or(0, |p| p.owner);
        let writer = Writer::start(path.clone(), false)?;
        Ok(Self {
            analytics: crate::launch_analytics::Journal::open(
                path.with_extension("analytics.json"),
            ),
            path: Some(path),
            writer: Some(writer),
            last_write: 0,
            closed: restored_owner != 0,
            recovery_owner: restored_owner,
            recovery_revision_staged: false,
            recovery_activation_pending: false,
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
            analytics: Default::default(),
            path: None,
            writer: None,
            last_write: 0,
            closed: false,
            recovery_owner: 0,
            recovery_revision_staged: false,
            recovery_activation_pending: false,
            last_reset: None,
            data: Disk {
                version: 1,
                manual_abort_utc_ceiling: 0,
                recovery_pending: None,
                automatic_actions_inhibited: false,
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
                log::error!("Launch clock journal {} could not be restored: {error:#}. Existing journal retained; automatic actions remain inhibited.", path.display());
                let mut store = Self::ephemeral();
                store.writer = Some(Writer::start(path.clone(), true)?);
                store.path = Some(path);
                store.observer = role;
                store.synchronized = false;
                store.data.legacy_anchor_unknown = true;
                // Unreadable launch history cannot safely re-arm automatic actions.
                store.data.automatic_actions_inhibited = true;
                store.error = Some(error.to_string());
                // An unreadable critical journal may contain a recovery that
                // partially reached boards. Keep UI and manual Stop live, but
                // never open ordinary admission or overwrite it with defaults.
                store.closed = true;
                store.recovery_owner = u64::MAX;
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
    pub fn automatic_actions_inhibited(&self) -> bool {
        self.recovery_activation_pending || self.data.automatic_actions_inhibited
            || self.data.recovery_pending.as_ref().is_some_and(|p| p.published && p.reset_policy.is_some() && !p.verified_startup_policy())
    }
    /// Save before admitting any irreversible launch command. A failed save
    /// still inhibits automation in RAM and prevents launch dispatch.
    pub fn inhibit_automatic_actions(&mut self) -> Result<()> {
        ensure!(!self.closed, "Launch journal is closed");
        self.data.automatic_actions_inhibited = true;
        if let Some(writer) = &self.writer {
            self.last_write = writer.enqueue(self.data.clone())?;
            writer.wait(self.last_write)?;
        }
        Ok(())
    }
    pub fn launch_action_utc_ms(&self) -> Option<i64> {
        self.analytics.action(&self.data.current)
    }
    pub fn snapshot(&self) -> ClockRecord {
        self.data.current.clone()
    }
    /// Shutdown linearization: no later clock/recovery commit can escape this ticket.
    pub fn close_and_waiter(&mut self) -> Option<(Arc<Writer>, u64)> {
        self.closed = true;
        {
            self.recovery_owner = 0;
            self.recovery_revision_staged = false;
        } // Shutdown supersedes recovery.
        self.waiter()
    }
    pub(crate) fn close_for_recovery(&mut self, owner: u64) -> Result<RecoveryFence> {
        ensure!(
            owner != 0 && owner != u64::MAX,
            "Recovery owner is unavailable"
        );
        ensure!(
            !self.closed || self.recovery_owner == owner,
            "Journal belongs to another close owner"
        );
        ensure!(
            self.data.automatic_actions_inhibited,
            "Automation must be inhibited before journal fencing"
        );
        ensure!(
            self.path.is_some(),
            "A durable journal is required for recovery"
        );
        let writer = self
            .writer
            .clone()
            .context("A journal writer is required for recovery")?;
        let fence = RecoveryFence {
            owner,
            ticket: self.last_write,
            clock: self.snapshot(),
            writer,
            revision_staged: self.closed && self.recovery_revision_staged,
        };
        if !self.closed {
            self.recovery_revision_staged = false;
        }
        self.closed = true;
        self.recovery_owner = owner;
        Ok(fence)
    }
    pub(crate) fn recovery_quiescent(&self, fence: &RecoveryFence) -> Result<bool> {
        ensure!(
            self.closed && self.recovery_owner == fence.owner,
            "Journal recovery owner changed"
        );
        ensure!(
            self.last_write == fence.ticket && self.data.current == fence.clock,
            "Journal capture changed"
        );
        ensure!(
            self.writer
                .as_ref()
                .is_some_and(|writer| Arc::ptr_eq(writer, &fence.writer)),
            "Journal writer changed"
        );
        fence.writer.quiescent_through(fence.ticket)
    }
    /// Replace a terminated supervisor's ticket without reopening the journal.
    /// An already queued writer retains the exact same snapshot and prefix;
    /// its eventual completion is still required before publication.
    pub(crate) fn transfer_recovery_owner(
        &mut self,
        fence: &RecoveryFence,
        old_owner: u64,
        new_owner: u64,
    ) -> Result<RecoveryFence> {
        ensure!(
            new_owner != 0 && new_owner != u64::MAX && new_owner != fence.owner,
            "Replacement recovery owner is unavailable"
        );
        ensure!(
            old_owner == fence.owner && self.closed && self.recovery_owner == fence.owner,
            "Journal recovery owner changed"
        );
        ensure!(
            self.last_write == fence.ticket
                && self.data.current == fence.clock
                && self.recovery_revision_staged == fence.revision_staged,
            "Journal capture changed"
        );
        ensure!(
            self.writer
                .as_ref()
                .is_some_and(|writer| Arc::ptr_eq(writer, &fence.writer)),
            "Journal writer changed"
        );
        self.recovery_owner = new_owner;
        Ok(RecoveryFence {
            owner: new_owner,
            ..fence.clone()
        })
    }
    pub(crate) fn recovery_publication_ready(
        &self,
        fence: &RecoveryFence,
        owner: u64,
    ) -> Result<bool> {
        ensure!(
            owner != 0 && owner == fence.owner,
            "Recovery admission owner changed"
        );
        ensure!(
            self.recovery_revision_staged && fence.revision_staged,
            "Recovery authority revision is not staged"
        );
        self.recovery_quiescent(fence)
    }
    /// Advance only the existing authority revision; never reset T0/session.
    /// Old paired reset intents then fail their existing expected-record check.
    #[cfg(test)]
    pub(crate) fn stage_recovery_revision(
        &mut self,
        fence: &RecoveryFence,
    ) -> Result<RecoveryFence> {
        ensure!(
            self.recovery_quiescent(fence)?,
            "Old journal prefix is still pending"
        );
        ensure!(
            !self.recovery_revision_staged && !fence.revision_staged,
            "Recovery revision already staged"
        );
        let mut next = self.data.clone();
        next.current.revision = next
            .current
            .revision
            .checked_add(1)
            .context("Clock revision exhausted")?;
        next.current.validate()?;
        let ticket = fence.writer.enqueue(next.clone())?;
        self.last_write = ticket;
        self.data = next;
        self.recovery_revision_staged = true;
        Ok(RecoveryFence {
            owner: fence.owner,
            ticket,
            clock: self.snapshot(),
            writer: fence.writer.clone(),
            revision_staged: true,
        })
    }
    /// Explicit retry of the same known in-memory snapshot, after terminal IO
    /// failure. Does not format, change time, or clear automatic inhibition.
    pub(crate) fn retry_recovery_save(&mut self, fence: &RecoveryFence) -> Result<RecoveryFence> {
        ensure!(
            self.closed && self.recovery_owner == fence.owner,
            "Recovery owner changed"
        );
        ensure!(
            self.last_write == fence.ticket && self.data.current == fence.clock,
            "Journal capture changed"
        );
        ensure!(
            self.writer
                .as_ref()
                .is_some_and(|writer| Arc::ptr_eq(writer, &fence.writer)),
            "Journal writer changed"
        );
        fence.writer.failed_terminal_prefix(fence.ticket)?;
        let ticket = fence.writer.enqueue(self.data.clone())?;
        self.last_write = ticket;
        Ok(RecoveryFence {
            ticket,
            ..fence.clone()
        })
    }
    /// Called with this Store owner retained from the nonwaiting durable-prefix
    /// check. No writer lock, file IO or callback under final board/Stop guard.
    pub(crate) fn reopen_after_recovery_ram(&mut self, fence: &RecoveryFence) -> Result<()> {
        ensure!(self.closed && self.recovery_owner == fence.owner
            && self.last_write == fence.ticket && self.data.current == fence.clock
            && self.recovery_revision_staged && fence.revision_staged,
            "Final journal capture changed");
        self.recovery_owner = 0;
        self.recovery_revision_staged = false;
        self.closed = false;
        Ok(())
    }
    pub(crate) fn recovery_publication_ready_nowait(&self, fence: &RecoveryFence, owner: u64) -> Result<bool> {
        ensure!(owner != 0 && owner == fence.owner && self.closed && self.recovery_owner == owner
            && self.last_write == fence.ticket && self.data.current == fence.clock
            && self.recovery_revision_staged && fence.revision_staged
            && self.writer.as_ref().is_some_and(|writer| Arc::ptr_eq(writer, &fence.writer)),
            "Final journal capture changed");
        fence.writer.try_quiescent_through(fence.ticket)
    }
    #[cfg(test)]
    pub(crate) fn reopen_after_recovery(&mut self, fence: &RecoveryFence) -> Result<()> {
        ensure!(
            self.recovery_quiescent(fence)?,
            "Journal writer is still pending"
        );
        ensure!(
            self.recovery_revision_staged && fence.revision_staged,
            "Recovery authority revision is not durable"
        );
        self.recovery_owner = 0;
        self.recovery_revision_staged = false;
        self.closed = false;
        // The separate board coordinator must authorize any later rearm.
        // Reopening the journal never clears the durable inhibition flag.
        Ok(())
    }
    /// Bounded RAM inspection only. The caller uses try_lock; no writer lock or I/O here.
    pub(crate) fn manual_utc_key(&self) -> Option<ManualUtcKey> {
        if self.closed
            || self.recovery_activation_pending
            || self.observer
            || self.error.is_some()
            || self.path.is_none()
            || self.writer.is_none()
        {
            return None;
        }
        let marker = self.data.recovery_pending.as_ref()?;
        if !marker.published || !marker.verified || marker.targets.len() != 7 {
            return None;
        }
        let mut seen = 0u8;
        let mut boots = [0; 3];
        let mut epochs = [0; 3];
        for target in &marker.targets {
            let (bit, physical) = match target.board.as_str() {
                "RF" => (0, None),
                "FC" => (1, Some(2)),
                "PB" => (2, None),
                "AB" => (3, Some(0)),
                "VB" => (4, Some(1)),
                "GB" => (5, None),
                "DAQ" => (6, None),
                _ => return None,
            };
            if seen & (1 << bit) != 0
                || target.desired != 0
                || matches!(target.boot, 0 | u64::MAX)
                || target.kind != RecoveryIntentKind::Normal
                || target.epoch >= u32::MAX - 1
            {
                return None;
            }
            seen |= 1 << bit;
            if let Some(index) = physical {
                boots[index] = target.boot;
                epochs[index] = target.epoch + 1;
            }
        }
        (seen == 0x7f).then_some(ManualUtcKey {
            owner: marker.owner,
            generation: marker.generation,
            boundary: marker.origin_utc_ms,
            boots,
            epochs,
            ceiling: self.data.manual_abort_utc_ceiling,
            ticket: self.last_write,
        })
    }
    /// Only an acknowledged, verified full-cohort recovery proves that an
    /// earlier application Abort can be retired. Pending/attempted timestamps
    /// and UTC reservations are not recovery boundaries.
    pub(crate) fn completed_abort_boundary(&self) -> Option<u64> {
        let boundary = self.manual_utc_key()?.boundary;
        (boundary != 0 && boundary != u64::MAX && self.writer.as_ref()?.try_healthy())
            .then_some(boundary)
    }
    pub(crate) fn manual_writer_settled(&self) -> Result<bool> {
        let writer = self.writer.as_ref().context("Writer unavailable")?;
        match writer.quiescent_through(self.last_write) {
            Ok(settled) => Ok(settled),
            Err(_) => {
                writer.failed_terminal_prefix(self.last_write)?;
                Ok(true)
            }
        }
    }
    pub(crate) fn retained_manual_utc_ceiling(&self) -> Result<u64> {
        ensure!(
            self.error.is_none()
                && self.path.is_some()
                && self.writer.is_some()
                && self.data.manual_abort_utc_ceiling != u64::MAX,
            "Durable manual UTC history unavailable"
        );
        Ok(self.data.manual_abort_utc_ceiling)
    }
    /// Serial Store owner max-merges every reservation; ordinary cloned snapshots retain it.
    pub(crate) fn reserve_manual_utc_ceiling(
        &mut self,
        expected: ManualUtcKey,
        ceiling: u64,
    ) -> Result<ManualUtcReceipt> {
        ensure!(
            self.manual_utc_key() == Some(expected),
            "Completed recovery cohort changed"
        );
        ensure!(
            ceiling != 0 && ceiling != u64::MAX && ceiling > expected.boundary,
            "Invalid manual UTC ceiling"
        );
        let writer = self.writer.clone().context("Writer unavailable")?;
        ensure!(writer.error().is_none(), "Journal writer failed");
        if ceiling > self.data.manual_abort_utc_ceiling {
            let mut next = self.data.clone();
            next.manual_abort_utc_ceiling = ceiling;
            let ticket = writer.enqueue(next.clone())?;
            self.data = next;
            self.last_write = ticket;
        }
        Ok(ManualUtcReceipt {
            key: self.manual_utc_key().context("Cohort lost")?,
            path: self.path.clone().context("Path missing")?,
            marker: self.data.recovery_pending.clone().unwrap(),
            writer,
        })
    }
    pub(crate) fn recovery_available(&self) -> bool {
        !self.observer
            && self.path.is_some()
            && self.writer.is_some()
            && self.error.is_none()
            && self.recovery_owner != u64::MAX
    }
    pub(crate) fn recovery_pending(&self) -> Option<RecoveryPending> {
        self.data.recovery_pending.clone()
    }
    /// Immutable evidence view while the caller retains the Store owner.
    /// Final publication must not clone its heap-backed board/intent data.
    pub(crate) fn recovery_pending_ref(&self) -> Option<&RecoveryPending> {
        self.data.recovery_pending.as_ref()
    }
    pub(crate) fn restored_recovery_barrier(&self) -> (u64, u64) {
        if self.error.is_some() {
            return (u64::MAX, 1);
        }
        self.data.recovery_pending.as_ref().map_or((0, 1), |p| {
            (if p.published { 0 } else { p.owner }, p.generation)
        })
    }
    /// Enqueue only; the owning coordinator waits without holding this mutex.
    pub(crate) fn inhibit_for_recovery(&mut self) -> Result<()> {
        ensure!(
            self.recovery_available(),
            "Healthy durable primary journal required"
        );
        self.data.automatic_actions_inhibited = true;
        if !self.closed {
            self.last_write = self.writer.as_ref().unwrap().enqueue(self.data.clone())?;
        }
        Ok(())
    }
    pub(crate) fn stage_recovery_pending(
        &mut self,
        fence: &RecoveryFence,
        pending: RecoveryPending,
    ) -> Result<RecoveryFence> {
        pending.validate()?;
        ensure!(
            pending.owner == fence.owner && !pending.verified && self.recovery_quiescent(fence)?,
            "Recovery prefix is not quiescent"
        );
        let mut next = self.data.clone();
        next.current.revision = next
            .current
            .revision
            .checked_add(1)
            .context("Clock revision exhausted")?;
        next.current.validate()?;
        next.recovery_pending = Some(pending);
        let ticket = fence.writer.enqueue(next.clone())?;
        self.last_write = ticket;
        self.data = next;
        self.recovery_revision_staged = true;
        Ok(RecoveryFence {
            owner: fence.owner,
            ticket,
            clock: self.snapshot(),
            writer: fence.writer.clone(),
            revision_staged: true,
        })
    }
    /// Retain the marker even after successful RAM publication. A process
    /// restart always requires a fresh explicit reconciliation, never replay.
    pub(crate) fn stage_recovery_verified(
        &mut self,
        fence: &RecoveryFence,
    ) -> Result<RecoveryFence> {
        ensure!(
            self.recovery_publication_ready(fence, fence.owner)?,
            "Recovery prefix pending"
        );
        let mut next = self.data.clone();
        let pending = next
            .recovery_pending
            .as_mut()
            .context("Pending recovery missing")?;
        ensure!(
            pending.owner == fence.owner && !pending.verified,
            "Recovery intent changed"
        );
        pending.verified = true;
        let ticket = fence.writer.enqueue(next.clone())?;
        self.data = next;
        self.last_write = ticket;
        Ok(RecoveryFence {
            ticket,
            ..fence.clone()
        })
    }
    pub(crate) fn recovery_durable_snapshot(
        &self,
        fence: &RecoveryFence,
    ) -> Result<(PathBuf, RecoveryPending)> {
        ensure!(
            self.recovery_publication_ready(fence, fence.owner)?,
            "Recovery prefix pending"
        );
        Ok((
            self.path.clone().context("Durable path missing")?,
            self.data
                .recovery_pending
                .clone()
                .context("Pending recovery missing")?,
        ))
    }
    pub(crate) fn verify_recovery_file(path: &Path, expected: &RecoveryPending) -> Result<()> {
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "Recovery journal exceeds limit"
        );
        let disk: Disk = serde_json::from_slice(&bytes)?;
        let expected_inhibited = !expected.published
            || !expected.verified_startup_policy()
                && expected.prior_automatic_actions_inhibited;
        ensure!(
            disk.version == 1
                && (!expected_inhibited || disk.automatic_actions_inhibited)
                && disk.recovery_pending.as_ref() == Some(expected),
            "Durable recovery marker mismatch"
        );
        disk.current.validate()?;
        expected.validate()
    }
    /// Called ONLY after original-Stop-linearized RAM publication. Later
    /// journal snapshots retain this acknowledgement; no T0/session reset.
    pub(crate) fn acknowledge_recovery_publication(
        &mut self,
        owner: u64,
    ) -> Result<(PathBuf, RecoveryPending, Arc<Writer>, u64)> {
        ensure!(
            !self.closed && self.recovery_owner == 0,
            "RAM publication has not reopened"
        );
        let mut next = self.data.clone();
        let marker = next
            .recovery_pending
            .as_mut()
            .context("Recovery marker missing")?;
        ensure!(
            marker.owner == owner && marker.verified && !marker.published,
            "Publication owner changed"
        );
        marker.published = true;
        let expected = marker.clone();
        next.automatic_actions_inhibited = if expected.verified_startup_policy()
        {
            false
        } else {
            expected.prior_automatic_actions_inhibited
        };
        let writer = self.writer.clone().context("Writer missing")?;
        let ticket = writer.enqueue(next.clone())?;
        self.data = next;
        self.last_write = ticket;
        self.recovery_activation_pending = true;
        Ok((
            self.path.clone().context("Path missing")?,
            expected,
            writer,
            ticket,
        ))
    }
    pub(crate) fn activate_acknowledged_recovery(&mut self, owner: u64) -> Result<()> {
        ensure!(
            !self.closed
                && self.recovery_activation_pending
                && self
                    .data
                    .recovery_pending
                    .as_ref()
                    .is_some_and(|marker| marker.owner == owner
                        && marker.verified
                        && marker.published),
            "Publication activation owner changed"
        );
        self.recovery_activation_pending = false;
        Ok(())
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
        let launch_action = if clock.kind == LaunchClockKind::TMinus {
            clock.anchor_timestamp_ms
        } else {
            None
        };
        next.current.clock = clock;
        self.commit(next, now, mono)?;
        if let Some(anchor) = launch_action {
            self.analytics.record(&self.data.current, anchor);
        }
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

#[cfg(test)]
#[path = "launch_history/reset_policy_tests.rs"]
mod reset_policy_tests;
#[cfg(test)]
#[path = "launch_history/resync_journal_tests.rs"]
mod resync_journal_tests;
