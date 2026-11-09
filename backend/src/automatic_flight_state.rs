//! GS-derived prelaunch projection. The original owner precedes observations;
//! final RAM/model/SDK admission is live-only, never a desired-JSON writer.
use crate::{gse::LaunchAdmission, state::AppState, types::FlightState};
use std::sync::{Arc, atomic::Ordering};

/// The lease remains with the real tick/command/refresh, including when Stop
/// makes it ineligible to publish. Recovery must still drain actual work.
pub(crate) struct Observation {
    ticket: LaunchAdmission,
    generation: u64,
    _claim: crate::gse::OperatorDispatchClaim,
    captured: Option<Captured>,
    policy: Option<crate::sequences::SequencePolicyState>,
}
#[derive(Clone, Copy)]
pub(crate) struct Captured {
    pub(crate) ticket: LaunchAdmission,
    pub(crate) generation: u64,
    pub(crate) expected: FlightState,
    pub(crate) store: crate::network_variables::AutomaticPhaseSnapshot,
    pub(crate) policy: crate::sequences::SequencePolicyState,
}
#[derive(Clone, Copy)]
pub(crate) enum Cause {
    /// Normal and testing only.
    Readiness,
    LegacySequence {
        next_policy: crate::sequences::SequencePolicyState,
    },
    /// Never in HITL.
    QualifiedArmed { qualification_generation: u64 },
}
#[derive(Clone, Copy)]
pub(crate) struct Proposal {
    pub(crate) original: Captured,
    pub(crate) target: FlightState,
    pub(crate) cause: Cause,
}
#[derive(Debug)]
pub(crate) enum Error {
    Busy,
    Retired,
    StateChanged,
    StoreChanged,
    PolicyChanged,
    Ineligible,
    Sdk(sedsnet::router::TryManagedBatchError),
}
impl Observation {
    pub(crate) fn begin(state: &AppState) -> Option<Self> {
        // ALL fields in this existing ticket are atomic reads. In particular,
        // do not take Dispatch/GSE/state/Store/provider locks before this point.
        let ticket = LaunchAdmission::capture(state);
        #[cfg(all(test, not(feature = "firmware_sim")))]
        if !crate::runtime_mode::is_hitl_or_test_fire() {
            crate::normal_publication_probe::before_claim(state, ticket);
        }
        let generation = ticket.original_ownership().0;
        let claim = crate::gse::claim_operator_dispatch(state, generation)?;
        let board_current = ticket.automatic_board_context_current(state);
        let policy = state.sequence_policy_state.try_lock().ok().map(|p| *p);
        let captured = if board_current && !crate::runtime_mode::is_hitl()
            && !crate::rrls::is_rrls()
            && !matches!(generation, 0 | u64::MAX)
            && ticket.original_ownership().1 != u64::MAX
            && ticket.original_ownership().2 != u64::MAX
            && state.manual_abort_intent.pending_epoch().is_none()
            && !state.deferred_abort.manual_pending()
            && !state.abort_indicator_latched()
        {
            // Nonwaiting RAM capture only. Missing initialized Store or busy
            // owners means this cycle has no automatic publication authority.
            let expected = state.state.try_lock().ok().map(|v| *v);
            expected
                .filter(|v| (*v as u8) <= FlightState::Armed as u8)
                .zip(policy)
                .and_then(|(expected, policy)| {
                    crate::network_variables::capture_automatic_phase_store(
                        expected as u8,
                        generation,
                    )
                    .map(|store| Captured {
                        ticket,
                        generation,
                        expected,
                        store,
                        policy,
                    })
                })
        } else {
            None
        };
        Some(Self {
            ticket,
            generation,
            _claim: claim,
            captured,
            policy,
        })
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) fn captured(&self) -> Option<Captured> {
        self.captured
    }
    pub(crate) fn update_policy(
        &self,
        state: &AppState,
        expected_state: FlightState,
        next: crate::sequences::SequencePolicyState,
    ) {
        if let Some(expected) = self.policy {
            crate::gse::try_commit_automatic_policy(
                state,
                self.ticket,
                expected_state,
                expected,
                next,
            );
        }
    }
}
impl Captured {
    pub(crate) fn proposal(self, target: FlightState, cause: Cause) -> Option<Proposal> {
        if self.expected == target || !(1..=6).contains(&(target as u8)) {
            return None;
        }
        if crate::runtime_mode::is_test_fire()
            && matches!(self.expected, FlightState::Startup | FlightState::Idle)
        {
            return None;
        }
        if !crate::runtime_mode::is_hitl_or_test_fire()
            && matches!(cause, Cause::Readiness)
            && (self.expected != FlightState::Startup || target != FlightState::Idle)
        {
            return None;
        }
        Some(Proposal {
            original: self,
            target,
            cause,
        })
    }
}
/// Called ONLY outside all application/Stop/SDK owners. The caller retains its
/// original real-work lease; this new same-origin lease owns the DB handoff.
pub(crate) fn publish(state: &Arc<AppState>, proposal: Proposal) -> bool {
    #[cfg(all(test, not(feature = "firmware_sim")))]
    if !crate::runtime_mode::is_hitl_or_test_fire() {
        crate::normal_publication_probe::n6_record(state, 3);
    }
    #[cfg(all(test, not(feature = "firmware_sim")))]
    let publication_trace = (!crate::runtime_mode::is_hitl_or_test_fire())
        .then(|| crate::normal_publication_probe::PublicationClaim::enter(state, proposal.original.ticket));
    let Some(claim) = crate::gse::claim_operator_dispatch(state, proposal.original.generation)
    else {
        #[cfg(all(test, not(feature = "firmware_sim")))]
        if !crate::runtime_mode::is_hitl_or_test_fire() {
            crate::normal_publication_probe::n6_record(state, 4);
        }
        return false;
    };
    #[cfg(all(test, not(feature = "firmware_sim")))]
    drop(publication_trace);
    if state.manual_abort_intent.epoch() != proposal.original.ticket.original_ownership().1
        || state.deferred_abort.manual_epoch() != proposal.original.ticket.original_ownership().2
        || state.manual_abort_intent.pending_epoch().is_some()
        || state.deferred_abort.manual_pending()
        || state.shutdown_requested.load(Ordering::Acquire)
    {
        #[cfg(all(test, not(feature = "firmware_sim")))]
        if !crate::runtime_mode::is_hitl_or_test_fire() {
            crate::normal_publication_probe::n6_record(state, 5);
        }
        return false;
    }
    let Some(router) = state.topology_router.get() else {
        #[cfg(all(test, not(feature = "firmware_sim")))]
        if !crate::runtime_mode::is_hitl_or_test_fire() {
            crate::normal_publication_probe::n6_record(state, 6);
        }
        return false;
    };
    let packet =
        match crate::network_variables::automatic_phase_packet(router, proposal.target as u8) {
            Ok(packet) => packet,
            Err(error) => {
                #[cfg(all(test, not(feature = "firmware_sim")))]
                if !crate::runtime_mode::is_hitl_or_test_fire() {
                    crate::normal_publication_probe::n6_record(state, 7);
                }
                log::debug!("Automatic phase preparation refused: {error}");
                return false;
            }
        };
    let identity = crate::network_variables::AutomaticPhaseIdentity {
        original: crate::network_variables::AutomaticIdleIdentity {
            packet_id: packet.packet_id(),
            timestamp: packet.timestamp(),
            generation: proposal.original.generation,
        },
        phase: proposal.target as u8,
    };
    let request = crate::qualification_requests::Request::begin("set_flight_state");
    let request_id = request.bind_prepared(&packet);
    let mut prepared = match router.prepare_network_variable_batch(
        vec![packet],
        crate::network_variables::automatic_idle_batch_limits(),
    ) {
        Ok(prepared) => prepared,
        Err(failure) => {
            #[cfg(all(test, not(feature = "firmware_sim")))]
            if !crate::runtime_mode::is_hitl_or_test_fire() {
                crate::normal_publication_probe::n6_record(state, 8);
            }
            request.prepared_result(request_id, false);
            log::debug!(
                "Automatic phase SDK preparation refused: {:?}",
                failure.error
            );
            return false;
        }
    };
    let model =
        crate::flight_sim::prepare_automatic_phase(proposal.original.expected, proposal.target);
    let result =
        crate::gse::try_commit_automatic_phase(state, proposal, identity, &model, &mut prepared);
    request.prepared_result(request_id, result.is_ok());
    #[cfg(all(test, not(feature = "firmware_sim")))]
    if !crate::runtime_mode::is_hitl_or_test_fire() {
        crate::normal_publication_probe::n6_record(state, match &result { Ok(()) => 9, Err(Error::Busy) => 10, Err(Error::Retired) => 11, Err(Error::StateChanged) => 12, Err(Error::StoreChanged) => 13, Err(Error::PolicyChanged) => 14, Err(Error::Ineligible | Error::Sdk(_)) => 15 });
    }
    match result {
        Ok(()) => {
            state.observe_automatic_phase_admission(
                claim,
                proposal.target,
                identity.original.timestamp,
            );
            true
        }
        Err(error) => {
            // Read the nested error explicitly; Error has no hidden retry or
            // ownership transfer. Prepared originals drop after final guards.
            match error {
                Error::Sdk(error) => log::debug!("Automatic phase SDK refused: {error:?}"),
                other => log::debug!("Automatic phase refused: {other:?}"),
            }
            false
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
