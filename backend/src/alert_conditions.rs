//! Explicit application condition lifecycle. Event acknowledgment is deliberately absent.
//! No disk, network, threshold evaluation, interlock, or actuator operation lives here.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const HISTORY_PER_SEVERITY: usize = 500;
pub const MAX_KEYS: usize = 18;
const MAX_PENDING_JOURNAL: usize = 64;
const MAX_BOOTSTRAP_TRANSITIONS: usize = 64;
const MAX_BOOTSTRAP_DETAIL_BYTES: usize = 32 * 1024;
const MAX_ERROR_DETAIL_BYTES: usize = 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityKind {
    BootstrapHistoryIncomplete,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct IntegrityMarker {
    pub kind: IntegrityKind,
    pub reason: String,
}
impl IntegrityMarker {
    pub fn validate(&self) -> Result<(), String> {
        if self.reason.is_empty() || self.reason.len() > 1024 {
            Err("Invalid condition integrity marker".into())
        } else {
            Ok(())
        }
    }
}

/// A diagnostic history receipt, never board state/proof authority. Creation
/// scope fences a delayed restore result from another runtime/connection epoch.
#[derive(Clone, Debug)]
pub struct ReadyError {
    pub scope: String,
    pub key: Key,
    pub timestamp_ms: i64,
    pub message: String,
    pub condition: Option<ConditionLink>,
}
struct PendingError {
    timestamp_ms: i64,
    message: String,
}
struct StagedObservation {
    key: Key,
    state: ConditionState,
    message: String,
    at_ms: i64,
    error: Option<PendingError>,
    board_edge: Option<crate::board_abort::AlertEdge>,
}
struct BoardAbortBinding {
    episode: crate::board_abort::AlertEpisode,
    occurrence_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BoardKey {
    Gs,
    Rf,
    Fc,
    Pb,
    Gb,
    Ab,
    Vb,
    Daq,
}
impl BoardKey {
    pub fn from_sender(sender: &str) -> Option<Self> {
        Some(match sender {
            "GS" => Self::Gs,
            "RF" => Self::Rf,
            "FC" => Self::Fc,
            "PB" => Self::Pb,
            "GB" => Self::Gb,
            "AB" => Self::Ab,
            "VB" => Self::Vb,
            "DAQ" => Self::Daq,
            _ => return None,
        })
    }
    pub fn sender(self) -> &'static str {
        match self {
            Self::Gs => "GS",
            Self::Rf => "RF",
            Self::Fc => "FC",
            Self::Pb => "PB",
            Self::Gb => "GB",
            Self::Ab => "AB",
            Self::Vb => "VB",
            Self::Daq => "DAQ",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    BoardOffline(BoardKey),
    BoardAbort(BoardKey),
    GatewayLowVoltage,
    StationHeartbeatEnqueue,
}
impl Key {
    pub fn wire(self) -> String {
        match self {
            Self::BoardOffline(board) => format!("board_offline:{}", board.sender()),
            Self::BoardAbort(board) => format!("board_abort:{}", board.sender()),
            Self::GatewayLowVoltage => "gateway_low_voltage:GB".into(),
            Self::StationHeartbeatEnqueue => "station_heartbeat_enqueue".into(),
        }
    }
    pub fn from_wire(key: &str) -> Option<Self> {
        if let Some(source) = key.strip_prefix("board_offline:") {
            return BoardKey::from_sender(source).map(Self::BoardOffline);
        }
        if let Some(source) = key.strip_prefix("board_abort:") {
            return BoardKey::from_sender(source).map(Self::BoardAbort);
        }
        match key {
            "gateway_low_voltage:GB" => Some(Self::GatewayLowVoltage),
            "station_heartbeat_enqueue" => Some(Self::StationHeartbeatEnqueue),
            _ => None,
        }
    }
    fn severity(self) -> Severity {
        if matches!(self, Self::BoardAbort(_)) {
            Severity::Error
        } else {
            Severity::Warning
        }
    }
    fn source(self) -> &'static str {
        match self {
            Self::BoardOffline(board) | Self::BoardAbort(board) => board.sender(),
            Self::GatewayLowVoltage => "GB",
            Self::StationHeartbeatEnqueue => "station",
        }
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Warning,
    Error,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConditionState {
    Active,
    Resolved,
    Unknown,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Record {
    pub key: String,
    pub occurrence_id: String,
    pub authority: String,
    pub severity: Severity,
    pub source: String,
    pub message: String,
    pub state: ConditionState,
    pub raised_at_ms: i64,
    pub updated_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct KnownCondition {
    pub key: String,
    pub source: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConditionLink {
    pub condition_occurrence_id: String,
    pub condition_authority: String,
    pub condition_key: String,
    pub condition_source: String,
    pub condition_snapshot_authority: String,
    pub condition_revision: u64,
}
impl Record {
    pub fn link(&self, snapshot_authority: &str, revision: u64) -> ConditionLink {
        ConditionLink {
            condition_occurrence_id: self.occurrence_id.clone(),
            condition_authority: self.authority.clone(),
            condition_key: self.key.clone(),
            condition_source: self.source.clone(),
            condition_snapshot_authority: snapshot_authority.into(),
            condition_revision: revision,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Snapshot {
    pub version: u32,
    pub authority: String,
    pub revision: u64,
    pub complete: bool,
    pub evaluated: Vec<KnownCondition>,
    pub awaiting_evidence: Vec<String>,
    pub persistence_error: Option<String>,
    #[serde(default)]
    pub history_integrity: Option<IntegrityMarker>,
    pub records: Vec<Record>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct JournalEntry {
    pub authority: String,
    pub revision: u64,
    pub record: Record,
}
#[derive(Default)]
pub struct Change {
    pub changed: bool,
    /// Queue outside the registry lock. The existing DB writer must report
    /// success/failure using this token, not by emitting another alert.
    pub journal: Option<JournalEntry>,
}

pub struct Registry {
    authority: String,
    revision: u64,
    occurrence: u64,
    restored: bool,
    staged: VecDeque<StagedObservation>,
    staged_detail_bytes: usize,
    ready_errors: VecDeque<ReadyError>,
    bootstrap_incomplete: Option<IntegrityMarker>,
    integrity_persisted: bool,
    evaluated: BTreeSet<Key>,
    awaiting: BTreeSet<Key>,
    current: BTreeMap<Key, Record>,
    warnings: VecDeque<Record>,
    errors: VecDeque<Record>,
    pending: BTreeSet<u64>,
    persistence_error: Option<String>,
    board_edge_sequence: u64,
    board_abort_bindings: BTreeMap<BoardKey, BoardAbortBinding>,
    restored_board_occurrences: BTreeMap<BoardKey, String>,
    history_checkpoint_due: bool,
}
impl Registry {
    /// Expected keys must reflect the actually configured/running producers.
    /// Every key needs an affirmative first evaluation, including healthy ones.
    pub fn new(authority: String, expected: impl IntoIterator<Item = Key>) -> Self {
        let invalid = authority.is_empty() || authority.len() > 128;
        Self {
            authority,
            revision: 0,
            occurrence: 0,
            restored: false,
            staged: VecDeque::new(),
            staged_detail_bytes: 0,
            ready_errors: VecDeque::new(),
            bootstrap_incomplete: None,
            integrity_persisted: false,
            evaluated: BTreeSet::new(),
            awaiting: expected.into_iter().collect(),
            current: BTreeMap::new(),
            warnings: VecDeque::new(),
            errors: VecDeque::new(),
            pending: BTreeSet::new(),
            persistence_error: invalid.then(|| "Missing condition authority".into()),
            board_edge_sequence: 0,
            board_abort_bindings: BTreeMap::new(),
            restored_board_occurrences: BTreeMap::new(),
            history_checkpoint_due: false,
        }
    }
    fn advance(&mut self) {
        match self.revision.checked_add(1) {
            Some(next) => self.revision = next,
            None => self.persistence_error = Some("Condition revision exhausted".into()),
        }
    }
    fn retain_resolved(&mut self, record: Record) {
        let history = match record.severity {
            Severity::Warning => &mut self.warnings,
            Severity::Error => &mut self.errors,
        };
        history.push_back(record);
        while history.len() > HISTORY_PER_SEVERITY {
            history.pop_front();
        }
    }
    /// Called once with the journal's latest row for each occurrence, ordered
    /// by its durable insertion sequence. Active data from a previous process
    /// is unknown until a current producer confirms it; it never silently clears.
    pub fn restore(&mut self, rows: Vec<Record>) -> Result<Vec<JournalEntry>, String> {
        if self.restored {
            return Err("Condition journal already restored".into());
        }
        if rows.len() > HISTORY_PER_SEVERITY * 2 + MAX_KEYS {
            self.persistence_error = Some("Condition restore exceeds bounded history".into());
            self.advance();
            return Err("Condition restore exceeds bounded history".into());
        }
        let mut ids = BTreeSet::new();
        for record in &rows {
            if Key::from_wire(&record.key).is_none_or(|key| {
                record.source != key.source() || record.severity != key.severity()
            }) || record.authority.is_empty()
                || record.authority.len() > 128
                || !record
                    .occurrence_id
                    .starts_with(&format!("{}:", record.authority))
                || record.message.len() > 4096
                || record.occurrence_id.len() > 160
                || record.occurrence_id.is_empty()
                || !ids.insert(record.occurrence_id.clone())
                || (record.state == ConditionState::Resolved) != record.resolved_at_ms.is_some()
            {
                self.persistence_error = Some("Invalid condition journal".into());
                self.advance();
                return Err("Invalid condition journal".into());
            }
        }
        for mut record in rows {
            let key = Key::from_wire(&record.key).expect("validated key");
            if record.state == ConditionState::Resolved {
                self.retain_resolved(record);
            } else {
                if self.current.contains_key(&key) {
                    self.persistence_error =
                        Some("Conflicting unresolved condition occurrences".into());
                    self.advance();
                    return Err("Conflicting unresolved condition occurrences".into());
                }
                record.state = ConditionState::Unknown;
                record.resolved_at_ms = None;
                if let Key::BoardAbort(board) = key {
                    self.restored_board_occurrences.insert(board, record.occurrence_id.clone());
                }
                self.awaiting.insert(key);
                self.current.insert(key, record);
            }
        }
        self.restored = true;
        self.advance();
        // Replay original typed order. A late clear must not erase a transient
        // Active occurrence or merge a recurrence into a restored older fault.
        let staged = std::mem::take(&mut self.staged);
        self.staged_detail_bytes = 0;
        let mut journal = Vec::new();
        for observation in staged {
            if let Some(edge) = observation.board_edge {
                if let Some(entry) = self.observe_board_abort_edge(edge).journal { journal.push(entry); }
                continue;
            }
            let change = self.observe(
                observation.key,
                observation.state,
                observation.message,
                observation.at_ms,
            );
            if let Some(entry) = change.journal {
                journal.push(entry);
            }
            if let Some(error) = observation.error {
                let link = self.current_link(observation.key);
                self.ready_error(observation.key, error, link);
            }
        }
        Ok(journal)
    }
    pub fn restore_failed(&mut self, reason: String) {
        self.persistence_error = Some(reason);
        self.advance();
    }
    pub fn observe(
        &mut self,
        key: Key,
        state: ConditionState,
        message: String,
        at_ms: i64,
    ) -> Change {
        if !self.restored {
            return self.stage(key, state, message, at_ms, None);
        }
        let evaluated_changed = state != ConditionState::Unknown && self.evaluated.insert(key);
        let evidence_changed = self.awaiting.remove(&key) || evaluated_changed;
        if state == ConditionState::Resolved && !self.current.contains_key(&key) {
            if evidence_changed {
                self.advance();
            }
            return Change {
                changed: evidence_changed,
                journal: None,
            };
        }
        if self
            .current
            .get(&key)
            .is_some_and(|record| record.state == state && record.message == message)
        {
            if evidence_changed {
                self.advance();
            }
            return Change {
                changed: evidence_changed,
                journal: None,
            };
        }
        let mut record = if let Some(record) = self.current.remove(&key) {
            record
        } else {
            let Some(next) = self.occurrence.checked_add(1) else {
                self.persistence_error = Some("Condition occurrence identity exhausted".into());
                self.advance();
                return Change {
                    changed: true,
                    journal: None,
                };
            };
            self.occurrence = next;
            Record {
                key: key.wire(),
                occurrence_id: format!("{}:{next}", self.authority),
                authority: self.authority.clone(),
                severity: key.severity(),
                source: key.source().into(),
                message: String::new(),
                state,
                raised_at_ms: at_ms,
                updated_at_ms: at_ms,
                resolved_at_ms: None,
            }
        };
        record.state = state;
        record.message = message;
        record.updated_at_ms = at_ms;
        record.resolved_at_ms = (state == ConditionState::Resolved).then_some(at_ms);
        if state == ConditionState::Resolved {
            self.retain_resolved(record.clone());
        } else {
            self.current.insert(key, record.clone());
        }
        self.advance();
        if self.pending.len() < MAX_PENDING_JOURNAL {
            self.pending.insert(self.revision);
        } else {
            self.persistence_error = Some(
                "Condition journal backlog exceeded bounded tracking; checkpoint required".into(),
            );
        }
        Change {
            changed: true,
            journal: Some(JournalEntry {
                authority: self.authority.clone(),
                revision: self.revision,
                record,
            }),
        }
    }
    /// The caller supplies an existing typed source and original observation
    /// time. Only legacy history publication is deferred; raw board state is not.
    pub fn observe_error(
        &mut self,
        key: Key,
        condition_message: String,
        raised_at_ms: i64,
        timestamp_ms: i64,
        message: String,
    ) -> Change {
        let error = PendingError {
            timestamp_ms,
            message,
        };
        if key.severity() != Severity::Error
            || error.message.len() > MAX_ERROR_DETAIL_BYTES
            || condition_message.len() > 4096
        {
            self.incomplete(
                "Condition history incomplete: invalid/oversized typed error receipt".into(),
            );
            self.ready_error(key, PendingError { timestamp_ms,
                message: "Condition history incomplete: typed error details exceed the bounded receipt contract".into() }, None);
            return Change {
                changed: true,
                journal: None,
            };
        }
        if !self.restored {
            return self.stage(
                key,
                ConditionState::Active,
                condition_message,
                raised_at_ms,
                Some(error),
            );
        }
        let change = self.observe(key, ConditionState::Active, condition_message, raised_at_ms);
        let link = self.current_link(key);
        self.ready_error(key, error, link);
        change
    }
    pub(crate) fn board_abort_reporting_gap(&mut self, reason: &str) -> Change {
        let before = self.revision;
        self.incomplete(format!("Board Abort reporting history incomplete: {reason}"));
        Change { changed: self.revision != before, journal: None }
    }
    /// Exact process-local producer chronology, independent of current Launch
    /// readiness. Only the accepted BoardAbort observer creates these receipts.
    pub(crate) fn observe_board_abort_edge(&mut self, edge: crate::board_abort::AlertEdge) -> Change {
        use crate::board_abort::AlertEdgeKind;
        let staged_sequence = self.staged.iter().rev().find_map(|row| row.board_edge.map(|edge| edge.ordinal));
        let seen = staged_sequence.unwrap_or(self.board_edge_sequence);
        if edge.ordinal <= seen { return Change::default(); }
        if edge.ordinal == u64::MAX || edge.ordinal != seen.saturating_add(1)
            || edge.at_ms == 0 || i64::try_from(edge.at_ms).is_err()
            || !crate::board_abort::BOARDS.iter().any(|board| board.sender_id() == edge.board.sender()) {
            self.board_edge_sequence = self.board_edge_sequence.max(edge.ordinal);
            return self.board_abort_reporting_gap("invalid or missing ordered observer receipt");
        }
        if !self.restored {
            if self.staged.len() >= MAX_BOOTSTRAP_TRANSITIONS {
                return self.board_abort_reporting_gap("ordered observer bootstrap budget exceeded");
            }
            self.staged.push_back(StagedObservation {
                key: Key::BoardAbort(edge.board),
                state: if edge.kind == AlertEdgeKind::Raised { ConditionState::Active } else { ConditionState::Resolved },
                message: String::new(), at_ms: edge.at_ms as i64, error: None, board_edge: Some(edge),
            });
            self.advance();
            return Change { changed: true, journal: None };
        }
        self.board_edge_sequence = edge.ordinal;
        let key = Key::BoardAbort(edge.board);
        let label = crate::types::Board::from_sender_id(edge.board.sender()).map_or(edge.board.sender(), |board| board.as_str());
        match edge.kind {
            AlertEdgeKind::Raised => {
                let Some(episode) = edge.episode.filter(|episode| episode.ordinal == edge.ordinal
                    && episode.first_abort_ms != 0 && i64::try_from(episode.first_abort_ms).is_ok()) else {
                    return self.board_abort_reporting_gap("invalid raised episode identity");
                };
                if self.board_abort_bindings.contains_key(&edge.board)
                    || self.current.get(&key).is_some_and(|record| record.raised_at_ms != episode.first_abort_ms as i64) {
                    return self.board_abort_reporting_gap("raised episode conflicts with unresolved occurrence");
                }
                let change = self.observe_error(key, format!("{label} reports Abort"), episode.first_abort_ms as i64,
                    edge.at_ms as i64, format!("{label} reports Abort; new launch is disabled until a fresh actual exit report."));
                if let Some(record) = self.current.get(&key) {
                    self.board_abort_bindings.insert(edge.board, BoardAbortBinding { episode, occurrence_id: record.occurrence_id.clone() });
                    self.restored_board_occurrences.remove(&edge.board);
                }
                change
            }
            AlertEdgeKind::AcceptedExit => {
                let expected = if let Some(episode) = edge.episode {
                    self.board_abort_bindings.get(&edge.board)
                        .filter(|binding| binding.episode == episode)
                        .map(|binding| binding.occurrence_id.clone())
                } else if !self.board_abort_bindings.contains_key(&edge.board) {
                    self.restored_board_occurrences.get(&edge.board).cloned()
                } else { None };
                if let Some(expected) = expected {
                    if !self.current.get(&key).is_some_and(|record| record.occurrence_id == expected) {
                        return self.board_abort_reporting_gap("accepted exit no longer owns its exact occurrence");
                    }
                    let change = self.observe(key, ConditionState::Resolved, format!("{label} confirmed outside Abort"), edge.at_ms as i64);
                    self.board_abort_bindings.remove(&edge.board);
                    self.restored_board_occurrences.remove(&edge.board);
                    change
                } else if self.current.contains_key(&key) {
                    self.board_abort_reporting_gap("accepted exit does not identify current occurrence")
                } else {
                    // Affirmative validated evidence may evaluate an otherwise
                    // healthy key; absence/default masks never call this path.
                    self.observe(key, ConditionState::Resolved, format!("{label} confirmed outside Abort"), edge.at_ms as i64)
                }
            }
        }
    }
    fn stage(
        &mut self,
        key: Key,
        state: ConditionState,
        message: String,
        at_ms: i64,
        error: Option<PendingError>,
    ) -> Change {
        // Stable polling is not an occurrence and must not consume this queue.
        if error.is_none()
            && self
                .staged
                .iter()
                .rev()
                .find(|row| row.key == key)
                .is_some_and(|prior| prior.state == state && prior.message == message)
        {
            return Change {
                changed: false,
                journal: None,
            };
        }
        let bytes = message.len() + error.as_ref().map_or(0, |event| event.message.len());
        if message.len() > 4096
            || self.staged.len() >= MAX_BOOTSTRAP_TRANSITIONS
            || bytes > MAX_BOOTSTRAP_DETAIL_BYTES.saturating_sub(self.staged_detail_bytes)
        {
            self.incomplete(
                "Condition history incomplete: ordered startup receipt budget exceeded".into(),
            );
            // Retain the ordinary error history on a real resource failure. It
            // remains opaque/attention; the integrity flag prevents false Nominal.
            if let Some(error) = error {
                self.ready_error(key, error, None);
            }
            return Change {
                changed: true,
                journal: None,
            };
        }
        self.staged_detail_bytes += bytes;
        self.staged.push_back(StagedObservation {
            key,
            state,
            message,
            at_ms,
            error,
            board_edge: None,
        });
        self.advance();
        Change {
            changed: true,
            journal: None,
        }
    }
    fn incomplete(&mut self, reason: String) {
        if self.bootstrap_incomplete.is_none() {
            self.bootstrap_incomplete = Some(IntegrityMarker {
                kind: IntegrityKind::BootstrapHistoryIncomplete,
                reason,
            });
            self.integrity_persisted = false;
            self.advance();
        }
    }
    fn ready_error(&mut self, key: Key, error: PendingError, condition: Option<ConditionLink>) {
        if self.ready_errors.len() >= MAX_BOOTSTRAP_TRANSITIONS {
            self.incomplete("Condition history incomplete: ready receipt budget exceeded".into());
            return;
        }
        self.ready_errors.push_back(ReadyError {
            scope: self.authority.clone(),
            key,
            timestamp_ms: error.timestamp_ms,
            message: error.message,
            condition,
        });
    }
    pub(crate) fn authority(&self) -> &str { &self.authority }
    pub fn has_authority(&self, authority: &str) -> bool {
        self.authority == authority
    }
    pub fn restore_integrity(&mut self, marker: Option<IntegrityMarker>) -> Result<(), String> {
        if let Some(marker) = marker {
            marker.validate()?;
            self.bootstrap_incomplete = Some(marker);
            self.integrity_persisted = true;
            self.advance();
        }
        Ok(())
    }
    pub fn integrity_marker(&self) -> Option<IntegrityMarker> {
        self.bootstrap_incomplete.clone()
    }
    pub fn integrity_saved(&mut self, authority: &str) -> bool {
        if authority != self.authority || self.bootstrap_incomplete.is_none() {
            return false;
        }
        self.integrity_persisted = true;
        self.advance();
        true
    }
    pub fn integrity_failed(&mut self, authority: &str, reason: String) {
        if authority == self.authority {
            self.persistence_error = Some(reason);
            self.advance();
        }
    }
    pub fn take_ready_errors(&mut self) -> Vec<ReadyError> {
        self.ready_errors.drain(..).collect()
    }
    pub fn accepts_ready_error(&self, error: &ReadyError) -> bool {
        error.scope == self.authority
            && error.key.severity() == Severity::Error
            && error
                .condition
                .as_ref()
                .map_or(self.bootstrap_incomplete.is_some(), |link| {
                    link.condition_snapshot_authority == self.authority
                        && link.condition_revision <= self.revision
                        && link.condition_key == error.key.wire()
                        && link.condition_source == error.key.source()
                })
    }
    pub fn journal_succeeded(&mut self, authority: &str, revision: u64) -> bool {
        if authority != self.authority || !self.pending.remove(&revision) {
            return false;
        }
        self.advance();
        true
    }
    pub fn journal_failed(&mut self, authority: &str, revision: u64, reason: String) -> bool {
        if authority != self.authority || !self.pending.remove(&revision) {
            return false;
        }
        self.persistence_error = Some(reason);
        self.advance();
        true
    }
    /// Recovery requires a durable full snapshot at exactly the current revision.
    /// An old write completion cannot hide a later queue/write failure.
    pub fn checkpoint_succeeded(&mut self, authority: &str, revision: u64) -> bool {
        if authority != self.authority || revision != self.revision || !self.restored {
            return false;
        }
        self.pending.clear();
        self.persistence_error = None;
        self.history_checkpoint_due = false;
        if self.bootstrap_incomplete.is_some() {
            self.integrity_persisted = true;
        }
        self.advance();
        true
    }
    pub fn current_state(&self, key: Key) -> Option<ConditionState> {
        self.current.get(&key).map(|record| record.state)
    }
    pub fn current_link(&self, key: Key) -> Option<ConditionLink> {
        self.current
            .get(&key)
            .map(|record| record.link(&self.authority, self.revision))
    }
    pub fn needs_repair(&self) -> bool {
        self.persistence_error.is_some()
            || self.history_checkpoint_due
            || (self.bootstrap_incomplete.is_some() && !self.integrity_persisted)
    }
    /// Operator history clear. Only resolved occurrences leave; active and
    /// unknown conditions, abort bindings and integrity gaps are untouched.
    /// The journal keeps the old rows until the next checkpoint replaces them.
    pub fn clear_resolved_history(&mut self) -> Result<usize, String> {
        if !self.restored {
            return Err("Condition history is still loading; try again shortly".into());
        }
        let removed = self.warnings.len() + self.errors.len();
        if removed == 0 {
            return Ok(0);
        }
        self.warnings.clear();
        self.errors.clear();
        self.history_checkpoint_due = true;
        self.advance();
        Ok(removed)
    }
    pub fn resolved_history_len(&self) -> Option<usize> {
        self.restored
            .then(|| self.warnings.len() + self.errors.len())
    }
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            version: 1,
            authority: self.authority.clone(),
            revision: self.revision,
            complete: self.restored
                && !self.authority.is_empty()
                && self.authority.len() <= 128
                && self.awaiting.is_empty()
                && self.pending.is_empty()
                && self.persistence_error.is_none()
                && self.bootstrap_incomplete.is_none(),
            evaluated: self
                .evaluated
                .iter()
                .map(|key| KnownCondition {
                    key: key.wire(),
                    source: key.source().into(),
                })
                .collect(),
            awaiting_evidence: self.awaiting.iter().map(|key| key.wire()).collect(),
            persistence_error: self.persistence_error.clone().or_else(|| {
                self.bootstrap_incomplete
                    .as_ref()
                    .map(|marker| marker.reason.clone())
            }),
            history_integrity: self.bootstrap_incomplete.clone(),
            records: self
                .current
                .values()
                .chain(self.warnings.iter())
                .chain(self.errors.iter())
                .cloned()
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registry() -> Registry {
        let mut r = Registry::new("station-epoch".into(), [Key::GatewayLowVoltage]);
        r.restore(Vec::new()).unwrap();
        r
    }
    fn commit(r: &mut Registry, change: Change) {
        if let Some(entry) = change.journal {
            assert!(r.journal_succeeded(&entry.authority, entry.revision));
        }
    }
    #[test]
    fn first_evidence_and_journal_completion_are_required_for_complete_snapshot() {
        let mut r = registry();
        assert!(!r.snapshot().complete);
        let change = r.observe(
            Key::GatewayLowVoltage,
            ConditionState::Active,
            "Low voltage".into(),
            100,
        );
        assert!(!r.snapshot().complete);
        commit(&mut r, change);
        assert!(r.snapshot().complete);
        let id = r.snapshot().records[0].occurrence_id.clone();
        let change = r.observe(
            Key::GatewayLowVoltage,
            ConditionState::Resolved,
            "Voltage recovered".into(),
            200,
        );
        assert_eq!(r.snapshot().records[0].state, ConditionState::Resolved);
        assert!(!r.snapshot().complete);
        commit(&mut r, change);
        assert!(r.snapshot().complete);
        let change = r.observe(
            Key::GatewayLowVoltage,
            ConditionState::Active,
            "Low voltage again".into(),
            300,
        );
        commit(&mut r, change);
        assert_eq!(r.snapshot().records.len(), 2);
        assert_ne!(r.snapshot().records[0].occurrence_id, id);
    }
    #[test]
    fn restored_active_is_unknown_and_wrong_authority_cannot_confirm_persistence() {
        let mut old = registry();
        let c = old.observe(
            Key::GatewayLowVoltage,
            ConditionState::Active,
            "Low".into(),
            1,
        );
        commit(&mut old, c);
        let mut next = Registry::new("new-epoch".into(), []);
        next.restore(old.snapshot().records).unwrap();
        assert!(!next.snapshot().complete);
        assert_eq!(next.snapshot().records[0].state, ConditionState::Unknown);
        let c = next.observe(
            Key::GatewayLowVoltage,
            ConditionState::Resolved,
            "Recovered by current evidence".into(),
            2,
        );
        let entry = c.journal.unwrap();
        assert!(!next.journal_succeeded("old-epoch", entry.revision));
        assert!(!next.snapshot().complete);
        assert!(next.journal_succeeded("new-epoch", entry.revision));
        assert!(next.snapshot().complete);
    }
    #[test]
    fn producer_keys_do_not_clear_each_other_and_repeated_evidence_does_not_write_again() {
        let mut r = Registry::new("epoch".into(), []);
        r.restore(Vec::new()).unwrap();
        for key in [
            Key::BoardOffline(BoardKey::Daq),
            Key::BoardOffline(BoardKey::Vb),
        ] {
            let c = r.observe(key, ConditionState::Active, "Offline".into(), 1);
            commit(&mut r, c);
        }
        let c = r.observe(
            Key::BoardOffline(BoardKey::Daq),
            ConditionState::Resolved,
            "Online".into(),
            2,
        );
        commit(&mut r, c);
        assert_eq!(
            r.snapshot()
                .records
                .iter()
                .filter(|record| record.state == ConditionState::Active)
                .count(),
            1
        );
        let revision = r.snapshot().revision;
        assert!(
            !r.observe(
                Key::BoardOffline(BoardKey::Vb),
                ConditionState::Active,
                "Offline".into(),
                3
            )
            .changed
        );
        assert_eq!(r.snapshot().revision, revision);
    }
    #[test]
    fn clearing_history_drops_only_resolved_records_and_requires_a_durable_checkpoint() {
        let mut r = Registry::new("station-epoch".into(), [Key::GatewayLowVoltage]);
        assert!(
            r.clear_resolved_history().is_err(),
            "history still loading from the journal cannot be cleared"
        );
        r.restore(Vec::new()).unwrap();
        let abort = Key::BoardAbort(BoardKey::Fc);
        for (key, at) in [(abort, 10), (Key::GatewayLowVoltage, 20)] {
            let c = r.observe(key, ConditionState::Active, "Raised".into(), at);
            commit(&mut r, c);
            let c = r.observe(key, ConditionState::Resolved, "Cleared".into(), at + 1);
            commit(&mut r, c);
        }
        let c = r.observe(abort, ConditionState::Active, "Raised again".into(), 30);
        commit(&mut r, c);
        let c = r.observe(Key::StationHeartbeatEnqueue, ConditionState::Unknown, "Unconfirmed".into(), 31);
        commit(&mut r, c);
        assert!(!r.needs_repair());
        let before = r.snapshot().revision;

        assert_eq!(r.clear_resolved_history(), Ok(2));

        let snapshot = r.snapshot();
        assert!(snapshot.revision > before, "viewers must see a new revision");
        assert_eq!(snapshot.records.len(), 2);
        assert!(snapshot.records.iter().all(|record| record.state != ConditionState::Resolved));
        assert_eq!(r.current_state(abort), Some(ConditionState::Active));
        assert_eq!(r.current_state(Key::StationHeartbeatEnqueue), Some(ConditionState::Unknown));
        assert!(r.needs_repair(), "the cleared history must be checkpointed to the journal");
        let stale = snapshot.revision;
        let c = r.observe(abort, ConditionState::Active, "Raised again, new detail".into(), 32);
        commit(&mut r, c);
        assert!(!r.checkpoint_succeeded("station-epoch", stale));
        assert!(r.needs_repair());
        let current = r.snapshot().revision;
        assert!(r.checkpoint_succeeded("station-epoch", current));
        assert!(!r.needs_repair());
        assert_eq!(r.clear_resolved_history(), Ok(0));
    }
    #[test]
    fn resolved_history_is_bounded_without_evicting_active_or_hiding_journal_failure() {
        let mut r = registry();
        let c = r.observe(
            Key::GatewayLowVoltage,
            ConditionState::Active,
            "Low".into(),
            0,
        );
        commit(&mut r, c);
        for index in 0..550 {
            for state in [ConditionState::Active, ConditionState::Resolved] {
                let c = r.observe(
                    Key::StationHeartbeatEnqueue,
                    state,
                    "Enqueue state".into(),
                    index,
                );
                commit(&mut r, c);
            }
        }
        assert_eq!(r.snapshot().records.len(), HISTORY_PER_SEVERITY + 1);
        assert!(
            r.snapshot()
                .records
                .iter()
                .any(|record| record.key == Key::GatewayLowVoltage.wire()
                    && record.state == ConditionState::Active)
        );
        let c = r.observe(
            Key::GatewayLowVoltage,
            ConditionState::Resolved,
            "Recovered".into(),
            600,
        );
        let e = c.journal.unwrap();
        r.journal_failed(&e.authority, e.revision, "Queue refused transition".into());
        assert!(!r.snapshot().complete);
        let stale = r.snapshot().revision;
        r.observe(
            Key::StationHeartbeatEnqueue,
            ConditionState::Active,
            "Failed".into(),
            601,
        );
        assert!(!r.checkpoint_succeeded("station-epoch", stale));
        assert!(!r.snapshot().complete);
    }
    #[test]
    fn bounded_pending_and_failed_restore_never_appear_complete() {
        let mut r = registry();
        for index in 0..100 {
            r.observe(
                Key::GatewayLowVoltage,
                if index % 2 == 0 {
                    ConditionState::Active
                } else {
                    ConditionState::Resolved
                },
                "State".into(),
                index,
            );
        }
        assert!(r.pending.len() <= MAX_PENDING_JOURNAL);
        assert!(r.snapshot().persistence_error.is_some());
        assert!(!r.snapshot().complete);
        let mut bad = Registry::new("new".into(), []);
        bad.restore_failed("Read failed".into());
        assert!(!bad.snapshot().complete);
        assert_eq!(Key::from_wire("board_offline:unbounded-source"), None);
    }
    fn complete_entries(registry: &mut Registry, entries: Vec<JournalEntry>) {
        for entry in entries {
            assert!(registry.journal_succeeded(&entry.authority, entry.revision));
        }
    }
    #[test]
    fn bootstrap_transient_and_recurrence_bind_exact_occurrences_in_original_order() {
        let key = Key::BoardAbort(BoardKey::Daq);
        let mut prior = Registry::new("prior".into(), []);
        prior.restore(Vec::new()).unwrap();
        prior.observe(key, ConditionState::Active, "DAQ Abort".into(), 1234);
        let prior_id = prior.current_link(key).unwrap().condition_occurrence_id;
        let mut next = Registry::new("next".into(), []);
        next.observe_error(
            key,
            "DAQ Abort".into(),
            1234,
            2000,
            "Original Abort event".into(),
        );
        next.observe(key, ConditionState::Resolved, "DAQ clear".into(), 2100);
        next.observe_error(
            key,
            "DAQ Abort".into(),
            2200,
            2201,
            "Recurrence event".into(),
        );
        next.observe(key, ConditionState::Resolved, "DAQ clear".into(), 2300);
        let entries = next.restore(prior.snapshot().records).unwrap();
        let errors = next.take_ready_errors();
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].timestamp_ms, 2000);
        assert_eq!(errors[1].timestamp_ms, 2201);
        assert_eq!(
            errors[0]
                .condition
                .as_ref()
                .unwrap()
                .condition_occurrence_id,
            prior_id
        );
        assert_ne!(
            errors[1]
                .condition
                .as_ref()
                .unwrap()
                .condition_occurrence_id,
            prior_id
        );
        assert!(errors.iter().all(|error| next.accepts_ready_error(error)));
        complete_entries(&mut next, entries);
        let snapshot = next.snapshot();
        assert!(snapshot.complete);
        assert_eq!(snapshot.records.len(), 2);
        assert_eq!(snapshot.records[0].raised_at_ms, 1234);
        assert_eq!(snapshot.records[1].raised_at_ms, 2200);
        assert!(
            snapshot
                .records
                .iter()
                .all(|record| record.state == ConditionState::Resolved)
        );
    }
    #[test]
    fn first_ever_bootstrap_fault_cleared_before_restore_keeps_history_and_identity() {
        let key = Key::BoardAbort(BoardKey::Daq);
        let mut r = Registry::new("new".into(), []);
        r.observe_error(key, "Abort".into(), 10, 11, "Error".into());
        r.observe(key, ConditionState::Resolved, "Clear".into(), 12);
        let entries = r.restore(Vec::new()).unwrap();
        let errors = r.take_ready_errors();
        complete_entries(&mut r, entries);
        assert!(r.snapshot().complete);
        assert_eq!(r.snapshot().records[0].raised_at_ms, 10);
        assert_eq!(r.snapshot().records[0].resolved_at_ms, Some(12));
        assert_eq!(
            errors[0]
                .condition
                .as_ref()
                .unwrap()
                .condition_occurrence_id,
            r.snapshot().records[0].occurrence_id
        );
    }
    #[test]
    fn repeated_active_polls_do_not_exhaust_bootstrap_receipts_or_shift_first_time() {
        let key = Key::BoardAbort(BoardKey::Daq);
        let mut r = Registry::new("new".into(), []);
        r.observe_error(key, "Abort".into(), 10, 11, "Error".into());
        for time in 20..10020 {
            r.observe(key, ConditionState::Active, "Abort".into(), time);
        }
        assert_eq!(r.staged.len(), 1);
        assert!(r.staged_detail_bytes <= MAX_BOOTSTRAP_DETAIL_BYTES);
        let entries = r.restore(Vec::new()).unwrap();
        complete_entries(&mut r, entries);
        assert_eq!(r.snapshot().records[0].raised_at_ms, 10);
        assert_eq!(r.take_ready_errors().len(), 1);
    }
    #[test]
    fn overflow_latches_typed_integrity_and_checkpoint_or_restart_cannot_clear_gap() {
        let key = Key::BoardAbort(BoardKey::Daq);
        let mut r = Registry::new("new".into(), []);
        for n in 0..32 {
            r.observe_error(key, "Abort".into(), n * 10, n * 10 + 1, "Error".into());
            r.observe(key, ConditionState::Resolved, "Clear".into(), n * 10 + 2);
        }
        r.observe_error(
            key,
            "Abort".into(),
            1000,
            1001,
            "Overflow retained as opaque error".into(),
        );
        assert_eq!(r.staged.len(), 64);
        let fallback = r.take_ready_errors();
        assert_eq!(fallback.len(), 1);
        assert!(fallback[0].condition.is_none());
        let entries = r.restore(Vec::new()).unwrap();
        complete_entries(&mut r, entries);
        assert!(!r.snapshot().complete);
        let marker = r.snapshot().history_integrity.unwrap();
        assert_eq!(marker.kind, IntegrityKind::BootstrapHistoryIncomplete);
        let before = r.snapshot();
        assert!(r.checkpoint_succeeded(&before.authority, before.revision));
        assert!(!r.snapshot().complete);
        let mut restart = Registry::new("restart".into(), []);
        restart.restore_integrity(Some(marker.clone())).unwrap();
        restart.restore(r.snapshot().records).unwrap();
        restart.observe(key, ConditionState::Resolved, "Fresh clear".into(), 2000);
        assert!(!restart.snapshot().complete);
        assert_eq!(restart.snapshot().history_integrity, Some(marker));
    }
    #[test]
    fn bootstrap_detail_budget_is_independent_of_transition_count_and_utf8_content() {
        let key = Key::BoardAbort(BoardKey::Daq);
        let mut r = Registry::new("new".into(), []);
        for n in 0..9 {
            r.observe(
                key,
                if n % 2 == 0 {
                    ConditionState::Active
                } else {
                    ConditionState::Resolved
                },
                "é".repeat(2000),
                n,
            );
        }
        assert_eq!(r.staged.len(), 8);
        assert_eq!(r.staged_detail_bytes, 32000);
        assert!(r.snapshot().history_integrity.is_some());
        assert!(!r.snapshot().complete);
    }
    #[test]
    fn foreign_bootstrap_epoch_cannot_publish_a_correlated_error_in_new_runtime() {
        let key = Key::BoardAbort(BoardKey::Daq);
        let mut first = Registry::new("first".into(), []);
        first.restore(Vec::new()).unwrap();
        first.observe_error(key, "Abort".into(), 10, 11, "Error".into());
        let error = first.take_ready_errors().pop().unwrap();
        assert!(first.accepts_ready_error(&error));
        let mut other = Registry::new("other".into(), []);
        other.restore(first.snapshot().records).unwrap();
        assert!(!other.accepts_ready_error(&error));
        assert!(
            other
                .snapshot()
                .records
                .iter()
                .any(|record| record.state == ConditionState::Unknown)
        );
    }
}
