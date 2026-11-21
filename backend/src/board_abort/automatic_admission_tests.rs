// Actual authoritative register synchronization, not a physical Launch test.
use super::*;
#[test]
fn held_register_refuses_without_wait_or_running_admission() {
    let runtime = Runtime::default();
    let revision = runtime.abort_revision();
    let _held = runtime.register.lock().unwrap();
    assert_eq!(
        runtime.try_admit_automatic_prelaunch::<()>(revision, || panic!("Busy acquired ownership")),
        Err(AutomaticAdmissionError::Busy)
    );
}
#[test]
fn ingress_guard_is_held_through_admission_and_active_abort_cannot_run_it() {
    let runtime = Runtime::default();
    let revision = runtime.abort_revision();
    assert_eq!(
        runtime.try_admit_automatic_prelaunch(revision, || {
            assert!(
                runtime.register.try_lock().is_err(),
                "guard must survive actual callback/host commit"
            );
            17
        }),
        Ok(17)
    );
    runtime.observe_abort(Board::ActuatorBoard, Instant::now(), 1234);
    let current = runtime.abort_revision();
    assert!(current > revision);
    assert_eq!(
        runtime
            .try_admit_automatic_prelaunch::<()>(revision, || panic!("changed original admitted")),
        Err(AutomaticAdmissionError::Changed)
    );
    assert_eq!(
        runtime.try_admit_automatic_prelaunch::<()>(current, || panic!("active Abort admitted")),
        Err(AutomaticAdmissionError::ActiveAbort)
    );
}
