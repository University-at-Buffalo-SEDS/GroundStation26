use leptos::prelude::*;
mod telemetry_dashboard;

#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn main() {
    mount_to_body(telemetry_dashboard::TelemetryDashboard);
}
