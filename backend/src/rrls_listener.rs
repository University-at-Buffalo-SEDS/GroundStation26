//! Listening RRLS: display-only ingest of AV-bay frames addressed to the
//! primary.
//!
//! SEDSnet hands a received frame to local handlers only when it is untargeted
//! or targets this router's own sender. The AV bay addresses its reliable
//! telemetry (battery, board status readbacks, warnings) to "GS", so a
//! listening RRLS, whose router is "RRLS", never shows any of it, even though
//! its modem hears every copy. Making the RRLS a normal route or giving it a
//! listen mode is ruled out: its acknowledgements would cost air time.
//!
//! Instead every AV-bay frame the listening RRLS hears is also fed to a
//! receive-only shadow router whose sender is the primary's. Its radio side
//! never transmits: acknowledgements, requests and discovery it may produce
//! are discarded. Its one handler forwards only reliable (addressed) display
//! data into the RRLS's ordinary telemetry ingest; untargeted data already
//! reaches the RRLS's own router. Nothing from this path is ever a command:
//! ABORT, FLIGHT_STATE, network variables and recovery requests are refused,
//! and only the newest frame per (sender, type) is shown, so an old overheard
//! value can never overwrite a newer one. During a takeover the RRLS's own
//! router owns the AV bay and this path stands by.
use crate::state::AppState;
use sedsnet::packet::Packet;
use sedsnet::router::{EndpointHandler, Router, RouterConfig, RouterSideId, RouterSideOptions};
use std::collections::HashMap;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};

/// Frames buffered between the radio receive and the shadow router.
const QUEUE_FRAMES: usize = 256;

static FEED: OnceLock<SyncSender<Vec<u8>>> = OnceLock::new();

/// Radio receive (RRLS, not in takeover): offer one unwrapped AV-bay frame.
/// Never blocks; a full queue drops the frame (display data only).
pub(crate) fn observe(frame: &[u8]) {
    if crate::rrls::takeover_active() {
        return;
    }
    if let Some(feed) = FEED.get()
        && let Err(TrySendError::Full(_)) = feed.try_send(frame.to_vec())
    {
        log::debug!("RRLS listener ingest queue full; frame dropped");
    }
}

/// Types the listener never forwards: commands, Abort, desired state and
/// network variables (their owners and the primary decide those).
fn display_type(name: &str) -> bool {
    !matches!(
        name,
        "ABORT"
            | "FLIGHT_STATE"
            | "BOARD_RECOVERY_REQUEST"
            | "AV_BAY_UNDERGLOW"
            | "FLIGHT_BUZZER"
            | "HEARTBEAT"
    ) && !name.ends_with("_COMMAND")
}

/// Newest (timestamp, packet id) shown per (sender, type).
#[derive(Default)]
pub(crate) struct Latest {
    shown: HashMap<(String, u32), (u64, u64)>,
}
impl Latest {
    /// True when `packet` is newer than everything shown for its sender and
    /// type; a retransmission (same id) or an older frame is refused.
    pub(crate) fn admit(&mut self, sender: &str, ty: u32, timestamp: u64, id: u64) -> bool {
        match self.shown.get(&(sender.to_owned(), ty)) {
            Some(&(last, last_id)) if timestamp < last || (timestamp == last && id == last_id) => {
                false
            }
            _ => {
                self.shown.insert((sender.to_owned(), ty), (timestamp, id));
                true
            }
        }
    }
}

/// Whether the shadow router's handler forwards `packet` for display.
pub(crate) fn forwardable(packet: &Packet) -> bool {
    sedsnet::is_reliable_type(packet.data_type())
        && display_type(packet.data_type().as_str().as_ref())
        && !matches!(crate::types::canonical_sender_id(packet.sender()), "GS" | "RRLS")
}

fn ingest(state: &Arc<AppState>, latest: &Mutex<Latest>, packet: &Packet) {
    if crate::rrls::takeover_active() || !forwardable(packet) {
        return;
    }
    if !latest.lock().unwrap().admit(
        packet.sender(),
        packet.data_type().as_u32(),
        packet.timestamp(),
        packet.packet_id(),
    ) {
        return;
    }
    if packet.data_type() == crate::telemetry_schema::data_type("BOARD_STATE_STATUS") {
        crate::board_recovery::receive_status(state, packet);
        return;
    }
    let ingress_generation = crate::gse::automatic_generation(state);
    state.mark_packet_received(crate::telemetry_task::get_current_timestamp_ms());
    state
        .ring_buffer
        .lock()
        .unwrap()
        .push(crate::ring_buffer::ReceivedPacket::Telemetry {
            packet: packet.clone(),
            ingress_generation,
        });
}

/// RRLS only: start the receive-only shadow router (sender = the primary's).
pub(crate) fn start(state: Arc<AppState>, options: RouterSideOptions) -> anyhow::Result<()> {
    let latest = Arc::new(Mutex::new(Latest::default()));
    let handler_state = state.clone();
    let handler_latest = latest.clone();
    let handler = EndpointHandler::new_packet_handler(
        crate::telemetry_schema::endpoint("GROUND_STATION"),
        move |packet: &Packet| {
            ingest(&handler_state, &handler_latest, packet);
            Ok(())
        },
    );
    let router = Arc::new(Router::new(
        RouterConfig::new([handler]).with_sender(crate::types::Board::GroundStation.sender_id()),
    ));
    // Receive only: whatever the shadow router would send is discarded.
    let side: RouterSideId = router.add_side_packed_with_options(
        "rrls_listener",
        |_bytes: &[u8]| Ok(()),
        RouterSideOptions {
            egress_enabled: false,
            ..options
        },
    );
    let (feed, frames) = sync_channel::<Vec<u8>>(QUEUE_FRAMES);
    if FEED.set(feed).is_err() {
        anyhow::bail!("RRLS listener already started");
    }
    std::thread::Builder::new()
        .name("rrls_listener".into())
        .spawn(move || {
            while let Ok(frame) = frames.recv() {
                if let Err(error) = router.rx_packed_from_side(&frame, side) {
                    log::debug!("RRLS listener frame not decoded: {error}");
                }
            }
        })?;
    log::info!("RRLS listener ingest started (display only, never transmits)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_per_sender_and_type_only() {
        let mut latest = Latest::default();
        assert!(latest.admit("PB", 1, 1000, 7));
        assert!(!latest.admit("PB", 1, 1000, 7), "a retransmission is shown once");
        assert!(!latest.admit("PB", 1, 999, 8), "an older frame never overwrites a newer one");
        assert!(latest.admit("PB", 1, 1001, 9));
        assert!(latest.admit("FC", 1, 10, 1), "per sender");
        assert!(latest.admit("PB", 2, 10, 2), "per type");
    }

    #[test]
    fn commands_abort_desired_state_and_variables_are_never_forwarded() {
        for name in [
            "ABORT",
            "FLIGHT_STATE",
            "BOARD_RECOVERY_REQUEST",
            "AV_BAY_UNDERGLOW",
            "FLIGHT_BUZZER",
            "HEARTBEAT",
        ] {
            assert!(!display_type(name), "{name}");
        }
        for name in ["BATTERY_VOLTAGE", "BATTERY_CURRENT", "BOARD_STATE_STATUS", "WARNING"] {
            assert!(display_type(name), "{name}");
        }
    }

    #[test]
    fn addressed_reliable_display_data_is_forwarded_and_untargeted_data_is_not() {
        crate::telemetry_schema::initialize().unwrap();
        let ty = crate::telemetry_schema::data_type("BATTERY_VOLTAGE");
        let battery = Packet::new(
            ty,
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "PB",
            1,
            vec![0; 4].into(),
        )
        .unwrap();
        assert_eq!(forwardable(&battery), sedsnet::is_reliable_type(ty));
        assert!(sedsnet::is_reliable_type(ty), "battery is addressed reliable data");
        let imu = crate::telemetry_schema::data_type("IMU_DATA");
        assert!(!sedsnet::is_reliable_type(imu), "untargeted data reaches the RRLS router itself");
    }
}
