//! Lightweight mutex abstraction for `Router`
//!
//! This module provides a small wrapper type, [`RouterMutex`], which
//! abstracts over `std::sync::Mutex` (when the `std` feature is enabled)
//! and `spin::Mutex` (in `no_std` builds).
//!
//! The goal is to provide a uniform API where:
//! - In `std` builds, a poisoned mutex will **panic** with
//!   `"RouterMutex poisoned"` rather than forcing callers to handle
//!   `PoisonError` everywhere.
//! - In `no_std` builds, we use `spin::Mutex`, which never poisons.
//!
//! Typical usage:
//!
//! ```text
//! let m = RouterMutex::new(router);
//! {
//!     let mut r = m.lock();
//!     // use `r`
//! }
//! ```

// ============================================================================
//  std implementation
// ============================================================================

#[cfg(feature = "std")]
pub struct RouterMutex<T>(std::sync::Mutex<T>);

#[cfg(feature = "std")]
impl<T> RouterMutex<T> {
    /// Create a new `RouterMutex` wrapping the given value.
    #[inline]
    pub fn new(v: T) -> Self {
        Self(std::sync::Mutex::new(v))
    }

    /// Acquire the lock, panicking if the mutex has been poisoned.
    ///
    /// This is a convenience for router usage where poisoning is treated
    /// as a fatal error. Internally calls `.lock().expect("RouterMutex poisoned")`.
    #[inline]
    pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.0.lock().expect("RouterMutex poisoned")
    }
}

// ============================================================================
//  no_std (spin) implementation
// ============================================================================

#[cfg(not(feature = "std"))]
pub struct RouterMutex<T>(spin::Mutex<T>);

#[cfg(not(feature = "std"))]
impl<T> RouterMutex<T> {
    /// Create a new `RouterMutex` wrapping the given value.
    #[inline]
    pub fn new(v: T) -> Self {
        Self(spin::Mutex::new(v))
    }

    /// Acquire the lock.
    ///
    /// In `no_std` builds we use `spin::Mutex`, which never poisons, so this
    /// cannot fail.
    #[inline]
    pub fn lock(&self) -> spin::MutexGuard<'_, T> {
        self.0.lock()
    }
}
