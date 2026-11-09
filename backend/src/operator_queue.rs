//! Receiver-owned host queue drain prerequisite. It never clears SDK work,
//! reopens admission, advances an epoch, or certifies board/proxy quiescence.
use crate::{state::AppState, types::TelemetryCommand};
use std::sync::{Mutex, atomic::{AtomicU64, Ordering}};
pub(crate) mod safe_manual;
pub(crate) mod manual_origin;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Token {
    pub(crate) owner: u64,
    pub(crate) generation: u64,
    pub(crate) manual_epoch: u64,
    pub(crate) observed_manual_epoch: u64,
    pub(crate) relief_epoch: u64,
}
impl Token {
    pub(crate) fn capture(state: &AppState, owner: u64) -> Self {
        Self {
            owner,
            generation: crate::gse::automatic_generation(state),
            manual_epoch: state.manual_abort_intent.epoch(),
            observed_manual_epoch: state.deferred_abort.manual_epoch(),
            relief_epoch: state.operator_queue.relief_epoch(),
        }
    }
}
pub(crate) struct Request {
    token: Token,
    reply: oneshot::Sender<Result<usize, &'static str>>,
}
pub(crate) struct Control {
    relief_epoch: AtomicU64,
    // One atomic observation: monotonic validity deadline << 1 | reported active.
    fill_launch_lease: AtomicU64,
    fill_launch_history: Mutex<(crate::gse::ValveCaptureHistory, Option<u64>)>,
    fill_launch_clock: std::time::Instant,
    // Capacity-one administration is independent of ordinary serde commands.
    sender: mpsc::Sender<Request>,
    receiver: Mutex<Option<mpsc::Receiver<Request>>>,
}
impl Default for Control {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel(1);
        Self {
            relief_epoch: AtomicU64::new(0),
            fill_launch_lease: AtomicU64::new(0),
            fill_launch_history: Mutex::new(Default::default()),
            fill_launch_clock: std::time::Instant::now(),
            sender,
            receiver: Mutex::new(Some(receiver)),
        }
    }
}
impl Control {
    /// Expiry means unknown, never completed. Only a fresh positive board
    /// observation owns the temporary manual fill-system exclusion.
    pub(crate) fn fill_launch_activity(&self)->Option<bool> {
        let lease=self.fill_launch_lease.load(Ordering::Acquire);
        let now=u64::try_from(self.fill_launch_clock.elapsed().as_millis()).ok()?;
        (lease!=0 && now<=lease>>1).then_some(lease&1!=0)
    }
    pub(crate) fn fill_launch_active(&self)->bool {self.fill_launch_activity()==Some(true)}
    pub(crate) fn observe_fill_launch_active(&self,active:bool,source_ms:i64,packet_id:u64,receipt:crate::gse::ValveReceipt)->bool {
        let Some(now)=receipt.monotonic_at.checked_duration_since(self.fill_launch_clock).and_then(|d|u64::try_from(d.as_millis()).ok()) else {return false;};
        let mut history=self.fill_launch_history.lock().unwrap();
        let mut candidate=history.0;
        let Some(at)=candidate.admit(source_ms,receipt.network_ms,now,active,packet_id,history.1) else {return false;};
        let processing=u64::try_from(self.fill_launch_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        if !processing.checked_sub(at).is_some_and(|age|age<=2000) {return false;}
        // Original source/receipt age counts against the lease. Re-reading,
        // processing a duplicate, or a policy refresh cannot renew it.
        let Some(lease)=at.checked_add(5000).and_then(|until|until.checked_mul(2)) else {return false;};
        *history=(candidate,Some(at));
        self.fill_launch_lease.store(lease|u64::from(active),Ordering::Release);
        true
    }

    pub(crate) fn relief_epoch(&self) -> u64 { self.relief_epoch.load(Ordering::Acquire) }
    // Called only under the dispatch owner. Exhaustion permanently refuses renewal.
    pub(crate) fn advance_relief_epoch(&self, expected: u64) -> bool {
        expected.checked_add(1).is_some_and(|next| self.relief_epoch.compare_exchange(expected, next, Ordering::AcqRel, Ordering::Acquire).is_ok())
    }
    /// Undo [`Self::advance_relief_epoch`] for an offer the SDK never admitted
    /// (its core was held), so the same intent can be offered again. Called
    /// only under the dispatch owner that advanced it.
    pub(crate) fn restore_relief_epoch(&self, expected: u64) {
        if let Some(next) = expected.checked_add(1) {
            let _ = self.relief_epoch.compare_exchange(next, expected, Ordering::AcqRel, Ordering::Acquire);
        }
    }
    pub(crate) fn take_receiver(&self) -> Option<mpsc::Receiver<Request>> {
        self.receiver.lock().unwrap().take()
    }
    // The owned coordinator requests service from the actual telemetry receiver.
    pub(crate) fn request(
        &self,
        token: Token,
    ) -> Result<oneshot::Receiver<Result<usize, &'static str>>, ()> {
        let (reply, receipt) = oneshot::channel();
        self.sender
            .try_send(Request { token, reply })
            .map_err(|_| ())?;
        Ok(receipt)
    }
}

pub(crate) fn serve(
    state: &AppState,
    commands: &mut mpsc::Receiver<TelemetryCommand>,
    request: Request,
) {
    if request.reply.is_closed() {
        return;
    }
    let drained = crate::gse::with_closed_operator_queue(state, request.token, || {
        // Drain the bounded snapshot already present, not a producer-fed loop.
        let present = commands.len();
        let mut count = 0;
        let mut manual_abort = false;
        for _ in 0..present {
            let Ok(command) = commands.try_recv() else {
                break;
            };
            manual_abort |= matches!(&command, TelemetryCommand::Abort)
                || matches!(&command, TelemetryCommand::QueuedOperator { command, .. }
                    if matches!(**command, TelemetryCommand::Abort));
            count += 1;
        }
        (count, manual_abort)
    });
    let result = match drained {
        Some((_, true)) => {
            // Outside the Stop locks: retain explicit Stop independently and
            // invalidate this drain token rather than erasing its intent.
            state.manual_abort_intent.request();
            Err("manual Stop retained; drain receipt invalidated")
        }
        Some((count, false)) => {
            crate::gse::with_closed_operator_queue(state, request.token, || count)
                .ok_or("drain fence changed")
        }
        None => Err("drain requires current closed owner, quiescent claims and unchanged Stop"),
    };
    // Cancellation after service cannot reopen or advance anything. Even a
    // successful receipt requires the same fence at coordinator finalization;
    // an older tagged producer may enqueue after this bounded snapshot.
    let _ = request.reply.send(result);
}/// File-mutating regressions always own their process configuration. They must
/// be safe under plain `cargo test`, without editing operator files or changing
/// process-global environment while unrelated tests run.
#[cfg(test)]
pub(crate) fn isolated_case(module: &str, name: &str) -> bool {
    isolated_case_inner(module, name, false, None)
}
#[cfg(test)]
pub(crate) fn isolated_case_with_capture_clock(module: &str, name: &str) -> bool {
    isolated_case_inner(module, name, true, None)
}
#[cfg(test)]
pub(crate) fn isolated_simulated_case(module:&str,name:&str)->bool {
    isolated_case_inner(module,name,false,Some(true))
}
#[cfg(test)]
pub(crate) fn isolated_native_case(module: &str, name: &str) -> bool {
    isolated_case_inner(module,name,false,Some(false))
}
#[cfg(test)]
fn isolated_case_inner(module: &str, name: &str, capture_clock: bool, simulated:Option<bool>) -> bool {
    let module = module.split_once("::").map_or(module, |(_, path)| path);
    let case = format!("{module}::{name}");
    if std::env::var("GS_RECOVERY_ISOLATED_CASE").as_deref() == Ok(case.as_str()) {
        let root = std::path::PathBuf::from(
            std::env::var_os("GS_RECOVERY_ISOLATED_ROOT").expect("private test root required"),
        );
        let canonical = root.canonicalize().unwrap();
        assert!(canonical.starts_with(std::env::temp_dir().canonicalize().unwrap()));
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("gs-recovery-test-")
        );
        for key in [
            "GS_LOADCELL_CALIBRATION_PATH",
            "GS_CHECKLISTS_FILE",
            "GS_NETWORK_VARIABLE_CACHE",
            "GS_LAUNCH_CLOCK_STATE",
            "GS_FLIGHT_SETUP_CONFIG",
            "GS_FILL_TARGETS_CONFIG",
            "GS_GSE_CONFIG",
            "GS_AUTO_ZERO_CONFIG",
            "GS_CALIBRATION_CONFIG_PATH",
            "GS_STATION_POSITIONS_PATH",
            "GS_GSE_QUALIFICATION",
            "GS_ZERO_CAPTURE_DIR",
        ] {
            let path = std::path::PathBuf::from(
                std::env::var_os(key).expect("isolated configuration path required"),
            );
            assert!(
                path.starts_with(&root),
                "test must never write operator configuration: {key}"
            );
        }
        return false;
    }
    let directory =
        std::env::temp_dir().join(format!("gs-recovery-test-{}", rand::random::<u128>()));
    let config = directory.join("config");
    std::fs::create_dir_all(&config).unwrap();
    for name in [
        "flight_setup.json",
        "fill_targets.json",
        "gse_sequence.json",
        "calibration_config.json",
        "station_positions.json",
    ] {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("config")
            .join(name);
        if source.is_file() {
            std::fs::copy(source, config.join(name)).unwrap();
        }
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &case, "--nocapture", "--test-threads=1"])
        .env("GS_RECOVERY_ISOLATED_CASE", &case)
        .env("GS_RECOVERY_ISOLATED_ROOT", &directory)
        .env("GS_RRLS_ROLE", "primary")
        .env("GS_STATION_ROLE", "groundstation")
        .env_remove("GS_RRLS")
        .env(
            "GS_STATION_PAIRING_FILE",
            directory.join("absent-pairing.json"),
        );
    if let Some(simulated)=simulated { command.env("GS_SIM_MODE", if simulated {"1"} else {"0"}); }
    for (key, path) in [
        ("GS_LOADCELL_CALIBRATION_PATH", "loadcell.json"),
        ("GS_NETWORK_VARIABLE_CACHE", "variables.json"),
        ("GS_LAUNCH_CLOCK_STATE", "launch.json"),
        ("GS_CHECKLISTS_FILE", "checklists.json"),
        ("GS_FLIGHT_SETUP_CONFIG", "config/flight_setup.json"),
        ("GS_FILL_TARGETS_CONFIG", "config/fill_targets.json"),
        ("GS_GSE_CONFIG", "config/gse_sequence.json"),
        ("GS_AUTO_ZERO_CONFIG", "config/auto_zero.json"),
        (
            "GS_CALIBRATION_CONFIG_PATH",
            "config/calibration_config.json",
        ),
        ("GS_STATION_POSITIONS_PATH", "config/station_positions.json"),
        ("GS_GSE_QUALIFICATION", "config/qualification.json"),
        ("GS_ZERO_CAPTURE_DIR", "zero-captures"),
    ] {
        command.env(key, directory.join(path));
    }
    if capture_clock {
        let clock = directory.join("capture-clock");
        std::fs::write(&clock, "100000").unwrap();
        command.env("GS_SIM_CLOCK_FILE", clock).env("GS_SIM_NETWORK_EPOCH_MS", "1767225600000");
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exact_one = stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;");
    if output.status.success() && exact_one {
        let _ = std::fs::remove_dir_all(&directory);
    }
    assert!(
        output.status.success() && exact_one,
        "isolated actual regression {case} failed or did not execute once (files retained at {directory:?}):\n{stdout}\n{stderr}"
    );
    eprintln!("isolated actual regression {case}: 1 passed, private configuration");
    if !stderr.is_empty() {
        eprintln!("{stderr}");
    }
    true
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::Ordering};
    fn close(state: &AppState) -> Token {
        state
            .operator_admission_barrier
            .store(77, Ordering::Release);
        Token::capture(state, 77)
    }
    async fn request(
        state: &AppState,
        commands: &mut mpsc::Receiver<TelemetryCommand>,
        token: Token,
    ) -> Result<usize, &'static str> {
        let mut admin = state.operator_queue.take_receiver().unwrap();
        let receipt = state.operator_queue.request(token).unwrap();
        serve(state, commands, admin.recv().await.unwrap());
        receipt.await.unwrap()
    }
    #[tokio::test]
    async fn bounded_receiver_drains_only_closed_current_owner_without_reopening() {
        let state = crate::state::tests::test_app_state().await;
        let (tx, mut commands) = mpsc::channel(4);
        let token = close(&state);
        tx.try_send(TelemetryCommand::Postinit).unwrap(); // defense-in-depth fixture, not a production sender
        tx.try_send(TelemetryCommand::AutomaticNitrogenClose {
            generation: token.generation,
        })
        .unwrap();
        assert_eq!(request(&state, &mut commands, token).await, Ok(2));
        assert!(commands.try_recv().is_err());
        assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 77);
        assert_eq!(crate::gse::automatic_generation(&state), token.generation);
        assert!(crate::gse::with_closed_operator_queue(&state, token, || ()).is_some());
        state
            .operator_admission_barrier
            .store(78, Ordering::Release);
        assert!(crate::gse::with_closed_operator_queue(&state, token, || ()).is_none());
        assert!(state.operator_queue.take_receiver().is_none());
    }
    #[tokio::test]
    async fn drain_refuses_active_claim_new_stop_and_stale_generation_or_owner() {
        let state = crate::state::tests::test_app_state().await;
        let generation = crate::gse::automatic_generation(&state);
        let claim = crate::gse::claim_operator_dispatch(&state, generation).unwrap();
        let token = close(&state);
        assert!(
            crate::gse::with_closed_operator_queue(&state, token, || panic!("not quiescent"))
                .is_none()
        );
        drop(claim);
        assert!(
            crate::gse::with_closed_operator_queue(&state, Token { owner: 76, ..token }, || ())
                .is_none()
        );
        state
            .automatic_generation
            .store(generation + 1, Ordering::Release);
        assert!(crate::gse::with_closed_operator_queue(&state, token, || ()).is_none());
        state
            .automatic_generation
            .store(generation, Ordering::Release);
        state.manual_abort_intent.request();
        assert!(crate::gse::with_closed_operator_queue(&state, token, || ()).is_none());
        assert!(state.manual_abort_intent.pending_epoch().is_some());
    }
    #[tokio::test]
    async fn queued_explicit_stop_is_retained_independently_and_invalidates_receipt() {
        let state = crate::state::tests::test_app_state().await;
        let token = close(&state);
        let (tx, mut commands) = mpsc::channel(2);
        tx.try_send(TelemetryCommand::Abort).unwrap();
        tx.try_send(TelemetryCommand::Postinit).unwrap();
        assert!(request(&state, &mut commands, token).await.is_err());
        assert!(state.manual_abort_intent.pending_epoch().is_some());
        assert!(crate::gse::with_closed_operator_queue(&state, token, || ()).is_none());
        assert!(commands.try_recv().is_err());
    }
    #[tokio::test]
    async fn cancellation_and_capacity_never_create_a_reopen_receipt() {
        let state = crate::state::tests::test_app_state().await;
        let token = close(&state);
        let mut admin = state.operator_queue.take_receiver().unwrap();
        let (tx, mut commands) = mpsc::channel(1);
        tx.try_send(TelemetryCommand::Postinit).unwrap();
        let cancelled = state.operator_queue.request(token).unwrap();
        assert!(state.operator_queue.request(token).is_err());
        drop(cancelled);
        serve(&state, &mut commands, admin.recv().await.unwrap());
        assert_eq!(commands.len(), 1);
        let receipt = state.operator_queue.request(token).unwrap();
        serve(&state, &mut commands, admin.recv().await.unwrap());
        drop(receipt); // cancellation after the actual drain still cannot reopen
        assert!(commands.try_recv().is_err());
        state.manual_abort_intent.request();
        assert!(crate::gse::with_closed_operator_queue(&state, token, || ()).is_none());
        assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 77);
    }
    #[tokio::test]
    async fn successful_autozero_callback_cannot_mutate_fill_after_admission_closes() {
        let state = crate::state::tests::test_app_state().await;
        *state.latest_fill_mass_kg.lock().unwrap() = Some(12.0);
        let token = close(&state);
        crate::gse::start_fill_after_automatic_zero(&state, token.generation);
        assert_eq!(*state.latest_fill_mass_kg.lock().unwrap(), Some(12.0));
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
    }
    #[tokio::test]
    async fn actual_worker_services_admin_before_ordinary_queue_and_rejects_late_old_tag() {
        let mut state = crate::state::tests::test_app_state().await;
        let (tx, commands) = mpsc::channel(4);
        Arc::get_mut(&mut state).unwrap().cmd_tx = tx.clone();
        let (db_tx, db_rx) = mpsc::channel(8);
        let token = close(&state);
        tx.try_send(TelemetryCommand::QueuedOperator {
            recovery_origin: None,
            launch_revision: None,
            generation: token.generation,
            command: Box::new(TelemetryCommand::Launch),
            plumbing_origin: None,
        })
        .unwrap();
        let receipt = state.operator_queue.request(token).unwrap();
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]),
        ));
        let worker = tokio::spawn(crate::telemetry_task::telemetry_task(
            state.clone(),
            router,
            vec![],
            commands,
            db_rx,
            state.shutdown_subscribe(),
        ));
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), receipt).await;
        if !matches!(result, Ok(Ok(Ok(1)))) {
            state.request_shutdown();
            worker.await.unwrap();
            panic!("actual receiver must drain before ordinary dispatch: {result:?}");
        }
        // Test-modeled verified epoch/reopen boundary; no recovery API is enabled.
        state
            .automatic_generation
            .store(token.generation + 1, Ordering::Release);
        state.operator_admission_barrier.store(0, Ordering::Release);
        tx.send(TelemetryCommand::QueuedOperator {
            recovery_origin: None,
            launch_revision: None,
            generation: token.generation,
            command: Box::new(TelemetryCommand::Launch),
            plumbing_origin: None,
        })
        .await
        .unwrap();
        tx.send(TelemetryCommand::QueuedOperator {
            recovery_origin: None,
            launch_revision: None,
            generation: token.generation + 1,
            command: Box::new(TelemetryCommand::Postinit),
            plumbing_origin: None,
        })
        .await
        .unwrap();
        let progress = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !state
                .last_command_ms
                .lock()
                .unwrap()
                .contains_key("Postinit")
            {
                tokio::task::yield_now().await;
            }
        })
        .await;
        state.request_shutdown();
        worker.await.unwrap();
        drop(db_tx);
        progress.unwrap();
        assert!(!state.last_command_ms.lock().unwrap().contains_key("Launch"));
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
    }
    fn router() -> sedsnet::router::Router {
        sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]))
    }
    #[tokio::test]
    async fn closed_barrier_rejects_late_automatic_supply_admission() {
        let state = crate::state::tests::test_app_state().await;
        let token = close(&state);
        assert!(
            !crate::gse::queue_automatic_supply_close(&state, &router(), 12, token.generation)
                .unwrap()
        );
    }
    #[tokio::test]
    async fn closed_barrier_rejects_late_automatic_abort_admission() {
        let state = crate::state::tests::test_app_state().await;
        let token = close(&state);
        assert!(
            !crate::gse::queue_automatic_abort_for_generation(
                &state,
                &router(),
                token.generation,
                "closed admission control"
            )
            .unwrap()
        );
    }
    #[tokio::test]
    async fn closed_barrier_rejects_late_calibration_publication() {
        let state = crate::state::tests::test_app_state().await;
        let network = Arc::new(router());
        state.topology_router.set(network.clone()).unwrap();
        let ty = crate::telemetry_schema::data_type("DAQ_LOADCELL_CALIBRATION");
        assert!(network.get_cached_network_variable(ty).unwrap().is_none());
        let token = close(&state);
        let result = crate::gse::sync_calibration_after_automatic_zero(
            &state,
            token.generation,
            &crate::loadcell::LoadcellCalibrationFile::default(),
        );
        assert!(
            network.get_cached_network_variable(ty).unwrap().is_none(),
            "closed callback must not publish actual SDK calibration cache"
        );
        assert!(result.is_err());
    }
    #[tokio::test]
    async fn fresh_open_callbacks_keep_success_but_unowned_zero_is_inert() {
        let state = crate::state::tests::test_app_state().await;
        let generation = crate::gse::automatic_generation(&state);
        *state.latest_fill_mass_kg.lock().unwrap() = Some(12.0);
        assert!(state.fill_recording.continuation.lock().unwrap().is_none());
        crate::gse::start_fill_after_automatic_zero(&state, generation);
        // A generation alone cannot create the accepted recording/zero owner.
        // Actual owned >=5s/<7s completion is exercised in fill_recording tests.
        assert_eq!(*state.latest_fill_mass_kg.lock().unwrap(), Some(12.0));
        assert!(state.fill_recording.continuation.lock().unwrap().is_none());
        assert!(
            crate::gse::queue_automatic_supply_close(&state, &router(), 12, generation).unwrap()
        );
        assert!(
            crate::gse::queue_automatic_abort_for_generation(
                &state,
                &router(),
                generation,
                "fresh positive"
            )
            .unwrap()
        );
        let network = Arc::new(router());
        state.topology_router.set(network.clone()).unwrap();
        assert!(
            crate::gse::sync_calibration_after_automatic_zero(
                &state,
                generation,
                &crate::loadcell::LoadcellCalibrationFile::default()
            )
            .is_ok()
        );
        assert!(
            network
                .get_cached_network_variable(crate::telemetry_schema::data_type(
                    "DAQ_LOADCELL_CALIBRATION"
                ))
                .unwrap()
                .is_some()
        );
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
    }
}

#[cfg(test)]
mod fill_launch_lease_tests {
    use super::*;
    #[tokio::test]
    async fn expired_actual_active_status_releases_manual_policy_without_resetting_launch() {
        let mut state=crate::state::tests::test_app_state().await;
        state.action_policy.lock().unwrap().software_buttons_enabled=true;
        state.launch_history.lock().unwrap().inhibit_automatic_actions().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let packet=sedsnet::packet::Packet::new(crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],"VB",
            crate::telemetry_task::get_current_timestamp_ms(),std::sync::Arc::from(&[6,1][..])).unwrap();
        crate::telemetry_task::apply_umbilical_status_at_ingress(&state,&packet);
        assert_eq!(state.operator_queue.fill_launch_activity(),Some(true));
        assert!(!state.is_command_allowed(&TelemetryCommand::NitrogenClose));
        let clock=state.launch_history.lock().unwrap().snapshot();
        let raw=state.operator_queue.fill_launch_lease.load(Ordering::Acquire);
        let app=std::sync::Arc::get_mut(&mut state).expect("isolated state has no background owner");
        std::sync::Arc::get_mut(&mut app.operator_queue).expect("isolated control has no background owner").fill_launch_clock-=std::time::Duration::from_secs(6);
        assert_eq!(state.operator_queue.fill_launch_activity(),None);
        assert!(state.is_command_allowed(&TelemetryCommand::NitrogenClose));
        assert_eq!(state.operator_queue.fill_launch_lease.load(Ordering::Acquire),raw);
        let history=state.launch_history.lock().unwrap();
        assert_eq!(history.snapshot(),clock,"expiry cannot reset the launch clock or history");
        assert!(history.automatic_actions_inhibited(),"retained launch protocol remains independent");
    }

    #[test]
    fn active_fill_lease_expires_to_unknown_without_completion_or_replay_floor_reset() {
        let mut control=Control::default();
        let source=1_793_563_200_000_i64;
        let receipt=crate::gse::ValveReceipt {network_ms:source,monotonic_at:std::time::Instant::now(),simulation_ms:None};
        assert!(control.observe_fill_launch_active(true,source,41,receipt));
        assert_eq!(control.fill_launch_activity(),Some(true));
        let retained_lease=control.fill_launch_lease.load(Ordering::Acquire);
        let retained_floor=*control.fill_launch_history.lock().unwrap();
        // Advance only this isolated control's elapsed monotonic clock. Keep
        // the reported wire value, source UTC, packet identity and floor.
        control.fill_launch_clock-=std::time::Duration::from_secs(6);
        assert_eq!(control.fill_launch_activity(),None);
        assert!(!control.fill_launch_active(),"unknown activity must not indefinitely lock manual controls");
        assert_eq!(control.fill_launch_lease.load(Ordering::Acquire),retained_lease);
        assert_eq!(*control.fill_launch_history.lock().unwrap(),retained_floor);
        assert_eq!(retained_lease&1,1,"expiry did not manufacture an inactive report");
        let late=crate::gse::ValveReceipt {monotonic_at:std::time::Instant::now(),..receipt};
        assert!(!control.observe_fill_launch_active(false,source-1,42,late));
        assert_eq!(control.fill_launch_activity(),None);
        assert!(!control.observe_fill_launch_active(true,source,41,late),"a retransmission cannot renew the expired lease");
        assert!(control.observe_fill_launch_active(false,source+1,43,crate::gse::ValveReceipt{network_ms:source+1,..late}));
        assert_eq!(control.fill_launch_activity(),Some(false));
    }
}
