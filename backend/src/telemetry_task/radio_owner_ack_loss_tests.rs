//! Native regression: maintained GS transport helpers and actual registry SDK.
//! RAM owner/status and modeled UART only; no RF MCU or modem MAC simulation.
use super::*;
use crate::comms::{
    reference_decoder::{Decoder, frame_data},
    rfd_capacity::{Capacity, Defaults},
    rfd_profile::Profile,
    virtual_uart::WireDrain,
};
use sedsnet::{
    config::{self, DataType},
    packet::Packet,
    router::{
        EndpointHandler, PackedWriteLease, RetryTrackerLimits, Router, RouterConfig, RouterSideId,
        RouterSideOptions, WriteStart,
    },
};
use std::{
    sync::atomic::{AtomicU8, AtomicU64},
    time::Instant,
};

// This clock belongs only to this isolated test module, never the application or Session.
static MODEL_MS: AtomicU64 = AtomicU64::new(1000);

#[derive(Clone)]
struct Attempt {
    model_ms: u64,
    id: Option<u64>,
    ty: Option<u32>,
    utc: Option<u64>,
    busy: bool,
    unexpected_error: bool,
}
struct Channel {
    budget: Arc<TxQueueBudget>,
    activity: Arc<TransportTxActivity>,
    receiver: mpsc::UnboundedReceiver<(u8, Vec<u8>)>,
    trace: Arc<Mutex<Vec<Attempt>>>,
    capacity: Capacity,
    wire: WireDrain,
    decoder: Decoder,
    active: Option<Writing>,
    written: Vec<(u64, u64, u64)>,
    completed_bytes: usize,
}
struct Writing {
    priority: u8,
    body: Vec<u8>,
    framed: Vec<u8>,
    lease: PackedWriteLease,
    stop_us: u64,
}
impl Channel {
    fn new(profile: &Profile, share_percent: u64) -> (Self, mpsc::UnboundedSender<(u8, Vec<u8>)>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let t = &profile.transport;
        let capacity = Capacity::from_defaults(
            115200,
            Defaults {
                air_bps: t.usable_air_bps,
                overhead_bytes: t.frame_overhead_bytes,
                packet_burst_ms: t.packet_burst_ms,
                packet_gap_ms: t.packet_gap_ms,
                host_share_percent: share_percent,
            },
        );
        assert_eq!(capacity.airtime_config(), (32000, share_percent));
        assert_eq!(capacity.packetization(), (100, 55));
        (
            Self {
                budget: Arc::new(TxQueueBudget::new_radio(2048, 256)),
                activity: Arc::new(TransportTxActivity::default()),
                receiver,
                trace: Arc::new(Mutex::new(Vec::new())),
                capacity,
                wire: Default::default(),
                decoder: Default::default(),
                active: None,
                written: Vec::new(),
                completed_bytes: 0,
            },
            sender,
        )
    }
    fn install(
        &self,
        router: &Router,
        sender: mpsc::UnboundedSender<(u8, Vec<u8>)>,
    ) -> RouterSideId {
        let budget = self.budget.clone();
        let activity = self.activity.clone();
        let trace = self.trace.clone();
        router
            .add_side_packed_leased_with_priority_and_options(
                "original-radio",
                move |lease, priority| {
                    let packet = serialize::unpack_packet(lease.bytes()).ok();
                    let id = packet.as_ref().map(Packet::packet_id);
                    let ty = packet.as_ref().map(|p| p.data_type().as_u32());
                    let utc = packet.as_ref().map(Packet::timestamp);
                    let result =
                        budget.try_send_leased_tracked(&sender, priority, lease, &activity);
                    let busy = matches!(&result, Err(sedsnet::TelemetryError::Io("side tx busy")));
                    let unexpected_error = result.is_err() && !busy;
                    let mut entries = trace.lock().unwrap();
                    assert!(
                        entries.len() < 20000,
                        "actual callback trace is bounded, never omit overflow"
                    );
                    entries.push(Attempt {
                        model_ms: MODEL_MS.load(Ordering::SeqCst),
                        id,
                        ty,
                        utc,
                        busy,
                        unexpected_error,
                    });
                    result
                },
                RouterSideOptions {
                    reliable_enabled: false,
                    ..RouterSideOptions::default()
                }
                .with_small_packet_transport(192),
                RetryTrackerLimits::default(),
            )
            .unwrap()
            .0
    }
    fn pump(&self, router: &Router) {
        let before = router.export_runtime_stats();
        let mark = self.trace.lock().unwrap().len();
        let result = router.process_all_queues_with_timeout(0);
        let after = router.export_runtime_stats();
        let trace = self.trace.lock().unwrap();
        let entries = &trace[mark..];
        assert!(
            entries.iter().all(|a| !a.unexpected_error),
            "known backpressure cannot mask any other callback error"
        );
        let attempts: u64 = after
            .sides
            .iter()
            .map(|s| {
                let old = before
                    .sides
                    .iter()
                    .find(|x| x.side_id == s.side_id)
                    .unwrap();
                assert_eq!(
                    s.local_handler_failures, old.local_handler_failures,
                    "no unobserved local/RX error waiver"
                );
                s.tx_packets.checked_sub(old.tx_packets).unwrap()
                    + s.tx_retries.checked_sub(old.tx_retries).unwrap()
            })
            .sum();
        assert_eq!(
            attempts,
            entries.len() as u64,
            "all actual SDK callbacks, including failed multiframe prefixes"
        );
        let tx_retries: u64 = after
            .sides
            .iter()
            .map(|side| {
                let old = before
                    .sides
                    .iter()
                    .find(|old| old.side_id == side.side_id)
                    .unwrap();
                side.tx_retries.checked_sub(old.tx_retries).unwrap()
            })
            .sum();
        let tx_failures: u64 = after
            .sides
            .iter()
            .map(|side| {
                let old = before
                    .sides
                    .iter()
                    .find(|old| old.side_id == side.side_id)
                    .unwrap();
                side.tx_handler_failures
                    .checked_sub(old.tx_handler_failures)
                    .unwrap()
            })
            .sum();
        let busy = entries.iter().filter(|entry| entry.busy).count() as u64;
        // Raw SDK side tx_retries includes successful multiframe prefixes,
        // not only refused callbacks and not E2E retry ownership.
        assert!(
            busy <= tx_retries,
            "known refused callbacks cannot exceed actual accounted callback attempts"
        );
        assert_eq!(
            after
                .total_handler_failures
                .checked_sub(before.total_handler_failures)
                .unwrap(),
            tx_failures,
            "no unrelated failure can hide behind accepted queue deferral"
        );
        match result {
            // SDK4311 dispatch_queued_item_with_origin retains an already
            // accepted original and can report Ok after its callback refused.
            // Ok supplies no wire/progress evidence; exact counters above and
            // the later real ACK/owner/convergence assertions remain mandatory.
            Ok(()) => {}
            Err(sedsnet::TelemetryError::Io("side tx busy")) => {
                assert!(
                    busy > 0 && tx_failures > 0,
                    "exact busy return requires fresh exhaustive actual callback refusal"
                );
            }
            Err(error) => panic!("unexpected SDK result remains fatal: {error:?}"),
        }
    }
    /// Completes actual modeled stop bits, then releases this original local
    /// write owner. This is not a peer/SDK command ACK or a hardware receipt.
    fn step(&mut self, now: u64) -> Vec<Vec<u8>> {
        let mut delivered = Vec::new();
        if self
            .active
            .as_ref()
            .is_some_and(|a| a.stop_us <= now * 1000)
        {
            let a = self.active.take().unwrap();
            for chunk in a.framed.chunks(7) {
                delivered.extend(
                    self.decoder
                        .push(chunk, now)
                        .into_iter()
                        .map(|(_, body)| body),
                );
            }
            assert_eq!(
                self.decoder.rejected, 0,
                "no corruption or modem byte braid is injected"
            );
            a.lease.report_drained().unwrap();
            self.budget.complete(a.priority, &a.body);
            self.activity.complete_frame(&a.body);
            self.completed_bytes += a.framed.len();
            if let Ok(packet) = serialize::unpack_packet(&a.body) {
                self.written
                    .push((now, packet.packet_id(), packet.timestamp()));
            }
        }
        if self.active.is_none()
            && self.wire.ready(now)
            && let Ok((priority, body)) = self.receiver.try_recv()
        {
            let contract = crate::comms::radio_packet_boundary_contract(priority, &body);
            let framed = frame_data(&body).unwrap();
            let wall = Instant::now();
            let ready = match contract {
                Some((id, true)) => {
                    self.capacity
                        .ready_for_receipt(wall, Some(now), id, framed.len())
                }
                Some((id, false)) => self.capacity.ready_for_command(
                    wall,
                    Some(now),
                    id,
                    framed.len(),
                    crate::comms::is_radio_batch_command(priority, &body),
                ),
                None => self.capacity.ready_for(wall, Some(now), false),
            };
            let lease = self
                .budget
                .write_lease(&body)
                .expect("actual SDK admission lease remains attached through queue pop");
            assert!(self.budget.pending.load(Ordering::Acquire) >= body.len());
            if !ready {
                // Hold the SAME accepted Vec/lease without modifying queue
                // ownership, write counters or SDK completion evidence.
                self.active = Some(Writing {
                    priority,
                    body,
                    framed,
                    lease,
                    stop_us: u64::MAX,
                });
            } else {
                self.start(now, priority, body, framed, lease, contract);
            }
        }
        // A selected but not started owner is retained as such, not completed.
        if self.active.as_ref().is_some_and(|a| a.stop_us == u64::MAX) {
            let a = self.active.take().unwrap();
            let wall = Instant::now();
            let contract = crate::comms::radio_packet_boundary_contract(a.priority, &a.body);
            let ready = match contract {
                Some((id, true)) => {
                    self.capacity
                        .ready_for_receipt(wall, Some(now), id, a.framed.len())
                }
                Some((id, false)) => self.capacity.ready_for_command(
                    wall,
                    Some(now),
                    id,
                    a.framed.len(),
                    crate::comms::is_radio_batch_command(a.priority, &a.body),
                ),
                None => self.capacity.ready_for(wall, Some(now), false),
            };
            if ready {
                self.start(now, a.priority, a.body, a.framed, a.lease, contract);
            } else {
                self.active = Some(a);
            }
        }
        delivered
    }
    fn start(
        &mut self,
        now: u64,
        priority: u8,
        body: Vec<u8>,
        framed: Vec<u8>,
        lease: PackedWriteLease,
        contract: Option<(u64, bool)>,
    ) {
        assert!(matches!(
            PackedWriteLease::try_start_batch(&[&lease]).unwrap(),
            WriteStart::Ready
        ));
        let wall = Instant::now();
        let len = framed.len();
        match contract {
            Some((id, true)) => {
                self.capacity
                    .committed_receipt(wall, Some(now), id, len, len, true);
            }
            Some((id, false)) => {
                self.capacity.committed_command(
                    wall,
                    Some(now),
                    (id, len),
                    crate::comms::is_radio_batch_command(priority, &body),
                    len,
                    true,
                );
            }
            None => self.capacity.committed(wall, Some(now), len),
        }
        self.wire.committed(now, len, 115200);
        let stop_us = self.wire.stop_us();
        self.capacity.drained_virtual(stop_us);
        if crate::comms::closes_radio_packet_batch(priority, &body) {
            self.capacity
                .close_packet_batch_for_contract(wall, Some(now), contract);
        }
        self.active = Some(Writing {
            priority,
            body,
            framed,
            lease,
            stop_us,
        });
    }
    fn idle(&self) -> bool {
        self.active.is_none() && self.budget.pending.load(Ordering::Acquire) == 0
    }
}
fn wire_symbol(name: &str) -> u64 {
    sedsnet::packet::hash_bytes_u64(0x517C_C1B7_2722_0A95, name.as_bytes())
}
fn exact_owner_ack_for(body: &[u8], id: u64, owner: &str) -> bool {
    let Ok(info) = serialize::peek_frame_info(body) else {
        return false;
    };
    if info.envelope.ty != DataType::ReliableAck
        || info.ack_only()
        || info.envelope.target_senders.as_ref() != [wire_symbol(owner), wire_symbol("GS")]
    {
        return false;
    }
    serialize::unpack_packet(body).is_ok_and(|p| p.payload() == id.to_le_bytes())
}
fn exact_owner_ack(body: &[u8], id: u64) -> bool {
    exact_owner_ack_for(body, id, "RF")
}
fn scenario(drop_first: bool) {
    MODEL_MS.store(1000, Ordering::SeqCst);
    let wall_started = Instant::now();
    crate::telemetry_schema::initialize().unwrap();
    let mut tuning = config::runtime_tuning_config();
    tuning.payload_compress_threshold = usize::MAX;
    tuning.reliable_retransmit_ms = 250;
    tuning.reliable_max_retries = 17;
    config::set_runtime_tuning_config(tuning).unwrap();
    // One maintained GS application profile configures both RAM directions. This does not
    // execute RF firmware or claim its live modem settings match this fixture.
    let profile = Profile::parse(include_bytes!("../../comms/rfd-profile.json")).unwrap();
    let ty = crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW");
    let owner = crate::telemetry_schema::endpoint("AV_BAY_UNDERGLOW_OWNER");
    let station = crate::telemetry_schema::endpoint("GROUND_STATION");
    let status = config::register_data_type_with_description(
        "RADIO_OWNER_ACK_LOSS_DIAGNOSTIC_STATUS",
        "RAM fixture status, never physical board readiness",
        sedsnet::MessageElement::Static(
            9,
            sedsnet::MessageDataType::UInt8,
            sedsnet::MessageClass::Data,
        ),
        &[station],
        sedsnet::ReliableMode::Unordered,
        16,
    )
    .unwrap();
    let effects = Arc::new(Mutex::new(Vec::<(u64, u64, u8)>::new()));
    let received = Arc::new(Mutex::new(Vec::<(u64, u64, u8)>::new()));
    let state = Arc::new(AtomicU8::new(0));
    let observed = received.clone();
    let gs = Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(station, move |p| {
            if p.data_type() == status {
                let data = p.payload();
                assert_eq!(data.len(), 9);
                assert_eq!(p.sender(), "RF");
                observed.lock().unwrap().push((
                    u64::from_le_bytes(data[..8].try_into().unwrap()),
                    MODEL_MS.load(Ordering::SeqCst),
                    data[8],
                ));
            }
            Ok(())
        })])
        .with_sender("GS"),
        Box::new(|| MODEL_MS.load(Ordering::SeqCst)),
    );
    let rf = Router::new_with_clock(
        RouterConfig::new([])
            .with_sender("RF")
            .with_e2e_encryption(sedsnet::router::RouterE2eEncryptionMode::Disabled),
        Box::new(|| MODEL_MS.load(Ordering::SeqCst)),
    );
    for (router, write) in [(&gs, true), (&rf, false)] {
        router
            .enable_network_variable(
                ty,
                sedsnet::router::NetworkVariablePermissions { read: true, write },
            )
            .unwrap();
    }
    let applied = effects.clone();
    let actual = state.clone();
    rf.on_network_variable_update(ty, move |packet| {
        assert_eq!(packet.payload(), [1]);
        actual.store(1, Ordering::SeqCst);
        applied
            .lock()
            .unwrap()
            .push((packet.packet_id(), MODEL_MS.load(Ordering::SeqCst), 1));
        Ok(())
    })
    .unwrap();
    let (mut outbound, tx) = Channel::new(&profile, profile.transport.host_default_share_percent);
    let gs_side = outbound.install(&gs, tx);
    let (mut returning, tx) = Channel::new(&profile, profile.transport.rf_share_percent);
    let rf_side = returning.install(&rf, tx);
    let mut now = 1000;
    MODEL_MS.store(now, Ordering::SeqCst);
    gs.announce_discovery().unwrap();
    rf.announce_discovery().unwrap();
    let mut warmed = false;
    for _ in 0..60000 {
        MODEL_MS.store(now, Ordering::SeqCst);
        outbound.pump(&gs);
        returning.pump(&rf);
        for bytes in outbound.step(now) {
            rf.rx_packed_queue_from_side(&bytes, rf_side).unwrap();
        }
        for bytes in returning.step(now) {
            gs.rx_packed_queue_from_side(&bytes, gs_side).unwrap();
        }
        if gs
            .export_topology()
            .routes
            .iter()
            .any(|r| r.reachable_network_variables.contains(&ty))
            && outbound.idle()
            && returning.idle()
            && !gs.has_queued_work()
            && !rf.has_queued_work()
        {
            warmed = true;
            break;
        }
        now += 1;
    }
    assert!(
        warmed,
        "real same-path discovery/admission must converge before loss scene; no invented owner"
    );
    let issued = now;
    let packet = Packet::new(ty, &[owner], "GS", issued, Arc::from([1u8])).unwrap();
    let id = packet.packet_id();
    let utc = packet.timestamp();
    gs.set_network_variable(packet).unwrap();
    assert!(
        gs.export_reliable_pending_stats()
            .iter()
            .any(|p| p.packet_id == id && p.pending_destinations == vec![wire_symbol("RF")]),
        "must observe genuine frozen RF reliable owner, not a loss label"
    );
    assert!(
        outbound.budget.pending.load(Ordering::Acquire) > 0,
        "actual local transport admission precedes write completion"
    );
    let mut dropped = 0;
    let mut actual_acks = 0;
    let mut max_retries = 0;
    let mut status_sent = false;
    let mut completed = None;
    let mut owner_survived_uart = false;
    for _ in 0..=500 {
        MODEL_MS.store(now, Ordering::SeqCst);
        outbound.pump(&gs);
        returning.pump(&rf);
        for body in outbound.step(now) {
            rf.rx_packed_queue_from_side(&body, rf_side).unwrap();
        }
        if outbound.written.iter().any(|(_, p, _)| *p == id)
            && gs
                .export_reliable_pending_stats()
                .iter()
                .any(|p| p.packet_id == id)
        {
            owner_survived_uart = true;
        }
        if !status_sent && !effects.lock().unwrap().is_empty() {
            let mut body = id.to_le_bytes().to_vec();
            body.push(state.load(Ordering::SeqCst));
            rf.try_tx_queue(Packet::new(status, &[station], "RF", now, body.into()).unwrap())
                .unwrap();
            status_sent = true;
        }
        for body in returning.step(now) {
            if exact_owner_ack(&body, id) {
                actual_acks += 1;
                assert!(!exact_owner_ack(&body, id.wrapping_add(1)));
                assert!(
                    !exact_owner_ack_for(&body, id, "VB"),
                    "another logical owner cannot satisfy originalRFreceipt"
                );
                if drop_first && dropped == 0 {
                    dropped += 1;
                    assert_eq!(effects.lock().unwrap().len(), 1);
                    assert!(
                        gs.export_reliable_pending_stats()
                            .iter()
                            .any(|p| p.packet_id == id)
                    );
                    continue;
                }
            }
            gs.rx_packed_queue_from_side(&body, gs_side).unwrap();
        }
        for p in gs
            .export_reliable_pending_stats()
            .iter()
            .filter(|p| p.packet_id == id)
        {
            max_retries = max_retries.max(p.retries);
        }
        if status_sent
            && received
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _, value)| *p == id && *value == 1)
            && !gs
                .export_reliable_pending_stats()
                .iter()
                .any(|p| p.packet_id == id)
            && !rf
                .export_reliable_pending_stats()
                .iter()
                .any(|p| p.data_type == status)
        {
            completed = Some(now);
            break;
        }
        now += 1;
    }
    let done = completed
        .expect("original command/status/owner ACK must converge within unchanged500MODELms");
    assert!(done - issued <= 500);
    assert!(
        owner_survived_uart,
        "local modeled UART completion is not SDK owner ACK"
    );
    let effects = effects.lock().unwrap();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].0, id);
    assert!(effects[0].1 - issued <= 500);
    assert_eq!(dropped, usize::from(drop_first));
    assert!(actual_acks >= 1);
    let trace = outbound.trace.lock().unwrap();
    let command = trace
        .iter()
        .filter(|x| x.id == Some(id) && !x.busy)
        .collect::<Vec<_>>();
    for offer in &command {
        assert_eq!(offer.utc, Some(utc));
        assert_eq!(offer.ty, Some(ty.as_u32()));
        assert!(offer.model_ms >= issued);
    }
    let copies = outbound.written.iter().filter(|(_, p, _)| *p == id).count();
    if drop_first {
        assert!(max_retries >= 1);
        assert!(
            command.len() >= 2 && copies >= 2,
            "actual same-ID SDK retry and wire write mandatory"
        );
        assert!(actual_acks >= 2);
    } else {
        assert_eq!(command.len(), 1);
        assert_eq!(copies, 1);
        assert_eq!(max_retries, 0);
    }
    let command_count = command.len();
    drop(command);
    drop(trace);
    for (router, channel, side) in [(&gs, &outbound, gs_side), (&rf, &returning, rf_side)] {
        let stats = router.export_runtime_stats();
        let actual = stats.sides.iter().find(|s| s.side_id == side).unwrap();
        assert_eq!(
            actual.tx_packets + actual.tx_retries,
            channel.trace.lock().unwrap().len() as u64,
            "whole-scene callback accounting includes direct SDK offers and every startup prefix"
        );
        assert_eq!(actual.local_handler_failures, 0);
    }
    assert_eq!(outbound.decoder.rejected, 0);
    assert_eq!(returning.decoder.rejected, 0);
    // Existing native isolation forwards child stderr, preserving both actual scene receipts.
    eprintln!(
        "ACTUAL_OWNER_ACK_LOSS {}",
        serde_json::json!({"drop_first":drop_first,"packet_id":id,"original_utc":utc,"warmup_model_ms":issued-1000,"completed_model_ms":done-issued,"wall_ms":wall_started.elapsed().as_millis(),"actual_owner_acks":actual_acks,"actual_drops":dropped,"sdk_retries":max_retries,"command_callbacks":command_count,"modeled_wire_copies":copies,"owner_effects":effects.len(),"hardware":false})
    );
}

#[test]
fn actual_first_owner_ack_loss_preserves_original_radio_command_and_status_inside_500ms() {
    if crate::operator_queue::isolated_native_case(
        module_path!(),
        "actual_first_owner_ack_loss_preserves_original_radio_command_and_status_inside_500ms",
    ) {
        return;
    }
    assert!(
        !std::env::vars_os().any(|(key, _)| key.to_string_lossy().starts_with("GS_RADIO_")),
        "no hidden pacing/profile overrides in bounded test child"
    );
    let saved = config::runtime_tuning_config();
    struct Restore(config::RuntimeTuningConfig);
    impl Drop for Restore {
        fn drop(&mut self) {
            config::set_runtime_tuning_config(self.0).unwrap();
        }
    }
    let _restore = Restore(saved);
    scenario(false);
    scenario(true);
}
