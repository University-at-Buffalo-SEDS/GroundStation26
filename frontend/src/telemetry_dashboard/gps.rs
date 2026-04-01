// frontend/src/telemetry_dashboard/gps.rs
#![allow(dead_code)]

use dioxus::prelude::*;
use dioxus_signals::Signal;

/// Imperative start (only meaningful on platforms that need it).
/// Safe to call multiple times.
pub fn start_gps_updates(_user_gps: Signal<Option<(f64, f64)>>) {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
    imp::start(_user_gps);
}

/// Imperative stop (only meaningful on platforms that need it).
pub fn stop_gps_updates() {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
    imp::stop();
}

/// ONE common interface:
/// Mount this once in the dashboard and it will:
/// - connect `user_gps`
/// - start/stop native backends if needed
/// - use dioxus_sdk_geolocation on wasm
///
/// This component renders nothing visible.
#[component]
pub fn GpsDriver(
    user_gps: Signal<Option<(f64, f64)>>,
    #[props(optional)] js_ready: Option<bool>,
) -> Element {
    // wasm: hook-based SDK (no globals, no stop needed)
    #[cfg(target_arch = "wasm32")]
    {
        use dioxus_sdk_geolocation::use_geolocation;

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

    #[cfg(target_os = "windows")]
    {
        use dioxus_sdk_geolocation::{PowerMode, init_geolocator, use_geolocation};

        let _geo = init_geolocator(PowerMode::High);
        let geo = use_geolocation();

        use_effect(move || {
            if let Ok(pos) = geo() {
                let lat = pos.latitude;
                let lon = pos.longitude;
                if lat.is_finite() && lon.is_finite() {
                    user_gps.set(Some((lat, lon)));
                }
            }
        });

        return rsx!(div {});
    }

    // native imperative backends: start on mount, stop on unmount
    #[cfg(not(any(target_arch = "wasm32", target_os = "windows")))]
    {
        use_effect({
            let user_gps = user_gps;
            move || {
                start_gps_updates(user_gps);
            }
        });

        #[cfg(target_os = "android")]
        use_effect(move || {
            spawn(async move {
                loop {
                    if let Some((lat, lon)) =
                        crate::telemetry_dashboard::gps_android::latest_location()
                    {
                        user_gps.set(Some((lat, lon)));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            });
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

    pub fn start(_user_gps: Signal<Option<(f64, f64)>>) {
        crate::telemetry_dashboard::gps_android::start();
    }

    pub fn stop() {
        crate::telemetry_dashboard::gps_android::stop();
    }
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
    use dioxus_signals::Signal;

    pub fn start(_user_gps: Signal<Option<(f64, f64)>>) {}
    pub fn stop() {}
}
