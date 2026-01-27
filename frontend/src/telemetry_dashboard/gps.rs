// frontend/src/telemetry_dashboard/gps.rs
#![allow(dead_code)]

use dioxus::prelude::*;
#[allow(unused_imports)]
use dioxus_signals::{Signal, WritableExt};
use std::sync::atomic::{AtomicBool, Ordering};

static STARTED: AtomicBool = AtomicBool::new(false);

/// Imperative start (only meaningful on platforms that need it).
/// Safe to call multiple times.
pub fn start_gps_updates(user_gps: Signal<Option<(f64, f64)>>) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    imp::start(user_gps);
}

/// Imperative stop (only meaningful on platforms that need it).
pub fn stop_gps_updates() {
    if STARTED.swap(false, Ordering::SeqCst) {
        imp::stop();
    }
}

/// ONE common interface:
/// Mount this once in your dashboard and it will:
/// - connect `user_gps`
/// - start/stop native backends if needed
/// - use dioxus_sdk_geolocation on wasm/windows (hook-based)
///
/// This component renders nothing visible.
#[component]
pub fn GpsDriver(
    user_gps: Signal<Option<(f64, f64)>>,
    /// Optional: gate GPS init until your JS is ready (only used on wasm).
    #[props(optional)]
    js_ready: Option<bool>,
) -> Element {
    // wasm/windows: hook-based SDK (no globals, no stop needed)
    #[cfg(any(target_arch = "wasm32", target_os = "windows"))]
    {
        use dioxus_sdk_geolocation::use_geolocation;

        #[cfg(target_arch = "wasm32")]
        if let Some(false) = js_ready {
            return rsx!(div {});
        }

        let geo = use_geolocation();

        use_effect(move || {
            if let Ok(pos) = geo() {
                let lat = pos.latitude;
                let lon = pos.longitude;
                if lat.is_finite() && lon.is_finite() {
                    user_gps.set(Some((lat, lon)));
                }
            } else {
                // not supported / permission denied / unavailable / etc.
                // ignore (or log if you want)
            }
        });

        return rsx!(div {});
    }

    // native imperative backends: start on mount, stop on unmount
    #[cfg(not(any(target_arch = "wasm32", target_os = "windows")))]
    {
        // Start (idempotent due to STARTED)
        use_effect({
            let user_gps = user_gps;
            move || {
                start_gps_updates(user_gps);
            }
        });

        // Stop when this component is dropped (unmounted)
        use_drop(|| {
            stop_gps_updates();
        });

        rsx!(div {})
    }
}

//
// Platform-specific imperative backends
//

// Apple platforms
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod imp {
    use super::*;

    pub fn start(user_gps: Signal<Option<(f64, f64)>>) {
        crate::telemetry_dashboard::gps_apple::start(user_gps);
    }

    pub fn stop() {
        crate::telemetry_dashboard::gps_apple::stop();
    }
}

// Android
#[cfg(target_os = "android")]
mod imp {
    use super::*;

    pub fn start(user_gps: Signal<Option<(f64, f64)>>) {
        crate::telemetry_dashboard::gps_android::start(user_gps);
    }

    pub fn stop() {
        #[allow(unused)]
        {
            // If you have it:
            // crate::telemetry_dashboard::gps_android::stop();
        }
    }
}

// wasm/windows imperative: no-op (GpsDriver does hook-based work)
#[cfg(any(target_arch = "wasm32", target_os = "windows"))]
mod imp {
    use super::*;
    pub fn start(_user_gps: Signal<Option<(f64, f64)>>) {}
    pub fn stop() {}
}

// Everything else native: no-op
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "android",
    target_arch = "wasm32",
    target_os = "windows"
)))]
mod imp {
    use super::*;
    pub fn start(_user_gps: Signal<Option<(f64, f64)>>) {}
    pub fn stop() {}
}
