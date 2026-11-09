//! Actual selected DummyComms/SDK discovery + endpoint application owner.
//! No HTTP/user-gate/MCU/NVM/hardware qualification is claimed here.
use super::*;
use crate::comms::{CommsDevice, DummyComms};
use sedsnet::router::{EndpointHandler, Router, RouterConfig};

fn pump(host: &Router, links: &mut [DummyComms], out: &Mutex<VecDeque<(usize, Vec<u8>)>>) {
    for _ in 0..100 {
        host.process_all_queues().unwrap();
        loop {
            let next = { out.lock().unwrap().pop_front() };
            let Some((side, wire)) = next else {
                break;
            };
            links[side].send_data(&wire).unwrap();
        }
        for link in &mut *links {
            link.recv_packet(host, &mut |_| {}).unwrap();
        }
    }
}
fn request(operation: u8, nonce: u64, boot: u64, epoch: u32, source: u64) -> Packet {
    let mut body = [0u8; 32];
    body[0] = 1;
    body[1] = operation;
    body[2] = if operation == 0 { 0 } else { 1 };
    body[4..12].copy_from_slice(&nonce.to_le_bytes());
    body[12..20].copy_from_slice(&boot.to_le_bytes());
    body[20..24].copy_from_slice(&epoch.to_le_bytes());
    body[24..32].copy_from_slice(&77u64.to_le_bytes());
    Packet::new(
        crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST"),
        &[crate::telemetry_schema::endpoint("BOARD_RECOVERY_AB")],
        "GS",
        source,
        Arc::from(body),
    )
    .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_selected_virtual_pair_query_raw14_offset_replay_and_delayed_recovery() {
    crate::mode_suite!(Testing);
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_selected_virtual_pair_query_raw14_offset_replay_and_delayed_recovery",
    ) {
        return;
    }
    assert!(sim_mode_enabled());
    crate::telemetry_schema::initialize().unwrap();
    let state = crate::state::tests::test_app_state().await;
    initialize_model_owners(&state, 0, 0);
    let captured = Arc::new(Mutex::new(Vec::<Packet>::new()));
    let capture = captured.clone();
    let host = Router::new(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("GROUND_STATION"),
            move |p| {
                capture.lock().unwrap().push(p.clone());
                Ok(())
            },
        )])
        .with_sender("GS"),
    );
    let wall = crate::telemetry_task::get_current_timestamp_ms();
    let network = wall - 7_200_000;
    host.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(network))
        .unwrap();
    let out = Arc::new(Mutex::new(VecDeque::<(usize, Vec<u8>)>::new()));
    let mut links = Vec::new();
    for (index, name) in ["Rocket Comms", "Umbilical Comms"].into_iter().enumerate() {
        let capture = out.clone();
        let side = host.add_side_packed_with_options(
            name,
            move |bytes| {
                capture.lock().unwrap().push_back((index, bytes.to_vec()));
                Ok(())
            },
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                ..Default::default()
            }
            .with_small_packet_transport(if index == 0 { 192 } else { 1024 }),
        );
        let mut link = DummyComms::new(name);
        assert!(link.testing_model_link());
        link.set_side_id(side);
        links.push(link);
    }
    crate::testing_board_lifecycle::bind_selected(
        links[0].testing_model_link(),
        links[1].testing_model_link(),
    );
    assert!(crate::testing_board_lifecycle::enabled());
    pump(&host, &mut links, &out);
    let query_source = host.network_time_ms().expect("explicit actual SDK network UTC required");
    host.tx_queue(request(0, 1, 0, 0, query_source)).unwrap();
    pump(&host, &mut links, &out);
    let query = captured
        .lock()
        .unwrap()
        .iter()
        .find(|p| {
            p.sender() == "AB"
                && p.data_type() == crate::telemetry_schema::data_type("BOARD_STATE_STATUS")
                && p.payload().len() == 40
                && u64::from_le_bytes(p.payload()[4..12].try_into().unwrap()) == 1
        })
        .cloned()
        .expect("actual nonce-correlated virtual AB response");
    assert_eq!(query.payload()[1], 0);
    assert_eq!(query.payload()[3] & 1, 0);
    let boot = u64::from_le_bytes(query.payload()[12..20].try_into().unwrap());
    let epoch = u32::from_le_bytes(query.payload()[20..24].try_into().unwrap());
    assert_ne!(boot, 0);
    let source = host.network_time_ms().expect("explicit actual SDK network UTC required");
    let raw = Packet::new(
        crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
        &[crate::telemetry_schema::endpoint("ACTUATOR_BOARD")],
        "GS",
        source,
        Arc::from([14u8]),
    )
    .unwrap();
    let id = raw.packet_id();
    let before = crate::telemetry_task::get_current_timestamp_ms();
    host.tx_queue(raw.clone()).unwrap();
    pump(&host, &mut links, &out);
    let after = crate::telemetry_task::get_current_timestamp_ms();
    let binding = {
        let model = sim().lock().unwrap();
        let owner = model
            .accepted_igniter_source
            .as_ref()
            .expect("actual SDKraw14 owner effect");
        assert_eq!(owner.source_utc, source);
        assert!(
            owner.start_model_ms >= before + 3500 && owner.start_model_ms <= after + 4251,
            "networkUTC was compared directly with host/modelclock"
        );
        assert_eq!(owner.stop_model_ms - owner.start_model_ms, 10000);
        (owner.source_utc, owner.start_model_ms, owner.stop_model_ms)
    };
    host.tx_queue(raw).unwrap();
    pump(&host, &mut links, &out);
    {
        let model = sim().lock().unwrap();
        let owner = model.accepted_igniter_source.as_ref().unwrap();
        assert_eq!(
            (owner.source_utc, owner.start_model_ms, owner.stop_model_ms),
            binding,
            "sameID retry moved retained original deadlines"
        );
    }
    // Same boot/epoch + fresh delayed preLaunch first-leg must NOT stop owner.
    host.tx_queue(request(1, 2, boot, epoch, source.saturating_sub(1)))
        .unwrap();
    pump(&host, &mut links, &out);
    let rejected = captured
        .lock()
        .unwrap()
        .iter()
        .find(|p| {
            p.sender() == "AB"
                && p.data_type() == crate::telemetry_schema::data_type("BOARD_STATE_STATUS")
                && p.payload().len() == 40
                && u64::from_le_bytes(p.payload()[4..12].try_into().unwrap()) == 2
        })
        .cloned()
        .expect("actual recovery refusal reply");
    assert_eq!(rejected.payload()[1], 4);
    assert_eq!(rejected.payload()[3] & 1, 0);
    assert_eq!(
        u32::from_le_bytes(rejected.payload()[20..24].try_into().unwrap()),
        epoch
    );
    let mut model = sim().lock().unwrap();
    assert!(model.accepted_igniter_source.is_some());
    model.update_actuator_igniter_sequence(binding.1);
    assert!(model.valve_on(7));
    model.update_actuator_igniter_sequence(binding.2);
    assert!(!model.valve_on(7));
    model.update_actuator_igniter_sequence(binding.1);
    assert!(!model.valve_on(7));
    eprintln!(
        "SIMULATED_ACTUAL_SDK_RAW14 packetID={id} originalUTC={source} modelDeadline={binding:?} queryBoot={boot} queryEpoch={epoch}; no physical transport"
    );
}
