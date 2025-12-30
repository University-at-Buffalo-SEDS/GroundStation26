// frontend/src/telemetry_dashboard/gps_android.rs
#![cfg(target_os = "android")]

use dioxus_signals::{Signal, WritableExt};
use std::sync::OnceLock;

static GPS_SIGNAL: OnceLock<Signal<Option<(f64, f64)>>> = OnceLock::new();

pub fn start(user_gps: Signal<Option<(f64, f64)>>) {
    // store signal so JNI callback can update it
    let _ = GPS_SIGNAL.set(user_gps);

    // Call into Java/Kotlin to start location updates
    // This requires a Java class:
    //   com.ubbeds.groundstation26.LocationShim.start()
    unsafe { gs26_android_location_start() };
}

extern "C" {
    /// Implemented on the Java/Kotlin side via JNI to start GPS updates.
    fn gs26_android_location_start();
}

/// Called from Java/Kotlin when you receive a location update.
#[no_mangle]
pub extern "C" fn gs26_android_location_on_update(lat: f64, lon: f64) {
    if let Some(sig) = GPS_SIGNAL.get() {
        sig.set(Some((lat, lon)));
    }
}
