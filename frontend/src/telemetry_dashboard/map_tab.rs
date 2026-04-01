// frontend/src/telemetry_dashboard/map_tab.rs

#[cfg(target_os = "android")]
use crate::telemetry_dashboard::gps_android;
#[cfg(target_os = "ios")]
use crate::telemetry_dashboard::gps_apple;
#[cfg(any(target_arch = "wasm32", target_os = "ios"))]
use crate::telemetry_dashboard::js_read_window_string;
use crate::telemetry_dashboard::{
    http_get_json, js_eval, js_is_ground_map_ready, layout::ThemeConfig, map_tiles_url,
};
use dioxus::prelude::*;
use dioxus_signals::{ReadableExt, Signal, WritableExt};
use serde::Deserialize;

const RESIZE_DEBOUNCE_MS: u64 = 250;
const FULLSCREEN_REINIT_DELAY_MS: u64 = 80;
const DEFAULT_MAX_NATIVE_ZOOM: u32 = 12;
const DEFAULT_MAP_CENTER_LAT: f64 = 31.0;
const DEFAULT_MAP_CENTER_LON: f64 = -99.0;
const DEFAULT_MAP_ZOOM: f64 = 7.0;
const DEFAULT_MAP_TITLE: &str = "Map";
const DEFAULT_TRACKED_ASSET_LABEL: &str = "Tracked Asset";

fn tiles_url() -> String {
    map_tiles_url()
}

fn format_distance_label(
    rocket: Option<(f64, f64)>,
    user: Option<(f64, f64)>,
    metric: bool,
) -> Option<String> {
    let (rocket_lat, rocket_lon) = rocket?;
    let (user_lat, user_lon) = user?;
    let meters = haversine_meters(rocket_lat, rocket_lon, user_lat, user_lon);
    Some(format_human_distance(meters, metric))
}

fn haversine_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_RADIUS_M: f64 = 6_371_000.0;
    let lat1 = lat1.to_radians();
    let lon1 = lon1.to_radians();
    let lat2 = lat2.to_radians();
    let lon2 = lon2.to_radians();
    let d_lat = lat2 - lat1;
    let d_lon = lon2 - lon1;
    let a = (d_lat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (d_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    EARTH_RADIUS_M * c
}

fn format_human_distance(meters: f64, metric: bool) -> String {
    if metric {
        if meters < 1_000.0 {
            format!("{:.0} m", meters.round())
        } else {
            let km = meters / 1_000.0;
            if km < 10.0 {
                format!("{km:.1} km")
            } else {
                format!("{km:.0} km")
            }
        }
    } else {
        let feet = meters * 3.280_839_895;
        if feet < 1_000.0 {
            format!("{:.0} ft", feet.round())
        } else {
            let miles = feet / 5_280.0;
            if miles < 10.0 {
                format!("{miles:.1} mi")
            } else {
                format!("{miles:.0} mi")
            }
        }
    }
}

#[component]
pub fn MapTab(
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
    #[props(default = false)] distance_units_metric: bool,
    #[props(default)] theme: Option<ThemeConfig>,
    #[props(default)] title: Option<String>,
) -> Element {
    let _ = *rocket_gps.read();
    let _ = *user_gps.read();
    let mut is_fullscreen = use_signal(|| false);
    #[cfg(target_os = "ios")]
    let mut show_enable_compass = use_signal(|| false);
    #[cfg(not(target_os = "ios"))]
    let show_enable_compass = use_signal(|| false);

    #[cfg(target_arch = "wasm32")]
    let mut browser_user_gps = use_signal(|| None::<(f64, f64)>);
    let has_centered_on_user = use_signal(|| false);
    let map_config = use_signal(MapConfig::default);
    let theme = theme.unwrap_or_default();
    let resolved_title = if title.as_deref().unwrap_or_default().trim().is_empty() {
        map_config.read().map_title.clone()
    } else {
        title.clone().unwrap_or_default()
    };

    {
        let mut map_config = map_config;
        use_future(move || async move {
            if let Ok(cfg) = http_get_json::<MapConfig>("/api/map_config").await {
                map_config.set(cfg.sanitized());
            }
        });
    }

    // --- 0) One-time JS setup (iOS/native safe: JS owns resize/orientation detection) ---
    {
        let tiles = tiles_url();
        let map_config = map_config;
        use_effect(move || {
            let config = map_config.read().clone();
            js_eval(r#"console.error("[GS26 map] setup effect entered");"#);
            #[cfg(target_os = "ios")]
            {
                *show_enable_compass.write() = js_is_compass_denied();
            }

            js_setup_map_touch_guard();
            js_setup_map_size_guard();
            js_setup_js_init_retry(&tiles, &config);
            #[cfg(target_arch = "wasm32")]
            _js_setup_js_geolocation_watch();

            // Debounced resize/orientation/visualViewport reinit path
            js_setup_js_resize_reinit(&tiles, &config, RESIZE_DEBOUNCE_MS);

            // Fullscreen enter/exit explicit reinit hook (independent of rotation)
            js_setup_js_fullscreen_reinit(&tiles, &config);

            js_eval(&format!(
                r#"
                    (function() {{
                      try {{
                        console.error("[GS26 map] forcing immediate init attempt");
                        if (window.__gs26_ground_station_loaded === true &&
                            typeof window.initGroundMap === "function") {{
                          window.initGroundMap(
                            {tiles:?},
                            {center_lat},
                            {center_lon},
                            {zoom},
                            {max_native_zoom},
                            {tracked_asset_label:?}
                          );
                        }} else {{
                          console.error("[GS26 map] init prerequisites missing", {{
                            loaded: window.__gs26_ground_station_loaded,
                            hasInit: typeof window.initGroundMap === "function"
                          }});
                        }}
                      }} catch (e) {{
                        console.error("[GS26 map] immediate init failed", String(e), e && e.stack ? e.stack : "");
                      }}
                    }})();
                    "#,
                center_lat = config.default_center_lat,
                center_lon = config.default_center_lon,
                zoom = config.default_zoom,
                max_native_zoom = config.max_native_zoom,
                tracked_asset_label = config.tracked_asset_label
            ));
        });
    }

    // --- 1) Fullscreen enter/exit ALWAYS forces a reinit + invalidate (independent of rotation) ---
    {
        let tiles = tiles_url();
        let map_config = map_config;
        let is_fullscreen_sig = is_fullscreen;
        use_effect(move || {
            let config = map_config.read().clone();
            let fs = *is_fullscreen_sig.read();
            js_eval(r#"console.error("[GS26 map] fullscreen/reinit effect entered");"#);
            js_force_map_reinit_now(&tiles, &config, fs, FULLSCREEN_REINIT_DELAY_MS);
        });
    }

    // --- 2) Hydrate browser_user_gps once from JS cache/window vars ---
    #[cfg(target_arch = "wasm32")]
    {
        let mut browser_user_gps = browser_user_gps;
        let mut has_centered_on_user = has_centered_on_user;
        use_effect(move || {
            if let Some((lat, lon)) = js_cached_user_latlon() {
                browser_user_gps.set(Some((lat, lon)));
                if !*has_centered_on_user.read() {
                    js_center_on(lat, lon);
                    has_centered_on_user.set(true);
                }
            } else if let Some((lat, lon)) = js_read_user_latlon_from_window() {
                browser_user_gps.set(Some((lat, lon)));
                if !*has_centered_on_user.read() {
                    js_center_on(lat, lon);
                    has_centered_on_user.set(true);
                }
            }
        });
    }

    // --- 2b) Keep browser geolocation in sync (watchPosition updates window vars asynchronously) ---
    #[cfg(target_arch = "wasm32")]
    {
        let mut browser_user_gps = browser_user_gps;
        let mut has_centered_on_user = has_centered_on_user;
        use_future(move || async move {
            loop {
                if let Some((lat, lon)) = js_read_user_latlon_from_window() {
                    if *browser_user_gps.read() != Some((lat, lon)) {
                        browser_user_gps.set(Some((lat, lon)));
                    }
                    if !*has_centered_on_user.read() {
                        js_center_on(lat, lon);
                        has_centered_on_user.set(true);
                    }
                }

                gloo_timers::future::TimeoutFuture::new(20).await;
            }
        });
    }

    // --- 2c) Native platforms trust the parent/native GPS signal directly ---
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut has_centered_on_user = has_centered_on_user;
        use_effect(move || {
            if let Some((lat, lon)) = *user_gps.read() {
                if !*has_centered_on_user.read() {
                    js_center_on(lat, lon);
                    has_centered_on_user.set(true);
                }
            }
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        use_future(move || async move {
            loop {
                #[cfg(target_os = "ios")]
                if let Some(deg) = gps_apple::latest_heading_deg() {
                    js_set_user_heading(deg);
                }

                #[cfg(target_os = "android")]
                if let Some(deg) = gps_android::latest_heading_deg() {
                    js_set_user_heading(deg);
                }

                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
    }

    // Effective user GPS:
    // native prefers the parent/native GPS signal, web prefers browser geolocation.
    #[cfg(not(target_arch = "wasm32"))]
    let effective_user = move || -> Option<(f64, f64)> { *user_gps.read() };
    #[cfg(target_arch = "wasm32")]
    let effective_user = move || -> Option<(f64, f64)> { *browser_user_gps.read() };
    let distance_text =
        format_distance_label(*rocket_gps.read(), effective_user(), distance_units_metric);
    #[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
    let native_location_warning = if (*user_gps.read()).is_none() {
        Some("User location unavailable. Native GPS has not provided coordinates yet.".to_string())
    } else {
        None
    };
    #[cfg(not(any(target_os = "ios", target_os = "macos", target_os = "android")))]
    let native_location_warning = None::<String>;
    #[cfg(target_os = "ios")]
    let native_compass_warning =
        if gps_apple::latest_heading_deg().is_none() && *show_enable_compass.read() {
            Some(
                "Compass unavailable. Orientation permission was denied or has not initialized."
                    .to_string(),
            )
        } else {
            None
        };
    #[cfg(not(target_os = "ios"))]
    let native_compass_warning = None::<String>;
    let diagnostics_warning = native_location_warning
        .clone()
        .or_else(|| native_compass_warning.clone());

    // --- 3) Update markers whenever rocket/user changes ---
    {
        use_effect(move || {
            let r = *rocket_gps.read();
            let u = effective_user();

            let (r_lat, r_lon) = r.unwrap_or((f64::NAN, f64::NAN));
            let (u_lat, u_lon) = u.unwrap_or((f64::NAN, f64::NAN));

            js_update_markers(r_lat, r_lon, u_lat, u_lon);
        });
    }

    let on_center_me = move |_| {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((lat, lon)) = *user_gps.read() {
            js_center_on(lat, lon);
            return;
        }

        #[cfg(target_arch = "wasm32")]
        js_request_user_geolocation_once();

        // Refresh from JS at click-time
        #[cfg(target_arch = "wasm32")]
        if let Some((lat, lon)) = js_cached_user_latlon().or_else(js_read_user_latlon_from_window) {
            browser_user_gps.set(Some((lat, lon)));
            js_center_on(lat, lon);
        } else if let Some((lat, lon)) = effective_user() {
            js_center_on(lat, lon);
        } else {
            js_eval(r#"console.warn("No user location yet; cannot center.");"#);
        }
    };

    let on_toggle_fullscreen = move |_| {
        let next = !*is_fullscreen.read();
        is_fullscreen.set(next);
        // use_effect will fire -> js_force_map_reinit_now(...)
    };

    let on_enable_compass = move |_| {
        #[cfg(target_os = "ios")]
        {
            js_eval(
                r#"
                (function() {
                  try {
                    window.__gs26_disable_compass = false;
                    if (typeof window.initCompassOnce === "function") {
                      window.initCompassOnce();
                    }
                  } catch (e) {
                    console.warn("Enable compass failed:", e);
                  }
                })();
                "#,
            );
            *show_enable_compass.write() = js_is_compass_denied();
        }

        #[cfg(not(target_os = "ios"))]
        {
            // no-op on non-iOS
        }
    };

    rsx! {
        if *is_fullscreen.read() {
            div { style: "position:fixed; inset:0; z-index:9999; padding:16px; background:{theme.app_background}; display:flex; flex-direction:column; gap:12px;",
                div { style: "display:flex; align-items:center; gap:12px; flex-wrap:wrap; justify-content:space-between;",
                    div { style: "display:flex; align-items:baseline; gap:10px; flex-wrap:wrap;",
                        h2 { style: "margin:0; color:#22c55e;", "{resolved_title}" }
                        if let Some(distance_text) = distance_text.clone() {
                            span { style: "color:#fecaca; font-size:0.95rem; font-weight:700;", "(Distance: {distance_text})" }
                        }
                    }
                    div { style: "display:flex; gap:8px; flex-wrap:wrap;",
                        button {
                            style: "padding:6px 12px; border-radius:999px; border:1px solid #22c55e; background:#022c22; color:#bbf7d0; font-size:0.85rem; cursor:pointer;",
                            onclick: on_center_me,
                            "Center on Me"
                        }
                        if cfg!(target_os = "ios") && *show_enable_compass.read() {
                            button {
                                style: "padding:6px 12px; border-radius:999px; border:1px solid #f59e0b; background:#3f2a06; color:#fde68a; font-size:0.85rem; cursor:pointer;",
                                onclick: on_enable_compass,
                                "Enable Compass"
                            }
                        }
                        button {
                            style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                            onclick: on_toggle_fullscreen,
                            "Exit Fullscreen"
                        }
                    }
                }
                if let Some(warning_text) = diagnostics_warning.clone() {
                    div { style: "padding:10px 12px; border-radius:12px; border:1px solid #f59e0b; background:#451a03; color:#fde68a; font-size:0.92rem; font-weight:700;",
                        "{warning_text}"
                    }
                }
                div { style: "flex:1; min-height:0; width:100%;",
                    div {
                        id: "ground-map",
                        style: "width:100%; height:100%; border-radius:12px; overflow:hidden; background:{theme.panel_background}; border:1px solid {theme.border_strong}; touch-action:manipulation; overscroll-behavior:contain;",
                        ontouchstart: move |e| {
                            let touches = e.touches();
                            if touches.len() > 1 {
                                e.prevent_default();
                                e.stop_propagation();
                            }
                        },
                    }
                }
            }
        } else {
            div {
                id: "map-card",
                style: "display:flex; flex-direction:column; gap:12px; width:100%; height:var(--gs26-map-max, 60vh); \
                        max-height:var(--gs26-map-max, 60vh); \
                        border-radius:12px; background:{theme.tab_shell_background}; border:1px solid {theme.border_strong}; \
                        box-shadow:0 10px 25px rgba(0,0,0,0.45);",
                div {
                    style: "display:flex; align-items:center; gap:12px; flex-wrap:wrap;",
                    h2 { style: "margin:0; color:#22c55e;", "{resolved_title}" }
                    if let Some(distance_text) = distance_text {
                        span { style: "color:#fecaca; font-size:0.95rem; font-weight:700;", "(Distance: {distance_text})" }
                    }
                    button {
                        style: "padding:6px 12px; border-radius:999px; border:1px solid #22c55e; background:#022c22; color:#bbf7d0; font-size:0.85rem; cursor:pointer;",
                        onclick: on_center_me,
                        "Center on Me"
                    }
                    if cfg!(target_os = "ios") && *show_enable_compass.read() {
                        button {
                            style: "padding:6px 12px; border-radius:999px; border:1px solid #f59e0b; background:#3f2a06; color:#fde68a; font-size:0.85rem; cursor:pointer;",
                            onclick: on_enable_compass,
                            "Enable Compass"
                        }
                    }
                    button {
                        style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                        onclick: on_toggle_fullscreen,
                        "Fullscreen"
                    }
                }
                if let Some(warning_text) = diagnostics_warning {
                    div { style: "margin:0 12px; padding:10px 12px; border-radius:12px; border:1px solid #f59e0b; background:#451a03; color:#fde68a; font-size:0.92rem; font-weight:700;",
                        "{warning_text}"
                    }
                }

                div { style: "flex:1; min-height:0; width:100%;",
                    div {
                        id: "ground-map",
                        style: "width:100%; height:100%; border-radius:12px; overflow:hidden; background:{theme.panel_background}; border:1px solid {theme.border_strong}; touch-action:manipulation; overscroll-behavior:contain;",
                        ontouchstart: move |e| {
                            let touches = e.touches();
                            if touches.len() > 1 {
                                e.prevent_default();
                                e.stop_propagation();
                            }
                        },
                    }
                }
            }
        }
    }
}

/* ================================================================================================
 * JS bridge helpers (no wasm-bindgen imports)
 * ============================================================================================== */

fn js_setup_js_fullscreen_reinit(tiles: &str, config: &MapConfig) {
    let tiles_js = serde_json::to_string(tiles).unwrap_or_else(|_| "\"\"".to_string());
    let max_native_zoom_js = config.max_native_zoom.to_string();
    let center_lat_js = config.default_center_lat.to_string();
    let center_lon_js = config.default_center_lon.to_string();
    let zoom_js = config.default_zoom.to_string();
    let tracked_asset_label_js = serde_json::to_string(&config.tracked_asset_label)
        .unwrap_or_else(|_| "\"Tracked Asset\"".to_string());

    let script = r#"
    (function() {
      if (window.__gs26_fullscreen_reinit_installed) return;
      window.__gs26_fullscreen_reinit_installed = true;

      window.__gs26_tiles_url = __TILES__;
      window.__gs26_max_native_zoom = __MAX_NATIVE_ZOOM__;
      window.__gs26_default_center_lat = __CENTER_LAT__;
      window.__gs26_default_center_lon = __CENTER_LON__;
      window.__gs26_default_zoom = __DEFAULT_ZOOM__;
      window.__gs26_tracked_asset_title = __TRACKED_ASSET_TITLE__;

      function doInvalidateMulti() {
        try {
          const m = window.__gs26_ground_map;
          if (m && typeof m.invalidateSize === "function") {
            requestAnimationFrame(() => { try { m.invalidateSize(); } catch(e) {} });
            setTimeout(() => { try { m.invalidateSize(); } catch(e) {} }, 80);
            setTimeout(() => { try { m.invalidateSize(); } catch(e) {} }, 200);
            setTimeout(() => { try { m.invalidateSize(); } catch(e) {} }, 400);
          }
        } catch(e) {}
      }

      function applyMarkers() {
        try {
          if (typeof window.updateGroundMapMarkers === "function") {
            window.updateGroundMapMarkers(
              window.__gs26_pending_r_lat,
              window.__gs26_pending_r_lon,
              window.__gs26_pending_u_lat,
              window.__gs26_pending_u_lon
            );
          }
        } catch(e) {}
      }

      window.__gs26_force_map_reinit = function(isFullscreen, delayMs) {
        try {
          const d = (typeof delayMs === "number") ? delayMs : 60;

          setTimeout(() => {
            try {
              if (window.__gs26_ground_station_loaded === true &&
                  typeof window.initGroundMap === "function") {
                window.initGroundMap(
                  window.__gs26_tiles_url,
                  window.__gs26_default_center_lat,
                  window.__gs26_default_center_lon,
                  window.__gs26_default_zoom,
                  window.__gs26_max_native_zoom,
                  window.__gs26_tracked_asset_title
                );
              }
            } catch(e) {}

            try {
              if (typeof window.__gs26_map_size_hook_update === "function") {
                window.__gs26_map_size_hook_update();
              }
            } catch(e) {}

            applyMarkers();
            doInvalidateMulti();
          }, d);
        } catch(e) {}
      };
    })();
    "#;

    js_eval(
        &script
            .replace("__TILES__", &tiles_js)
            .replace("__MAX_NATIVE_ZOOM__", &max_native_zoom_js)
            .replace("__CENTER_LAT__", &center_lat_js)
            .replace("__CENTER_LON__", &center_lon_js)
            .replace("__DEFAULT_ZOOM__", &zoom_js)
            .replace("__TRACKED_ASSET_TITLE__", &tracked_asset_label_js),
    );
}

fn js_force_map_reinit_now(tiles: &str, config: &MapConfig, is_fullscreen: bool, delay_ms: u64) {
    let tiles_js = serde_json::to_string(tiles).unwrap_or_else(|_| "\"\"".to_string());
    let max_native_zoom_js = config.max_native_zoom.to_string();
    let center_lat_js = config.default_center_lat.to_string();
    let center_lon_js = config.default_center_lon.to_string();
    let zoom_js = config.default_zoom.to_string();
    let tracked_asset_label_js = serde_json::to_string(&config.tracked_asset_label)
        .unwrap_or_else(|_| "\"Tracked Asset\"".to_string());
    let fs_js = if is_fullscreen { "true" } else { "false" };
    let delay_js = delay_ms.to_string();

    let script = r#"
    (function() {
      try {
        window.__gs26_tiles_url = __TILES__;
        window.__gs26_max_native_zoom = __MAX_NATIVE_ZOOM__;
        window.__gs26_default_center_lat = __CENTER_LAT__;
        window.__gs26_default_center_lon = __CENTER_LON__;
        window.__gs26_default_zoom = __DEFAULT_ZOOM__;
        window.__gs26_tracked_asset_title = __TRACKED_ASSET_TITLE__;
        if (typeof window.__gs26_force_map_reinit === "function") {
          window.__gs26_force_map_reinit(__FS__, __DELAY__);
        }
      } catch(e) {}
    })();
    "#;

    js_eval(
        &script
            .replace("__TILES__", &tiles_js)
            .replace("__MAX_NATIVE_ZOOM__", &max_native_zoom_js)
            .replace("__CENTER_LAT__", &center_lat_js)
            .replace("__CENTER_LON__", &center_lon_js)
            .replace("__DEFAULT_ZOOM__", &zoom_js)
            .replace("__TRACKED_ASSET_TITLE__", &tracked_asset_label_js)
            .replace("__FS__", fs_js)
            .replace("__DELAY__", &delay_js),
    );
}

fn js_setup_js_init_retry(tiles: &str, config: &MapConfig) {
    let tiles_js = serde_json::to_string(tiles).unwrap_or_else(|_| "\"\"".to_string());
    let max_native_zoom_js = config.max_native_zoom.to_string();
    let center_lat_js = config.default_center_lat.to_string();
    let center_lon_js = config.default_center_lon.to_string();
    let zoom_js = config.default_zoom.to_string();
    let tracked_asset_label_js = serde_json::to_string(&config.tracked_asset_label)
        .unwrap_or_else(|_| "\"Tracked Asset\"".to_string());

    let script = r#"
    (function() {
      if (window.__gs26_init_retry_installed) return;
      window.__gs26_init_retry_installed = true;

      window.__gs26_tiles_url = __TILES__;
      window.__gs26_max_native_zoom = __MAX_NATIVE_ZOOM__;
      window.__gs26_default_center_lat = __CENTER_LAT__;
      window.__gs26_default_center_lon = __CENTER_LON__;
      window.__gs26_default_zoom = __DEFAULT_ZOOM__;
      window.__gs26_tracked_asset_title = __TRACKED_ASSET_TITLE__;

      let tries = 0;
      const maxTries = 200; // ~10s at 50ms

      const t = setInterval(() => {
        tries++;
        try {
          const el = document.getElementById("ground-map");
          if (!el) return;

          if (window.__gs26_ground_station_loaded === true &&
              typeof window.initGroundMap === "function") {

            window.initGroundMap(
              window.__gs26_tiles_url,
              window.__gs26_default_center_lat,
              window.__gs26_default_center_lon,
              window.__gs26_default_zoom,
              window.__gs26_max_native_zoom,
              window.__gs26_tracked_asset_title
            );

            try {
              if (typeof window.__gs26_map_size_hook_update === "function") {
                window.__gs26_map_size_hook_update();
              }
            } catch (e) {}

            try {
              if (typeof window.updateGroundMapMarkers === "function") {
                window.updateGroundMapMarkers(
                  window.__gs26_pending_r_lat,
                  window.__gs26_pending_r_lon,
                  window.__gs26_pending_u_lat,
                  window.__gs26_pending_u_lon
                );
              }
            } catch (e) {}

            try {
              const m = window.__gs26_ground_map;
              if (m && typeof m.invalidateSize === "function") {
                requestAnimationFrame(() => { try { m.invalidateSize(); } catch(e) {} });
                setTimeout(() => { try { m.invalidateSize(); } catch(e) {} }, 80);
                setTimeout(() => { try { m.invalidateSize(); } catch(e) {} }, 200);
              }
            } catch (e) {}

            clearInterval(t);
          }
        } catch (e) {}

        if (tries >= maxTries) {
          clearInterval(t);
          try { console.warn("[GS26] initGroundMap retry timed out"); } catch (e) {}
        }
      }, 50);
    })();
    "#;

    js_eval(
        &script
            .replace("__TILES__", &tiles_js)
            .replace("__MAX_NATIVE_ZOOM__", &max_native_zoom_js)
            .replace("__CENTER_LAT__", &center_lat_js)
            .replace("__CENTER_LON__", &center_lon_js)
            .replace("__DEFAULT_ZOOM__", &zoom_js)
            .replace("__TRACKED_ASSET_TITLE__", &tracked_asset_label_js),
    );
}

#[cfg(not(target_os = "android"))]
fn _js_setup_js_geolocation_watch() {
    js_eval(
        r#"
        (function() {
          if (window.__gs26_disable_browser_geo === true) return;
          if (window.__gs26_geo_watch_started) return;
          if (typeof window.isSecureContext === "boolean" && window.isSecureContext !== true) {
            // WebViews on insecure origins cannot use navigator.geolocation.
            return;
          }
          if (!navigator || !navigator.geolocation) return;
          window.__gs26_geo_watch_started = true;

          try {
            navigator.geolocation.watchPosition(
              (pos) => {
                const c = pos.coords;
                window.__gs26_user_lat = c.latitude;
                window.__gs26_user_lon = c.longitude;
              },
              (err) => {
                try {
                  if (err && (err.code === 1 || err.code === 2 || err.code === 3)) return;
                } catch (e) {}
                console.warn("geolocation watch error:", err);
              },
              { enableHighAccuracy: true, maximumAge: 1000, timeout: 10000 }
            );
          } catch (e) {}
        })();
        "#,
    );
}

#[cfg(target_arch = "wasm32")]
fn js_request_user_geolocation_once() {
    js_eval(
        r#"
        (function() {
          if (window.__gs26_disable_browser_geo === true) return;
          if (typeof window.isSecureContext === "boolean" && window.isSecureContext !== true) return;
          if (!navigator || !navigator.geolocation) return;

          try {
            navigator.geolocation.getCurrentPosition(
              (pos) => {
                const c = pos.coords;
                window.__gs26_user_lat = c.latitude;
                window.__gs26_user_lon = c.longitude;
              },
              () => {},
              { enableHighAccuracy: true, maximumAge: 1000, timeout: 10000 }
            );
          } catch (e) {}
        })();
        "#,
    );
}

fn js_setup_js_resize_reinit(tiles: &str, config: &MapConfig, debounce_ms: u64) {
    let tiles_js = serde_json::to_string(tiles).unwrap_or_else(|_| "\"\"".to_string());
    let max_native_zoom_js = config.max_native_zoom.to_string();
    let center_lat_js = config.default_center_lat.to_string();
    let center_lon_js = config.default_center_lon.to_string();
    let zoom_js = config.default_zoom.to_string();
    let tracked_asset_label_js = serde_json::to_string(&config.tracked_asset_label)
        .unwrap_or_else(|_| "\"Tracked Asset\"".to_string());
    let debounce_js = debounce_ms.to_string();

    let script = r#"
    (function() {
      if (window.__gs26_resize_reinit_installed) return;
      window.__gs26_resize_reinit_installed = true;

      window.__gs26_tiles_url = __TILES__;
      window.__gs26_max_native_zoom = __MAX_NATIVE_ZOOM__;
      window.__gs26_default_center_lat = __CENTER_LAT__;
      window.__gs26_default_center_lon = __CENTER_LON__;
      window.__gs26_default_zoom = __DEFAULT_ZOOM__;
      window.__gs26_tracked_asset_title = __TRACKED_ASSET_TITLE__;
      const DEBOUNCE = __DEBOUNCE__;

      function doInvalidateMulti() {
        try {
          const m = window.__gs26_ground_map;
          if (m && typeof m.invalidateSize === "function") {
            requestAnimationFrame(() => { try { m.invalidateSize(); } catch(e) {} });
            setTimeout(() => { try { m.invalidateSize(); } catch(e) {} }, 80);
            setTimeout(() => { try { m.invalidateSize(); } catch(e) {} }, 200);
            setTimeout(() => { try { m.invalidateSize(); } catch(e) {} }, 400);
          }
        } catch(e) {}
      }

      function applyMarkers() {
        try {
          if (typeof window.updateGroundMapMarkers === "function") {
            window.updateGroundMapMarkers(
              window.__gs26_pending_r_lat,
              window.__gs26_pending_r_lon,
              window.__gs26_pending_u_lat,
              window.__gs26_pending_u_lon
            );
          }
        } catch(e) {}
      }

      function doReinit() {
        try {
          if (window.__gs26_ground_station_loaded === true &&
              typeof window.initGroundMap === "function") {
            window.initGroundMap(
              window.__gs26_tiles_url,
              window.__gs26_default_center_lat,
              window.__gs26_default_center_lon,
              window.__gs26_default_zoom,
              window.__gs26_max_native_zoom,
              window.__gs26_tracked_asset_title
            );
          }
        } catch (e) {}

        try {
          if (typeof window.__gs26_map_size_hook_update === "function") {
            window.__gs26_map_size_hook_update();
          }
        } catch (e) {}

        applyMarkers();
        doInvalidateMulti();
      }

      let t = null;
      function schedule() {
        try {
          if (t) clearTimeout(t);
          t = setTimeout(doReinit, DEBOUNCE);
        } catch (e) {}
      }

      window.addEventListener('resize', schedule, { passive: true });
      window.addEventListener('orientationchange', schedule, { passive: true });

      // iOS: visualViewport is often the only reliable signal during rotations/UI chrome changes
      try {
        if (window.visualViewport) {
          window.visualViewport.addEventListener('resize', schedule, { passive: true });
          window.visualViewport.addEventListener('scroll', schedule, { passive: true });
        }
      } catch (e) {}

      // iOS: matchMedia can fire even when resize doesn't
      try {
        const mq = window.matchMedia && window.matchMedia("(orientation: portrait)");
        if (mq && typeof mq.addEventListener === "function") mq.addEventListener("change", schedule);
        else if (mq && typeof mq.addListener === "function") mq.addListener(schedule);
      } catch (e) {}

      // initial settle
      setTimeout(schedule, 0);
      setTimeout(schedule, 250);
    })();
    "#;

    js_eval(
        &script
            .replace("__TILES__", &tiles_js)
            .replace("__MAX_NATIVE_ZOOM__", &max_native_zoom_js)
            .replace("__CENTER_LAT__", &center_lat_js)
            .replace("__CENTER_LON__", &center_lon_js)
            .replace("__DEFAULT_ZOOM__", &zoom_js)
            .replace("__TRACKED_ASSET_TITLE__", &tracked_asset_label_js)
            .replace("__DEBOUNCE__", &debounce_js),
    );
}

fn js_setup_map_touch_guard() {
    js_eval(
        r#"
        (function() {
          const el = document.getElementById("ground-map");
          if (!el || el.__gs26_touch_guard) return;
          el.__gs26_touch_guard = true;
          let last = 0;
          el.addEventListener('touchstart', function(e) {
            if (e.touches && e.touches.length > 1) {
              e.preventDefault();
              e.stopPropagation();
              return;
            }
          }, { passive: false });
          el.addEventListener('touchend', function(e) {
            const now = Date.now();
            if (now - last <= 300) {
              e.preventDefault();
              e.stopPropagation();
            }
            last = now;
          }, { passive: false });
        })();
        "#,
    );
}

fn js_setup_map_size_guard() {
    js_eval(
        r#"
        (function() {
          if (window.__gs26_map_size_hook) return;
          window.__gs26_map_size_hook = true;

          function getH() {
            try {
              const vv = window.visualViewport;
              if (vv && typeof vv.height === "number") return vv.height;
            } catch (e) {}
            return window.innerHeight || 800;
          }

          function updateSize() {
            try {
              const card = document.getElementById("map-card");
              if (!card) return;
              const rect = card.getBoundingClientRect();
              const h = getH();
              const max = Math.max(220, h - rect.top - 24);
              card.style.setProperty('--gs26-map-max', max + 'px');
            } catch (e) {}
          }

          window.__gs26_map_size_hook_update = updateSize;
          updateSize();

          window.addEventListener('resize', updateSize);
          window.addEventListener('orientationchange', updateSize);
          try {
            if (window.visualViewport) {
              window.visualViewport.addEventListener('resize', updateSize);
              window.visualViewport.addEventListener('scroll', updateSize);
            }
          } catch (e) {}
        })();
        "#,
    );
}

fn js_update_markers(r_lat: f64, r_lon: f64, u_lat: f64, u_lon: f64) {
    // Always cache the most recent values so the JS side can apply them later.
    js_eval(&format!(
        r#"
        (function() {{
          try {{
            window.__gs26_pending_r_lat = {r_lat};
            window.__gs26_pending_r_lon = {r_lon};
            window.__gs26_pending_u_lat = {u_lat};
            window.__gs26_pending_u_lon = {u_lon};
          }} catch (e) {{}}
        }})();
        "#,
        r_lat = r_lat,
        r_lon = r_lon,
        u_lat = u_lat,
        u_lon = u_lon,
    ));

    if !js_is_ground_map_ready() {
        return;
    }

    js_eval(
        r#"
        (function() {
          try {
            if (typeof window.updateGroundMapMarkers === "function") {
              window.updateGroundMapMarkers(
                window.__gs26_pending_r_lat,
                window.__gs26_pending_r_lon,
                window.__gs26_pending_u_lat,
                window.__gs26_pending_u_lon
              );
            }
          } catch (e) {
            console.warn("updateGroundMapMarkers threw:", e);
          }
        })();
        "#,
    );
}

fn js_center_on(lat: f64, lon: f64) {
    js_eval(&format!(
        r#"
        (function() {{
          try {{
            // Cache the requested center so first-time permission grants can
            // still center once the map bridge finishes initializing.
            window.__gs26_pending_center_lat = {lat};
            window.__gs26_pending_center_lon = {lon};

            const tryCenter = function() {{
              try {{
                if (typeof window.centerGroundMapOn !== "function") return false;
                const clat = window.__gs26_pending_center_lat;
                const clon = window.__gs26_pending_center_lon;
                if (!Number.isFinite(clat) || !Number.isFinite(clon)) return false;
                window.centerGroundMapOn(clat, clon);
                return true;
              }} catch (e) {{
                console.warn("centerGroundMapOn threw:", e);
                return false;
              }}
            }};

            if (tryCenter()) {{
              return;
            }}

            // Retry a few times for startup races (permission granted before map ready).
            const retryMs = [50, 150, 300, 600, 1000];
            for (const ms of retryMs) {{
              setTimeout(tryCenter, ms);
            }}
          }} catch (e) {{
            console.warn("queue center request failed:", e);
          }}
        }})();
        "#,
        lat = lat,
        lon = lon
    ));
}

#[cfg(any(target_os = "ios", target_os = "android"))]
fn js_set_user_heading(deg: f64) {
    js_eval(&format!(
        r#"
        (function() {{
          try {{
            if (typeof window.setGroundMapUserHeading === "function") {{
              window.setGroundMapUserHeading({deg});
            }}
          }} catch (e) {{
            console.warn("setGroundMapUserHeading threw:", e);
          }}
        }})();
        "#,
        deg = deg
    ));
}

#[cfg(target_arch = "wasm32")]
fn js_cached_user_latlon() -> Option<(f64, f64)> {
    js_eval(
        r#"
        (function() {
          try {
            if (typeof window.getLastUserLatLng === "function") {
              const v = window.getLastUserLatLng();
              window.__gs26_tmp_latlng = v ? JSON.stringify(v) : "";
            } else {
              window.__gs26_tmp_latlng = "";
            }
          } catch (e) {
            window.__gs26_tmp_latlng = "";
          }
        })();
        "#,
    );

    let s = js_read_window_string("__gs26_tmp_latlng")?;
    if s.is_empty() {
        return None;
    }

    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    let lat = v.get("lat")?.as_f64()?;
    let lon = v.get("lon")?.as_f64()?;
    Some((lat, lon))
}

#[cfg(target_arch = "wasm32")]
fn js_read_user_latlon_from_window() -> Option<(f64, f64)> {
    let lat = js_read_window_f64("__gs26_user_lat")?;
    let lon = js_read_window_f64("__gs26_user_lon")?;
    Some((lat, lon))
}

#[derive(Debug, Clone, Deserialize)]
struct MapConfig {
    max_native_zoom: u32,
    #[serde(default = "default_map_center_lat")]
    default_center_lat: f64,
    #[serde(default = "default_map_center_lon")]
    default_center_lon: f64,
    #[serde(default = "default_map_zoom")]
    default_zoom: f64,
    #[serde(default = "default_map_title")]
    map_title: String,
    #[serde(default = "default_tracked_asset_label")]
    tracked_asset_label: String,
}

impl Default for MapConfig {
    fn default() -> Self {
        Self {
            max_native_zoom: DEFAULT_MAX_NATIVE_ZOOM,
            default_center_lat: default_map_center_lat(),
            default_center_lon: default_map_center_lon(),
            default_zoom: default_map_zoom(),
            map_title: default_map_title(),
            tracked_asset_label: default_tracked_asset_label(),
        }
    }
}

impl MapConfig {
    fn sanitized(mut self) -> Self {
        self.max_native_zoom = self.max_native_zoom.max(1);
        if !self.default_center_lat.is_finite() {
            self.default_center_lat = default_map_center_lat();
        }
        if !self.default_center_lon.is_finite() {
            self.default_center_lon = default_map_center_lon();
        }
        if !self.default_zoom.is_finite() || self.default_zoom < 0.0 {
            self.default_zoom = default_map_zoom();
        }
        if self.map_title.trim().is_empty() {
            self.map_title = default_map_title();
        }
        if self.tracked_asset_label.trim().is_empty() {
            self.tracked_asset_label = default_tracked_asset_label();
        }
        self
    }
}

fn default_map_center_lat() -> f64 {
    DEFAULT_MAP_CENTER_LAT
}

fn default_map_center_lon() -> f64 {
    DEFAULT_MAP_CENTER_LON
}

fn default_map_zoom() -> f64 {
    DEFAULT_MAP_ZOOM
}

fn default_map_title() -> String {
    DEFAULT_MAP_TITLE.to_string()
}

fn default_tracked_asset_label() -> String {
    DEFAULT_TRACKED_ASSET_LABEL.to_string()
}

#[cfg(target_os = "ios")]
fn js_is_compass_denied() -> bool {
    js_eval(
        r#"
        (function() {
          try {
            const k = "gs26_compass_permission_v1";
            const v = (window.localStorage && window.localStorage.getItem(k)) || "";
            window.__gs26_compass_perm_state = v;
          } catch (e) {
            window.__gs26_compass_perm_state = "";
          }
        })();
        "#,
    );
    js_read_window_string("__gs26_compass_perm_state")
        .map(|v| v == "denied")
        .unwrap_or(false)
}

#[cfg(target_arch = "wasm32")]
fn js_read_window_f64(key: &str) -> Option<f64> {
    js_eval(&format!(
        r#"
        (function() {{
          try {{
            const v = window[{key:?}];
            window.__gs26_tmp_num = (typeof v === "number") ? String(v) : "";
          }} catch (e) {{
            window.__gs26_tmp_num = "";
          }}
        }})();
        "#,
        key = key
    ));
    let s = js_read_window_string("__gs26_tmp_num")?;
    if s.is_empty() {
        None
    } else {
        s.parse::<f64>().ok()
    }
}
