use dioxus_signals::{Signal, WritableExt};

unsafe extern "C" {
    fn gs26_location_start(cb: extern "C" fn(f64, f64));
}

static mut GPS_SIGNAL: Option<Signal<Option<(f64, f64)>>> = None;

extern "C" fn on_loc(lat: f64, lon: f64) {
    unsafe {
        if let Some(mut sig) = GPS_SIGNAL {
            sig.set(Some((lat, lon)));
        }
    }
}

pub fn start(user_gps: Signal<Option<(f64, f64)>>) {
    unsafe {
        GPS_SIGNAL = Some(user_gps);
        gs26_location_start(on_loc);
    }
}
