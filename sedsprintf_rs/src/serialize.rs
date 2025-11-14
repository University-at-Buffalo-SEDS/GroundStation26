//! Serialization and deserialization of telemetry packets.
//!
//! This module defines the compact v2 wire format used to send and receive
//! [`TelemetryPacket`]s, along with:
//! - [`serialize_packet`] / [`deserialize_packet`] for full packets.
//! - [`peek_envelope`] for header-only inspection without touching the payload.
//! - Size helpers like [`header_size_bytes`] and [`packet_wire_size`].
//!
//! The core public type here is [`TelemetryEnvelope`], a lightweight view of
//! the header fields used by `peek_envelope`.

use crate::{
    telemetry_packet::{DataEndpoint, TelemetryPacket}, try_enum_from_u32,
    TelemetryError,
    TelemetryResult,
    {config::DataType, MAX_VALUE_DATA_ENDPOINT, MAX_VALUE_DATA_TYPE},
};
use alloc::{sync::Arc, vec::Vec};


/// Lightweight header-only view of a serialized [`TelemetryPacket`].
///
/// Produced by [`peek_envelope`] without allocating or copying the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelemetryEnvelope {
    /// Telemetry [`DataType`] discriminant.
    pub ty: DataType,
    /// All endpoints this packet is destined for (set bits in the bitmap).
    pub endpoints: Arc<[DataEndpoint]>,
    /// Sender identity as UTF-8 string.
    pub sender: Arc<str>,
    /// Timestamp in milliseconds (as stored on the wire).
    pub timestamp_ms: u64,
}

// ===========================================================================
// Compact v2 wire format
// ===========================================================================
//
// Layout:
//
//   [NEP: u8]
//       Number of selected endpoints (i.e. bits set in the endpoint bitmap).
//
//   VARINT(ty: u32 as u64)       -- ULEB128
//   VARINT(data_size: u64)       -- ULEB128
//   VARINT(timestamp: u64)       -- ULEB128
//   VARINT(sender_len: u64)      -- ULEB128
//
//   ENDPOINTS_BITMAP             -- 1 bit per possible DataEndpoint; LSB-first
//   SENDER BYTES                 -- UTF-8, length = sender_len
//   PAYLOAD BYTES                -- raw payload, length = data_size
//
// Notes:
// - Bitmap has one bit per possible `DataEndpoint` discriminant
//   in the range `0..=MAX_VALUE_DATA_ENDPOINT`.
// - NEP is the count of bits set in the bitmap (not the total possible).
// - VARINT fields use unsigned LEB128 encoding (ULEB128).
//
// ===========================================================================
// ULEB128 (varint) encoding helpers
// ===========================================================================

/// Encode a `u64` as ULEB128 and append it to `out`.
#[inline]
fn write_uleb128<T>(mut v: u64, out: &mut Vec<T>)
where
    T: From<u8>,
{
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(T::from(byte));
        if v == 0 {
            break;
        }
    }
}

/// Decode a ULEB128-encoded `u64` from the given reader.
#[inline]
fn read_uleb128(r: &mut ByteReader) -> Result<u64, TelemetryError> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    // u64 fits in at most 10 ULEB128 bytes.
    for _ in 0..10 {
        let b = r.read_bytes(1)?[0];
        result |= ((b & 0x7F) as u64) << shift;
        if (b & 0x80) == 0 {
            return Ok(result);
        }
        shift += 7;
    }
    Err(TelemetryError::Deserialize("uleb128 too long"))
}

/// Compute the encoded length (in bytes) of a ULEB128-encoded `u64`.
#[inline]
fn uleb128_size(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

/// Count the total number of bits set across all bytes of the bitmap.
#[inline]
fn bitmap_popcount(bm: &[u8]) -> usize {
    bm.iter().map(|b| b.count_ones() as usize).sum()
}

// ===========================================================================
// ByteReader: tiny cursor over a byte slice
// ===========================================================================

#[derive(Clone, Copy)]
struct ByteReader<'a> {
    buf: &'a [u8],
    off: usize,
}

impl<'a> ByteReader<'a> {
    /// Create a new reader over the given buffer starting at offset 0.
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, off: 0 }
    }

    /// Remaining bytes that can still be read.
    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.off)
    }

    /// Read exactly `n` bytes, advancing the internal offset.
    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], TelemetryError> {
        if self.remaining() < n {
            return Err(TelemetryError::Deserialize("short read"));
        }
        let s = &self.buf[self.off..self.off + n];
        self.off += n;
        Ok(s)
    }
}

// ===========================================================================
// Endpoint bitmap constants and helpers
// ===========================================================================

/// Number of bits needed to cover all possible `DataEndpoint` discriminants.
const EP_BITMAP_BITS: usize = (MAX_VALUE_DATA_ENDPOINT as usize) + 1;

/// Number of bytes required to store [`EP_BITMAP_BITS`] bits.
const EP_BITMAP_BYTES: usize = (EP_BITMAP_BITS + 7) / 8;

/// Build a compact endpoint bitmap from the provided list of endpoints.
///
/// Each endpoint `ep` sets the bit at position `ep as u32` in the bitmap.
/// Bits are packed LSB-first within each byte.
#[inline]
fn build_endpoint_bitmap(eps: &[DataEndpoint]) -> [u8; EP_BITMAP_BYTES] {
    let mut bm = [0u8; EP_BITMAP_BYTES];
    for &ep in eps {
        let idx = ep as u32 as usize;
        debug_assert!(idx < EP_BITMAP_BITS, "endpoint discriminant out of range");
        if idx < EP_BITMAP_BITS {
            let byte = idx / 8;
            let bit = idx % 8;
            bm[byte] |= 1u8 << bit;
        }
    }
    bm
}

/// Expand a bitmap of endpoints into a dense array and its logical length.
///
/// Returns `(array, len)` where:
/// - `array[0..len]` are the active endpoints in ascending discriminant order.
/// - `array[len..]` is filled with a dummy `DataEndpoint` and should be ignored.
fn expand_endpoint_bitmap(
    bm: &[u8],
) -> Result<([DataEndpoint; EP_BITMAP_BITS], usize), TelemetryError> {
    if bm.len() != EP_BITMAP_BYTES {
        return Err(TelemetryError::Deserialize("bad endpoint bitmap size"));
    }

    // Pick *any* valid endpoint as filler/dummy for the array.
    let dummy = DataEndpoint::try_from_u32(0).expect("0 must be a valid DataEndpoint discriminant");

    // Entire array is initialized to a valid value ⇒ fully safe.
    let mut arr = [dummy; EP_BITMAP_BITS];

    let mut len = 0usize;
    for idx in 0..EP_BITMAP_BITS {
        let byte = idx / 8;
        let bit = idx % 8;
        if (bm[byte] >> bit) & 1 != 0 {
            let v = idx as u32;
            let ep = DataEndpoint::try_from_u32(v)
                .ok_or(TelemetryError::Deserialize("bad endpoint bit set"))?;
            arr[len] = ep;
            len += 1;
        }
    }

    Ok((arr, len))
}

// ===========================================================================
// Serialization
// ===========================================================================

/// Serialize a [`TelemetryPacket`] into the compact v2 wire format.
///
/// The returned `Arc<[u8]>` owns the encoded bytes and can be shared cheaply.
/// # Arguments
/// - `pkt`: Telemetry packet to serialize.
///
/// # Returns
/// - `Arc<[u8]>`: Serialized packet in compact v2 wire format.
pub fn serialize_packet(pkt: &TelemetryPacket) -> Arc<[u8]> {
    let bm = build_endpoint_bitmap(&pkt.endpoints());

    // Heuristic capacity: fixed prelude + bitmap + sender + payload.
    let mut out =
        Vec::with_capacity(16 + EP_BITMAP_BYTES + pkt.sender().len() + pkt.payload().len());

    // Prelude: NEP = number of UNIQUE endpoints (bits set in bitmap).
    let nep_unique = bitmap_popcount(&bm);
    out.push(nep_unique as u8);

    write_uleb128(pkt.data_type() as u64, &mut out);
    write_uleb128(pkt.data_size() as u64, &mut out);
    write_uleb128(pkt.timestamp(), &mut out);
    write_uleb128(pkt.sender().len() as u64, &mut out);

    out.extend_from_slice(&bm);
    out.extend_from_slice(pkt.sender().as_bytes());
    out.extend_from_slice(&pkt.payload());

    Arc::<[u8]>::from(out)
}

// ===========================================================================
// Deserialization (full packet)
// ===========================================================================

/// Deserialize a full [`TelemetryPacket`] from the compact v2 wire format.
/// # Arguments
/// - `buf`: Byte slice containing the serialized packet.
/// # Returns
/// - `TelemetryPacket`: Deserialized telemetry packet.
/// # Errors
/// - `TelemetryError::Deserialize` if the buffer is malformed.
/// - `TelemetryError::InvalidType` if the data type is invalid.
pub fn deserialize_packet(buf: &[u8]) -> Result<TelemetryPacket, TelemetryError> {
    if buf.is_empty() {
        return Err(TelemetryError::Deserialize("short prelude"));
    }
    let mut r = ByteReader::new(buf);

    let nep = r.read_bytes(1)?[0] as usize;
    let ty_v = read_uleb128(&mut r)?;
    let dsz = read_uleb128(&mut r)? as usize;
    let ts_v = read_uleb128(&mut r)?;
    let slen = read_uleb128(&mut r)? as usize;

    if r.remaining() < EP_BITMAP_BYTES + slen + dsz {
        return Err(TelemetryError::Deserialize("short buffer"));
    }

    let bm = r.read_bytes(EP_BITMAP_BYTES)?;
    let (ep_buf, ep_len) = expand_endpoint_bitmap(bm)?;
    if ep_len != nep {
        return Err(TelemetryError::Deserialize("endpoint count mismatch"));
    }
    let eps: Arc<[DataEndpoint]> = Arc::from(&ep_buf[..ep_len]);

    let sender_bytes = r.read_bytes(slen)?;
    let sender_str = core::str::from_utf8(sender_bytes)
        .map_err(|_| TelemetryError::Deserialize("sender not UTF-8"))?;
    let payload_slice = r.read_bytes(dsz)?;

    let ty_u32 = u32::try_from(ty_v).map_err(|_| TelemetryError::Deserialize("type too large"))?;
    if ty_u32 > MAX_VALUE_DATA_TYPE {
        return Err(TelemetryError::InvalidType);
    }
    let ty = DataType::try_from_u32(ty_u32).ok_or(TelemetryError::InvalidType)?;

    TelemetryPacket::new(
        ty,
        &eps,
        Arc::<str>::from(sender_str),
        ts_v,
        Arc::<[u8]>::from(payload_slice),
    )
}

// ===========================================================================
// Peek / envelope-only decode
// ===========================================================================

/// Decode only the header/envelope of a serialized packet.
///
/// This reads just enough bytes to determine:
/// - [`TelemetryEnvelope::ty`]
/// - [`TelemetryEnvelope::endpoints`]
/// - [`TelemetryEnvelope::sender`]
/// - [`TelemetryEnvelope::timestamp_ms`]
///
/// It **does not** touch or allocate the payload.
/// # Arguments
/// - `buf`: Byte slice containing the serialized packet.
/// # Returns
/// - `TelemetryEnvelope`: Decoded telemetry envelope.
/// # Errors
/// - `TelemetryError::Deserialize` if the buffer is malformed.
/// - `TelemetryError::InvalidType` if the data type is invalid.
pub fn peek_envelope(buf: &[u8]) -> TelemetryResult<TelemetryEnvelope> {
    if buf.is_empty() {
        return Err(TelemetryError::Deserialize("short prelude"));
    }
    let mut r = ByteReader::new(buf);

    let nep = r.read_bytes(1)?[0] as usize;
    let ty_v = read_uleb128(&mut r)?;
    let _dsz = read_uleb128(&mut r)? as usize;
    let ts_v = read_uleb128(&mut r)?;
    let slen = read_uleb128(&mut r)? as usize;

    if r.remaining() < EP_BITMAP_BYTES + slen {
        return Err(TelemetryError::Deserialize("short buffer"));
    }

    let bm = r.read_bytes(EP_BITMAP_BYTES)?;
    let (ep_buf, ep_len) = expand_endpoint_bitmap(bm)?;
    if ep_len != nep {
        return Err(TelemetryError::Deserialize("endpoint count mismatch"));
    }
    let eps: Arc<[DataEndpoint]> = Arc::from(&ep_buf[..ep_len]);

    let sender_bytes = r.read_bytes(slen)?;
    let sender_str = core::str::from_utf8(sender_bytes)
        .map_err(|_| TelemetryError::Deserialize("sender not UTF-8"))?;

    let ty_u32 = u32::try_from(ty_v).map_err(|_| TelemetryError::Deserialize("type too large"))?;
    if ty_u32 > MAX_VALUE_DATA_TYPE {
        return Err(TelemetryError::InvalidType);
    }
    let ty = DataType::try_from_u32(ty_u32).ok_or(TelemetryError::InvalidType)?;

    Ok(TelemetryEnvelope {
        ty,
        endpoints: Arc::<[_]>::from(eps),
        sender: Arc::<str>::from(sender_str),
        timestamp_ms: ts_v,
    })
}

// ===========================================================================
// Size helpers
// ===========================================================================

/// Compute the size (in bytes) of just the header portion of `pkt`
/// *excluding* the endpoint bitmap, sender string, and payload.
///
/// This is mainly useful for preallocation or introspection.
/// # Arguments
/// - `pkt`: Telemetry packet to measure.
/// # Returns
/// - `usize`: Size of the header in bytes.
pub fn header_size_bytes(pkt: &TelemetryPacket) -> usize {
    let prelude = 1; // NEP (u8)
    prelude
        + uleb128_size(pkt.data_type() as u32 as u64)
        + uleb128_size(pkt.data_size() as u64)
        + uleb128_size(pkt.timestamp())
        + uleb128_size(pkt.sender().len() as u64)
}

/// Compute the total wire size (header + bitmap + sender + payload) in bytes.
/// This is mainly useful for preallocation or introspection.
/// # Arguments
/// - `pkt`: Telemetry packet to measure.
/// # Returns
/// - `usize`: Total size of the serialized packet in bytes.
pub fn packet_wire_size(pkt: &TelemetryPacket) -> usize {
    header_size_bytes(pkt) + EP_BITMAP_BYTES + pkt.sender().len() + pkt.payload().len()
}

// ===========================================================================
// Enum conversions (u32 ↔ enums)
// ===========================================================================

impl DataType {
    /// Convert a raw `u32` discriminant to `DataType`, returning `None` if out
    /// of range or invalid.
    /// # Arguments
    /// - `x`: Raw `u32` discriminant.
    /// # Returns
    /// - `Option<DataType>`: Corresponding `DataType` variant or `None
    pub fn try_from_u32(x: u32) -> Option<Self> {
        try_enum_from_u32(x)
    }
}

impl DataEndpoint {
    /// Convert a raw `u32` discriminant to `DataEndpoint`, returning `None` if
    /// out of range or invalid.
    /// # Arguments
    /// - `x`: Raw `u32` discriminant.
    /// # Returns
    /// - `Option<DataEndpoint>`: Corresponding `DataEndpoint` variant or `None
    pub fn try_from_u32(x: u32) -> Option<Self> {
        try_enum_from_u32(x)
    }
}
