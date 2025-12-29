mod app;
mod telemetry_dashboard;

use dioxus::prelude::*;
#[cfg(target_arch = "wasm32")]
fn init_panic_hook() {
    console_error_panic_hook::set_once();
}

#[cfg(not(target_arch = "wasm32"))]
fn init_panic_hook() {}

#[cfg(target_arch = "wasm32")]
fn main() {
    init_panic_hook();

    // Web launch (wasm)
    // You can add web config here if you want; default is fine.
    launch(app::App);
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    init_panic_hook();
    launch(app::App);
}
