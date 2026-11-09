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
    /// The SDK reports an exhausted Ordered owner once, from the queue pass.
    allow_ordered_exhaustion: bool,
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
                allow_ordered_exhaustion: false,
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
            Err(sedsnet::TelemetryError::HandlerError(
                "ordered end-to-end delivery exhausted retries",
            )) if self.allow_ordered_exhaustion => {}
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

// Network confirmation for the cosmetic AV-bay network variables. The same modeled UART
// pair as above, driven through the production network_variables entry points.
// Measured 10-29 field round trips reached 4976 ms against a 4.5 s retry horizon.
const FIELD_SLOW_ACK_MS: u64 = 4976;
const OWNER_REPORT_PERIOD_MS: u64 = 1000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Back {
    Deliver,
    Hold,
}

struct OwnerHarness {
    gs: Router,
    rf: Router,
    outbound: Channel,
    returning: Channel,
    gs_side: RouterSideId,
    rf_side: RouterSideId,
    now: u64,
    ty: sedsnet::config::DataType,
    status: sedsnet::config::DataType,
    station: sedsnet::config::DataEndpoint,
    rf_value: Arc<AtomicU8>,
    rf_applied: Arc<Mutex<Vec<(u64, u8)>>>,
    reports_seen: Arc<AtomicU64>,
    next_report: u64,
    held: Vec<Vec<u8>>,
    /// Every distinct underglow packet the GS radio side completed on the wire.
    written: Vec<(u64, u64, u8)>,
}

impl OwnerHarness {
    fn new() -> Self {
        MODEL_MS.store(1000, Ordering::SeqCst);
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        BASE.get_or_init(Instant::now);
        crate::network_variables::set_network_test_clock(|| {
            *BASE.get().unwrap() + std::time::Duration::from_millis(MODEL_MS.load(Ordering::SeqCst))
        });
        crate::telemetry_schema::initialize().unwrap();
        let mut tuning = config::runtime_tuning_config();
        tuning.payload_compress_threshold = usize::MAX;
        tuning.reliable_retransmit_ms = 250;
        tuning.reliable_max_retries = 17;
        config::set_runtime_tuning_config(tuning).unwrap();
        let profile = Profile::parse(include_bytes!("../../comms/rfd-profile.json")).unwrap();
        let ty = crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW");
        let station = crate::telemetry_schema::endpoint("GROUND_STATION");
        let status = config::register_data_type_with_description(
            "RADIO_OWNER_CONFIRMATION_REPORT",
            "RAM fixture periodic owner report, never physical board readiness",
            sedsnet::MessageElement::Static(
                1,
                sedsnet::MessageDataType::UInt8,
                sedsnet::MessageClass::Data,
            ),
            &[station],
            sedsnet::ReliableMode::None,
            16,
        )
        .unwrap();
        let reports_seen = Arc::new(AtomicU64::new(0));
        let seen = reports_seen.clone();
        let gs = Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(station, move |p| {
                if p.data_type() == status && p.sender() == "RF" {
                    seen.fetch_add(1, Ordering::SeqCst);
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
        let rf_value = Arc::new(AtomicU8::new(0));
        let rf_applied = Arc::new(Mutex::new(Vec::new()));
        let (value, applied) = (rf_value.clone(), rf_applied.clone());
        rf.on_network_variable_update(ty, move |packet| {
            let v = packet.payload()[0];
            value.store(v, Ordering::SeqCst);
            applied.lock().unwrap().push((packet.packet_id(), v));
            Ok(())
        })
        .unwrap();
        let (mut outbound, tx) =
            Channel::new(&profile, profile.transport.host_default_share_percent);
        outbound.allow_ordered_exhaustion = true;
        let gs_side = outbound.install(&gs, tx);
        let (returning, tx) = Channel::new(&profile, profile.transport.rf_share_percent);
        let rf_side = returning.install(&rf, tx);
        gs.announce_discovery().unwrap();
        rf.announce_discovery().unwrap();
        let mut h = Self {
            gs,
            rf,
            outbound,
            returning,
            gs_side,
            rf_side,
            now: 1000,
            ty,
            status,
            station,
            rf_value,
            rf_applied,
            reports_seen,
            next_report: 0,
            held: Vec::new(),
            written: Vec::new(),
        };
        let ty = h.ty;
        let warmed = h.run(60_000, |_| false, |_, _| Back::Deliver, |h| {
            h.gs.export_topology()
                .routes
                .iter()
                .any(|r| r.reachable_network_variables.contains(&ty))
                && h.outbound.idle()
                && h.returning.idle()
                && !h.gs.has_queued_work()
                && !h.rf.has_queued_work()
        });
        assert!(warmed, "real discovery must converge before the owner scene");
        h
    }

    fn underglow(body: &[u8], ty: sedsnet::config::DataType) -> Option<(u64, u8)> {
        let packet = serialize::unpack_packet(body).ok()?;
        (packet.data_type() == ty && packet.payload().len() == 1)
            .then(|| (packet.packet_id(), packet.payload()[0]))
    }

    fn view(&self) -> crate::network_variables::NetworkOutputView {
        crate::network_variables::network_output_view(
            crate::network_variables::NetworkOutput::Underglow,
        )
    }

    fn pending_id(&self) -> u64 {
        self.gs
            .export_reliable_pending_stats()
            .iter()
            .find(|p| p.data_type == self.ty)
            .map(|p| p.packet_id)
            .expect("published underglow has a frozen RF owner")
    }

    fn off_copies(&self) -> Vec<u64> {
        let mut ids = self
            .written
            .iter()
            .filter(|(_, _, v)| *v == 0)
            .map(|(_, id, _)| *id)
            .collect::<Vec<_>>();
        ids.dedup();
        ids
    }

    /// Runs model time in 1 ms steps. `drop_out` discards a GS->RF frame;
    /// `back` decides whether an RF->GS frame is delivered now or held.
    fn run(
        &mut self,
        limit_ms: u64,
        mut drop_out: impl FnMut(&[u8]) -> bool,
        mut back: impl FnMut(&[u8], u64) -> Back,
        mut done: impl FnMut(&Self) -> bool,
    ) -> bool {
        let stop = self.now + limit_ms;
        while self.now < stop {
            MODEL_MS.store(self.now, Ordering::SeqCst);
            if self.next_report != 0 && self.now >= self.next_report {
                self.rf
                    .try_tx_queue(
                        Packet::new(self.status, &[self.station], "RF", self.now, Arc::from([1u8]))
                            .unwrap(),
                    )
                    .unwrap();
                self.next_report = self.now + OWNER_REPORT_PERIOD_MS;
            }
            self.outbound.pump(&self.gs);
            self.returning.pump(&self.rf);
            for body in self.outbound.step(self.now) {
                if let Some((id, v)) = Self::underglow(&body, self.ty) {
                    self.written.push((self.now, id, v));
                }
                if drop_out(&body) {
                    continue;
                }
                self.rf.rx_packed_queue_from_side(&body, self.rf_side).unwrap();
            }
            let mut released = Vec::new();
            for body in std::mem::take(&mut self.held) {
                match back(&body, self.now) {
                    Back::Deliver => released.push(body),
                    Back::Hold => self.held.push(body),
                }
            }
            for body in self.returning.step(self.now) {
                match back(&body, self.now) {
                    Back::Deliver => released.push(body),
                    Back::Hold => self.held.push(body),
                }
            }
            for body in released {
                self.gs.rx_packed_queue_from_side(&body, self.gs_side).unwrap();
            }
            crate::network_variables::reconcile_network_outputs(
                &self.gs,
                [self.reports_seen.load(Ordering::SeqCst), 0],
                || Some(()),
            );
            if done(self) {
                return true;
            }
            self.now += 1;
        }
        false
    }

    fn confirm_on(&mut self) {
        crate::network_variables::set_underglow(&self.gs, true).unwrap();
        let ok = self.run(3000, |_| false, |_, _| Back::Deliver, |h| {
            h.view().confirmed == Some(true)
                && h.gs.export_reliable_pending_stats().is_empty()
        });
        assert!(ok, "a delivered On must become network-confirmed");
        assert_eq!(self.rf_value.load(Ordering::SeqCst), 1);
    }
}

fn owner_confirmation_case(case: &str) {
    let mut h = OwnerHarness::new();
    h.confirm_on();
    let ty = h.ty;
    match case {
        "held_ack" => {
            // The operator turns it off from the confirmed On state.
            assert!(!crate::network_variables::toggle_underglow(&h.gs).unwrap());
            let first = h.pending_id();
            let issued = h.now;
            let release = issued + FIELD_SLOW_ACK_MS;
            let mut claimed_while_outstanding = false;
            let mut republished_before_release = false;
            let ok = h.run(
                FIELD_SLOW_ACK_MS + 4000,
                |_| false,
                |body, now| {
                    if exact_owner_ack(body, first) && now < release {
                        Back::Hold
                    } else {
                        Back::Deliver
                    }
                },
                |h| {
                    if h.gs.export_reliable_pending_stats().iter().any(|p| p.packet_id == first)
                        && h.view().confirmed.is_some()
                    {
                        claimed_while_outstanding = true;
                    }
                    if h.now < release && h.off_copies().len() >= 2 {
                        republished_before_release = true;
                    }
                    h.now >= release && h.view().confirmed == Some(false)
                },
            );
            assert!(
                !claimed_while_outstanding,
                "an Off whose ACK is still outstanding is unconfirmed, not Off"
            );
            assert!(
                republished_before_release,
                "exhaustion at the 4.5 s horizon republishes before the 4976 ms ACK lands"
            );
            assert!(ok, "the slow-ACK Off must end network-confirmed Off");
            assert_eq!(h.rf_value.load(Ordering::SeqCst), 0);
            let off = h.off_copies();
            assert!(
                off.len() >= 2 && off[0] == first,
                "GS republishes desired Off after its first retry horizon: {off:?}"
            );
            assert!(
                h.written.iter().all(|(at, _, v)| *at < issued || *v == 0),
                "no On is ever sent after the operator asked for Off"
            );
            assert!(h.view().confirmed == Some(false) && !h.view().requested);
        }
        "dropped_off" => {
            assert!(!crate::network_variables::toggle_underglow(&h.gs).unwrap());
            let first = h.pending_id();
            let issued = h.now;
            // Two full horizons: the original and one immediate republish.
            let both_lost = h.run(
                12_000,
                |body| OwnerHarness::underglow(body, ty).is_some_and(|(_, v)| v == 0),
                |_, _| Back::Deliver,
                |h| {
                    h.off_copies().len() >= 2
                        && h.gs.export_reliable_pending_stats().iter().all(|p| p.data_type != ty)
                },
            );
            assert!(both_lost, "original and republished Off both reach exhaustion");
            assert_eq!(h.rf_value.load(Ordering::SeqCst), 1, "RF never saw Off");
            assert_eq!(h.view().confirmed, None, "GS must not claim Off");
            assert!(!h.view().requested);
            assert_ne!(h.off_copies()[0], h.off_copies()[1]);
            assert_eq!(h.off_copies()[0], first);
            // A press now acts on the board's confirmed On, so it asks for Off
            // again instead of flipping the stored desired value to On.
            let before = h.written.len();
            assert!(!crate::network_variables::toggle_underglow(&h.gs).unwrap());
            let ok = h.run(4000, |_| false, |_, _| Back::Deliver, |h| {
                h.view().confirmed == Some(false)
            });
            assert!(ok, "the press after loss delivers Off and confirms it");
            assert_eq!(h.rf_value.load(Ordering::SeqCst), 0);
            assert!(
                h.written[before..].iter().all(|(_, _, v)| *v == 0)
                    && h.written.iter().all(|(at, _, v)| *at < issued || *v == 0),
                "a press while unconfirmed never sends On"
            );
        }
        "report_republish" => {
            assert!(!crate::network_variables::toggle_underglow(&h.gs).unwrap());
            let both_lost = h.run(
                12_000,
                |body| OwnerHarness::underglow(body, ty).is_some_and(|(_, v)| v == 0),
                |_, _| Back::Deliver,
                |h| {
                    h.off_copies().len() >= 2
                        && h.gs.export_reliable_pending_stats().iter().all(|p| p.data_type != ty)
                },
            );
            assert!(both_lost);
            assert_eq!(h.view().confirmed, None);
            let lost = h.off_copies().len();
            // No owner traffic: GS stays quiet rather than retrying forever.
            let quiet_from = h.written.len();
            h.run(15_000, |_| false, |_, _| Back::Deliver, |_| false);
            assert_eq!(h.written.len(), quiet_from, "no republish without an owner report");
            assert_eq!(h.view().confirmed, None);
            // The next RF report re-publishes desired Off.
            h.next_report = h.now;
            let ok = h.run(4000, |_| false, |_, _| Back::Deliver, |h| {
                h.view().confirmed == Some(false)
            });
            assert!(ok, "the next owner report republishes and confirms desired Off");
            assert!(h.off_copies().len() > lost);
            assert_eq!(h.rf_value.load(Ordering::SeqCst), 0);
        }
        other => panic!("unknown case {other}"),
    }
    let applied = h.rf_applied.lock().unwrap();
    assert!(applied.iter().filter(|(_, v)| *v == 1).count() == 1, "RF applied On exactly once: {applied:?}");
}

fn owner_confirmation_test(name: &str, case: &str) {
    if crate::operator_queue::isolated_native_case(module_path!(), name) {
        return;
    }
    let saved = config::runtime_tuning_config();
    struct Restore(config::RuntimeTuningConfig);
    impl Drop for Restore {
        fn drop(&mut self) {
            config::set_runtime_tuning_config(self.0).unwrap();
        }
    }
    let _restore = Restore(saved);
    owner_confirmation_case(case);
}

#[test]
fn underglow_off_with_ack_held_past_retry_horizon_is_unconfirmed_then_republished() {
    owner_confirmation_test(
        "underglow_off_with_ack_held_past_retry_horizon_is_unconfirmed_then_republished",
        "held_ack",
    );
}

#[test]
fn dropped_underglow_off_stays_unconfirmed_and_toggle_acts_on_confirmed_on() {
    owner_confirmation_test(
        "dropped_underglow_off_stays_unconfirmed_and_toggle_acts_on_confirmed_on",
        "dropped_off",
    );
}

#[test]
fn unconfirmed_underglow_republishes_on_next_owner_report_only() {
    owner_confirmation_test(
        "unconfirmed_underglow_republishes_on_next_owner_report_only",
        "report_republish",
    );
}
