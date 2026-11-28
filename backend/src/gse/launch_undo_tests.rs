//! Undo of a Launch none of whose packets the SDK took, on the production
//! recording paths (`record_fire_intent_for_pressed_launch`) and files.
use super::*;
use crate::gse::operational::Saved;
use gse_sequence::qualification::MissionStage;

struct Scene {
    state: Arc<AppState>,
    qualification: std::path::PathBuf,
    journal: std::path::PathBuf,
}
impl Drop for Scene {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.qualification);
        let _ = std::fs::remove_file(&self.journal);
        let _ = std::fs::remove_file(self.journal.with_extension("analytics.json"));
    }
}

/// An armed station whose qualification and launch journal are real files.
async fn scene() -> Scene {
    let state = super::super::operational_policy::tests::fixture().await;
    let qualification = super::super::armed_fixture_for_test(&state, state.topology_router.get().unwrap());
    let journal = std::env::temp_dir().join(format!("launch-undo-journal-{:032x}.json", rand::random::<u128>()));
    *state.launch_history.lock().unwrap() = crate::launch_history::Store::open(journal.clone(), false, None, 10_000, 0).unwrap();
    Scene { state, qualification, journal }
}

fn record(state: &Arc<AppState>) -> LaunchAttempt {
    let revision = crate::board_abort::capture_launch_revision_named(state, "Launch");
    super::super::record_fire_intent_for_pressed_launch(state, revision, None).unwrap()
}

fn refused(admitted: usize) -> LaunchQueueError {
    LaunchQueueError {
        admitted,
        message: format!("Launch SDK admission accepted {admitted}/3 packets: Io(\"router core busy\"){}", super::super::LAUNCH_STAYS_INHIBITED),
    }
}

fn stage(state: &AppState) -> MissionStage {
    state.gse.lock().unwrap().operational.saved.qualification.stage()
}

/// Recorded, as a press does before offering any packet.
fn assert_recorded(scene: &Scene) {
    let history = scene.state.launch_history.lock().unwrap();
    assert!(history.launch_locked() && history.automatic_actions_inhibited());
    drop(history);
    assert_eq!(stage(&scene.state), MissionStage::Fire);
    assert_eq!(Saved::load(&scene.qualification).unwrap().qualification.stage(), MissionStage::Fire);
}

#[tokio::test]
async fn an_unsent_launch_is_undone_and_stays_undone_after_a_restart() {
    crate::mode_suite!(Normal, TestFire, Testing, IntegratedGroundTest);
    let scene = scene().await;
    let state = &scene.state;
    let claimed = state.gse.lock().unwrap().engine.claimed;
    let attempt = record(state);
    assert_recorded(&scene);
    undo_unsent_launch(state, attempt, &refused(0), 3).unwrap();
    // Launch is available again, exactly as before the press.
    {
        let history = state.launch_history.lock().unwrap();
        assert!(!history.launch_locked() && !history.automatic_actions_inhibited());
        assert_eq!(super::super::launch_journal_block(&history), None);
        let undone = history.undone_launches();
        assert_eq!(undone.len(), 1, "the attempt is kept as refused and undone");
        assert_eq!((undone[0].packets, undone[0].mode), (3, crate::station_mode::local_mode()));
        assert!(undone[0].reason.contains("router core busy"));
    }
    {
        let rt = state.gse.lock().unwrap();
        assert_eq!(rt.operational.saved.qualification.stage(), MissionStage::Armed);
        assert!(rt.operational.saved.qualification.launch_qualified());
        assert!(rt.operational.saved.pending_checklists.is_empty());
        assert_eq!(rt.engine.claimed, claimed);
    }
    // A restart right after the undo reads it back from both files.
    let reopened = crate::launch_history::Store::open(scene.journal.clone(), false, None, 10_000, 0).unwrap();
    assert!(!reopened.launch_locked() && !reopened.automatic_actions_inhibited());
    assert_eq!(reopened.undone_launches().len(), 1);
    let saved = Saved::load(&scene.qualification).unwrap();
    assert_eq!(saved.qualification.stage(), MissionStage::Armed);
    assert!(saved.qualification.launch_qualified());
    // And the operator can simply press Launch again.
    let again = record(state);
    assert_recorded(&scene);
    drop(again);
}

#[tokio::test]
async fn a_launch_with_any_packet_taken_is_never_undone() {
    crate::mode_suite!(Normal, TestFire, Testing, IntegratedGroundTest);
    for admitted in [1, 2, 3] {
        let scene = scene().await;
        let attempt = record(&scene.state);
        let error = undo_unsent_launch(&scene.state, attempt, &refused(admitted), 3).unwrap_err();
        assert!(error.contains(&format!("{admitted}/3 of its packets were taken")), "{error}");
        assert_recorded(&scene);
        assert!(scene.state.launch_history.lock().unwrap().undone_launches().is_empty());
    }
}

#[tokio::test]
async fn an_abort_after_the_press_keeps_the_attempt_recorded() {
    crate::mode_suite!(Normal, TestFire, Testing, IntegratedGroundTest);
    let scene = scene().await;
    let attempt = record(&scene.state);
    scene.state.manual_abort_intent.request();
    let error = undo_unsent_launch(&scene.state, attempt, &refused(0), 3).unwrap_err();
    assert!(error.contains("Manual Abort"), "{error}");
    assert_recorded(&scene);
    assert!(scene.state.launch_history.lock().unwrap().undone_launches().is_empty());
}

#[tokio::test]
async fn a_latched_abort_keeps_the_attempt_recorded_and_latched() {
    crate::mode_suite!(Normal, TestFire, Testing, IntegratedGroundTest);
    let scene = scene().await;
    let attempt = record(&scene.state);
    scene.state.set_abort_indicator_latched(true);
    let error = undo_unsent_launch(&scene.state, attempt, &refused(0), 3).unwrap_err();
    assert!(error.contains("Abort is latched"), "{error}");
    assert_recorded(&scene);
    assert!(scene.state.abort_indicator_latched(), "the undo never clears an Abort latch");
}

/// An Abort pressed while the undo is writing the qualification record (the
/// disk stalled) keeps Launch locked: the journal is never undone after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_during_the_undo_keeps_launch_locked() {
    crate::mode_suite!(Normal, TestFire, Testing, IntegratedGroundTest);
    if crate::operator_queue::isolated_native_case(module_path!(), "an_abort_during_the_undo_keeps_launch_locked") {
        return;
    }
    let scene = scene().await;
    let attempt = record(&scene.state);
    let stall = crate::disk_stall::Stall::begin(crate::disk_stall::Site::Qualification);
    let state = scene.state.clone();
    let undo = std::thread::spawn(move || undo_unsent_launch(&state, attempt, &refused(0), 3));
    assert!(tokio::task::block_in_place(|| stall.entered()), "the undo never wrote the qualification record");
    // The Abort request takes no lock the undo holds across its disk write.
    let pressed = std::time::Instant::now();
    scene.state.manual_abort_intent.request();
    assert!(pressed.elapsed() < std::time::Duration::from_millis(200), "the Abort waited for the undo");
    drop(stall);
    let error = undo.join().unwrap().unwrap_err();
    assert!(error.contains("Manual Abort"), "{error}");
    let history = scene.state.launch_history.lock().unwrap();
    assert!(history.launch_locked() && history.automatic_actions_inhibited(), "Launch stays locked");
    assert!(history.undone_launches().is_empty());
}
