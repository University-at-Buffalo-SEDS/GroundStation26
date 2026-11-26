//! Received frames the router's ingress queue could not take at that moment.
//!
//! The SDK's ingress queue is a try-lock (ISR-safe): a push made while another
//! thread pops answers "rx queue busy", and a full queue answers "shared
//! priority queue saturated". The frame was read off the device already, so
//! returning that error dropped it. Instead each link keeps such frames in a
//! small FIFO, offers them first (in order) on its next frame or pass, and the
//! reading thread never waits: a stalled reader would overrun the device
//! buffer, which loses more. Nothing here changes what is sent or when.
use sedsnet::TelemetryError;
use sedsnet::router::{Router, RouterSideId};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// Frames one link holds before it drops the oldest ordinary one.
pub(crate) const HOLD_FRAMES: usize = 64;

#[derive(Debug)]
struct Held {
    bytes: Vec<u8>,
    rx_ns: Option<u64>,
    /// ABORT or the Abort's FLIGHT_STATE=15: never dropped here.
    abort: bool,
}

#[derive(Default, Debug)]
pub(crate) struct Hold {
    frames: VecDeque<Held>,
    /// Ordinary frames dropped because the hold was full.
    pub(crate) dropped: u64,
}

/// An ingress refusal that admitted nothing and passes: try again later.
pub(crate) fn transient(error: &TelemetryError) -> bool {
    matches!(error, TelemetryError::Io("rx queue busy" | "shared priority queue saturated"))
}

/// Whether a received frame (or any record of a wire bundle) is Abort traffic.
pub(crate) fn is_abort_traffic(payload: &[u8]) -> bool {
    let one = |frame: &[u8]| {
        sedsnet::wire_format::peek_frame_info_unchecked(frame)
            .is_ok_and(|info| info.envelope.ty == crate::telemetry_schema::types::ABORT)
            || super::is_abort_flight_state(frame)
    };
    if sedsnet::wire_bundle::is_bundle(payload) {
        sedsnet::wire_bundle::unpack_bundle_frames(payload)
            .unwrap_or_default()
            .iter()
            .any(|frame| one(frame))
    } else {
        one(payload)
    }
}

impl Hold {
    pub(crate) fn len(&self) -> usize {
        self.frames.len()
    }

    /// Offers the held frames, oldest first, until one is refused as busy.
    /// A frame refused for any other reason is dropped (as before: it would
    /// never be taken) and its error returned after the pass.
    fn drain(&mut self, enqueue: &mut dyn FnMut(&[u8], Option<u64>) -> Result<(), TelemetryError>) -> Result<(), TelemetryError> {
        let mut failure = Ok(());
        while let Some(front) = self.frames.front() {
            match enqueue(&front.bytes, front.rx_ns) {
                Err(error) if transient(&error) => break,
                Err(error) => {
                    self.frames.pop_front();
                    failure = Err(error);
                }
                Ok(()) => {
                    self.frames.pop_front();
                }
            }
        }
        failure
    }

    fn keep(&mut self, bytes: &[u8], rx_ns: Option<u64>) {
        let abort = is_abort_traffic(bytes);
        if self.frames.len() >= HOLD_FRAMES {
            if let Some(oldest) = self.frames.iter().position(|held| !held.abort) {
                self.frames.remove(oldest);
                self.dropped += 1;
                log::warn!("radio ingress hold full: dropped the oldest ordinary frame");
            } else if !abort {
                // Full of Abort traffic: this ordinary frame is the one dropped.
                self.dropped += 1;
                log::warn!("radio ingress hold full of Abort traffic: dropped an ordinary frame");
                return;
            }
            // Abort traffic is kept even past the bound (a handful at most).
        }
        self.frames.push_back(Held { bytes: bytes.to_vec(), rx_ns, abort });
    }

    /// One received frame: the held ones go first, then this one; any the
    /// queue refuses as busy is kept, in order, for the next pass. Never waits.
    pub(crate) fn offer(
        &mut self,
        bytes: &[u8],
        rx_ns: Option<u64>,
        enqueue: &mut dyn FnMut(&[u8], Option<u64>) -> Result<(), TelemetryError>,
    ) -> Result<(), TelemetryError> {
        let held = self.drain(enqueue);
        if !self.frames.is_empty() {
            self.keep(bytes, rx_ns);
            return held;
        }
        match enqueue(bytes, rx_ns) {
            Err(error) if transient(&error) => {
                self.keep(bytes, rx_ns);
                held
            }
            result => held.and(result),
        }
    }

    /// A pass with no new frame: offer what is held.
    pub(crate) fn pass(&mut self, enqueue: &mut dyn FnMut(&[u8], Option<u64>) -> Result<(), TelemetryError>) -> Result<(), TelemetryError> {
        self.drain(enqueue)
    }
}

/// The hold of each (router, side). An emptied hold is removed.
static HOLDS: Mutex<Option<HashMap<(usize, RouterSideId), Hold>>> = Mutex::new(None);

fn enqueue_on(router: &Router, side: RouterSideId) -> impl FnMut(&[u8], Option<u64>) -> Result<(), TelemetryError> + '_ {
    move |bytes, rx_ns| match rx_ns {
        Some(rx_ns) => router.rx_packed_queue_from_side_at(bytes, side, rx_ns),
        None => router.rx_packed_queue_from_side(bytes, side),
    }
}

fn with_hold<T>(router: &Router, side: RouterSideId, f: impl FnOnce(&mut Hold) -> T) -> T {
    let key = (router as *const Router as usize, side);
    let mut holds = HOLDS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = holds.get_or_insert_with(HashMap::new);
    let hold = map.entry(key).or_default();
    let result = f(hold);
    if hold.len() == 0 {
        map.remove(&key);
    }
    result
}

/// Queues a received frame on `side`, keeping it (and its receive stamp) for
/// the next pass if the ingress queue is busy or full. Never waits.
pub(crate) fn queue(router: &Router, side: RouterSideId, bytes: &[u8], rx_ns: Option<u64>) -> Result<(), TelemetryError> {
    let mut enqueue = enqueue_on(router, side);
    with_hold(router, side, |hold| hold.offer(bytes, rx_ns, &mut enqueue))
}

/// Offers the frames `side` holds, if any. Called on every receive pass.
pub(crate) fn pass(router: &Router, side: RouterSideId) -> Result<(), TelemetryError> {
    let key = (router as *const Router as usize, side);
    if !HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|map| map.contains_key(&key))
    {
        return Ok(());
    }
    let mut enqueue = enqueue_on(router, side);
    with_hold(router, side, |hold| hold.pass(&mut enqueue))
}

/// `pass` at the start of a link's receive pass; a held frame the router
/// refuses for good is dropped and logged, as it was when first received.
pub(crate) fn pass_logged(router: &Router, side: RouterSideId) {
    if let Err(error) = pass(router, side) {
        log::warn!("held radio frame refused by the router: {error:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sedsnet::packet::Packet;
    use std::sync::Arc;

    fn frame(ty: &str, payload: &[u8], stamp: u64) -> Vec<u8> {
        crate::telemetry_schema::initialize().unwrap();
        sedsnet::wire_format::pack_packet(
            &Packet::new(
                crate::telemetry_schema::data_type(ty),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "VB",
                stamp,
                Arc::from(payload),
            )
            .unwrap(),
        )
        .to_vec()
    }

    /// A queue that is busy for the next `busy` offers, recording what it took.
    struct Queue {
        busy: usize,
        taken: Vec<Vec<u8>>,
    }
    impl Queue {
        fn enqueue(&mut self) -> impl FnMut(&[u8], Option<u64>) -> Result<(), TelemetryError> + '_ {
            move |bytes, _| {
                if self.busy > 0 {
                    self.busy -= 1;
                    return Err(TelemetryError::Io("rx queue busy"));
                }
                self.taken.push(bytes.to_vec());
                Ok(())
            }
        }
    }

    /// A frame refused as busy is delivered on the next pass, before and in
    /// order with frames received after it; no offer ever waits.
    #[test]
    fn a_frame_refused_as_busy_is_delivered_later_in_order() {
        let (a, b, c) = (frame("BATTERY_VOLTAGE", &[1, 0, 0, 0], 1), frame("BATTERY_VOLTAGE", &[2, 0, 0, 0], 2), frame("BATTERY_VOLTAGE", &[3, 0, 0, 0], 3));
        let mut hold = Hold::default();
        let mut queue = Queue { busy: 1, taken: Vec::new() };
        let started = std::time::Instant::now();
        hold.offer(&a, None, &mut queue.enqueue()).unwrap();
        assert_eq!(hold.len(), 1, "busy: kept, not dropped");
        assert!(queue.taken.is_empty());
        queue.busy = 1;
        hold.offer(&b, None, &mut queue.enqueue()).unwrap();
        assert_eq!(hold.len(), 2, "a later frame waits behind the held one");
        hold.offer(&c, None, &mut queue.enqueue()).unwrap();
        assert_eq!(queue.taken, vec![a.clone(), b.clone(), c.clone()], "in order");
        assert_eq!(hold.len(), 0);
        queue.busy = 1;
        hold.offer(&a, None, &mut queue.enqueue()).unwrap();
        hold.pass(&mut queue.enqueue()).unwrap();
        assert_eq!(queue.taken.len(), 4, "a pass with no new frame delivers the held one");
        assert!(started.elapsed() < std::time::Duration::from_millis(500), "no offer waits");
    }

    /// Full: the oldest ordinary frame goes, Abort traffic never does.
    #[test]
    fn overflow_drops_the_oldest_ordinary_frame_and_never_abort_traffic() {
        let abort = frame("ABORT", b"SEDS_MANUAL_ABORT_V1:T", 1);
        let aborted_state = frame("FLIGHT_STATE", &[15], 2);
        assert!(is_abort_traffic(&abort) && is_abort_traffic(&aborted_state));
        let ordinary = |i: u32| frame("BATTERY_VOLTAGE", &i.to_le_bytes(), 10 + u64::from(i));
        assert!(!is_abort_traffic(&ordinary(0)));
        let mut hold = Hold::default();
        let mut queue = Queue { busy: usize::MAX, taken: Vec::new() };
        hold.offer(&abort, None, &mut queue.enqueue()).unwrap();
        for i in 0..HOLD_FRAMES as u32 {
            hold.offer(&ordinary(i), None, &mut queue.enqueue()).unwrap();
        }
        assert_eq!(hold.len(), HOLD_FRAMES, "bounded");
        assert_eq!(hold.dropped, 1);
        assert!(hold.frames.iter().any(|h| h.bytes == abort), "the Abort is kept");
        assert!(!hold.frames.iter().any(|h| h.bytes == ordinary(0)), "the oldest ordinary frame went");
        hold.offer(&aborted_state, None, &mut queue.enqueue()).unwrap();
        assert!(hold.frames.iter().any(|h| h.bytes == aborted_state), "Abort traffic is never dropped");
        // A hold of nothing but Abort traffic keeps it all, even past the bound.
        let mut full = Hold::default();
        for i in 0..HOLD_FRAMES as u64 + 2 {
            full.offer(&frame("ABORT", b"SEDS_MANUAL_ABORT_V1:T", 100 + i), None, &mut queue.enqueue()).unwrap();
        }
        assert_eq!(full.len(), HOLD_FRAMES + 2);
        full.offer(&ordinary(99), None, &mut queue.enqueue()).unwrap();
        assert_eq!(full.len(), HOLD_FRAMES + 2, "the ordinary frame is the one dropped");
        queue.busy = 0;
        hold.pass(&mut queue.enqueue()).unwrap();
        assert_eq!(queue.taken.first(), Some(&abort), "held frames leave oldest first");
    }

    /// Through the real SDK with a free ingress queue: the frame is taken at
    /// once and the link keeps no hold.
    #[test]
    fn a_free_router_takes_the_frame_and_nothing_is_held() {
        crate::telemetry_schema::initialize().unwrap();
        let router = Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
        let side = router.add_side_packed("radio", |_| Ok(()));
        let bytes = frame("BATTERY_VOLTAGE", &[7, 0, 0, 0], 5);
        queue(&router, side, &bytes, None).unwrap();
        pass(&router, side).unwrap();
        let key = (&router as *const Router as usize, side);
        assert!(!HOLDS.lock().unwrap().as_ref().is_some_and(|m| m.contains_key(&key)));
    }
}
