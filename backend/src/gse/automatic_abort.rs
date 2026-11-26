//! Automatic-abort ownership. An automatic abort is claimed and sent without
//! the GSE lock, the dispatch gate or a disk: all three can be held by a write
//! that does not finish. The claim is latched in RAM first (Launch and Fire are
//! refused from then on), the ABORT is sent, and the durable record follows on
//! a thread of its own.
//!
//! Against a Launch: both are admitted under this owner's RAM lock, so one is
//! always first. A claim that is first refuses the Launch. A Launch that is
//! first holds `LaunchHold` from before its first write until its outcome can
//! be read (the Fire stage, or the journal's inhibition): until then an abort
//! is neither sent nor refused (`Decision::LaunchSaving`), and is decided
//! again afterwards. Automatic aborts stop at Launch admission; a Launch whose
//! save fails was not one.
use super::*;
use std::sync::{MutexGuard, PoisonError, TryLockError, mpsc};

/// Test seam: work another thread does at one point of this thread's path.
#[cfg(test)]
pub(crate) mod between {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Point {
        /// A claim has read the journal and the flight state, not taken yet.
        ClaimTaken,
        /// Fire passed its checks and is about to be admitted.
        FireAdmitted,
        /// The overpressure monitor read whether a Launch is on, not decided yet.
        OverpressureDecided,
    }
    type Work = Box<dyn FnOnce() + Send>;
    thread_local! {
        static AT: std::cell::RefCell<Option<(Point, Work)>> = const { std::cell::RefCell::new(None) };
    }
    /// Once, on this thread, at `point`: `work` runs on a thread of its own and
    /// is waited for.
    pub(crate) fn at(point: Point, work: impl FnOnce() + Send + 'static) {
        AT.with(|at| *at.borrow_mut() = Some((point, Box::new(work))));
    }
    pub(crate) fn here(point: Point) {
        let work = AT.with(|at| {
            let mut at = at.borrow_mut();
            at.as_ref().is_some_and(|(at, _)| *at == point).then(|| at.take().unwrap().1)
        });
        if let Some(work) = work {
            crate::runtime_mode::spawn_thread("between", work).unwrap().join().unwrap();
        }
    }
}

/// Bounded RAM only; a panic inside leaves nothing half written.
fn ram<T>(lock: &std::sync::Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}
type Record = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Claims {
    /// A claim, or a Clear of one, that is not finished: until it is, its
    /// record is not on disk. Holds Launch, Fire, Clear and Reset.
    in_flight: bool,
    /// Claimed, and the runtime does not say so yet.
    unapplied: bool,
    /// Launches being saved now, and all there ever were.
    saving: usize,
    launches: u64,
    /// A station mode change is replacing the runtime's qualification.
    switching: bool,
}
/// The runtime's qualification as of the last release of the GSE lock.
#[derive(Default)]
struct Published {
    claimed: bool,
    fire_committed: bool,
}
#[derive(Default)]
pub(crate) struct AbortOwner {
    claims: std::sync::Mutex<Claims>,
    published: std::sync::Mutex<Published>,
    finished: std::sync::Condvar,
    records: std::sync::Mutex<Option<mpsc::Sender<Record>>>,
    /// Cycles the abort watch finished deciding.
    #[cfg(test)]
    pub(crate) cycles: std::sync::atomic::AtomicUsize,
}
impl AbortOwner {
    pub(super) fn adopt(&self, rt: &mut Runtime) {
        let mut claims = ram(&self.claims);
        if std::mem::take(&mut claims.unapplied) {
            latch(rt);
            ram(&self.published).claimed = true;
        }
    }
    pub(super) fn publish(&self, rt: &Runtime) {
        let mut published = ram(&self.published);
        published.claimed = rt.operational.saved.automatic_abort_claimed;
        published.fire_committed =
            rt.operational.saved.qualification.stage() == gse_sequence::qualification::MissionStage::Fire;
    }
    /// A claim being sent or recorded, or a Clear being saved.
    pub(crate) fn in_flight(&self) -> bool {
        ram(&self.claims).in_flight
    }
    /// A Launch is being saved: whether it is one is not decided yet.
    pub(super) fn launch_saving(&self) -> bool {
        ram(&self.claims).saving != 0
    }
    /// The Launches admitted so far. None while one is being saved.
    pub(super) fn launches(&self) -> Option<u64> {
        let claims = ram(&self.claims);
        (claims.saving == 0).then_some(claims.launches)
    }
    pub(super) fn fire_committed(&self) -> bool {
        !crate::runtime_mode::is_hitl() && ram(&self.published).fire_committed
    }
    fn claimed(&self, claims: &Claims) -> bool {
        claims.unapplied || ram(&self.published).claimed
    }
    fn open(&self, claims: &Claims, hitl: bool) -> Result<u64, Closed> {
        if claims.in_flight || (!hitl && self.claimed(claims)) {
            Err(Closed::Refused)
        } else if claims.saving != 0 || claims.switching {
            Err(Closed::Later)
        } else if !hitl && ram(&self.published).fire_committed {
            Err(Closed::Refused)
        } else {
            Ok(claims.launches)
        }
    }
    /// What an abort may still become: the Launches admitted so far, to be
    /// compared when it is taken.
    fn check(&self, hitl: bool) -> Result<u64, Closed> {
        self.open(&ram(&self.claims), hitl)
    }
    fn take(&self, hitl: bool, launches: u64, token: crate::runtime_mode::ModeToken) -> Result<bool, Closed> {
        let mut claims = ram(&self.claims);
        // Still the mode it was decided in: a switch that is not excluded by
        // `open` either has not stored its mode yet, or is finished.
        if self.open(&claims, hitl)? != launches || !token.is_current() {
            return Ok(false);
        }
        claims.in_flight = true;
        claims.unapplied = !hitl;
        Ok(true)
    }
    /// Admit a Launch before its first write. `fire`: a claim that was never
    /// cleared refuses it too.
    pub(super) fn admit_launch(self: &Arc<Self>, fire: bool) -> Option<LaunchHold> {
        let mut claims = ram(&self.claims);
        if claims.in_flight || (fire && self.claimed(&claims)) {
            return None;
        }
        claims.saving += 1;
        claims.launches += 1;
        Some(LaunchHold(self.clone()))
    }
    pub(super) fn launch_refused(&self, fire: bool) -> bool {
        let claims = ram(&self.claims);
        claims.in_flight || (fire && self.claimed(&claims))
    }
    /// For a Clear, which excludes new aborts and Fire while it runs.
    fn reserve(&self) -> bool {
        !std::mem::replace(&mut ram(&self.claims).in_flight, true)
    }
    fn release(&self) {
        ram(&self.claims).in_flight = false;
        self.finished.notify_all();
    }
    /// Run on the recorder thread, in order. It may wait on a disk.
    pub(crate) fn record(&self, record: impl FnOnce() + Send + 'static) {
        let mut records = ram(&self.records);
        if records.is_none() {
            *records = start_recorder();
        }
        let record: Record = Box::new(record);
        let refused = match records.as_ref() {
            Some(sender) => sender.send(record).err().map(|unsent| unsent.0),
            None => Some(record),
        };
        drop(records);
        // No thread could be started: the ABORT is already sent.
        if let Some(record) = refused {
            record();
        }
    }
    /// Before it is needed, so that an abort starts no thread.
    pub(crate) fn start(&self) {
        let mut records = ram(&self.records);
        if records.is_none() {
            *records = start_recorder();
        }
    }
    /// Wait until no claim or Clear is unfinished. False: not in time.
    pub(crate) fn finished_within(&self, timeout: std::time::Duration) -> bool {
        let (claims, timeout) = self
            .finished
            .wait_timeout_while(ram(&self.claims), timeout, |claims| claims.in_flight)
            .unwrap_or_else(PoisonError::into_inner);
        drop(claims);
        !timeout.timed_out()
    }
    #[cfg(test)]
    pub(crate) fn settle(&self) {
        assert!(self.finished_within(std::time::Duration::from_secs(30)), "the automatic abort was not recorded");
    }
}
fn start_recorder() -> Option<mpsc::Sender<Record>> {
    let (sender, records) = mpsc::channel::<Record>();
    std::thread::Builder::new()
        .name("auto_abort_record".to_string())
        .spawn(move || {
            for record in records {
                // A failed record must not cost the next one.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(record));
            }
        })
        .inspect_err(|error| log::error!("Automatic abort recorder thread could not be started: {error}"))
        .ok()?;
    Some(sender)
}
enum Closed {
    Refused,
    Later,
}
fn latch(rt: &mut Runtime) {
    rt.operational.saved.automatic_abort_claimed = true;
    rt.operational.running = false;
    rt.engine.claimed = true;
    rt.engine.status.phase = Phase::Fault;
    rt.engine.status.message =
        "Automatic abort requested; clear GSE abort before another operation".into();
}

/// A Launch whose outcome is not decided yet. Declared before the runtime
/// guard, so that the guard publishes the outcome before this is released.
pub(super) struct LaunchHold(Arc<AbortOwner>);
impl Drop for LaunchHold {
    fn drop(&mut self) {
        ram(&self.0.claims).saving -= 1;
    }
}
/// Keeps automatic aborts undecided while a mode change replaces the
/// qualification they are latched in.
pub(crate) struct ModeSwitchHold(Arc<AbortOwner>);
impl Drop for ModeSwitchHold {
    fn drop(&mut self) {
        ram(&self.0.claims).switching = false;
    }
}
pub(crate) fn hold_for_mode_switch(state: &AppState) -> Result<ModeSwitchHold, String> {
    let mut claims = ram(&state.gse.abort.claims);
    if claims.in_flight {
        return Err("an automatic abort is being recorded".into());
    }
    claims.switching = true;
    drop(claims);
    Ok(ModeSwitchHold(state.gse.abort.clone()))
}

struct InFlight {
    owner: Arc<AbortOwner>,
    operator_claim: Option<OperatorDispatchClaim>,
}
impl Drop for InFlight {
    fn drop(&mut self) {
        // Its dispatch lease ends first: nothing finished still holds one.
        drop(self.operator_claim.take());
        self.owner.release();
    }
}
/// An admitted automatic abort (or a Clear) and its dispatch. Dropping an
/// abort's claim hands its record to the recorder thread; it stays in flight
/// until the record is on disk or its save failed.
pub(crate) struct AutomaticAbortClaim {
    runtime: Arc<Owner>,
    in_flight: Option<InFlight>,
    durable: bool,
}
impl Drop for AutomaticAbortClaim {
    fn drop(&mut self) {
        let in_flight = self.in_flight.take();
        if !self.durable {
            return;
        }
        let runtime = self.runtime.clone();
        self.runtime.abort.record(move || {
            let _in_flight = in_flight;
            // Taking the lock puts the claim into the runtime, so this save
            // has it, and so has every later one. A save that began before
            // ends before this one: no older record overwrites it.
            let mut rt = runtime.lock().unwrap_or_else(PoisonError::into_inner);
            // Storage failure cannot suppress the critical abort. The RAM latch
            // and persist_error still block Fire; durability is not claimed on
            // I/O error.
            let saved = rt.operational.persist();
            drop(rt);
            if let Err(error) = saved {
                log::error!("Automatic abort could not be persisted; abort sent, Fire blocked: {error}");
            }
        });
    }
}

// Safety catalog (About safety): no-automatic-abort-after-launch. Keep backend/src/safety_catalog_rules.rs in step.
pub(super) fn stage_allows_automatic_abort(stage: FlightState) -> bool {
    matches!(
        stage,
        FlightState::Startup
            | FlightState::Idle
            | FlightState::PreFill
            | FlightState::FillTest
            | FlightState::NitrogenFill
            | FlightState::NitrousFill
            | FlightState::Armed
    )
}

pub(crate) enum Decision {
    Claimed(AutomaticAbortClaim),
    /// A disconnect abort while in Startup, where none is armed.
    Startup(FlightState),
    /// A Launch is being saved. Whether this abort may go depends on the
    /// outcome: decide again later.
    LaunchSaving,
    Unavailable,
}
#[cfg(test)]
impl Decision {
    fn claimed(self) -> Option<AutomaticAbortClaim> {
        match self {
            Self::Claimed(claim) => Some(claim),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) fn claim_automatic_abort(state: &AppState) -> Option<AutomaticAbortClaim> {
    claim_automatic_abort_for_generation(state, automatic_generation(state))
}

#[cfg(test)]
pub(crate) fn claim_automatic_abort_for_generation(
    state: &AppState,
    generation: u64,
) -> Option<AutomaticAbortClaim> {
    decide_automatic_abort(state, generation).claimed()
}

pub(crate) fn decide_automatic_abort(state: &AppState, generation: u64) -> Decision {
    claim_from_source(state, false, generation, None)
}

#[cfg(test)]
pub(crate) fn claim_liveness_abort(state: &AppState) -> Decision {
    claim_from_source(state, true, automatic_generation(state), None)
}

/// `token`: the station mode the disconnect was seen in.
pub(crate) fn decide_liveness_abort(state: &AppState, token: crate::runtime_mode::ModeToken) -> Decision {
    claim_from_source(state, true, automatic_generation(state), Some(token))
}

/// Never waits for a disk, or for a lock that is held across a disk write.
fn claim_from_source(
    state: &AppState,
    liveness: bool,
    generation: u64,
    token: Option<crate::runtime_mode::ModeToken>,
) -> Decision {
    let Some(operator_claim) = claim_abort_dispatch(state, generation) else {
        return Decision::Unavailable;
    };
    let owner = &state.gse.abort;
    loop {
        let mode = crate::runtime_mode::token();
        if token.is_some_and(|token| token != mode) {
            return Decision::Unavailable;
        }
        let hitl = mode.mode == crate::station_mode::Mode::Hitl;
        // Preserve the original general-claim short circuit: a held/latched
        // claim must not acquire the flight-state mutex unnecessarily.
        let launches = match owner.check(hitl) {
            Ok(launches) => launches,
            Err(Closed::Later) => return Decision::LaunchSaving,
            Err(Closed::Refused) => return Decision::Unavailable,
        };
        match launch_inhibited(state) {
            Some(false) => {}
            Some(true) => return Decision::Unavailable,
            // A Launch admitted meanwhile: the check above says so.
            None => continue,
        }
        let stage = state.local_flight_state_snapshot();
        // Source-specific eligibility is checked at the same serialized claim
        // decision as the latch. Disconnect aborts arm from the Idle
        // mission phase onward; only Startup is exempt.
        if liveness
            && crate::mission_phase::safety_phase(stage) == crate::mission_phase::MissionPhase::Startup
        {
            return Decision::Startup(stage);
        }
        if !stage_allows_automatic_abort(stage) {
            return Decision::Unavailable;
        }
        #[cfg(test)]
        between::here(between::Point::ClaimTaken);
        match owner.take(hitl, launches, mode) {
            Ok(true) => {
                return Decision::Claimed(AutomaticAbortClaim {
                    runtime: state.gse.clone(),
                    in_flight: Some(InFlight {
                        owner: owner.clone(),
                        operator_claim: Some(operator_claim),
                    }),
                    durable: !hitl,
                });
            }
            // A Launch or a mode change came and went since the check.
            Ok(false) => continue,
            Err(Closed::Later) => return Decision::LaunchSaving,
            Err(Closed::Refused) => return Decision::Unavailable,
        }
    }
}

/// A Launch is on record. None: a Launch is being saved, which holds this
/// record across its disk write (and nothing else does).
pub(super) fn launch_inhibited(state: &AppState) -> Option<bool> {
    loop {
        match state.launch_history.try_lock() {
            Ok(history) => return Some(history.automatic_actions_inhibited()),
            Err(TryLockError::Poisoned(poisoned)) => {
                return Some(poisoned.into_inner().automatic_actions_inhibited());
            }
            Err(TryLockError::WouldBlock) if state.gse.abort.launch_saving() => return None,
            Err(TryLockError::WouldBlock) => std::thread::sleep(std::time::Duration::from_micros(200)),
        }
    }
}

/// The boundary every automatic abort passes, and the automatic valve actions
/// before Launch. Waits for nothing. None: a Launch is being saved.
pub(super) fn abort_boundary(state: &AppState) -> Option<bool> {
    within_abort_boundary(state, || ()).map(|within| within.is_some())
}
/// `apply` (atomics only) runs inside the boundary or not at all: no Launch is
/// admitted between the check and it.
pub(super) fn within_abort_boundary<T>(state: &AppState, apply: impl FnOnce() -> T) -> Option<Option<T>> {
    let owner = &state.gse.abort;
    let hitl = crate::runtime_mode::is_hitl();
    loop {
        let before = {
            let claims = ram(&owner.claims);
            if claims.saving != 0 || claims.switching {
                return None;
            }
            claims.launches
        };
        let Some(inhibited) = launch_inhibited(state) else { return None };
        if inhibited
            || (!hitl && owner.fire_committed())
            || !stage_allows_automatic_abort(state.local_flight_state_snapshot())
        {
            return Some(None);
        }
        let claims = ram(&owner.claims);
        if claims.launches == before && claims.saving == 0 {
            return Some(Some(apply()));
        }
    }
}

/// Claimed, before any of its slower bookkeeping: latch, queue and transmit.
/// Only transmits: received packets are the router worker's, and their
/// handlers wait for station state.
pub(crate) fn send_claimed(
    state: &AppState,
    router: &Arc<sedsnet::router::Router>,
    claim: &AutomaticAbortClaim,
    reason: &str,
    cause: crate::abort_retirement::AbortCause,
) -> sedsnet::TelemetryResult<()> {
    debug_assert!(claim.in_flight.is_some());
    state.set_abort_indicator_latched(true);
    crate::telemetry_task::queue_automatic_abort_packet(router, reason, cause)?;
    if let Err(error) = crate::telemetry_task::dispatch_abort_tx(router) {
        log::error!("failed to flush automatic abort: {error:?}");
    }
    Ok(())
}

/// For a Clear: excludes automatic aborts and Fire until it is dropped. None
/// while an abort is being sent or recorded.
pub(super) fn reserve_abort_clear(state: &AppState) -> Option<AutomaticAbortClaim> {
    state.gse.abort.reserve().then(|| AutomaticAbortClaim {
        runtime: state.gse.clone(),
        // Existing explicit clear already owns its outer dispatch reservation.
        in_flight: Some(InFlight { owner: state.gse.abort.clone(), operator_claim: None }),
        durable: false,
    })
}

#[cfg(test)]
mod disk_stall_tests;

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    async fn fixture() -> (
        Arc<AppState>,
        Arc<sedsnet::router::Router>,
        Arc<AtomicUsize>,
        std::path::PathBuf,
    ) {
        fixture_with_state(crate::state::tests::test_app_state().await).await
    }

    /// The recorder thread may hold the lock for a moment; the thread that runs
    /// this callback must not.
    pub(crate) fn lock_free_for_callback(state: &AppState) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while state.gse.try_lock().is_err() {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    pub(in crate::gse) async fn fixture_with_state(
        state: Arc<AppState>,
    ) -> (
        Arc<AppState>,
        Arc<sedsnet::router::Router>,
        Arc<AtomicUsize>,
        std::path::PathBuf,
    ) {
        let weak = Arc::downgrade(&state);
        let seen = Arc::new(AtomicUsize::new(0));
        let count = seen.clone();
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([
                sedsnet::router::EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoint("ABORT"),
                    move |packet| {
                        assert!(
                            lock_free_for_callback(&weak.upgrade().unwrap()),
                            "router callback must not run under GSE lock"
                        );
                        if packet.data_type() == crate::telemetry_schema::data_type("ABORT") {
                            count.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(())
                    },
                ),
            ]),
        ));
        let path = armed_fixture_for_test(&state, &router);
        (state, router, seen, path)
    }
    // The direct safety producer is deliberately absent from test-fire builds.
    // Exercise shared claim/TX/Clear policy there; the separate actual command
    // worker regression covers the applicable automatic producer end to end.
    fn dispatch_abort_for_shared_policy_test(
        state: &Arc<AppState>,
        router: &Arc<sedsnet::router::Router>,
    ) -> bool {
        let dispatched = if !crate::runtime_mode::is_test_fire() {
            // The HITL suite skips every caller (HITL builds had no safety task).
            crate::safety_task::dispatch_safety_abort(state, router)
        } else {
            let Some(_claim) = claim_automatic_abort(state) else {
                return false;
            };
            crate::telemetry_task::queue_automatic_abort_packet(
                router,
                "Safety Task Abort Command Issued",
                crate::abort_retirement::AbortCause::Unknown("test".into()),
            )
            .unwrap();
            router.process_all_queues_with_timeout(3).unwrap();
            true
        };
        state.gse.abort.settle();
        dispatched
    }
    fn acknowledge_abort_owners(router: &sedsnet::router::Router) {
        for row in router.export_reliable_pending_stats() {
            if row.data_type != crate::telemetry_schema::data_type("ABORT") {
                continue;
            }
            assert_eq!(row.pending_destinations.len(), 1);
            // The prerequisite fixture advertises GB. Named ACKs follow the
            // same production identity validation as that discovered owner.
            let ack = sedsnet::packet::Packet::new(
                sedsnet::config::DataType::ReliableAck,
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "E2EACK:GB",
                1,
                Arc::<[u8]>::from(row.packet_id.to_le_bytes()),
            )
            .unwrap();
            router.rx_queue_from_side(ack, 0).unwrap();
        }
        router.process_all_queues_with_timeout(0).unwrap();
    }
    #[tokio::test]
    async fn liveness_claim_rechecks_startup_without_weakening_general_fault_claims() {
        crate::mode_suite!(Normal, Testing);
        let (state, _router, _seen, path) = fixture().await;
        state.set_preexisting_flight_state_for_test(FlightState::Startup);
        assert!(matches!(
            claim_liveness_abort(&state),
            Decision::Startup(FlightState::Startup)
        ));
        assert!(
            !state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .automatic_abort_claimed
        );
        // GENERIC_ERROR and automatic ADC failure use this unchanged general path.
        let claim = claim_automatic_abort(&state).expect("general fault remains eligible in Startup");
        assert!(
            state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .automatic_abort_claimed
        );
        drop(claim);
        state.gse.abort.settle();
        assert!(
            claim_automatic_abort(&state).is_none(),
            "durable claim remains latched"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn liveness_claim_is_armed_from_the_idle_phase() {
        crate::mode_suite!(Normal, Testing);
        let (state, _router, _seen, path) = fixture().await;
        state.set_local_flight_state(FlightState::Idle);
        assert!(matches!(
            claim_liveness_abort(&state),
            Decision::Claimed(_)
        ));
        assert!(
            state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .automatic_abort_claimed
        );
        let _ = std::fs::remove_file(path);
    }

    /// Every GS automatic-abort producer, enumerated: none may act from Launch
    /// admission onward (Launch, In flight and Landed mission phases).
    /// Operator manual Abort/Stop is a separate path and is not gated here.
    #[tokio::test]
    async fn no_automatic_abort_path_acts_after_launch() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let (state, router, seen, path) = fixture().await;
        let post_launch = [
            FlightState::Launch,
            FlightState::Ascent,
            FlightState::Coast,
            FlightState::Apogee,
            FlightState::Descent,
            FlightState::Reefing,
            FlightState::Landed,
            FlightState::Recovery,
        ];
        for stage in post_launch {
            state.set_preexisting_flight_state_for_test(stage);
            let phase = crate::mission_phase::safety_phase(stage);
            assert!(matches!(
                phase,
                crate::mission_phase::MissionPhase::Launch
                    | crate::mission_phase::MissionPhase::InFlight
                    | crate::mission_phase::MissionPhase::Landed
            ));
            assert!(!phase.automatic_aborts_allowed(), "{stage:?}");
            assert!(!phase.disconnect_aborts_armed(), "{stage:?}");
            let generation = automatic_generation(&state);
            // 1. Shared stage boundary used by every producer.
            assert!(!automatic_abort_allowed(&state), "{stage:?}");
            // 2. General claim: safety-task faults, ADC/tare failure, overpressure.
            assert!(claim_automatic_abort(&state).is_none(), "{stage:?}");
            assert!(claim_automatic_abort_for_generation(&state, generation).is_none());
            // 3. Disconnect (board-offline) claim.
            if !crate::runtime_mode::is_test_fire() {
                assert!(!matches!(
                    claim_liveness_abort(&state),
                    Decision::Claimed(_)
                ));
            }
            // 4. Queued automatic abort and automatic supply closes.
            assert!(!queue_automatic_abort_for_generation(&state, &router, generation, "test").unwrap());
            assert!(!queue_automatic_supply_close(
                &state,
                &router,
                crate::rocket_commands::ActuatorBoardCommands::NitrousClose as u8,
                generation
            )
            .unwrap());
            // 5. Final dispatch filter for automatic command intents.
            for command in [
                TelemetryCommand::AutomaticAbort { generation },
                TelemetryCommand::AutomaticNitrogenClose { generation },
                TelemetryCommand::AutomaticNitrousClose { generation },
            ] {
                assert!(crate::telemetry_task::command_at_dispatch(&state, command).is_none());
            }
            // 6. Observed automatic abort acceptance. This is where the GS sees
            // board-side automatic aborts: AB heartbeat/initial-heartbeat/ADC
            // and VB heartbeat/continuity/overpressure aborts arrive as ABORT
            // packets tagged SEDS_AUTO_ABORT_V1 (About safety: Board behaviour).
            // The boards gate them before Launch themselves; the GS must not
            // adopt one after Launch either.
            assert_eq!(accept_observed_abort(&state, true, generation), Some(false));
        }
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 0, "no ABORT left the station");
        assert!(!state.abort_indicator_latched());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn latched_general_claim_returns_without_waiting_for_flight_state_mutex() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let (state, _router, _seen, path) = fixture().await;
        drop(claim_automatic_abort(&state).unwrap());
        state.gse.abort.settle();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker_state = state.clone();
        let held_state = state.state.lock().unwrap();
        let worker = std::thread::spawn(move || {
            let _ = started_tx.send(());
            let unavailable = matches!(
                claim_from_source(&worker_state, false, automatic_generation(&worker_state), None),
                Decision::Unavailable
            );
            let _ = result_tx.send(unavailable);
        });
        // Generous bounds: a non-blocking claim answers at once, so these only
        // matter when the regression (blocking on the held mutex) is present,
        // and a loaded test host must not turn scheduling delay into failure.
        let started = started_rx.recv_timeout(std::time::Duration::from_secs(30));
        let outcome = if started.is_ok() {
            result_rx.recv_timeout(std::time::Duration::from_secs(30))
        } else {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        };
        // Always release the lock and join before asserting, including the
        // old-order negative path, so the regression cannot strand a worker.
        drop(held_state);
        let joined = worker.join();
        let _ = std::fs::remove_file(path);
        assert!(started.is_ok(), "claim worker started");
        assert!(joined.is_ok(), "claim worker completed after cleanup");
        assert!(
            matches!(outcome, Ok(true)),
            "latched claim returns Unavailable without reading state"
        );
    }

    #[tokio::test]
    async fn abort_claim_first_blocks_fire_and_clear_until_dispatch_finishes() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let (state, router, seen, path) = fixture().await;
        let ready = Arc::new(Barrier::new(2));
        let go = Arc::new(Barrier::new(2));
        let worker_state = state.clone();
        let worker_router = router.clone();
        let worker_ready = ready.clone();
        let worker_go = go.clone();
        let worker = std::thread::spawn(move || {
            let claim = claim_automatic_abort(&worker_state).unwrap();
            worker_ready.wait();
            worker_go.wait();
            crate::telemetry_task::queue_automatic_abort_packet(
                &worker_router,
                "claimed automatic abort",
                crate::abort_retirement::AbortCause::Unknown("test".into()),
            )
            .unwrap();
            worker_router.process_all_queues_with_timeout(0).unwrap();
            drop(claim);
        });
        ready.wait();
        assert!(
            commit_fire_intent(&state)
                .unwrap_err()
                .contains("Automatic abort")
        );
        assert!(
            clear_ground_abort(&state)
                .unwrap_err()
                .1
                .contains("in progress")
        );
        go.wait();
        worker.join().unwrap();
        state.gse.abort.settle();
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert!(
            operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        // Requalification cannot erase the separate persisted abort latch.
        {
            let mut rt = state.gse.lock().unwrap();
            rt.operational.saved.qualification.reset().unwrap();
            rt.operational.persist().unwrap();
        }
        assert!(commit_fire_intent(&state).is_err());
        acknowledge_abort_owners(&router);
        router
            .log_queue(crate::telemetry_schema::data_type("FLIGHT_BUZZER"), &[1u8])
            .unwrap();
        assert!(
            router.has_queued_work(),
            "unrelated telemetry need not globally drain"
        );
        clear_ground_abort(&state).unwrap();
        assert!(
            !operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
        let _ = std::fs::remove_file(path);
    }
    #[tokio::test]
    async fn fire_first_denies_automatic_claim_but_manual_abort_remains_available() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let (state, router, seen, path) = fixture().await;
        commit_fire_intent(&state).unwrap();
        assert!(claim_automatic_abort(&state).is_none());
        assert!(!dispatch_abort_for_shared_policy_test(&state, &router));
        assert!(state.is_command_allowed(&TelemetryCommand::Abort));
        crate::telemetry_task::queue_abort_packet(&router, "manual operator abort").unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_file(path);
    }
    #[tokio::test]
    async fn persistence_failure_still_dispatches_abort_and_keeps_fire_blocked() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let (state, router, seen, path) = fixture().await;
        state.gse.lock().unwrap().operational.path = Some(path.join("cannot-create-below-file"));
        assert!(
            dispatch_abort_for_shared_policy_test(&state, &router),
            "storage failure must not suppress critical abort"
        );
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        let rt = state.gse.lock().unwrap();
        assert!(rt.operational.saved.automatic_abort_claimed);
        assert!(rt.operational.persist_error.is_some());
        drop(rt);
        assert!(commit_fire_intent(&state).is_err());
        assert!(clear_ground_abort(&state).is_err());
        assert!(
            state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .automatic_abort_claimed
        );
        let _ = std::fs::remove_file(path);
    }
    #[tokio::test]
    async fn failed_final_clear_save_retains_claim_and_queue_failure_does_not_rearm() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let (state, _router, _seen, path) = fixture().await;
        let claim = claim_automatic_abort(&state).unwrap();
        // A genuine router queue-admission failure, not a mocked callback.
        let memory = sedsnet::config::RuntimeMemoryConfig {
            max_queue_budget: 64,
            starting_queue_size: 64,
            max_recent_rx_ids: 1,
            queue_grow_step: 1.0,
        };
        let bounded = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([])
                .with_memory_config(memory)
                .unwrap(),
        ));
        assert!(
            crate::telemetry_task::queue_automatic_abort_packet(
                &bounded,
                "automatic abort cannot fit in tiny queue",
                crate::abort_retirement::AbortCause::Unknown("test".into()),
            )
            .is_err()
        );
        drop(claim);
        state.gse.abort.settle();
        assert!(commit_fire_intent(&state).is_err());
        {
            let mut rt = state.gse.lock().unwrap();
            rt.operational.path = Some(path.join("invalid-final-save"));
            assert!(rt.operational.clear_automatic_abort_claim().is_err());
            assert!(rt.operational.saved.automatic_abort_claimed);
        }
        assert!(
            operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        let _ = std::fs::remove_file(path);
    }
    #[tokio::test]
    async fn persisted_claim_restores_abort_and_explicit_clear_recovers_after_restart() {
        crate::mode_suite!(Normal, TestFire, Testing);
        const CHILD: &str = "GS_ABORT_CLAIM_RESTART_TEST";
        const NAME: &str = "gse::automatic_abort::tests::persisted_claim_restores_abort_and_explicit_clear_recovers_after_restart";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let (state, _router, _seen, path) = fixture().await;
        let claim = claim_automatic_abort(&state).unwrap();
        drop(claim);
        state.gse.abort.settle();
        let restored = crate::state::tests::test_app_state().await;
        unsafe {
            std::env::set_var("GS_GSE_QUALIFICATION", &path);
        }
        initialize(&restored);
        unsafe {
            std::env::remove_var("GS_GSE_QUALIFICATION");
        }
        assert_eq!(restored.local_flight_state_snapshot(), FlightState::Aborted);
        assert!(
            restored
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .automatic_abort_claimed
        );
        assert!(commit_fire_intent(&restored).is_err());
        // A late FC Idle cannot bypass the latch; explicit recovery remains reachable.
        restored.set_local_flight_state(FlightState::Idle);
        assert!(commit_fire_intent(&restored).is_err());
        assert!(
            clear_ground_abort(&restored).is_err(),
            "missing router must not clear the persisted latch"
        );
        assert!(
            restored
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .automatic_abort_claimed
        );
        restored
            .topology_router
            .set(Arc::new(sedsnet::router::Router::new(
                sedsnet::router::RouterConfig::new([]),
            )))
            .unwrap();
        crate::telemetry_task::refresh_host_network_time(
            restored.topology_router.get().unwrap(),
            crate::telemetry_task::get_current_timestamp_ms(),
        );
        clear_ground_abort(&restored).unwrap();
        assert!(
            !operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        let _ = std::fs::remove_file(path);
    }
    #[tokio::test]
    async fn clear_waits_for_busy_sdk_retry_then_becomes_available() {
        crate::mode_suite!(Normal, TestFire, Testing);
        use std::sync::atomic::{AtomicBool, AtomicU64};
        let state = crate::state::tests::test_app_state().await;
        let now = Arc::new(AtomicU64::new(0));
        let clock = now.clone();
        let router = Arc::new(sedsnet::router::Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]),
            Box::new(move || clock.load(Ordering::SeqCst)),
        ));
        let path = armed_fixture_for_test(&state, &router);
        router.remove_side(0).unwrap(); // Remove prerequisite-only advertised peer.
        let busy = Arc::new(AtomicBool::new(true));
        let blocked = busy.clone();
        router.add_side_packed("no_advertised_peer", move |wire| {
            if sedsnet::wire_format::peek_frame_info(wire)
                .is_ok_and(|f| f.envelope.ty == crate::telemetry_schema::data_type("ABORT"))
                && blocked.load(Ordering::SeqCst)
            {
                return Err(sedsnet::TelemetryError::Io("side tx busy"));
            }
            Ok(())
        });
        assert!(dispatch_abort_for_shared_policy_test(&state, &router));
        assert!(
            clear_ground_abort(&state)
                .unwrap_err()
                .1
                .contains("delivery is still pending")
        );
        assert!(commit_fire_intent(&state).is_err());
        assert!(
            operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        busy.store(false, Ordering::SeqCst);
        now.store(1000, Ordering::SeqCst);
        router.process_all_queues_with_timeout(0).unwrap();
        assert!(!router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("ABORT")));
        clear_ground_abort(&state).unwrap();
        assert!(
            !operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn clear_retires_an_automatic_abort_the_radio_cannot_deliver() {
        crate::mode_suite!(Normal, TestFire, Testing);
        use std::sync::atomic::AtomicU64;
        let state = crate::state::tests::test_app_state().await;
        let clock = Arc::new(AtomicU64::new(0));
        let model = clock.clone();
        let router = Arc::new(sedsnet::router::Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]),
            Box::new(move || model.load(Ordering::SeqCst)),
        ));
        let path = armed_fixture_for_test(&state, &router);
        router.remove_side(0).unwrap();
        // The AV bay sits behind a saturated radio that refuses every ABORT.
        let radio = router.add_side_packed("saturated_radio", |wire| {
            if sedsnet::wire_format::peek_frame_info(wire)
                .is_ok_and(|f| f.envelope.ty == crate::telemetry_schema::data_type("ABORT"))
            {
                return Err(sedsnet::TelemetryError::Io("side tx busy"));
            }
            Ok(())
        });
        let announce = sedsnet::discovery::build_discovery_announce(
            "FC",
            123,
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
        )
        .unwrap();
        router.rx_packed_from_side(&sedsnet::wire_format::pack_packet(&announce), radio).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        assert!(dispatch_abort_for_shared_policy_test(&state, &router));
        let abort = crate::telemetry_schema::data_type("ABORT");
        let id = router.export_reliable_pending_stats().into_iter()
            .find(|row| row.data_type == abort)
            .expect("owed to the AV bay").packet_id;
        assert!(operator_reset_block_reason(&state).is_some_and(|r| r.contains("ABORT")));
        // The normal delivery window still holds Clear (unchanged rule).
        assert!(clear_ground_abort(&state).unwrap_err().1.contains("delivery is still pending"));
        crate::gse::age_delivery_tracking_for_test(crate::sim_freshness::wall(undeliverable_after()) * 2);
        clear_ground_abort(&state).unwrap();
        assert_eq!(
            crate::abort_retirement::retired_reason(&router, id),
            Some(crate::abort_retirement::RetireReason::OperatorCleared)
        );
        assert!(router.has_pending_reliable_tx_type(abort), "the SDK still holds it");
        assert!(
            !operator_reset_block_reason(&state).is_some_and(|r| r.contains("ABORT")),
            "a retired abort no longer holds Reset"
        );
        let notes = state.notifications.lock().unwrap().iter().map(|n| n.message.clone()).collect::<Vec<_>>();
        assert!(notes.iter().any(|n| n.contains("Stale automatic Abort retired") && n.contains("Flight Computer")), "{notes:?}");
        assert!(!notes.iter().any(|n| n == UNDELIVERED_ABORT_AFTER_CLEAR), "{notes:?}");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn clear_waits_for_no_peer_abort_already_handed_to_software_transport() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let state = crate::state::tests::test_app_state().await;
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]),
        ));
        let path = armed_fixture_for_test(&state, &router);
        router.remove_side(0).unwrap(); // This test deliberately has no ACK owner.
        let activity = state.transport_tx_activity.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<(u8, Vec<u8>)>();
        router.add_side_packed("asynchronous_transport", move |wire| {
            if sedsnet::wire_format::peek_frame_info(wire)
                .is_ok_and(|f| f.envelope.ty == crate::telemetry_schema::data_type("ABORT"))
            {
                activity.try_enqueue(|| {
                    sender
                        .send((255, wire.to_vec()))
                        .map_err(|_| sedsnet::TelemetryError::Io("software transport closed"))
                })?;
            }
            Ok(())
        });
        assert!(dispatch_abort_for_shared_policy_test(&state, &router));
        assert!(
            !router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("ABORT")),
            "no advertised peer: SDK has handed off this publication"
        );
        assert!(state.has_pending_transport_tx());
        let (_, wire) = receiver
            .try_recv()
            .expect("actual Comms channel owns the accepted abort");
        let packet = sedsnet::wire_format::unpack_packet(&wire).unwrap();
        assert_eq!(
            packet.data_type(),
            crate::telemetry_schema::data_type("ABORT")
        );
        assert_eq!(
            packet.payload(),
            b"SEDS_AUTO_ABORT_V1:Safety Task Abort Command Issued"
        );
        // Popping the software channel does not release ownership before the
        // worker completes its write; active-write retries remain pending.
        assert!(state.has_pending_transport_tx());
        assert!(
            clear_ground_abort(&state)
                .unwrap_err()
                .1
                .contains("delivery is still pending")
        );
        assert!(
            operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        assert!(commit_fire_intent(&state).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn old_saved_record_defaults_to_no_automatic_abort_claim() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let mut old = serde_json::to_value(operational::Saved::default()).unwrap();
        old.as_object_mut()
            .unwrap()
            .remove("automatic_abort_claimed");
        let restored: operational::Saved = serde_json::from_value(old).unwrap();
        assert!(!restored.automatic_abort_claimed);
    }
}

#[cfg(test)]
mod launch_inhibition_tests {
    use super::*;

    #[tokio::test]
    async fn launch_inhibits_automatic_abort_despite_delayed_state_and_clock_reset() {
        let state = crate::state::tests::test_app_state().await;
        crate::gse::qualified_launch_proofs_for_test(&state);
        state.set_local_flight_state(FlightState::Armed);
        super::super::inhibit_automatic_actions_for_launch(&state).unwrap();
        for stage in [
            FlightState::Armed,
            FlightState::Idle,
            FlightState::Startup,
            FlightState::Launch,
        ] {
            // A still-active fill with missing pressure would otherwise fault and
            // automatically close/open valves on the next real controller tick.
            state.gse.lock().unwrap().engine.status.phase = Phase::Filling;
            state.set_preexisting_flight_state_for_test(stage);
            state.reset_launch_clock();
            assert!(!super::super::automatic_abort_allowed(&state));
            assert!(claim_automatic_abort(&state).is_none());
            for command in [
                TelemetryCommand::AutomaticNitrogenClose {
                    generation: crate::gse::automatic_generation(&state),
                },
                TelemetryCommand::AutomaticNitrousClose {
                    generation: crate::gse::automatic_generation(&state),
                },
            ] {
                assert!(crate::telemetry_task::command_at_dispatch(&state, command).is_none());
            }
            assert!(
                crate::telemetry_task::command_at_dispatch(&state, TelemetryCommand::Abort)
                    .is_some()
            );
            assert!(
                super::super::inhibit_automatic_actions_for_launch(&state).is_err(),
                "T-clock reset cannot admit launch twice"
            );
            super::super::tick(&state, false);
            super::super::pause_fill_for_recoverable_fault(&state, "late board error");
            super::super::start_fill_after_automatic_zero(
                &state,
                crate::gse::automatic_generation(&state),
            );
            super::super::report_safety(&state);
            assert_eq!(
                state.gse.lock().unwrap().engine.status.phase,
                Phase::Filling,
                "post-launch automatic tick/cancel must not change valve sequencing"
            );
        }
    }

    #[tokio::test]
    async fn launch_cannot_overtake_an_admitted_automatic_abort() {
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Armed);
        let claim = claim_automatic_abort(&state).expect("prelaunch automatic abort admission");
        assert!(super::super::inhibit_automatic_actions_for_launch(&state).is_err());
        drop(claim);
    }
}
