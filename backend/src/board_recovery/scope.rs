//! Immutable participation for the explicit mode-scoped Startup reset only.
//! Prospective/Unknown history is deliberately NOT admitted recovery ownership.
use super::*;

#[derive(Clone, Copy)]
pub(crate) struct Selection {
    context: crate::board_abort::ResetContext,
    required: u8,
    targets: u8,
}
/// No board is required: a reset covers whichever are available, so an absent
/// board can't block it. Boards it misses keep Launch locked if they come back.
pub(crate) const fn base_mask(_mode: crate::station_mode::Mode) -> u8 {
    0
}
/// Boards this mode resets and tracks. Test-fire has no AV bay requirement.
pub(crate) const fn mode_scope(mode: crate::station_mode::Mode) -> u8 {
    match mode {
        crate::station_mode::Mode::TestFire => 0x78,
        _ => 0x7f,
    }
}
fn selected_mask(available: u8, actual_abort: u8, pending: u8, explicit: u8) -> u8 {
    available | actual_abort | pending | explicit
}
/// HITL host Launch reset must ask the master to retire its actual owner.
/// AB participates when its retained readback says Launch, or the pair proof
/// was enforced. Missing/stale selected identities still need a fresh query.
fn launch_proof_mask(state: &AppState) -> u8 {
    if crate::runtime_mode::is_hitl()
        && state.local_flight_state_snapshot() == crate::types::FlightState::Launch
    {
        let inner = state.board_recovery.inner.lock().unwrap();
        let actuator_launch = inner.values[index(Board::ActuatorBoard).unwrap()]
            .as_ref().is_some_and(|row| row.value.state == Some(crate::types::FlightState::Launch as u8));
        return crate::board_abort::present_mask(state) & ((1 << index(Board::ValveBoard).unwrap())
            | if actuator_launch || crate::ignition_pair::proof_enforced(state) {
                1 << index(Board::ActuatorBoard).unwrap()
            } else { 0 });
    }
    let _ = state;
    0
}
/// Final publication already owns host state. Retain any Launch readback
/// owner before entering Stop/Abort fences; never reacquire it inside them.
pub(crate) struct PublicationLaunchProof<'a> {
    host: crate::types::FlightState,
    readbacks: Option<std::sync::MutexGuard<'a, Inner>>,
}
impl PublicationLaunchProof<'_> {
    fn mask(&self, state: &AppState) -> u8 {
        if !crate::runtime_mode::is_hitl() || self.host != crate::types::FlightState::Launch {
            return 0;
        }
        let inner = self.readbacks.as_ref().expect("Launch proof owner pre-acquired");
        let actuator_launch = inner.values[index(Board::ActuatorBoard).unwrap()]
            .as_ref().is_some_and(|row| row.value.state == Some(crate::types::FlightState::Launch as u8));
        crate::board_abort::present_mask(state) & ((1 << index(Board::ValveBoard).unwrap())
            | if actuator_launch || crate::ignition_pair::proof_enforced(state) {
                1 << index(Board::ActuatorBoard).unwrap()
            } else { 0 })
    }
}
impl Selection {
    pub(super) fn capture(state: &AppState, optional: &[String]) -> Result<Self, &'static str> {
        let mut explicit = 0;
        for sender in optional {
            let board = Board::from_sender_id(sender).ok_or("Unsupported optional reset board")?;
            let i = index(board).ok_or("Unsupported optional reset board")?;
            if explicit & (1 << i) != 0 {
                return Err("Duplicate optional reset board");
            }
            explicit |= 1 << i;
        }
        // Only actual retained admitted target ownership is mandatory here.
        // The prospective0x7f roster and unrelated Unknown rows are not targets.
        let pending = state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .filter(|pending| !pending.published)
            .map(|pending| {
                pending.targets.iter().try_fold(0u8, |mask, target| {
                    let board = Board::from_sender_id(&target.board)
                        .ok_or("Unknown retained recovery target")?;
                    let i = index(board).ok_or("Unsupported retained recovery target")?;
                    Ok::<_, &'static str>(mask | (1 << i))
                })
            })
            .transpose()?
            .unwrap_or(0);
        let (context, required, actual) = state
            .board_abort
            .capture_reset_context(|| base_mask(crate::station_mode::local_mode()))?;
        let present = crate::board_abort::present_mask(state);
        let scope = mode_scope(crate::station_mode::local_mode());
        // `actual` from a board that left the network can't be verified; a fresh
        // report counts as present even before board status records it.
        let targets = selected_mask(present & scope, actual & state.board_abort.launch_abort_mask(state), pending, explicit)
            | launch_proof_mask(state);
        Ok(Self {
            context,
            required,
            targets,
        })
    }
    pub(crate) fn current(&self, state: &AppState) -> Result<(), &'static str> {
        self.with_current(state, || Ok(()))
    }
    pub(crate) fn with_current<T>(
        &self,
        state: &AppState,
        apply: impl FnOnce() -> Result<T, &'static str>,
    ) -> Result<T, &'static str> {
        if launch_proof_mask(state) & !self.targets != 0 {
            return Err("Launch participation changed; prepare a fresh reset scope");
        }
        state.board_abort.with_reset_context(self.context, apply)
    }
    pub(crate) fn hold_for_publication<'a>(
        &self,
        state: &'a AppState,
        host: crate::types::FlightState,
    ) -> Result<PublicationLaunchProof<'a>, &'static str> {
        let readbacks = if crate::runtime_mode::is_hitl() && host == crate::types::FlightState::Launch {
            Some(state.board_recovery.inner.try_lock().map_err(|_| "Launch readback owner busy")?)
        } else {
            None
        };
        Ok(PublicationLaunchProof { host, readbacks })
    }
    pub(crate) fn with_current_publication<T>(
        &self,
        state: &AppState,
        proof: &PublicationLaunchProof<'_>,
        apply: impl FnOnce() -> Result<T, &'static str>,
    ) -> Result<T, &'static str> {
        state.board_abort.with_reset_context(self.context, || {
            if proof.mask(state) & !self.targets != 0 {
                return Err("Launch participation changed; prepare a fresh reset scope");
            }
            apply()
        })
    }
    pub(super) fn boards(&self) -> Vec<Board> {
        BOARDS
            .into_iter()
            .enumerate()
            .filter_map(|(i, board)| (self.targets & (1 << i) != 0).then_some(board))
            .collect()
    }
    pub(super) fn required_boards(&self) -> Vec<&'static str> {
        BOARDS
            .into_iter()
            .enumerate()
            .filter_map(|(i, board)| (self.required & (1 << i) != 0).then_some(board.sender_id()))
            .collect()
    }
    pub(crate) fn policy(&self) -> crate::launch_history::ResetProofPolicy {
        crate::launch_history::ResetProofPolicy {
            mode: crate::station_mode::local_mode(),
            required_mask: self.required,
            verified_mask: self.targets,
        }
    }
    pub(crate) fn valid_targets(&self, pending: &crate::launch_history::RecoveryPending) -> bool {
        pending.targets.len() == self.targets.count_ones() as usize
            && BOARDS.iter().enumerate().all(|(i, board)| {
                let count = pending
                    .targets
                    .iter()
                    .filter(|target| target.board == board.sender_id() && target.desired == 0)
                    .count();
                count == usize::from(self.targets & (1 << i) != 0)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scope_never_implicitly_enrolls_prospective_or_unknown_roster() {
        for optional_truth in 0..=0x7f {
            assert_eq!(selected_mask(0x78, 0, 0, 0), 0x78);
            assert_eq!(selected_mask(0, 0, 0, 0), 0);
            assert_eq!(
                selected_mask(0x78, optional_truth, 0, 0),
                0x78 | optional_truth
            );
            assert_eq!(selected_mask(0, 0, optional_truth, 0), optional_truth);
        }
    }
    #[test]
    fn explicit_optional_targets_cannot_remove_required_abort_or_admitted_targets() {
        assert_eq!(selected_mask(0x78, 1, 2, 4), 0x7f);
        assert_eq!(selected_mask(0, 1, 2, 0), 3);
    }
}

#[cfg(test)]
mod actual_policy_tests {
    use super::*;
    fn seen(state: &AppState, boards: &[Board]) {
        let mut status = state.board_status.lock().unwrap();
        for board in boards {
            status.get_mut(board).unwrap().last_seen_instant = Some(std::time::Instant::now());
        }
    }
    /// A reset covers what is available and nothing else in this mode's scope.
    #[tokio::test]
    async fn reset_targets_available_boards_and_absent_ones_cannot_hold_it() {
        let state = crate::state::tests::test_app_state().await;
        assert!(Selection::capture(&state, &[]).unwrap().boards().is_empty());
        seen(&state, &[Board::ValveBoard]);
        assert_eq!(Selection::capture(&state, &[]).unwrap().boards(), vec![Board::ValveBoard]);
    }
    #[test]
    fn base_policy_is_mode_fixed_not_link_or_layout_derived() {
        use crate::station_mode::Mode::*;
        for mode in [Normal, TestFire, Hitl, Testing] { assert_eq!(base_mask(mode), 0); }
        assert_eq!(mode_scope(Normal), 0x7f);
        assert_eq!(mode_scope(TestFire), 0x78);
        assert_eq!(mode_scope(Hitl), 0x7f);
        assert_eq!(mode_scope(Testing), 0x7f);
    }
    #[tokio::test]
    async fn actual_abort_and_explicit_optional_are_mandatory_without_unknown_roster() {
        let state = crate::state::tests::test_app_state().await;
        let expected = 0;
        let selection = Selection::capture(&state, &[]).unwrap();
        assert_eq!(selection.targets, expected);
        state
            .board_abort
            .observe_abort(Board::RFBoard, std::time::Instant::now(), 100);
        assert!(selection.current(&state).is_err());
        let selection = Selection::capture(&state, &["FC".into()]).unwrap();
        if crate::runtime_mode::is_test_fire() {
            // A test fire's reset covers the stand only: an aborted AV-bay
            // board is not a mandatory target (it does not block Launch either),
            // but the operator may still name it explicitly.
            assert_eq!(selection.targets, expected | 2);
            let selection = Selection::capture(&state, &["RF".into(), "FC".into()]).unwrap();
            assert_eq!(selection.targets, expected | 3);
            state
                .board_abort
                .observe_abort(Board::ValveBoard, std::time::Instant::now(), 101);
            let selection = Selection::capture(&state, &[]).unwrap();
            assert_eq!(selection.targets, 1 << index(Board::ValveBoard).unwrap());
        } else {
            assert_eq!(selection.targets, expected | 3);
        }
        assert!(Selection::capture(&state, &["FC".into(), "FC".into()]).is_err());
    }
    #[tokio::test]
    async fn launch_reset_requires_master_and_cannot_shed_new_pair_participation() {
        crate::mode_suite!(Hitl);
        let state = crate::state::tests::test_app_state().await;
        seen(&state, &[Board::ValveBoard]);
        *state.state.lock().unwrap() = crate::types::FlightState::Launch;
        let selection = Selection::capture(&state, &[]).unwrap();
        assert_eq!(selection.boards(), vec![Board::ValveBoard]);
        assert_eq!(selection.policy().required_mask, 0);
        seen(&state, &[Board::ActuatorBoard]);
        state.toggle_hitl_safety_interlock(3);
        assert!(selection.current(&state).is_err());
        let selection = Selection::capture(&state, &[]).unwrap();
        assert_eq!(selection.boards(), vec![Board::ActuatorBoard, Board::ValveBoard]);
    }
    #[tokio::test]
    async fn launch_reset_keeps_observed_actuator_even_after_its_proof_expires() {
        crate::mode_suite!(Hitl);
        let state = crate::state::tests::test_app_state().await;
        seen(&state, &[Board::ValveBoard]);
        *state.state.lock().unwrap() = crate::types::FlightState::Launch;
        let before = Selection::capture(&state, &[]).unwrap();
        let at = Stamp::now();
        let mut old = at;
        old.wall -= Duration::from_secs(60);
        state.board_recovery.inner.lock().unwrap().values[index(Board::ActuatorBoard).unwrap()] = Some(Observation {
            received: old, observed_after: old,
            value: Readback { result: 0, state: Some(crate::types::FlightState::Launch as u8),
                flags: 0x3c, nonce: 1, boot: 1, epoch: 1, uptime: 1, outputs: 0, issuer: 1 },
        });
        seen(&state, &[Board::ActuatorBoard]);
        assert!(before.current(&state).is_err(), "new participation cannot escape frozen scope");
        let after = Selection::capture(&state, &[]).unwrap();
        assert_eq!(after.boards(), vec![Board::ActuatorBoard, Board::ValveBoard]);
        assert!(!state.board_recovery.inner.lock().unwrap().values[index(Board::ActuatorBoard).unwrap()].as_ref().unwrap().fresh(at));
    }
    #[tokio::test]
    async fn empty_hitl_is_explicit_host_only_and_policy_aba_retires_original_intent() {
        crate::mode_suite!(Hitl);
        let state = crate::state::tests::test_app_state().await;
        let selection = Selection::capture(&state, &[]).unwrap();
        assert!(selection.boards().is_empty());
        assert_eq!(selection.policy().verified_mask, 0);
        state.toggle_hitl_safety_interlock(3);
        state.toggle_hitl_safety_interlock(3);
        assert!(selection.current(&state).is_err());
    }
    #[tokio::test]
    async fn publication_uses_held_host_state_without_relocking_and_keeps_scope_fences() {
        crate::mode_suite!(Hitl);
        let state = crate::state::tests::test_app_state().await;
        let selection = Selection::capture(&state, &[]).unwrap();
        let host = state.state.lock().unwrap();
        let proof = selection.hold_for_publication(&state, *host).unwrap();
        assert!(selection.with_current_publication(&state, &proof, || Ok(())).is_ok());
        state.toggle_hitl_safety_interlock(3);
        assert!(selection.with_current_publication(&state, &proof, || -> Result<(), &'static str> { panic!("changed context cannot publish") }).is_err());
    }
    #[tokio::test]
    async fn publication_retains_launch_readbacks_and_refuses_new_actuator_participation() {
        crate::mode_suite!(Hitl);
        let state = crate::state::tests::test_app_state().await;
        seen(&state, &[Board::ValveBoard]);
        *state.state.lock().unwrap() = crate::types::FlightState::Launch;
        let selection = Selection::capture(&state, &[]).unwrap();
        assert_eq!(selection.boards(), vec![Board::ValveBoard]);
        let at = Stamp::now();
        state.board_recovery.inner.lock().unwrap().values[index(Board::ActuatorBoard).unwrap()] = Some(Observation {
            received: at, observed_after: at,
            value: Readback { result: 0, state: Some(crate::types::FlightState::Launch as u8),
                flags: 0x3c, nonce: 1, boot: 1, epoch: 1, uptime: 1, outputs: 0, issuer: 1 },
        });
        seen(&state, &[Board::ActuatorBoard]);
        let host = state.state.lock().unwrap();
        let proof = selection.hold_for_publication(&state, *host).unwrap();
        assert!(state.board_recovery.inner.try_lock().is_err(), "readback owner remains held through publication");
        assert!(selection.with_current_publication(&state, &proof, || -> Result<(), &'static str> { panic!("new AB owner cannot be omitted") }).is_err());
    }

}
