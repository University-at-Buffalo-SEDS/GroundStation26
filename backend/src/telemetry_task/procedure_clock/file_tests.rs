//! Process isolation keeps the explicit clock and fake persistence paths local.
use super::*;
#[test]
fn explicit_file_clock_and_saved_replay_are_isolated() {
    const NAME: &str = "telemetry_task::procedure_clock::file_tests::explicit_file_clock_and_saved_replay_are_isolated";
    if std::env::var_os("GS_MODEL_CLOCK_CHILD").is_some() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(child());
        return;
    }
    let directory =
        std::env::temp_dir().join(format!("gs-model-purpose-{}", rand::random::<u128>()));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("clock"), "100000").unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env("GS_MODEL_CLOCK_CHILD", "1")
        .env("GS_MODEL_CLOCK_ROOT", &directory)
        .env("GS_SIM_CLOCK_FILE", directory.join("clock"))
        .env("GS_SIM_NETWORK_EPOCH_MS", "1767225600000")
        .env_remove("GS_STATION_PAIRING_FILE");
    for (name, file) in [
        ("GS_NETWORK_VARIABLE_CACHE", "variables.json"),
        ("GS_CHECKLISTS_FILE", "checklists.json"),
        ("GS_GSE_CONFIG", "gse.json"),
        ("GS_GSE_QUALIFICATION", "qualification.json"),
        ("GS_AUTO_ZERO_CONFIG", "zero.json"),
        ("GS_FLIGHT_SETUP_CONFIG", "flight.json"),
        ("GS_LOADCELL_CALIBRATION_PATH", "calibration.json"),
        ("GS_CALIBRATION_CONFIG_PATH", "calibration-config.json"),
    ] {
        command.env(name, directory.join(file));
    }
    let mut process = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while process.try_wait().unwrap().is_none() {
        if start.elapsed() > std::time::Duration::from_secs(30) {
            process.kill().unwrap();
            let _ = process.wait();
            panic!("isolated model-clock test timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = process.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_dir_all(directory).unwrap();
}
async fn child() {
    use crate::{
        telemetry_db::{DbQueueItem, DbWrite},
        types::{Board, FlightState},
    };
    let directory = std::path::PathBuf::from(std::env::var_os("GS_MODEL_CLOCK_ROOT").unwrap());
    let state = crate::state::tests::test_app_state().await;
    let mut changes = state.launch_clock_tx.subscribe();
    for board in Board::ALL {
        state.mark_board_seen(board.sender_id(), super::super::get_current_timestamp_ms());
    }
    super::super::start_launch_clock_from_valve_sequence_status(&state);
    let original = state.launch_clock_snapshot();
    assert_eq!(original.anchor_timestamp_ms, Some(1_767_225_700_000));
    changes.try_recv().unwrap();
    for text in ["100000", "99999", "partial", ""] {
        std::fs::write(directory.join("clock"), text).unwrap();
        for _ in 0..10 {
            assert_eq!(
                state.launch_clock_presentation_snapshot().duration_ms,
                Some(10000)
            );
        }
    }
    assert!(
        changes.try_recv().is_err(),
        "presentation must not mutate canonical clock/republish DAQ"
    );
    assert!(!directory.join("variables.json").exists());
    std::fs::write(directory.join("clock"), "100833").unwrap();
    assert_eq!(
        state.launch_clock_presentation_snapshot().duration_ms,
        Some(9167)
    );
    assert_eq!(state.launch_clock_snapshot(), original);
    std::fs::write(directory.join("clock"), "110000").unwrap();
    let (db_tx, mut db_rx) = tokio::sync::mpsc::channel(8);
    let packet = sedsnet::packet::Packet::new(
        crate::telemetry_schema::data_type("FLIGHT_STATE"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        "FC",
        1_767_225_710_000,
        std::sync::Arc::from([FlightState::Ascent as u8]),
    )
    .unwrap();
    let wall_before = super::super::get_current_timestamp_ms() as i64;
    super::super::handle_packet(&state, &db_tx, &super::super::DbOverflow, packet).await;
    let ascent = state.launch_clock_snapshot();
    assert_eq!(ascent.kind, LaunchClockKind::TPlus);
    assert_eq!(ascent.anchor_timestamp_ms, Some(1_767_225_710_000));
    super::super::transition_launch_clock_to_t_plus_from_pilot_open(&state);
    assert_eq!(
        state.launch_clock_snapshot(),
        ascent,
        "late pilot must preserve correct first virtual T0"
    );
    changes.try_recv().unwrap(); // one canonical Ascent transition
    state.set_launch_clock(original.clone()); // stale TMinus must not re-anchor T+
    assert_eq!(state.launch_clock_snapshot(), ascent);
    assert!(changes.try_recv().is_err());
    std::fs::write(directory.join("clock"), "112500").unwrap();
    assert_eq!(
        state.launch_clock_presentation_snapshot().duration_ms,
        Some(2500)
    );
    assert!(
        super::super::get_current_timestamp_ms() as i64 > 1_767_225_712_500 + 86_400_000,
        "receipt time must remain host wall time"
    );
    let mut writes = Vec::new();
    while let Ok(DbQueueItem::Write(write)) = db_rx.try_recv() {
        writes.push(write);
    }
    let phase_wall = writes
        .iter()
        .find_map(|write| {
            if let DbWrite::FlightState { timestamp_ms, .. } = write {
                Some(*timestamp_ms)
            } else {
                None
            }
        })
        .unwrap();
    assert!(phase_wall >= wall_before);
    writes.push(DbWrite::Telemetry {
        timestamp_ms: phase_wall,
        source_timestamp_ms: Some(1_767_225_710_000),
        data_type: "ALTITUDE".into(),
        sender_id: "FC".into(),
        values_json: Some("[123.0]".into()),
        payload_json: "[]".into(),
    });
    let database = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
    crate::telemetry_db::ensure_telemetry_schema(&database)
        .await
        .unwrap();
    super::super::insert_db_batch_once(&database, &writes)
        .await
        .unwrap();
    let upper_id = sqlx::query_scalar("SELECT MAX(id) FROM telemetry")
        .fetch_one(&database)
        .await
        .unwrap();
    let page = crate::recording_replay::test_page_from(
        &database,
        &crate::recording_replay::Window {
            upper_id,
            after_id: 0,
            start_ms: phase_wall - 1,
            end_ms: phase_wall + 1,
            types: "[\"FLIGHT_STATE\",\"ALTITUDE\"]".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(page.rows.len(), 2);
    let phase = page
        .rows
        .iter()
        .find(|row| row.phase.as_deref() == Some("Ascent"))
        .unwrap();
    assert_eq!(phase.timestamp_ms, phase_wall);
    assert_eq!(phase.source_timestamp_ms, Some(1_767_225_710_000));
    assert!(page.rows.iter().all(|row| row.timestamp_ms >= wall_before));
    state.mark_board_seen(
        Board::ValveBoard.sender_id(),
        super::super::get_current_timestamp_ms(),
    );
    // A slow/paused model does not turn real packet receipt into a stale
    // procedure board merely because more host wall time has passed.
    {
        let mut boards = state.board_status.lock().unwrap();
        let board = boards.get_mut(&Board::ValveBoard).unwrap();
        board.last_seen_instant =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(30));
        board.last_seen_ms = Some(super::super::get_current_timestamp_ms() - 30_000);
    }
    assert!(state.operational_board_recent(Board::ValveBoard, 3_000));
    std::fs::write(directory.join("clock"), "115501").unwrap();
    assert!(!state.operational_board_recent(Board::ValveBoard, 3_000));
    state.mark_board_seen(
        Board::ValveBoard.sender_id(),
        super::super::get_current_timestamp_ms(),
    );
    assert!(state.operational_board_recent(Board::ValveBoard, 3_000));

    crate::auto_zero::verify_paused_capture_cancellation_and_shutdown(&state).await;
    println!("MODEL_PURPOSE_CLOCK_REPLAY_PHASE_AND_NO_REPUBLISH_PASS");
}
