//! SDK callbacks retain bounded observations only. GSE/journal/UI work happens
//! after the SDK can forward the unchanged packet to every original owner.
use crate::state::AppState;
use sedsnet::packet::Packet;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const AUTOMATIC_PREFIX: &[u8] = b"SEDS_AUTO_ABORT_V1:";
const KNOWN_MANUAL: &[u8] = b"SEDS_MANUAL_ABORT_V1:K";

/// Inspect bounded application state only. In particular, do not query the
/// SDK clock/address resolver or wait for journal I/O from an SDK callback.
/// Uncertain provenance or a busy/unhealthy journal retains Stop authority.
fn retired(state: &AppState, packet: &Packet) -> bool {
    if crate::rrls::is_rrls()
        || matches!(packet.timestamp(), 0 | u64::MAX)
        || packet.data_type() != crate::telemetry_schema::data_type("ABORT")
    {
        return false;
    }
    let known_source = if packet.payload() == KNOWN_MANUAL {
        matches!(packet.sender(), "GS" | "RRLS")
    } else if packet.payload().starts_with(AUTOMATIC_PREFIX) {
        crate::types::Board::from_sender_id(packet.sender()).is_some()
    } else {
        false
    };
    if !known_source {
        return false;
    }
    state
        .launch_history
        .try_lock()
        .ok()
        .and_then(|history| history.completed_abort_boundary())
        .is_some_and(|boundary| packet.timestamp() < boundary)
}

pub(crate) fn observe(state: &AppState, packet: &Packet, generation: u64) {
    // Retirement precedes manual_epoch/pending bookkeeping: an old packet
    // cannot cancel a new recovery merely by arriving after its generation.
    if !retired(state, packet) {
        state.deferred_abort.observe(packet, generation);
    }
}
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
    // Completion may have occurred while this observation waited for its
    // blocking worker. Never relatch a retired Abort in the new host state.
    if retired(state, &observation.packet) {
        return;
    }
    // The lease belongs to actual blocking observation work, never its awaiter.
    // Independent manual observations must still cancel recovery whileclosed.
    let _automatic_claim = if observation.automatic {
        let Some(claim) = crate::gse::claim_operator_dispatch(state, observation.generation) else {
            return;
        };
        Some(claim)
    } else {
        None
    };

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
pub(crate) struct ShutdownOutcome { pub(crate) quiesced: bool }
/// Main signals this owner only after actual SDK receiver/packet drain. Global
/// shutdown alone is not proof that no more Abort observations can arrive.
pub(crate) fn spawn(state: Arc<AppState>) -> (tokio::task::JoinHandle<ShutdownOutcome>, tokio::sync::oneshot::Sender<()>) {
    spawn_with_apply(state,apply)
}
fn spawn_with_apply(
    state:Arc<AppState>, apply:impl Fn(&Arc<AppState>,&Observation)+Send+Sync+'static,
) -> (tokio::task::JoinHandle<ShutdownOutcome>, tokio::sync::oneshot::Sender<()>) {
    let apply=Arc::new(apply);
    let (stop,mut stopping)=tokio::sync::oneshot::channel();
    let task=tokio::spawn(async move {
        let mut healthy=true;
        loop {
            if let Some(mut claim)=state.deferred_abort.claim() {
                let current=state.clone();
                let apply=apply.clone();
                let applied=tokio::task::spawn_blocking(move || {
                    apply(&current,claim.observation.as_ref().expect("claimed observation"));
                    claim.accepted=true;
                    drop(claim);
                }).await;
                if applied.is_err() {
                    healthy=false;
                    // Keep the original unaccepted observation/epoch. A failed
                    // apply cannot be relabeled completed just to seal Clean.
                    tokio::select! {
                        _=&mut stopping=>break,
                        _=tokio::time::sleep(std::time::Duration::from_millis(5))=>{},
                    }
                }
                continue;
            }
            tokio::select! {
                _=&mut stopping=>break,
                _=state.deferred_abort.wake.notified()=>{},
            }
        }
        let pending=state.deferred_abort.state.lock().unwrap();
        ShutdownOutcome { quiesced:healthy && pending.manual.is_none() && pending.automatic.is_none() && !pending.active_manual }
    });
    (task,stop)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::types::FlightState;
    pub(crate) fn assert_completed_cohort_retires_original_aborts(state: Arc<AppState>) {
        use sedsnet::router::{EndpointHandler, Router, RouterConfig};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let boundary = state
            .launch_history
            .lock()
            .unwrap()
            .completed_abort_boundary()
            .expect("actual completed healthy cohort");
        let packet_at = |sender: &str, payload: &[u8], timestamp| {
            Packet::new(
                crate::telemetry_schema::data_type("ABORT"),
                &[crate::telemetry_schema::endpoint("ABORT")],
                sender,
                timestamp,
                Arc::from(payload),
            )
            .unwrap()
        };
        let generation = crate::gse::automatic_generation(&state);
        let epoch = state.deferred_abort.manual_epoch();
        assert!(!state.deferred_abort.manual_pending());
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let observed_state = state.clone();
        let router = Router::new(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("ABORT"),
                move |packet| {
                    called.fetch_add(1, Ordering::Relaxed);
                    observe(&observed_state, packet, generation);
                    Ok(())
                },
            )])
            .with_sender("GS"),
        );
        // Distinct packet IDs pass actual SDK deduplication. Application
        // retirement, rather than recent-ID suppression, must reject both.
        let first = packet_at("GS", KNOWN_MANUAL, boundary - 1);
        let second = packet_at("GS", KNOWN_MANUAL, boundary - 1);
        assert_ne!(first.packet_id(), second.packet_id());
        for packet in [first, second] {
            router.tx_queue(packet).unwrap();
            router.drain_queues_with_timeout(0).unwrap();
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(state.deferred_abort.manual_epoch(), epoch);
        assert!(!state.deferred_abort.manual_pending());

        for sender in ["GS", "RRLS"] {
            assert!(retired(
                &state,
                &packet_at(sender, KNOWN_MANUAL, boundary - 1)
            ));
        }
        for sender in ["AB", "VB", "FC", "RF", "PB", "GB", "DAQ"] {
            let packet = packet_at(sender, b"SEDS_AUTO_ABORT_V1:old cause", boundary - 1);
            assert!(retired(&state, &packet));
            observe(&state, &packet, generation);
            assert!(state.deferred_abort.claim().is_none());
            apply(
                &state,
                &Observation {
                    packet,
                    generation,
                    automatic: true,
                },
            );
            assert!(!state.abort_indicator_latched());
        }
        // An already retained old manual observation must also be harmless
        // when its worker runs after the completed recovery.
        apply(
            &state,
            &Observation {
                packet: packet_at("GS", KNOWN_MANUAL, boundary - 1),
                generation,
                automatic: false,
            },
        );
        assert!(!state.abort_indicator_latched());
        assert_eq!(crate::gse::automatic_generation(&state), generation);

        for timestamp in [0, boundary, boundary + 1, u64::MAX] {
            for payload in [KNOWN_MANUAL, b"SEDS_AUTO_ABORT_V1:unknown".as_slice()] {
                assert!(!retired(&state, &packet_at("GS", payload, timestamp)));
            }
        }
        for (sender, payload) in [
            ("unknown", KNOWN_MANUAL),
            ("@addr:123", KNOWN_MANUAL),
            ("AB", KNOWN_MANUAL),
            ("GS", b"SEDS_MANUAL_ABORT_V1:".as_slice()),
            ("GS", b"SEDS_MANUAL_ABORT_V1:Kextra".as_slice()),
            ("GS", b"Manual Abort".as_slice()),
            ("unknown", b"SEDS_AUTO_ABORT_V1:old".as_slice()),
        ] {
            assert!(!retired(&state, &packet_at(sender, payload, boundary - 1)));
        }
        let old = packet_at("GS", KNOWN_MANUAL, boundary - 1);
        {
            let mut history = state.launch_history.lock().unwrap();
            // Busy inspection cannot block an SDK callback or fabricate proof.
            assert!(!retired(&state, &old));
            history.error = Some("injected unavailable journal".into());
        }
        assert!(!retired(&state, &old));
        state.launch_history.lock().unwrap().error = None;
        assert!(retired(&state, &old));
        let fresh = packet_at("GS", KNOWN_MANUAL, boundary);
        router.tx_queue(fresh).unwrap();
        router.drain_queues_with_timeout(0).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(state.deferred_abort.manual_epoch(), epoch + 1);
        assert!(state.deferred_abort.manual_pending());
    }
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

#[cfg(test)]
mod recovery_observation_tests {
    use super::*;
    use std::sync::atomic::Ordering;
    fn packet(automatic: bool) -> Packet {
        Packet::new(
            crate::telemetry_schema::data_type("ABORT"),
            &[crate::telemetry_schema::endpoint("ABORT")],
            "RF",
            123,
            Arc::from(if automatic {
                b"SEDS_AUTO_ABORT_V1:observation".as_slice()
            } else {
                b"Manual Stop observation".as_slice()
            }),
        )
        .unwrap()
    }
    #[tokio::test]
    async fn automatic_observation_uses_original_generation_and_cannot_publish_whileclosed() {
        let state = crate::state::tests::test_app_state().await;
        let old = crate::gse::automatic_generation(&state);
        state
            .operator_admission_barrier
            .store(77, Ordering::Release);
        let observation = Observation {
            packet: packet(true),
            generation: old,
            automatic: true,
        };
        apply(&state, &observation);
        let closed_latched = state.abort_indicator_latched();
        state.automatic_generation.store(old + 1, Ordering::Release);
        state.operator_admission_barrier.store(0, Ordering::Release);
        apply(&state, &observation);
        let stale_latched = state.abort_indicator_latched();
        let fresh = Observation {
            packet: packet(true),
            generation: old + 1,
            automatic: true,
        };
        apply(&state, &fresh);
        assert!(
            !closed_latched,
            "old actual observation published Abort policy whileclosed"
        );
        assert!(!stale_latched);
        assert!(state.abort_indicator_latched());
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
    }
    #[tokio::test]
    async fn actual_manual_observation_lane_remains_retained_and_applies_whileclosed() {
        let state = crate::state::tests::test_app_state().await;
        state
            .operator_admission_barrier
            .store(77, Ordering::Release);
        state
            .deferred_abort
            .observe(&packet(false), crate::gse::automatic_generation(&state));
        assert_eq!(state.deferred_abort.manual_epoch(), 1);
        let mut claim = state.deferred_abort.claim().unwrap();
        apply(&state, claim.observation.as_ref().unwrap());
        claim.accepted = true;
        drop(claim);
        assert!(state.abort_indicator_latched());
        assert!(!state.deferred_abort.manual_pending());
        assert_eq!(state.deferred_abort.manual_epoch(), 1);
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
    }
}

#[cfg(test)]
mod shutdown_receipt_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool,Ordering};
    use std::time::Duration;
    fn manual() -> Packet {
        Packet::new(crate::telemetry_schema::data_type("ABORT"),&[crate::telemetry_schema::endpoint("ABORT")],"GS",123456,Arc::from(KNOWN_MANUAL)).unwrap()
    }
    #[tokio::test]
    async fn global_shutdown_does_not_discard_late_manual_observation_or_pending_apply() {
        struct Release(Arc<(Mutex<bool>,std::sync::Condvar)>);
        impl Drop for Release { fn drop(&mut self) { *self.0.0.lock().unwrap()=true; self.0.1.notify_all(); } }
        let state=crate::state::tests::test_app_state().await;
        let release=Release(Arc::new((Mutex::new(false),std::sync::Condvar::new())));
        let work=release.0.clone();
        let entered=Arc::new(AtomicBool::new(false)); let signaled=entered.clone();
        let (mut task,stop)=spawn_with_apply(state.clone(),move |_,_| {
            signaled.store(true,Ordering::Release);
            let (lock,wake)=&*work; let mut ready=lock.lock().unwrap();
            while !*ready { ready=wake.wait(ready).unwrap(); }
        });
        tokio::task::yield_now().await;
        state.request_shutdown(); // SDK receivers have NOT yet joined.
        tokio::task::yield_now().await;
        assert!(!task.is_finished(),"global shutdown retired observer before receiver quiescence");
        state.deferred_abort.observe(&manual(),0);
        tokio::time::timeout(Duration::from_secs(2),async {while !entered.load(Ordering::Acquire) {tokio::task::yield_now().await;}}).await.unwrap();
        assert!(state.deferred_abort.manual_pending());
        assert!(state.deferred_abort.state.try_lock().is_ok(),"apply retained inbox mutex");
        let epoch=state.deferred_abort.manual_epoch();
        stop.send(()).unwrap(); // Test has now stopped its synthetic receiver.
        assert!(tokio::time::timeout(Duration::from_millis(5),&mut task).await.is_err(),"outer owner claimed completion while inner apply was live");
        let before=state.manual_abort_intent.epoch(); state.manual_abort_intent.request();
        assert!(state.manual_abort_intent.epoch()>before);
        drop(release);
        assert!(task.await.unwrap().quiesced);
        assert!(!state.deferred_abort.manual_pending());
        assert_eq!(state.deferred_abort.manual_epoch(),epoch,"join silently renewed manual authority");
    }
    #[tokio::test]
    async fn inner_blocking_panic_cannot_be_hidden_by_outer_ok_or_clear_manual_authority() {
        let state=crate::state::tests::test_app_state().await;
        let entered=Arc::new(AtomicBool::new(false));let signaled=entered.clone();
        let (task,stop)=spawn_with_apply(state.clone(),move |_,_| {signaled.store(true,Ordering::Release);panic!("mock blocking manual apply panic");});
        state.deferred_abort.observe(&manual(),0);
        tokio::time::timeout(Duration::from_secs(2),async{while !entered.load(Ordering::Acquire){tokio::task::yield_now().await;}}).await.unwrap();
        let epoch=state.deferred_abort.manual_epoch();
        stop.send(()).unwrap();
        assert!(!task.await.unwrap().quiesced);
        assert!(state.deferred_abort.manual_pending(),"failed apply erased retained manual observation");
        assert_eq!(state.deferred_abort.manual_epoch(),epoch);
    }
}
