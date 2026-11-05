//! The implicit readiness transition owns only live RAM and one SDK original.
//! It never writes the durable desired-state seed. A restart qualifies again.
use crate::{
    gse::LaunchAdmission, network_variables::AutomaticIdleSnapshot, state::AppState,
    types::FlightState,
};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub(crate) struct Captured {
    pub(crate) ticket: LaunchAdmission,
    pub(crate) generation: u64,
    pub(crate) store: AutomaticIdleSnapshot,
}
impl Captured {
    /// Called at actual dequeue, before the first await/provider preparation.
    pub(crate) fn capture(state: &AppState, primary: bool) -> Option<Self> {
        // Finite original ownership precedes the first state/Store wait.
        let ticket = LaunchAdmission::capture(state);
        if !cfg!(feature = "test_fire_mode")
            || !primary
            || !ticket.automatic_board_context_current(state)
            || state.local_flight_state_snapshot() != FlightState::Startup
        {
            return None;
        }
        let generation = crate::gse::capture_operator_origin(state)?;
        if ticket.original_ownership().0 != generation {
            return None;
        }
        Some(Self {
            ticket,
            generation,
            store: crate::network_variables::capture_automatic_idle_store()?,
        })
    }
}
#[derive(Debug)]
pub(crate) enum Error {
    Busy,
    Retired,
    StateChanged,
    StoreChanged,
    Sdk(sedsnet::router::TryManagedBatchError),
}
pub(crate) async fn publish(state: &Arc<AppState>, original: Captured) {
    publish_inner(state, original).await;
    #[cfg(all(test, feature = "test_fire_mode"))]
    crate::recovery_pending_probe::automatic_completed(state);
}
async fn publish_inner(state: &Arc<AppState>, original: Captured) {
    let Some(claim) = crate::gse::claim_operator_dispatch(state, original.generation) else {
        return;
    };
    let Some(router) = state.topology_router.get() else {
        return;
    };
    // The original counted owner is retained through all preparation and
    // notification/DB-queue admission. No IO is performed under final guards.
    let packet = match crate::network_variables::automatic_idle_packet(router) {
        Ok(packet) => packet,
        Err(error) => {
            log::warn!("Automatic Startup readiness not published: {error}");
            return;
        }
    };
    let identity = crate::network_variables::AutomaticIdleIdentity {
        packet_id: packet.packet_id(),
        timestamp: packet.timestamp(),
        generation: original.generation,
    };
    let request = crate::qualification_requests::Request::begin("set_flight_state");
    let id = request.bind_prepared(&packet);
    let mut prepared = match router.prepare_network_variable_batch(
        vec![packet],
        crate::network_variables::automatic_idle_batch_limits(),
    ) {
        Ok(prepared) => prepared,
        Err(failure) => {
            request.prepared_result(id, false);
            log::debug!(
                "Automatic readiness preparation refused: {:?}",
                failure.error
            );
            return;
        }
    };
    let model = crate::flight_sim::prepare_automatic_readiness();
    #[cfg(all(test, feature = "test_fire_mode"))]
    // This is the real prepared-publication boundary, NOT a fake disk write.
    crate::recovery_pending_probe::before_state_commit(1);
    // A canceled receiver is observed while this original is still entirely
    // caller-owned. Final admission revalidates every original owner afterward.
    tokio::task::yield_now().await;
    // One nonwaiting attempt. A fresh dequeued event may qualify independently;
    // no actor retains/restamps an unadmitted original or invents a new TTL.
    let outcome =
        crate::gse::try_commit_automatic_idle(state, original, identity, &model, &mut prepared);
    #[cfg(all(test,feature="test_fire_mode"))]
    crate::recovery_pending_probe::record_final_commit_outcome(state,&outcome);
    request.prepared_result(id, outcome.is_ok());
    match outcome {
        Ok(()) => {
            state.observe_automatic_idle_admission(claim);
        }
        Err(Error::Sdk(error)) => {
            log::debug!("Automatic readiness SDK admission refused: {error:?}")
        }
        Err(error) => log::debug!("Automatic readiness original retired/refused: {error:?}"),
    }
    // Prepared old maps/unadmitted originals drop only after all final guards.
}

#[cfg(all(test, feature="test_fire_mode"))]
mod guard_tests;
