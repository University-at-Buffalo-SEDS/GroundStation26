//! Copy-only producer bridge. Never dispatches, persists or changes an engine.
use super::{
    sequence_progress::{Error, Kind, RunKey, Snapshot},
    sequence_progress_projection::{Cause, Cursor, TerminalProof},
};
use gse_sequence::{NitrogenStep, ProgressObservation};
use std::collections::VecDeque;
const MAX_PENDING: usize = 16;
pub struct Publication {
    pub snapshot: Snapshot,
    pub observed_at_ms: u64,
}
#[derive(Default)]
pub struct Producer {
    cursor: Option<Cursor>,
    pending: VecDeque<Publication>,
    pub error: Option<Error>,
}
impl Producer {
    pub fn current(&self) -> Option<&Snapshot> {
        self.cursor.as_ref().map(Cursor::snapshot)
    }
    fn emit(&mut self, now: u64) {
        if self.error.is_some() {
            return;
        }
        if self.pending.len() >= MAX_PENDING {
            self.error = Some(Error::NoCapacity);
            return;
        }
        if let Some(cursor) = &self.cursor {
            self.pending.push_back(Publication {
                snapshot: cursor.snapshot().clone(),
                observed_at_ms: now,
            });
        }
    }
    pub fn accepted(
        &mut self,
        key: RunKey,
        kind: Kind,
        view: ProgressObservation,
        plan: &[NitrogenStep],
        now: u64,
    ) {
        if self.current().is_some_and(|s| !s.outcome.terminal()) {
            self.complete(TerminalProof::Interrupted, now);
        }
        match Cursor::accepted(key, kind, None, view, plan) {
            Ok(cursor) => {
                self.cursor = Some(cursor);
                self.emit(now);
            }
            Err(error) => self.error = Some(error),
        }
    }
    pub fn observe(&mut self, view: ProgressObservation, cause: Cause, now: u64) {
        if let Some(cursor) = &mut self.cursor {
            if cursor.snapshot().outcome.terminal() {
                return;
            }
            match cursor.observe(view, cause) {
                Ok(_) => self.emit(now),
                Err(error) => self.error = Some(error),
            }
        }
    }
    pub fn observe_fill(
        &mut self,
        view: ProgressObservation,
        mass: Option<gse_sequence::Sample>,
        target: Option<f32>,
        source_now_ms: u64,
        cause: Cause,
        now: u64,
    ) {
        if let Some(cursor) = &mut self.cursor {
            if cursor.snapshot().outcome.terminal() {
                return;
            }
            match cursor.observe_fill(view, mass, target, source_now_ms, cause) {
                Ok(_) => self.emit(now),
                Err(error) => self.error = Some(error),
            }
        }
    }
    pub fn observe_trim(
        &mut self,
        trim: gse_sequence::fill_trim::ProgressObservation,
        view: ProgressObservation,
        mass: Option<gse_sequence::Sample>,
        source_now_ms: u64,
        reason: Option<&str>,
        now: u64,
    ) {
        if let Some(cursor) = &mut self.cursor {
            if cursor.snapshot().outcome.terminal() {
                return;
            }
            match cursor.observe_trim(trim, view, mass, source_now_ms, reason) {
                Ok(_) => self.emit(now),
                Err(error) => self.error = Some(error),
            }
        }
    }
    pub fn observe_turbo(
        &mut self,
        turbo: gse_sequence::turbocharge::ProgressObservation,
        view: ProgressObservation,
        mass: Option<gse_sequence::Sample>,
        source_now_ms: u64,
        reason: Option<&str>,
        now: u64,
    ) {
        if let Some(cursor) = &mut self.cursor {
            if cursor.snapshot().outcome.terminal() {
                return;
            }
            match cursor.observe_turbo(turbo, view, mass, source_now_ms, reason) {
                Ok(_) => self.emit(now),
                Err(error) => self.error = Some(error),
            }
        }
    }
    pub fn observe_cold_flow(
        &mut self,
        cold: gse_sequence::cold_flow::Observation,
        view: ProgressObservation,
        now: u64,
    ) {
        if let Some(cursor) = &mut self.cursor {
            if cursor.snapshot().outcome.terminal() {
                return;
            }
            match cursor.observe_cold_flow(cold, view) {
                Ok(_) => self.emit(now),
                Err(error) => self.error = Some(error),
            }
        }
    }
    pub fn complete(&mut self, proof: TerminalProof, now: u64) {
        if let Some(cursor) = &mut self.cursor {
            if cursor.snapshot().outcome.terminal() {
                return;
            }
            let before = cursor.snapshot().revision;
            match cursor.seal(proof) {
                Ok(snapshot) if snapshot.revision != before => self.emit(now),
                Ok(_) => {}
                Err(error) => self.error = Some(error),
            }
        }
    }
    pub fn drain(&mut self) -> Vec<Publication> {
        self.pending.drain(..).collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use gse_sequence::{Engine, Inputs, Phase};
    fn key(n: u64) -> RunKey {
        RunKey {
            backend_instance: "1".repeat(32),
            reset_epoch: "2".repeat(16),
            run_id: format!("{n:032x}"),
        }
    }
    fn view() -> ProgressObservation {
        Engine::default().progress_observation(Inputs {
            now_ms: 100,
            pressure: None,
            valves: [None; 5],
            interlock: true,
            prelaunch: true,
        })
    }
    #[test]
    fn replacement_is_interrupted_never_passed_and_pending_is_bounded() {
        let mut producer = Producer::default();
        producer.accepted(key(1), Kind::Fill, view(), &[], 100);
        producer.accepted(key(2), Kind::Fill, view(), &[], 100);
        let rows = producer.drain();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[1].snapshot.outcome,
            super::super::sequence_progress::Outcome::Interrupted
        );
        let mut v = view();
        v.phase = Phase::Filling;
        for _ in 0..100 {
            producer.observe(v, Cause::Tick, 100);
        }
        assert_eq!(producer.error, Some(Error::NoCapacity));
        assert_eq!(producer.drain().len(), MAX_PENDING);
    }
    #[test]
    fn later_manual_handoff_does_not_rewrite_or_poison_an_existing_terminal_result() {
        let mut producer = Producer::default();
        producer.accepted(key(1), Kind::Fill, view(), &[], 100);
        producer.complete(
            TerminalProof::DurablePass {
                kind: Kind::Fill,
                event_id: "fixture-recorded-pass".into(),
            },
            100,
        );
        producer.drain();
        producer.complete(TerminalProof::Interrupted, 200);
        assert!(producer.error.is_none());
        assert!(producer.drain().is_empty());
    }
    #[test]
    fn no_observed_or_idle_state_creates_an_accepted_run() {
        let mut producer = Producer::default();
        producer.observe(view(), Cause::Tick, 100);
        assert!(producer.drain().is_empty());
        producer.complete(TerminalProof::Interrupted, 100);
        assert!(producer.drain().is_empty());
    }
}
