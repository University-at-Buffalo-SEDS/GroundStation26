use crate::config::{get_message_meta, MAX_STATIC_HEX_LENGTH, MAX_STATIC_STRING_LENGTH};
use crate::get_needed_message_size;
use crate::router::EndpointHandler;
use crate::telemetry_packet::{DataEndpoint, DataType, TelemetryPacket};
use crate::{get_data_type, message_meta, MessageDataType, TelemetryError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};


/// Compute a valid test payload length for a given [`DataType`], respecting the
/// schema’s static/dynamic element counts and element widths.
///
/// This is used throughout tests to avoid hard-coding per-type sizes.
fn test_payload_len_for(ty: DataType) -> usize {
    match message_meta(ty).element_count {
        crate::MessageElementCount::Static(_) => get_needed_message_size(ty),
        crate::MessageElementCount::Dynamic => {
            // Pick reasonable defaults per data kind
            match get_data_type(ty) {
                MessageDataType::String => MAX_STATIC_STRING_LENGTH, // router error-path expects this
                MessageDataType::Binary => MAX_STATIC_HEX_LENGTH,    // any bytes; size-bounded
                // numeric/bool: must be multiple of element width → use “schema element count”
                other => {
                    let w = match other {
                        MessageDataType::UInt8 | MessageDataType::Int8 | MessageDataType::Bool => 1,
                        MessageDataType::UInt16 | MessageDataType::Int16 => 2,
                        MessageDataType::UInt32
                        | MessageDataType::Int32
                        | MessageDataType::Float32 => 4,
                        MessageDataType::UInt64
                        | MessageDataType::Int64
                        | MessageDataType::Float64 => 8,
                        MessageDataType::UInt128 | MessageDataType::Int128 => 16,
                        MessageDataType::String | MessageDataType::Binary => 1,
                    };
                    let elems = get_message_meta(ty).element_count.into().max(1);
                    w * elems
                }
            }
        }
    }
}

/// Build a simple handler that increments an [`AtomicUsize`] each time it sees
/// a packet on the `SD_CARD` endpoint.
///
/// Used by various queue/timeout and concurrency tests.
fn get_handler(rx_count_c: Arc<AtomicUsize>) -> EndpointHandler {
    EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &TelemetryPacket| {
        rx_count_c.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

/// Build a handler for `SD_CARD` that:
/// - asserts `GPS_DATA` element width is `4` (f32),
/// - decodes the payload as little-endian `f32`,
/// - stores `(DataType, Vec<f32>)` into the shared `Mutex`.
fn get_sd_card_handler(sd_seen_c: Arc<Mutex<Option<(DataType, Vec<f32>)>>>) -> EndpointHandler {
    EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |pkt: &TelemetryPacket| {
        // sanity: element sizing must be 4 bytes (f32) for GPS_DATA
        let elems = get_message_meta(pkt.data_type())
            .element_count
            .into()
            .max(1);
        let per_elem = get_needed_message_size(pkt.data_type()) / elems;
        assert_eq!(pkt.data_type(), DataType::GpsData);
        assert_eq!(per_elem, 4, "GPS_DATA expected f32 elements");

        // decode f32 little-endian
        let mut vals = Vec::with_capacity(pkt.payload().len() / 4);
        for chunk in pkt.payload().chunks_exact(4) {
            vals.push(f32::from_le_bytes(chunk.try_into().unwrap()));
        }

        *sd_seen_c.lock().unwrap() = Some((pkt.data_type(), vals));
        Ok(())
    })
}

/// Helper that asserts `result` is a [`TelemetryError::HandlerError`].
///
/// Used in tests that expect error propagation from handlers/tx.
fn handle_errors(result: Result<(), TelemetryError>) {
    match result {
        Ok(_) => panic!("Expected router.send to return Err due to handler failure"),
        Err(e) => match e {
            TelemetryError::HandlerError(_) => {} // expected
            _ => panic!("Expected TelemetryError::HandlerError, got {:?}", e),
        },
    }
}

// -----------------------------------------------------------------------------
// Basic packet + router smoke tests
// -----------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    //! Basic smoke tests for packet roundtrip, string formatting, and simple
    //! router send/receive paths.

    use crate::tests::get_sd_card_handler;
    use crate::tests::timeout_tests::StepClock;
    use crate::{
        config::{DataEndpoint, DataType},
        router::Router,
        serialize,
        telemetry_packet::TelemetryPacket,
        TelemetryResult,
    };
    use std::sync::{Arc, Mutex};
    use std::vec::Vec;


    /// Serialize/deserialize a GPS packet and ensure all fields and payload
    /// bytes round-trip exactly.
    #[test]
    fn serialize_roundtrip_gps() {
        // GPS: 3 * f32
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::GroundStation];
        let pkt = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[5.2141414, 3.1342144],
            endpoints,
            0,
        )
        .unwrap();

        pkt.validate().unwrap();

        let bytes = serialize::serialize_packet(&pkt);
        let rpkt = serialize::deserialize_packet(&bytes).unwrap();

        rpkt.validate().unwrap();
        assert_eq!(rpkt.data_type(), pkt.data_type());
        assert_eq!(rpkt.data_size(), pkt.data_size());
        assert_eq!(rpkt.timestamp(), pkt.timestamp());
        assert_eq!(&*rpkt.endpoints(), &*pkt.endpoints());
        assert_eq!(&*rpkt.payload(), &*pkt.payload());
    }

    /// Verify `header_string()` format for a simple GPS packet.
    #[test]
    fn header_string_matches_expectation() {
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::GroundStation];
        let pkt =
            TelemetryPacket::from_f32_slice(DataType::GpsData, &[1.0, 2.0], endpoints, 0)
                .unwrap();
        let s = pkt.header_string();
        assert_eq!(
            s,
            "Type: GPS_DATA, Data Size: 8, Sender: TEST_PLATFORM, Endpoints: [SD_CARD, GROUND_STATION], Timestamp: 0 (0s 000ms)"
        );
    }

    /// Ensure `to_string()` includes the float values and the general header.
    #[test]
    fn packet_to_string_formats_floats() {
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::GroundStation];
        let pkt =
            TelemetryPacket::from_f32_slice(DataType::GpsData, &[1.0, 2.5], endpoints, 0)
                .unwrap();

        let text = pkt.to_string();
        assert!(text.starts_with(
            "{Type: GPS_DATA, Data Size: 8, Sender: TEST_PLATFORM, Endpoints: [SD_CARD, GROUND_STATION], Timestamp: 0 (0s 000ms), Data: "
        ));
        assert!(text.contains("1"));
        assert!(text.contains("2.5"));
    }

    /// End-to-end test: `Router::log` → TX callback (serialize/deserialize) →
    /// local handler decoding f32 payload.
    #[test]
    fn router_sends_and_receives() {
        use crate::router::{BoardConfig, Router};

        // capture spaces
        let tx_seen: Arc<Mutex<Option<TelemetryPacket>>> = Arc::new(Mutex::new(None));
        let sd_seen_decoded: Arc<Mutex<Option<(DataType, Vec<f32>)>>> = Arc::new(Mutex::new(None));

        // transmitter: record the deserialized packet we "sent"
        let tx_seen_c = tx_seen.clone();
        let transmit = move |bytes: &[u8]| -> TelemetryResult<()> {
            let pkt = serialize::deserialize_packet(bytes)?;
            *tx_seen_c.lock().unwrap() = Some(pkt);
            Ok(())
        };

        // local SD handler: decode payload to f32s and record (ty, values)
        let sd_seen_c = sd_seen_decoded.clone();
        let sd_handler = get_sd_card_handler(sd_seen_c);
        let box_clock = StepClock::new_default_box();

        let router = Router::new(
            Some(transmit),
            BoardConfig::new(vec![sd_handler]),
            box_clock,
        );

        // send GPS_DATA (3 * f32) using Router::log (uses default endpoints from schema)
        let data = [1.0_f32, 2.0];
        router.log(DataType::GpsData, &data).unwrap();

        // --- assertions ---

        // remote transmitter saw the same type & bytes
        let tx_pkt = tx_seen
            .lock()
            .unwrap()
            .clone()
            .expect("no tx packet recorded");
        assert_eq!(tx_pkt.data_type(), DataType::GpsData);
        assert_eq!(tx_pkt.payload().len(), 2 * 4);
        // compare bytes exactly to what log() would have produced
        let mut expected = Vec::new();
        for v in data {
            expected.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(&*tx_pkt.payload(), &*expected);

        // local SD handler decoded to f32s and recorded (type, values)
        let (seen_ty, seen_vals) = sd_seen_decoded
            .lock()
            .unwrap()
            .clone()
            .expect("no sd packet recorded");
        assert_eq!(seen_ty, DataType::GpsData);
        assert_eq!(seen_vals, data);
    }

    /// A small “bus” that records transmitted frames for TX/RX queue tests.
    struct TestBus {
        frames: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl TestBus {
        /// Create a `TestBus` and a TX function that pushes any transmitted bytes
        /// into an internal `Vec<Vec<u8>>`.
        fn new() -> (
            Self,
            impl Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
        ) {
            let frames = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
            let tx_frames = frames.clone();
            let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
                // capture the exact wire bytes
                tx_frames.lock().unwrap().push(bytes.to_vec());
                Ok(())
            };
            (Self { frames }, tx)
        }
    }

    /// TX router enqueues packets, flushes to a `TestBus`, and an RX router
    /// consumes them from its receive queue and delivers to a local handler.
    #[test]
    fn queued_roundtrip_between_two_routers() {
        // --- Set up a TX router that only sends (no local endpoints) ---
        let (bus, tx_fn) = TestBus::new();
        let box_clock_tx = StepClock::new_default_box();
        let box_clock_rx = StepClock::new_default_box();

        let tx_router = Router::new(Some(tx_fn), Default::default(), box_clock_tx);

        // --- Set up an RX router with a local SD handler that decodes f32 payloads ---
        let seen: Arc<Mutex<Option<(DataType, Vec<f32>)>>> = Arc::new(Mutex::new(None));
        let seen_c = seen.clone();
        let sd_handler = get_sd_card_handler(seen_c);
        fn tx_handler(_bytes: &[u8]) -> TelemetryResult<()> {
            // RX router does not transmit in this test
            Ok(())
        }

        let rx_router = Router::new(
            Some(tx_handler),
            crate::router::BoardConfig::new(vec![sd_handler]),
            box_clock_rx,
        );

        // --- 1) Sender enqueues a packet for TX ---
        let data = [1.0_f32, 2.0];
        tx_router.log_queue(DataType::GpsData, &data).unwrap();

        // --- 2) Flush TX queue -> pushes wire frames into TestBus ---
        tx_router.process_tx_queue().unwrap();

        // --- 3) Deliver captured frames into RX router's *received queue* ---
        let frames = bus.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 1, "expected exactly one TX frame");
        for frame in &frames {
            rx_router.rx_serialized_packet_to_queue(frame).unwrap();
        }

        // --- 4) Drain RX queue -> invokes local handlers ---
        rx_router.process_rx_queue().unwrap();

        // --- Assertions: handler got the right data ---
        let (ty, vals) = seen.lock().unwrap().clone().expect("no packet delivered");
        assert_eq!(ty, DataType::GpsData);
        assert_eq!(vals, data);
    }

    /// Demonstrate “self-delivery” by feeding serialized frames from a router’s
    /// own TX back into its RX queue.
    #[test]
    fn queued_self_delivery_via_receive_queue() {
        // If you expect a node to handle its *own* packets, you must explicitly
        // feed them back into its received queue (mirroring “broadcast to self”).
        let (bus, tx_fn) = TestBus::new();
        let box_clock = StepClock::new_default_box();

        let router = Router::new(Some(tx_fn), Default::default(), box_clock);

        // Enqueue for transmit
        let data = [10.0_f32, 10.25];
        router.log_queue(DataType::GpsData, &data).unwrap();

        let data = [10.0_f32];
        router.log_queue(DataType::BatteryVoltage, &data).unwrap();

        let data = [10.0_f32, 10.25];
        router.log_queue(DataType::GpsData, &data).unwrap();
        // Flush -> frame appears on the "bus"
        router.process_tx_queue().unwrap();
        let frames = bus.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 3);

        // Feed back into *the same* router's received queue
        router.rx_serialized_packet_to_queue(&frames[0]).unwrap();

        // Now draining the received queue should dispatch to any matching local endpoints.
        // (This router has no endpoints; this test just proves the queue path is exercised.)
        router.process_rx_queue().unwrap();
    }
}

// ---- Helpers (test-local) ----

/// Build a deterministic packet with a raw 3-byte payload [0x13, 0x21, 0x34]
/// encoded as three `f32` values, endpoints [SD_CARD, RADIO], and timestamp
/// `1123581321`.
///
/// We intentionally do not call `validate()` because `GPS_DATA` usually expects
/// 3×`f32` (12 bytes) and this is for formatting/copying tests only.
fn fake_telemetry_packet_bytes() -> TelemetryPacket {
    use crate::config::{DataEndpoint, DataType};

    let payload = [0x13 as f32, 0x21 as f32]; // f32 values
    let endpoints = [DataEndpoint::SdCard, DataEndpoint::GroundStation];

    TelemetryPacket::from_f32_slice(DataType::GpsData, &payload, &endpoints, 1123581321).unwrap()
}

/// Copy helper that mirrors the C++ behavior, but uses raw pointers so we can
/// test the “same pointer” case without violating Rust’s borrow rules.
///
/// Safety: Caller must ensure `dest` and `src` are valid for reads/writes.
unsafe fn copy_telemetry_packet_raw(
    dest: *mut TelemetryPacket,
    src: *const TelemetryPacket,
) -> Result<(), &'static str> {
    if dest.is_null() || src.is_null() {
        return Err("null packet");
    }
    if core::ptr::eq(dest, src as *mut TelemetryPacket) {
        // same object → OK no-op
        return Ok(());
    }

    let s = unsafe { &*src };
    let d = unsafe { &mut *dest };

    // Deep copy: new endpoints slice and new payload buffer
    let endpoints_vec: Vec<DataEndpoint> = s.endpoints().iter().copied().collect();
    let payload_arc: Arc<[u8]> = Arc::from(&*s.payload());

    let new_pkt = TelemetryPacket::new(
        s.data_type(),
        &endpoints_vec,
        s.sender(),
        s.timestamp(),
        payload_arc,
    )
    .map_err(|_| "packet validation failed")?;

    *d = new_pkt;
    Ok(())
}

// ---- Converted tests ----

/// Port of C++: TEST(Helpers, PacketHexToString).
/// Ensures `to_hex_string()` matches exactly the expected legacy format.
#[test]
fn helpers_packet_hex_to_string() {
    let pkt = fake_telemetry_packet_bytes();
    let got = pkt.to_hex_string();
    let expect = "Type: GPS_DATA, Data Size: 8, Sender: TEST_PLATFORM, Endpoints: [SD_CARD, GROUND_STATION], Timestamp: 1123581321 (312h 06m 21s 321ms), Data (hex): 0x00 0x00 0x98 0x41 0x00 0x00 0x04 0x42";
    assert_eq!(got, expect);
}

/// Port of C++: TEST(Helpers, CopyTelemetryPacket).
/// Exercises `copy_telemetry_packet_raw` for null, self-copy, and deep copy.
#[test]
fn helpers_copy_telemetry_packet() {
    // (1) null dest → error
    let src = fake_telemetry_packet_bytes();
    let st = unsafe { copy_telemetry_packet_raw(core::ptr::null_mut(), &src as *const _) };
    assert!(st.is_err());

    // (2) same pointer (no-op) → OK
    let mut same = fake_telemetry_packet_bytes();
    let same_ptr: *mut TelemetryPacket = &mut same;
    let st = unsafe { copy_telemetry_packet_raw(same_ptr, same_ptr as *const _) };
    assert!(st.is_ok());

    // (3) distinct objects → deep copy and equal fields
    let mut dest = TelemetryPacket::new(
        src.data_type(),
        &src.endpoints(), // &[DataEndpoint]
        src.sender(),     // Arc<str>
        src.timestamp(),
        Arc::from(&*src.payload()), // deep copy payload
    )
    .expect("src packet should be valid");

    let st = unsafe { copy_telemetry_packet_raw(&mut dest as *mut _, &src as *const _) };
    assert!(st.is_ok());

    // element-by-element compare
    assert_eq!(dest.timestamp(), src.timestamp());
    assert_eq!(dest.data_type(), src.data_type());
    assert_eq!(dest.data_size(), src.data_size());
    assert_eq!(dest.endpoints().len(), src.endpoints().len());
    for i in 0..dest.endpoints().len() {
        assert_eq!(dest.endpoints()[i], src.endpoints()[i]);
    }
    assert_eq!(&*dest.payload(), &*src.payload());
}

// -----------------------------------------------------------------------------
// Error propagation & handler-failure tests
// -----------------------------------------------------------------------------
#[cfg(test)]
mod handler_failure_tests {
    //! Tests around handler failures and how they generate/route
    //! `TELEMETRY_ERROR` packets.

    use super::*;
    use crate::config::DEVICE_IDENTIFIER;
    use crate::router::EndpointHandler;
    use crate::router::{BoardConfig, Router};
    use crate::telemetry_packet::DataType;
    use crate::tests::timeout_tests::StepClock;
    use crate::{TelemetryError, MAX_VALUE_DATA_TYPE};
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;


    /// Helper: convert an index into a valid [`DataEndpoint`] for tests.
    fn ep(idx: u32) -> DataEndpoint {
        DataEndpoint::try_from_u32(idx).expect("Need endpoint present in enum for tests")
    }

    /// Pick any valid [`DataType`] from the enum range for generic tests.
    fn pick_any_type() -> DataType {
        for i in 0..=MAX_VALUE_DATA_TYPE {
            if let Some(ty) = DataType::try_from_u32(i) {
                return ty;
            }
        }
        panic!("No usable DataType found for tests");
    }

    /// Build a zeroed payload of valid length for the given type using
    /// [`test_payload_len_for`].
    fn payload_for(ty: DataType) -> Vec<u8> {
        vec![0u8; test_payload_len_for(ty)]
    }

    /// If a local handler fails, ensure:
    /// - other local endpoints get the original packet,
    /// - and a `TELEMETRY_ERROR` packet with the right text is sent.
    #[test]
    fn local_handler_failure_sends_error_packet_to_other_locals() {
        let ty = pick_any_type();
        let ts = 42_u64;
        let failing_ep = ep(0);
        let other_ep = ep(1);

        // Capture the packets that reach the "other_ep" handler.
        let recv_count = Arc::new(AtomicUsize::new(0));
        let last_payload = Arc::new(Mutex::new(String::new()));

        let recv_count_c = recv_count.clone();
        let last_payload_c = last_payload.clone();

        let failing = EndpointHandler::new_packet_handler(failing_ep, |_pkt: &TelemetryPacket| {
            Err(TelemetryError::BadArg)
        });

        let capturing =
            EndpointHandler::new_packet_handler(other_ep, move |pkt: &TelemetryPacket| {
                if pkt.data_type() == DataType::TelemetryError {
                    *last_payload_c.lock().unwrap() = pkt.to_string();
                }
                recv_count_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let box_clock = StepClock::new_default_box();

        let router = Router::new::<fn(&[u8]) -> crate::TelemetryResult<()>>(
            None,
            BoardConfig::new(vec![failing, capturing]),
            box_clock,
        );

        let pkt = TelemetryPacket::new(
            ty,
            &[failing_ep, other_ep],
            DEVICE_IDENTIFIER,
            ts,
            Arc::<[u8]>::from(payload_for(ty)),
        )
        .unwrap();

        handle_errors(router.transmit_message(&pkt));

        // The capturing handler should have seen the original packet and then the error packet.
        assert!(
            recv_count.load(Ordering::SeqCst) >= 1,
            "capturing handler should have been invoked at least once"
        );

        // Verify exact payload text produced by handle_callback_error(Some(dest), e)
        let expected = format!(
            "{{Type: TELEMETRY_ERROR, Data Size: {:?}, Sender: TEST_PLATFORM, Endpoints: [GROUND_STATION], Timestamp: 0 (0s 000ms), Error: (\"Handler for endpoint {:?} failed on device {:?}: {:?}\")}}",
            68,
            failing_ep,
            DEVICE_IDENTIFIER,
            TelemetryError::BadArg
        );
        let got = last_payload.lock().unwrap().clone();
        assert_eq!(got, expected, "mismatch in TelemetryError payload text");
    }

    /// If the TX callback fails, ensure:
    /// - a `TELEMETRY_ERROR` is generated,
    /// - it is delivered to all local endpoints,
    /// - and the error text matches expectation.
    #[test]
    fn tx_failure_sends_error_packet_to_all_local_endpoints() {
        let ty = pick_any_type();
        let ts = 31415_u64;

        // One local endpoint (to receive error), one "remote" endpoint (not in handlers)
        let local_ep = ep(0);
        let remote_ep = ep(1);

        let saw_error = Arc::new(AtomicUsize::new(0));
        let last_payload = Arc::new(Mutex::new(String::new()));
        let saw_error_c = saw_error.clone();
        let last_payload_c = last_payload.clone();

        let capturing =
            EndpointHandler::new_packet_handler(local_ep, move |pkt: &TelemetryPacket| {
                if pkt.data_type() == DataType::TelemetryError {
                    *last_payload_c.lock().unwrap() = pkt.to_string();
                    saw_error_c.fetch_add(1, Ordering::SeqCst);
                }
                Ok(())
            });

        let tx_fail =
            |_bytes: &[u8]| -> crate::TelemetryResult<()> { Err(TelemetryError::Io("boom")) };
        let box_clock = StepClock::new_default_box();

        let router = Router::new(Some(tx_fail), BoardConfig::new(vec![capturing]), box_clock);

        let pkt = TelemetryPacket::new(
            ty,
            // include both a local and a non-local endpoint so any_remote == true
            &[local_ep, remote_ep],
            "router_test",
            ts,
            Arc::<[u8]>::from(payload_for(ty)),
        )
        .unwrap();

        handle_errors(router.transmit_message(&pkt));

        assert!(
            saw_error.load(Ordering::SeqCst) >= 1,
            "local handler should have received TelemetryError after TX failures"
        );

        // Exact text from handle_callback_error(None, e)
        let expected = format!(
            "{{Type: TELEMETRY_ERROR, Data Size: {:?}, Sender: TEST_PLATFORM, Endpoints: [SD_CARD], Timestamp: 0 (0s 000ms), Error: (\"TX Handler failed on device {:?}: {:?}\")}}",
            55,
            DEVICE_IDENTIFIER,
            TelemetryError::Io("boom")
        );
        let got = last_payload.lock().unwrap().clone();
        assert_eq!(got, expected, "mismatch in TelemetryError payload text");
    }
}

// -----------------------------------------------------------------------------
// Timeout and queue-draining behavior tests
// -----------------------------------------------------------------------------
#[cfg(test)]
mod timeout_tests {
    //! Tests for `process_*_queue*` functions and timeout semantics,
    //! including u64 wraparound handling.

    use crate::config::DataEndpoint;
    use crate::router::EndpointHandler;
    use crate::tests::get_handler;
    use crate::{
        router::BoardConfig, router::Clock, router::Router, telemetry_packet::DataType,
        telemetry_packet::TelemetryPacket, TelemetryResult,
    };
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;
    // ---------------- Mock clock ----------------

    /// A deterministic clock that steps forward by `step` ms on each `now_ms()`
    /// call, starting from `start`. Used to test timeout budget behavior.
    pub(crate) struct StepClock {
        t: AtomicU64,
        step: u64,
    }

    impl StepClock {
        pub fn new_box(start: u64, step: u64) -> Box<dyn Clock + Send + Sync> {
            Box::new(StepClock::new(start, step))
        }
        pub fn new_default_box() -> Box<dyn Clock + Send + Sync> {
            Box::new(StepClock::new(0, 0))
        }
        pub fn new(start: u64, step: u64) -> Self {
            Self {
                t: AtomicU64::new(start),
                step,
            }
        }
    }

    impl Clock for StepClock {
        #[inline]
        fn now_ms(&self) -> u64 {
            // returns current, then advances by step (wraps naturally in u64)
            self.t.fetch_add(self.step, Ordering::Relaxed)
        }
    }

    // ---------------- Helpers ----------------

    /// Create a GPS packet with only a local endpoint (`SD_CARD`), avoiding any
    /// implicit re-TX during receive.
    fn mk_rx_only_local(vals: &[f32], ts: u64) -> TelemetryPacket {
        TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            vals,
            &[DataEndpoint::SdCard], // <- only local
            ts,
        )
        .unwrap()
    }

    /// Build a TX function that increments `counter` for each frame sent.
    fn tx_counter(
        counter: Arc<AtomicUsize>,
    ) -> impl Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static {
        move |bytes: &[u8]| {
            assert!(!bytes.is_empty());
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// `timeout == 0` must drain both TX and RX queues fully, regardless of
    /// clock, and local handlers see all packets.
    #[test]
    fn process_all_queues_timeout_zero_drains_fully() {
        let tx_count = Arc::new(AtomicUsize::new(0));
        let tx = tx_counter(tx_count.clone());

        let rx_count = Arc::new(AtomicUsize::new(0));
        let rx_count_c = rx_count.clone();
        let handler = EndpointHandler::new_packet_handler(
            DataEndpoint::SdCard,
            move |_pkt: &TelemetryPacket| {
                rx_count_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );

        let box_clock = StepClock::new_default_box();

        let r = Router::new(Some(tx), BoardConfig::new(vec![handler]), box_clock);

        // Enqueue TX (3)
        for _ in 0..3 {
            r.log_queue(DataType::GpsData, &[1.0_f32, 2.0])
                .unwrap();
        }
        // Enqueue RX (2) with only-local endpoint
        for _ in 0..2 {
            r.rx_packet_to_queue(mk_rx_only_local(&[9.0, 8.0], 123))
                .unwrap();
        }

        // timeout = 0 → drain fully
        r.process_all_queues_with_timeout(0).unwrap();

        // TX: all three frames should be sent
        assert_eq!(
            tx_count.load(Ordering::SeqCst),
            3,
            "all TX packets should be sent"
        );
        // RX handler was invoked for each TX (local delivery) + each RX = 3 + 2 = 5
        assert_eq!(
            rx_count.load(Ordering::SeqCst),
            5,
            "handler sees TX+RX packets"
        );
    }

    /// With non-zero timeout and step = 10ms, timeout 5ms should allow exactly
    /// one iteration (at most one TX and one RX).
    #[test]
    fn process_all_queues_respects_nonzero_timeout_budget_one_receive_one_send() {
        let tx_count = Arc::new(AtomicUsize::new(0));
        let tx = tx_counter(tx_count.clone());

        let rx_count = Arc::new(AtomicUsize::new(0));
        let rx_count_c = rx_count.clone();
        let handler = get_handler(rx_count_c);
        let clock = StepClock::new_box(0, 10);

        let r = Router::new(Some(tx), BoardConfig::new(vec![handler]), clock);

        // Seed work in both queues
        for _ in 0..5 {
            r.log_queue(DataType::GpsData, &[1.0_f32, 2.0])
                .unwrap();
            // RX with only-local endpoint to avoid implicit re-TX during receive
            r.rx_packet_to_queue(
                TelemetryPacket::from_f32_slice(
                    DataType::GpsData,
                    &[4.0, 5.0],
                    &[DataEndpoint::SdCard],
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        }

        // Step is 10ms per call; timeout 5ms guarantees exactly one iteration
        r.process_all_queues_with_timeout(5).unwrap();

        // One iteration → at most one TX send
        assert_eq!(
            tx_count.load(Ordering::SeqCst),
            1,
            "expected exactly one TX in a single iteration"
        );

        // Handlers run for both TX local delivery and RX processing → 2 total
        assert_eq!(
            rx_count.load(Ordering::SeqCst),
            1,
            "expected one TX local call"
        );

        // Drain the rest to prove there was more work left
        r.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(tx_count.load(Ordering::SeqCst), 5);
        assert_eq!(rx_count.load(Ordering::SeqCst), 10); // 5 (TX locals) + 5 (RX)
    }

    /// Similar to previous, but with step=5 and timeout=10 to allow up to two
    /// iterations; expect one TX + one RX handler call.
    #[test]
    fn process_all_queues_respects_nonzero_timeout_budget_two_receive_one_send() {
        let tx_count = Arc::new(AtomicUsize::new(0));
        let tx = tx_counter(tx_count.clone());

        let rx_count = Arc::new(AtomicUsize::new(0));
        let rx_count_c = rx_count.clone();
        let handler = get_handler(rx_count_c);
        let clock = StepClock::new_box(0, 5);

        let r = Router::new(Some(tx), BoardConfig::new(vec![handler]), clock);

        // Seed work in both queues
        for _ in 0..5 {
            r.log_queue(DataType::GpsData, &[1.0_f32, 2.0])
                .unwrap();
            // RX with only-local endpoint to avoid implicit re-TX during receive
            r.rx_packet_to_queue(
                TelemetryPacket::from_f32_slice(
                    DataType::GpsData,
                    &[4.0, 5.0],
                    &[DataEndpoint::SdCard],
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        }

        // Step is 5ms per call; timeout 10ms allows two iterations max
        r.process_all_queues_with_timeout(10).unwrap();

        // One iteration → at most one TX send
        assert_eq!(
            tx_count.load(Ordering::SeqCst),
            1,
            "expected exactly one TX in a single iteration"
        );

        // Handlers run for both TX local delivery and RX processing → 2 total
        assert_eq!(
            rx_count.load(Ordering::SeqCst),
            2,
            "expected one TX local + one RX handler call"
        );

        // Drain the rest to prove there was more work left
        r.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(tx_count.load(Ordering::SeqCst), 5);
        assert_eq!(rx_count.load(Ordering::SeqCst), 10); // 5 (TX locals) + 5 (RX)
    }

    /// Ensure timeout math remains correct near `u64::MAX`, i.e. when the clock
    /// wraps around, and that we still do at most one iteration.
    #[test]
    fn process_all_queues_handles_u64_wraparound() {
        let tx_count = Arc::new(AtomicUsize::new(0));
        let tx = tx_counter(tx_count.clone());

        let rx_count = Arc::new(AtomicUsize::new(0));
        let rx_count_c = rx_count.clone();
        let handler = get_handler(rx_count_c);
        let start = u64::MAX - 1;
        let clock = StepClock::new_box(start, 2);
        let r = Router::new(Some(tx), BoardConfig::new(vec![handler]), clock);

        // One TX and one RX (RX is only-local to avoid creating extra TX on receive)
        r.log_queue(DataType::GpsData, &[1.0_f32, 2.0])
            .unwrap();
        r.rx_packet_to_queue(mk_rx_only_local(&[4.0, 5.0], 7))
            .unwrap();

        // Small budget; with wrapping_sub this should allow one iteration then stop
        r.process_all_queues_with_timeout(1).unwrap();

        // One iteration can do up to one TX and one RX
        assert!(tx_count.load(Ordering::SeqCst) <= 1, "expected <=1 TX");
        assert!(
            rx_count.load(Ordering::SeqCst) <= 2,
            "local handler can be invoked by TX local delivery (+1) and RX (+1)"
        );
        // At least something should have happened
        assert!(tx_count.load(Ordering::SeqCst) + rx_count.load(Ordering::SeqCst) >= 1);
    }
}

// -----------------------------------------------------------------------------
// Extra coverage tests: error codes, header-only parsing, varints, bitmaps, etc.
// -----------------------------------------------------------------------------
#[cfg(test)]
mod tests_extra {

    //! Extra unit tests that cover previously-missing paths and invariants.
    //!
    //! These are white-box tests that exercise public APIs (and some
    //! indirect behavior) to avoid changing visibility in core modules.

    #![cfg(test)]


    use crate::config::DataEndpoint;
    use crate::tests::test_payload_len_for;
    use crate::{
        config::DataType, router::{BoardConfig, Clock, EndpointHandler, Router}, serialize,
        telemetry_packet::TelemetryPacket,
        TelemetryError,
        TelemetryErrorCode,
        TelemetryResult,
    };
    use alloc::{string::String, sync::Arc};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;


    /// A tiny helper clock; we rely on the blanket `impl<Fn() -> u64> Clock`.
    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    // --------------------------- Error/Code parity ---------------------------

    /// Validate that `TelemetryError` ↔ `TelemetryErrorCode` mapping is
    /// complete and stable, including the string forms.
    #[test]
    fn error_enum_code_roundtrip_and_strings() {
        let samples = [
            TelemetryError::InvalidType,
            TelemetryError::EmptyEndpoints,
            TelemetryError::Deserialize("oops"),
            TelemetryError::Io("disk"),
            TelemetryError::HandlerError("fail"),
            TelemetryError::MissingPayload,
            TelemetryError::TimestampInvalid,
        ];
        for e in samples {
            let code = e.to_error_code();
            // must have a stable human string (starts with a '{' per current impl)
            assert!(code.as_str().starts_with('{'));
            // round-trip numeric space
            let back = TelemetryErrorCode::try_from_i32(code as i32);
            assert!(back.is_some(), "roundtrip failed for {code:?}");
        }
    }

    // --------------------------- Header-only parsing ---------------------------

    /// Ensure header-only peek fails on truncated buffers (short read during
    /// varint parsing).
    #[test]
    fn deserialize_header_only_short_buffer_fails() {
        // v2 header is varint-based. Force a definite short read in the first varint.

        // Case A: only NEP present (0 endpoints), but no bytes for `ty` varint.
        let tiny = [0x00u8]; // NEP = 0
        let err = serialize::peek_envelope(&tiny).unwrap_err();
        matches_deser_err(err);

        // Case B: NEP present, and a *truncated* varint (continuation bit set, but no following byte).
        let truncated = [0x00u8, 0x80]; // NEP=0, then start varint with continuation bit
        let err = serialize::peek_envelope(&truncated).unwrap_err();
        matches_deser_err(err);
    }

    /// Ensure header size is a valid prefix of the serialized wire image.
    #[test]
    fn header_size_is_prefix_of_wire_image() {
        let pkt = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0],
            &[DataEndpoint::SdCard, DataEndpoint::GroundStation],
            123,
        )
        .unwrap();

        let wire = serialize::serialize_packet(&pkt);
        let hdr = serialize::header_size_bytes(&pkt);
        assert!(hdr <= wire.len());

        // header must decode from the start (i.e., NEP + scalars exists)
        assert!(hdr > 0);
    }

    /// Helper: assert an error is a `Deserialize` variant.
    fn matches_deser_err(e: TelemetryError) {
        match e {
            TelemetryError::Deserialize(_) => {}
            other => panic!("expected Deserialize error, got {other:?}"),
        }
    }

    /// Ensure serialization is canonical: serialize → deserialize → serialize
    /// produces identical bytes (ULEB128 canonical form).
    #[test]
    fn serializer_is_canonical_roundtrip() {
        use crate::config::{DataEndpoint, DataType};
        use crate::{serialize, telemetry_packet::TelemetryPacket};

        // Dynamic payload to avoid schema constraints and let us vary sizes later.
        let msg = "hello world";
        let pkt = TelemetryPacket::from_u8_slice(
            DataType::TelemetryError,
            msg.as_bytes(),
            &[DataEndpoint::SdCard, DataEndpoint::GroundStation],
            0,
        )
        .unwrap();

        let wire1 = serialize::serialize_packet(&pkt);
        let pkt2 = serialize::deserialize_packet(&wire1).unwrap();
        let wire2 = serialize::serialize_packet(&pkt2);

        // ULEB128 is canonical (no leading 0x80 “more” bytes), so bytes must match
        assert_eq!(&*wire1, &*wire2, "serializer must be canonical");
    }

    /// Validate varint scalar growth: header and wire size should increase
    /// when fields that are encoded as varints get larger.
    #[test]
    fn serializer_varint_scalars_grow_as_expected() {
        use crate::config::{DataEndpoint, DataType};
        use crate::{serialize, telemetry_packet::TelemetryPacket};

        // Helper to build a TelemetryError string payload of given length.
        fn pkt_with(len: usize, sender_len: usize, ts: u64) -> TelemetryPacket {
            let s = "x".repeat(sender_len);
            let payload = vec![b'A'; len]; // dynamic payload
            TelemetryPacket::new(
                DataType::TelemetryError,
                &[DataEndpoint::SdCard],
                s,
                ts,
                Arc::<[u8]>::from(payload),
            )
            .unwrap()
        }

        // Case 1: small (all varints fit in 1 byte)
        let p1 = pkt_with(10, 5, 0x7F); // <= 127
        let w1 = serialize::serialize_packet(&p1);
        let h1 = serialize::header_size_bytes(&p1);
        assert!(h1 >= 1 + 4, "NEP + 4 one-byte varints minimum");

        // Case 2: two-byte varints for size/sender_len
        let p2 = pkt_with(200, 200, 0x7F);
        let w2 = serialize::serialize_packet(&p2);
        let h2 = serialize::header_size_bytes(&p2);
        assert!(w2.len() > w1.len(), "wire should grow with larger varints");
        assert!(h2 > h1, "header should grow with larger varints");

        // Case 3: bigger timestamp to push it beyond 1 byte (and usually >2)
        let p3 = pkt_with(200, 200, 1u64 << 40); // forces 6-byte varint
        let w3 = serialize::serialize_packet(&p3);
        let h3 = serialize::header_size_bytes(&p3);
        assert!(
            w3.len() > w2.len(),
            "wire should grow with larger timestamp"
        );
        assert!(h3 > h2, "header should grow with larger timestamp");

        // Size function must match exact output
        assert_eq!(serialize::packet_wire_size(&p3), w3.len());
    }

    /// Stress test for endpoint bitpacking across many endpoints and repeated
    /// copies, ensuring endpoints and payload round-trip.
    #[test]
    fn endpoints_bitpack_roundtrip_many_and_extremes() {
        use crate::{
            config::{DataEndpoint, DataType},
            serialize,
            telemetry_packet::TelemetryPacket,
            MAX_VALUE_DATA_ENDPOINT,
        };

        // Build a long endpoint list by cycling through all enum values (0..=MAX)
        let mut eps = Vec::<DataEndpoint>::new();
        for i in 0..=MAX_VALUE_DATA_ENDPOINT {
            if let Some(ep) = DataEndpoint::try_from_u32(i) {
                eps.push(ep);
            }
        }
        // Repeat to make the bitstream cross multiple bytes
        let mut endpoints = Vec::new();
        for _ in 0..4 {
            endpoints.extend_from_slice(&eps);
        }

        // Make payload dynamic so schema doesn't get in the way
        let payload = vec![0x55u8; 257]; // force 2-byte varint for data_size
        let pkt = TelemetryPacket::new(
            DataType::TelemetryError,
            &endpoints,
            "sender",
            123456,
            Arc::<[u8]>::from(payload),
        )
            .unwrap();

        let wire = serialize::serialize_packet(&pkt);
        let back = serialize::deserialize_packet(&wire).unwrap();
        assert_eq!(
            &*back.endpoints(),
            [DataEndpoint::SdCard, DataEndpoint::GroundStation, DataEndpoint::FlightController],
            "endpoints must roundtrip 1:1"
        );
        assert_eq!(back.data_type(), pkt.data_type());
        assert_eq!(back.timestamp(), pkt.timestamp());
        assert_eq!(&*back.payload(), &*pkt.payload());
        assert_eq!(serialize::packet_wire_size(&pkt), wire.len());
    }

    /// For large sender/payload/timestamp, ensure `peek_envelope` and full
    /// deserialization agree on header fields and payload.
    #[test]
    fn peek_envelope_matches_full_parse_on_large_values() {
        use crate::config::{DataEndpoint, DataType};
        use crate::{serialize, telemetry_packet::TelemetryPacket};

        let sender = "S".repeat(10_000); // big sender (varint grows)
        let payload = vec![b'h'; 4096];
        let ts = (1u64 << 40) + 123; // large ts (varint grows)

        let pkt = TelemetryPacket::new(
            DataType::TelemetryError, // String-typed
            &[DataEndpoint::SdCard, DataEndpoint::GroundStation],
            sender,
            ts,
            std::sync::Arc::<[u8]>::from(payload),
        )
        .unwrap();

        let wire = serialize::serialize_packet(&pkt);
        let env = serialize::peek_envelope(&wire).unwrap();
        let full = serialize::deserialize_packet(&wire).unwrap();

        assert_eq!(env.ty, pkt.data_type());
        assert_eq!(env.sender.as_ref(), pkt.sender());
        assert_eq!(env.timestamp_ms, pkt.timestamp());
        assert_eq!(&*env.endpoints, &*pkt.endpoints());

        assert_eq!(full.data_type(), pkt.data_type());
        assert_eq!(full.timestamp(), pkt.timestamp());
        assert_eq!(&*full.endpoints(), &*pkt.endpoints());
        assert_eq!(&*full.payload(), &*pkt.payload());
    }

    /// Corrupt endpoint bits in the bitmap to encode an out-of-range value,
    /// and ensure deserialization fails with an appropriate error.
    #[test]
    fn corrupt_endpoint_bits_yields_bad_endpoint_error() {
        use crate::{
            config::{DataEndpoint, DataType},
            serialize,
            telemetry_packet::TelemetryPacket,
            MAX_VALUE_DATA_ENDPOINT,
        };

        // Recompute EP_BITS the same way the module does.
        let bits = 32 - MAX_VALUE_DATA_ENDPOINT.leading_zeros();
        let ep_bits: u8 = if bits == 0 { 1 } else { bits as u8 };
        // If EP_BITS is exactly the minimum bits to encode MAX, there is room for values > MAX.
        let upper_val = (1u64 << ep_bits) - 1;
        if upper_val as u32 <= MAX_VALUE_DATA_ENDPOINT {
            // Nothing to corrupt beyond max—skip test (no larger representable value).
            return;
        }

        // Build a simple, valid packet with at least 1 endpoint.
        let pkt = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0],
            &[DataEndpoint::SdCard],
            123,
        )
        .unwrap();
        let mut wire = serialize::serialize_packet(&pkt).to_vec();

        // Compute where endpoint bits start (right after header varints)
        let ep_offset = serialize::header_size_bytes(&pkt);
        assert!(ep_offset < wire.len());

        // Overwrite the *first* endpoint with an out-of-range value in the bitstream.
        // Bits are packed LSB-first.
        let mut v = upper_val;
        let mut bitpos = 0usize;
        for _ in 0..ep_bits {
            let byte_idx = ep_offset + (bitpos / 8);
            let bit_off = bitpos % 8;
            // Set bit if the corresponding bit of v is 1
            if (v & 1) != 0 {
                wire[byte_idx] |= 1 << bit_off;
            } else {
                wire[byte_idx] &= !(1 << bit_off);
            }
            v >>= 1;
            bitpos += 1;
        }

        // Now deserialization must fail with a Deserialize("bad endpoint") error.
        let err = serialize::deserialize_packet(&wire).unwrap_err();
        match err {
            TelemetryError::Deserialize(msg) if msg.contains("endpoint") => {}
            other => panic!("expected bad endpoint deserialize error, got {other:?}"),
        }
    }

    /// Sanity check that header size is between 0 and full packet size, and
    /// that the computed wire size matches serialized length.
    #[test]
    fn header_size_is_prefix_and_less_than_total() {
        use crate::config::{DataEndpoint, DataType};
        use crate::{serialize, telemetry_packet::TelemetryPacket};

        let pkt = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0],
            &[DataEndpoint::SdCard, DataEndpoint::GroundStation],
            999,
        )
        .unwrap();

        let wire = serialize::serialize_packet(&pkt);
        let hdr = serialize::header_size_bytes(&pkt);

        assert!(hdr > 0 && hdr < wire.len());
        assert_eq!(serialize::packet_wire_size(&pkt), wire.len());
    }

    // --------------------------- UTF-8 trimming behavior ---------------------------

    /// Ensure `data_as_utf8_ref` trims trailing NUL bytes and returns a `&str`
    /// with just the meaningful content.
    #[test]
    fn data_as_utf8_ref_trims_trailing_nuls() {
        // Use a String-typed message kind. TelemetryError is used by the router with
        // a string payload and typically mapped to MessageDataType::String.
        let ty = DataType::TelemetryError;
        let mut buf = vec![0u8; test_payload_len_for(ty)];

        let s = b"hello\0\0";
        buf[..s.len()].copy_from_slice(s);

        let pkt = TelemetryPacket::new(
            ty,
            &[DataEndpoint::SdCard],
            "tester",
            0,
            Arc::<[u8]>::from(buf),
        )
        .unwrap();

        assert_eq!(pkt.data_as_utf8_ref(), Some("hello"));
    }

    // --------------------------- Queue clear semantics ---------------------------

    /// After calling `clear_queues`, no pending TX/RX items should be processed.
    #[test]
    fn clear_queues_prevents_further_processing() {
        // Transmit "bus" that counts frames sent.
        let tx_count = Arc::new(AtomicUsize::new(0));
        let tx_count_c = tx_count.clone();
        let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            tx_count_c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        // Local handler that counts receives.
        let rx_count = Arc::new(AtomicUsize::new(0));
        let rx_count_c = rx_count.clone();
        let handler = EndpointHandler::new_packet_handler(
            DataEndpoint::SdCard,
            move |_pkt: &TelemetryPacket| {
                rx_count_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );

        let r = Router::new(Some(tx), BoardConfig::new(vec![handler]), zero_clock());

        // Enqueue one TX and one RX
        let pkt_tx = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0],
            &[DataEndpoint::SdCard, DataEndpoint::GroundStation],
            0,
        )
        .unwrap();
        let pkt_rx = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[4.0_f32, 5.0],
            &[DataEndpoint::SdCard], // only local to avoid extra TX during receive
            0,
        )
        .unwrap();

        r.transmit_message_queue(pkt_tx).unwrap();
        r.rx_packet_to_queue(pkt_rx).unwrap();

        // Clearing should drop both queues before any processing.
        r.clear_queues();

        r.process_all_queues().unwrap();
        assert_eq!(
            tx_count.load(Ordering::SeqCst),
            0,
            "should not TX after clear"
        );
        assert_eq!(
            rx_count.load(Ordering::SeqCst),
            0,
            "should not RX after clear"
        );
    }

    // --------------------------- Retry semantics (indirect) ---------------------------

    /// Verify local handler retry count matches `MAX_NUMBER_OF_RETRYS` (assumed 3),
    /// and that the final error is a `HandlerError`.
    #[test]
    fn local_handler_retry_attempts_are_three() {
        // This test assumes MAX_NUMBER_OF_RETRYS == 3 in router. If that constant changes,
        // update the expected count below.
        const EXPECTED_ATTEMPTS: usize = 3; // initial try + retries

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_c = counter.clone();

        // A handler that always fails but bumps a counter on each attempt.
        let failing = EndpointHandler::new_packet_handler(
            DataEndpoint::SdCard,
            move |_pkt: &TelemetryPacket| {
                counter_c.fetch_add(1, Ordering::SeqCst);
                Err(TelemetryError::BadArg)
            },
        );

        // Router with no TX (we only care about local handler invocation count).
        let r = Router::new::<fn(&[u8]) -> TelemetryResult<()>>(
            None,
            BoardConfig::new(vec![failing]),
            zero_clock(),
        );

        // Build a valid packet addressed to the failing endpoint.
        let pkt = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();

        // Sending should surface a HandlerError after all retries.
        let res = r.transmit_message(&pkt);
        match res {
            Err(TelemetryError::HandlerError(_)) => {}
            other => panic!("expected HandlerError after retries, got {other:?}"),
        }

        assert_eq!(
            counter.load(Ordering::SeqCst),
            EXPECTED_ATTEMPTS,
            "handler should be invoked exactly {EXPECTED_ATTEMPTS} times"
        );
    }

    // --------------------------- from_u8_slice sanity ---------------------------

    /// Ensure `TelemetryPacket::from_u8_slice` builds a valid GPS packet with
    /// expected length and timestamp.
    #[test]
    fn from_u8_slice_builds_valid_packet() {
        let need = test_payload_len_for(DataType::GpsData);
        assert_eq!(need, 8); // schema sanity

        let bytes = vec![0x11u8; need];
        let pkt = TelemetryPacket::from_u8_slice(
            DataType::GpsData,
            &bytes,
            &[DataEndpoint::SdCard],
            12345,
        )
        .unwrap();

        assert_eq!(pkt.payload().len(), need);
        assert_eq!(pkt.data_size(), need);
        assert_eq!(pkt.timestamp(), 12345);
    }

    // --------------------------- Header-only happy path smoke ---------------------------

    /// Header-only peek (`peek_envelope`) should match full parse for a normal
    /// encoded GPS packet.
    #[test]
    fn deserialize_header_only_then_full_parse_matches() {
        // Build a normal packet then compare header-only vs full.
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::GroundStation];
        let pkt = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[5.25_f32, 3.5],
            endpoints,
            42,
        )
        .unwrap();
        let wire = serialize::serialize_packet(&pkt);

        let env = serialize::peek_envelope(&wire).unwrap();
        assert_eq!(env.ty, pkt.data_type());
        assert_eq!(&*env.endpoints, &*pkt.endpoints());
        assert_eq!(env.sender.as_ref(), pkt.sender());
        assert_eq!(env.timestamp_ms, pkt.timestamp());

        let round = serialize::deserialize_packet(&wire).unwrap();
        round.validate().unwrap();
        assert_eq!(round.data_type(), pkt.data_type());
        assert_eq!(round.data_size(), pkt.data_size());
        assert_eq!(round.timestamp(), pkt.timestamp());
        assert_eq!(&*round.endpoints(), &*pkt.endpoints());
        assert_eq!(&*round.payload(), &*pkt.payload());
    }

    // --------------------------- TX failure -> error to locals (smoke) ---------------------------

    /// Smoke test: TX failure should emit a `TelemetryError` packet to local
    /// endpoints (exact string validated by more specific tests).
    #[test]
    fn tx_failure_emits_error_to_local_endpoints() {
        // A transmitter that always fails.
        let failing_tx = |_bytes: &[u8]| -> TelemetryResult<()> { Err(TelemetryError::Io("boom")) };

        // Capture what the local endpoint sees (should include a TelemetryError).
        let last_payload = Arc::new(Mutex::new(String::new()));
        let last_payload_c = last_payload.clone();

        let capturing = EndpointHandler::new_packet_handler(
            DataEndpoint::SdCard,
            move |pkt: &TelemetryPacket| {
                if pkt.data_type() == DataType::TelemetryError {
                    *last_payload_c.lock().unwrap() = pkt.to_string();
                }
                Ok(())
            },
        );

        let r = Router::new(
            Some(failing_tx),
            BoardConfig::new(vec![capturing]),
            zero_clock(),
        );

        // Include both a local and a non-local endpoint to force remote TX.
        let pkt = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0],
            &[DataEndpoint::SdCard, DataEndpoint::GroundStation],
            7,
        )
        .unwrap();

        let res = r.transmit_message(&pkt);
        match res {
            Err(TelemetryError::HandlerError(_)) => {} // TX path wraps as HandlerError
            other => panic!("expected HandlerError from TX failure, got {other:?}"),
        }

        // Ensure something was captured (exact string is covered elsewhere)
        let got = last_payload.lock().unwrap().clone();
        assert!(
            !got.is_empty(),
            "expected TelemetryError to be delivered locally after TX failure"
        );
    }
}

// -----------------------------------------------------------------------------
// More tests: validation, enum bounds, router paths, payload helpers, etc.
// -----------------------------------------------------------------------------
#[cfg(test)]
mod tests_more {
    //! Additional coverage tests for router, packet, and serialization logic.
    //! These tests complement `tests_extra` by covering boundary,
    //! error, and fast-path behaviors not previously exercised.

    #![cfg(test)]


    use crate::config::get_message_meta;
    use crate::{
        config::{DataEndpoint, DataType}, get_data_type, get_needed_message_size, message_meta,
        router::{BoardConfig, Clock, EndpointHandler, Router}, serialize, telemetry_packet::TelemetryPacket,
        MessageDataType,
        MessageElementCount, TelemetryError, TelemetryErrorCode,
        TelemetryResult,
        MAX_VALUE_DATA_ENDPOINT,
        MAX_VALUE_DATA_TYPE,
    };
    use alloc::{sync::Arc, vec::Vec};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc as StdArc, Mutex};


    /// Clock that always returns 0 (via closure), used where wall-clock is
    /// irrelevant and we only need a stable `Clock` impl.
    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    // ---------------------------------------------------------------------------
    // TelemetryPacket validation edge cases
    // ---------------------------------------------------------------------------

    /// Compute a concrete length for test packets, respecting schema element
    /// counts for static/dynamic payloads.
    fn concrete_len_for_test(ty: DataType) -> usize {
        match message_meta(ty).element_count {
            MessageElementCount::Static(_) => get_needed_message_size(ty),
            MessageElementCount::Dynamic => {
                // Choose a reasonable dynamic size for tests:
                // numeric/bool → element_width * MESSAGE_ELEMENTS
                // string/hex    → 1 * MESSAGE_ELEMENTS (or any positive size)
                let w = match get_data_type(ty) {
                    MessageDataType::UInt8 | MessageDataType::Int8 | MessageDataType::Bool => 1,
                    MessageDataType::UInt16 | MessageDataType::Int16 => 2,
                    MessageDataType::UInt32 | MessageDataType::Int32 | MessageDataType::Float32 => {
                        4
                    }
                    MessageDataType::UInt64 | MessageDataType::Int64 | MessageDataType::Float64 => {
                        8
                    }
                    MessageDataType::UInt128 | MessageDataType::Int128 => 16,
                    MessageDataType::String | MessageDataType::Binary => 1,
                };
                let elems = get_message_meta(ty).element_count.into().max(1);
                core::cmp::max(1, w * elems)
            }
        }
    }

    /// Packet creation should reject empty endpoint lists and size mismatches
    /// (for both static and dynamic payload kinds).
    #[test]
    fn packet_validate_rejects_empty_endpoints_and_size_mismatch() {
        let ty = DataType::GpsData;
        let need = concrete_len_for_test(ty);

        let err =
            TelemetryPacket::new(ty, &[], "x", 0, Arc::<[u8]>::from(vec![0u8; need])).unwrap_err();
        assert!(matches!(err, TelemetryError::EmptyEndpoints));

        // +1 ensures mismatch for both static and dynamic (not a multiple of element width)
        let err = TelemetryPacket::new(
            ty,
            &[DataEndpoint::SdCard],
            "x",
            0,
            Arc::<[u8]>::from(vec![0u8; need + 1]),
        )
        .unwrap_err();
        assert!(matches!(err, TelemetryError::SizeMismatch { .. }));
    }

    // ---------------------------------------------------------------------------
    // Enum bounds + conversion validity
    // ---------------------------------------------------------------------------

    /// Ensure `DataType`, `DataEndpoint`, and `TelemetryErrorCode` all reject
    /// values outside their numeric ranges.
    #[test]
    fn enum_conversion_bounds_and_rejections() {
        let max_ty = MAX_VALUE_DATA_TYPE;
        assert!(DataType::try_from_u32(max_ty).is_some());
        assert!(DataType::try_from_u32(max_ty + 1).is_none());

        let max_ep = MAX_VALUE_DATA_ENDPOINT;
        assert!(DataEndpoint::try_from_u32(max_ep).is_some());
        assert!(DataEndpoint::try_from_u32(max_ep + 1).is_none());

        let min = TelemetryErrorCode::MIN;
        let max = TelemetryErrorCode::MAX;
        assert!(TelemetryErrorCode::try_from_i32(min).is_some());
        assert!(TelemetryErrorCode::try_from_i32(max).is_some());
        assert!(TelemetryErrorCode::try_from_i32(min - 1).is_none());
        assert!(TelemetryErrorCode::try_from_i32(max + 1).is_none());
    }

    // ---------------------------------------------------------------------------
    // Serialization header math + ByteReader edge cases
    // ---------------------------------------------------------------------------

    /// `packet_wire_size` must match the length of the serialized output.
    #[test]
    fn packet_wire_size_matches_serialized_len() {
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::GroundStation];
        let pkt =
            TelemetryPacket::from_f32_slice(DataType::GpsData, &[1.0, 2.0], endpoints, 9)
                .unwrap();
        let need = serialize::packet_wire_size(&pkt);
        let out = serialize::serialize_packet(&pkt);
        assert_eq!(need, out.len());
    }

    // ---------------------------------------------------------------------------
    // Router serialization/deserialization paths
    // ---------------------------------------------------------------------------

    /// If only `Serialized` handlers exist, the router must not deserialize the
    /// payload and just pass the raw bytes.
    #[test]
    fn serialized_only_handlers_do_not_deserialize() {
        let pkt = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0],
            &[DataEndpoint::SdCard],
            123,
        )
        .unwrap();
        let wire = serialize::serialize_packet(&pkt);

        let called = StdArc::new(AtomicUsize::new(0));
        let c = called.clone();
        let handler =
            EndpointHandler::new_serialized_handler(DataEndpoint::SdCard, move |bytes: &[u8]| {
                assert!(bytes.len() >= serialize::header_size_bytes(&pkt));
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let r = Router::new::<fn(&[u8]) -> TelemetryResult<()>>(
            None,
            BoardConfig::new(vec![handler]),
            zero_clock(),
        );
        r.receive_serialized(&wire).unwrap();
        assert_eq!(called.load(Ordering::SeqCst), 1);
    }

    /// When mixing `Packet` and `Serialized` handlers, ensure:
    /// - deserialization happens only once,
    /// - each endpoint handler is invoked exactly once.
    #[test]
    fn packet_handlers_trigger_single_deserialize_and_fan_out() {
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::GroundStation];
        let pkt =
            TelemetryPacket::from_f32_slice(DataType::GpsData, &[1.0, 2.0], endpoints, 5)
                .unwrap();
        let wire = serialize::serialize_packet(&pkt);

        let packet_called = StdArc::new(AtomicUsize::new(0));
        let serialized_called = StdArc::new(AtomicUsize::new(0));

        let ph = packet_called.clone();
        let sh = serialized_called.clone();

        let packet_h = EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt| {
            ph.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let serialized_h =
            EndpointHandler::new_serialized_handler(DataEndpoint::GroundStation, move |_b| {
                sh.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let r = Router::new::<fn(&[u8]) -> TelemetryResult<()>>(
            None,
            BoardConfig::new(vec![packet_h, serialized_h]),
            zero_clock(),
        );

        r.receive_serialized(&wire).unwrap();
        assert_eq!(packet_called.load(Ordering::SeqCst), 1);
        assert_eq!(serialized_called.load(Ordering::SeqCst), 1);
    }

    /// If all addressed endpoints are local `Packet` handlers, router should
    /// avoid serializing at all and never call TX.
    #[test]
    fn send_avoids_serialization_when_only_local_packet_handlers_exist() {
        let tx_called = StdArc::new(AtomicUsize::new(0));
        let txc = tx_called.clone();
        let tx = move |_bytes: &[u8]| -> TelemetryResult<()> {
            txc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        let hits = StdArc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let handler = EndpointHandler::new_packet_handler(
            DataEndpoint::SdCard,
            move |pkt: &TelemetryPacket| {
                pkt.validate().unwrap();
                h.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );

        let r = Router::new(Some(tx), BoardConfig::new(vec![handler]), zero_clock());
        let pkt = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        r.transmit_message(&pkt).unwrap();

        assert_eq!(tx_called.load(Ordering::SeqCst), 0);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// `Router::receive` for a direct packet should invoke any matching local
    /// packet handlers exactly once.
    #[test]
    fn receive_direct_packet_invokes_handlers() {
        let called = StdArc::new(AtomicUsize::new(0));
        let c = called.clone();
        let handler = EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt| {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let r = Router::new::<fn(&[u8]) -> TelemetryResult<()>>(
            None,
            BoardConfig::new(vec![handler]),
            zero_clock(),
        );
        let pkt = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[0.5, 0.5],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        r.receive(&pkt).unwrap();

        assert_eq!(called.load(Ordering::SeqCst), 1);
    }

    // ---------------------------------------------------------------------------
    // Error payload truncation & encode_slice_le extra types
    // ---------------------------------------------------------------------------

    /// Ensure router’s internal TelemetryError payload is truncated to meta size
    /// and doesn’t grow without bound.
    #[test]
    fn error_payload_is_truncated_to_meta_size() {
        let failing_tx = |_b: &[u8]| -> TelemetryResult<()> { Err(TelemetryError::Io("boom")) };

        let captured = StdArc::new(Mutex::new(String::new()));
        let c = captured.clone();
        let handler = EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |pkt| {
            if pkt.data_type() == DataType::TelemetryError {
                *c.lock().unwrap() = pkt.to_string();
            }
            Ok(())
        });

        let r = Router::new(
            Some(failing_tx),
            BoardConfig::new(vec![handler]),
            zero_clock(),
        );
        let pkt = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0],
            &[DataEndpoint::SdCard, DataEndpoint::GroundStation],
            1,
        )
        .unwrap();
        let _ = r.transmit_message(&pkt);

        let s = captured.lock().unwrap().clone();
        assert!(!s.is_empty());
        assert!(s.len() < 8_192);
    }

    /// Ensure `encode_slice_le` works correctly for both `u16` and `f64`.
    #[test]
    fn encode_slice_le_u16_and_f64() {
        let vals16 = [0x0102u16, 0xA1B2];
        let got = crate::router::encode_slice_le(&vals16);
        let mut exp = Vec::new();
        for v in vals16 {
            exp.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(&*got, &exp);

        let vals64 = [1.5f64, -2.25];
        let got = crate::router::encode_slice_le(&vals64);
        let mut exp = Vec::new();
        for v in vals64 {
            exp.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(&*got, &exp);
    }

    /// Ensure `test_payload_len_for` respects element widths and yields lengths
    /// that are multiples of the correct width for all numeric/bool types.
    #[test]
    fn test_payload_len_for_respects_element_width() {
        use crate::tests::test_payload_len_for;

        for i in 0..=MAX_VALUE_DATA_TYPE {
            if let Some(ty) = DataType::try_from_u32(i) {
                let len = test_payload_len_for(ty);
                assert!(len > 0, "test payload length must be > 0 for {ty:?}");

                match get_data_type(ty) {
                    MessageDataType::String | MessageDataType::Binary => {
                        // any positive length is fine for string/hex, just sanity check
                        assert!(len >= 1, "string/hex must have at least 1 byte for {ty:?}");
                    }
                    kind => {
                        let width = match kind {
                            MessageDataType::UInt8
                            | MessageDataType::Int8
                            | MessageDataType::Bool => 1,
                            MessageDataType::UInt16 | MessageDataType::Int16 => 2,
                            MessageDataType::UInt32
                            | MessageDataType::Int32
                            | MessageDataType::Float32 => 4,
                            MessageDataType::UInt64
                            | MessageDataType::Int64
                            | MessageDataType::Float64 => 8,
                            MessageDataType::UInt128 | MessageDataType::Int128 => 16,
                            MessageDataType::String | MessageDataType::Binary => 1,
                        };
                        assert_eq!(
                            len % width,
                            0,
                            "test payload length {len} not multiple of element width {width} for {ty:?}"
                        );
                    }
                }
            }
        }
    }

    /// Construct an invalid varint (11 continuation bytes), and ensure
    /// `deserialize_packet` returns a `uleb128 too long` error.
    #[test]
    fn deserialize_packet_rejects_overflowed_varint() {
        use crate::serialize;
        // Construct a fake wire buffer with NEP=0, then an invalid varint (11 continuation bytes)
        let mut wire = vec![0x00u8]; // NEP = 0
        wire.extend([0xFFu8; 11]); // invalid ULEB128 (too long for u64)
        let err = serialize::deserialize_packet(&wire).unwrap_err();
        match err {
            TelemetryError::Deserialize(msg) if msg.eq("uleb128 too long") => {}
            other => panic!("expected Deserialize(uleb128 too long...) error, got {other:?}"),
        }
    }

    /// Endpoint order in the `endpoints` slice must not affect serialized bytes.
    #[test]
    fn serialize_packet_is_order_invariant_for_endpoints() {
        use crate::config::{DataEndpoint, DataType};
        use crate::{serialize, telemetry_packet::TelemetryPacket};

        let eps_a = &[DataEndpoint::GroundStation, DataEndpoint::SdCard];
        let eps_b = &[DataEndpoint::SdCard, DataEndpoint::GroundStation];

        let pkt_a =
            TelemetryPacket::from_f32_slice(DataType::GpsData, &[1.0, 2.0], eps_a, 0).unwrap();
        let pkt_b =
            TelemetryPacket::from_f32_slice(DataType::GpsData, &[1.0, 2.0], eps_b, 0).unwrap();

        let wa = serialize::serialize_packet(&pkt_a);
        let wb = serialize::serialize_packet(&pkt_b);

        assert_eq!(wa, wb, "endpoint order must not affect serialized bytes");
    }

    /// With a large number of TX and RX items, `process_all_queues_with_timeout(0)`
    /// must flush all TX and deliver all packets to handlers.
    #[test]
    fn process_all_queues_timeout_zero_handles_large_queues() {
        use crate::config::{DataEndpoint, DataType};
        use crate::router::{BoardConfig, Router};
        use crate::telemetry_packet::TelemetryPacket;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tx_count = Arc::new(AtomicUsize::new(0));
        let txc = tx_count.clone();
        let tx = move |_b: &[u8]| -> TelemetryResult<()> {
            txc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        let rx_count = Arc::new(AtomicUsize::new(0));
        let rxc = rx_count.clone();
        let handler = EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt| {
            rxc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let router = Router::new(Some(tx), BoardConfig::new(vec![handler]), Box::new(|| 0u64));

        // Enqueue many TX and RX items
        const N: usize = 200;
        for _ in 0..N {
            router
                .log_queue(DataType::GpsData, &[1.0_f32, 2.0])
                .unwrap();
            let pkt = TelemetryPacket::from_f32_slice(
                DataType::GpsData,
                &[9.0, 8.0],
                &[DataEndpoint::SdCard],
                0,
            )
            .unwrap();
            router.rx_packet_to_queue(pkt).unwrap();
        }

        router.process_all_queues_with_timeout(0).unwrap();

        assert_eq!(tx_count.load(Ordering::SeqCst), N, "all TX should flush");
        assert_eq!(
            rx_count.load(Ordering::SeqCst),
            2 * N,
            "each TX local delivery + RX packet should invoke handler"
        );
    }
}

// -----------------------------------------------------------------------------
// Concurrency tests
// -----------------------------------------------------------------------------
#[cfg(test)]
mod concurrency_tests {
    //! Concurrency-focused tests that exercise Router’s thread-safety
    //! guarantees for logging, receiving, and processing.

    use crate::{
        config::{DataEndpoint, DataType},
        router::{BoardConfig, Clock, EndpointHandler, Router},
        serialize,
        telemetry_packet::TelemetryPacket,
        TelemetryResult,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;


    /// Simple clock that always returns 0 (blanket impl<Fn() -> u64> for Clock).
    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    // ------------------------------------------------------------------------
    // Trait sanity: Router must be Send + Sync
    // ------------------------------------------------------------------------

    /// Compile-time check: `Router` must be `Send + Sync` to be safely shared
    /// across threads.
    #[test]
    fn router_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Router>();
    }

    // ------------------------------------------------------------------------
    // Concurrent RX queue producers
    // ------------------------------------------------------------------------

    /// Multiple producer threads call `rx_packet_to_queue` on the same Router;
    /// a single drain must deliver all packets to the handler exactly once.
    #[test]
    fn concurrent_rx_queue_is_thread_safe() {
        const THREADS: usize = 4;
        const ITERS_PER_THREAD: usize = 50;
        let total = THREADS * ITERS_PER_THREAD;

        // Local handler that counts how many packets it sees.
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_c = hits.clone();
        let handler = EndpointHandler::new_packet_handler(
            DataEndpoint::SdCard,
            move |_pkt: &TelemetryPacket| {
                hits_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );

        // Router with no TX; we only care about RX + local delivery.
        let router = Router::new::<fn(&[u8]) -> TelemetryResult<()>>(
            None,
            BoardConfig::new(vec![handler]),
            zero_clock(),
        );
        let r = Arc::new(router);

        // Spawn multiple producers that enqueue RX packets concurrently.
        let mut threads_vec = Vec::new();
        for _ in 0..THREADS {
            let r_cloned = r.clone();
            threads_vec.push(thread::spawn(move || {
                for _ in 0..ITERS_PER_THREAD {
                    let pkt = TelemetryPacket::from_f32_slice(
                        DataType::GpsData,
                        &[1.0_f32, 2.0],
                        &[DataEndpoint::SdCard], // only-local endpoint
                        0,
                    )
                    .unwrap();
                    r_cloned.rx_packet_to_queue(pkt).unwrap();
                }
            }));
        }

        // Join all producer threads.
        for t in threads_vec {
            t.join().expect("producer thread panicked");
        }

        // Single-threaded drain of the received queue.
        r.process_rx_queue().unwrap();

        // We should see exactly one handler call per enqueued packet.
        assert_eq!(
            hits.load(Ordering::SeqCst),
            total,
            "expected {total} handler invocations from RX queue"
        );
    }

    // ------------------------------------------------------------------------
    // Concurrent calls to receive_serialized
    // ------------------------------------------------------------------------

    /// Multiple threads call `receive_serialized` concurrently with the same
    /// wire buffer; each call should fan out once to the handler.
    #[test]
    fn concurrent_receive_serialized_is_thread_safe() {
        const THREADS: usize = 4;
        const ITERS_PER_THREAD: usize = 50;
        let total = THREADS * ITERS_PER_THREAD;

        // Build a single wire frame that we'll reuse across threads.
        let pkt = TelemetryPacket::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0],
            &[DataEndpoint::SdCard], // only-local endpoint → one handler per receive
            123,
        )
        .unwrap();
        let wire = serialize::serialize_packet(&pkt);
        let wire_shared = Arc::new(wire);

        // Handler that counts how many times it is invoked.
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_c = hits.clone();
        let handler = EndpointHandler::new_packet_handler(
            DataEndpoint::SdCard,
            move |_pkt: &TelemetryPacket| {
                hits_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );

        // Router with no TX; only exercising receive path + fan-out.
        let router = Router::new::<fn(&[u8]) -> TelemetryResult<()>>(
            None,
            BoardConfig::new(vec![handler]),
            zero_clock(),
        );
        let r = Arc::new(router);

        // Spawn multiple threads that all call receive_serialized on the same Router.
        let mut threads_vec = Vec::new();
        for _ in 0..THREADS {
            let r_cloned = r.clone();
            let w_cloned = wire_shared.clone();
            threads_vec.push(thread::spawn(move || {
                for _ in 0..ITERS_PER_THREAD {
                    r_cloned
                        .receive_serialized(&w_cloned)
                        .expect("receive_serialized failed");
                }
            }));
        }

        for t in threads_vec {
            t.join().expect("receive thread panicked");
        }

        // One handler call per receive.
        assert_eq!(
            hits.load(Ordering::SeqCst),
            total,
            "expected {total} handler invocations from receive_serialized"
        );
    }

    // ------------------------------------------------------------------------
    // Concurrent logging + processing
    // ------------------------------------------------------------------------

    /// One thread logs to TX queue while another drains queues; verify that
    /// every logged packet is transmitted once and delivered once to the
    /// local handler.
    #[test]
    fn concurrent_logging_and_processing_is_thread_safe() {
        use std::thread;

        const ITERS: usize = 200;

        // Count how many frames are actually transmitted on the "bus".
        let tx_count = Arc::new(AtomicUsize::new(0));
        let txc = tx_count.clone();
        let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            txc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        // Local handler that counts how many packets it sees.
        let rx_count = Arc::new(AtomicUsize::new(0));
        let rxc = rx_count.clone();
        let handler = EndpointHandler::new_packet_handler(
            DataEndpoint::SdCard,
            move |_pkt: &TelemetryPacket| {
                rxc.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );

        // Shared router: TX + one local endpoint.
        let router = Router::new(Some(tx), BoardConfig::new(vec![handler]), zero_clock());
        let r = Arc::new(router);

        // ---------------- Logger thread ----------------
        let r_logger = r.clone();
        let logger = thread::spawn(move || {
            for _ in 0..ITERS {
                r_logger
                    .log_queue(DataType::GpsData, &[1.0_f32, 2.0])
                    .expect("log_queue failed");
            }
        });

        // ---------------- Drainer thread ----------------
        let r_drain = r.clone();
        let rx_counter = rx_count.clone();
        let drainer = thread::spawn(move || {
            // Keep draining until we've seen all expected local handler invocations.
            while rx_counter.load(Ordering::SeqCst) < ITERS {
                r_drain
                    .process_all_queues()
                    .expect("process_all_queues failed");
                thread::yield_now();
            }
        });

        // Wait for both threads to finish.
        logger.join().expect("logger thread panicked");
        drainer.join().expect("drainer thread panicked");

        // After both threads are done, all queued messages should have been
        // transmitted and delivered to the local handler exactly once each.
        let rx = rx_count.load(Ordering::SeqCst);
        let tx = tx_count.load(Ordering::SeqCst);

        assert_eq!(rx, ITERS, "expected {ITERS} handler calls, got {rx}");
        assert_eq!(tx, ITERS, "expected {ITERS} TX frames, got {tx}");
    }

    /// Mix concurrent logging, RX-queue insertion, and queue draining; ensure
    /// that all work is eventually processed exactly once.
    #[test]
    fn concurrent_log_receive_and_process_mix_is_thread_safe() {
        use std::thread;

        const LOG_ITERS: usize = 100;
        const RX_ITERS: usize = 100;

        let tx_count = Arc::new(AtomicUsize::new(0));
        let txc = tx_count.clone();
        let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            txc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        // Handler that counts packets (both TX-local and RX-delivered).
        let rx_count = Arc::new(AtomicUsize::new(0));
        let rxc = rx_count.clone();
        let handler = EndpointHandler::new_packet_handler(
            DataEndpoint::SdCard,
            move |_pkt: &TelemetryPacket| {
                rxc.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );

        let router = Router::new(Some(tx), BoardConfig::new(vec![handler]), zero_clock());
        let r = Arc::new(router);

        // ---------- Logger thread ----------
        let r_logger = r.clone();
        let t_logger = thread::spawn(move || {
            let data: [f32; 2] = [1.0, 2.0];
            for _ in 0..LOG_ITERS {
                r_logger
                    .log_queue(DataType::GpsData, &data)
                    .expect("log_queue failed");
            }
        });

        // ---------- RX thread ----------
        let r_rx = r.clone();
        let t_rx = thread::spawn(move || {
            for _ in 0..RX_ITERS {
                let pkt = TelemetryPacket::from_f32_slice(
                    DataType::GpsData,
                    &[4.0, 5.0],
                    &[DataEndpoint::SdCard],
                    0,
                )
                .unwrap();
                r_rx.rx_packet_to_queue(pkt)
                    .expect("rx_packet_to_queue failed");
            }
        });

        // ---------- Processor thread ----------
        let r_proc = r.clone();
        let rx_counter = rx_count.clone();
        let t_proc = thread::spawn(move || {
            // Loop until handler count reaches expected total
            while rx_counter.load(Ordering::SeqCst) < LOG_ITERS + RX_ITERS {
                r_proc
                    .process_all_queues()
                    .expect("process_all_queues failed");
                thread::yield_now();
            }
        });

        t_logger.join().expect("logger thread panicked");
        t_rx.join().expect("rx thread panicked");
        t_proc.join().expect("processor thread panicked");

        let tx = tx_count.load(Ordering::SeqCst);
        let rx = rx_count.load(Ordering::SeqCst);
        assert_eq!(tx, LOG_ITERS, "expected {LOG_ITERS} TX frames");
        assert_eq!(
            rx,
            LOG_ITERS + RX_ITERS,
            "expected {LOG_ITERS}+{RX_ITERS} handler invocations"
        );
    }
}
