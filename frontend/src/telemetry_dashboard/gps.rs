// frontend/src/telemetry_dashboard/gps.rs
//
// Dioxus GPS/location module using dioxus-sdk geolocation.

#![allow(dead_code)]

use dioxus::prelude::*;
use dioxus_signals::Signal;
use std::sync::atomic::{AtomicBool, Ordering};

static STARTED: AtomicBool = AtomicBool::new(false);

/// Start continuously updating `user_gps` with the best available platform
/// location provider (via dioxus-sdk).
///
/// Call this from a component (recommended: App root) so it can spawn tasks
/// and react to lifecycle properly.
///
/// Example:
///   let user_gps = use_signal(|| None::<(f64, f64)>);
///   gps::start_gps_updates(cx, user_gps);
pub fn start_gps_updates(user_gps: Signal<Option<(f64, f64)>>) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    // Run the SDK-based watcher.
    sdk::start(user_gps);
}

mod sdk {
    use super::*;
    use dioxus_signals::WritableExt;

    // dioxus-sdk geolocation hook
    use dioxus_sdk::geolocation::{use_geolocation};

    /// Must be called from a component scope.
    pub fn start(user_gps: Signal<Option<(f64, f64)>>) {
        // dioxus-sdk hook: manages permissions + watching internally
        let geo = use_geolocation();

        // We need a mutable handle to call `.set()`
        let mut user_gps = user_gps;

        // React whenever the status changes and write into the signal.
        use_effect(move || {
            match geo() {
                Ok(pos) => {
                    let lat = pos.latitude;
                    let lon = pos.longitude;
                    if lat.is_finite() && lon.is_finite() {
                        user_gps.set(Some((lat, lon)));
                    }
                }

                Err(_) => {
                    // leave last value
                }
            }
        });
    }
}
