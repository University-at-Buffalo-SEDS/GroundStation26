// An RRLS in takeover publishes the cosmetic outputs to the AV bay. For the
// discovery route lifetime (30 s) after the cut it still listed the
// out-of-range primary (a replica of the variable) as a destination, so a
// write stayed pending for the primary until its retries ran out and the
// first presses of every takeover window never confirmed (fullnet C-fix:
// frames owed to RF and GS; RF acknowledged each copy). Taking over now
// forgets the primary as a destination. Confirmation still needs a readback:
// for underglow the owner's receipt (it applies every well-formed write
// before acknowledging), for the buzzer only a real readback (the FC
// acknowledges a stale buzzer write without applying it).
use super::*;
use sedsnet::router::{EndpointHandler, RouterConfig};
use std::sync::atomic::{AtomicBool, AtomicU64};

const CHILD: &str = "GS_TAKEOVER_CONFIRM_TEST_CHILD";

fn rrls_child(name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        assert!(crate::rrls::is_rrls());
        return false;
    }
    let root = std::env::temp_dir().join(format!("gs-takeover-confirm-{}", rand::random::<u128>()));
    std::fs::create_dir_all(&root).unwrap();
    let case = format!("network_variables::takeover_confirm_tests::{name}");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &case, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("GS_RRLS_ROLE", "rrls")
        .env("GS_SIM_MODE", "0")
        .env_remove("GS_SIM_CLOCK_FILE")
        .env("GS_NETWORK_VARIABLE_CACHE", root.join("variables.json"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

/// An RRLS that discovered RF, FC and the primary, then lost the primary and
/// took over. AV-bay frames stay "on the air" until `deliver`.
struct Takeover {
    rrls: Arc<Router>,
    bay: Vec<Arc<Router>>,
    in_air: Arc<Mutex<Vec<Vec<u8>>>>,
}
impl Takeover {
    fn deliver(&self) {
        for bytes in self.in_air.lock().unwrap().drain(..) {
            for board in &self.bay {
                board.rx_packed_from_side(&bytes, 0).unwrap();
            }
        }
        for _ in 0..10 {
            for board in &self.bay {
                board.process_all_queues().unwrap();
            }
            self.rrls.process_all_queues().unwrap();
        }
    }
    fn reconcile(&self) {
        self.rrls.process_all_queues().unwrap();
        reconcile_network_outputs(&self.rrls, [0; 2], || Some(()));
    }
    fn owed(&self, data_type: &str) -> Vec<u64> {
        self.rrls.export_reliable_pending_stats().into_iter()
            .filter(|row| row.data_type == crate::telemetry_schema::data_type(data_type))
            .flat_map(|row| row.pending_destinations)
            .collect()
    }
}

fn hash(name: &str) -> u64 {
    sedsnet::packet::hash_bytes_u64(0x517C_C1B7_2722_0A95, name.as_bytes())
}

fn takeover() -> Takeover {
    crate::telemetry_schema::initialize().unwrap();
    let now = Arc::new(AtomicU64::new(1_000));
    let node = |name: &str, endpoint: Option<&str>| {
        let clock = now.clone();
        let handlers: Vec<EndpointHandler> = endpoint
            .map(|endpoint| EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint(endpoint), |_| Ok(())))
            .into_iter()
            .collect();
        let router = Arc::new(Router::new_with_clock(
            RouterConfig::new(handlers).with_sender(name),
            Box::new(move || clock.load(Ordering::Relaxed)),
        ));
        for ty in [UNDERGLOW_TYPE, FLIGHT_BUZZER_TYPE] {
            router.enable_network_variable(crate::telemetry_schema::data_type(ty), NetworkVariablePermissions::READ_ONLY).unwrap();
        }
        router
    };
    let rrls = Arc::new(Router::new_with_clock(
        RouterConfig::new([]).with_sender("RRLS"),
        Box::new({ let clock = now.clone(); move || clock.load(Ordering::Relaxed) }),
    ));
    initialize(&rrls).unwrap();
    let rf = node("RF", Some("AV_BAY_UNDERGLOW_OWNER"));
    let fc = node("FC", Some("FLIGHT_CONTROLLER"));
    let gs = node("GS", None);
    let in_air = Arc::new(Mutex::new(Vec::new()));
    let (holding, gs_up) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(true)));
    let (air, hold, gs_ok, bay, to_gs) = (in_air.clone(), holding.clone(), gs_up.clone(), vec![rf.clone(), fc.clone()], gs.clone());
    rrls.add_side_packed("rocket_comms", move |bytes| {
        if hold.load(Ordering::Relaxed) {
            air.lock().unwrap().push(bytes.to_vec());
        } else {
            for board in &bay {
                board.rx_packed_from_side(bytes, 0)?;
            }
        }
        if gs_ok.load(Ordering::Relaxed) {
            to_gs.rx_packed_from_side(bytes, 0)?;
        }
        Ok(())
    });
    for (peer, up) in [(&rf, None), (&fc, None), (&gs, Some(gs_up.clone()))] {
        let back = rrls.clone();
        peer.add_side_packed("RRLS", move |bytes| {
            if up.as_ref().is_none_or(|up| up.load(Ordering::Relaxed)) { back.rx_packed_from_side(bytes, 0) } else { Ok(()) }
        });
    }
    for router in [&rf, &fc, &gs, &rrls] {
        router.announce_discovery().unwrap();
    }
    for _ in 0..10 {
        for router in [&rrls, &rf, &fc, &gs] {
            router.process_all_queues().unwrap();
        }
    }
    // The cut: the primary's route outlives it (30 s), so it is still listed.
    gs_up.store(false, Ordering::Relaxed);
    holding.store(true, Ordering::Relaxed);
    crate::rrls::set_takeover_for_test(true);
    takeover_changed(&rrls, true).unwrap();
    std::mem::forget(gs);
    Takeover { rrls, bay: vec![rf, fc], in_air }
}

#[test]
fn a_takeover_press_confirms_once_rf_reads_it_back_while_the_primary_is_still_listed() {
    if rrls_child("a_takeover_press_confirms_once_rf_reads_it_back_while_the_primary_is_still_listed") {
        return;
    }
    let bay = takeover();
    assert!(toggle_underglow(&bay.rrls).unwrap());
    bay.reconcile();
    let mut owed = bay.owed(UNDERGLOW_TYPE);
    owed.sort_unstable();
    let mut boards = vec![hash("RF"), hash("FC")];
    boards.sort_unstable();
    assert_eq!(owed, boards, "owed to the AV bay only, not to the out-of-range primary");
    assert_eq!(network_output_view(NetworkOutput::Underglow).confirmed, None, "nothing read back yet");
    bay.deliver();
    bay.reconcile();
    assert_eq!(
        network_output_view(NetworkOutput::Underglow).confirmed,
        Some(true),
        "RF applied it (its receipt is its readback)"
    );
}

#[test]
fn a_buzzer_receipt_without_a_readback_stays_unconfirmed() {
    if rrls_child("a_buzzer_receipt_without_a_readback_stays_unconfirmed") {
        return;
    }
    let bay = takeover();
    assert!(toggle_flight_buzzer(&bay.rrls).unwrap());
    bay.reconcile();
    assert!(!bay.owed(FLIGHT_BUZZER_TYPE).contains(&hash("GS")));
    bay.deliver();
    bay.reconcile();
    assert!(bay.owed(FLIGHT_BUZZER_TYPE).is_empty(), "the FC acknowledged it");
    assert_eq!(
        network_output_view(NetworkOutput::FlightBuzzer).confirmed,
        None,
        "the FC acknowledges a stale buzzer write without applying it: not a readback"
    );
    // The FC's own report of the applied value is.
    let mut words = vec![0u32; 24];
    words[14] = (FC_OUTPUTS_TAG << 16) | 2 | 8;
    observe_fc_health_outputs(&words);
    assert_eq!(network_output_view(NetworkOutput::FlightBuzzer).confirmed, Some(true));
}

/// Handing back, the RRLS leaves the network as a station at once: the AV
/// bay stops addressing it (it no longer acknowledges anything) instead of
/// for the 30 s route lifetime.
#[test]
fn handing_back_sends_a_discovery_leave_the_av_bay_acts_on() {
    if rrls_child("handing_back_sends_a_discovery_leave_the_av_bay_acts_on") {
        return;
    }
    let bay = takeover();
    let rf = bay.bay[0].clone();
    let knows_rrls = |rf: &Router| rf.export_topology().routes.iter()
        .any(|route| route.announcers.iter().any(|a| a.sender_id == "RRLS"));
    assert!(knows_rrls(&rf), "the RRLS is a known station during the takeover");
    crate::rrls::set_takeover_for_test(false);
    bay.in_air.lock().unwrap().clear();
    takeover_changed(&bay.rrls, false).unwrap();
    bay.rrls.process_all_queues().unwrap();
    // What the hand-back grace lets on the air (rrls::Mesh::local_tx):
    // acknowledgements and the discovery leave, nothing else.
    let on_air: Vec<Vec<u8>> = bay.in_air.lock().unwrap().drain(..)
        .filter(|bytes| sedsnet::wire_format::peek_frame_info_unchecked(bytes).is_ok_and(|info| {
            matches!(info.envelope.ty, sedsnet::config::DataType::ReliableAck | sedsnet::config::DataType::DiscoveryLeave)
        }))
        .collect();
    assert!(!on_air.is_empty(), "the hand-back put a discovery leave on the air");
    for bytes in on_air {
        rf.rx_packed_from_side(&bytes, 0).unwrap();
    }
    rf.process_all_queues().unwrap();
    assert!(!knows_rrls(&rf), "the AV bay no longer routes to the RRLS");
}
