//! RRLS: the underglow and buzzer rows from the frames the RRLS relays or
//! hears, as they pass through.
//!
//! The RRLS shows the primary's view of the two cosmetic network variables
//! from the primary's control observation (89-byte observation, marker 6).
//! While the RRLS relays, it never sends an observe refresh (the relay's air is
//! the primary's link), so after a toggle it kept showing the old request,
//! "confirmed", until some later exchange: 15-20 s on the bench, for a toggle
//! the AV bay had already applied.
//!
//! The primary's write of either variable crosses the RRLS (relayed uplink,
//! or heard on air while the RRLS listens), and so do the owners'
//! acknowledgements and the FC's BOARD_HEALTH readback. Each one updates the
//! rows at once:
//! - a write from the primary's sender is its new request, not yet confirmed;
//! - a readback is the FC's BOARD_HEALTH word 14, a value of the variable
//!   from any other sender, or an owner's acknowledgement of the primary's
//!   underglow write (RF, PB and FC apply every underglow write before
//!   acknowledging it; a buzzer acknowledgement is not a readback);
//! - a readback confirms the request only when seen after it.
//!
//! Display only: nothing here sends, acknowledges or writes a variable, and
//! during a takeover the RRLS's own network variables show the rows.
use crate::network_variables::NetworkOutputView;
use sedsnet::packet::Packet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

const TYPES: [&str; 2] = ["AV_BAY_UNDERGLOW", "FLIGHT_BUZZER"];

#[derive(Clone, Copy, Debug, Default)]
struct Output {
    /// The primary's latest write: value, packet id, first seen.
    request: Option<(bool, u64, Instant)>,
    /// The latest network value: value, seen at.
    readback: Option<(bool, Instant)>,
}

static OUTPUTS: Mutex<[Output; 2]> = Mutex::new([Output { request: None, readback: None }; 2]);
static CHANGED: AtomicBool = AtomicBool::new(false);

fn outputs() -> std::sync::MutexGuard<'static, [Output; 2]> {
    OUTPUTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn primary_sender(sender: &str) -> bool {
    sedsnet::packet::sender_address_u32(sender)
        == sedsnet::packet::sender_address_u32(crate::types::Board::GroundStation.sender_id())
}

fn station_sender(sender: &str) -> bool {
    primary_sender(sender)
        || sedsnet::packet::sender_address_u32(sender) == sedsnet::packet::sender_address_u32("RRLS")
}

/// Canonical packets in one radio frame: a plain frame or a bundle. Stateful
/// SDT fragments are skipped (the primary's writes and the acknowledgements
/// are canonical; the FC readback reaches the RRLS's own router).
fn packets(frame: &[u8]) -> Vec<Packet> {
    if sedsnet::wire_bundle::is_bundle(frame) {
        sedsnet::wire_bundle::unpack_bundle_frames(frame)
            .map(|frames| {
                frames
                    .iter()
                    .filter_map(|f| sedsnet::wire_format::unpack_packet(f).ok())
                    .collect()
            })
            .unwrap_or_default()
    } else if sedsnet::side_peek::is_side_transport_frame(frame) {
        Vec::new()
    } else {
        sedsnet::wire_format::unpack_packet(frame).into_iter().collect()
    }
}

/// RRLS, not in takeover: one SEDSnet frame it relays or hears, in either
/// direction. Cheap when the frame carries neither variable nor an ACK.
/// Called only from the RRLS's mesh receive, outside a takeover.
pub(crate) fn observe_frame(frame: &[u8]) {
    for packet in packets(frame) {
        observe_packet(&packet, Instant::now());
    }
}

fn observe_packet(packet: &Packet, now: Instant) {
    let ty = packet.data_type();
    if ty == sedsnet::config::DataType::ReliableAck {
        let Ok(id) = <[u8; 8]>::try_from(packet.payload()) else {
            return;
        };
        if station_sender(packet.sender()) {
            return;
        }
        let mut outputs = outputs();
        // Underglow only: its owners apply a write before acknowledging it.
        let underglow = &mut outputs[0];
        if let Some((value, request_id, _)) = underglow.request
            && request_id == u64::from_le_bytes(id)
            && underglow.readback.is_none_or(|(old, _)| old != value)
        {
            underglow.readback = Some((value, now));
            CHANGED.store(true, Ordering::Release);
        }
        return;
    }
    let Some(index) = TYPES
        .iter()
        .position(|name| ty == crate::telemetry_schema::data_type(name))
    else {
        return;
    };
    let Some(&byte) = packet.payload().first() else {
        return;
    };
    let value = byte != 0;
    if primary_sender(packet.sender()) {
        let mut outputs = outputs();
        let output = &mut outputs[index];
        // A retransmission of the same write keeps its first sighting.
        if output.request.is_none_or(|(_, id, _)| id != packet.packet_id()) {
            output.request = Some((value, packet.packet_id(), now));
            CHANGED.store(true, Ordering::Release);
        }
    } else if !station_sender(packet.sender()) {
        readback(index, value, now);
    }
}

/// An inbound value of either variable delivered to this RRLS's own router.
pub(crate) fn observe_inbound(packet: &Packet) {
    if crate::rrls::is_rrls() && !crate::rrls::takeover_active() {
        observe_packet(packet, Instant::now());
    }
}

/// A readback the RRLS decoded itself (FC BOARD_HEALTH word 14).
pub(crate) fn observe_readback(index: usize, value: bool) {
    if crate::rrls::is_rrls() && !crate::rrls::takeover_active() {
        readback(index, value, Instant::now());
    }
}

fn readback(index: usize, value: bool, now: Instant) {
    let mut outputs = outputs();
    let output = &mut outputs[index];
    let changed = output.readback.is_none_or(|(old, _)| old != value);
    output.readback = Some((value, now));
    if changed {
        CHANGED.store(true, Ordering::Release);
    }
}

/// The primary's own view of both rows from its station-state broadcast
/// (sent within a second of any change, also when the frames the RRLS relays
/// are compact side-transport frames this decoder skips): the byte, when it
/// last changed (its age against the observation and the relayed frames) and
/// when it was last confirmed (its freshness).
#[derive(Clone, Copy, Debug)]
struct PrimaryView {
    byte: u8,
    changed: Instant,
    seen: Instant,
}
static PRIMARY_VIEW: Mutex<Option<PrimaryView>> = Mutex::new(None);

/// RRLS, not in takeover: the primary's view from its station-state broadcast.
pub(crate) fn observe_primary_view(byte: u8) {
    if crate::rrls::is_rrls() && !crate::rrls::takeover_active() {
        primary_view_at(byte, Instant::now());
    }
}
fn primary_view_at(byte: u8, now: Instant) {
    if !super::netvar_valid(byte) {
        return;
    }
    let mut view = PRIMARY_VIEW.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let changed = match *view {
        Some(old) if old.byte == byte => old.changed,
        _ => {
            CHANGED.store(true, Ordering::Release);
            now
        }
    };
    *view = Some(PrimaryView { byte, changed, seen: now });
}
/// The base the relayed frames update: the newer of the observation and the
/// broadcast view (while that is as fresh as the observation must be).
fn base(
    index: usize,
    observed: Option<NetworkOutputView>,
    observed_at: Instant,
) -> (Option<NetworkOutputView>, Instant) {
    let view = *PRIMARY_VIEW.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match view {
        Some(view)
            if view.seen.elapsed().as_millis() <= u128::from(super::POLICY_FRESH_MS)
                && (view.changed > observed_at || observed.is_none()) =>
        {
            (Some(super::netvar_view(view.byte, index)), view.changed.max(observed_at))
        }
        _ => (observed, observed_at),
    }
}

/// True once since a relayed write or readback changed what the rows show.
/// True once since a relayed write or readback changed what the rows show.
pub(crate) fn take_changed() -> bool {
    CHANGED.swap(false, Ordering::AcqRel)
}

/// The row for output `index`: the primary's observation (`observed`, received
/// at `observed_at`, None when stale) updated by whatever crossed the RRLS
/// after it. None when neither has anything.
pub(super) fn view(
    index: usize,
    observed: Option<NetworkOutputView>,
    observed_at: Instant,
) -> Option<NetworkOutputView> {
    let output = outputs()[index];
    let (observed, observed_at) = base(index, observed, observed_at);
    combine(output, observed, observed_at)
}

fn combine(
    output: Output,
    observed: Option<NetworkOutputView>,
    observed_at: Instant,
) -> Option<NetworkOutputView> {
    let newer_request = output.request.filter(|(_, _, at)| *at > observed_at);
    let requested = match (newer_request, observed) {
        (Some((value, _, _)), _) => value,
        (None, Some(view)) => view.requested,
        // Stale observation and no write since: the request is unknown.
        (None, None) => return None,
    };
    let since = newer_request.map_or(observed_at, |(_, _, at)| at);
    let newer_readback = output.readback.filter(|(_, at)| *at >= since);
    let readback_seen = output.readback.is_some() || observed.is_some_and(|view| view.readback_seen);
    Some(match (newer_request, newer_readback, observed) {
        (_, Some((value, _)), _) => NetworkOutputView {
            requested,
            confirmed: (value == requested).then_some(requested),
            readback_seen: true,
        },
        // A new request with no readback since: unconfirmed.
        (Some(_), None, _) => NetworkOutputView { requested, confirmed: None, readback_seen },
        (None, None, Some(view)) => view,
        (None, None, None) => unreachable!(),
    })
}

/// Tests that read the rows hold this: the relayed state is process-wide.
#[cfg(test)]
pub(super) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(super) fn reset_for_test() {
    *outputs() = [Output::default(); 2];
    *PRIMARY_VIEW.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    CHANGED.store(false, Ordering::Release);
}

#[cfg(test)]
pub(super) fn observe_packet_for_test(packet: &Packet, now: Instant) {
    observe_packet(packet, now);
}

#[cfg(test)]
pub(super) fn primary_view_for_test(byte: u8, now: Instant) {
    primary_view_at(byte, now);
}

#[cfg(test)]
pub(super) fn readback_for_test(index: usize, value: bool, now: Instant) {
    readback(index, value, now);
}
