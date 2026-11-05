use super::*;
fn prepare() {
    crate::telemetry_schema::initialize().unwrap();
}
// Keep independently varied on-wire fields explicit in adversarial codec/owner tests.
#[expect(
    clippy::too_many_arguments,
    reason = "independent request wire fields remain explicit in owner tests"
)]
fn request(
    board: Board,
    operation: u8,
    desired: u8,
    nonce: u64,
    boot: u64,
    epoch: u32,
    issuer: u64,
    time: u64,
) -> Packet {
    let mut body = [0; 32];
    body[0] = 1;
    body[1] = operation;
    body[2] = desired;
    body[4..12].copy_from_slice(&nonce.to_le_bytes());
    body[12..20].copy_from_slice(&boot.to_le_bytes());
    body[20..24].copy_from_slice(&epoch.to_le_bytes());
    body[24..32].copy_from_slice(&issuer.to_le_bytes());
    Packet::new(
        crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST"),
        &[crate::telemetry_schema::endpoint(endpoint(board))],
        "GS",
        time,
        Arc::from(body),
    )
    .unwrap()
}
fn query(owner: &mut Owner, b: Board, nonce: u64, time: u64) {
    let q = request(b, 0, 0, nonce, 0, 0, 77, time);
    let before = owner.nodes[at(b)].clone();
    let d = owner.receive(b, &q, time);
    assert!(d.reply.is_some());
    let after = &owner.nodes[at(b)];
    assert_eq!(before.epoch, after.epoch);
    assert_eq!(before.blocked, after.blocked);
    assert_eq!(before.floor, after.floor);
    assert_eq!(before.attempted, after.attempted);
}
fn complete(owner: &mut Owner, b: Board, boundary: u64) -> Intent {
    query(owner, b, 1, boundary + 1);
    let n = &owner.nodes[at(b)];
    let q = request(b, 1, 0, 2, n.boot, n.epoch, 77, boundary);
    assert_eq!(owner.receive(b, &q, boundary + 2).reply.unwrap().result, 1);
    let i = owner
        .ready_jobs()
        .into_iter()
        .find(|(x, _)| *x == b)
        .unwrap()
        .1;
    assert!(owner.ticket_current(b, &i, boundary + 2));
    assert_eq!(owner.finish(b, &i, boundary + 2).result, 0);
    i
}
#[test]
fn all_seven_pending_owned_complete_and_domains() {
    prepare();
    for board in BOARDS {
        let mut o = Owner::new();
        o.initialize(0, 0, 0);
        let epoch = o.nodes[at(board)].epoch;
        let i = complete(&mut o, board, 10005);
        let n = &o.nodes[at(board)];
        assert_eq!(n.epoch, epoch + 1);
        assert!(!n.blocked && !n.aborted);
        assert_eq!(n.floor, 10005);
        let p = o.status(
            board,
            Reply {
                result: 0,
                nonce: i.nonce,
                issuer: i.issuer,
            },
            if board == Board::ValveBoard { 6 } else { 0 },
            10006,
        );
        assert_eq!(p[3], if physical(board) { 0x7e } else { 0x2c });
        assert_eq!(p[2], 0);
    }
}
#[test]
fn unknown_hold_is_not_actual_abort_and_cannot_be_visual_idle() {
    prepare();
    let mut o = Owner::new();
    let ab = 1 << at(Board::ActuatorBoard);
    let vb = 1 << at(Board::ValveBoard);
    o.initialize(ab | vb, ab, 0);
    assert!(o.nodes[at(Board::ActuatorBoard)].aborted);
    assert!(!o.nodes[at(Board::ValveBoard)].aborted);
    assert_eq!(o.nodes[at(Board::ValveBoard)].state, None);
    o.note_state(Board::ActuatorBoard, 1);
    o.note_state(Board::ValveBoard, 1);
    assert_eq!(o.nodes[at(Board::ActuatorBoard)].state, Some(15));
    assert_eq!(o.nodes[at(Board::ValveBoard)].state, None);
    assert!(!o.source_allowed(Board::ValveBoard, 10001, 10001));
}
#[test]
fn original_tuple_duplicate_time_binding_and_fresh_stop() {
    prepare();
    let b = Board::ActuatorBoard;
    let mut o = Owner::new();
    o.initialize(0, 0, 0);
    let i = complete(&mut o, b, 10005);
    let p = request(b, 1, 0, i.nonce, i.boot, i.epoch, i.issuer, i.source);
    let epoch = o.nodes[at(b)].epoch;
    assert_eq!(o.receive(b, &p, 10007).reply.unwrap().result, 0);
    assert_eq!(o.nodes[at(b)].epoch, epoch);
    let changed = request(b, 1, 0, i.nonce, i.boot, i.epoch, i.issuer, i.source + 1);
    assert_eq!(o.receive(b, &changed, 10007).reply.unwrap().result, 6);
    o.manual_stop(b);
    assert_eq!(o.receive(b, &p, 10008).reply.unwrap().result, 4);
    assert!(o.blocked(b));
}
#[test]
fn stop_during_pending_requires_new_query_epoch_and_old_ticket_cannot_clobber() {
    prepare();
    let b = Board::ValveBoard;
    let mut o = Owner::new();
    o.initialize(0, 0, 0);
    query(&mut o, b, 1, 10000);
    let n = &o.nodes[at(b)];
    let old = request(b, 1, 0, 2, n.boot, n.epoch, 77, 10001);
    assert_eq!(o.receive(b, &old, 10001).reply.unwrap().result, 1);
    let first = o.ready_jobs()[0].1.clone();
    o.manual_stop(b);
    assert!(!o.ticket_current(b, &first, 10004));
    query(&mut o, b, 3, 10003);
    let n = &o.nodes[at(b)];
    let fresh = request(b, 1, 1, 4, n.boot, n.epoch, 77, 10004);
    assert_eq!(o.receive(b, &fresh, 10004).reply.unwrap().result, 1);
    let second = o.ready_jobs()[0].1.clone();
    assert_eq!(o.finish(b, &first, 10004).result, 4);
    assert!(o.ticket_current(b, &second, 10004));
    assert_eq!(o.finish(b, &second, 10004).result, 0);
    assert_eq!(o.nodes[at(b)].state, Some(1));
}
#[test]
fn fresh_query_fixed_boundary_and_stale_authority_negatives() {
    prepare();
    for board in BOARDS {
        for case in 0..6 {
            let mut o = Owner::new();
            o.initialize(0, 0, 100);
            query(&mut o, board, 1, 10010);
            let n = &o.nodes[at(board)];
            let (nonce, boot, epoch, time, now) = match case {
                0 => (2, n.boot, n.epoch, 10005, 10011),
                1 => (2, n.boot + 1, n.epoch, 10005, 10011),
                2 => (2, n.boot, n.epoch + 1, 10005, 10011),
                3 => (2, n.boot, n.epoch, 10005, 16000),
                4 => (1, n.boot, n.epoch, 10005, 10011),
                _ => (2, n.boot, n.epoch, 99, 10011),
            };
            let p = request(board, 1, 0, nonce, boot, epoch, 77, time);
            let result = o.receive(board, &p, now).reply.unwrap().result;
            assert_eq!(result == 1, case == 0, "{board:?}/{case}/{result}");
        }
    }
}
#[test]
fn manual_old_marked_only_strictly_before_completed_boundary() {
    prepare();
    let b = Board::FlightComputer;
    for (body, sender, time, retired) in [
        (b"SEDS_MANUAL_ABORT_V1:K".as_slice(), "GS", 10004, true),
        (b"SEDS_MANUAL_ABORT_V1:K".as_slice(), "GS", 10005, false),
        (b"SEDS_MANUAL_ABORT_V1:K".as_slice(), "GS", 0, false),
        (b"SEDS_MANUAL_ABORT_V1:".as_slice(), "GS", 10004, false),
    ] {
        let mut o = Owner::new();
        o.initialize(0, 0, 0);
        complete(&mut o, b, 10005);
        let packet = Packet::new(
            crate::telemetry_schema::data_type("ABORT"),
            &[crate::telemetry_schema::endpoint("ABORT")],
            sender,
            time,
            Arc::from(body),
        )
        .unwrap();
        let d = o.receive(b, &packet, 10006);
        assert_eq!(matches!(d.action, Action::None), retired);
        assert_eq!(o.blocked(b), !retired);
    }
}
#[test]
fn model_reboot_invalidates_proofs_and_retains_stop_floor() {
    prepare();
    let b = Board::ActuatorBoard;
    let mut o = Owner::new();
    o.initialize(0, 0, 0);
    complete(&mut o, b, 10005);
    let boot = o.nodes[at(b)].boot;
    o.manual_stop(b);
    o.reboot();
    let n = &o.nodes[at(b)];
    assert!(n.boot != 0 && n.boot != boot && n.boot != u64::MAX);
    assert!(n.blocked && n.aborted && !n.completed);
    assert_eq!(n.floor, 10005);
    assert!(n.query.is_none() && n.last.is_none());
}
#[test]
fn heartbeat_idle_exemption_does_not_weaken_generic_fault_or_manual_stop() {
    prepare();
    let b = Board::ValveBoard;
    for (reason, idle, launch, stop) in [
        ("heartbeat timeout", true, false, false),
        ("ADC start", true, false, true),
        ("heartbeat timeout", false, false, true),
        ("ADC start", false, true, false),
    ] {
        let mut o = Owner::new();
        o.initialize(0, 0, 0);
        o.note_state(b, if idle { 1 } else { 5 });
        if launch {
            o.note_launch(b);
        }
        let body = format!("SEDS_AUTO_ABORT_V1:{reason}");
        let p = Packet::new(
            crate::telemetry_schema::data_type("ABORT"),
            &[crate::telemetry_schema::endpoint("ABORT")],
            "AB",
            1000,
            Arc::from(body.into_bytes()),
        )
        .unwrap();
        assert_eq!(matches!(o.receive(b, &p, 1000).action, Action::Stop), stop);
    }
}

#[test]
fn pending_completion_rechecks_original_boundary_clock_without_restamping() {
    prepare();
    for now in [0, u64::MAX, 16001] {
        let mut o = Owner::new();
        o.initialize(0, 0, 0);
        let b = Board::FlightComputer;
        query(&mut o, b, 1, 10001);
        let n = &o.nodes[at(b)];
        let packet = request(b, 1, 0, 2, n.boot, n.epoch, 77, 10000);
        assert_eq!(o.receive(b, &packet, 10002).reply.unwrap().result, 1);
        let intent = o.ready_jobs()[0].1.clone();
        let epoch = o.nodes[at(b)].epoch;
        assert!(!o.ticket_current(b, &intent, now));
        assert_ne!(o.finish(b, &intent, now).result, 0);
        assert_eq!(o.nodes[at(b)].epoch, epoch);
        assert!(o.blocked(b));
        assert!(!o.nodes[at(b)].completed);
    }
}
#[test]
fn domain_manual_compatibility_and_registered_endpoint_shapes() {
    prepare();
    for b in [
        Board::ActuatorBoard,
        Board::ValveBoard,
        Board::FlightComputer,
        Board::DaqBoard,
    ] {
        for (sender, body) in [
            ("BAD", b"legacy".as_slice()),
            ("BAD", b"SEDS_MANUAL_ABORT_V1:K"),
            ("GS", b"SEDS_MANUAL_ABORT_V1:"),
        ] {
            let mut o = Owner::new();
            o.initialize(0, 0, 0);
            let packet = Packet::new(
                protocol().abort,
                &[crate::telemetry_schema::endpoints::ABORT],
                sender,
                0,
                Arc::from(body),
            )
            .unwrap();
            let expected = matches!(b, Board::ActuatorBoard | Board::ValveBoard)
                || (b == Board::FlightComputer && sender == "GS");
            assert_eq!(
                matches!(o.receive(b, &packet, 10000).action, Action::Stop),
                expected,
                "{b:?}/{sender}/{body:?}"
            );
        }
    }
    use crate::telemetry_schema::endpoints as ep;
    for endpoints in [
        vec![ep::FLIGHT_STATE],
        vec![ep::ABORT],
        vec![ep::ACTUATOR_BOARD, ep::VALVE_BOARD],
        vec![ep::ACTUATOR_BOARD, ep::ACTUATOR_BOARD],
    ] {
        let mut o = Owner::new();
        o.initialize(0, 0, 0);
        let p = Packet::new(protocol().actuator, &endpoints, "GS", 10000, Arc::from([1])).unwrap();
        assert!(matches!(
            o.receive(Board::ActuatorBoard, &p, 10000).action,
            Action::None
        ));
    }
}
#[test]
fn unknown_only_boot_is_honest_startup_while_retained_abort_stays_held() {
    prepare();
    let mut o = Owner::new();
    let ab = 1 << at(Board::ActuatorBoard);
    o.initialize_boot(ab, ab, 0x7f, 0);
    for b in BOARDS {
        let n = &o.nodes[at(b)];
        assert_ne!(n.boot, 0);
        assert_eq!(n.aborted, b == Board::ActuatorBoard);
        assert_eq!(n.blocked, b == Board::ActuatorBoard);
        assert_eq!(
            n.state,
            Some(if b == Board::ActuatorBoard { 15 } else { 0 })
        );
    }
}
#[test]
fn old_fc_state_provenance_never_credits_equal_new_or_manual_stop() {
    prepare();
    let b = Board::FlightComputer;
    let mut o = Owner::new();
    o.initialize(0, 0, 0);
    o.manual_stop(b);
    let origin = o.report_origin(b).unwrap();
    complete(&mut o, b, 10005);
    assert!(o.report_is_old(origin, 0));
    assert!(o.report_is_old(origin, 10004));
    assert!(!o.report_is_old(origin, 10005));
    assert!(!o.report_is_old(origin, 10006));
    assert!(!o.report_is_old(origin, u64::MAX));
    assert!(!o.report_is_old(o.report_origin(b).unwrap(), 0));
    o.manual_stop(b);
    assert!(o.blocked(b));
    assert!(o.report_is_old(origin, 0));
}

#[test]
fn tagged_automatic_domains_preserve_origin_and_launch_floor_guards() {
    prepare();
    for b in [
        Board::ActuatorBoard,
        Board::ValveBoard,
        Board::FlightComputer,
    ] {
        for case in 0..5 {
            let mut o = Owner::new();
            o.initialize(0, 0, 0);
            complete(&mut o, b, 10005);
            if case == 1 {
                o.note_launch(b);
            }
            if case == 2 {
                o.manual_stop(b);
            }
            let (ty, ep, value) = match b {
                Board::ActuatorBoard => (
                    protocol().actuator,
                    crate::telemetry_schema::endpoints::ACTUATOR_BOARD,
                    0x89,
                ),
                Board::ValveBoard => (
                    protocol().valve,
                    crate::telemetry_schema::endpoints::VALVE_BOARD,
                    0x80,
                ),
                _ => (
                    protocol().flight,
                    crate::telemetry_schema::endpoints::FLIGHT_CONTROLLER,
                    0x81,
                ),
            };
            let source = if case == 3 { 10004 } else { 10010 };
            let now = if case == 4 { 16000 } else { 10010 };
            let p = Packet::new(ty, &[ep], "GS", source, Arc::from([value])).unwrap();
            let d = o.receive(b, &p, now);
            if b == Board::FlightComputer {
                if let Action::Ordinary(Kind::Flight, value) = d.action {
                    assert_eq!(value, 0x81);
                }
                continue;
            }
            assert_eq!(
                matches!(
                    d.action,
                    Action::Ordinary(Kind::AutomaticActuator | Kind::AutomaticValve, _)
                ),
                case == 0,
                "{b:?}/{case}"
            );
        }
    }
}
#[test]
fn endpoint_allocation_provenance_clone_reparse_wrap_and_eviction() {
    if std::env::var_os("MODEL_REGISTRY_CHILD").is_none() {
        let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","testing_board_lifecycle::tests::endpoint_allocation_provenance_clone_reparse_wrap_and_eviction","--nocapture"]).env("MODEL_REGISTRY_CHILD","1").env("GS_SIM_MODE","1").env("GS_RRLS_ROLE","primary").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    prepare();
    bind_selected(true, true);
    let mut owner = Owner::new();
    owner.initialize(0, 0, 0);
    owner.manual_stop(Board::FlightComputer);
    let origin = owner.report_origin(Board::FlightComputer).unwrap();
    let packet = Packet::new(
        protocol().state,
        &[crate::telemetry_schema::endpoints::GROUND_STATION],
        "FC",
        0,
        Arc::from([15]),
    )
    .unwrap();
    register_model_report(&packet, origin);
    assert_eq!(registered_origin(&packet.clone()), Some(origin));
    let parsed =
        sedsnet::wire_format::unpack_packet(&sedsnet::wire_format::pack_packet(&packet)).unwrap();
    assert_eq!(parsed.packet_id(), packet.packet_id());
    assert_eq!(registered_origin(&parsed), None);
    // Explicit u16 nonce reuse models process-counter wrap without assuming entropy.
    let reused = Packet::new_with_nonce(
        packet.data_type(),
        packet.endpoints(),
        packet.sender(),
        packet.timestamp(),
        packet.nonce(),
        Arc::from(packet.payload()),
    )
    .unwrap();
    assert_eq!(reused.packet_id(), packet.packet_id());
    assert_eq!(registered_origin(&reused), None);
    let mut changed = origin;
    changed.epoch += 1;
    register_model_report(&reused, changed);
    assert_eq!(
        registered_origin(&packet),
        None,
        "ambiguous registration revokes original"
    );
    let fresh = Packet::new_with_nonce(
        protocol().state,
        &[crate::telemetry_schema::endpoints::GROUND_STATION],
        "FC",
        0,
        0,
        Arc::from([15]),
    )
    .unwrap();
    register_model_report(&fresh, origin);
    assert_eq!(registered_origin(&fresh), Some(origin));
    for nonce in 1..=300 {
        let next = Packet::new_with_nonce(
            protocol().state,
            &[crate::telemetry_schema::endpoints::GROUND_STATION],
            "FC",
            0,
            nonce,
            Arc::from([15]),
        )
        .unwrap();
        register_model_report(&next, origin);
    }
    assert_eq!(registered_origin(&fresh), None);
    assert!(report_registry().lock().unwrap().len() <= 256);
    bind_selected(false, false);
}

#[test]
fn observed_cause_clock_expiry_rollback_and_completed_floor_are_conservative() {
    prepare();
    let mut o = Owner::new();
    o.initialize(0, 0, 0);
    o.observe_clock(Some(10000));
    assert_eq!(o.original_event_stamp(), 10000);
    o.observed_clock.as_mut().unwrap().1 = Instant::now() - std::time::Duration::from_secs(1);
    assert_eq!(o.original_event_stamp(), 0);
    o.observe_clock(Some(9999));
    assert_eq!(o.original_event_stamp(), 0);
    o.observe_clock(Some(10001));
    assert_eq!(o.original_event_stamp(), 10001);
    complete(&mut o, Board::FlightComputer, 10005);
    assert_eq!(o.original_event_stamp(), 0);
    o.observe_clock(Some(10005));
    assert_eq!(o.original_event_stamp(), 10005);
    for utc in [None, Some(0), Some(u64::MAX)] {
        assert_eq!(ClockSample::new(utc).current(), 0);
    }
    let mut old = ClockSample::new(Some(10005));
    old.taken = Instant::now() - std::time::Duration::from_secs(1);
    assert_eq!(old.current(), 0);
}
#[test]
fn runtime_disabled_or_rrls_cannot_enable_dummy_model() {
    if let Ok(case) = std::env::var("MODEL_DISABLED_CHILD") {
        prepare();
        bind_selected(true, true);
        assert!(!eligible(), "{case}");
        assert!(!enabled());
        let dummy = crate::comms::DummyComms::new("Rocket Comms");
        assert!(!crate::comms::CommsDevice::testing_model_link(&dummy));
        assert!(!crate::flight_sim::handle_command(
            &crate::types::TelemetryCommand::Abort
        ));
        assert!(!crate::flight_sim::handle_command_stamped(
            &crate::types::TelemetryCommand::Launch,
            10000,
            10000
        ));
        return;
    }
    for (case, mode, role) in [("disabled", "0", "primary"), ("rrls", "1", "rrls")] {
        let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","testing_board_lifecycle::tests::runtime_disabled_or_rrls_cannot_enable_dummy_model","--nocapture"]).env("MODEL_DISABLED_CHILD",case).env("GS_SIM_MODE",mode).env("GS_RRLS_ROLE",role).output().unwrap();
        assert!(
            output.status.success(),
            "{case}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn postlaunch_recovery_clears_launch_only_on_verified_owned_success() {
    prepare();
    for board in [
        Board::ActuatorBoard,
        Board::ValveBoard,
        Board::FlightComputer,
    ] {
        for failure in [1, 2, 0] {
            let mut o = Owner::new();
            o.initialize(0, 0, 0);
            o.note_launch(board);
            o.manual_stop(board);
            query(&mut o, board, 1, 10001);
            let n = &o.nodes[at(board)];
            let p = request(board, 1, 0, 2, n.boot, n.epoch, 77, 10002);
            assert_eq!(o.receive(board, &p, 10002).reply.unwrap().result, 1);
            assert!(
                o.nodes[at(board)].launched,
                "Pending never clears launch latch"
            );
            let intent = o.ready_jobs()[0].1.clone();
            if failure == 2 {
                o.manual_stop(board);
            }
            let result = o
                .finish(board, &intent, if failure == 1 { 16003 } else { 10003 })
                .result;
            assert_eq!(result == 0, failure == 0);
            assert_eq!(
                o.nodes[at(board)].launched,
                failure != 0,
                "{board:?}/failure={failure}"
            );
        }
    }
}

#[test]
fn delayed_prelaunch_recovery_cannot_revoke_an_accepted_model_owner() {
    prepare();
    for board in [Board::ActuatorBoard,Board::ValveBoard,Board::FlightComputer] {
        let mut o=Owner::new();o.initialize(0,0,0);query(&mut o,board,1,10000);
        let before=&o.nodes[at(board)];let (boot,epoch,floor,attempted)=(before.boot,before.epoch,before.floor,before.attempted);
        let delayed=request(board,1,0,2,boot,epoch,77,10001);
        o.note_launch(board);
        let reply=o.receive(board,&delayed,10003);
        assert!(matches!(reply.action,Action::None));assert_eq!(reply.reply.unwrap().result,4);
        let n=&o.nodes[at(board)];assert_eq!((n.boot,n.epoch,n.floor,n.attempted),(boot,epoch,floor,attempted));assert!(!n.blocked&&!n.aborted);assert!(n.launched&&n.accepted_launch);assert!(n.last.is_none());
        // A real fresh ManualStop revokes the RAM owner independently; only
        // verified recovery can clear the historical launch latch afterward.
        o.manual_stop(board);assert!(!o.nodes[at(board)].accepted_launch);assert!(o.nodes[at(board)].launched);
        query(&mut o,board,3,10004);let n=&o.nodes[at(board)];let fresh=request(board,1,0,4,n.boot,n.epoch,77,10005);
        assert_eq!(o.receive(board,&fresh,10005).reply.unwrap().result,1);let intent=o.ready_jobs()[0].1.clone();assert_eq!(o.finish(board,&intent,10006).result,0);assert!(!o.nodes[at(board)].launched&&!o.nodes[at(board)].accepted_launch);
    }
}
