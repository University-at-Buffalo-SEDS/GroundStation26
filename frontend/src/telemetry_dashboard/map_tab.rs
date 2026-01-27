// frontend/src/telemetry_dashboard/map_tab.rs

use crate::telemetry_dashboard::{
    abs_http, js_eval, js_is_ground_map_ready, js_read_window_string,
};
use dioxus::prelude::*;
use dioxus_signals::{ReadableExt, Signal, WritableExt};
// #[cfg(target_arch = "wasm32")]
// use gloo_timers::future::TimeoutFuture;

fn tiles_url() -> String {
    abs_http("/tiles/{z}/{x}/{y}.jpg")
}

#[component]
pub fn MapTab(
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
) -> Element {
    let mut is_fullscreen = use_signal(|| false);
    // Browser-derived location (from navigator.geolocation inside the webview/page)
    let browser_user_gps = use_signal(|| None::<(f64, f64)>);
    let has_centered_on_user = use_signal(|| false);

    // --- 1) Ensure map + geolocation watch is started (idempotent on JS side) ---
    use_effect(move || {
        spawn(async move {
            // Retry for ~5 seconds (or whatever you want)
            for _ in 0..100 {
                // This is safe to run repeatedly.
                // It will do nothing until ground_station.js is loaded AND the map div exists.
                js_eval(&format!(
                    r#"
                    (function() {{
                      try {{
                        if (window.__gs26_ground_station_loaded === true &&
                            typeof window.initGroundMap === "function") {{

                          window.initGroundMap({tiles:?}, 31.0, -99.0, 7.0);

                          if (typeof window.updateGroundMapMarkers === "function") {{
                            window.updateGroundMapMarkers(
                              window.__gs26_pending_r_lat,
                              window.__gs26_pending_r_lon,
                              window.__gs26_pending_u_lat,
                              window.__gs26_pending_u_lon
                            );
                          }}

                          return;
                        }}
                      }} catch (e) {{}}
                    }})();
                    "#,
                    tiles = tiles_url(),
                ));

                // Yield so scripts can load / event loop can run
                #[cfg(target_arch = "wasm32")]
                gloo_timers::future::TimeoutFuture::new(50).await;

                #[cfg(not(target_arch = "wasm32"))]
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }

            // Optional: only warn if we never saw the loader flag
            js_eval(
                r#"
          if (window.__gs26_ground_station_loaded !== true) {
            console.warn("[GS26] ground_station.js never loaded (after retries)");
          }
        "#,
            );

            // Now start geolocation watch (this is also idempotent on your JS side)
            js_eval(
                r#"
          (function() {
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
        });
    });

    // --- 2) Hydrate browser_user_gps once from JS cache/window vars (no Rust<->JS type bindings) ---
    {
        let mut browser_user_gps = browser_user_gps;
        let mut has_centered_on_user = has_centered_on_user;
        use_effect(move || {
            // First try getLastUserLatLng (your helper), else window vars.
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

    // --- 3) Poll window vars at 10 Hz and update browser_user_gps ---
    //
    // Why poll? It avoids any web_sys Position types, and works the same in wasm + native webview.
    {
        let mut browser_user_gps = browser_user_gps;
        let mut has_centered_on_user = has_centered_on_user;

        use_effect(move || {
            // install a single interval (JS-side guard)
            js_eval(
                r#"
                (function() {
                  if (window.__gs26_geo_poll_started) return;
                  window.__gs26_geo_poll_started = true;

                  window.__gs26_geo_poll_tick = function() {
                    // no-op; Rust will read window vars
                  };

                  setInterval(() => {
                    try { window.__gs26_geo_poll_tick(); } catch (e) {}
                  }, 100);
                })();
                "#,
            );

            // On every tick, we read from window vars from Rust side by re-running this effect
            // when any captured signals change — BUT we want periodic updates.
            //
            // Dioxus effects are not time-based. So we do *native* interval for native,
            // and `setInterval`-driven "poke" is not visible to Rust.
            //
            // Solution: use a Dioxus interval on the Rust side.
            //
            // Dioxus 0.7 provides `use_future` + timers via `gloo_timers` on wasm,
            // and tokio on native. The simplest cross-platform: spawn a task that loops.
            spawn(async move {
                loop {
                    // ~10 Hz
                    #[cfg(target_arch = "wasm32")]
                    use gloo_timers::future::TimeoutFuture;

                    #[cfg(target_arch = "wasm32")]
                    TimeoutFuture::new(500).await;
                    #[cfg(not(target_arch = "wasm32"))]
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if let Some((lat, lon)) = js_read_user_latlon_from_window() {
                        browser_user_gps.set(Some((lat, lon)));
                        if !*has_centered_on_user.read() {
                            js_center_on(lat, lon);
                            has_centered_on_user.set(true);
                        }
                    }
                }
            });
        });
    }

    // Effective user GPS: browser > parent
    let effective_user =
        move || -> Option<(f64, f64)> { browser_user_gps.read().or_else(|| *user_gps.read()) };

    // --- 4) Update markers whenever rocket/user changes ---
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
        if let Some((lat, lon)) = effective_user() {
            js_center_on(lat, lon);
        } else {
            js_eval(r#"console.warn("No user location yet; cannot center.");"#);
        }
    };

    {
        use_effect(move || {
            js_invalidate_map();
            js_setup_map_touch_guard();
            js_setup_map_size_guard();
        });
    }

    let on_toggle_fullscreen = move |_| {
        let next = !*is_fullscreen.read();
        is_fullscreen.set(next);
    };

    {
        let is_fullscreen = is_fullscreen;
        use_effect(move || {
            let is_fullscreen = *is_fullscreen.read();
            spawn(async move {
                #[cfg(target_arch = "wasm32")]
                gloo_timers::future::TimeoutFuture::new(60).await;

                #[cfg(not(target_arch = "wasm32"))]
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;

                js_reinit_map(is_fullscreen);
                if !is_fullscreen {
                    js_eval(
                        r#"
                        (function() {
                          try {
                            if (typeof window.__gs26_map_size_hook_update === "function") {
                              window.__gs26_map_size_hook_update();
                            }
                          } catch (e) {}
                        })();
                        "#,
                    );
                }
            });
        });
    }

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

                div {
                    style: "flex:1; min-height:0; width:100%;",
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

    // If map isn't ready yet, don't drop the data—just return.
    if !js_is_ground_map_ready() {
        return;
    }

    // If ready, apply immediately.
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

fn js_invalidate_map() {
    js_eval(
        r#"
        (function() {
          try {
            const m = window.__gs26_ground_map;
            if (m && typeof m.invalidateSize === "function") {
              setTimeout(() => { try { m.invalidateSize(); } catch (e) {} }, 50);
            }
          } catch (e) {}
        })();
        "#,
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

fn js_reinit_map(_fullscreen: bool) {
    js_eval(&format!(
        r#"
        (function() {{
          try {{
            if (window.__gs26_ground_station_loaded === true &&
                typeof window.initGroundMap === "function") {{
              window.initGroundMap({tiles:?}, 31.0, -99.0, 7.0);
            }}
          }} catch (e) {{}}
        }})();
        "#,
        tiles = tiles_url(),
    ));
}

fn js_setup_map_size_guard() {
    js_eval(
        r#"
        (function() {
          if (window.__gs26_map_size_hook) return;
          window.__gs26_map_size_hook = true;

          function updateSize() {
            try {
              const card = document.getElementById("map-card");
              if (!card) return;
              const rect = card.getBoundingClientRect();
              const h = window.innerHeight || 800;
              const max = Math.max(220, h - rect.top - 24);
              card.style.setProperty('--gs26-map-max', max + 'px');
            } catch (e) {}
          }

          window.__gs26_map_size_hook_update = updateSize;
          updateSize();
          window.addEventListener('resize', updateSize);
          window.addEventListener('orientationchange', updateSize);
        })();
        "#,
    );
}

fn js_cached_user_latlon() -> Option<(f64, f64)> {
    // Ask JS for getLastUserLatLng() and return JSON via a temporary window var.
    // We avoid JS<->Rust typed bindings by doing: window.__gs26_tmp = JSON.stringify(...)
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

    // Parse {lat,lon}
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

fn _js_apply_cached_markers_if_ready() {
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
            console.warn("apply cached markers failed:", e);
          }
        })();
        "#,
    );
}
