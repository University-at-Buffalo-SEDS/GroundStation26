#![allow(dead_code)]

use dioxus::prelude::*;
use dioxus_signals::{Signal, WritableExt};

const GEOLOCATION_WATCH_JS: &str = r#"
(function() {
  try {
    if (typeof window.isSecureContext === "boolean" && window.isSecureContext !== true) {
      window.__gs26_geo_error = "navigator.geolocation requires a secure context";
      return;
    }
    if (!navigator || !navigator.geolocation) {
      window.__gs26_geo_error = "navigator.geolocation is unavailable";
      return;
    }

    if (window.__gs26_geo_watch_id != null) {
      navigator.geolocation.clearWatch(window.__gs26_geo_watch_id);
      window.__gs26_geo_watch_id = null;
    }

    window.__gs26_geo_error = "";
    window.__gs26_geo_watch_id = navigator.geolocation.watchPosition(
      function(pos) {
        window.__gs26_user_lat = pos.coords.latitude;
        window.__gs26_user_lon = pos.coords.longitude;
        window.__gs26_geo_error = "";
      },
      function(err) {
        window.__gs26_geo_error = (err && err.message) ? err.message : String(err);
      },
      {
        enableHighAccuracy: true,
        maximumAge: 1000,
        timeout: 15000
      }
    );
  } catch (e) {
    window.__gs26_geo_error = String(e);
  }
})();
"#;

#[cfg(target_arch = "wasm32")]
async fn sleep_poll_interval() {
    gloo_timers::future::sleep(std::time::Duration::from_millis(250)).await;
}

#[cfg(not(target_arch = "wasm32"))]
async fn sleep_poll_interval() {
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
}

pub async fn run(mut user_gps: Signal<Option<(f64, f64)>>) {
    document::eval(GEOLOCATION_WATCH_JS);

    loop {
        if let Some((lat, lon)) = read_user_latlon_from_window().await {
            user_gps.set(Some((lat, lon)));
        } else if let Some(message) = read_window_string("__gs26_geo_error").await
            && !message.is_empty()
        {
            eprintln!("GPS watch error: {message}");
        }

        sleep_poll_interval().await;
    }
}

pub fn stop() {
    document::eval(
        r#"
        (function() {
          try {
            if (navigator && navigator.geolocation && window.__gs26_geo_watch_id != null) {
              navigator.geolocation.clearWatch(window.__gs26_geo_watch_id);
            }
          } catch (e) {
            console.warn("Failed clearing geolocation watch:", e);
          } finally {
            window.__gs26_geo_watch_id = null;
          }
        })();
        "#,
    );
}

async fn read_user_latlon_from_window() -> Option<(f64, f64)> {
    let lat = read_window_f64("__gs26_user_lat").await?;
    let lon = read_window_f64("__gs26_user_lon").await?;
    Some((lat, lon))
}

async fn read_window_f64(key: &str) -> Option<f64> {
    document::eval(&format!(
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
    let s = read_window_string("__gs26_tmp_num").await?;
    if s.is_empty() {
        None
    } else {
        s.parse::<f64>().ok()
    }
}

async fn read_window_string(key: &str) -> Option<String> {
    let eval = document::eval(&format!(
        r#"
        (function() {{
          try {{
            return String(window[{key:?}] ?? "");
          }} catch (e) {{
            return "";
          }}
        }})()
        "#,
        key = key
    ));
    eval.join::<String>().await.ok()
}
