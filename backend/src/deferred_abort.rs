//! SDK callbacks retain bounded observations only. GSE/journal/UI work happens
//! after the SDK can forward the unchanged packet to every original owner.
use crate::state::AppState;
use sedsnet::packet::Packet;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const AUTOMATIC_PREFIX: &[u8] = b"SEDS_AUTO_ABORT_V1:";
struct Observation {
    packet: Packet,
    generation: u64,
    automatic: bool,
}
#[derive(Default)]
struct Pending {
    manual: Option<Observation>,
    automatic: Option<Observation>,
    active_manual: bool,
    manual_epoch: u64,
    recent: [u64; 8],
    recent_len: usize,
    next: usize,
}
#[derive(Default)]
pub(crate) struct Inbox {
    state: Mutex<Pending>,
    wake: Notify,
}
impl Inbox {
    /// No GSE, journal, DB, packet parsing or network operations under this lock.
    pub(crate) fn observe(&self, packet: &Packet, generation: u64) {
        let automatic = packet.payload().starts_with(AUTOMATIC_PREFIX);
        let observation = Observation {
            packet: packet.clone(),
            generation,
            automatic,
        };
        {
            let mut state = self.state.lock().unwrap();
            let id = packet.packet_id();
            if state.recent[..state.recent_len].contains(&id) {
                return;
            }
            let next = state.next;
            state.recent[next] = id;
            state.next = (next + 1) % state.recent.len();
            state.recent_len = (state.recent_len + 1).min(state.recent.len());
            if automatic {
                state.automatic = Some(observation);
            } else {
                state.manual_epoch = state.manual_epoch.saturating_add(1);
                state.manual = Some(observation);
            }
        }
        self.wake.notify_one();
    }
    pub(crate) fn manual_epoch(&self) -> u64 {
        self.state.lock().unwrap().manual_epoch
    }
    pub(crate) fn manual_pending(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.manual.is_some() || state.active_manual
    }
    /// Keep a received manual observation ordered with final SDK admission.
    /// The caller only enqueues prebuilt packets while this bounded RAM lock is held.
    pub(crate) fn admit_if_no_manual_since<T>(
        &self,
        epoch: u64,
        admit: impl FnOnce() -> T,
    ) -> Option<T> {
        let state = self.state.lock().unwrap();
        if epoch == u64::MAX
            || state.manual_epoch == u64::MAX
            || state.manual_epoch != epoch
            || state.manual.is_some()
            || state.active_manual
        {
            return None;
        }
        Some(admit())
    }
    #[cfg(test)]
    pub(crate) fn exhaust_manual_epoch_for_test(&self) {
        let mut state = self.state.lock().unwrap();
        assert!(state.manual.is_none() && !state.active_manual);
        state.manual_epoch = u64::MAX;
    }
    fn claim(self: &Arc<Self>) -> Option<Claim> {
        let mut state = self.state.lock().unwrap();
        let observation = state.manual.take().or_else(|| state.automatic.take())?;
        if !observation.automatic {
            state.active_manual = true;
        }
        Some(Claim {
            owner: self.clone(),
            observation: Some(observation),
            accepted: false,
        })
    }
}
struct Claim {
    owner: Arc<Inbox>,
    observation: Option<Observation>,
    accepted: bool,
}
impl Drop for Claim {
    fn drop(&mut self) {
        let Some(observation) = self.observation.take() else {
            return;
        };
        let mut state = self.owner.state.lock().unwrap();
        if !observation.automatic {
            state.active_manual = false;
        }
        if !self.accepted {
            let slot = if observation.automatic {
                &mut state.automatic
            } else {
                &mut state.manual
            };
            if slot.is_none() {
                *slot = Some(observation);
            }
            self.owner.wake.notify_one();
        }
    }
}
fn apply(state: &Arc<AppState>, observation: &Observation) {
    if !crate::gse::accept_observed_abort(state, observation.automatic, observation.generation) {
        log::info!(
            "ignored stale automatic abort observation packet={}",
            observation.packet.packet_id()
        );
        return;
    }
    log::info!(
        "abort observation processed manual_epoch={} pending={}",
        state.deferred_abort.manual_epoch(),
        state.deferred_abort.manual_pending()
    );
    if !observation.automatic {
        // The independent emergency publisher bypasses the ordinary command
        // worker. Preserve its local cancellation after SDK admission without
        // putting GSE/calibration locks back into the SDK callback.
        let command = crate::types::TelemetryCommand::Abort;
        state.auto_zero.lock().unwrap().cancel_for_command(&command);
        crate::gse::handle_command(state, &command);
        crate::flight_sim::handle_command(&command);
    }
    state.mark_packet_received(crate::telemetry_task::get_current_timestamp_ms());
    // The original non-control generic receivers preserve telemetry recording.
    // No received Abort is originated/echoed by this observation task.
    crate::sequences::refresh_action_policy_now(state);
    state.broadcast_action_policy_snapshot();
    let reason = observation
        .packet
        .data_as_string()
        .unwrap_or_else(|_| String::from_utf8_lossy(observation.packet.payload()).into_owned());
    log::error!(
        "abort packet received sender={} packet={} message={reason}",
        observation.packet.sender(),
        observation.packet.packet_id()
    );
    crate::web::emit_error(state, reason);
}
pub(crate) fn spawn(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut shutdown = state.shutdown_subscribe();
        loop {
            if let Some(mut claim) = state.deferred_abort.claim() {
                let current = state.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    apply(
                        &current,
                        claim.observation.as_ref().expect("claimed observation"),
                    );
                    claim.accepted = true;
                    drop(claim);
                })
                .await;
                continue;
            }
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = state.deferred_abort.wake.notified() => {},
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FlightState;
    fn packet(message: &str) -> Packet {
        Packet::new(
            crate::telemetry_schema::data_type("ABORT"),
            &[
                crate::telemetry_schema::endpoint("ABORT"),
                crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
            ],
            "GS",
            10_000,
            Arc::from(message.as_bytes()),
        )
        .unwrap()
    }
    #[test]
    fn incoming_manual_owner_survives_claim_failure_and_automatic_cannot_overwrite_it() {
        let inbox = Arc::new(Inbox::default());
        let manual = packet("Manual Abort");
        let automatic = packet("SEDS_AUTO_ABORT_V1:late");
        inbox.observe(&manual, 1);
        inbox.observe(&manual, 1);
        assert_eq!(inbox.manual_epoch(), 1);
        inbox.observe(&automatic, 1);
        assert!(inbox.manual_pending());
        {
            let claim = inbox.claim().unwrap();
            assert!(!claim.observation.as_ref().unwrap().automatic);
            assert!(inbox.manual_pending());
        }
        assert!(inbox.manual_pending());
        {
            let mut claim = inbox.claim().unwrap();
            assert!(!claim.observation.as_ref().unwrap().automatic);
            claim.accepted = true;
        }
        assert!(!inbox.manual_pending());
        assert!(
            inbox
                .claim()
                .unwrap()
                .observation
                .as_ref()
                .unwrap()
                .automatic
        );
    }
    #[tokio::test]
    async fn actual_public_sdk_forwards_with_gse_mutex_held_and_retains_local_observation() {
        use sedsnet::router::{EndpointHandler, Router, RouterConfig};
        let state = crate::state::tests::test_app_state().await;
        let inbox = state.deferred_abort.clone();
        let callback_inbox = inbox.clone();
        let generation = crate::gse::automatic_generation(&state);
        let router = Arc::new(Router::new(RouterConfig::new([
            EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("ABORT"),
                move |p| {
                    callback_inbox.observe(p, generation);
                    Ok(())
                },
            ),
        ])));
        let (sent, received) = std::sync::mpsc::channel();
        router.add_side_packed("radio", move |bytes| {
            let p = sedsnet::wire_format::unpack_packet(bytes)?;
            if p.data_type() == crate::telemetry_schema::data_type("ABORT") {
                let _ = sent.send(p);
            }
            Ok(())
        });
        let expected = packet("Manual Abort physical-first probe");
        let expected_id = expected.packet_id();
        router.tx_queue(expected.clone()).unwrap();
        let held = state.gse.lock().unwrap();
        let worker_router = router.clone();
        let worker = std::thread::spawn(move || worker_router.drain_queues_with_timeout(0));
        let physical = received.recv_timeout(std::time::Duration::from_secs(1));
        drop(held);
        worker.join().unwrap().unwrap();
        let physical = physical.expect("SDK transport must run before GSE is released");
        assert_eq!(physical.packet_id(), expected_id);
        assert_eq!(physical.payload(), expected.payload());
        let mut actual_endpoints = physical
            .endpoints()
            .iter()
            .map(|endpoint| endpoint.0)
            .collect::<Vec<_>>();
        let mut expected_endpoints = expected
            .endpoints()
            .iter()
            .map(|endpoint| endpoint.0)
            .collect::<Vec<_>>();
        actual_endpoints.sort_unstable();
        expected_endpoints.sort_unstable();
        assert_eq!(actual_endpoints, expected_endpoints);
        assert!(inbox.manual_pending());
    }
    #[tokio::test]
    async fn deferred_manual_abort_cancels_active_automation_after_priority_sdk_progress() {
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Idle);
        state.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Filling;
        assert!(crate::gse::active(&state));
        let observation = Observation {
            packet: packet("Manual Abort"),
            generation: crate::gse::automatic_generation(&state),
            automatic: false,
        };
        apply(&state, &observation);
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            gse_sequence::Phase::Fault
        );
        assert!(!crate::gse::active(&state));
        assert!(state.abort_indicator_latched());
    }

    #[tokio::test]
    async fn delayed_marked_automatic_abort_cannot_change_postlaunch_state_or_publish_aborted() {
        let state = crate::state::tests::test_app_state().await;
        *state.state.lock().unwrap() = FlightState::Launch;
        state
            .launch_history
            .lock()
            .unwrap()
            .inhibit_automatic_actions()
            .unwrap();
        let mut updates = state.state_tx.subscribe();
        let observation = Observation {
            packet: packet("SEDS_AUTO_ABORT_V1:old"),
            generation: crate::gse::automatic_generation(&state),
            automatic: true,
        };
        apply(&state, &observation);
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Launch);
        assert!(!state.abort_indicator_latched());
        assert!(updates.try_recv().is_err());
        let manual = Observation {
            packet: packet("Manual Abort"),
            generation: observation.generation,
            automatic: false,
        };
        apply(&state, &manual);
        assert!(state.abort_indicator_latched());
    }
}
