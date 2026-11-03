//! Optional application manual-Stop UTC. Uncertainty always retains legacy authority.
//! The existing SDK clock call may wait on SDK internals; it runs only after
//! retained intent ownership and outside every intent/cache/history/GSE mutex.
use crate::{
    launch_history::ManualUtcKey,
    manual_abort_intent::{Claim, WirePolicy},
    state::AppState,
};
use sedsnet::router::Router;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, broadcast};

#[derive(Clone, Copy)]
struct Eligible {
    revision: u64,
    key: ManualUtcKey,
    nonces: [u64; 3],
    router: usize,
    published: Instant,
}
#[derive(Default)]
pub(crate) struct Runtime {
    revision: AtomicU64,
    revoked: AtomicBool,
    reserving: AtomicBool,
    cache: Mutex<Option<Eligible>>,
    pub(crate) wake: Notify,
}
impl Runtime {
    pub(crate) fn reservation_pending(&self) -> bool {
        self.reserving.load(Ordering::Acquire)
    }
    pub(crate) fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// Sticky through failed/cancelled PREPARE; a late writer cannot undo it.
    pub(crate) fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        let _ = self
            .revision
            .try_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_add(1));
    }
    pub(crate) fn after_completed(&self) {
        // Called only by the actual coordinator AFTER durable publication ack.
        if self
            .revision
            .try_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_add(1))
            .is_err()
        {
            self.revoked.store(true, Ordering::Release);
            return;
        }
        self.revoked.store(false, Ordering::Release);
        self.wake.notify_one();
    }
}
fn key(state: &AppState) -> Option<ManualUtcKey> {
    if crate::rrls::is_rrls() || state.operator_admission_barrier.load(Ordering::Acquire) != 0 {
        return None;
    }
    let key = state.launch_history.try_lock().ok()?.manual_utc_key()?;
    (key.generation == state.automatic_generation.load(Ordering::Acquire)).then_some(key)
}
fn eligible(state: &AppState, router: &Router) -> Option<Eligible> {
    let runtime = &state.board_recovery.manual_utc;
    if runtime.is_revoked() || !crate::board_recovery::coordinator::manual_utc_idle(state) {
        return None;
    }
    let cached = (*runtime.cache.try_lock().ok()?)?;
    if cached.revision != runtime.revision.load(Ordering::Acquire)
        || cached.router != router as *const Router as usize
        || cached.published.elapsed() > Duration::from_secs(2)
        || key(state) != Some(cached.key)
        || crate::board_recovery::manual_utc_proof(state, cached.key) != Some(cached.nonces)
    {
        return None;
    }
    Some(cached)
}
/// One epoch's first attempted publication selects actual UTC once. All refused
/// retries reuse the same wire. Renewed epochs cannot inherit an old blocked sample.
pub(crate) fn publish_claim(
    state: &AppState,
    router: &Router,
    claim: &mut Claim,
) -> sedsnet::TelemetryResult<bool> {
    let wire = if let Some(wire) = claim.wire() {
        wire
    } else {
        let proposed = if let Some(before) = eligible(state, router) {
            // No new application lock is held over this existing SDK API.
            let sample = crate::system_clock::required_control_network_utc_ms(router).ok();
            let after = eligible(state, router);
            match (sample, after) {
                (Some(original), Some(after))
                    if before.revision == after.revision
                        && before.key == after.key
                        && original > after.key.boundary
                        && original <= after.key.ceiling
                        && !matches!(original, 0 | u64::MAX) =>
                {
                    WirePolicy::KnownUtc(original)
                }
                _ => WirePolicy::Untimed,
            }
        } else {
            WirePolicy::Untimed
        };
        let Some(wire) = claim.freeze_wire(proposed) else {
            return Ok(false);
        };
        wire
    };
    crate::telemetry_task::queue_manual_abort_wire(router, wire)?;
    Ok(true)
}
struct Reservation(Arc<AppState>);
impl Drop for Reservation {
    fn drop(&mut self) {
        self.0
            .board_recovery
            .manual_utc
            .reserving
            .store(false, Ordering::Release);
    }
}
pub(crate) async fn refresh(state: &Arc<AppState>, router: &Arc<Router>) {
    let runtime = &state.board_recovery.manual_utc;
    if runtime.is_revoked() || !crate::board_recovery::coordinator::manual_utc_idle(state) {
        return;
    }
    let revision = runtime.revision.load(Ordering::Acquire);
    let Some(original_key) = key(state) else {
        return;
    };
    let Some(nonces) = crate::board_recovery::manual_utc_proof(state, original_key) else {
        return;
    };
    if !crate::board_recovery::manual_utc_endpoints(state, router) {
        return;
    }
    if runtime
        .reserving
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let reservation = Reservation(state.clone());
    let generation = state.automatic_generation.load(Ordering::Acquire);
    let Some(claim) = crate::gse::claim_operator_dispatch(state, generation) else {
        return;
    };
    // A ceiling is an upper bound, NEVER an occurrence/packet timestamp.
    let Some(ceiling) = crate::system_clock::required_control_network_utc_ms(router)
        .ok()
        .and_then(|actual| actual.checked_add(2000))
        .filter(|v| *v != u64::MAX)
    else {
        return;
    };
    if runtime.is_revoked() || runtime.revision.load(Ordering::Acquire) != revision {
        return;
    }
    let receipt = {
        let mut history = state.launch_history.lock().unwrap();
        match history.reserve_manual_utc_ceiling(original_key, ceiling) {
            Ok(receipt) => receipt,
            Err(_) => return,
        }
    };
    let expected = receipt.key();
    // Owned async service retains the operator claim throughout real wait/readback.
    if !matches!(
        tokio::task::spawn_blocking(move || {
            let _reservation = reservation;
            let _claim = claim;
            receipt.wait_and_verify()
        })
        .await,
        Ok(Ok(()))
    ) {
        return;
    }
    if runtime.is_revoked()
        || runtime.revision.load(Ordering::Acquire) != revision
        || !crate::board_recovery::coordinator::manual_utc_idle(state)
        || key(state) != Some(expected)
        || crate::board_recovery::manual_utc_proof(state, expected) != Some(nonces)
    {
        return;
    }
    *runtime.cache.lock().unwrap() = Some(Eligible {
        revision,
        key: expected,
        nonces,
        router: Arc::as_ptr(router) as usize,
        published: Instant::now(),
    });
}
/// Existing dedicated QUERY completion only wakes this single bounded service;
/// ingress never writes disk or starts unbounded detached reservation tasks.
pub(crate) async fn service(
    state: Arc<AppState>,
    router: Arc<Router>,
    mut shutdown: broadcast::Receiver<()>,
) {
    loop {
        tokio::select! {biased;_ = shutdown.recv()=>return,_ = state.board_recovery.manual_utc.wake.notified()=>{}}
        refresh(&state, &router).await;
    }
}
/// Revoke before making a challenge. A cancelled waiter never resumes renewal.
/// Waiting/refusal here does not delay or suppress independent manual Stop.
pub(crate) async fn before_prepare(state: &Arc<AppState>) -> Result<(), &'static str> {
    let runtime = &state.board_recovery.manual_utc;
    runtime.revoke();
    let start = Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(5) {
            return Err(
                "Manual UTC ceiling/prefix pending or clock rolled back; wait and prepare a new intent",
            );
        }
        if !runtime.reservation_pending() {
            let settled = state
                .launch_history
                .lock()
                .unwrap()
                .manual_writer_settled()
                .map_err(|_| "Manual UTC writer ownership unknown")?;
            if !settled {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            let bound = retained_bound(state)?;
            let router = state.topology_router.get().ok_or("Router unavailable")?;
            let actual = crate::system_clock::required_control_network_utc_ms(router)
                .map_err(|_| "Network UTC unavailable")?;
            if actual > bound {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
pub(crate) fn retained_bound(state: &AppState) -> Result<u64, &'static str> {
    let ceiling = state
        .launch_history
        .try_lock()
        .map_err(|_| "Manual UTC journal busy")?
        .retained_manual_utc_ceiling()
        .map_err(|_| "Manual UTC journal unavailable")?;
    let attempted = state
        .manual_abort_intent
        .try_known_highwater()
        .ok_or("Manual Stop owner busy")?;
    Ok(ceiling.max(attempted))
}
