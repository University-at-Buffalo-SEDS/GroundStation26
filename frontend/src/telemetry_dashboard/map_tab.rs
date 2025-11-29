use crate::telemetry_dashboard::GpsPoint;
use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{console, js_sys, Position, PositionError};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = initGroundMap)]
    fn init_ground_map(tiles_url: &str, center_lat: f64, center_lon: f64, zoom: f64);

    #[wasm_bindgen(js_name = updateGroundMapMarkers)]
    fn update_ground_map_markers(rocket_lat: f64, rocket_lon: f64, user_lat: f64, user_lon: f64);

    // NEW: JS helper that recenters the Leaflet map on a given lat/lon
    #[wasm_bindgen(js_name = centerGroundMapOn)]
    fn center_ground_map_on(lat: f64, lon: f64);
}

#[component]
pub fn MapTab(
    /// Rocket GPS from parent (telemetry)
    rocket_gps: Signal<Option<GpsPoint>>,
    /// Optional initial/fallback user GPS from parent
    user_gps: Signal<Option<GpsPoint>>,
) -> impl IntoView {
    // Local signal for browser-derived user location
    let (browser_user_gps, set_browser_user_gps) = signal(None::<GpsPoint>);

    // Track whether we've already auto-centered the map on the user
    let (has_centered_on_user, set_has_centered_on_user) = signal(false);

    // Start geolocation watch when the component mounts
    Effect::new({
        let set_browser_user_gps = set_browser_user_gps.clone();
        let has_centered_on_user = has_centered_on_user.clone();
        let set_has_centered_on_user = set_has_centered_on_user.clone();

        move |_| {
            if let Some(window) = web_sys::window() {
                let navigator = window.navigator();

                if let Ok(geo) = navigator.geolocation() {
                    // success callback: Position -> update browser_user_gps and maybe center map
                    let success_cb = wasm_bindgen::closure::Closure::<dyn FnMut(Position)>::new(
                        move |pos: Position| {
                            let coords = pos.coords();
                            let lat = coords.latitude();
                            let lon = coords.longitude();

                            set_browser_user_gps.set(Some(GpsPoint { lat, lon }));

                            // Center map on user *once* (first fix only)
                            if !has_centered_on_user.get_untracked() {
                                center_ground_map_on(lat, lon);
                                set_has_centered_on_user.set(true);
                            }
                        },
                    );

                    // error callback: PositionError -> log to console
                    let error_cb = wasm_bindgen::closure::Closure::<dyn FnMut(PositionError)>::new(
                        move |err: PositionError| {
                            let msg = format!(
                                "geolocation error (code {}): {}",
                                err.code(),
                                err.message()
                            );
                            console::error_1(&msg.into());
                        },
                    );

                    // Watch position with both callbacks
                    let _ = geo.watch_position_with_error_callback(
                        success_cb.as_ref().unchecked_ref::<js_sys::Function>(),
                        Some(error_cb.as_ref().unchecked_ref::<js_sys::Function>()),
                    );

                    // Leak closures so they stay alive for page lifetime
                    success_cb.forget();
                    error_cb.forget();
                }
            }
        }
    });

    // Effective user GPS = browser location if available, otherwise parent-provided
    let effective_user_gps = Signal::derive({
        let browser_user_gps = browser_user_gps.clone();
        move || browser_user_gps.get().or_else(|| user_gps.get())
    });

    // Initialize the map once. JS side will guard against duplicate init.
    Effect::new(|_| {
        // Initial center is just a default; it will get recentered to user once GPS arrives.
        init_ground_map("/tiles/{z}/{x}/{y}.jpg", 31.0, -99.0, 7.0);
    });

    // Update markers whenever rocket or user GPS changes
    Effect::new(move |_| {
        let rocket = rocket_gps.get();
        let user = effective_user_gps.get();

        let (r_lat, r_lon) = rocket
            .map(|p| (p.lat, p.lon))
            .unwrap_or((f64::NAN, f64::NAN));
        let (u_lat, u_lon) = user.map(|p| (p.lat, p.lon)).unwrap_or((f64::NAN, f64::NAN));

        update_ground_map_markers(r_lat, r_lon, u_lat, u_lon);
    });

    view! {
        <div style="
            display:flex;
            flex-direction:column;
            gap:0.75rem;
            padding:1rem;
            border-radius:0.75rem;
            background:#020617ee;
            border:1px solid #4b5563;
            box-shadow:0 10px 25px rgba(0,0,0,0.45);
        ">
            <h2 style="margin:0; color:#22c55e;">"Launch Site Map"</h2>
            <p style="margin:0; color:#9ca3af; font-size:0.85rem;">
                "Interactive map showing the rocket (🚀) and your location (🧍)."
            </p>

            <div
                id="ground-map"
                style="
                    width:100%;
                    height:60vh;
                    border-radius:0.75rem;
                    overflow:hidden;
                    border:1px solid #4b5563;
                "
            ></div>

            <div style="display:flex; gap:1rem; font-size:0.8rem; color:#9ca3af;">
                <Show when=move || rocket_gps.get().is_some()>
                    {move || {
                        let pt = rocket_gps.get().unwrap();
                        view! {
                            <span>
                                {format!("Rocket: {:.6}°, {:.6}°", pt.lat, pt.lon)}
                            </span>
                        }
                    }}
                </Show>

                <Show when=move || effective_user_gps.get().is_some()>
                    {move || {
                        let pt = effective_user_gps.get().unwrap();
                        view! {
                            <span>
                                {format!("You: {:.6}°, {:.6}°", pt.lat, pt.lon)}
                            </span>
                        }
                    }}
                </Show>
            </div>
        </div>
    }
}
