//! Original authenticated manual control across a retained recovery barrier.
//! The capability is private to ingress and never accepted from JSON.
use crate::{state::AppState, types::TelemetryCommand};
use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rejection { Shutdown, Expired, ResetChanged, StopChanged, RecoveryChanged, Superseded, MissingOrigin }
impl Rejection {
    pub(crate) fn message(self)->&'static str {match self {
        Self::Shutdown=>"GroundStation is shutting down. Reconnect after it restarts.",
        Self::Expired=>"The request expired while waiting for authorization or dispatch. Try again.",
        Self::ResetChanged=>"The station reset while this request was pending. Refresh status and send a new action.",
        Self::StopChanged=>"A newer manual Stop superseded this request. Check Stop delivery and board recovery before sending a new action.",
        Self::RecoveryChanged=>"Recovery changed while this request was pending. Refresh status and send a new action.",
        Self::Superseded=>"Another manual action superseded this pending request. Send a new action if still needed.",
        Self::MissingOrigin=>"This request has no fresh manual-control receipt for retained recovery. Refresh the station connection and try again.",
    }}
}
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
        // MAX is the retained unhealthy-journal sentinel, not permission to
        // reset or launch. A fresh manual request may bind it without opening
        // the barrier or changing any persisted recovery evidence.
        if owner == 0 {
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
        self.context_current(state) && state.operator_queue.relief_epoch() == self.token.relief_epoch
    }
    /// Duplicate detection may acknowledge an already admitted command after
    /// another copy spent its relief epoch; it must never renew any authority.
    pub(crate) fn context_current(&self, state: &AppState) -> bool {
        self.captured.elapsed() <= Duration::from_secs(5)
            && !state.shutdown_requested.load(Ordering::Acquire)
            && self.token.owner != 0
            && self.token.generation != 0
            && self.token.generation != u64::MAX
            && state.operator_admission_barrier.load(Ordering::Acquire) == self.token.owner
            && crate::gse::automatic_generation(state) == self.token.generation
            && state.manual_abort_intent.epoch() == self.token.manual_epoch
            && state.deferred_abort.manual_epoch() == self.token.observed_manual_epoch
    }
    pub(crate) fn same_command_context(&self,state:&AppState,command:&TelemetryCommand)->bool {
        self.context_current(state) && self.command==crate::sequences::command_name(command)
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
    pub(crate) fn rejection(&self,state:&AppState)->Rejection {
        if state.shutdown_requested.load(Ordering::Acquire) {Rejection::Shutdown}
        else if self.captured.elapsed()>Duration::from_secs(5) {Rejection::Expired}
        else if crate::gse::automatic_generation(state)!=self.token.generation {Rejection::ResetChanged}
        else if state.manual_abort_intent.epoch()!=self.token.manual_epoch || state.deferred_abort.manual_epoch()!=self.token.observed_manual_epoch {Rejection::StopChanged}
        else if state.operator_admission_barrier.load(Ordering::Acquire)!=self.token.owner {Rejection::RecoveryChanged}
        else {Rejection::Superseded}
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

#[cfg(test)]
mod rejection_tests {
    use super::*;
    #[tokio::test]
    async fn rejection_diagnostics_distinguish_expiry_reset_stop_recovery_and_supersession() {
        let state=crate::state::tests::test_app_state().await;
        state.operator_admission_barrier.store(77,Ordering::Release);
        let original=Origin::capture(&state,&TelemetryCommand::Postinit).unwrap();
        let mut expired=original;expired.captured=Instant::now()-Duration::from_secs(6);
        assert_eq!(expired.rejection(&state),Rejection::Expired);
        state.operator_admission_barrier.store(78,Ordering::Release);
        assert_eq!(original.rejection(&state),Rejection::RecoveryChanged);
        state.operator_admission_barrier.store(77,Ordering::Release);
        state.automatic_generation.fetch_add(1,Ordering::AcqRel);
        assert_eq!(original.rejection(&state),Rejection::ResetChanged);
        let original=Origin::capture(&state,&TelemetryCommand::Postinit).unwrap();
        state.manual_abort_intent.request();
        assert_eq!(original.rejection(&state),Rejection::StopChanged);
        let original=Origin::capture(&state,&TelemetryCommand::Postinit).unwrap();
        assert!(state.operator_queue.advance_relief_epoch(original.token.relief_epoch));
        assert_eq!(original.rejection(&state),Rejection::Superseded);
    }
}
