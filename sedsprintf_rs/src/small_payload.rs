//! Small, inline-optimized payload storage.
//!
//! This module provides [`SmallPayload`], a tiny enum that stores short
//! payloads inline on the stack and transparently spills to the heap when
//! they exceed [`MAX_STACK_PAYLOAD_SIZE`].
//!
//! It derefs to `&[u8]`, so you can pass it anywhere a byte slice is expected.

use crate::config::MAX_STACK_PAYLOAD_SIZE;
use alloc::sync::Arc;
use core::{fmt, ops::Deref};


/// Compact payload container with inline storage for small byte buffers.
///
/// - If `len <= MAX_STACK_PAYLOAD_SIZE`, the data is stored directly in the
///   `Inline` variant.
/// - Otherwise, the data is stored in an `Arc<[u8]>` in the `Heap` variant.
///
/// Use [`SmallPayload::new`] to construct instances; prefer [`SmallPayload::as_slice`]
/// or `Deref` to treat it as `&[u8]`.
#[derive(Clone)]
pub enum SmallPayload {
    /// Inline storage for small payloads.
    ///
    /// - `len` is the number of valid bytes stored in `buf`.
    /// - `buf` is always `MAX_STACK_PAYLOAD_SIZE` bytes long.
    Inline {
        len: u8,
        buf: [u8; MAX_STACK_PAYLOAD_SIZE],
    },

    /// Heap-backed payload for larger data.
    ///
    /// Stores the bytes in a reference-counted slice.
    Heap(Arc<[u8]>),
}

// ===========================================================================
// Core API
// ===========================================================================

impl SmallPayload {
    /// Construct a new [`SmallPayload`] from a byte slice.
    ///
    /// - If `data.len() <= MAX_STACK_PAYLOAD_SIZE`, the bytes are copied into
    ///   inline storage.
    /// - Otherwise, the bytes are stored in an `Arc<[u8]>`.
    #[inline]
    pub fn new(data: &[u8]) -> Self {
        if data.len() <= MAX_STACK_PAYLOAD_SIZE {
            let mut buf = [0u8; MAX_STACK_PAYLOAD_SIZE];
            buf[..data.len()].copy_from_slice(data);
            Self::Inline {
                len: data.len() as u8,
                buf,
            }
        } else {
            Self::Heap(Arc::from(data))
        }
    }

    /// Return the payload as a borrowed byte slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        match self {
            SmallPayload::Inline { len, buf } => &buf[..*len as usize],
            SmallPayload::Heap(arc) => arc,
        }
    }

    /// Return the payload as an owned `Arc<[u8]>`.
    ///
    /// - For inline payloads this allocates a new `Arc<[u8]>`.
    /// - For heap-backed payloads this simply clones the existing `Arc`.
    #[inline]
    #[allow(dead_code)]
    pub fn to_arc(&self) -> Arc<[u8]> {
        match self {
            SmallPayload::Inline { len, buf } => Arc::from(&buf[..*len as usize]),
            SmallPayload::Heap(arc) => arc.clone(),
        }
    }

    /// Length of the payload in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            SmallPayload::Inline { len, .. } => *len as usize,
            SmallPayload::Heap(a) => a.len(),
        }
    }

    /// Returns `true` if the payload is stored inline on the stack.
    #[inline]
    #[allow(dead_code)]
    pub fn is_inline(&self) -> bool {
        matches!(self, SmallPayload::Inline { .. })
    }
}

// ===========================================================================
// Trait impls
// ===========================================================================

impl fmt::Debug for SmallPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inline { len, .. } => write!(f, "SmallPayload::Inline({} bytes)", len),
            Self::Heap(a) => write!(f, "SmallPayload::Heap({} bytes)", a.len()),
        }
    }
}

impl Deref for SmallPayload {
    type Target = [u8];

    /// Deref to the underlying payload slice.
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}
