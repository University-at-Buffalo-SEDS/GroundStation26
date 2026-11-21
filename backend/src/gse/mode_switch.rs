//! GSE state that depends on the station mode, put back to what a fresh start
//! in the new mode would have. Called by the station mode switch while the
//! runtime-mode gate is held exclusively, after the new mode is in force and
//! only once the station was found idle (nothing claimed, no sequence, no
//! turbocharge), so no operation can observe the intermediate state.
use super::*;

pub(crate) fn reset_for_mode_switch(state: &AppState) {
    {
        let mut rt = state.gse.lock().unwrap();
        // The operational (qualification) runtime exists outside HITL only;
        // a fresh start builds it from its default and the saved file.
        rt.operational = Default::default();
        // Engine status and claim are rebuilt by the qualification restore,
        // as at start. The loaded configuration is kept (same file).
        rt.engine.status = Engine::default().status;
        rt.engine.claimed = false;
        // Turbocharge evidence and its binding name the previous mode's
        // qualification generation; a fresh process never had them.
        rt.turbocharge = None;
        rt.turbo_evidence = None;
        rt.turbo_binding = None;
        reset_phase_control_locked(state, &mut rt);
    }
    restore_operational(state);
}

/// HITL phase control (ground station or rocket) starts on the ground station
/// in every mode. The revision only grows, so UIs see the change.
pub(crate) fn reset_phase_control(state: &AppState) {
    let mut rt = state.gse.lock().unwrap();
    reset_phase_control_locked(state, &mut rt);
}

fn reset_phase_control_locked(state: &AppState, rt: &mut Runtime) {
    // The fill-resume deadband follows the mode, as a fresh engine would.
    rt.engine.legacy_resume_deadband = station_engine().legacy_resume_deadband;
    rt.ground_station_control = true;
    rt.phase_control_revision = rt.phase_control_revision.saturating_add(1);
    state
        .phase_control_selected
        .store(true, std::sync::atomic::Ordering::Release);
    state
        .phase_control_revision
        .store(rt.phase_control_revision, std::sync::atomic::Ordering::Release);
}
