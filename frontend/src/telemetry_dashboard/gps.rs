// frontend/src/telemetry_dashboard/gps.rs
#![allow(dead_code)]

use dioxus::prelude::*;
use dioxus_signals::Signal;
use std::sync::atomic::{AtomicBool, Ordering};

static STARTED: AtomicBool = AtomicBool::new(false);

pub fn start_gps_updates(user_gps: Signal<Option<(f64, f64)>>) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    imp::start(user_gps);
}

//
// WASM / Web: use the hook-based SDK approach (NO globals, NO OnceLock).
//
#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use dioxus_signals::WritableExt;

    // OLD WORKING STYLE: hook-based geolocation (must be called from component context).
    //
    // IMPORTANT:
    // - This requires the crate/path you used before.
    // - If your dependency is now `dioxus_sdk_geolocation`, see the note below.
    use dioxus_sdk_geolocation::use_geolocation;

    pub fn start(mut user_gps: Signal<Option<(f64, f64)>>) {
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
    }
}

//
// Windows
//
#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    pub fn start(user_gps: Signal<Option<(f64, f64)>>) {
        crate::telemetry_dashboard::gps_windows::start(user_gps);
    }
}

//
// Apple platforms
//
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod imp {
    use super::*;
    pub fn start(user_gps: Signal<Option<(f64, f64)>>) {
        crate::telemetry_dashboard::gps_apple::start(user_gps);
    }
}

//
// Android
//
#[cfg(target_os = "android")]
mod imp {
    use super::*;
    pub fn start(user_gps: Signal<Option<(f64, f64)>>) {
        crate::telemetry_dashboard::gps_android::start(user_gps);
    }
}

//
// Everything else: no-op.
//
#[cfg(not(any(
    target_arch = "wasm32",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios",
    target_os = "android"
)))]
mod imp {
    use super::*;
    pub fn start(_user_gps: Signal<Option<(f64, f64)>>) {
        // no-op
    }
}
