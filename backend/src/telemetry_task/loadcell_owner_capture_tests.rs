// Actual SDK packed ingress -> backend handler -> long-capture Moments.
// No network device or physical output is attached.
use super::*;

#[tokio::test]
async fn original_source_owner_cannot_count_twice_after_sdk_duplicate_cache_turnover() {
    use sedsnet::router::{EndpointHandler, RouterConfig};
    let (db_tx, mut db_rx) = mpsc::channel(16);
    let state = test_app_state(db_tx.clone()).await;
    let config = state.loadcell_calibration.lock().unwrap().clone();
    let capture = state
        .loadcell_processing
        .start_calibrated("KG1000", 60, true, config)
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::<Packet>::new()));
    let delivered = seen.clone();
    let endpoint = crate::telemetry_schema::endpoint("GROUND_STATION");
    let router = Router::new(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            endpoint,
            move |packet| {
                delivered.lock().unwrap().push(packet.clone());
                Ok(())
            },
        )])
        .with_reliable_enabled(false)
        .with_sender("GS"),
    );
    let side = router.add_side_packed("capture-test", |_| Ok(()));
    let source_ms = get_current_timestamp_ms();
    let original = Packet::new(
        crate::telemetry_schema::data_type("KG1000"),
        &[endpoint],
        "DAQ",
        source_ms,
        f32_payload(&[0.2]),
    )
    .unwrap();
    let wire = serialize::pack_packet(&original);
    let overflow = test_db_overflow();
    let mut original_deliveries = 0;
    let mut original_raw_rows = 0;
    for stage in 0..3 {
        if stage == 2 {
            // Retain default SDK policy. Unrelated original owners turn over
            // its bounded cache; no altered cache capacity or forced bypass.
            for index in 0..sedsnet::config::MAX_RECENT_RX_IDS + 8 {
                let other = Packet::new(
                    crate::telemetry_schema::data_type("FUEL_FLOW"),
                    &[endpoint],
                    "DAQ",
                    source_ms + index as u64 + 1,
                    f32_payload(&[index as f32]),
                )
                .unwrap();
                router
                    .rx_packed_from_side(&serialize::pack_packet(&other), side)
                    .unwrap();
            }
            // Run every actual SDK callback through the backend handler too.
            let delivered_packets = std::mem::take(&mut *seen.lock().unwrap());
            for packet in delivered_packets {
                handle_packet(&state, &db_tx, &overflow, packet).await;
                while db_rx.try_recv().is_ok() {}
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        router.rx_packed_from_side(&wire, side).unwrap();
        let delivered_packets = std::mem::take(&mut *seen.lock().unwrap());
        for packet in delivered_packets {
            if packet.packet_id() == original.packet_id() {
                original_deliveries += 1;
                eprintln!(
                    "stage={stage} original_owner={} source_timestamp={} local_receipt={}",
                    packet.packet_id(),
                    packet.timestamp(),
                    get_current_timestamp_ms()
                );
            }
            let at = get_current_timestamp_ms() as i64;
            state.cache_recent_telemetry(TelemetryRow {
                timestamp_ms: at,
                data_type: "DAQ_ADC_TEMPERATURE".into(),
                sender_id: "DAQ".into(),
                values: vec![Some(20.0)],
            });
            let rows = handle_packet(&state, &db_tx, &overflow, packet).await;
            original_raw_rows += rows.iter().filter(|row| row.data_type == "KG1000").count();
            while db_rx.try_recv().is_ok() {}
        }
        if stage == 1 {
            assert_eq!(
                original_deliveries, 1,
                "immediate duplicate is suppressed by actual SDK"
            );
        }
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    state.loadcell_processing.stop(&capture.session_id).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.loadcell_processing.status().running {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let report = state.loadcell_processing.status();
    eprintln!(
        "SDK original deliveries={original_deliveries}, distinct original IDs=1, Moments accepted={}, rejected={}, dropped={}",
        report.report.samples, report.rejected_samples, report.dropped_samples
    );
    eprintln!(
        "Captured CSV:\n{}",
        std::fs::read_to_string(&report.csv_path).unwrap()
    );
    let _ = std::fs::remove_file(&report.csv_path);
    let _ = std::fs::remove_file(&report.report_path);
    assert_eq!(
        original_deliveries, 2,
        "same original really traversed actual SDK twice after cache turnover"
    );
    assert_eq!(
        original_raw_rows, 2,
        "capture rejection cannot erase raw backend telemetry rows"
    );
    assert_eq!(report.rejected_samples, 1);
    assert_eq!(
        report.report.samples, 1,
        "replayed original source owner must not become another independent long-capture observation"
    );
}
