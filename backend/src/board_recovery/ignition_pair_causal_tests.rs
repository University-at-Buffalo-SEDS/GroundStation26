// Same actual-SDK desired control can run against the unmodified public parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_absent_owners_refuses_new_sdk_sequence() {
    crate::telemetry_schema::initialize().unwrap();
    let fixture = super::super::tests::http_fixture().await;
    let state = fixture.state.clone();
    let host = Arc::new(Router::new(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("GROUND_STATION"),
            |_| Ok(()),
        )])
        .with_sender("GS"),
    ));
    host.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
        crate::telemetry_task::get_current_timestamp_ms(),
    ))
    .unwrap();
    assert!(state.topology_router.set(host.clone()).is_ok());
    let kind = crate::telemetry_schema::data_type("ACTUATOR_COMMAND");
    let result = crate::gse::queue_controller_with_stop_fence(&state, &host, kind, &[14]);
    assert!(
        !matches!(result, Ok(true)),
        "no selected AB/VB owner or proof may admit an ignition sequence: {result:?}"
    );
    assert!(
        !host.has_pending_reliable_tx_type(kind),
        "no SDK ignition owner may exist"
    );
}
