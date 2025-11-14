//! C FFI bindings for the SEDS telemetry router.
//!
//! This module exposes a stable C ABI for:
//! - Router lifecycle (create / free)
//! - Logging (typed, bytes, strings)
//! - RX / TX queue processing
//! - Packet serialization / deserialization
//! - Fixed-size schema queries
//! - Typed payload extraction

use crate::{
    config::DataEndpoint, do_vec_log_typed, get_needed_message_size, message_meta,
    router::{BoardConfig, EndpointHandler, Router},
    router::{Clock, LeBytes}, serialize::{deserialize_packet, packet_wire_size, peek_envelope, serialize_packet}, telemetry_packet::{DataType, TelemetryPacket},
    MessageElementCount,
    TelemetryError,
    TelemetryErrorCode,
    TelemetryResult,
};
use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};
use core::{ffi::c_char, ffi::c_void, mem::size_of, ptr, slice, str::from_utf8};

// ============================================================================
//  Constants / basic types shared with the C side
// ============================================================================

/// Element-kind tags for typed logging / extraction.
const SEDS_EK_UNSIGNED: u32 = 0;
const SEDS_EK_SIGNED: u32 = 1;
const SEDS_EK_FLOAT: u32 = 2;

/// Small stack buffer size for endpoint lists in callbacks.
const STACK_EPS: usize = 16; // number of endpoints to store on stack for callback

/// Generic "OK/ERR" status returned for simple FFI entry points.
#[repr(i32)]
enum SedsResult {
    SedsOk = 0,
    SedsErr = 1,
}

/// Opaque owned packet for C. Keeps Rust allocations alive across calls.
#[repr(C)]
pub struct SedsOwnedPacket {
    inner: TelemetryPacket,
    // cache endpoints as u32 so the view can point at stable memory
    endpoints_u32: Vec<u32>,
}

/// Opaque owned header/envelope for C.
/// Owns only header pieces (no payload and no size).
#[repr(C)]
pub struct SedsOwnedHeader {
    ty: u32,
    sender: Arc<str>,        // own the sender so view can borrow it
    endpoints_u32: Vec<u32>, // own endpoints as u32 for stable pointers
    timestamp: u64,
}

// ============================================================================
//  Status / error helpers (shared for all FFI functions)
// ============================================================================
#[inline]
fn status_from_result_code(e: SedsResult) -> i32 {
    match e {
        SedsResult::SedsOk => 0,
        SedsResult::SedsErr => 1,
    }
}
#[inline]
fn status_from_err(e: TelemetryError) -> i32 {
    e.to_error_code() as i32
}
#[inline]
fn ok_or_status(r: TelemetryResult<()>) -> i32 {
    match r {
        Ok(()) => status_from_result_code(SedsResult::SedsOk),
        Err(e) => status_from_err(e),
    }
}

/// Returns the fixed payload size (in bytes) for a static schema, or `None`
/// if the message type is dynamically sized.
#[inline]
fn fixed_payload_size_if_static(ty: DataType) -> Option<usize> {
    match message_meta(ty).element_count {
        MessageElementCount::Static(_) => Some(get_needed_message_size(ty)),
        MessageElementCount::Dynamic => None,
    }
}

/// Convert an optional pointer to an `Option<u64>` timestamp.
#[inline]
fn opt_ts(ts_ptr: *const u64) -> Option<u64> {
    if ts_ptr.is_null() {
        None
    } else {
        Some(unsafe { *ts_ptr })
    }
}

/// Convert a C-side `u32` type tag into a Rust `DataType`.
#[inline]
fn dtype_from_u32(x: u32) -> TelemetryResult<DataType> {
    DataType::try_from_u32(x).ok_or(TelemetryError::InvalidType)
}

/// Convert a C-side `u32` endpoint into a Rust `DataEndpoint`.
#[inline]
fn endpoint_from_u32(x: u32) -> TelemetryResult<DataEndpoint> {
    DataEndpoint::try_from_u32(x).ok_or(TelemetryError::Deserialize("bad endpoint"))
}

// ============================================================================
//  C-facing opaque types and handler descriptors
// ============================================================================

/// Opaque router handle exposed to C.
#[repr(C)]
pub struct SedsRouter {
    inner: Arc<Router>,
}

/// Must match the C header layout for `SedsPacketView`.
#[repr(C)]
pub struct SedsPacketView {
    ty: u32,
    data_size: usize,
    sender: *const c_char, // pointer
    sender_len: usize,     // length
    endpoints: *const u32,
    num_endpoints: usize,
    timestamp: u64,
    payload: *const u8,
    payload_len: usize,
}

/// Transmit callback signature used from C.

type CTransmit = Option<extern "C" fn(bytes: *const u8, len: usize, user: *mut c_void) -> i32>;

/// Endpoint handler callback (packet view).
type CEndpointHandler = Option<extern "C" fn(pkt: *const SedsPacketView, user: *mut c_void) -> i32>;

/// Endpoint handler callback (serialized bytes).
type CSerializedHandler =
    Option<extern "C" fn(bytes: *const u8, len: usize, user: *mut c_void) -> i32>;

/// C-facing endpoint descriptor (must match C header).
#[repr(C)]
pub struct SedsLocalEndpointDesc {
    endpoint: u32,                          // DataEndpoint as u32
    packet_handler: CEndpointHandler,       // optional
    serialized_handler: CSerializedHandler, // optional
    user: *mut c_void,
}

#[derive(Copy, Clone)]
struct TxCtx {
    user_addr: usize,
}

unsafe impl Send for TxCtx {}
unsafe impl Sync for TxCtx {}

// ============================================================================
//  Internal helpers: view_to_packet, string buffer writing, clock adapter
// ============================================================================

/// Convert a C `SedsPacketView` into an owned Rust `TelemetryPacket`.
/// Returns `Err(())` if type/endpoints/sender are invalid or inconsistent.
#[inline]
fn view_to_packet(view: &SedsPacketView) -> Result<TelemetryPacket, ()> {
    // Map type
    let ty = DataType::try_from_u32(view.ty).ok_or(())?;

    // Endpoints (u32 → DataEndpoint)
    let eps_u32 = unsafe { slice::from_raw_parts(view.endpoints, view.num_endpoints) };
    let mut eps = Vec::with_capacity(eps_u32.len());
    for &e in eps_u32 {
        let ep = DataEndpoint::try_from_u32(e).ok_or(())?;
        eps.push(ep);
    }

    // Sender as Arc<str>
    let sender_owned: Arc<str> = if view.sender.is_null() || view.sender_len == 0 {
        Arc::<str>::from("")
    } else {
        let sb = unsafe { slice::from_raw_parts(view.sender as *const u8, view.sender_len) };
        let s = from_utf8(sb).map_err(|_| ())?;
        Arc::<str>::from(s)
    };

    // Payload bytes
    let bytes = unsafe { slice::from_raw_parts(view.payload, view.payload_len) };

    // Optional: keep the C view honest
    if view.data_size != view.payload_len {
        return Err(());
    }

    let payload = Arc::<[u8]>::from(bytes);

    TelemetryPacket::new(ty, &eps, sender_owned, view.timestamp, payload).map_err(|_| ())
}

/// Write a Rust string into a C buffer, respecting "query mode":
///
/// - If `buf` is NULL or `buf_len == 0`, returns the required size
///   (including the NUL terminator) without writing.
/// - If the buffer is too small, writes as much as fits (NUL-terminated)
///   and returns the required size.
/// - On success, returns `SEDS_OK` (0).
 #[inline]
unsafe fn write_str_to_buf(s: &str, buf: *mut c_char, buf_len: usize) -> i32 {
    if buf.is_null() && buf_len != 0 {
        return status_from_err(TelemetryError::BadArg);
    }
    let needed = s.len() + 1; // include NUL

    // Query mode: tell caller required buffer size (including NUL)
    if buf.is_null() || buf_len == 0 {
        return needed as i32;
    }

    let ncopy = core::cmp::min(s.len(), buf_len.saturating_sub(1));
    unsafe {
        ptr::copy_nonoverlapping(s.as_ptr(), buf as *mut u8, ncopy);
        *buf.add(ncopy) = 0;
    }

    // If too small, return required size (not success)
    if buf_len < needed {
        return needed as i32;
    }

    status_from_result_code(SedsResult::SedsOk)
}

/// Validate that a width is NOT one of the disallowed sizes.
/// Used to reject 1, 2, 4, 8, 16 byte widths for typed logging.
/// Used in FFI logging helpers.
#[inline]
fn width_is_valid(width: usize) -> bool {
    matches!(width, 1 | 2 | 4 | 8 | 16)
}

/// FFI-facing clock adapter that calls back into C when present.
type CNowMs = Option<extern "C" fn(user: *mut c_void) -> u64>;

struct FfiClock {
    cb: CNowMs,
    user_addr: usize,
}

impl Clock for FfiClock {
    fn now_ms(&self) -> u64 {
        if let Some(f) = self.cb {
            f(self.user_addr as *mut c_void)
        } else {
            0
        }
    }
}

// ============================================================================
//  FFI: String / error formatting helpers
// ============================================================================

/// Returns the required buffer length (including NUL) for a packet header string,
/// or a negative error code if `pkt` is invalid.
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_header_string_len(pkt: *const SedsPacketView) -> i32 {
    if pkt.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let view = unsafe { &*pkt };
    let tpkt = match view_to_packet(view) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::BadArg),
    };
    let s = tpkt.header_string();
    (s.len() + 1) as i32
}

/// Returns the required buffer length (including NUL) for a full packet string,
/// or a negative error code if `pkt` is invalid.
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_to_string_len(pkt: *const SedsPacketView) -> i32 {
    let result = packet_to_string(pkt);
    if let Err(err) = result {
        return err;
    }
    let s = result.unwrap();
    (s.len() + 1) as i32
}

/// Returns the required buffer length (including NUL) for an error string.
#[unsafe(no_mangle)]
pub extern "C" fn seds_error_to_string_len(error_code: i32) -> i32 {
    let s = error_code_to_string(error_code);
    (s.len() + 1) as i32
}

fn packet_to_string(pkt: *const SedsPacketView) -> Result<String, i32> {
    if pkt.is_null() {
        return Err(status_from_err(TelemetryError::BadArg));
    }
    let view = unsafe { &*pkt };
    let tpkt = match view_to_packet(view) {
        Ok(p) => p,
        Err(_) => return Err(status_from_err(TelemetryError::BadArg)),
    };
    Ok(tpkt.to_string())
}

fn error_code_to_string(error_code: i32) -> &'static str {
    let result = TelemetryErrorCode::try_from_i32(error_code);

    match result {
        Some(s) => s.as_str(),
        None => {
            if error_code == SedsResult::SedsOk as i32 {
                "SEDS OK"
            } else if error_code == SedsResult::SedsErr as i32 {
                "SEDS ERROR"
            } else {
                "Unknown error"
            }
        }
    }
}

/// Format only the header portion of a packet into a C buffer.
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_header_string(
    pkt: *const SedsPacketView,
    buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    if pkt.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let view = unsafe { &*pkt };
    let tpkt = match view_to_packet(view) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::BadArg),
    };
    let s = tpkt.header_string();
    unsafe { write_str_to_buf(&s, buf, buf_len) }
}

/// Format the full packet into a C buffer.
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_to_string(
    pkt: *const SedsPacketView,
    buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    let result = packet_to_string(pkt);
    if let Err(err) = result {
        return err;
    }
    let s = result.unwrap();

    if s.len() > buf_len {
        return status_from_err(TelemetryError::BadArg);
    }
    unsafe { write_str_to_buf(&s, buf, buf_len) }
}

/// Format a numeric error code into a human-readable string.
#[unsafe(no_mangle)]
pub extern "C" fn seds_error_to_string(error_code: i32, buf: *mut c_char, buf_len: usize) -> i32 {
    let s = error_code_to_string(error_code);

    if s.len() > buf_len {
        return status_from_err(TelemetryError::BadArg);
    }
    unsafe { write_str_to_buf(&s, buf, buf_len) }
}

// ============================================================================
//  FFI: Router lifecycle (new / free)
// ============================================================================

/// Create a new router instance.
///
/// - `tx` / `tx_user` are used for outgoing serialized packets.
/// - `now_ms_cb` provides a monotonic clock (may be NULL).
/// - `handlers` / `n_handlers` register endpoint handlers.
///
/// Returns a non-null pointer on success or NULL on failure.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_new(
    tx: CTransmit,
    tx_user: *mut c_void,
    now_ms_cb: CNowMs,
    handlers: *const SedsLocalEndpointDesc,
    n_handlers: usize,
) -> *mut SedsRouter {
    // Build transmit closure
    let tx_ctx = TxCtx {
        user_addr: tx_user as usize,
    };

    let transmit = tx.map(move |f| {
        let ctx = tx_ctx;
        move |bytes: &[u8]| -> TelemetryResult<()> {
            let code = f(bytes.as_ptr(), bytes.len(), ctx.user_addr as *mut c_void);
            if code == status_from_result_code(SedsResult::SedsOk) {
                Ok(())
            } else {
                Err(TelemetryError::Io("tx error"))
            }
        }
    });

    // Build handler vector
    let mut v: Vec<EndpointHandler> = Vec::new();
    if n_handlers > 0 && !handlers.is_null() {
        v.reserve(n_handlers.saturating_mul(2));
        let slice = unsafe { slice::from_raw_parts(handlers, n_handlers) };
        for desc in slice {
            let endpoint = match endpoint_from_u32(desc.endpoint) {
                Ok(e) => e,
                Err(_) => return ptr::null_mut(),
            };

            // Common user ctx for either callback kind
            let user_addr = desc.user as usize;

            // If a PACKET handler is provided, register it
            if let Some(cb_fn) = desc.packet_handler {
                let eh =
                    EndpointHandler::new_packet_handler(endpoint, move |pkt: &TelemetryPacket| {
                        // Fast path: up to STACK_EPS endpoints, no heap allocation
                        let mut stack_eps: [u32; STACK_EPS] = [0; STACK_EPS];

                        // Decide whether we use the stack array or a heap Vec
                        let (endpoints_ptr, num_endpoints, _owned_vec): (
                            *const u32,
                            usize,
                            Option<Vec<u32>>,
                        ) = if pkt.endpoints().len() <= STACK_EPS {
                            for (i, e) in pkt.endpoints().iter().enumerate() {
                                stack_eps[i] = *e as u32;
                            }
                            (
                                stack_eps.as_ptr(),
                                pkt.endpoints().len(),
                                None, // no heap allocation used
                            )
                        } else {
                            // Rare path: heap
                            let mut eps_u32 = Vec::with_capacity(pkt.endpoints().len());
                            for e in pkt.endpoints().iter() {
                                eps_u32.push(*e as u32);
                            }
                            let ptr = eps_u32.as_ptr();
                            let len = eps_u32.len();
                            (
                                ptr,
                                len,
                                Some(eps_u32), // keep Vec alive for the duration of the call
                            )
                        };

                        let sender_bytes = pkt.sender().as_bytes();
                        let view = SedsPacketView {
                            ty: pkt.data_type() as u32,
                            data_size: pkt.data_size(),
                            sender: sender_bytes.as_ptr() as *const c_char,
                            sender_len: sender_bytes.len(),
                            endpoints: endpoints_ptr,
                            num_endpoints,
                            timestamp: pkt.timestamp(),
                            payload: pkt.payload().as_ptr(),
                            payload_len: pkt.payload().len(),
                        };

                        let code = cb_fn(&view as *const _, user_addr as *mut c_void);
                        if code == status_from_result_code(SedsResult::SedsOk) {
                            Ok(())
                        } else {
                            Err(TelemetryError::Io("handler error"))
                        }
                    });

                v.push(eh);
            }

            // If a SERIALIZED handler is provided, register it
            if let Some(cb_fn) = desc.serialized_handler {
                let eh =
                    EndpointHandler::new_serialized_handler(endpoint, move |bytes: &[u8]| {
                        let code = cb_fn(bytes.as_ptr(), bytes.len(), user_addr as *mut c_void);
                        if code == status_from_result_code(SedsResult::SedsOk) {
                            Ok(())
                        } else {
                            Err(TelemetryError::Io("handler error"))
                        }
                    });

                v.push(eh);
            }
        }
    }

    let clock = FfiClock {
        cb: now_ms_cb,
        user_addr: tx_user as usize,
    };

    let box_clock = Box::new(clock);
    let cfg = BoardConfig::new(v);
    let router = Router::new(transmit, cfg, box_clock);
    Box::into_raw(Box::new(SedsRouter {
        inner: Arc::from(router),
    }))
}

/// Destroy a router previously returned by `seds_router_new`.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_free(r: *mut SedsRouter) {
    if r.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(r));
    }
}

// ============================================================================
//  FFI: Schema helper (fixed payload size)
// ============================================================================

/// Return the fixed payload size (in bytes) for a given `SedsDataType`.
///
/// - On success: returns `size >= 0`
/// - For dynamic-sized types: returns `0`
/// - Invalid type: returns a negative error code.
#[unsafe(no_mangle)]
pub extern "C" fn seds_dtype_expected_size(ty_u32: u32) -> i32 {
    let ty = match dtype_from_u32(ty_u32) {
        Ok(t) => t,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };

    match fixed_payload_size_if_static(ty) {
        Some(sz) => sz as i32,
        None => 0, // dynamic schema: no single fixed size
    }
}

// ============================================================================
//  Internal logging helper: dispatch queue vs immediate, with optional ts
// ============================================================================

/// Unified helper to dispatch queue vs. immediate logging with optional timestamp.
fn call_log_or_queue<T: LeBytes>(
    router: *mut SedsRouter,
    ty: DataType,
    ts: Option<u64>,
    data: &[T],
    queue: bool,
) -> TelemetryResult<()> {
    unsafe {
        let r = &(*router).inner; // shared borrow only
        if queue {
            match ts {
                Some(t) => r.log_queue_ts::<T>(ty, t, data),
                None => r.log_queue::<T>(ty, data),
            }
        } else {
            match ts {
                Some(t) => r.log_ts::<T>(ty, t, data),
                None => r.log::<T>(ty, data),
            }
        }
    }
}

/// Shared helper for fixed-size typed logging.
///
/// Takes a padded byte slice and converts it into `T` values via `vectorize_data`.
fn finish_with<T: LeBytes + Copy>(
    r: *mut SedsRouter,
    ty: DataType,
    ts: Option<u64>,
    queue: bool,
    padded: &[u8],
    required_elems: usize,
    elem_size: usize,
) -> i32 {
    let mut tmp: Vec<T> = Vec::with_capacity(required_elems);
    // vectorize_data reads unaligned little-endian elements into tmp
    if let Err(_) = vectorize_data::<T>(padded.as_ptr(), required_elems, elem_size, &mut tmp) {
        return status_from_err(TelemetryError::Io("vectorize_data failed"));
    }
    ok_or_status(unsafe {
        let router = &(*r).inner; // shared borrow
        if queue {
            match ts {
                Some(t) => router.log_queue_ts::<T>(ty, t, &tmp),
                None => router.log_queue::<T>(ty, &tmp),
            }
        } else {
            match ts {
                Some(t) => router.log_ts::<T>(ty, t, &tmp),
                None => router.log::<T>(ty, &tmp),
            }
        }
    })
}

// ============================================================================
//  FFI: Unified logging entry points (typed / bytes / string)
// ============================================================================

/// Log a byte payload, with optional timestamp and queue flag.
///
/// If the schema has a fixed size, the data is padded/truncated to exactly
/// that size. For dynamic schemas, the data is passed as-is.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_bytes_ex(
    r: *mut SedsRouter,
    ty_u32: u32,
    data: *const u8,
    len: usize,
    timestamp_ms_opt: *const u64, // NULL => use now_ms()
    queue: bool,                  // false = send now, true = queue
) -> i32 {
    if r.is_null() || (len > 0 && data.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let ty = match dtype_from_u32(ty_u32) {
        Ok(t) => t,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };

    let src = unsafe { slice::from_raw_parts(data, len) };
    let ts = opt_ts(timestamp_ms_opt);

    // If this type has a fixed schema size, pad/truncate to exactly that many bytes.
    if let Some(required) = fixed_payload_size_if_static(ty) {
        // Fast path: exact match → pass through
        if src.len() == required {
            return ok_or_status(call_log_or_queue::<u8>(r, ty, ts, src, queue));
        }

        // Build zero-filled buffer of the required size and copy what we have.
        let mut tmp = vec![0u8; required];
        let ncopy = core::cmp::min(src.len(), required);
        if ncopy > 0 {
            tmp[..ncopy].copy_from_slice(&src[..ncopy]);
        }

        return ok_or_status(call_log_or_queue::<u8>(r, ty, ts, &tmp, queue));
    }

    // Otherwise (Dynamic): pass through as-is
    ok_or_status(call_log_or_queue::<u8>(r, ty, ts, src, queue))
}

/// Log `f32` values, with optional timestamp and queue flag.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_f32_ex(
    r: *mut SedsRouter,
    ty_u32: u32,
    vals: *const f32,
    n_vals: usize,
    timestamp_ms_opt: *const u64, // NULL => now()
    queue: bool,
) -> i32 {
    if r.is_null() || (n_vals > 0 && vals.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let ty = match dtype_from_u32(ty_u32) {
        Ok(t) => t,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    let slice = unsafe { slice::from_raw_parts(vals, n_vals) };
    ok_or_status(call_log_or_queue::<f32>(
        r,
        ty,
        opt_ts(timestamp_ms_opt),
        slice,
        queue,
    ))
}

/// String/byte logger that pads or truncates to the schema’s fixed size.
///
/// This is the string-aware entry point used by the C/C++ convenience
/// macros for NUL-terminated strings.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_string_ex(
    r: *mut SedsRouter,
    ty_u32: u32,
    bytes: *const c_char,
    len: usize,
    timestamp_ms_opt: *const u64,
    queue: bool,
) -> i32 {
    if r.is_null() || (len > 0 && bytes.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let ty = match dtype_from_u32(ty_u32) {
        Ok(t) => t,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };

    // Cast char* → u8*
    let src = unsafe { slice::from_raw_parts(bytes as *const u8, len) };
    let ts = opt_ts(timestamp_ms_opt);

    // Reuse the same logic as seds_router_log_bytes_ex:
    if let Some(required) = fixed_payload_size_if_static(ty) {
        if src.len() == required {
            return ok_or_status(call_log_or_queue::<u8>(r, ty, ts, src, queue));
        }
        let mut tmp = vec![0u8; required];
        let ncopy = core::cmp::min(src.len(), required);
        if ncopy > 0 {
            tmp[..ncopy].copy_from_slice(&src[..ncopy]);
        }
        return ok_or_status(call_log_or_queue::<u8>(r, ty, ts, &tmp, queue));
    }
    ok_or_status(call_log_or_queue::<u8>(r, ty, ts, src, queue))
}

/// Unified typed logger with optional timestamp and queue flag.
///
/// - `elem_size` must be 1,2,4,8.
/// - `elem_kind` is one of SEDS_EK_UNSIGNED / SEDS_EK_SIGNED / SEDS_EK_FLOAT.
/// - For fixed-size schemas, the payload is padded/truncated to match.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_typed_ex(
    r: *mut SedsRouter,
    ty_u32: u32,
    data: *const c_void,
    count: usize,
    elem_size: usize,             // 1,2,4,8
    elem_kind: u32,               // 0=unsigned,1=signed,2=float
    timestamp_ms_opt: *const u64, // NULL => now()
    queue: bool,
) -> i32 {
    if r.is_null() || (count > 0 && data.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    if !width_is_valid(elem_size) {
        return status_from_err(TelemetryError::BadArg);
    }
    let ty = match dtype_from_u32(ty_u32) {
        Ok(t) => t,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    let ts = opt_ts(timestamp_ms_opt);

    // Common padded path ONLY if the type declares a fixed payload size
    if let Some(required_bytes) = fixed_payload_size_if_static(ty) {
        if required_bytes % elem_size != 0 {
            return status_from_err(TelemetryError::BadArg);
        }

        let src_bytes_len = count.saturating_mul(elem_size);
        let src = unsafe { slice::from_raw_parts(data as *const u8, src_bytes_len) };

        let mut padded = vec![0u8; required_bytes];
        let ncopy = core::cmp::min(src.len(), required_bytes);
        if ncopy > 0 {
            padded[..ncopy].copy_from_slice(&src[..ncopy]);
        }

        let required_elems = required_bytes / elem_size;

        return match (elem_kind, elem_size) {
            (SEDS_EK_UNSIGNED, 1) => {
                finish_with::<u8>(r, ty, ts, queue, &padded, required_elems, 1)
            }
            (SEDS_EK_UNSIGNED, 2) => {
                finish_with::<u16>(r, ty, ts, queue, &padded, required_elems, 2)
            }
            (SEDS_EK_UNSIGNED, 4) => {
                finish_with::<u32>(r, ty, ts, queue, &padded, required_elems, 4)
            }
            (SEDS_EK_UNSIGNED, 8) => {
                finish_with::<u64>(r, ty, ts, queue, &padded, required_elems, 8)
            }
            (SEDS_EK_UNSIGNED, 16) => {
                finish_with::<u128>(r, ty, ts, queue, &padded, required_elems, 16)
            }

            (SEDS_EK_SIGNED, 1) => finish_with::<i8>(r, ty, ts, queue, &padded, required_elems, 1),
            (SEDS_EK_SIGNED, 2) => finish_with::<i16>(r, ty, ts, queue, &padded, required_elems, 2),
            (SEDS_EK_SIGNED, 4) => finish_with::<i32>(r, ty, ts, queue, &padded, required_elems, 4),
            (SEDS_EK_SIGNED, 8) => finish_with::<i64>(r, ty, ts, queue, &padded, required_elems, 8),
            (SEDS_EK_SIGNED, 16) => finish_with::<i128>(r, ty, ts, queue, &padded, required_elems, 16),


            (SEDS_EK_FLOAT, 4) => finish_with::<f32>(r, ty, ts, queue, &padded, required_elems, 4),
            (SEDS_EK_FLOAT, 8) => finish_with::<f64>(r, ty, ts, queue, &padded, required_elems, 8),
            _ => status_from_err(TelemetryError::BadArg),
        };
    }

    // No fixed-size requirement (Dynamic): fast path, no resizing
    match (elem_kind, elem_size) {
        (SEDS_EK_UNSIGNED, 1) => do_vec_log_typed!(r, ty, ts, queue, data, count, u8),
        (SEDS_EK_UNSIGNED, 2) => do_vec_log_typed!(r, ty, ts, queue, data, count, u16),
        (SEDS_EK_UNSIGNED, 4) => do_vec_log_typed!(r, ty, ts, queue, data, count, u32),
        (SEDS_EK_UNSIGNED, 8) => do_vec_log_typed!(r, ty, ts, queue, data, count, u64),
        (SEDS_EK_UNSIGNED, 16) => do_vec_log_typed!(r, ty, ts, queue, data, count, u128),

        (SEDS_EK_SIGNED, 1) => do_vec_log_typed!(r, ty, ts, queue, data, count, i8),
        (SEDS_EK_SIGNED, 2) => do_vec_log_typed!(r, ty, ts, queue, data, count, i16),
        (SEDS_EK_SIGNED, 4) => do_vec_log_typed!(r, ty, ts, queue, data, count, i32),
        (SEDS_EK_SIGNED, 8) => do_vec_log_typed!(r, ty, ts, queue, data, count, i64),
        (SEDS_EK_SIGNED, 16) => do_vec_log_typed!(r, ty, ts, queue, data, count, i128),


        (SEDS_EK_FLOAT, 4) => do_vec_log_typed!(r, ty, ts, queue, data, count, f32),
        (SEDS_EK_FLOAT, 8) => do_vec_log_typed!(r, ty, ts, queue, data, count, f64),

        _ => status_from_err(TelemetryError::BadArg),
    }
}

// ---------- Legacy logging wrappers (preserve existing ABI) ----------

/// Legacy convenience: log bytes immediately without timestamp.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_bytes(
    r: *mut SedsRouter,
    ty_u32: u32,
    data: *const u8,
    len: usize,
) -> i32 {
    seds_router_log_bytes_ex(r, ty_u32, data, len, ptr::null(), false)
}

/// Legacy convenience: log `f32` values immediately without timestamp.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_f32(
    r: *mut SedsRouter,
    ty_u32: u32,
    vals: *const f32,
    n_vals: usize,
) -> i32 {
    seds_router_log_f32_ex(r, ty_u32, vals, n_vals, ptr::null(), false)
}

/// Legacy convenience: typed logger (immediate).
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_typed(
    r: *mut SedsRouter,
    ty_u32: u32,
    data: *const c_void,
    count: usize,
    elem_size: usize,
    elem_kind: u32,
) -> i32 {
    seds_router_log_typed_ex(
        r,
        ty_u32,
        data,
        count,
        elem_size,
        elem_kind,
        ptr::null(),
        false,
    )
}

/// Legacy convenience: typed logger into the TX queue.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_queue_typed(
    r: *mut SedsRouter,
    ty_u32: u32,
    data: *const c_void,
    count: usize,
    elem_size: usize,
    elem_kind: u32,
) -> i32 {
    seds_router_log_typed_ex(
        r,
        ty_u32,
        data,
        count,
        elem_size,
        elem_kind,
        ptr::null(),
        true,
    )
}

// ============================================================================
//  FFI: Receive / queue RX and process queues
// ============================================================================

/// Feed a serialized packet directly into the router RX path.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_receive_serialized(
    r: *mut SedsRouter,
    bytes: *const u8,
    len: usize,
) -> i32 {
    if r.is_null() || (len > 0 && bytes.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    let slice = unsafe { slice::from_raw_parts(bytes, len) };
    ok_or_status(router.receive_serialized(slice))
}

/// Feed a packet view (constructed on the C side) into the router RX path.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_receive(r: *mut SedsRouter, view: *const SedsPacketView) -> i32 {
    if r.is_null() || view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    let pkt = match view_to_packet(unsafe { &*view }) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    ok_or_status(router.receive(&pkt))
}

/// Transmit a packet view via the router’s TX queue immediately.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_transmit_message_queue(
    r: *mut SedsRouter,
    view: *const SedsPacketView,
) -> i32 {
    if r.is_null() || view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    let pkt = match view_to_packet(unsafe { &*view }) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    ok_or_status(router.transmit_message_queue(pkt))
}

// ----- Queue processing (no time budget) -----

/// Process all pending TX in the router TX queue.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_process_tx_queue(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    ok_or_status(router.process_tx_queue())
}

/// Process all pending RX in the router RX queue.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_process_rx_queue(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    ok_or_status(router.process_rx_queue())
}

/// Enqueue a serialized packet into the RX queue (deferred processing).
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_rx_serialized_packet_to_queue(
    r: *mut SedsRouter,
    bytes: *const u8,
    len: usize,
) -> i32 {
    if r.is_null() || (len > 0 && bytes.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    let slice = unsafe { slice::from_raw_parts(bytes, len) };
    ok_or_status(router.rx_serialized_packet_to_queue(slice))
}

/// Enqueue a packet view into the RX queue (deferred processing).
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_rx_packet_to_queue(
    r: *mut SedsRouter,
    view: *const SedsPacketView,
) -> i32 {
    if r.is_null() || view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    let pkt = match view_to_packet(unsafe { &*view }) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    ok_or_status(router.rx_packet_to_queue(pkt))
}

/// Process all TX and RX queues until empty.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_process_all_queues(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    ok_or_status(router.process_all_queues())
}

/// Clear both RX and TX queues.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_clear_queues(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    router.clear_queues();
    status_from_result_code(SedsResult::SedsOk)
}

/// Clear only the RX queue.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_clear_rx_queue(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    router.clear_rx_queue();
    status_from_result_code(SedsResult::SedsOk)
}

/// Clear only the TX queue.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_clear_tx_queue(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    router.clear_tx_queue();
    status_from_result_code(SedsResult::SedsOk)
}

// ----- Queue processing with time budget -----

/// Process the TX queue until timeout (in ms) or completion.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_process_tx_queue_with_timeout(
    r: *mut SedsRouter,
    timeout_ms: u32,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    ok_or_status(router.process_tx_queue_with_timeout(timeout_ms))
}

/// Process the RX queue until timeout (in ms) or completion.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_process_rx_queue_with_timeout(
    r: *mut SedsRouter,
    timeout_ms: u32,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    ok_or_status(router.process_rx_queue_with_timeout(timeout_ms))
}

/// Process all queues until timeout (in ms) or completion.
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_process_all_queues_with_timeout(
    r: *mut SedsRouter,
    timeout_ms: u32,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner }; // shared borrow
    ok_or_status(router.process_all_queues_with_timeout(timeout_ms))
}

// ============================================================================
//  FFI: Payload pointer & copy helpers
// ============================================================================

/// Get a raw pointer to the payload bytes in a packet view.
///
/// - If `out_len` is non-null, it is filled with the payload length.
/// - Returns NULL if `pkt` is NULL.
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_bytes_ptr(
    pkt: *const SedsPacketView,
    out_len: *mut usize,
) -> *const c_void {
    if pkt.is_null() {
        return ptr::null();
    }
    let view = unsafe { &*pkt };
    if !out_len.is_null() {
        unsafe { *out_len = view.payload_len };
    }
    view.payload as *const c_void
}

/// Get a typed pointer into the payload given an element size (1,2,4,8).
///
/// - Fails (returns NULL) if `pkt` is NULL or size does not divide the payload.
/// - If `out_count` is non-null, it is filled with the element count.
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_data_ptr(
    pkt: *const SedsPacketView,
    elem_size: usize,      // 1,2,4,8
    out_count: *mut usize, // optional
) -> *const c_void {
    if pkt.is_null() || !width_is_valid(elem_size) {
        return ptr::null();
    }
    let view = unsafe { &*pkt };

    if elem_size == 0 || view.payload_len % elem_size != 0 {
        if !out_count.is_null() {
            unsafe { *out_count = 0 };
        }
        return ptr::null();
    }

    let count = view.payload_len / elem_size;
    if !out_count.is_null() {
        unsafe { *out_count = count };
    }

    view.payload as *const c_void
}

/// Copy raw payload bytes into `dst`.
///
/// Query mode:
/// - If `dst` is NULL or `dst_len < payload_len`, returns the required
///   byte count without writing.
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_copy_bytes(
    pkt: *const SedsPacketView,
    dst: *mut u8,
    dst_len: usize,
) -> i32 {
    if pkt.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let view = unsafe { &*pkt };
    let needed = view.payload_len;

    if needed == 0 {
        return 0;
    } // fast path
    if view.payload.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    // Query/too-small
    if dst.is_null() || dst_len < needed {
        return needed as i32;
    }

    unsafe {
        ptr::copy_nonoverlapping(view.payload, dst, needed);
    }
    needed as i32
}

/// Copy payload as elements of a given size into `dst`.
///
/// - `elem_size` must be 1,2,4,8.
/// - Query mode: if `dst` is NULL, `dst_elems` is 0, or `dst_elems < count`,
///   returns the element count without writing.
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_copy_data(
    pkt: *const SedsPacketView,
    elem_size: usize, // must be 1,2,4,8
    dst: *mut c_void,
    dst_elems: usize, // number of elements available in dst
) -> i32 {
    if pkt.is_null() || !width_is_valid(elem_size) {
        return status_from_err(TelemetryError::BadArg);
    }

    let view = unsafe { &*pkt };

    // Validate divisibility
    if elem_size == 0 || view.payload_len % elem_size != 0 {
        return status_from_err(TelemetryError::BadArg);
    }

    let count = view.payload_len / elem_size;
    if count == 0 {
        return 0;
    } // after computing `count = view.payload_len / elem_size`

    // Query mode or too-small -> return required size (elements)
    if dst.is_null() || dst_elems == 0 || dst_elems < count {
        return count as i32;
    }

    if count > 0 && view.payload.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    // Compute total byte size, guarding overflow
    let total_bytes = match count.checked_mul(elem_size) {
        Some(n) => n,
        None => return status_from_err(TelemetryError::BadArg),
    };

    unsafe {
        ptr::copy_nonoverlapping(view.payload, dst as *mut u8, total_bytes);
    }
    count as i32
}

// ============================================================================
//  Typed extraction support: vectorize_data + seds_pkt_get_typed
// ============================================================================

#[derive(Debug)]
#[allow(dead_code)]
pub enum VectorizeError {
    NullBasePtr,
    ElemSizeMismatch { elem_size: usize, expected: usize },
    Overflow,
    ZeroCount,
}

/// Convert a raw byte buffer into a vector of `T`, interpreting each element
/// as little-endian and allowing unaligned reads.
fn vectorize_data<T: LeBytes + Copy>(
    base: *const u8,
    count: usize,
    elem_size: usize,
    tmp: &mut Vec<T>,
) -> Result<(), VectorizeError> {
    if base.is_null() {
        return Err(VectorizeError::NullBasePtr);
    }
    if elem_size != size_of::<T>() {
        return Err(VectorizeError::ElemSizeMismatch {
            elem_size,
            expected: size_of::<T>(),
        });
    }
    if count == 0 {
        return Err(VectorizeError::ZeroCount);
    }
    let _ = count
        .checked_mul(elem_size)
        .ok_or(VectorizeError::Overflow)?;

    // allocate once; then fill with ptr writes
    tmp.reserve_exact(count);
    unsafe {
        let mut p = base;
        let dst = tmp.as_mut_ptr().add(tmp.len());
        for i in 0..count {
            let v = ptr::read_unaligned(p as *const T);
            dst.add(i).write(v);
            p = p.add(elem_size);
        }
        tmp.set_len(tmp.len() + count);
    }
    Ok(())
}

/// Internal helper: decode payload as `count` elements of `T` into `out`.
fn extract_typed_into<T: LeBytes + Copy>(
    view: &SedsPacketView,
    elem_size: usize,
    count: usize,
    out: *mut T,
) -> Result<(), VectorizeError> {
    let mut tmp: Vec<T> = Vec::with_capacity(count);
    vectorize_data::<T>(view.payload, count, elem_size, &mut tmp)?;
    unsafe {
        ptr::copy_nonoverlapping(tmp.as_ptr(), out, count);
    }
    Ok(())
}

/// Typed extractor that mirrors the logging API.
///
/// - `elem_size` must be 1,2,4,8.
/// - `elem_kind` is one of SEDS_EK_UNSIGNED / SEDS_EK_SIGNED / SEDS_EK_FLOAT.
/// - Query mode: if `out` is NULL, `count` is 0, or `count < needed`,
///   returns the element count without writing.
/// - On success, returns `needed` (element count).
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_get_typed(
    pkt: *const SedsPacketView,
    out: *mut c_void,
    count: usize,     // number of elements available in `out`
    elem_size: usize, // must be 1,2,4,8
    elem_kind: u32,   // 0=unsigned,1=signed,2=float
) -> i32 {
    // Basic argument validation
    if pkt.is_null() || !width_is_valid(elem_size) {
        return status_from_err(TelemetryError::BadArg);
    }

    let view = unsafe { &*pkt };

    // Empty payload → nothing to read, success
    if view.payload_len == 0 {
        return 0;
    }

    if view.payload.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    // Payload length must be divisible by element size
    if elem_size == 0 || view.payload_len % elem_size != 0 {
        return status_from_err(TelemetryError::BadArg);
    }

    let needed = view.payload_len / elem_size;

    // Query / too-small mode → return required element count
    if out.is_null() || count == 0 || count < needed {
        return needed as i32;
    }

    // Now actually decode into the caller's buffer based on kind + size
    let res = match (elem_kind, elem_size) {
        (SEDS_EK_UNSIGNED, 1) => extract_typed_into::<u8>(view, elem_size, needed, out as *mut u8),
        (SEDS_EK_UNSIGNED, 2) => {
            extract_typed_into::<u16>(view, elem_size, needed, out as *mut u16)
        }
        (SEDS_EK_UNSIGNED, 4) => {
            extract_typed_into::<u32>(view, elem_size, needed, out as *mut u32)
        }
        (SEDS_EK_UNSIGNED, 8) => {
            extract_typed_into::<u64>(view, elem_size, needed, out as *mut u64)
        }
        (SEDS_EK_UNSIGNED, 16) => {
            extract_typed_into::<u128>(view, elem_size, needed, out as *mut u128)
        }

        (SEDS_EK_SIGNED, 1) => extract_typed_into::<i8>(view, elem_size, needed, out as *mut i8),
        (SEDS_EK_SIGNED, 2) => extract_typed_into::<i16>(view, elem_size, needed, out as *mut i16),
        (SEDS_EK_SIGNED, 4) => extract_typed_into::<i32>(view, elem_size, needed, out as *mut i32),
        (SEDS_EK_SIGNED, 8) => extract_typed_into::<i64>(view, elem_size, needed, out as *mut i64),
        (SEDS_EK_SIGNED, 16) => extract_typed_into::<i128>(view, elem_size, needed, out as *mut i128),

        (SEDS_EK_FLOAT, 4) => extract_typed_into::<f32>(view, elem_size, needed, out as *mut f32),
        (SEDS_EK_FLOAT, 8) => extract_typed_into::<f64>(view, elem_size, needed, out as *mut f64),

        // Unknown kind/size combo
        _ => Err(VectorizeError::ElemSizeMismatch {
            elem_size,
            expected: elem_size, // dummy; we just map to BadArg below
        }),
    };

    match res {
        Ok(()) => needed as i32,
        Err(_) => status_from_err(TelemetryError::BadArg),
    }
}

// ============================================================================
//  Serialization / deserialization helpers
// ============================================================================

/// Return the serialized size of a packet view, in bytes, or an error code.
///
/// This will:
/// - Convert the view to a `TelemetryPacket`
/// - Validate the packet against its schema
/// - Return the wire size
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_serialize_len(view: *const SedsPacketView) -> i32 {
    if view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let view = unsafe { &*view };
    // Convert first so we validate fields before reporting size
    let pkt = match view_to_packet(view) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::BadArg),
    };
    // Optional: validate schema before reporting size
    if let Err(e) = pkt.validate() {
        return status_from_err(e);
    }
    packet_wire_size(&pkt) as i32
}

/// Serialize a packet view into a caller-provided buffer.
///
/// Query mode:
/// - If `out` is NULL, `out_len` is 0, or `out_len < needed`,
///   returns the required size without writing.
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_serialize(
    view: *const SedsPacketView,
    out: *mut u8,
    out_len: usize,
) -> i32 {
    if view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let view = unsafe { &*view };
    let pkt = match view_to_packet(view) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::BadArg),
    };
    if let Err(e) = pkt.validate() {
        return status_from_err(e);
    }

    let bytes = serialize_packet(&pkt);
    let needed = bytes.len();

    // Query mode or too-small: return required size (not success)
    if out.is_null() || out_len == 0 || out_len < needed {
        return needed as i32;
    }

    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), out, needed);
    }
    needed as i32
}

/// Deserialize a serialized packet into an owned `SedsOwnedPacket`.
///
/// Returns NULL on failure.
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_deserialize_owned(bytes: *const u8, len: usize) -> *mut SedsOwnedPacket {
    if len > 0 && bytes.is_null() {
        return ptr::null_mut();
    }
    let slice = unsafe { slice::from_raw_parts(bytes, len) };

    let tpkt = match deserialize_packet(slice) {
        Ok(p) => p,
        Err(_) => return ptr::null_mut(),
    };
    if let Err(_e) = tpkt.validate() {
        return ptr::null_mut();
    }

    let endpoints_u32: Vec<u32> = tpkt.endpoints().iter().map(|e| *e as u32).collect();
    let owned = SedsOwnedPacket {
        inner: tpkt,
        endpoints_u32,
    };
    Box::into_raw(Box::new(owned))
}

/// Free an owned packet previously allocated by `seds_pkt_deserialize_owned`.
#[unsafe(no_mangle)]
pub extern "C" fn seds_owned_pkt_free(p: *mut SedsOwnedPacket) {
    if p.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(p));
    }
}

/// Build a transient `SedsPacketView` pointing into an owned packet.
///
/// The view borrows from the owned packet and is only valid as long as the
/// `SedsOwnedPacket` remains alive.
#[unsafe(no_mangle)]
pub extern "C" fn seds_owned_pkt_view(
    pkt: *const SedsOwnedPacket,
    out_view: *mut SedsPacketView,
) -> i32 {
    if pkt.is_null() || out_view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let pkt = unsafe { &*pkt };
    let inner = &pkt.inner;

    // sender bytes (Arc<str> -> &[u8])
    let sender_bytes = inner.sender().as_bytes();

    let view = SedsPacketView {
        ty: inner.data_type() as u32,
        data_size: inner.data_size(),
        sender: sender_bytes.as_ptr() as *const c_char,
        sender_len: sender_bytes.len(),
        endpoints: pkt.endpoints_u32.as_ptr(),
        num_endpoints: pkt.endpoints_u32.len(),
        timestamp: inner.timestamp(),
        payload: inner.payload().as_ptr(),
        payload_len: inner.payload().len(),
    };

    unsafe {
        *out_view = view;
    }
    status_from_result_code(SedsResult::SedsOk)
}

/// Validate a serialized packet against the schema.
///
/// Returns 0 on success or a negative error code.
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_validate_serialized(bytes: *const u8, len: usize) -> i32 {
    if len > 0 && bytes.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let slice = unsafe { slice::from_raw_parts(bytes, len) };
    match deserialize_packet(slice) {
        Ok(p) => match p.validate() {
            Ok(()) => status_from_result_code(SedsResult::SedsOk),
            Err(e) => status_from_err(e),
        },
        Err(_e) => status_from_err(TelemetryError::Deserialize("bad packet")),
    }
}

/// Deserialize only the header/envelope of a serialized packet.
///
/// This avoids allocating/copying the payload and returns an owned header
/// object usable to construct a `SedsPacketView` without the payload.
#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_deserialize_header_owned(
    bytes: *const u8,
    len: usize,
) -> *mut SedsOwnedHeader {
    if len > 0 && bytes.is_null() {
        return ptr::null_mut();
    }
    let slice = unsafe { slice::from_raw_parts(bytes, len) };

    // Header-only peek: type, endpoints, sender, timestamp
    let env = match peek_envelope(slice) {
        Ok(e) => e,
        Err(_) => return ptr::null_mut(),
    };

    let endpoints_u32: Vec<u32> = env.endpoints.iter().map(|&e| e as u32).collect();
    let owned = SedsOwnedHeader {
        ty: env.ty as u32,
        sender: env.sender,
        endpoints_u32,
        timestamp: env.timestamp_ms,
    };
    Box::into_raw(Box::new(owned))
}

/// Free an owned header previously allocated by `seds_pkt_deserialize_header_owned`.
#[unsafe(no_mangle)]
pub extern "C" fn seds_owned_header_free(h: *mut SedsOwnedHeader) {
    if h.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(h));
    }
}

/// Build a `SedsPacketView` from an owned header, with no payload.
///
/// The resulting view has `data_size == 0` and `payload == NULL`.
#[unsafe(no_mangle)]
pub extern "C" fn seds_owned_header_view(
    h: *const SedsOwnedHeader,
    out_view: *mut SedsPacketView,
) -> i32 {
    if h.is_null() || out_view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let h = unsafe { &*h };
    let sender_bytes = h.sender.as_bytes();

    let view = SedsPacketView {
        ty: h.ty,
        data_size: 0, // unknown in envelope
        sender: sender_bytes.as_ptr() as *const c_char,
        sender_len: sender_bytes.len(),
        endpoints: h.endpoints_u32.as_ptr(),
        num_endpoints: h.endpoints_u32.len(),
        timestamp: h.timestamp,
        payload: ptr::null(),
        payload_len: 0,
    };

    unsafe {
        *out_view = view;
    }
    status_from_result_code(SedsResult::SedsOk)
}
