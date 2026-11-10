//! The station owns every managed variable it serves (network_variables:
//! FLIGHT_STATE, AV_BAY_UNDERGLOW, the DAQ calibrations ...) and answers an
//! umbilical board's request for one itself. Forwarding those broadcast
//! requests onto the AV-bay radio cost about 7.6 frames/s of uplink air in the
//! faithful sim (four umbilical boards), where nothing on the AV bay can serve
//! them: AV-bay boards are read-only replicas. A request for a variable the
//! station does not own still crosses. Actual SDK router and radio budget.
use super::*;
use sedsnet::router::{RetryTrackerLimits, RouterConfig, RouterSideOptions};

fn forwarded_to_radio(requested: sedsnet::config::DataType) -> usize {
    crate::telemetry_schema::initialize().unwrap();
    let router = Router::new(RouterConfig::new([]).with_sender("GS"));
    let budget = Arc::new(TxQueueBudget::new_radio(2048, 256));
    let activity = Arc::new(TransportTxActivity::default());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (side_budget, side_activity) = (budget.clone(), activity.clone());
    router
        .add_side_packed_leased_with_priority_and_options(
            "rocket_comms",
            move |lease, priority| side_budget.try_send_leased_tracked(&tx, priority, lease, &side_activity),
            RouterSideOptions { reliable_enabled: false, ..Default::default() }.with_small_packet_transport(192),
            RetryTrackerLimits::default(),
        )
        .unwrap();
    let umbilical = router.add_side_packed("umbilical_comms", |_| Ok(()));
    let request = sedsnet::discovery::build_managed_variable_request("GB", 1, requested).unwrap();
    router.rx_packed_from_side(&sedsnet::wire_format::pack_packet(&request), umbilical).unwrap();
    router.process_all_queues_with_timeout(0).unwrap();
    let mut forwarded = 0;
    while let Ok((_, bytes)) = rx.try_recv() {
        if is_managed_variable_request(&bytes) {
            forwarded += 1;
        }
    }
    forwarded
}

#[test]
fn umbilical_requests_for_station_variables_stay_off_the_radio() {
    for name in ["FLIGHT_STATE", "AV_BAY_UNDERGLOW", "DAQ_LOG_CLOCK"] {
        let ty = crate::telemetry_schema::data_type(name);
        assert_eq!(forwarded_to_radio(ty), 0, "{name}: the station answers it; the radio must not carry it");
    }
    // A variable the station does not serve is still requested across the radio.
    let other = crate::telemetry_schema::data_type("BAROMETER_DATA");
    assert_eq!(forwarded_to_radio(other), 1, "a request for a variable the station does not own must cross");
}
