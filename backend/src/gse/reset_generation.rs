//! Verified coordinator owner/epoch fences. Admission, actual producer and
//! journal ownership must quiesce before transitions. These helpers never
//! delete calibration/history or reset the launch clock/session.
use super::*;

#[cfg(test)]
pub(crate) fn invalidate_capture_epoch(
    state: &AppState,
    expected_generation: u64,
) -> Result<u64, &'static str> {
    // Non-reentrant: callers must not already hold DISPATCH_GATE.
    let _dispatch = DISPATCH_GATE.lock().unwrap();
    invalidate_capture_epoch_under_dispatch(state, expected_generation)
}

fn invalidate_capture_epoch_under_dispatch(
    state: &AppState,
    expected_generation: u64,
) -> Result<u64, &'static str> {
    if state
        .operator_dispatch_inflight
        .load(std::sync::atomic::Ordering::Acquire)
        != 0
    {
        return Err("Operator dispatch is still inflight");
    }
    // A slow tare file write must not make reset hold the shared dispatch
    // gate while waiting. The supervisor keeps admission closed and retries.
    let mut zero = state
        .auto_zero
        .try_lock()
        .map_err(|_| "Tare capture/commit is busy")?;
    let current = automatic_generation(state);
    if current != expected_generation || current == 0 {
        return Err("Reset epoch changed or is unavailable");
    }
    let next = current
        .checked_add(1)
        .filter(|next| *next != u64::MAX)
        .ok_or("Reset epoch exhausted")?;
    // The same zero mutex excludes file save/apply. Never take DISPATCH/GSE
    // underneath zeroRuntime in the commit path; lateStartFill takes it later.
    zero.invalidate_captures_for_verified_reset();
    state
        .automatic_generation
        .compare_exchange(
            current,
            next,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .map_err(|_| "Reset epoch changed during invalidation")?;
    Ok(next)
}

/// This stage closes host admission only. Board proofs and the full recovery
/// coordinator remain prerequisites for exposing any recovery endpoint.
pub(crate) fn close_operator_admission(
    state: &AppState,
    expected_generation: u64,
    owner: u64,
) -> Result<(), &'static str> {
    let _dispatch = DISPATCH_GATE.lock().unwrap();
    if matches!(owner, 0 | u64::MAX) || automatic_generation(state) != expected_generation {
        return Err("Recovery admission ticket is unavailable or stale");
    }
    match state.operator_admission_barrier.compare_exchange(
        0,
        owner,
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Acquire,
    ) {
        Ok(_) => Ok(()),
        Err(current) if current == owner => Ok(()),
        Err(_) => Err("Recovery admission belongs to another owner"),
    }
}

#[cfg(test)]
pub(crate) fn advance_with_clock_fence(
    state: &AppState,
    expected_generation: u64,
    owner: u64,
    fence: &crate::launch_history::RecoveryFence,
) -> Result<u64, &'static str> {
    let _dispatch = DISPATCH_GATE.lock().unwrap();
    if owner == 0
        || state
            .operator_admission_barrier
            .load(std::sync::atomic::Ordering::Acquire)
            != owner
    {
        return Err("Recovery admission owner changed");
    }
    let history = state
        .launch_history
        .try_lock()
        .map_err(|_| "Clock journal is busy")?;
    if !history
        .recovery_publication_ready(fence, owner)
        .map_err(|_| "Clock journal fence changed")?
    {
        return Err("Clock journal prefix is still pending");
    }
    let next = invalidate_capture_epoch_under_dispatch(state, expected_generation)?;
    state.last_command_ms.lock().unwrap().clear();
    Ok(next)
}

/// Staged supervisor handoff. The complete coordinator must first join its
/// old board workers and revoke the old proxy lease. This helper transfers
/// only host admission/journal ownership; it grants no board permission.
pub(crate) fn transfer_closed_recovery_owner(
    state: &AppState,
    expected_generation: u64,
    old_owner: u64,
    new_owner: u64,
    fence: &crate::launch_history::RecoveryFence,
) -> Result<crate::launch_history::RecoveryFence, &'static str> {
    let _dispatch = DISPATCH_GATE.lock().unwrap();
    if matches!(old_owner, 0 | u64::MAX)
        || matches!(new_owner, 0 | u64::MAX)
        || new_owner == old_owner
        || automatic_generation(state) != expected_generation
        || state
            .operator_admission_barrier
            .load(std::sync::atomic::Ordering::Acquire)
            != old_owner
        || state
            .operator_dispatch_inflight
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    {
        return Err("Recovery handoff is stale or still inflight");
    }
    let mut history = state
        .launch_history
        .try_lock()
        .map_err(|_| "Clock journal is busy")?;
    // The journal compares the fence owner too: a mismatched journal ticket
    // must not transfer the barrier independently of its durable prefix.
    let transferred = history
        .transfer_recovery_owner(fence, old_owner, new_owner)
        .map_err(|_| "Clock journal handoff changed")?;
    state
        .operator_admission_barrier
        .store(new_owner, std::sync::atomic::Ordering::Release);
    Ok(transferred)
}

#[cfg(test)]
pub(crate) fn reopen_operator_admission_and_journal(
    state: &AppState,
    old_generation: u64,
    owner: u64,
    fence: &crate::launch_history::RecoveryFence,
) -> Result<(), &'static str> {
    let _dispatch = DISPATCH_GATE.lock().unwrap();
    if owner == 0
        || state
            .operator_admission_barrier
            .load(std::sync::atomic::Ordering::Acquire)
            != owner
        || old_generation.checked_add(1) != Some(automatic_generation(state))
        || state
            .operator_dispatch_inflight
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    {
        return Err("Recovery publication is stale or still inflight");
    }
    let mut history = state
        .launch_history
        .try_lock()
        .map_err(|_| "Clock journal is busy")?;
    if !history
        .recovery_publication_ready(fence, owner)
        .map_err(|_| "Clock journal fence changed")?
    {
        return Err("Clock journal prefix is still pending");
    }
    history
        .reopen_after_recovery(fence)
        .map_err(|_| "Clock journal cannot reopen")?;
    state
        .operator_admission_barrier
        .store(0, std::sync::atomic::Ordering::Release);
    Ok(())
}

/// Every board send and final publication retains the ORIGINAL finite Stop
/// epochs. Never capture a replacement Stop baseline after closure.
pub(crate) fn advance_verified_recovery(
    state: &AppState,
    token: crate::operator_queue::Token,
    fence: &crate::launch_history::RecoveryFence,
) -> Result<u64, &'static str> {
    let _dispatch = DISPATCH_GATE.lock().unwrap();
    if state
        .operator_admission_barrier
        .load(std::sync::atomic::Ordering::Acquire)
        != token.owner
        || automatic_generation(state) != token.generation
        || state.operator_queue.relief_epoch() != token.relief_epoch
        || state
            .operator_dispatch_inflight
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    {
        return Err("Recovery owner/generation is stale or inflight");
    }
    state
        .manual_abort_intent
        .admit_if_unchanged(token.manual_epoch, || {
            state
                .deferred_abort
                .admit_if_no_manual_since(token.observed_manual_epoch, || {
                    let history = state
                        .launch_history
                        .try_lock()
                        .map_err(|_| "Clock journal busy")?;
                    if !history
                        .recovery_publication_ready(fence, token.owner)
                        .map_err(|_| "Clock fence changed")?
                    {
                        return Err("Clock prefix pending");
                    }
                    let generation =
                        invalidate_capture_epoch_under_dispatch(state, token.generation)?;
                    state.last_command_ms.lock().unwrap().clear();
                    Ok(generation)
                })
        })
        .flatten()
        .ok_or("Manual Stop superseded recovery")?
}

/// Clock contention is retryable only before any finalization mutation. All
/// original owner/Stop/proof guards must be rerun by the same operation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FinalizeError {
    ClockBusy,
    Refused(&'static str),
}
impl From<&'static str> for FinalizeError {
    fn from(message: &'static str) -> Self {
        Self::Refused(message)
    }
}
impl From<crate::final_proof_clock::Error> for FinalizeError {
    fn from(error: crate::final_proof_clock::Error) -> Self {
        match error {
            crate::final_proof_clock::Error::Busy => Self::ClockBusy,
            other => Self::Refused(other.message()),
        }
    }
}

/// Explicit global publication intent is distinct from its verified MCU set.
#[derive(Clone, Copy)]
pub(crate) enum StartupPublication {
    TargetedRecovery,
    LegacyAllSeven,
    ModeAware(crate::board_recovery::scope::Selection),
}
impl StartupPublication {
    fn global(self) -> bool {
        !matches!(self, Self::TargetedRecovery)
    }
    fn selection(self) -> Option<crate::board_recovery::scope::Selection> {
        match self {
            Self::ModeAware(selection) => Some(selection),
            _ => None,
        }
    }
}

pub(crate) fn finalize_verified_recovery(
    state: &AppState,
    token: crate::operator_queue::Token,
    fence: &crate::launch_history::RecoveryFence,
    proxy: &mut Option<crate::command_proxy::recovery::Barrier>,
    proofs: &crate::board_recovery::coordinator::FinalProofs,
    publication: StartupPublication,
    clock: &crate::final_proof_clock::Admission,
) -> Result<(), FinalizeError> {
    let reset_all = publication.global();
    let selection = publication.selection();
    let reporting_owner = super::progress::verified_reset_marker(state);
    let reporting_ms = crate::sequence_progress_service::now_ms();
    // Randomness may initialize an entropy provider; prepare outside every
    // final owner. The current proxy boot/active/collision checks remain below.
    let proxy_nonce = proxy
        .as_ref()
        .map(|_| crate::command_proxy::recovery::Barrier::prepare_reopen_nonce());
    let _dispatch = DISPATCH_GATE.lock().unwrap();
    if state
        .operator_admission_barrier
        .load(std::sync::atomic::Ordering::Acquire)
        != token.owner
        || automatic_generation(state) != token.generation
        || state.operator_queue.relief_epoch() != token.relief_epoch
        || state
            .operator_dispatch_inflight
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    {
        return Err("Recovery final owner/generation is stale or inflight".into());
    }
    // Existing order begins at DGate. All additional owners use try_lock and
    // precede both Stop and board-register fences. Never wait for GSE/state
    // while holding either of those owners. Non-reset recovery needs neither.
    let mut runtime = if reset_all {
        Some(state.gse.try_lock().map_err(|_| "GSE owner busy")?)
    } else {
        None
    };
    let mut host_state = if reset_all {
        Some(
            state
                .state
                .try_lock()
                .map_err(|_| "Host state owner busy")?,
        )
    } else {
        None
    };
    let mut history = state
        .launch_history
        .try_lock()
        .map_err(|_| "Clock journal busy")?;
    let mut proxy_owner = proxy
        .as_ref()
        .map(|p| p.try_prepare_reopen(proxy_nonce.expect("owned proxy nonce prepared")))
        .transpose()
        .map_err(|_| "Proxy owner busy or changed")?;
    if !history
        .recovery_publication_ready_nowait(fence, token.owner)
        .map_err(|_| "Clock prefix busy or changed")?
    {
        return Err("Clock prefix pending".into());
    }
    let result = state
        .manual_abort_intent
        .admit_if_unchanged(token.manual_epoch, || {
            state.deferred_abort.admit_if_no_manual_since(
                token.observed_manual_epoch,
                || -> Result<(), FinalizeError> {
                    let now = clock.current().map_err(FinalizeError::from)?;
                    if !({
                        let pending = history
                            .recovery_pending_ref()
                            .ok_or("Durable marker missing")?;
                        pending.owner == token.owner
                            && pending.verified
                            && proofs.valid_at(pending, token.generation, now)
                            && (!reset_all || pending.verified_startup_policy())
                            && selection.is_none_or(|s| {
                                s.valid_targets(pending)
                                    && pending.reset_policy.as_ref() == Some(&s.policy())
                            })
                    }) {
                        return Err("Durable verified marker unavailable".into());
                    }
                    let mut publish = || -> Result<(), &'static str> {
                        // RAM-only and no further owner/SDK/clock/file acquisition.
                        // The history readiness check above makes reopen deterministic.
                        history
                            .reopen_after_recovery_ram(fence)
                            .map_err(|_| "Journal cannot reopen")?;
                        if let Some(owner) = &mut proxy_owner {
                            owner.commit();
                        }
                        if reset_all {
                            super::progress::capture_verified_reset_under_gate(
                                runtime.as_mut().unwrap(),
                                reporting_owner,
                                reporting_ms,
                            );
                            state.publish_verified_startup_under_gate(host_state.as_mut().unwrap());
                        }
                        state
                            .operator_admission_barrier
                            .store(0, std::sync::atomic::Ordering::Release);
                        Ok(())
                    };
                    if let Some(selection) = selection {
                        selection.with_current(state, publish)?;
                    } else {
                        publish()?;
                    }
                    Ok(())
                },
            )
        })
        .flatten()
        .ok_or(FinalizeError::Refused("Manual Stop superseded recovery"))?;
    drop(proxy_owner);
    if result.is_ok() {
        *proxy = None;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_reset_ticket_cannot_cancel_new_capture_or_advance_epoch_again() {
        let state = crate::state::tests::test_app_state().await;
        let old = automatic_generation(&state);
        let next = invalidate_capture_epoch(&state, old).unwrap();
        *state.auto_zero.lock().unwrap() = crate::auto_zero::Runtime::testing(true);
        crate::auto_zero::launch(&state);
        state.auto_zero.lock().unwrap().observe("KG1000", 0.25);
        assert!(state.auto_zero.lock().unwrap().pending());
        let counts = state
            .auto_zero
            .lock()
            .unwrap()
            .captured_source_counts_for_test();
        assert!(invalidate_capture_epoch(&state, old).is_err());
        assert_eq!(automatic_generation(&state), next);
        assert!(state.auto_zero.lock().unwrap().pending());
        assert_eq!(
            state
                .auto_zero
                .lock()
                .unwrap()
                .captured_source_counts_for_test(),
            counts
        );
        state.auto_zero.lock().unwrap().cancel();
    }

    #[tokio::test]
    async fn actual_inflight_operator_claim_blocks_epoch_advance_until_released() {
        let state = crate::state::tests::test_app_state().await;
        let old = automatic_generation(&state);
        let claim = claim_operator_dispatch(&state, old).unwrap();
        assert!(invalidate_capture_epoch(&state, old).is_err());
        assert_eq!(automatic_generation(&state), old);
        drop(claim);
        assert_eq!(invalidate_capture_epoch(&state, old).unwrap(), old + 1);
    }

    #[tokio::test]
    async fn busy_tare_commit_refuses_without_holding_dispatch_wait_or_advancing_epoch() {
        let state = crate::state::tests::test_app_state().await;
        let old = automatic_generation(&state);
        let committing = state.auto_zero.lock().unwrap();
        assert!(invalidate_capture_epoch(&state, old).is_err());
        assert_eq!(automatic_generation(&state), old);
        drop(committing);
        assert_eq!(invalidate_capture_epoch(&state, old).unwrap(), old + 1);
    }

    #[tokio::test]
    async fn exhausted_epoch_refuses_without_wrapping_or_mutating_calibration() {
        let state = crate::state::tests::test_app_state().await;
        let before = serde_json::to_value(&*state.loadcell_calibration.lock().unwrap()).unwrap();
        for exhausted in [u64::MAX - 1, u64::MAX] {
            state
                .automatic_generation
                .store(exhausted, std::sync::atomic::Ordering::Release);
            assert!(invalidate_capture_epoch(&state, exhausted).is_err());
            assert_eq!(automatic_generation(&state), exhausted);
        }
        assert_eq!(
            serde_json::to_value(&*state.loadcell_calibration.lock().unwrap()).unwrap(),
            before
        );
    }
}
