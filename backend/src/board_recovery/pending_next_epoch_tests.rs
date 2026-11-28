// Actual correlated protocol regression: Pending with a fresh owner snapshot
// remains nonterminal, and never supplies permission to reopen admission.
#[test]
fn next_epoch_pending_is_not_terminal_and_keeps_original_deadline() {
    let (mut e, now) = setup(Board::ActuatorBoard);
    let original = e.packet(now).unwrap().clone();
    let pending = reply(&e, 1, 0x3e, 9);
    assert_eq!(e.observe(&pending, now), Ok(Outcome::Pending));
    assert!(e.terminal.is_none());
    let poll = e.poll_packet(now + Duration::from_millis(50)).unwrap();
    assert_eq!(poll.timestamp(), original.timestamp());
    assert_eq!(poll.payload(), original.payload());
    assert!(
        e.observe(&pending, now + QUERY_TTL + Duration::from_millis(1))
            .is_err()
    );
    assert!(e.terminal.is_none());
}
#[test]
fn next_epoch_pending_requires_exact_terminal_shape_but_never_applies() {
    for board in BOARDS {
        let (mut e, now) = setup(board);
        let flags = if matches!(
            board,
            Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer
        ) {
            0x3e
        } else {
            0x2c
        };
        let pending = reply(&e, 1, flags, 9);
        assert_eq!(e.observe(&pending, now), Ok(Outcome::Pending), "{board:?}");
        assert!(e.terminal.is_none());
        assert_eq!(
            e.observe(&reply(&e, 0, flags, 9), now),
            Ok(Outcome::Applied)
        );
    }
}
#[test]
fn next_epoch_pending_rejects_wrong_owner_state_safety_and_epoch() {
    for change in 0..12 {
        let (mut e, now) = setup(Board::ActuatorBoard);
        let good = reply(&e, 1, 0x3e, 9);
        let mut bytes = good.payload().to_vec();
        match change {
            0 => bytes[2] = 1,
            1 => bytes[3] |= ABORT,
            2 => bytes[3] &= !STATE_VALID,
            3 => bytes[3] &= !OUTPUTS_VALID,
            4 => bytes[3] &= !SAFE,
            5 => bytes[28] = 1,
            6 => bytes[20..24].copy_from_slice(&10u32.to_le_bytes()),
            7 => bytes[12] ^= 1,
            8 => bytes[4] ^= 1,
            9 => bytes[32] ^= 1,
            _ => (),
        }
        let sender = if change == 10 { "VB" } else { "AB" };
        let endpoints = if change == 11 {
            vec![crate::telemetry_schema::endpoint("BOARD_RECOVERY_AB")]
        } else {
            good.endpoints().to_vec()
        };
        let invalid = Packet::new(
            good.data_type(),
            &endpoints,
            sender,
            good.timestamp(),
            Arc::from(bytes),
        )
        .unwrap();
        assert!(e.observe(&invalid, now).is_err(), "change {change}");
        assert!(e.terminal.is_none());
    }
    let (mut e, now) = setup(Board::ActuatorBoard);
    e.epoch = u32::MAX;
    assert!(e.observe(&reply(&e, 1, 0x3e, 0), now).is_err());
}
#[test]
fn original_epoch_pending_behavior_and_manual_stop_shape_remain_closed() {
    let (mut e, now) = setup(Board::ActuatorBoard);
    assert_eq!(e.observe(&reply(&e, 1, 0x3f, 8), now), Ok(Outcome::Pending));
    assert!(e.observe(&reply(&e, 1, 0x3f, 9), now).is_err());
    assert!(e.terminal.is_none());
}
