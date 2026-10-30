use super::*;
use crate::comms::{CommsDevice, OrdinaryLaneState, MAX_ORDINARY_SERVICE_CHARGE};

#[test]
fn completed_byte_balance_is_bounded_and_does_not_bank_while_peer_blocked() {
    let mut service = OrdinaryServiceBalance::default();
    assert_eq!(std::mem::size_of::<OrdinaryServiceBalance>(), 4);
    service.observe(OrdinaryLaneState::Ready);
    assert!(!service.allow_mesh(true));
    service.sdk_complete(220, OrdinaryLaneState::Ready);
    assert!(service.allow_mesh(true));
    service.mesh_complete(108, true);
    service.mesh_complete(108, true);
    assert_eq!(service.bytes, 4);
    service.mesh_complete(108, true);
    assert_eq!(service.bytes, -104); // exactly one completed owner may cross zero.
    assert!(!service.allow_mesh(true));
    service.sdk_complete(220, OrdinaryLaneState::Ready);
    let owed = service.bytes;
    service.observe(OrdinaryLaneState::Blocked);
    service.sdk_complete(220, OrdinaryLaneState::Blocked);
    service.mesh_complete(108, false); // SDK temporarily not ready; no credit bank.
    assert_eq!(service.bytes, owed);
    service.sdk_complete(0, OrdinaryLaneState::Ready);
    service.mesh_complete(0, true);
    assert_eq!(service.bytes, owed); // no successful ordinary bytes: no debit.
    service.sdk_complete(usize::MAX, OrdinaryLaneState::Ready);
    assert_eq!(service.bytes, MAX_ORDINARY_SERVICE_CHARGE as i32);
    service.mesh_complete(usize::MAX, true);
    assert_eq!(service.bytes, -(MAX_ORDINARY_SERVICE_CHARGE as i32));
    service.observe(OrdinaryLaneState::Empty);
    assert_eq!(service.bytes, 0);
    service.sdk_complete(220, OrdinaryLaneState::Ready);
    service.observe(OrdinaryLaneState::Ready);
    assert_eq!(service.bytes, 220, "a ready transport with a gap before the next SDK cursor offer must not erase earned Mesh history");
}

#[derive(Default)]
struct GenericCountedDevice { calls: usize, bytes: usize, fail: bool }
impl CommsDevice for GenericCountedDevice {
    fn recv_packet(&mut self, _: &Router, _: &mut dyn FnMut(&sedsnet::packet::Packet)) -> sedsnet::TelemetryResult<()> { Ok(()) }
    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.calls += 1;
        if self.fail { return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into()); }
        self.bytes += payload.len();
        Ok(())
    }
    fn set_side_id(&mut self, _: sedsnet::router::RouterSideId) {}
}

#[test]
fn unsupported_counted_send_has_already_sent_exactly_once_and_preserves_errors() {
    let mut device = GenericCountedDevice::default();
    assert_eq!(device.ordinary_background_state(), None);
    assert_eq!(device.send_tracked_counted(&[(0, b"opaque")], &[]).unwrap(), None);
    assert_eq!((device.calls, device.bytes), (1, 6));
    device.fail = true;
    assert!(device.send_tracked_counted(&[(0, b"refused")], &[]).is_err());
    assert_eq!((device.calls, device.bytes), (2, 6));
}
