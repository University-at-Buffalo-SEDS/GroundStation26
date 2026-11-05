//! Candidate coordinator RPC primitive. It is not an authorization token.
//! The coordinator must independently retain host/proxy/Stop/journal fences.
use super::*;

const HARDENED: u8 = 0x20;
const SUPPORTED: u8 = 4;
const ABORT: u8 = 1;
const SAFE: u8 = 2;

/// One immutable source-time boundary captured at CONFIRM admission. QUERY
/// may occur later; neither per-board preparation nor polls may restamp this.
#[derive(Clone, Copy)]
pub(super) struct ConfirmBoundary {
    utc: u64,
    captured: Stamp,
    clock_failure: crate::final_proof_clock::FailureFence,
}
impl ConfirmBoundary {
    pub(super) fn capture(router: &sedsnet::router::Router) -> Result<Self, &'static str> {
        let clock_failure = crate::final_proof_clock::FailureFence::capture();
        Ok(Self {
            clock_failure,
            utc: crate::system_clock::required_control_network_utc_ms(router)
                .map_err(|_| "Network UTC unavailable")?,
            captured: Stamp::now(),
        })
    }
    pub(super) fn utc(self) -> u64 {
        self.utc
    }
    pub(super) fn clock_failure(self) -> crate::final_proof_clock::FailureFence { self.clock_failure }
    pub(super) fn monotonic_current(self, now: Stamp) -> bool {
        self.clock_failure.current().is_ok() && now.checked_duration_since(self.captured).is_some_and(|age| age <= QUERY_TTL)
    }
    pub(super) fn current(self, router: &sedsnet::router::Router, now: Stamp) -> bool {
        self.monotonic_current(now)
            && crate::system_clock::required_control_network_utc_ms(router)
                .ok()
                .and_then(|utc| utc.checked_sub(self.utc))
                .is_some_and(|age| age <= QUERY_TTL.as_millis() as u64)
            && self.clock_failure.current().is_ok()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    Pending,
    Applied,
    Rejected(u8),
}

pub(super) struct Exchange {
    board: Board,
    router: Arc<sedsnet::router::Router>,
    pub(super) nonce: u64,
    issuer: u64,
    boot: u64,
    epoch: u32,
    desired: u8,
    started: Stamp,
    boundary: ConfirmBoundary,
    packet: Packet,
    poll_ids: Vec<u64>,
    terminal: Option<Outcome>,
}

impl Exchange {
    /// A fresh application QUERY, not discovery or SDK ACK, supplies the
    /// expected board identity. IO is inspectable for explicit commissioning;
    /// it never supplies an Applied result here.
    #[cfg(test)]
    fn prepare(
        board: Board,
        query: &Observation,
        nonce: u64,
        desired: u8,
        now: Stamp,
        router: Arc<sedsnet::router::Router>,
    ) -> Result<Self, &'static str> {
        // Legacy primitive controls supply a deterministic monotonic snapshot.
        // Production routes instead use capture() at actual CONFIRM admission.
        let boundary = ConfirmBoundary {
            clock_failure: crate::final_proof_clock::FailureFence::capture(),
            utc: crate::system_clock::required_control_network_utc_ms(&router)
                .map_err(|_| "Network UTC unavailable")?,
            captured: now,
        };
        Self::prepare_at(board, query, nonce, desired, now, router, boundary)
    }
    pub(super) fn prepare_at(
        board: Board,
        query: &Observation,
        nonce: u64,
        desired: u8,
        now: Stamp,
        router: Arc<sedsnet::router::Router>,
        boundary: ConfirmBoundary,
    ) -> Result<Self, &'static str> {
        Self::prepare_bound(board, query, nonce, desired, now, router, boundary, None)
    }
    #[expect(clippy::too_many_arguments, reason = "Independent owner query, original boundary and retained FIRST proof remain explicit")]
    pub(super) fn prepare_first_reconciliation_at(
        board: Board,
        query: &Observation,
        nonce: u64,
        desired: u8,
        now: Stamp,
        router: Arc<sedsnet::router::Router>,
        boundary: ConfirmBoundary,
        binding: &super::coordinator::FirstBinding,
    ) -> Result<Self, &'static str> {
        if !binding.authorizes(query, boundary) {
            return Err("Retained FIRST binding is unavailable or stale");
        }
        Self::prepare_bound(
            board,
            query,
            nonce,
            desired,
            now,
            router,
            boundary,
            Some(binding),
        )
    }
    #[expect(clippy::too_many_arguments, reason = "Independent owner query, original boundary and retained FIRST proof remain explicit")]
    fn prepare_bound(
        board: Board,
        query: &Observation,
        nonce: u64,
        desired: u8,
        now: Stamp,
        router: Arc<sedsnet::router::Router>,
        boundary: ConfirmBoundary,
        first: Option<&super::coordinator::FirstBinding>,
    ) -> Result<Self, &'static str> {
        let q = &query.value;
        let desired_allowed = desired == 0
            || (desired == 1 && matches!(board, Board::ActuatorBoard | Board::ValveBoard));
        if index(board).is_none()
            || !desired_allowed
            || !query_domain_consistent(board, q)
            || !query.fresh(now)
            || !matches!(q.result, 0 | 7)
            || q.flags & (SUPPORTED | HARDENED) != SUPPORTED | HARDENED
            || q.issuer == 0
            || q.boot == 0 && first.is_none()
            || q.epoch == 0
            || q.epoch >= u32::MAX - 1
            || nonce == 0
            || nonce == q.nonce
            || router.sender().as_ref() != "GS"
            || !boundary.current(&router, now)
        {
            return Err("Recovery context unavailable or stale");
        }
        let expected_boot = if first.is_some() { u64::MAX } else { q.boot };
        let utc = boundary.utc;
        let mut wire = [0u8; 32];
        wire[0] = 1;
        wire[1] = 1;
        wire[2] = desired;
        wire[4..12].copy_from_slice(&nonce.to_le_bytes());
        wire[12..20].copy_from_slice(&expected_boot.to_le_bytes());
        wire[20..24].copy_from_slice(&q.epoch.to_le_bytes());
        wire[24..32].copy_from_slice(&q.issuer.to_le_bytes());
        let packet = Packet::new(
            crate::telemetry_schema::data_type(QUERY_TYPE),
            &[crate::telemetry_schema::endpoint(endpoint(board))],
            &router.sender(),
            utc,
            Arc::from(wire),
        )
        .map_err(|_| "Recovery packet preparation failed")?;
        Ok(Self {
            board,
            router,
            nonce,
            issuer: q.issuer,
            boot: expected_boot,
            epoch: q.epoch,
            desired,
            started: boundary.captured,
            boundary,
            packet,
            poll_ids: Vec::new(),
            terminal: None,
        })
    }

    /// SDK retransmission retains this packet. Application polling is distinct:
    /// the receiver ACKs duplicate SDK IDs without invoking its handler again.
    pub(super) fn packet(&self, now: Stamp) -> Result<&Packet, &'static str> {
        if !self.boundary.current(&self.router, now)
            || self.terminal.is_some()
            || now
                .checked_duration_since(self.started)
                .is_none_or(|age| age > QUERY_TTL)
        {
            return Err("Recovery request expired or completed");
        }
        Ok(&self.packet)
    }

    /// A bounded application poll has a new SDK envelope, the original
    /// application nonce/body and the ORIGINAL canonical command timestamp.
    /// No new authorization or freshness is created by polling. Admission and
    /// rate control belong to the coordinator, which must still own its fences.
    pub(super) fn poll_packet(&mut self, now: Stamp) -> Result<Packet, &'static str> {
        self.packet(now)?;
        if self.poll_ids.len() >= 16 {
            return Err("Recovery application poll budget exhausted");
        }
        let packet = Packet::new(
            self.packet.data_type(),
            self.packet.endpoints(),
            self.packet.sender(),
            self.packet.timestamp(),
            Arc::from(self.packet.payload()),
        )
        .map_err(|_| "Recovery poll packet preparation failed")?;
        let id = packet.packet_id();
        if id == self.packet.packet_id() || self.poll_ids.contains(&id) {
            return Err("Recovery SDK envelope identity repeated");
        }
        self.poll_ids.push(id);
        Ok(packet)
    }

    pub(super) fn observe(&mut self, packet: &Packet, now: Stamp) -> Result<Outcome, &'static str> {
        self.packet(now)?;
        let source = Board::from_sender_id(crate::types::canonical_sender_id(packet.sender()))
            .or_else(|| {
                let address = packet
                    .sender()
                    .strip_prefix("@addr:")?
                    .parse::<u32>()
                    .ok()?;
                let peer = self.router.resolve_address(address)?;
                Board::from_sender_id(crate::types::canonical_sender_id(peer.hostname.as_ref()))
            });
        if source != Some(self.board)
            || packet.data_type() != crate::telemetry_schema::data_type(STATUS_TYPE)
            || packet.endpoints() != [crate::telemetry_schema::endpoint("GROUND_STATION")]
        {
            return Err("Wrong recovery status source/type/destination");
        }
        let row = Readback::decode(packet.payload()).ok_or("Malformed recovery status")?;
        if row.nonce != self.nonce || row.issuer != self.issuer {
            return Err("Recovery reply belongs to another intent");
        }
        // Commissioning can provision generation 1 from the explicit MAX
        // sentinel. Only IO is permitted to report this first-leg transition.
        let first_identity = self.boot == u64::MAX
            && row.result == 7
            && (row.boot == 1
                || row.boot == 0 && row.flags & 0x25 == 0x25 && row.epoch == self.epoch);
        if row.boot != self.boot && !first_identity {
            return Err("Board rebooted or identity changed");
        }
        if row.flags & (SUPPORTED | HARDENED) != SUPPORTED | HARDENED {
            return Err("Hardened board capability missing");
        }
        let outcome = match row.result {
            1 if row.epoch == self.epoch => Outcome::Pending,
            0 => {
                if self.boot == u64::MAX
                    || row.epoch != self.epoch + 1
                    || row.state != Some(self.desired)
                    || row.flags & ABORT != 0
                {
                    return Err("Actual recovery state/epoch not confirmed");
                }
                if matches!(
                    self.board,
                    Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer
                ) {
                    if row.flags & (STATE_VALID | OUTPUTS_VALID | SAFE)
                        != STATE_VALID | OUTPUTS_VALID | SAFE
                        || safe_output_mask(self.board) != Some(row.outputs)
                    {
                        return Err("Owned safe-output readback missing");
                    }
                } else if row.flags & STATE_VALID == 0 || row.flags & (OUTPUTS_VALID | SAFE) != 0 {
                    return Err("Metadata node supplied unsupported physical proof");
                }
                Outcome::Applied
            }
            1 => return Err("Stop epoch changed while recovery was pending"),
            result => Outcome::Rejected(result),
        };
        if outcome != Outcome::Pending {
            self.terminal = Some(outcome.clone());
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sedsnet::router::{Router, RouterConfig};

    fn setup(board: Board) -> (Exchange, Stamp) {
        let now = Stamp::now();
        let router = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
        router
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                1_793_000_000_000,
            ))
            .unwrap();
        let query = Observation {
            value: Readback {
                result: 0,
                state: Some(15),
                flags: 0x2d,
                nonce: 1,
                boot: 42,
                epoch: 8,
                uptime: 10,
                outputs: 0,
                issuer: 99,
            },
            received: now,
            observed_after: now,
        };
        (
            Exchange::prepare(board, &query, 2, 0, now, router).unwrap(),
            now,
        )
    }
    fn reply(e: &Exchange, result: u8, flags: u8, epoch: u32) -> Packet {
        let mut bytes = [0; 40];
        bytes[0] = 1;
        bytes[1] = result;
        bytes[2] = 0;
        bytes[3] = flags;
        bytes[4..12].copy_from_slice(&e.nonce.to_le_bytes());
        bytes[12..20].copy_from_slice(&e.boot.to_le_bytes());
        bytes[20..24].copy_from_slice(&epoch.to_le_bytes());
        bytes[28..32].copy_from_slice(&safe_output_mask(e.board).unwrap_or(0).to_le_bytes());
        bytes[32..40].copy_from_slice(&e.issuer.to_le_bytes());
        Packet::new(
            crate::telemetry_schema::data_type(STATUS_TYPE),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            e.board.sender_id(),
            1_793_000_000_001,
            Arc::from(bytes),
        )
        .unwrap()
    }

    #[test]
    fn actual_packet_preserves_original_body_identity_and_utc_for_sdk_retransmission() {
        let (exchange, now) = setup(Board::ActuatorBoard);
        let first = exchange.packet(now).unwrap();
        let later = exchange.packet(now + Duration::from_millis(300)).unwrap();
        assert_eq!(first.packet_id(), later.packet_id());
        assert_eq!(first.timestamp(), later.timestamp());
        assert_eq!(first.payload(), later.payload());
        assert_eq!(&first.payload()[..4], &[1, 1, 0, 0]);
        assert_eq!(first.payload().len(), 32);
        assert_eq!(
            first.endpoints(),
            [crate::telemetry_schema::endpoint("BOARD_RECOVERY_AB")]
        );
    }

    #[test]
    fn pending_is_not_confirmation_and_does_not_slide_deadline() {
        let (mut exchange, now) = setup(Board::ActuatorBoard);
        let pending = reply(&exchange, 1, 0x3f, 8);
        assert_eq!(
            exchange.observe(&pending, now + Duration::from_secs(4)),
            Ok(Outcome::Pending)
        );
        assert!(exchange.packet(now + Duration::from_millis(5001)).is_err());
        let complete = reply(&exchange, 0, 0x3e, 9);
        assert!(
            exchange
                .observe(&complete, now + Duration::from_millis(5001))
                .is_err()
        );
    }

    #[test]
    fn all_seven_require_actual_terminal_proof_with_separate_physical_semantics() {
        for board in BOARDS {
            let (mut exchange, now) = setup(board);
            let physical = matches!(
                board,
                Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer
            );
            let flags = if physical { 0x3e } else { 0x2c };
            let complete = reply(&exchange, 0, flags, 9);
            assert_eq!(exchange.observe(&complete, now), Ok(Outcome::Applied));
            assert!(exchange.packet(now).is_err());
            assert!(exchange.observe(&complete, now).is_err());
        }
    }

    #[test]
    fn false_completion_state_epoch_latch_and_output_flags_are_rejected() {
        for (flags, epoch) in [(0x3f, 9), (0x3e, 8), (0x3c, 9), (0x2c, 9)] {
            let (mut e, now) = setup(Board::ActuatorBoard);
            let packet = reply(&e, 0, flags, epoch);
            assert!(e.observe(&packet, now).is_err());
            assert!(e.terminal.is_none());
        }
        let (mut e, now) = setup(Board::DaqBoard);
        let packet = reply(&e, 0, 0x3e, 9);
        assert!(e.observe(&packet, now).is_err());
        let (mut e, now) = setup(Board::ActuatorBoard);
        let valid = reply(&e, 0, 0x3e, 9);
        let mut wire = valid.payload().to_vec();
        wire[2] = 1;
        let wrong_state = Packet::new(
            valid.data_type(),
            valid.endpoints(),
            "AB",
            valid.timestamp(),
            Arc::from(wire),
        )
        .unwrap();
        assert!(e.observe(&wrong_state, now).is_err());
    }

    #[test]
    fn targeted_startup_unknown_terminal_and_missing_physical_proof_never_apply() {
        for board in [Board::FlightComputer, Board::RFBoard, Board::PowerBoard, Board::GatewayBoard, Board::DaqBoard] {
            let (mut exchange, now) = setup(board);
            let flags = if board == Board::FlightComputer {0x3e} else {0x2c};
            let valid = reply(&exchange, 0, flags, 9);
            let mut wire = valid.payload().to_vec();
            wire[2] = 255;
            wire[3] &= !STATE_VALID;
            let unknown = Packet::new(valid.data_type(), valid.endpoints(), valid.sender(), valid.timestamp(), Arc::from(wire)).unwrap();
            assert!(exchange.observe(&unknown, now).is_err());
            assert!(exchange.terminal.is_none());
            let unsupported = reply(&exchange, 0, flags & !HARDENED, 9);
            assert!(exchange.observe(&unsupported, now).is_err());
            assert!(exchange.terminal.is_none());
            assert_eq!(exchange.observe(&valid, now), Ok(Outcome::Applied));
        }
        let (mut fc, now) = setup(Board::FlightComputer);
        let no_outputs = reply(&fc, 0, 0x2c, 9);
        assert!(fc.observe(&no_outputs, now).is_err());
        assert!(fc.terminal.is_none());
    }

    #[test]
    fn io_is_terminal_failure_never_applied_and_requires_explicit_new_intent() {
        let (mut e, now) = setup(Board::ActuatorBoard);
        let packet = reply(&e, 7, 0x3f, 8);
        assert_eq!(e.observe(&packet, now), Ok(Outcome::Rejected(7)));
        assert!(e.packet(now).is_err());
        let packet = reply(&e, 0, 0x3e, 9);
        assert!(e.observe(&packet, now).is_err());
    }

    #[test]
    fn wrong_sender_destination_nonce_issuer_and_boot_never_consume_intent() {
        for change in 0..5 {
            let (mut e, now) = setup(Board::ActuatorBoard);
            let valid = reply(&e, 0, 0x3e, 9);
            let mut payload = valid.payload().to_vec();
            match change {
                2 => payload[4] ^= 1,
                3 => payload[32] ^= 1,
                4 => payload[12] ^= 1,
                _ => (),
            }
            let sender = if change == 0 { "VB" } else { "AB" };
            let destination = if change == 1 {
                "BOARD_RECOVERY_AB"
            } else {
                "GROUND_STATION"
            };
            let invalid = Packet::new(
                valid.data_type(),
                &[crate::telemetry_schema::endpoint(destination)],
                sender,
                valid.timestamp(),
                Arc::from(payload),
            )
            .unwrap();
            assert!(e.observe(&invalid, now).is_err());
            assert!(e.terminal.is_none());
            assert_eq!(e.observe(&valid, now), Ok(Outcome::Applied));
        }
    }

    #[test]
    fn commissioning_identity_provision_is_io_and_never_a_successful_reset() {
        let (mut e, now) = setup(Board::ActuatorBoard);
        e.boot = u64::MAX;
        let mut wire = reply(&e, 7, 0x3f, 8).payload().to_vec();
        wire[12..20].copy_from_slice(&1u64.to_le_bytes());
        let packet = Packet::new(
            crate::telemetry_schema::data_type(STATUS_TYPE),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "AB",
            1_793_000_000_001,
            Arc::from(wire),
        )
        .unwrap();
        assert_eq!(e.observe(&packet, now), Ok(Outcome::Rejected(7)));
        assert!(e.packet(now).is_err());
    }

    #[test]
    fn compact_sender_requires_actual_discovered_identity() {
        let (mut e, now) = setup(Board::ActuatorBoard);
        let valid = reply(&e, 0, 0x3e, 9);
        let side = e.router.add_side_packed("identity", |_| Ok(()));
        e.router
            .rx_from_side(
                &sedsnet::discovery::build_discovery_announce(
                    "AB",
                    1,
                    &[crate::telemetry_schema::endpoint("BOARD_RECOVERY_AB")],
                )
                .unwrap(),
                side,
            )
            .unwrap();
        let address = e.router.resolve_hostname("AB").unwrap().address;
        let packet = Packet::new(
            valid.data_type(),
            valid.endpoints(),
            &format!("@addr:{address}"),
            valid.timestamp(),
            Arc::from(valid.payload()),
        )
        .unwrap();
        assert_eq!(e.observe(&packet, now), Ok(Outcome::Applied));
        let (mut e, now) = setup(Board::ActuatorBoard);
        assert!(
            e.observe(&packet, now).is_err(),
            "Unresolved address is not a board identity"
        );
    }

    #[test]
    fn fresh_query_and_network_utc_are_both_required_before_packet_creation() {
        let now = Stamp::now();
        let router = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
        let mut query = Observation {
            value: Readback {
                result: 0,
                state: Some(15),
                flags: 0x2d,
                nonce: 1,
                boot: 42,
                epoch: 8,
                uptime: 0,
                outputs: 0,
                issuer: 99,
            },
            received: now,
            observed_after: now,
        };
        assert!(
            Exchange::prepare(Board::ActuatorBoard, &query, 2, 0, now, router.clone()).is_err()
        );
        router
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                1_793_000_000_000,
            ))
            .unwrap();
        assert!(
            Exchange::prepare(
                Board::ActuatorBoard,
                &query,
                2,
                0,
                now + Duration::from_millis(2001),
                router.clone()
            )
            .is_err()
        );
        for invalid in [0, u32::MAX - 1, u32::MAX] {
            query.value.epoch = invalid;
            assert!(
                Exchange::prepare(Board::ActuatorBoard, &query, 2, 0, now, router.clone()).is_err()
            );
        }
        query.value.epoch = 8;
        assert!(
            Exchange::prepare(Board::FlightComputer, &query, 2, 1, now, router.clone()).is_err()
        );
        assert!(
            Exchange::prepare(Board::ActuatorBoard, &query, 1, 0, now, router.clone()).is_err()
        );
        assert!(Exchange::prepare(Board::ActuatorBoard, &query, 2, 0, now, router).is_ok());
    }

    #[test]
    fn progress_can_complete_once_without_refreshing_request_or_accepting_clock_reversal() {
        let (mut e, now) = setup(Board::ActuatorBoard);
        let pending = reply(&e, 1, 0x3f, 8);
        let id = e.packet.packet_id();
        let utc = e.packet.timestamp();
        assert_eq!(
            e.observe(&pending, now + Duration::from_millis(10)),
            Ok(Outcome::Pending)
        );
        assert_eq!(
            e.observe(&pending, now + Duration::from_millis(20)),
            Ok(Outcome::Pending)
        );
        assert_eq!(e.packet.packet_id(), id);
        assert_eq!(e.packet.timestamp(), utc);
        let valid = reply(&e, 0, 0x3e, 9);
        assert!(e.observe(&valid, now - Duration::from_millis(1)).is_err());
        let mut different_clock = now;
        different_clock.model_ms = Some(0);
        assert!(e.observe(&valid, different_clock).is_err());
        assert_eq!(
            e.observe(&valid, now + Duration::from_millis(21)),
            Ok(Outcome::Applied)
        );
        assert!(
            e.observe(&pending, now + Duration::from_millis(22))
                .is_err()
        );
    }

    #[test]
    fn application_polls_change_only_sdk_envelope_and_have_finite_budget() {
        let (mut e, now) = setup(Board::ActuatorBoard);
        for i in 0..16 {
            let packet = e.poll_packet(now + Duration::from_millis(i)).unwrap();
            assert_ne!(packet.packet_id(), e.packet.packet_id());
            assert_eq!(packet.timestamp(), e.packet.timestamp());
            assert_eq!(packet.payload(), e.packet.payload());
            assert_eq!(packet.sender(), e.packet.sender());
            assert_eq!(packet.endpoints(), e.packet.endpoints());
        }
        assert!(e.poll_packet(now).is_err());
        let (mut e, now) = setup(Board::ActuatorBoard);
        assert!(e.poll_packet(now + Duration::from_millis(5001)).is_err());
    }

    #[test]
    fn actual_sdk_exchange_requires_application_reply_after_delivery_ack() {
        use sedsnet::router::{EndpointHandler, RouterSideOptions};
        let (mut exchange, now) = setup(Board::ActuatorBoard);
        let statuses = Arc::new(Mutex::new(Vec::<Packet>::new()));
        let capture = statuses.clone();
        let host = Arc::new(Router::new(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                move |packet| {
                    capture.lock().unwrap().push(packet.clone());
                    Ok(())
                },
            )])
            .with_sender("GS"),
        ));
        host.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
            exchange.packet.timestamp(),
        ))
        .unwrap();
        exchange.router = host.clone();
        let requests = Arc::new(Mutex::new(Vec::<Packet>::new()));
        let capture = requests.clone();
        let dedicated = crate::telemetry_schema::endpoint("BOARD_RECOVERY_AB");
        let board = Router::new(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                dedicated,
                move |packet| {
                    capture.lock().unwrap().push(packet.clone());
                    Ok(())
                },
            )])
            .with_sender("AB"),
        );
        let outbound = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let capture = outbound.clone();
        let hs = host.add_side_packed_with_options(
            "wire",
            move |bytes| {
                capture.lock().unwrap().push(bytes.to_vec());
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                ..Default::default()
            },
        );
        let inbound = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let capture = inbound.clone();
        let bs = board.add_side_packed_with_options(
            "wire",
            move |bytes| {
                capture.lock().unwrap().push(bytes.to_vec());
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                ..Default::default()
            },
        );
        host.rx_from_side(
            &sedsnet::discovery::build_discovery_announce("AB", 1, &[dedicated]).unwrap(),
            hs,
        )
        .unwrap();
        board
            .rx_from_side(
                &sedsnet::discovery::build_discovery_announce(
                    "GS",
                    1,
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                )
                .unwrap(),
                bs,
            )
            .unwrap();
        host.tx_queue(exchange.packet(now).unwrap().clone())
            .unwrap();
        for _ in 0..20 {
            host.process_all_queues_with_timeout(0).unwrap();
            for bytes in std::mem::take(&mut *outbound.lock().unwrap()) {
                board.rx_packed_from_side(&bytes, bs).unwrap();
            }
            board.process_all_queues_with_timeout(0).unwrap();
            for bytes in std::mem::take(&mut *inbound.lock().unwrap()) {
                host.rx_packed_from_side(&bytes, hs).unwrap();
            }
        }
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(
            requests.lock().unwrap()[0].payload(),
            exchange.packet.payload()
        );
        assert!(statuses.lock().unwrap().is_empty());
        assert!(
            exchange.terminal.is_none(),
            "Delivery ACK is not actual completion"
        );
        // A replay of the same SDK packet is ACK-only: it cannot poll the
        // asynchronous owner. A fresh transport envelope preserves command UTC.
        host.tx_queue(exchange.packet(now).unwrap().clone())
            .unwrap();
        for _ in 0..20 {
            host.process_all_queues_with_timeout(0).unwrap();
            for bytes in std::mem::take(&mut *outbound.lock().unwrap()) {
                board.rx_packed_from_side(&bytes, bs).unwrap();
            }
            board.process_all_queues_with_timeout(0).unwrap();
            for bytes in std::mem::take(&mut *inbound.lock().unwrap()) {
                host.rx_packed_from_side(&bytes, hs).unwrap();
            }
        }
        assert_eq!(requests.lock().unwrap().len(), 1);
        host.tx_queue(exchange.poll_packet(now).unwrap()).unwrap();
        for _ in 0..20 {
            host.process_all_queues_with_timeout(0).unwrap();
            for bytes in std::mem::take(&mut *outbound.lock().unwrap()) {
                board.rx_packed_from_side(&bytes, bs).unwrap();
            }
            board.process_all_queues_with_timeout(0).unwrap();
            for bytes in std::mem::take(&mut *inbound.lock().unwrap()) {
                host.rx_packed_from_side(&bytes, hs).unwrap();
            }
        }
        assert_eq!(requests.lock().unwrap().len(), 2);
        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured[0].timestamp(), captured[1].timestamp());
        }
        board.tx_queue(reply(&exchange, 0, 0x3e, 9)).unwrap();
        for _ in 0..20 {
            board.process_all_queues_with_timeout(0).unwrap();
            for bytes in std::mem::take(&mut *inbound.lock().unwrap()) {
                host.rx_packed_from_side(&bytes, hs).unwrap();
            }
            host.process_all_queues_with_timeout(0).unwrap();
            for bytes in std::mem::take(&mut *outbound.lock().unwrap()) {
                board.rx_packed_from_side(&bytes, bs).unwrap();
            }
        }
        let received = statuses.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(exchange.observe(&received[0], now), Ok(Outcome::Applied));
    }
    #[test]
    fn physical_terminal_masks_are_board_domains_not_a_generic_zero_assumption() {
        for (board, unsafe_mask, safe_mask) in [
            (Board::ActuatorBoard, 6u32, 0u32),
            (Board::ValveBoard, 0, 6),
            (Board::FlightComputer, 6, 0),
        ] {
            let (mut exchange, now) = setup(board);
            let valid = reply(&exchange, 0, 0x3e, 9);
            let mut body = valid.payload().to_vec();
            body[28..32].copy_from_slice(&unsafe_mask.to_le_bytes());
            let invalid = Packet::new(
                valid.data_type(),
                valid.endpoints(),
                valid.sender(),
                valid.timestamp(),
                Arc::from(body),
            )
            .unwrap();
            assert!(exchange.observe(&invalid, now).is_err());
            assert!(exchange.terminal.is_none());
            assert_eq!(
                u32::from_le_bytes(valid.payload()[28..32].try_into().unwrap()),
                safe_mask
            );
            assert_eq!(exchange.observe(&valid, now).unwrap(), Outcome::Applied);
        }
    }
}
