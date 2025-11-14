// std on host/tests; no_std when the `std` feature is OFF

// Dear programmer:
// When I wrote this code, only god and I knew how it worked.
// Now, only god knows it!
// Therefore, if you are trying to optimize
// this routine, and it fails (it most surely will),
// please increase this counter as a warning for the next person:
// total hours wasted on this project = 80

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(unused_doc_comments)]
//! Crate root: common telemetry types, error codes, and feature-gated glue.
//!
//! This module:
//! - Sets up `no_std` vs `std` behavior.
//! - Provides an embedded allocator / panic handler when targeting bare metal.
//! - Re-exports core telemetry configuration and metadata helpers
//!   (`MessageElementCount`, `MessageMeta`, `TelemetryError`, etc.).
//! - Wires in the C and Python FFI layers (`c_api`, `python_api`).
//!
//! Most user-facing APIs live in:
//! - [`config`]: schema + data type/endpoint configuration.
//! - [`router`]: the core Router abstraction.
//! - [`telemetry_packet`]: `TelemetryPacket` and friends.
//! - [`serialize`]: wire serialization helpers.

extern crate alloc;
extern crate core;
#[cfg(feature = "std")]
extern crate std;


use core::mem::size_of;
use core::ops::Mul;
use strum::EnumCount;

use crate::config::{
    get_message_data_type, get_message_info_types, get_message_meta, DataEndpoint, DataType,
    MAX_STATIC_HEX_LENGTH, MAX_STATIC_STRING_LENGTH,
};
use crate::macros::{ReprI32Enum, ReprU32Enum};

// ============================================================================
//  Test / Python FFI modules (std-only)
// ============================================================================

#[cfg(feature = "std")]
#[cfg(test)]
mod tests;

#[cfg(feature = "python")]
#[cfg(feature = "std")]
mod python_api;

// ============================================================================
//  Allocator & panic handlers (embedded no_std)
// ============================================================================
//
// For EMBEDDED builds (no_std + bare-metal target), provide Telemetry allocator
// + panic handler. Host builds rely on the system allocator / default panic.

#[cfg(all(not(feature = "std"), target_os = "none"))]
unsafe extern "C" {
    fn seds_error_msg(msg: *const u8, len: usize);
}

#[cfg(all(not(feature = "std"), target_os = "none"))]
mod embedded_alloc {
    use core::alloc::{GlobalAlloc, Layout};


    unsafe extern "C" {
        fn telemetryMalloc(size: usize) -> *mut core::ffi::c_void;
        fn telemetryFree(ptr: *mut core::ffi::c_void);
    }

    /// Global allocator that forwards to `telemetryMalloc` / `telemetryFree`
    /// provided by the host environment.
    struct TelemetryAlloc;

    unsafe impl GlobalAlloc for TelemetryAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let p = unsafe { telemetryMalloc(layout.size()) as *mut u8 };
            debug_assert!(p.is_null() || (p as usize) % layout.align() == 0);
            p
        }

        unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
            unsafe { telemetryFree(ptr as *mut _) }
        }
    }

    #[global_allocator]
    static A: TelemetryAlloc = TelemetryAlloc;


    // Panic handler for embedded
    use core::panic::PanicInfo;


    #[panic_handler]
    fn panic(_info: &PanicInfo) -> ! {
        // Halt forever after panic
        loop {}
    }

    // ensure cortex-m only compiles on embedded
    // use cortex_m as _;
}

// For HOST builds (std is ON), the system allocator is used automatically.
// No custom panic handler needed.

// ============================================================================
//  Portable core logic: modules
// ============================================================================

mod c_api;
pub mod config;
mod lock;
mod macros;
pub mod router;
pub mod serialize;
mod small_payload;
pub mod telemetry_packet;

// ============================================================================
//  Schema-derived global constants
// ============================================================================

/// Element count used for "string value" schema entries.
#[allow(dead_code)]
pub const STRING_VALUE_ELEMENT: usize = 1;

/// Maximum enum value for `DataEndpoint` (inclusive), derived from the schema.
pub const MAX_VALUE_DATA_ENDPOINT: u32 = (DataEndpoint::COUNT - 1) as u32;

/// Maximum enum value for `DataType` (inclusive), derived from the schema.
pub const MAX_VALUE_DATA_TYPE: u32 = (DataType::COUNT - 1) as u32;

/// Implement `ReprU32Enum` helpers for `DataType`.
impl_repr_u32_enum!(DataType, MAX_VALUE_DATA_TYPE);

/// Implement `ReprU32Enum` helpers for `DataEndpoint`.
impl_repr_u32_enum!(DataEndpoint, MAX_VALUE_DATA_ENDPOINT);

// ============================================================================
//  Message metadata (element counts, data types, sizes)
// ============================================================================

/// Describes how many elements are present for a given message type.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum MessageElementCount {
    /// Fixed number of elements.
    Static(usize),
    /// Variable number of elements (payload size can vary).
    Dynamic,
}

/// Static metadata for a message type: element count and valid endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct MessageMeta {
    /// How many elements are present (fixed vs dynamic).
    element_count: MessageElementCount,
    /// Allowed endpoints for this message type.
    endpoints: &'static [DataEndpoint],
}

/// Lookup `MessageMeta` for a given [`DataType`] using the generated config.
/// # Arguments
/// - `ty`: Logical data type to query.
/// # Returns
/// - `MessageMeta` struct with element count and allowed endpoints.
#[inline]
pub fn message_meta(ty: DataType) -> MessageMeta {
    get_message_meta(ty)
}

// ---- Convenience multiplication helpers ----

impl Mul<MessageElementCount> for usize {
    type Output = usize;

    fn mul(self, rhs: MessageElementCount) -> usize {
        self * rhs.into()
    }
}

impl Mul<usize> for MessageElementCount {
    type Output = usize;

    fn mul(self, rhs: usize) -> usize {
        self.into() * rhs
    }
}

impl MessageElementCount {
    /// Convert the element count to a `usize`.
    ///
    /// - `Static(n)` → `n`
    /// - `Dynamic`   → `0` (caller must handle dynamic sizing separately)
    fn into(self) -> usize {
        match self {
            MessageElementCount::Static(a) => a,
            _ => 0,
        }
    }
}

/// Return the total payload size (in bytes) required for a given `DataType`
/// under the *static* schema.
///
/// This is `element_size * element_count`. For dynamic types, the
/// configuration ensures we only call this where it makes sense.
/// # Arguments
/// - `ty`: Logical data type to query.
/// # Returns
/// - Total static payload size in bytes.
#[inline]
pub fn get_needed_message_size(ty: DataType) -> usize {
    data_type_size(get_data_type(ty)) * get_message_meta(ty).element_count
}

/// Return the logical "info" type (Info/Error) for a given `DataType`.
/// # Arguments
/// - `ty`: Logical data type to query.
/// # Returns
/// - `MessageType` enum value.
#[inline]
pub const fn get_info_type(ty: DataType) -> MessageType {
    get_message_info_types(ty)
}

/// Return the *element* data type (e.g., `Float32`, `Int16`, `String`) for a
/// given `DataType`.
/// # Arguments
/// - `ty`: Logical data type to query.
/// # Returns
/// - `MessageDataType` enum value.
#[inline]
pub const fn get_data_type(ty: DataType) -> MessageDataType {
    get_message_data_type(ty)
}

/// Primitive element type used by a message.
///
/// This is the underlying "slot" type, not the high-level `DataType`
/// (which is the logical schema type).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum MessageDataType {
    Float64,
    Float32,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    UInt128,
    Int8,
    Int16,
    Int32,
    Int64,
    Int128,
    Bool,
    String,
    Binary,
}

/// Size in bytes of a single element for the given [`MessageDataType`].
///
/// For `String` / `Hex`, this returns the fixed maximum static length
/// configured by the schema (`MAX_STATIC_STRING_LENGTH`, `MAX_STATIC_HEX_LENGTH`).
/// # Arguments
/// - `dt`: Logical data type to query.
/// # Returns
/// - Size in bytes of a single element of that type.
#[inline]
pub const fn data_type_size(dt: MessageDataType) -> usize {
    match dt {
        MessageDataType::Float64 => size_of::<f64>(),
        MessageDataType::Float32 => size_of::<f32>(),
        MessageDataType::UInt8 => size_of::<u8>(),
        MessageDataType::UInt16 => size_of::<u16>(),
        MessageDataType::UInt32 => size_of::<u32>(),
        MessageDataType::UInt64 => size_of::<u64>(),
        MessageDataType::UInt128 => size_of::<u128>(),
        MessageDataType::Int8 => size_of::<i8>(),
        MessageDataType::Int16 => size_of::<i16>(),
        MessageDataType::Int32 => size_of::<i32>(),
        MessageDataType::Int64 => size_of::<i64>(),
        MessageDataType::Int128 => size_of::<i128>(),
        MessageDataType::Bool => size_of::<bool>(),
        MessageDataType::String => MAX_STATIC_STRING_LENGTH,
        MessageDataType::Binary => MAX_STATIC_HEX_LENGTH,
    }
}

/// High-level classification of message kind.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum MessageType {
    /// Informational telemetry.
    Info,
    /// Error / fault telemetry.
    Error,
}

// ============================================================================
//  Error types and error codes
// ============================================================================

/// Rich error type used throughout the telemetry crate.
///
/// Most public APIs expose a `TelemetryResult<T>` alias for
/// `Result<T, TelemetryError>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryError {
    /// Logical type ID is not a valid [`DataType`].
    InvalidType,

    /// Payload size doesn't match the schema's expectations.
    SizeMismatch { expected: usize, got: usize },

    /// Legacy / generic size mismatch error (for C/Python parity).
    SizeMismatchError,

    /// No endpoints were supplied where they are required.
    EmptyEndpoints,

    /// Timestamp is invalid (e.g., zero when disallowed).
    TimestampInvalid,

    /// A packet is missing its payload bytes.
    MissingPayload,

    /// A handler (C/Python callback) returned an error.
    HandlerError(&'static str),

    /// Generic invalid argument from caller.
    BadArg,

    /// Serialization error.
    Serialize(&'static str),

    /// Deserialization error.
    Deserialize(&'static str),

    /// IO / transport error.
    Io(&'static str),

    /// UTF-8 decoding failed where string payloads are expected.
    InvalidUtf8,
}

impl TelemetryError {
    /// Map a rich [`TelemetryError`] to a stable numeric error code
    /// used by the FFI layers.
    pub const fn to_error_code(&self) -> TelemetryErrorCode {
        match self {
            TelemetryError::InvalidType => TelemetryErrorCode::InvalidType,
            TelemetryError::SizeMismatch { .. } => TelemetryErrorCode::SizeMismatch,
            TelemetryError::SizeMismatchError => TelemetryErrorCode::SizeMismatchError,
            TelemetryError::EmptyEndpoints => TelemetryErrorCode::EmptyEndpoints,
            TelemetryError::TimestampInvalid => TelemetryErrorCode::TimestampInvalid,
            TelemetryError::MissingPayload => TelemetryErrorCode::MissingPayload,
            TelemetryError::HandlerError(_) => TelemetryErrorCode::HandlerError,
            TelemetryError::BadArg => TelemetryErrorCode::BadArg,
            TelemetryError::Serialize(_) => TelemetryErrorCode::Serialize,
            TelemetryError::Deserialize(_) => TelemetryErrorCode::Deserialize,
            TelemetryError::Io(_) => TelemetryErrorCode::Io,
            TelemetryError::InvalidUtf8 => TelemetryErrorCode::InvalidUtf8,
        }
    }
}

/// Numeric error codes used on the C/Python FFI boundary.
///
/// Negative values are used to avoid collisions with success codes
/// and other positive return values (e.g. lengths).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum TelemetryErrorCode {
    InvalidType = -2,
    SizeMismatch = -3,
    SizeMismatchError = -4,
    EmptyEndpoints = -5,
    TimestampInvalid = -6,
    MissingPayload = -7,
    HandlerError = -8,
    BadArg = -9,
    Serialize = -10,
    Deserialize = -11,
    Io = -12,
    InvalidUtf8 = -13,
}

// Generate ReprI32Enum helpers for TelemetryErrorCode
impl_repr_i32_enum!(
    TelemetryErrorCode,
    TelemetryErrorCode::MAX,
    TelemetryErrorCode::MIN
);

impl TelemetryErrorCode {
    /// Maximum valid numeric error code value.
    pub const MAX: i32 = TelemetryErrorCode::InvalidType as i32;

    /// Minimum valid numeric error code value.
    pub const MIN: i32 = TelemetryErrorCode::Io as i32;

    /// Human-readable string for logging / debugging.
    /// # Returns
    /// - Static string representation of the error code.
    #[inline]
    pub fn as_str(&self) -> &'static str {
        match self {
            TelemetryErrorCode::InvalidType => "{Invalid Type}",
            TelemetryErrorCode::SizeMismatch => "{Size Mismatch}",
            TelemetryErrorCode::SizeMismatchError => "{Size Mismatch Error}",
            TelemetryErrorCode::EmptyEndpoints => "{Empty Endpoints}",
            TelemetryErrorCode::TimestampInvalid => "{Timestamp Invalid}",
            TelemetryErrorCode::MissingPayload => "{Missing Payload}",
            TelemetryErrorCode::HandlerError => "{Handler Error}",
            TelemetryErrorCode::BadArg => "{Bad Arg}",
            TelemetryErrorCode::Serialize => "{Serialize Error}",
            TelemetryErrorCode::Deserialize => "{Deserialize Error}",
            TelemetryErrorCode::Io => "{IO Error}",
            TelemetryErrorCode::InvalidUtf8 => "{Invalid UTF-8}",
        }
    }

    /// Try to convert a raw i32 error code into a [`TelemetryErrorCode`].
    ///
    /// Returns `None` if the code is out of range or not recognized.
    /// # Arguments
    /// - `x`: Raw i32 error code to convert.
    /// # Returns
    /// - `Some(TelemetryErrorCode)` if valid, `None` if invalid.
    #[inline]
    pub fn try_from_i32(x: i32) -> Option<Self> {
        try_enum_from_i32(x)
    }
}

/// Common result alias for telemetry operations.
pub type TelemetryResult<T> = Result<T, TelemetryError>;

// ============================================================================
//  Generic enum helpers (repr(u32) / repr(i32))
// ============================================================================

/// Try to convert a `u32` into a `#[repr(u32)]` enum `E`.
///
/// Returns `None` if the value is out of range (greater than `E::MAX`).
/// # Arguments
/// - `x`: Raw u32 value to convert.
/// # Returns
/// - `Some(E)` if valid, `None` if invalid.
#[inline]
pub fn try_enum_from_u32<E: ReprU32Enum>(x: u32) -> Option<E> {
    if x > E::MAX {
        return None;
    }

    // SAFETY: `E` is promised to be a fieldless #[repr(u32)] enum (thus 4 bytes, Copy).
    let e = unsafe { (&x as *const u32 as *const E).read() };
    Some(e)
}

/// Try to convert an `i32` into a `#[repr(i32)]` enum `E`.
///
/// Returns `None` if the value is outside the `[E::MIN, E::MAX]` range.
/// # Arguments
/// - `x`: Raw i32 value to convert.
/// # Returns
/// - `Some(E)` if valid, `None` if invalid.
#[inline]
pub fn try_enum_from_i32<E: ReprI32Enum>(x: i32) -> Option<E> {
    if x < E::MIN || x > E::MAX {
        return None;
    }

    // SAFETY: `E` is promised to be a fieldless #[repr(i32)] enum (thus 4 bytes, Copy).
    let e = unsafe { (&x as *const i32 as *const E).read() };
    Some(e)
}
