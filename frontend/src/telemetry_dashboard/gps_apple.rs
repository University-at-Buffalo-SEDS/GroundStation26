// frontend/src/telemetry_dashboard/gps_apple.rs

use dioxus_signals::{Signal, WritableExt};

unsafe extern "C" {
    fn gs26_location_start(cb: extern "C" fn(f64, f64));
    // Optional if you have it:
    // fn gs26_location_stop();
}

// NOTE:
// We intentionally keep this as `static mut` and ONLY use it to *best-effort* write.
// We never unwrap in the callback, and we detach it in `stop()`.
static mut GPS_SIGNAL: Option<Signal<Option<(f64, f64)>>> = None;

extern "C" fn on_loc(lat: f64, lon: f64) {
    unsafe {
        let Some(mut sig) = GPS_SIGNAL else { return };

        // Never `sig.set(...)` here; it can panic if dropped.
        if let Ok(mut w) = sig.try_write() {
            *w = Some((lat, lon));
        } else {
            // Dropped/unmounted; ignore late callback
        }
    }
}

/// Start feeding CoreLocation updates into `user_gps`.
/// Safe to call multiple times; it replaces the stored signal.
pub fn start(user_gps: Signal<Option<(f64, f64)>>) {
    unsafe {
        GPS_SIGNAL = Some(user_gps);
        gs26_location_start(on_loc);
    }
}

/// Detach from UI so late callbacks can't panic.
pub fn stop() {
    unsafe {
        GPS_SIGNAL = None;
        // Optional:
        // gs26_location_stop();
    }
}
