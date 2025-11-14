//! Telemetry packet core type and formatting helpers.
//!
//! [`TelemetryPacket`] is the main payload-bearing type. It:
//! - holds sender, endpoints, timestamp and raw payload bytes,
//! - validates payload sizes and encodings against the schema from `message_meta`,
//! - supports pretty printing (header + decoded values) for debugging/logging,
//! - uses [`SmallPayload`] internally to keep small messages on the stack.

use crate::config::MAX_PRECISION_IN_STRINGS;
use crate::small_payload::SmallPayload;
pub(crate) use crate::{
    config::{DataEndpoint, DataType, DEVICE_IDENTIFIER}, data_type_size, get_data_type, get_info_type, message_meta,
    router::LeBytes,
    MessageDataType, MessageElementCount, MessageType, TelemetryError,
    TelemetryResult,
};
use alloc::{string::String, sync::Arc, vec::Vec};
use core::any::TypeId;
use core::fmt::Write;

// ============================================================================
// Constants
// ============================================================================

/// Threshold (in ms since boot/epoch) above which timestamps are treated as
/// Unix epoch time rather than an uptime counter.
///
/// Anything smaller is formatted as an uptime-style duration; larger values
/// are formatted as a UTC date-time.
const EPOCH_MS_THRESHOLD: u64 = 1_000_000_000_000; // clearly not an uptime counter

/// Default starting capacity for human-readable strings.
const DEFAULT_STRING_CAPACITY: usize = 96;

// ============================================================================
// TelemetryPacket
// ============================================================================

/// Payload-bearing packet (safe, validated, shareable).
///
/// This is the primary data structure passed around inside the crate and
/// across FFI boundaries (via views / wrappers).
#[derive(Clone, Debug)]
pub struct TelemetryPacket {
    /// Logical message type (schema selector).
    ty: DataType,

    /// Size of the payload in bytes.
    ///
    /// This is cached and must match `payload.len()`. [`TelemetryPacket::validate`]
    /// checks the invariant.
    data_size: usize,

    /// Logical sender identifier (e.g. device or subsystem name).
    sender: Arc<str>,

    /// Destination endpoints for this message.
    endpoints: Arc<[DataEndpoint]>,

    /// Timestamp in milliseconds.
    ///
    /// - If `< EPOCH_MS_THRESHOLD`, treated as an uptime counter and formatted
    ///   like `12m 34s 567ms`.
    /// - If `>= EPOCH_MS_THRESHOLD`, treated as Unix epoch ms and formatted
    ///   as `YYYY-MM-DD HH:MM:SS.mmmZ`.
    timestamp: u64,

    /// Raw payload bytes, stored via [`SmallPayload`] for small/large optimization.
    payload: SmallPayload,
}

// ============================================================================
// Internal helpers for validation / formatting
// ============================================================================

/// Effective element width (in bytes) for the given message data type.
///
/// For numeric/bool types this is the true width of one element.
/// For string/hex we return 1 to treat the payload as a byte stream when
/// checking dynamic length multiples.
#[inline]
const fn element_width(dt: MessageDataType) -> usize {
    match dt {
        MessageDataType::UInt8 | MessageDataType::Int8 | MessageDataType::Bool => 1,
        MessageDataType::UInt16 | MessageDataType::Int16 => 2,
        MessageDataType::UInt32 | MessageDataType::Int32 | MessageDataType::Float32 => 4,
        MessageDataType::UInt64 | MessageDataType::Int64 | MessageDataType::Float64 => 8,
        MessageDataType::UInt128 | MessageDataType::Int128 => 16,
        // For String/Hex we treat width as 1 (byte granularity) when checking dynamic multiples.
        MessageDataType::String | MessageDataType::Binary => 1,
    }
}

/// Validate the payload of a dynamic-length message.
///
/// - For `String`: trims trailing NULs for validation and ensures UTF-8 (if non-empty).
/// - For `Hex`: no additional validation.
/// - For numerics/bool: ensures the length is a multiple of the element width.
#[inline]
fn validate_dynamic_len_and_content(ty: DataType, bytes: &[u8]) -> TelemetryResult<()> {
    let dt = get_data_type(ty);
    match dt {
        MessageDataType::String => {
            // Trim trailing NULs for validation, but do not copy.
            let end = bytes
                .iter()
                .rposition(|&b| b != 0)
                .map(|i| i + 1)
                .unwrap_or(0);
            // Empty string is OK; otherwise ensure valid UTF-8.
            if end > 0 {
                core::str::from_utf8(&bytes[..end]).map_err(|_| TelemetryError::InvalidUtf8)?;
            }
            Ok(())
        }
        MessageDataType::Binary => {
            // No UTF-8 requirement for hex blobs.
            Ok(())
        }
        _ => {
            // Numeric / bool: length must be a multiple of the element width.
            let w = element_width(dt);
            if w == 0 || bytes.len() % w != 0 {
                return Err(TelemetryError::SizeMismatch {
                    expected: w,
                    got: bytes.len(),
                });
            }
            Ok(())
        }
    }
}

// ============================================================================
// TelemetryPacket impl
// ============================================================================

impl TelemetryPacket {
    /// Create a packet from a raw payload, validating against `message_meta(ty)`.
    ///
    /// Checks:
    /// - `endpoints` is non-empty.
    /// - For static element count:
    ///   - `payload.len() == element_count * data_type_size(get_data_type(ty))`.
    /// - For dynamic:
    ///   - Length and encoding are validated by [`validate_dynamic_len_and_content`].
    /// # Arguments
    /// - `ty`: logical message type (schema selector).
    /// - `endpoints`: destination endpoint list (must be non-empty).
    /// - `sender`: logical sender identifier (e.g. device or subsystem name).
    /// - `timestamp`: timestamp in milliseconds.
    /// - `payload`: raw payload bytes.
    /// # Returns
    /// - `Ok(TelemetryPacket)` if validation passes.
    /// - `Err(TelemetryError)` if validation fails.
    /// # Errors
    /// - [`TelemetryError::EmptyEndpoints`] if `endpoints` is empty.
    /// - [`TelemetryError::SizeMismatch`] if the payload size does not match
    ///   the expected size for static element counts, or is not a multiple
    ///   of the element width for dynamic types.
    /// - [`TelemetryError::InvalidUtf8`] if the payload is a string
    ///   type and is not valid UTF-8 .
    pub fn new(
        ty: DataType,
        endpoints: &[DataEndpoint],
        sender: impl Into<Arc<str>>,
        timestamp: u64,
        payload: Arc<[u8]>,
    ) -> TelemetryResult<Self> {
        if endpoints.is_empty() {
            return Err(TelemetryError::EmptyEndpoints);
        }

        let meta = message_meta(ty);
        match meta.element_count {
            MessageElementCount::Static(need) => {
                if payload.len() != (need * data_type_size(get_data_type(ty))) {
                    return Err(TelemetryError::SizeMismatch {
                        expected: need,
                        got: payload.len(),
                    });
                }
            }
            MessageElementCount::Dynamic => {
                validate_dynamic_len_and_content(ty, &payload)?;
            }
        }

        Ok(Self {
            ty,
            data_size: payload.len(),
            sender: sender.into(),
            endpoints: Arc::<[DataEndpoint]>::from(endpoints),
            timestamp,
            payload: SmallPayload::new(&payload),
        })
    }

    /// Convenience: create from a slice of `u8` (copied).
    ///
    /// Uses [`DEVICE_IDENTIFIER`] as the sender.
    /// # Arguments
    /// - `ty`: logical message type (schema selector).
    /// - `bytes`: raw payload bytes.
    /// - `endpoints`: destination endpoint list (must be non-empty).
    /// - `timestamp`: timestamp in milliseconds.
    /// # Returns
    /// - `Ok(TelemetryPacket)` if validation passes.
    /// - `Err(TelemetryError)` if validation fails.
    #[allow(dead_code)]
    pub fn from_u8_slice(
        ty: DataType,
        bytes: &[u8],
        endpoints: &[DataEndpoint],
        timestamp: u64,
    ) -> TelemetryResult<Self> {
        Self::new(
            ty,
            endpoints,
            Arc::<str>::from(DEVICE_IDENTIFIER),
            timestamp,
            Arc::<[u8]>::from(bytes),
        )
    }

    /// Convenience: create from a slice of `f32` (copied, little-endian).
    ///
    /// Uses [`DEVICE_IDENTIFIER`] as the sender. The values are packed as
    /// little-endian floats into the payload buffer.
    /// # Arguments
    /// - `ty`: logical message type (schema selector).
    /// - `values`: slice of `f32` values to encode.
    /// - `endpoints`: destination endpoint list (must be non-empty).
    /// - `timestamp`: timestamp in milliseconds.
    /// # Returns
    /// - `Ok(TelemetryPacket)` if validation passes.
    /// - `Err(TelemetryError)` if validation fails.
    #[allow(dead_code)]
    pub fn from_f32_slice(
        ty: DataType,
        values: &[f32],
        endpoints: &[DataEndpoint],
        timestamp: u64,
    ) -> TelemetryResult<Self> {
        let need = values.len() * 4;

        // Optional pre-check (can be dropped if we’re happy to let `new()` complain).
        let meta = message_meta(ty);
        if let MessageElementCount::Static(exact) = meta.element_count {
            if need != exact * data_type_size(MessageDataType::Float32) {
                return Err(TelemetryError::SizeMismatch {
                    expected: exact,
                    got: need,
                });
            }
        }

        let mut bytes = Vec::with_capacity(need);
        unsafe { bytes.set_len(need) };
        for (i, v) in values.iter().copied().enumerate() {
            let b = v.to_le_bytes();
            let off = i * 4;
            bytes[off..off + 4].copy_from_slice(&b);
        }

        Self::new(
            ty,
            endpoints,
            Arc::<str>::from(DEVICE_IDENTIFIER),
            timestamp,
            Arc::<[u8]>::from(bytes),
        )
    }

    /// Validate basic invariants:
    ///
    /// - `endpoints` is non-empty.
    /// - `payload.len() == data_size`.
    /// - For static element count:
    ///   - `data_size == element_count * data_type_size(get_data_type(ty))`.
    /// - For dynamic:
    ///   - Length and encoding are validated by [`validate_dynamic_len_and_content`].
    /// # Returns
    /// - `Ok(())` if validation passes.
    /// - `Err(TelemetryError)` if validation fails.
    pub fn validate(&self) -> TelemetryResult<()> {
        if self.endpoints.is_empty() {
            return Err(TelemetryError::EmptyEndpoints);
        }
        if self.payload.len() != self.data_size {
            return Err(TelemetryError::SizeMismatch {
                expected: self.data_size,
                got: self.payload.len(),
            });
        }

        let meta = message_meta(self.ty);
        match meta.element_count {
            MessageElementCount::Static(need) => {
                if self.data_size != (need * data_type_size(get_data_type(self.ty))) {
                    return Err(TelemetryError::SizeMismatch {
                        expected: need,
                        got: self.data_size,
                    });
                }
            }
            MessageElementCount::Dynamic => {
                validate_dynamic_len_and_content(self.ty, &self.payload)?;
            }
        }
        Ok(())
    }

    /* ---- Getters ---- */
    /// Get the message data type.
    /// This is the logical schema selector.
    pub fn data_type(&self) -> DataType {
        self.ty
    }
    /// Get the sender identifier.
    /// This is typically a device or subsystem name.
    pub fn sender(&self) -> &str {
        &self.sender
    }
    /// Get the destination endpoints for this message.
    pub fn endpoints(&self) -> &[DataEndpoint] {
        &self.endpoints
    }
    /// Get the timestamp in milliseconds.
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Get the payload size in bytes.
    pub fn data_size(&self) -> usize {
        self.data_size
    }
    /// Get the raw payload bytes.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Header-only string (no decoded data).
    ///
    /// Example:
    /// `Type: FOO, Data Size: 8, Sender: dev0, Endpoints: [EP_A, EP_B], Timestamp: 1234 (1s 234ms)`
    /// # Returns
    /// - Human-readable string with header fields.
    pub fn header_string(&self) -> String {
        let mut out = String::with_capacity(DEFAULT_STRING_CAPACITY);

        let _ = write!(
            &mut out,
            "Type: {}, Data Size: {}, Sender: {}, Endpoints: [",
            self.ty.as_str(),
            self.data_size,
            self.sender.as_ref(),
        );
        for (i, ep) in self.endpoints.iter().enumerate() {
            if i != 0 {
                out.push_str(", ");
            }
            out.push_str(ep.as_str());
        }
        out.push_str("], Timestamp: ");
        let _ = write!(&mut out, "{}", self.timestamp);

        out.push_str(" (");
        append_human_time(&mut out, self.timestamp);
        out.push(')');
        out
    }

    /// Borrow the payload as UTF-8 without trailing NULs (no allocation).
    ///
    /// Returns `None` if the message `DataType` is not a `String` type or if
    /// the payload is not valid UTF-8 (after trimming trailing NUL).
    /// # Returns
    /// - `Some(&str)` if the payload is a valid UTF-8 string.
    /// - `None` otherwise.
    pub fn data_as_utf8_ref(&self) -> Option<&str> {
        if get_data_type(self.ty) != MessageDataType::String {
            return None;
        }
        let bytes = &self.payload;
        let end = bytes.iter().rposition(|&b| b != 0).map(|i| i + 1)?;
        core::str::from_utf8(&bytes[..end]).ok()
    }

    /// Helper: append decoded numeric/float elements to `s`.
    ///
    /// - Uses `LeBytes::from_le_slice` with fixed-width chunks.
    /// - Floats (`f32`/`f64`) are formatted with a fixed precision
    ///   [`MAX_PRECISION_IN_STRINGS`].
    #[inline]
    fn data_to_string<T>(&self, s: &mut String)
    where
        T: LeBytes + core::fmt::Display + 'static,
    {
        let mut it = self.payload.chunks_exact(T::WIDTH);
        let mut first = true;

        while let Some(chunk) = it.next() {
            if !first {
                s.push_str(", ");
            }
            first = false;

            let v = T::from_le_slice(chunk);

            // If this is a float type, use precision; otherwise, default formatting.
            if TypeId::of::<T>() == TypeId::of::<f32>() || TypeId::of::<T>() == TypeId::of::<f64>()
            {
                // `{:.*}` = "use this precision argument"
                let _ = write!(s, "{:.*}", MAX_PRECISION_IN_STRINGS, v);
            } else {
                let _ = write!(s, "{v}");
            }
        }
    }

    /// Full pretty string including decoded data portion.
    ///
    /// - String payloads are rendered as `"..."`
    /// - Numeric/bool payloads are rendered as comma-separated values
    /// - Hex payloads are delegated to [`TelemetryPacket::to_hex_string`]
    /// # Returns
    /// - Human-readable string with header and decoded data.
    pub fn to_string(&self) -> String {
        let mut s = String::from("{");
        s.push_str(&self.header_string());

        if self.payload.is_empty() {
            s.push_str(", Data: (<empty>)}");
            return s;
        }

        if get_info_type(self.ty) == MessageType::Error {
            s.push_str(", Error: (");
        } else {
            s.push_str(", Data: (");
        }

        if let Some(msg) = self.data_as_utf8_ref() {
            s.push('"');
            s.push_str(msg);
            s.push_str("\")}");
            return s;
        }

        match get_data_type(self.ty) {
            MessageDataType::Float64 => {
                self.data_to_string::<f64>(&mut s);
            }
            MessageDataType::Float32 => {
                self.data_to_string::<f32>(&mut s);
            }
            MessageDataType::UInt128 => {
                self.data_to_string::<u128>(&mut s);
            }
            MessageDataType::UInt64 => {
                self.data_to_string::<u64>(&mut s);
            }
            MessageDataType::UInt32 => {
                self.data_to_string::<u32>(&mut s);
            }
            MessageDataType::UInt16 => {
                self.data_to_string::<u16>(&mut s);
            }
            MessageDataType::UInt8 => {
                // NOTE: this uses i8 for historical reasons; kept for compatibility.
                self.data_to_string::<i8>(&mut s);
            }
            MessageDataType::Int128 => {
                self.data_to_string::<i128>(&mut s);
            }
            MessageDataType::Int64 => {
                self.data_to_string::<i64>(&mut s);
            }
            MessageDataType::Int32 => {
                self.data_to_string::<i32>(&mut s);
            }
            MessageDataType::Int16 => {
                self.data_to_string::<i16>(&mut s);
            }
            MessageDataType::Int8 => {
                self.data_to_string::<i8>(&mut s);
            }
            MessageDataType::Bool => {
                // Interpret any nonzero as true.
                let mut it = self.payload.iter().peekable();
                while let Some(b) = it.next() {
                    let _ = write!(s, "{}", *b != 0);
                    if it.peek().is_some() {
                        s.push_str(", ");
                    }
                }
            }
            MessageDataType::String => {
                // Already handled above via `data_as_utf8_ref`.
            }
            MessageDataType::Binary => return self.to_hex_string(),
        }

        s.push_str(")}");
        s
    }

    /// Hex dump variant of [`TelemetryPacket::to_string`].
    ///
    /// Produces:
    ///
    /// `Type: ..., Data Size: ..., ..., Timestamp: ... (...), Data (hex): 0xNN 0xNN ...`
    /// # Returns
    /// - Human-readable string with header and hex-formatted data.
    pub fn to_hex_string(&self) -> String {
        // Header first.
        let mut s = self.header_string();
        s.push_str(", Data (hex):");

        if !self.payload.is_empty() {
            // Reserve roughly 5 chars per byte: " 0xNN".
            s.reserve(self.payload.len().saturating_mul(5));
            for &b in self.payload.iter() {
                let _ = write!(&mut s, " 0x{:02x}", b);
            }
        }
        s
    }
}

// ============================================================================
// Time formatting (no_std-friendly, UTC or uptime)
// ============================================================================

#[inline]
fn div_mod_u64(n: u64, d: u64) -> (u64, u64) {
    (n / d, n % d)
}

// Howard Hinnant–style civil-from-days (proleptic Gregorian).
fn civil_from_days(mut z: i64) -> (i32, u32, u32) {
    // epoch (1970-01-01) has days=0
    z += 719_468; // shift to civil base
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = (yoe as i32) + era as i32 * 400;
    let doy = (doe - (365 * yoe + yoe / 4 - yoe / 100)) as i32; // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = mp + if mp < 10 { 3 } else { -9 }; // [1, 12]
    let y = y + (m <= 2) as i32; // year
    (y, m as u32, d as u32)
}

/// Append a human-readable timestamp to `out`, either uptime (`hh:mm:ss.mmm`)
/// or UTC epoch like `YYYY-MM-DD HH:MM:SS.mmmZ`, depending on threshold.
fn append_human_time(out: &mut String, total_ms: u64) {
    if total_ms >= EPOCH_MS_THRESHOLD {
        // Unix epoch path.
        let (secs, sub_ms) = div_mod_u64(total_ms, 1_000);
        let days = (secs / 86_400) as i64;
        let sod = (secs % 86_400) as u32; // seconds of day
        let (year, month, day) = civil_from_days(days);
        let hour = sod / 3_600;
        let min = (sod % 3_600) / 60;
        let sec = sod % 60;
        let _ = Write::write_fmt(
            out,
            format_args!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}Z",
                year, month, day, hour, min, sec, sub_ms as u32
            ),
        );
    } else {
        // Uptime-style duration.
        let hours = total_ms / 3_600_000;
        let minutes = (total_ms % 3_600_000) / 60_000;
        let seconds = (total_ms % 60_000) / 1_000;
        let milliseconds = total_ms % 1_000;
        if hours > 0 {
            let _ = Write::write_fmt(
                out,
                format_args!("{hours}h {minutes:02}m {seconds:02}s {milliseconds:03}ms"),
            );
        } else if minutes > 0 {
            let _ = Write::write_fmt(
                out,
                format_args!("{minutes}m {seconds:02}s {milliseconds:03}ms"),
            );
        } else {
            let _ = Write::write_fmt(out, format_args!("{seconds}s {milliseconds:03}ms"));
        }
    }
}

// ============================================================================
// Display impl
// ============================================================================

impl core::fmt::Display for TelemetryPacket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&TelemetryPacket::to_string(self))
    }
}
