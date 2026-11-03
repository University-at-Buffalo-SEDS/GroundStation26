//! Observation-only sequence results. This module cannot queue commands or mutate an engine.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const VERSION: u16 = 1;
pub const MAX_ACTIVE: usize = 4;
pub const MAX_TERMINAL: usize = 32;
pub const MAX_STEPS: usize = 160;
pub const MAX_WATCH: usize = 8;
pub const MAX_ACTIVE_BYTES: usize = 32 * 1024;
pub const MAX_RESULT_BYTES: usize = 128 * 1024;
pub const MAX_MEMORY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunKey {
    pub backend_instance: String,
    pub reset_epoch: String,
    pub run_id: String,
}
impl RunKey {
    pub fn valid(&self) -> bool {
        fn hex(s: &str, n: usize) -> bool {
            s.len() == n && s.bytes().all(|c| c.is_ascii_hexdigit())
        }
        hex(&self.backend_instance, 32) && hex(&self.reset_epoch, 16) && hex(&self.run_id, 32)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    ValveSelfTest,
    NitrogenTest,
    Fill,
    Turbocharge,
    ContinueFillSequence,
    IgniterSequence,
}
impl Kind {
    pub fn has_gse_safe_cancel(self) -> bool {
        matches!(
            self,
            Self::ValveSelfTest | Self::NitrogenTest | Self::Fill | Self::Turbocharge
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Running,
    Paused,
    Cancelling,
    Passed,
    Failed,
    Cancelled,
    Interrupted,
    Unknown,
}
impl Outcome {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Passed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Planned,
    Current,
    Watching,
    Passed,
    Failed,
    Skipped,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    Planned,
    Commanded,
    ElectricalConfirmed,
    BoardReported,
    Measured,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Boolean(bool),
    Number(f64),
}
impl Value {
    fn finite(&self) -> bool {
        !matches!(self, Self::Number(v) if !v.is_finite())
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Watch {
    pub id: String,
    pub label: String,
    pub target: Option<Value>,
    pub observed: Option<Value>,
    pub unit: Option<String>,
    pub age_ms: Option<u64>,
    pub valid_for_ms: u64,
    pub evidence: Evidence,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub label: String,
    pub state: StepState,
    #[serde(default)]
    pub watch: Vec<Watch>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u16,
    pub key: RunKey,
    pub revision: u64,
    pub kind: Kind,
    pub outcome: Outcome,
    pub origin_client: Option<String>,
    pub title: String,
    pub message: String,
    pub current_step: Option<String>,
    pub next_step: Option<String>,
    pub steps: Vec<Step>,
    pub observed_age_ms: u64,
    pub valid_for_ms: u64,
    #[serde(default)]
    pub cancel_allowed: bool,
    #[serde(default)]
    pub notification_id: Option<u64>,
    #[serde(default)]
    pub notification_timestamp_ms: Option<i64>,
    #[serde(default)]
    pub reporting_error: Option<String>,
    #[serde(default)]
    pub watch_history_truncated: bool,
    #[serde(default = "complete_details")]
    pub details_complete: bool,
}
fn complete_details() -> bool {
    true
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Bounds,
    DuplicateId,
    Stale,
    RevisionGap,
    Conflict,
    NoCapacity,
    NotCurrent,
    Permission,
    Kind,
    Phase,
    Unavailable,
}
impl Snapshot {
    /// Compact authoritative terminal header; no synthetic step states.
    pub fn compact_result(&self) -> Self {
        Self {
            version: self.version,
            key: self.key.clone(),
            revision: self.revision,
            kind: self.kind,
            outcome: self.outcome,
            origin_client: self.origin_client.clone(),
            title: self.title.clone(),
            message: self.message.clone(),
            current_step: None,
            next_step: None,
            steps: Vec::new(),
            observed_age_ms: self.observed_age_ms,
            valid_for_ms: self.valid_for_ms,
            cancel_allowed: false,
            notification_id: self.notification_id,
            notification_timestamp_ms: self.notification_timestamp_ms,
            reporting_error: self.reporting_error.clone(),
            watch_history_truncated: self.watch_history_truncated,
            details_complete: false,
        }
    }
    fn memory_bytes(&self) -> usize {
        let mut bytes = std::mem::size_of::<Self>()
            + self.key.backend_instance.capacity()
            + self.key.reset_epoch.capacity()
            + self.key.run_id.capacity()
            + self.title.capacity()
            + self.message.capacity()
            + self.steps.capacity() * std::mem::size_of::<Step>();
        for value in [
            &self.origin_client,
            &self.current_step,
            &self.next_step,
            &self.reporting_error,
        ]
        .into_iter()
        .flatten()
        {
            bytes += value.capacity();
        }
        for step in &self.steps {
            bytes += step.id.capacity()
                + step.label.capacity()
                + step.watch.capacity() * std::mem::size_of::<Watch>();
            for watch in &step.watch {
                bytes += watch.id.capacity()
                    + watch.label.capacity()
                    + watch.unit.as_ref().map_or(0, String::capacity);
            }
        }
        bytes
    }
    pub fn validate(&self) -> Result<(), Error> {
        if !self.details_complete
            && (!self.outcome.terminal()
                || !self.steps.is_empty()
                || self.current_step.is_some()
                || self.next_step.is_some()
                || self.cancel_allowed)
        {
            return Err(Error::Invalid);
        }

        let label = |s: &str| !s.is_empty() && s.len() <= 96;
        if self.version != VERSION
            || !self.key.valid()
            || self.revision == 0
            || !label(&self.title)
            || self.message.len() > 512
            || self.steps.len() > MAX_STEPS
            || self.valid_for_ms == 0
            || self.valid_for_ms > 2000
            || self.origin_client.as_ref().is_some_and(|s| s.len() > 128)
            || self.reporting_error.as_ref().is_some_and(|s| s.len() > 512)
        {
            return Err(Error::Invalid);
        }
        let mut ids = std::collections::HashSet::new();
        for step in &self.steps {
            if !label(&step.id)
                || !label(&step.label)
                || !ids.insert(&step.id)
                || step.watch.len() > MAX_WATCH
            {
                return Err(Error::Invalid);
            }
            for watch in &step.watch {
                if !label(&watch.id)
                    || !label(&watch.label)
                    || watch.valid_for_ms > 2000
                    || watch.unit.as_ref().is_some_and(|s| s.len() > 24)
                    || watch.target.as_ref().is_some_and(|v| !v.finite())
                    || watch.observed.as_ref().is_some_and(|v| !v.finite())
                {
                    return Err(Error::Invalid);
                }
                if watch.observed.is_some()
                    && (watch.evidence == Evidence::Unknown
                        || watch.valid_for_ms == 0
                        || watch.age_ms.is_none_or(|age| age > watch.valid_for_ms))
                {
                    return Err(Error::Invalid);
                }
                if matches!(
                    watch.evidence,
                    Evidence::Measured | Evidence::ElectricalConfirmed | Evidence::BoardReported
                ) && (watch.observed.is_none()
                    || watch.valid_for_ms == 0
                    || watch.age_ms.is_none_or(|age| age > watch.valid_for_ms))
                {
                    return Err(Error::Invalid);
                }
            }
        }
        for id in [&self.current_step, &self.next_step].into_iter().flatten() {
            if !ids.contains(id) {
                return Err(Error::Invalid);
            }
        }
        if self.outcome.terminal() && self.cancel_allowed {
            return Err(Error::Invalid);
        }
        if self.cancel_allowed && !self.kind.has_gse_safe_cancel() {
            return Err(Error::Invalid);
        }
        let bytes = serde_json::to_vec(self).map_err(|_| Error::Invalid)?.len();
        let limit = if self.outcome.terminal() {
            MAX_RESULT_BYTES
        } else {
            MAX_ACTIVE_BYTES
        };
        if bytes > limit || self.memory_bytes() > MAX_MEMORY_BYTES {
            return Err(Error::Bounds);
        }
        Ok(())
    }
}

/// No engine/control locks are acquired here. Callers publish only after releasing them.
pub struct Journal {
    active: Vec<Snapshot>,
    terminal: VecDeque<Snapshot>,
}
impl Default for Journal {
    fn default() -> Self {
        Self {
            active: Vec::with_capacity(MAX_ACTIVE),
            terminal: VecDeque::with_capacity(MAX_TERMINAL),
        }
    }
}
impl Journal {
    /// Called only with the producer's actual accepted-start record. Never called by a GET/click.
    pub fn accepted_start(&mut self, snapshot: Snapshot) -> Result<(), Error> {
        snapshot.validate()?;
        if snapshot.revision != 1 || snapshot.outcome.terminal() {
            return Err(Error::Invalid);
        }
        if self
            .active
            .iter()
            .chain(self.terminal.iter())
            .any(|s| s.key == snapshot.key)
        {
            return Err(Error::DuplicateId);
        }
        if self.active.len() >= MAX_ACTIVE {
            return Err(Error::NoCapacity);
        }
        if self.memory_bytes() + snapshot.memory_bytes() > MAX_MEMORY_BYTES {
            return Err(Error::Bounds);
        }
        self.active.push(snapshot);
        Ok(())
    }
    /// Producer revisions include every real transition. A gap remains an observer error.
    pub fn observe(&mut self, snapshot: Snapshot) -> Result<(), Error> {
        snapshot.validate()?;
        let index = self
            .active
            .iter()
            .position(|s| s.key == snapshot.key)
            .ok_or(Error::NotCurrent)?;
        let previous = &self.active[index];
        if snapshot.kind != previous.kind || snapshot.origin_client != previous.origin_client {
            return Err(Error::Conflict);
        }
        if snapshot.revision == previous.revision {
            return if snapshot == *previous {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        if snapshot.revision < previous.revision {
            return Err(Error::Stale);
        }
        if snapshot.revision != previous.revision.saturating_add(1) {
            return Err(Error::RevisionGap);
        }
        let removed = previous.memory_bytes()
            + if snapshot.outcome.terminal() && self.terminal.len() == MAX_TERMINAL {
                self.terminal.front().map_or(0, Snapshot::memory_bytes)
            } else {
                0
            };
        if self.memory_bytes().saturating_sub(removed) + snapshot.memory_bytes() > MAX_MEMORY_BYTES
        {
            return Err(Error::Bounds);
        }
        if snapshot.outcome.terminal() {
            self.active.remove(index);
            if self.terminal.len() == MAX_TERMINAL {
                self.terminal.pop_front();
            }
            self.terminal.push_back(snapshot);
        } else {
            self.active[index] = snapshot;
        }
        Ok(())
    }
    pub fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .active
                .iter()
                .chain(self.terminal.iter())
                .map(Snapshot::memory_bytes)
                .sum::<usize>()
            + (self.active.capacity() - self.active.len() + self.terminal.capacity()
                - self.terminal.len())
                * std::mem::size_of::<Snapshot>()
    }
    pub fn active(&self) -> &[Snapshot] {
        &self.active
    }
    pub fn terminal(&self) -> &VecDeque<Snapshot> {
        &self.terminal
    }
    pub fn result(&self, key: &RunKey) -> Result<&Snapshot, Error> {
        self.terminal
            .iter()
            .find(|s| &s.key == key)
            .ok_or(Error::Unavailable)
    }
    pub fn bind_notification(
        &mut self,
        key: &RunKey,
        id: u64,
        timestamp_ms: i64,
    ) -> Result<(), Error> {
        if id == 0 {
            return Err(Error::Invalid);
        }
        let result = self
            .terminal
            .iter_mut()
            .find(|s| &s.key == key)
            .ok_or(Error::Unavailable)?;
        if result.notification_id.is_some_and(|old| old != id)
            || result
                .notification_timestamp_ms
                .is_some_and(|old| old != timestamp_ms)
        {
            return Err(Error::Conflict);
        }
        result.notification_id = Some(id);
        result.notification_timestamp_ms = Some(timestamp_ms);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelPhase {
    Baseline,
    SelfTest,
    NitrogenTest,
    Fill,
    FillTrim,
    Paused,
    Turbocharge,
    Cancelling,
    Terminal,
    Unknown,
}
/// Produced from the actual current runtime under existing admission ordering, not client JSON.
pub struct LiveCancelContext<'a> {
    pub key: &'a RunKey,
    pub kind: Kind,
    pub phase: CancelPhase,
    pub current_send_commands: bool,
    pub admission_epoch_current: bool,
}
/// Validation only. Integration must dispatch the existing safe cancel under the same fence.
pub fn validate_cancel(expected: &RunKey, live: &LiveCancelContext<'_>) -> Result<(), Error> {
    if !expected.valid() || expected != live.key {
        return Err(Error::NotCurrent);
    }
    if !live.current_send_commands || !live.admission_epoch_current {
        return Err(Error::Permission);
    }
    if !live.kind.has_gse_safe_cancel() {
        return Err(Error::Kind);
    }
    let phase_allowed = match live.kind {
        Kind::ValveSelfTest => matches!(live.phase, CancelPhase::Baseline | CancelPhase::SelfTest),
        Kind::NitrogenTest => matches!(
            live.phase,
            CancelPhase::Baseline | CancelPhase::NitrogenTest
        ),
        Kind::Fill => matches!(
            live.phase,
            CancelPhase::Fill | CancelPhase::FillTrim | CancelPhase::Paused
        ),
        Kind::Turbocharge => matches!(live.phase, CancelPhase::Turbocharge | CancelPhase::Paused),
        _ => false,
    };
    if !phase_allowed {
        return Err(Error::Phase);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(number: u64) -> Snapshot {
        Snapshot {
            version: VERSION,
            key: RunKey {
                backend_instance: "11".repeat(16),
                reset_epoch: "22".repeat(8),
                run_id: format!("{number:032x}"),
            },
            revision: 1,
            kind: Kind::NitrogenTest,
            outcome: Outcome::Running,
            origin_client: Some("operator-window-a".into()),
            title: "Nitrogen test".into(),
            message: "Watching fresh tank pressure".into(),
            current_step: Some("pressure-0".into()),
            next_step: Some("hold-0".into()),
            steps: vec![
                Step {
                    id: "pressure-0".into(),
                    label: "Raise to configured pressure".into(),
                    state: StepState::Watching,
                    watch: vec![Watch {
                        id: "tank-pressure".into(),
                        label: "Tank pressure".into(),
                        target: Some(Value::Number(50.)),
                        observed: Some(Value::Number(42.)),
                        unit: Some("psi".into()),
                        age_ms: Some(0),
                        valid_for_ms: 2000,
                        evidence: Evidence::Measured,
                    }],
                },
                Step {
                    id: "hold-0".into(),
                    label: "Verify pressure hold".into(),
                    state: StepState::Planned,
                    watch: vec![],
                },
            ],
            observed_age_ms: 0,
            valid_for_ms: 2000,
            cancel_allowed: true,
            notification_id: None,
            notification_timestamp_ms: None,
            reporting_error: None,
            watch_history_truncated: false,
            details_complete: true,
        }
    }
    #[test]
    fn actual_start_and_revision_are_required_without_guessing_a_run() {
        let mut journal = Journal::default();
        let first = snapshot(1);
        assert_eq!(journal.observe(first.clone()), Err(Error::NotCurrent));
        journal.accepted_start(first.clone()).unwrap();
        assert_eq!(
            journal.accepted_start(first.clone()),
            Err(Error::DuplicateId)
        );
        journal.observe(first.clone()).unwrap(); // Idempotent identical event.
        let mut conflict = first.clone();
        conflict.message = "Different same-revision data".into();
        assert_eq!(journal.observe(conflict), Err(Error::Conflict));
        let mut gap = first.clone();
        gap.revision = 3;
        assert_eq!(journal.observe(gap), Err(Error::RevisionGap));
        assert_eq!(journal.active()[0], first);
        let mut next = first;
        next.revision = 2;
        next.outcome = Outcome::Paused;
        journal.observe(next.clone()).unwrap();
        assert_eq!(journal.active()[0].key, next.key);
    }
    #[test]
    fn terminal_results_are_bounded_and_bind_actual_notification_identity() {
        let mut journal = Journal::default();
        let earliest = snapshot(1).key;
        for number in 1..=33 {
            let mut run = snapshot(number);
            journal.accepted_start(run.clone()).unwrap();
            run.revision = 2;
            run.outcome = Outcome::Cancelled;
            run.cancel_allowed = false;
            journal.observe(run.clone()).unwrap();
            journal
                .bind_notification(&run.key, number, 1000 + number as i64)
                .unwrap();
            assert_eq!(
                journal.result(&run.key).unwrap().notification_id,
                Some(number)
            );
            assert_eq!(
                journal.bind_notification(&run.key, number + 1, 1000),
                Err(Error::Conflict)
            );
        }
        assert_eq!(journal.result(&earliest), Err(Error::Unavailable));
        assert!(journal.memory_bytes() <= MAX_MEMORY_BYTES);
        assert!(journal.active().is_empty());
    }
    #[test]
    fn cancel_is_current_run_permission_phase_and_kind_bound() {
        let run = snapshot(1);
        let mut live = LiveCancelContext {
            key: &run.key,
            kind: Kind::NitrogenTest,
            phase: CancelPhase::NitrogenTest,
            current_send_commands: true,
            admission_epoch_current: true,
        };
        assert_eq!(validate_cancel(&run.key, &live), Ok(()));
        assert_eq!(
            validate_cancel(&snapshot(2).key, &live),
            Err(Error::NotCurrent)
        );
        let mut wrong_epoch = run.key.clone();
        wrong_epoch.reset_epoch = "33".repeat(8);
        assert_eq!(validate_cancel(&wrong_epoch, &live), Err(Error::NotCurrent));
        live.current_send_commands = false;
        assert_eq!(validate_cancel(&run.key, &live), Err(Error::Permission));
        live.current_send_commands = true;
        live.admission_epoch_current = false;
        assert_eq!(validate_cancel(&run.key, &live), Err(Error::Permission));
        live.admission_epoch_current = true;
        live.phase = CancelPhase::Terminal;
        assert_eq!(validate_cancel(&run.key, &live), Err(Error::Phase));
        live.phase = CancelPhase::NitrogenTest;
        live.kind = Kind::IgniterSequence;
        assert_eq!(validate_cancel(&run.key, &live), Err(Error::Kind));
        assert!(serde_json::from_str::<Kind>("\"launch\"").is_err());
    }
    #[test]
    fn invalid_stale_or_nonfinite_evidence_never_becomes_confirmed_zero() {
        let valid = snapshot(1);
        assert_eq!(valid.validate(), Ok(()));
        let mut invalid = valid.clone();
        invalid.steps[0].watch[0].observed = Some(Value::Number(f64::NAN));
        assert_eq!(invalid.validate(), Err(Error::Invalid));
        invalid = valid.clone();
        invalid.steps[0].watch[0].age_ms = Some(2001);
        assert_eq!(invalid.validate(), Err(Error::Invalid));
        invalid = valid.clone();
        invalid.steps[0].watch[0].observed = None;
        assert_eq!(invalid.validate(), Err(Error::Invalid));
        invalid.steps[0].watch[0].evidence = Evidence::Unknown;
        assert_eq!(invalid.validate(), Ok(()));
        let mut board = valid.clone();
        board.steps[0].watch[0].evidence = Evidence::BoardReported;
        board.steps[0].watch[0].age_ms = Some(2001);
        assert_eq!(board.validate(), Err(Error::Invalid));
        board.steps[0].watch[0].age_ms = Some(0);
        board.steps[0].watch[0].evidence = Evidence::Unknown;
        assert_eq!(board.validate(), Err(Error::Invalid));
        assert!(invalid.steps[0].watch[0].observed.is_none());
        invalid = valid.clone();
        invalid.steps.push(invalid.steps[0].clone());
        assert_eq!(invalid.validate(), Err(Error::Invalid));
        invalid = valid.clone();
        invalid.current_step = Some("not-an-observed-step".into());
        assert_eq!(invalid.validate(), Err(Error::Invalid));
    }
    #[test]
    fn memory_and_active_caps_do_not_evict_running_owners() {
        let mut journal = Journal::default();
        for id in 1..=4 {
            journal.accepted_start(snapshot(id)).unwrap();
        }
        assert_eq!(journal.accepted_start(snapshot(5)), Err(Error::NoCapacity));
        assert_eq!(journal.active().len(), 4);
        let mut oversized = snapshot(9);
        oversized.message = String::with_capacity(MAX_MEMORY_BYTES + 1);
        oversized
            .message
            .push_str("Small length, excessive reserved capacity");
        assert_eq!(oversized.validate(), Err(Error::Bounds));
        assert_eq!(journal.active()[0].key, snapshot(1).key);
    }
}
