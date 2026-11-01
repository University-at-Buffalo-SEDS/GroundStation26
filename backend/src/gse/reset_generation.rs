//! Staged reset-epoch/tare barrier, not a Reset HTTP endpoint or authorization.
//!
//! Kept test-gated until the verified-reset API is implemented. That consumer
//! must ungate this SAME helper only after admission is closed, all inflight
//! work is quiescent and fresh board proofs are checked. No journal/config or
//! calibration deletion occurs here.
use super::*;

pub(crate) fn invalidate_capture_epoch(
    state: &AppState,
    expected_generation: u64,
) -> Result<u64, &'static str> {
    // Non-reentrant: callers must not already hold DISPATCH_GATE.
    let _dispatch = DISPATCH_GATE.lock().unwrap();
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
