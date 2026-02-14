// frontend/src/telemetry_dashboard/map_tab.rs

use crate::telemetry_dashboard::{
    js_eval, js_is_ground_map_ready, js_read_window_string, map_tiles_url,
};
use dioxus::prelude::*;
use dioxus_signals::{ReadableExt, Signal, WritableExt};

const RESIZE_DEBOUNCE_MS: u64 = 250;
const FULLSCREEN_REINIT_DELAY_MS: u64 = 80;

fn tiles_url() -> String {
    map_tiles_url()
}

#[component]
pub fn MapTab(
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
) -> Element {
    let mut is_fullscreen = use_signal(|| false);

    // Browser-derived location (from navigator.geolocation inside the webview/page)
    let mut browser_user_gps = use_signal(|| None::<(f64, f64)>);
    let has_centered_on_user = use_signal(|| false);

    // --- 0) One-time JS setup (iOS/native safe: JS owns resize/orientation detection) ---
    {
        let tiles = tiles_url();
        use_effect(move || {
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            js_eval(
                r#"
                (function() {
                  window.__gs26_disable_browser_geo = true;
                  window.__gs26_disable_compass = true;
                })();
                "#,
            );

            js_setup_map_touch_guard();
            js_setup_map_size_guard();
            js_setup_js_init_retry(&tiles);
            js_setup_js_geolocation_watch();

            // Debounced resize/orientation/visualViewport reinit path
            js_setup_js_resize_reinit(&tiles, RESIZE_DEBOUNCE_MS);

            // Fullscreen enter/exit explicit reinit hook (independent of rotation)
            js_setup_js_fullscreen_reinit(&tiles);
        });
    }

    // --- 1) Fullscreen enter/exit ALWAYS forces a reinit + invalidate (independent of rotation) ---
    {
        let tiles = tiles_url();
        let is_fullscreen_sig = is_fullscreen;
        use_effect(move || {
            let fs = *is_fullscreen_sig.read();
            js_force_map_reinit_now(&tiles, fs, FULLSCREEN_REINIT_DELAY_MS);
        });
    }

    // --- 2) Hydrate browser_user_gps once from JS cache/window vars ---
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

                #[cfg(target_arch = "wasm32")]
                gloo_timers::future::TimeoutFuture::new(700).await;

                #[cfg(not(target_arch = "wasm32"))]
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            }
        });
    }

    // Effective user GPS: browser > parent
    let effective_user =
        move || -> Option<(f64, f64)> { browser_user_gps.read().or_else(|| *user_gps.read()) };

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
        // Refresh from JS at click-time
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

    rsx! {
        if *is_fullscreen.read() {
            div { style: "position:fixed; inset:0; z-index:9999; padding:16px; background:#020617; display:flex; flex-direction:column; gap:12px;",
                div { style: "display:flex; align-items:center; gap:12px; flex-wrap:wrap; justify-content:space-between;",
                    h2 { style: "margin:0; color:#22c55e;", "Launch Site Map" }
                    div { style: "display:flex; gap:8px; flex-wrap:wrap;",
                        button {
                            style: "padding:6px 12px; border-radius:999px; border:1px solid #22c55e; background:#022c22; color:#bbf7d0; font-size:0.85rem; cursor:pointer;",
                            onclick: on_center_me,
                            "Center on Me"
                        }
                        button {
                            style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                            onclick: on_toggle_fullscreen,
                            "Exit Fullscreen"
                        }
                    }
                }
                div { style: "flex:1; min-height:0; width:100%;",
                    div {
                        id: "ground-map",
                        style: "width:100%; height:100%; border-radius:12px; overflow:hidden; background:#000; border:1px solid #4b5563; touch-action:manipulation; overscroll-behavior:contain;",
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
                        border-radius:12px; background:#020617ee; border:1px solid #4b5563; \
                        box-shadow:0 10px 25px rgba(0,0,0,0.45);",
                div {
                    style: "display:flex; align-items:center; gap:12px; flex-wrap:wrap;",
                    h2 { style: "margin:0; color:#22c55e;", "Launch Site Map" }
                    button {
                        style: "padding:6px 12px; border-radius:999px; border:1px solid #22c55e; background:#022c22; color:#bbf7d0; font-size:0.85rem; cursor:pointer;",
                        onclick: on_center_me,
                        "Center on Me"
                    }
                    button {
                        style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                        onclick: on_toggle_fullscreen,
                        "Fullscreen"
                    }
                }

                div { style: "flex:1; min-height:0; width:100%;",
                    div {
                        id: "ground-map",
                        style: "width:100%; height:100%; border-radius:12px; overflow:hidden; background:#000; border:1px solid #4b5563; touch-action:manipulation; overscroll-behavior:contain;",
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

fn js_setup_js_fullscreen_reinit(tiles: &str) {
    let tiles_js = serde_json::to_string(tiles).unwrap_or_else(|_| "\"\"".to_string());

    let script = r#"
    (function() {
      if (window.__gs26_fullscreen_reinit_installed) return;
      window.__gs26_fullscreen_reinit_installed = true;

      window.__gs26_tiles_url = __TILES__;

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
                window.initGroundMap(window.__gs26_tiles_url, 31.0, -99.0, 7.0);
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

    js_eval(&script.replace("__TILES__", &tiles_js));
}

fn js_force_map_reinit_now(tiles: &str, is_fullscreen: bool, delay_ms: u64) {
    let tiles_js = serde_json::to_string(tiles).unwrap_or_else(|_| "\"\"".to_string());
    let fs_js = if is_fullscreen { "true" } else { "false" };
    let delay_js = delay_ms.to_string();

    let script = r#"
    (function() {
      try {
        window.__gs26_tiles_url = __TILES__;
        if (typeof window.__gs26_force_map_reinit === "function") {
          window.__gs26_force_map_reinit(__FS__, __DELAY__);
        }
      } catch(e) {}
    })();
    "#;

    js_eval(
        &script
            .replace("__TILES__", &tiles_js)
            .replace("__FS__", fs_js)
            .replace("__DELAY__", &delay_js),
    );
}

fn js_setup_js_init_retry(tiles: &str) {
    let tiles_js = serde_json::to_string(tiles).unwrap_or_else(|_| "\"\"".to_string());

    let script = r#"
    (function() {
      if (window.__gs26_init_retry_installed) return;
      window.__gs26_init_retry_installed = true;

      window.__gs26_tiles_url = __TILES__;

      let tries = 0;
      const maxTries = 200; // ~10s at 50ms

      const t = setInterval(() => {
        tries++;
        try {
          const el = document.getElementById("ground-map");
          if (!el) return;

          if (window.__gs26_ground_station_loaded === true &&
              typeof window.initGroundMap === "function") {

            window.initGroundMap(window.__gs26_tiles_url, 31.0, -99.0, 7.0);

            try {
              if (typeof window.__gs26_map_size_hook_update === "function") {
                window.__gs26_map_size_hook_update();
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

    js_eval(&script.replace("__TILES__", &tiles_js));
}

fn js_setup_js_geolocation_watch() {
    js_eval(
        r#"
        (function() {
          if (window.__gs26_disable_browser_geo === true) return;
          if (window.__gs26_geo_watch_started) return;
          window.__gs26_geo_watch_started = true;
          if (!navigator || !navigator.geolocation) return;

          try {
            navigator.geolocation.watchPosition(
              (pos) => {
                const c = pos.coords;
                window.__gs26_user_lat = c.latitude;
                window.__gs26_user_lon = c.longitude;
              },
              (err) => console.warn("geolocation watch error:", err),
              { enableHighAccuracy: true, maximumAge: 1000, timeout: 10000 }
            );
          } catch (e) {}
        })();
        "#,
    );
}

fn js_setup_js_resize_reinit(tiles: &str, debounce_ms: u64) {
    let tiles_js = serde_json::to_string(tiles).unwrap_or_else(|_| "\"\"".to_string());
    let debounce_js = debounce_ms.to_string();

    let script = r#"
    (function() {
      if (window.__gs26_resize_reinit_installed) return;
      window.__gs26_resize_reinit_installed = true;

      window.__gs26_tiles_url = __TILES__;
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
            window.initGroundMap(window.__gs26_tiles_url, 31.0, -99.0, 7.0);
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
            if (typeof window.centerGroundMapOn === "function") {{
              window.centerGroundMapOn({lat}, {lon});
            }} else {{
              console.warn("centerGroundMapOn not found on window");
            }}
          }} catch (e) {{
            console.warn("centerGroundMapOn threw:", e);
          }}
        }})();
        "#,
        lat = lat,
        lon = lon
    ));
}

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

fn js_read_user_latlon_from_window() -> Option<(f64, f64)> {
    let lat = js_read_window_f64("__gs26_user_lat")?;
    let lon = js_read_window_f64("__gs26_user_lon")?;
    Some((lat, lon))
}

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
