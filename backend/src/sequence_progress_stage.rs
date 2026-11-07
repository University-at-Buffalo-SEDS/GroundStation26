//! Small bounded owner observations only. Projection/JSON/publication is a
//! separate post-control operation; no clock lookup, journal lock or snapshot.
use crate::{
    sequence_progress::{Error, Kind, Outcome, RunKey},
    sequence_progress_producer::{Producer, Publication},
    sequence_progress_projection::{Cause, TerminalProof},
};
#[cfg(any(test, feature = "hitl_mode"))]
use gse_sequence::Inputs;
use gse_sequence::{NitrogenStep, Phase, ProgressObservation, Sample};
const CAP: usize = 16;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    instance: u128,
    epoch: u64,
    run: u128,
}
impl Owner {
    fn parse(k: &RunKey) -> Option<Self> {
        Some(Self {
            instance: u128::from_str_radix(&k.backend_instance, 16).ok()?,
            epoch: u64::from_str_radix(&k.reset_epoch, 16).ok()?,
            run: u128::from_str_radix(&k.run_id, 16).ok()?,
        })
    }
    fn key(self) -> RunKey {
        RunKey {
            backend_instance: format!("{:032x}", self.instance),
            reset_epoch: format!("{:016x}", self.epoch),
            run_id: format!("{:032x}", self.run),
        }
    }
}
#[derive(Clone, Copy)]
struct Text<const N: usize> {
    bytes: [u8; N],
    len: usize,
}
impl<const N: usize> Text<N> {
    fn new(s: &str) -> Self {
        let mut n = s.len().min(N);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        let mut bytes = [0; N];
        bytes[..n].copy_from_slice(&s.as_bytes()[..n]);
        Self { bytes, len: n }
    }
    fn string(self) -> String {
        String::from_utf8_lossy(&self.bytes[..self.len]).into_owned()
    }
}
#[derive(Clone, Copy)]
struct PlanStep {
    target: f32,
    settle: u64,
    hold: u64,
}
const EMPTY_STEP: PlanStep = PlanStep {
    target: 0.,
    settle: 0,
    hold: 0,
};
#[derive(Clone, Copy)]
enum Proof {
    Durable(Kind, Text<128>),
    Safe(ProgressObservation),
    Stopped(ProgressObservation),
    Interrupted,
    Failed(Text<512>),
    #[cfg(any(test, feature = "hitl_mode"))]
    Engine(Kind, ProgressObservation, ProgressObservation),
    #[cfg(any(test, feature = "hitl_mode"))]
    Trim(gse_sequence::fill_trim::ProgressObservation, Inputs),
}
impl Proof {
    fn capture(p: TerminalProof) -> Self {
        match p {
            TerminalProof::DurablePass { kind, event_id } => {
                Self::Durable(kind, Text::new(&event_id))
            }
            TerminalProof::SafeCancelled(v) => Self::Safe(v),
            TerminalProof::SafeStopped(v) => Self::Stopped(v),
            TerminalProof::Interrupted => Self::Interrupted,
            TerminalProof::Failed { message } => Self::Failed(Text::new(&message)),
            #[cfg(any(test, feature = "hitl_mode"))]
            TerminalProof::EngineCompleted {
                kind,
                before,
                after,
            } => Self::Engine(kind, before, after),
            #[cfg(any(test, feature = "hitl_mode"))]
            TerminalProof::VerifiedFillTrim { trim, input } => Self::Trim(trim, input),
        }
    }
    fn expand(self) -> TerminalProof {
        match self {
            Self::Durable(kind, id) => TerminalProof::DurablePass {
                kind,
                event_id: id.string(),
            },
            Self::Safe(v) => TerminalProof::SafeCancelled(v),
            Self::Stopped(v) => TerminalProof::SafeStopped(v),
            Self::Interrupted => TerminalProof::Interrupted,
            Self::Failed(message) => TerminalProof::Failed {
                message: message.string(),
            },
            #[cfg(any(test, feature = "hitl_mode"))]
            Self::Engine(kind, before, after) => TerminalProof::EngineCompleted {
                kind,
                before,
                after,
            },
            #[cfg(any(test, feature = "hitl_mode"))]
            Self::Trim(trim, input) => TerminalProof::VerifiedFillTrim { trim, input },
        }
    }
}
#[derive(Clone, Copy)]
enum Payload {
    Cold(gse_sequence::cold_flow::Observation, ProgressObservation),
    Start(Kind, ProgressObservation, [PlanStep; 32], usize),
    Observe(ProgressObservation, Cause),
    Fill(ProgressObservation, Option<Sample>, Option<f32>, u64, Cause),
    Trim(
        gse_sequence::fill_trim::ProgressObservation,
        ProgressObservation,
        Option<Sample>,
        u64,
        Option<Text<512>>,
    ),
    Turbo(
        gse_sequence::turbocharge::ProgressObservation,
        ProgressObservation,
        Option<Sample>,
        u64,
        Option<Text<512>>,
    ),
    Complete(Proof),
}
#[derive(Clone, Copy)]
pub struct Event {
    owner: Owner,
    ordinal: u64,
    at: u64,
    payload: Payload,
}
/// Exact raw owner/revision captured outside the final Stop chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Marker {
    owner: Owner,
    ordinal: u64,
}
pub struct Summary {
    pub key: RunKey,
    pub kind: Kind,
    pub outcome: Outcome,
    pub cancel_allowed: bool,
}
pub struct Stage {
    current: Option<Summary>,
    ordinal: u64,
    events: [Option<Event>; CAP],
    len: usize,
    pub error: Option<Error>,
}
impl Default for Stage {
    fn default() -> Self {
        Self {
            current: None,
            ordinal: 0,
            events: [None; CAP],
            len: 0,
            error: None,
        }
    }
}
impl Stage {
    pub fn marker(&self) -> Option<Marker> {
        let summary = self.current.as_ref().filter(|s| !s.outcome.terminal())?;
        Some(Marker {
            owner: Owner::parse(&summary.key)?,
            ordinal: self.ordinal,
        })
    }
    pub fn interrupt_if_current(&mut self, marker: Option<Marker>, at: u64) -> bool {
        if self.marker() != marker {
            self.error = Some(Error::Stale);
            return false;
        }
        if marker.is_some() {
            self.complete(TerminalProof::Interrupted, at);
        }
        true
    }
    pub fn current(&self) -> Option<&Summary> {
        self.current.as_ref()
    }
    fn event(&mut self, payload: Payload, at: u64) {
        if self.error.is_some() {
            return;
        }
        let Some(summary) = &self.current else {
            return;
        };
        let Some(owner) = Owner::parse(&summary.key) else {
            self.error = Some(Error::Invalid);
            return;
        };
        if self.len == CAP {
            self.error = Some(Error::NoCapacity);
            return;
        }
        let Some(next) = self.ordinal.checked_add(1) else {
            self.error = Some(Error::Bounds);
            return;
        };
        self.ordinal = next;
        self.events[self.len] = Some(Event {
            owner,
            ordinal: next,
            at,
            payload,
        });
        self.len += 1;
    }
    pub fn accepted(
        &mut self,
        key: RunKey,
        kind: Kind,
        view: ProgressObservation,
        plan: &[NitrogenStep],
        at: u64,
    ) {
        if self.current.as_ref().is_some_and(|s| !s.outcome.terminal()) {
            self.complete(TerminalProof::Interrupted, at);
        }
        if plan.len() > 32 || !key.valid() {
            self.error = Some(Error::Bounds);
            return;
        }
        let mut steps = [EMPTY_STEP; 32];
        for (i, p) in plan.iter().enumerate() {
            steps[i] = PlanStep {
                target: p.target_psi,
                settle: p.settle_ms,
                hold: p.hold_ms,
            };
        }
        self.current = Some(Summary {
            key,
            kind,
            outcome: Outcome::Running,
            cancel_allowed: kind.has_gse_safe_cancel(),
        });
        self.ordinal = 0;
        self.event(Payload::Start(kind, view, steps, plan.len()), at);
    }
    fn observing(&mut self, view: ProgressObservation) {
        if let Some(s) = &mut self.current {
            if s.outcome.terminal() {
                return;
            }
            s.outcome = match view.phase {
                Phase::Fault => Outcome::Failed,
                Phase::CancelSetup | Phase::Cancelled => Outcome::Cancelling,
                Phase::Paused => Outcome::Paused,
                _ => Outcome::Running,
            };
            s.cancel_allowed = s.kind.has_gse_safe_cancel()
                && !matches!(s.outcome, Outcome::Failed | Outcome::Cancelling);
        }
    }
    pub fn observe(&mut self, view: ProgressObservation, cause: Cause, at: u64) {
        if self.current.as_ref().is_none_or(|s| s.outcome.terminal()) {
            return;
        }
        self.observing(view);
        self.event(Payload::Observe(view, cause), at);
    }
    pub fn observe_fill(
        &mut self,
        view: ProgressObservation,
        mass: Option<Sample>,
        target: Option<f32>,
        now: u64,
        cause: Cause,
        at: u64,
    ) {
        if self.current.as_ref().is_none_or(|s| s.outcome.terminal()) {
            return;
        }
        self.observing(view);
        self.event(Payload::Fill(view, mass, target, now, cause), at);
    }
    pub fn observe_trim(
        &mut self,
        trim: gse_sequence::fill_trim::ProgressObservation,
        view: ProgressObservation,
        mass: Option<Sample>,
        now: u64,
        reason: Option<&str>,
        at: u64,
    ) {
        if self.current.as_ref().is_none_or(|s| s.outcome.terminal()) {
            return;
        }
        self.event(
            Payload::Trim(trim, view, mass, now, reason.map(Text::new)),
            at,
        );
    }
    pub fn observe_turbo(
        &mut self,
        turbo: gse_sequence::turbocharge::ProgressObservation,
        view: ProgressObservation,
        mass: Option<Sample>,
        now: u64,
        reason: Option<&str>,
        at: u64,
    ) {
        if self.current.as_ref().is_none_or(|s| s.outcome.terminal()) {
            return;
        }
        self.event(
            Payload::Turbo(turbo, view, mass, now, reason.map(Text::new)),
            at,
        );
    }
    pub fn observe_cold_flow(
        &mut self,
        cold: gse_sequence::cold_flow::Observation,
        view: ProgressObservation,
        at: u64,
    ) {
        if self.current.as_ref().is_none_or(|s| s.outcome.terminal()) {
            return;
        }
        self.event(Payload::Cold(cold, view), at);
        if let Some(s) = &mut self.current {
            use gse_sequence::cold_flow::Phase as C;
            s.outcome = if cold.phase == C::Cancelled {
                Outcome::Cancelled
            } else if matches!(cold.phase, C::CancelClosing | C::CancelRelieving) {
                Outcome::Cancelling
            } else if matches!(cold.phase, C::ParkedUnverified | C::Paused) {
                Outcome::Paused
            } else {
                Outcome::Running
            };
            s.cancel_allowed = !matches!(
                cold.phase,
                C::CancelClosing | C::CancelRelieving | C::Cancelled
            );
        }
    }
    pub fn complete_failure(&mut self, message: &str, at: u64) {
        if self.current.as_ref().is_none_or(|s| s.outcome.terminal()) {
            return;
        }
        self.event(Payload::Complete(Proof::Failed(Text::new(message))), at);
        if let Some(s) = &mut self.current {
            s.outcome = Outcome::Failed;
            s.cancel_allowed = false;
        }
    }
    pub fn complete(&mut self, proof: TerminalProof, at: u64) {
        if self.current.as_ref().is_none_or(|s| s.outcome.terminal()) {
            return;
        }
        if matches!(&proof,TerminalProof::DurablePass{event_id,..} if event_id.len()>128) {
            self.error = Some(Error::Invalid);
            return;
        }
        let proof = Proof::capture(proof);
        self.event(Payload::Complete(proof), at);
        if let Some(s) = &mut self.current {
            s.outcome = match proof {
                Proof::Interrupted => Outcome::Interrupted,
                Proof::Failed(_) => Outcome::Failed,
                Proof::Safe(_) | Proof::Stopped(_) => Outcome::Cancelled,
                _ => Outcome::Passed,
            };
            s.cancel_allowed = false;
        }
    }
    /// Moves fixed-size entries only; allocation happens outside the GSE guard.
    pub fn take(&mut self) -> Batch {
        let events = std::mem::replace(&mut self.events, [None; CAP]);
        let len = std::mem::take(&mut self.len);
        Batch {
            events,
            len,
            error: self.error,
        }
    }
}
pub struct Batch {
    events: [Option<Event>; CAP],
    len: usize,
    pub error: Option<Error>,
}
impl Batch {
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            events: [None; CAP],
            len: 0,
            error: None,
        }
    }
}
#[derive(Default)]
pub struct Projector {
    failed: Option<Error>,
    producer: Producer,
    owner: Option<Owner>,
    ordinal: u64,
}
impl Projector {
    pub fn apply(&mut self, batch: Batch) -> (Vec<Publication>, Option<Error>) {
        let mut output = Vec::new();
        if let Some(error) = self.failed {
            return (output, Some(error));
        }
        let mut error = batch.error;
        for event in batch.events.into_iter().take(batch.len).flatten() {
            let start = matches!(event.payload, Payload::Start(..));
            if (start && event.ordinal != 1)
                || (!start
                    && (self.owner != Some(event.owner)
                        || self.ordinal.checked_add(1) != Some(event.ordinal)))
            {
                error = Some(Error::RevisionGap);
                break;
            }
            if start {
                self.owner = Some(event.owner);
                self.ordinal = 0;
            }
            self.ordinal = event.ordinal;
            match event.payload {
                Payload::Cold(cold, view) => self.producer.observe_cold_flow(cold, view, event.at),
                Payload::Start(kind, view, plan, len) => {
                    let plan: Vec<_> = plan[..len]
                        .iter()
                        .map(|p| NitrogenStep {
                            target_psi: p.target,
                            settle_ms: p.settle,
                            hold_ms: p.hold,
                        })
                        .collect();
                    self.producer
                        .accepted(event.owner.key(), kind, view, &plan, event.at);
                }
                Payload::Observe(v, c) => self.producer.observe(v, c, event.at),
                Payload::Fill(v, m, t, n, c) => self.producer.observe_fill(v, m, t, n, c, event.at),
                Payload::Trim(t, v, m, n, r) => {
                    let r = r.map(Text::string);
                    self.producer
                        .observe_trim(t, v, m, n, r.as_deref(), event.at);
                }
                Payload::Turbo(t, v, m, n, r) => {
                    let r = r.map(Text::string);
                    self.producer
                        .observe_turbo(t, v, m, n, r.as_deref(), event.at);
                }
                Payload::Complete(p) => self.producer.complete(p.expand(), event.at),
            }
            output.extend(self.producer.drain());
            if self.producer.error.is_some() {
                error = self.producer.error;
            }
        }
        self.failed = error;
        (output, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(n: u128) -> RunKey {
        RunKey {
            backend_instance: format!("{:032x}", 1),
            reset_epoch: format!("{:016x}", 2),
            run_id: format!("{n:032x}"),
        }
    }
    fn view() -> ProgressObservation {
        gse_sequence::Engine::default().progress_observation(Inputs {
            now_ms: 100,
            pressure: None,
            valves: [None; 5],
            prelaunch: true,
            interlock: true,
        })
    }
    fn bytes(rows: Vec<Publication>) -> Vec<String> {
        rows.into_iter()
            .map(|r| serde_json::to_string(&r.snapshot).unwrap())
            .collect()
    }
    #[test]
    fn staged_observations_preserve_exact_existing_projection_and_original_owner() {
        let mut stage = Stage::default();
        let mut original = Producer::default();
        let mut projector = Projector::default();
        let v = view();
        stage.accepted(key(3), Kind::Fill, v, &[], 100);
        original.accepted(key(3), Kind::Fill, v, &[], 100);
        assert_eq!(
            bytes(projector.apply(stage.take()).0),
            bytes(original.drain())
        );
        for cause in [
            Cause::Tick,
            Cause::PauseAccepted,
            Cause::Tick,
            Cause::ResumeAccepted,
        ] {
            stage.observe(v, cause, 101);
            original.observe(v, cause, 101);
            assert_eq!(
                bytes(projector.apply(stage.take()).0),
                bytes(original.drain())
            );
        }
        stage.complete(TerminalProof::Interrupted, 102);
        original.complete(TerminalProof::Interrupted, 102);
        let (rows, error) = projector.apply(stage.take());
        assert!(error.is_none());
        assert_eq!(bytes(rows), bytes(original.drain()));
    }
    #[test]
    fn missing_reordered_or_wrong_owner_event_is_unavailable_never_recredited() {
        for corrupt_owner in [false, true] {
            let mut stage = Stage::default();
            let mut projector = Projector::default();
            stage.accepted(key(3), Kind::Fill, view(), &[], 100);
            projector.apply(stage.take());
            stage.observe(view(), Cause::Tick, 101);
            let mut batch = stage.take();
            let e = batch.events[0].as_mut().unwrap();
            if corrupt_owner {
                e.owner.run = 4;
            } else {
                e.ordinal += 1;
            }
            let (rows, error) = projector.apply(batch);
            assert!(rows.is_empty());
            assert_eq!(error, Some(Error::RevisionGap));
        }
    }
    #[test]
    fn full_ring_retains_owner_and_bounds_memory_without_any_projection() {
        let mut stage = Stage::default();
        stage.accepted(key(3), Kind::Fill, view(), &[], 100);
        for _ in 0..100 {
            stage.observe(view(), Cause::Tick, 100);
        }
        assert_eq!(stage.len, CAP);
        assert_eq!(stage.error, Some(Error::NoCapacity));
        assert_eq!(stage.current().unwrap().key, key(3));
        assert!(std::mem::size_of::<Stage>() < 32768);
        println!("SEQUENCE_STAGE_BYTES={}", std::mem::size_of::<Stage>());
    }
    #[test]
    fn old_owner_interrupted_batch_survives_runtime_stage_replacement() {
        let mut stage = Stage::default();
        let mut projector = Projector::default();
        stage.accepted(key(3), Kind::Fill, view(), &[], 100);
        projector.apply(stage.take());
        stage.complete(TerminalProof::Interrupted, 101);
        let before_reset = stage.take();
        stage = Stage::default();
        assert!(stage.current().is_none());
        let (rows, error) = projector.apply(before_reset);
        assert!(error.is_none());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].snapshot.key, key(3));
        assert_eq!(rows[0].snapshot.outcome, Outcome::Interrupted);
    }
    #[test]
    fn final_reset_marker_cannot_interrupt_a_new_owner_and_old_event_precedes_reopened_admission() {
        let mut stage = Stage::default();
        let mut projector = Projector::default();
        stage.accepted(key(3), Kind::Fill, view(), &[], 100);
        projector.apply(stage.take());
        let marker = stage.marker();
        stage.interrupt_if_current(marker, 101);
        stage.accepted(key(4), Kind::Fill, view(), &[], 102);
        let (rows, error) = projector.apply(stage.take());
        assert!(error.is_none());
        assert_eq!(rows[0].snapshot.key, key(3));
        assert_eq!(rows[0].snapshot.outcome, Outcome::Interrupted);
        assert_eq!(rows[1].snapshot.key, key(4));
        assert_eq!(rows[1].snapshot.outcome, Outcome::Running);
        stage.interrupt_if_current(marker, 103);
        assert_eq!(stage.error, Some(Error::Stale));
        assert_eq!(stage.current().unwrap().key, key(4));
        assert_eq!(stage.current().unwrap().outcome, Outcome::Running);
        assert_eq!(stage.take().len, 0);
    }
}
