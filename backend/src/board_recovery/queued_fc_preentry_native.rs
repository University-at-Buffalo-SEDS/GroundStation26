#[tokio::test]
async fn queued_fc_preentry_original_must_not_survive_actual_completed_reset() {
    use crate::types::FlightState;
    use crate::telemetry_db::{DbQueueItem,RecordingCommand,RecordingModeWire};
    let mut outcomes=Vec::new();
    for queued_before_reset in [true,false] {
        let fixture=Fixture::new_with_managed_peers().await;
        let state=fixture.state();
        state.set_local_flight_state(FlightState::Aborted);
        for board in BOARDS {fixture.query(board).await;}
        fixture._db.send(DbQueueItem::Control(RecordingCommand::StartNow)).await.unwrap();
        let path=tokio::time::timeout(Duration::from_secs(3),async {
            loop {let status=state.recording_status_snapshot();
                if status.mode==RecordingModeWire::Recording {break status.db_path.unwrap();}
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }).await.expect("actual recording owner must start before the packet/reset experiment");
        let router=state.topology_router.get().unwrap();
        let fc_side=router.export_topology().routes.iter().find(|row|
            row.announcers.iter().any(|peer|peer.sender_id=="FC")).unwrap().side_id;
        let before_generation=crate::gse::automatic_generation(&state);
        let original_utc=crate::system_clock::required_control_network_utc_ms(router).unwrap();
        let packet=Packet::new(crate::telemetry_schema::data_type("FLIGHT_STATE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION"),crate::telemetry_schema::endpoint("FLIGHT_STATE")],
            "FC",original_utc,Arc::from([FlightState::Ascent as u8])).unwrap();
        let packet_id=packet.packet_id();
        let hold=crate::telemetry_task::queued_phase_probe::install(&state,packet_id);
        if queued_before_reset {
            router.rx_packed_from_side(&sedsnet::wire_format::pack_packet(&packet),fc_side).unwrap();
            tokio::time::timeout(Duration::from_secs(3),hold.entered.notified()).await
                .expect("actual SDK callback/ring/receiver must reach paused real bulk worker");
            assert_eq!(hold.ingress_generation.load(Ordering::Acquire),before_generation);
            assert_eq!(hold.ingress_utc.load(Ordering::Acquire),original_utc);
        }
        let ticket=fixture.prepare("reset_startup",None).await;
        assert_eq!(fixture.confirm(&ticket).await.status(),StatusCode::OK);
        let terminal=fixture.terminal().await;
        assert_eq!(terminal["success"],true,"{terminal}");
        assert_eq!(terminal["reset_applied_state"],0);
        let after_reset=state.local_flight_state_snapshot();
        let after_reset_clock=state.launch_clock_snapshot();
        let after_generation=crate::gse::automatic_generation(&state);
        assert!(matches!(after_reset,FlightState::Startup|FlightState::Idle),"actual completed reset phase");
        assert!(after_generation>before_generation);
        if !queued_before_reset {
            // Positive control: the identical FC packet shape genuinely arrives
            // after Reset, with its own actual post-reset UTC/ID. No phase override.
            // This first packet was not injected; retire only its test observation.
            crate::telemetry_task::queued_phase_probe::withdraw_uninjected(&state,packet_id);
            let fresh_utc=crate::system_clock::required_control_network_utc_ms(router).unwrap();
            let fresh=Packet::new(crate::telemetry_schema::data_type("FLIGHT_STATE"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION"),crate::telemetry_schema::endpoint("FLIGHT_STATE")],
                "FC",fresh_utc,Arc::from([FlightState::Ascent as u8])).unwrap();
            let fresh_hold=crate::telemetry_task::queued_phase_probe::install(&state,fresh.packet_id());
            router.rx_packed_from_side(&sedsnet::wire_format::pack_packet(&fresh),fc_side).unwrap();
            tokio::time::timeout(Duration::from_secs(3),fresh_hold.entered.notified()).await.unwrap();
            fresh_hold.released.notify_one();
            tokio::time::timeout(Duration::from_secs(3),fresh_hold.processed.notified()).await.unwrap();
            assert_eq!(state.local_flight_state_snapshot(),FlightState::Ascent,"fresh authority must remain functional");
            assert_eq!(state.launch_clock_snapshot().kind,crate::telemetry_db::LaunchClockKind::TPlus);
            fixture._db.send(DbQueueItem::Control(RecordingCommand::Pause)).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3),async {
                while state.recording_status_snapshot().mode!=RecordingModeWire::Paused {tokio::time::sleep(Duration::from_millis(2)).await;}
            }).await.unwrap();
            let db=sqlx::SqlitePool::connect(&format!("sqlite://{path}?mode=ro")).await.unwrap();
            let fresh_rows:i64=sqlx::query_scalar("SELECT COUNT(*) FROM flight_state WHERE f_state=8 AND timestamp_ms=?")
                .bind(fresh_utc as i64).fetch_one(&db).await.unwrap();
            db.close().await;
            assert_eq!(fresh_rows,1,"fresh phase must still be recorded authoritatively");
            outcomes.push(serde_json::json!({"queued_before_reset":false,"packet_id":fresh.packet_id(),"source_utc":fresh_utc,
                "ingress_generation":fresh_hold.ingress_generation.load(Ordering::Acquire),"entry_generation":fresh_hold.entry_generation.load(Ordering::Acquire),
                "actual_phase":state.local_flight_state_snapshot() as u8,"fresh_positive":true,"recorded_fresh_phase_rows":fresh_rows,"carried_generation":fresh_hold.carried_generation.load(Ordering::Acquire)}));
        } else {
            hold.released.notify_one();
            tokio::time::timeout(Duration::from_secs(3),hold.processed.notified()).await.unwrap();
            fixture._db.send(DbQueueItem::Control(RecordingCommand::Pause)).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3),async {
                while state.recording_status_snapshot().mode!=RecordingModeWire::Paused {tokio::time::sleep(Duration::from_millis(2)).await;}
            }).await.unwrap();
            let db=sqlx::SqlitePool::connect(&format!("sqlite://{path}?mode=ro")).await.unwrap();
            let rows:i64=sqlx::query_scalar("SELECT COUNT(*) FROM flight_state WHERE f_state=8 AND timestamp_ms=?")
                .bind(original_utc as i64).fetch_one(&db).await.unwrap();
            let historical_rows:i64=sqlx::query_scalar("SELECT COUNT(*) FROM telemetry WHERE data_type='FLIGHT_STATE' AND sender_id='FC' AND source_timestamp_ms=?")
                .bind(original_utc as i64).fetch_one(&db).await.unwrap();
            db.close().await;
            outcomes.push(serde_json::json!({"queued_before_reset":true,"packet_id":packet_id,"source_utc":original_utc,
                "original_sdk_generation":hold.ingress_generation.load(Ordering::Acquire),"post_reset_generation":after_generation,
                "bulk_entry_generation":hold.entry_generation.load(Ordering::Acquire),"carried_generation":hold.carried_generation.load(Ordering::Acquire),"reset_phase":after_reset as u8,
                "actual_phase":state.local_flight_state_snapshot() as u8,"clock_before_release":after_reset_clock,
                "clock_after_release":state.launch_clock_snapshot(),"recorded_old_phase_rows":rows,"historical_raw_rows":historical_rows,"recording_path":path}));
        }
    }
    for row in &outcomes {eprintln!("QUEUED_FC_PREENTRY {}",row);}
    let old=&outcomes[0];
    assert_eq!(old["actual_phase"],old["reset_phase"],"pre-reset queued FC report must not overwrite actual completed Reset: {old}");
    assert_eq!(old["clock_before_release"],old["clock_after_release"],"queued historical report must not change T0");
    assert_eq!(old["recorded_old_phase_rows"],0,"queued historical report must not enter authoritative phase history");
    assert_eq!(old["historical_raw_rows"],1,"retired phase remains available as historical raw telemetry");
    assert_eq!(old["carried_generation"],old["original_sdk_generation"],"dequeue must not rebase original authority");
}
