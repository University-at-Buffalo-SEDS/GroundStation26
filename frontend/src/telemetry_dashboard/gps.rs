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

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use dioxus_signals::WritableExt;
    use wasm_bindgen::JsCast;
    use web_sys::{window, GeolocationPosition};

    pub fn start(mut user_gps: Signal<Option<(f64, f64)>>) {
        let geo = window().unwrap().navigator().geolocation().unwrap();

        let success = Closure::<dyn FnMut(GeolocationPosition)>::new(move |pos| {
            let coords = pos.coords();
            let lat = coords.latitude();
            let lon = coords.longitude();
            user_gps.set(Some((lat, lon)));
        });

        let error = Closure::<dyn FnMut(web_sys::GeolocationPositionError)>::new(move |_e| {
            // ignore / log
        });

        geo.watch_position_with_error_callback(
            success.as_ref().unchecked_ref(),
            error.as_ref().unchecked_ref(),
        )
        .ok();

        success.forget();
        error.forget();
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    pub fn start(user_gps: Signal<Option<(f64, f64)>>) {
        crate::telemetry_dashboard::gps_windows::start(user_gps);
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod imp {
    use super::*;
    pub fn start(user_gps: Signal<Option<(f64, f64)>>) {
        crate::telemetry_dashboard::gps_apple::start(user_gps);
    }
}

#[cfg(target_os = "android")]
mod imp {
    use super::*;
    pub fn start(user_gps: Signal<Option<(f64, f64)>>) {
        crate::telemetry_dashboard::gps_android::start(user_gps);
    }
}

// Optional: for linux/etc either stub or add another backend
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
