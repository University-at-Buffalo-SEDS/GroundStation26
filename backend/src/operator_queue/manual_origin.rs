//! Original authenticated manual control across a retained recovery barrier.
//! The capability is private to ingress and never accepted from JSON.
use crate::{state::AppState, types::TelemetryCommand};
use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
#[derive(Clone, Copy, Debug)]
pub(crate) struct Origin {
    token: super::Token,
    captured: Instant,
    command: &'static str,
    admitted: bool,
}
impl Origin {
    pub(crate) fn capture(state: &AppState, command: &TelemetryCommand) -> Option<Self> {
        if crate::rrls::is_rrls() || command.automatic_generation().is_some() {
            return None;
        }
        let owner = state.operator_admission_barrier.load(Ordering::Acquire);
        if matches!(owner, 0 | u64::MAX) {
            return None;
        }
        Some(Self {
            token: super::Token::capture(state, owner),
            captured: Instant::now(),
            command: crate::sequences::command_name(command),
            admitted: false,
        })
    }
    pub(crate) fn current(&self, state: &AppState) -> bool {
        self.captured.elapsed() <= Duration::from_secs(5)
            && !state.shutdown_requested.load(Ordering::Acquire)
            && self.token.owner != 0
            && self.token.owner != u64::MAX
            && self.token.generation != 0
            && self.token.generation != u64::MAX
            && state.operator_admission_barrier.load(Ordering::Acquire) == self.token.owner
            && crate::gse::automatic_generation(state) == self.token.generation
            && state.operator_queue.relief_epoch() == self.token.relief_epoch
            && state.manual_abort_intent.epoch() == self.token.manual_epoch
            && state.deferred_abort.manual_epoch() == self.token.observed_manual_epoch
    }
    pub(crate) fn admits(&self, state: &AppState, command: &TelemetryCommand) -> bool {
        self.current(state) && self.command == crate::sequences::command_name(command)
    }
    pub(crate) fn admit(&mut self, state: &AppState) -> bool {
        if self.admitted
            || !self.current(state)
            || !state
                .operator_queue
                .advance_relief_epoch(self.token.relief_epoch)
        {
            return false;
        }
        self.token.relief_epoch += 1;
        self.admitted = true;
        true
    }
    pub(crate) fn dispatched(&self, state: &AppState) -> bool {
        self.admitted && self.current(state)
    }
    pub(crate) fn generation(&self) -> u64 {
        self.token.generation
    }
    pub(crate) fn phase_successor(&self, state: &AppState, previous: super::Token) -> Option<u64> {
        (self.dispatched(state)
            && previous.generation == self.token.generation
            && previous.manual_epoch == self.token.manual_epoch
            && previous.observed_manual_epoch == self.token.observed_manual_epoch
            && previous.relief_epoch.checked_add(1) == Some(self.token.relief_epoch))
        .then_some(self.token.relief_epoch)
    }
}
