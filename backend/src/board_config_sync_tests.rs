use super::*;
use crate::actuator_plumbing::http_tests::Fixture;
use crate::types::FlightState;

const AUX: u8 = 2;

fn mismatch(class: Class, identity: u64) -> Observed {
    Observed::Mismatch {
        class,
        board: "x".into(),
        identity,
    }
}

#[test]
fn a_default_or_known_value_is_pushed_and_counts_as_synced_only_on_readback() {
    for class in [Class::Default, Class::Known] {
        let mut sync = ItemSync::default();
        let t = Instant::now();
        let (decision, _) = sync.step(Item::AbFillDrive, t, Some(1), mismatch(class, 0), Ok(()), None);
        assert_eq!(decision, Decision::Push);
        assert_eq!(sync.phase, Phase::Resyncing);
        // Within the settle time: wait for the readback, no second push.
        let (decision, _) = sync.step(
            Item::AbFillDrive,
            t + Duration::from_secs(1),
            Some(1),
            mismatch(class, 0),
            Ok(()),
            None,
        );
        assert_eq!((decision, sync.phase), (Decision::None, Phase::Resyncing));
        let (_, events) = sync.step(
            Item::AbFillDrive,
            t + Duration::from_secs(2),
            Some(1),
            Observed::Match,
            Ok(()),
            None,
        );
        assert!(events.verified, "the readback verifies the push");
        assert_eq!(sync.phase, Phase::Synced);
        let (_, events) = sync.step(
            Item::AbFillDrive,
            t + Duration::from_secs(3),
            Some(1),
            Observed::Match,
            Ok(()),
            None,
        );
        assert!(!events.verified, "verified once");
    }
}

#[test]
fn a_push_is_deferred_with_the_reason_while_the_state_forbids_it() {
    let mut sync = ItemSync::default();
    let t = Instant::now();
    let armed = Err("Deferred: board config is only re-applied in Startup or Idle (station is in Armed).".to_string());
    let (decision, _) = sync.step(
        Item::DaqCalibration,
        t,
        Some(1),
        mismatch(Class::Default, 7),
        armed,
        None,
    );
    assert_eq!((decision, sync.phase), (Decision::None, Phase::OutOfSync));
    assert!(
        sync.message.contains("out of sync") && sync.message.contains("Armed"),
        "{}",
        sync.message
    );
    let (decision, _) = sync.step(
        Item::DaqCalibration,
        t,
        Some(1),
        mismatch(Class::Default, 7),
        Ok(()),
        None,
    );
    assert_eq!(decision, Decision::Push, "resyncs once the state allows it");
}

#[test]
fn an_unknown_board_value_is_a_conflict_until_the_operator_chooses_to_overwrite() {
    let mut sync = ItemSync::default();
    let t = Instant::now();
    let (decision, events) = sync.step(
        Item::AbFillDrive,
        t,
        Some(1),
        mismatch(Class::Unknown, 0x1234),
        Ok(()),
        None,
    );
    assert_eq!((decision, sync.phase), (Decision::None, Phase::Conflict));
    assert!(events.new_conflict);
    assert_eq!(sync.conflict_identity, Some(0x1234));
    let (_, events) = sync.step(
        Item::AbFillDrive,
        t,
        Some(1),
        mismatch(Class::Unknown, 0x1234),
        Ok(()),
        None,
    );
    assert!(!events.new_conflict, "raised once");
    // Overwriting another unknown value is not authorized by this choice.
    let (decision, _) = sync.step(
        Item::AbFillDrive,
        t,
        Some(1),
        mismatch(Class::Unknown, 0x9999),
        Ok(()),
        Some(0x1234),
    );
    assert_eq!((decision, sync.phase), (Decision::None, Phase::Conflict));
    let (decision, _) = sync.step(
        Item::AbFillDrive,
        t,
        Some(1),
        mismatch(Class::Unknown, 0x1234),
        Ok(()),
        Some(0x1234),
    );
    assert_eq!(decision, Decision::Push);
}

#[test]
fn a_board_that_never_takes_the_value_is_out_of_sync_and_retried_slowly_and_a_reboot_restarts() {
    let mut sync = ItemSync::default();
    let t = Instant::now();
    let mut pushes = Vec::new();
    let mut gave_up = 0;
    for second in 0..200u64 {
        let (decision, events) = sync.step(
            Item::AbFillDrive,
            t + Duration::from_secs(second),
            Some(1),
            mismatch(Class::Default, 0),
            Ok(()),
            None,
        );
        if decision == Decision::Push {
            pushes.push(second);
        }
        gave_up += u32::from(events.gave_up);
    }
    assert_eq!(&pushes[..3], &[0, 4, 8]);
    assert!(pushes.len() <= 6, "{pushes:?}");
    assert!(pushes.windows(2).skip(2).all(|w| w[1] - w[0] >= 60), "{pushes:?}");
    assert_eq!(gave_up, 1);
    let (decision, _) = sync.step(
        Item::AbFillDrive,
        t + Duration::from_secs(201),
        Some(2),
        mismatch(Class::Default, 0),
        Ok(()),
        None,
    );
    assert_eq!(decision, Decision::Push, "a new boot gets quick attempts again");
}

#[tokio::test]
async fn identities_this_station_sent_are_known() {
    let fixture = Fixture::new().await;
    let identity = rand::random::<u32>() | 1;
    assert!(!daq_known(identity));
    note_daq_published(&fixture.host, identity);
    assert!(daq_known(identity));
    let source = 1_793_563_000_000 + u64::from(rand::random::<u16>());
    note_ab_drive_source(source);
    assert!(ab_known(crate::actuator_plumbing::setting_identity(source)));
}

/// Reflashed actuator: factory stepper default (identity 0) with idle holding
/// torque, while the station saved auxiliary. In Idle the station re-applies
/// it and only the actuator's readback marks it synced.
#[tokio::test]
async fn reflashed_actuator_is_restored_in_idle_and_verified_by_readback() {
    let mut fixture = Fixture::new().await;
    crate::actuator_plumbing::set_desired_for_test(&fixture.state, AUX, 1, 1_000);
    fixture.prime().await;
    assert_eq!((fixture.mode, fixture.drive_id, fixture.outputs), (0, 0, 16));
    assert!(
        crate::actuator_plumbing::motion_ready(&fixture.state)
            .unwrap_err()
            .contains("out of sync")
    );
    tick(&fixture.state, Instant::now());
    let item = item_for_test(&fixture.state, Item::AbFillDrive);
    assert_eq!(item.phase, Phase::Resyncing, "{}", item.message);
    assert!(item.message.contains("built-in default"), "{}", item.message);
    fixture.pump();
    assert_eq!(
        (fixture.mode, fixture.sets),
        (AUX, 1),
        "the actuator got the saved drive"
    );
    assert_ne!(fixture.drive_id, 0);
    tick(&fixture.state, Instant::now());
    let item = item_for_test(&fixture.state, Item::AbFillDrive);
    assert_eq!(item.phase, Phase::Synced, "{}", item.message);
    assert!(item.message.contains("verified"), "{}", item.message);
    crate::actuator_plumbing::motion_ready(&fixture.state).unwrap();
    // The applied identity is the station's own from now on.
    assert!(ab_known(fixture.drive_id));
    let view = view(&fixture.state);
    assert_eq!(view["boards"][1]["board"], "AB");
    assert_eq!(view["boards"][1]["state"], "synced");
}

#[tokio::test]
async fn actuator_resync_is_deferred_while_armed_and_runs_back_in_idle() {
    let mut fixture = Fixture::new().await;
    crate::actuator_plumbing::set_desired_for_test(&fixture.state, AUX, 1, 1_000);
    fixture.prime().await;
    fixture.state.set_local_flight_state(FlightState::Armed);
    tick(&fixture.state, Instant::now());
    fixture.pump();
    let item = item_for_test(&fixture.state, Item::AbFillDrive);
    assert_eq!(item.phase, Phase::OutOfSync, "{}", item.message);
    assert!(item.message.contains("Startup or Idle"), "{}", item.message);
    assert_eq!(fixture.sets, 0, "nothing pushed while Armed");
    let blocked = crate::actuator_plumbing::motion_ready(&fixture.state).unwrap_err();
    assert!(blocked.contains("out of sync"), "{blocked}");
    assert_eq!(view(&fixture.state)["boards"][1]["state"], "out_of_sync");
    fixture.state.set_local_flight_state(FlightState::Idle);
    tick(&fixture.state, Instant::now());
    fixture.pump();
    assert_eq!(fixture.sets, 1);
    tick(&fixture.state, Instant::now());
    assert_eq!(item_for_test(&fixture.state, Item::AbFillDrive).phase, Phase::Synced);
}

#[tokio::test]
async fn an_actuator_drive_the_station_never_saved_is_a_conflict_not_overwritten() {
    let mut fixture = Fixture::new().await;
    crate::actuator_plumbing::set_desired_for_test(&fixture.state, 0, 3, 1_000);
    fixture.mode = AUX;
    fixture.outputs = 0;
    fixture.drive_id = 0x5A5A ^ rand::random::<u16>() | 0x8000;
    fixture.prime().await;
    tick(&fixture.state, Instant::now());
    fixture.pump();
    let item = item_for_test(&fixture.state, Item::AbFillDrive);
    assert_eq!(item.phase, Phase::Conflict, "{}", item.message);
    assert!(item.message.contains("never sent"), "{}", item.message);
    assert_eq!(fixture.sets, 0, "a conflict is never silently overwritten");
    let view = view(&fixture.state);
    assert_eq!(view["boards"][1]["items"][0]["conflict"], true);
    assert_eq!(view["boards"][1]["items"][0]["can_keep_board"], true);
    // Keep the station's value: re-applied under the normal gates.
    resolve_conflict(&fixture.state, Item::AbFillDrive, "station").unwrap();
    tick(&fixture.state, Instant::now());
    fixture.pump();
    assert_eq!((fixture.sets, fixture.mode), (1, 0));
    tick(&fixture.state, Instant::now());
    assert_eq!(item_for_test(&fixture.state, Item::AbFillDrive).phase, Phase::Synced);
}

#[tokio::test]
async fn keeping_the_actuators_drive_adopts_it_without_a_set() {
    let mut fixture = Fixture::new().await;
    crate::actuator_plumbing::set_desired_for_test(&fixture.state, 0, 3, 1_000);
    fixture.mode = AUX;
    fixture.outputs = 0;
    fixture.drive_id = 0x4000 | rand::random::<u16>() >> 2;
    fixture.prime().await;
    tick(&fixture.state, Instant::now());
    assert_eq!(item_for_test(&fixture.state, Item::AbFillDrive).phase, Phase::Conflict);
    let message = resolve_conflict(&fixture.state, Item::AbFillDrive, "board").unwrap();
    assert!(message.contains("auxiliary"), "{message}");
    tick(&fixture.state, Instant::now());
    fixture.pump();
    assert_eq!(fixture.sets, 0);
    assert_eq!(item_for_test(&fixture.state, Item::AbFillDrive).phase, Phase::Synced);
    assert!(
        resolve_conflict(&fixture.state, Item::AbFillDrive, "board").is_err(),
        "nothing left to resolve"
    );
}

fn non_default_calibration(state: &AppState) -> u32 {
    let mut calibration = state.loadcell_calibration.lock().unwrap();
    calibration.ch1.m = Some(2.5 + f32::from(rand::random::<u8>()) / 1e3);
    crate::network_variables::daq_calibration_identity(&calibration).unwrap()
}

/// Reflashed DAQ that could not load the station's values (still on its
/// built-in calibration after the load grace): re-published in Idle and
/// verified by the identity in its next health report.
#[tokio::test]
async fn reflashed_daq_is_republished_in_idle_and_verified_by_its_health_report() {
    let fixture = Fixture::new().await;
    let state = &fixture.state;
    let station = non_default_calibration(state);
    let t = Instant::now();
    let default = crate::network_variables::daq_default_identity();
    // Shortly after boot it is still loading: not judged yet.
    observe_daq_health_for_test(state, &daq_health_words(default, 0, 5_000), t);
    tick(state, t);
    assert_eq!(item_for_test(state, Item::DaqCalibration).phase, Phase::Resyncing);
    observe_daq_health_for_test(state, &daq_health_words(default, 0, 40_000), t);
    tick(state, t);
    let item = item_for_test(state, Item::DaqCalibration);
    assert_eq!(item.phase, Phase::Resyncing, "{}", item.message);
    assert!(item.message.contains("built-in default"), "{}", item.message);
    assert!(daq_known(station), "the push records the station's identity");
    observe_daq_health_for_test(state, &daq_health_words(station, 0x1F, 45_000), t);
    tick(state, t);
    let item = item_for_test(state, Item::DaqCalibration);
    assert_eq!(item.phase, Phase::Synced);
    assert!(item.message.contains("verified"));
}

#[tokio::test]
async fn daq_resync_is_deferred_while_armed() {
    let fixture = Fixture::new().await;
    let state = &fixture.state;
    non_default_calibration(state);
    state.set_local_flight_state(FlightState::Armed);
    let t = Instant::now();
    observe_daq_health_for_test(
        state,
        &daq_health_words(crate::network_variables::daq_default_identity(), 0, 60_000),
        t,
    );
    tick(state, t);
    let item = item_for_test(state, Item::DaqCalibration);
    assert_eq!(item.phase, Phase::OutOfSync, "{}", item.message);
    assert!(item.message.contains("Armed"));
}

#[tokio::test]
async fn a_daq_calibration_from_another_writer_is_a_conflict() {
    let fixture = Fixture::new().await;
    let state = &fixture.state;
    let station = non_default_calibration(state);
    let stray = station ^ 0x8000_0001;
    let t = Instant::now();
    observe_daq_health_for_test(state, &daq_health_words(stray, 0x1F, 60_000), t);
    tick(state, t);
    let item = item_for_test(state, Item::DaqCalibration);
    assert_eq!(item.phase, Phase::Conflict, "{}", item.message);
    assert!(
        resolve_conflict(state, Item::DaqCalibration, "board").is_err(),
        "DAQ values cannot be adopted"
    );
    resolve_conflict(state, Item::DaqCalibration, "station").unwrap();
    tick(state, t);
    assert_eq!(item_for_test(state, Item::DaqCalibration).phase, Phase::Resyncing);
    // A DAQ reboot is seen from its uptime and restarts the resync.
    observe_daq_health_for_test(state, &daq_health_words(station, 0x1F, 1_000), t);
    tick(state, t);
    assert_eq!(item_for_test(state, Item::DaqCalibration).phase, Phase::Synced);
}
