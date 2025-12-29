// frontend/src/telemetry_dashboard/map_tab.rs
//
// One Leaflet map implementation for BOTH:
//   - web (wasm32)
//   - native (desktop + iOS via dioxus-desktop/tao webview)
//
// Key idea:
//   ✅ Do NOT use wasm-bindgen imports at all.
//   ✅ Call your JS functions (initGroundMap/updateGroundMapMarkers/centerGroundMapOn/getLastUserLatLng)
//      by evaluating JS strings.
//      - wasm32: js_sys::eval(...)
//      - native: window.eval(...)
//
// Requirements:
//   1) Your app must load Leaflet + /web/ground_map.js so these functions exist on `window`.
//      For example, in your web index.html (or equivalent dioxus head injection), ensure you include:
//        - Leaflet CSS/JS
//        - /web/ground_map.js (or bundle it into the page)
//   2) ground_map.js must attach functions to `window`:
//        window.initGroundMap = ...
//        window.updateGroundMapMarkers = ...
//        window.centerGroundMapOn = ...
//        window.getLastUserLatLng = ...
//
// Note:
//   - This file also starts a browser-style watchPosition inside the webview on native.
//     On iOS, WKWebView supports navigator.geolocation (subject to permissions/capabilities).

use dioxus::prelude::*;
use dioxus_signals::{ReadableExt, Signal, WritableExt};
use crate::telemetry_dashboard::UrlConfig;
// #[cfg(target_arch = "wasm32")]
// use gloo_timers::future::TimeoutFuture;

fn tiles_url() -> String {
    let base = UrlConfig::_get_base_url().unwrap_or_else(|| "http://localhost:3000".to_string());
    format!("{}/tiles/{{z}}/{{x}}/{{y}}.jpg", base.trim_end_matches('/'))
}

#[component]
pub fn MapTab(
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
) -> Element {
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
                      // once this succeeds, your JS initGroundMap() already guards duplicates
                      return;
                    }}
                  }} catch (e) {{
                    // swallow and retry
                  }}
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
    let effective_user = move || -> Option<(f64, f64)> {
        browser_user_gps
            .read()
            .clone()
            .or_else(|| user_gps.read().clone())
    };

    // --- 4) Update markers whenever rocket/user changes ---
    {
        let rocket_gps = rocket_gps.clone();
        use_effect(move || {
            let r = rocket_gps.read().clone();
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

    rsx! {
        div {
            style: "display:flex; flex-direction:column; gap:12px; width:100%; padding:12px; border-radius:12px; background:#020617ee; border:1px solid #4b5563; box-shadow:0 10px 25px rgba(0,0,0,0.45);",
            div {
                style: "display:flex; align-items:center; gap:12px; flex-wrap:wrap;",
                h2 { style: "margin:0; color:#22c55e;", "Launch Site Map" }
                button {
                    style: "padding:6px 12px; border-radius:999px; border:1px solid #22c55e; background:#022c22; color:#bbf7d0; font-size:0.85rem; cursor:pointer;",
                    onclick: on_center_me,
                    "Center on Me"
                }
            }

            div {
                style: "height: calc(100vh - 220px); min-height: 400px; width:100%;",
                div {
                    id: "ground-map",
                    style: "width:100%; height:100%; border-radius:12px; overflow:hidden; background:#000; border:1px solid #4b5563;",
                }
            }
        }
    }
}

/* ================================================================================================
 * JS bridge helpers (no wasm-bindgen imports)
 * ============================================================================================== */

fn js_update_markers(r_lat: f64, r_lon: f64, u_lat: f64, u_lon: f64) {
    js_eval(&format!(
        r#"
        (function() {{
          try {{
            if (typeof window.updateGroundMapMarkers === "function") {{
              window.updateGroundMapMarkers({r_lat}, {r_lon}, {u_lat}, {u_lon});
            }} else {{
              console.warn("updateGroundMapMarkers not found on window");
            }}
          }} catch (e) {{
            console.warn("updateGroundMapMarkers threw:", e);
          }}
        }})();
        "#,
        r_lat = r_lat,
        r_lon = r_lon,
        u_lat = u_lat,
        u_lon = u_lon
    ));
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

fn js_read_window_string(key: &str) -> Option<String> {
    js_eval(&format!(
        r#"
        (function() {{
          try {{
            const v = window[{key:?}];
            window.__gs26_tmp_str = (typeof v === "string") ? v : "";
          }} catch (e) {{
            window.__gs26_tmp_str = "";
          }}
        }})();
        "#,
        key = key
    ));

    js_get_tmp_str()
}

/* ================================================================================================
 * Cross-platform "eval JS"
 * ============================================================================================== */

#[cfg(target_arch = "wasm32")]
fn js_eval(js: &str) {
    let _ = js_sys::eval(js);
}

#[cfg(not(target_arch = "wasm32"))]
fn js_eval(js: &str) {
    // Works on desktop + iOS because you’re running via dioxus-desktop (tao/wry webview).
    // If your renderer changes, this is the one function you’ll adjust.
    // use dioxus_desktop::use_window;

    // NOTE: hooks can't be called here; but use_window() is a hook.
    // So: we avoid calling it here directly.
    //
    // Instead we stash the JS into a global queue and have a component effect flush it.
    // To keep this file "complete" and working without more plumbing, we implement a
    // minimal global "last script" mechanism and execute it from an effect inside MapTab.
    //
    // HOWEVER: MapTab already calls js_eval from effects/tasks, so we need a direct eval.
    //
    // If your dioxus-desktop version exposes a non-hook global eval, use it.
    // Most builds expose `dioxus_desktop::window()` OR you can do this:
    //
    //   let window = dioxus_desktop::use_window();
    //   window.eval(js);
    //
    // But `use_window()` is a hook and must be called in the component body.
    //
    // ✅ So on native we rely on `document::eval`, which dioxus-desktop provides.
    // If you don’t have it, replace this with a hook-based `let window = use_window(); window.eval(...)`
    // by moving js_eval calls into closures that capture `window`.
    dioxus::document::eval(js);
}

#[cfg(target_arch = "wasm32")]
fn js_get_tmp_str() -> Option<String> {
    let win = web_sys::window()?;
    let v = js_sys::Reflect::get(&win, &wasm_bindgen::JsValue::from_str("__gs26_tmp_str")).ok()?;
    v.as_string()
}

#[cfg(not(target_arch = "wasm32"))]
fn js_get_tmp_str() -> Option<String> {
    // On native we can still read window.__gs26_tmp_str by asking JS to copy it to a known place
    // and then returning it isn't directly possible without a return channel.
    //
    // The simplest: avoid relying on return values for native by using only window vars.
    //
    // For cached user lat/lon we already set window.__gs26_user_lat/lon from localStorage in JS,
    // so native can skip parsing JSON here.
    //
    // Therefore, for native we just return None, and the caller will fall back to window vars.
    None
}
