#![allow(dead_code)]

use dioxus::prelude::*;
use dioxus_signals::{Signal, WritableExt};
use serde::Deserialize;

const GEOLOCATION_WATCH_JS: &str = r#"
(function() {
  try {
    if (!navigator || !navigator.geolocation) {
      dioxus.send({ kind: "error", message: "navigator.geolocation is unavailable" });
      return;
    }

    if (window.__gs26_geo_watch_id != null) {
      navigator.geolocation.clearWatch(window.__gs26_geo_watch_id);
      window.__gs26_geo_watch_id = null;
    }

    window.__gs26_geo_watch_id = navigator.geolocation.watchPosition(
      function(pos) {
        dioxus.send({
          kind: "position",
          lat: pos.coords.latitude,
          lon: pos.coords.longitude
        });
      },
      function(err) {
        dioxus.send({
          kind: "error",
          message: (err && err.message) ? err.message : String(err)
        });
      },
      {
        enableHighAccuracy: true,
        maximumAge: 1000,
        timeout: 15000
      }
    );
  } catch (e) {
    dioxus.send({ kind: "error", message: String(e) });
  }
})();
"#;

#[derive(Deserialize)]
struct GeolocationEvent {
    kind: String,
    lat: Option<f64>,
    lon: Option<f64>,
    message: Option<String>,
}

pub async fn run(mut user_gps: Signal<Option<(f64, f64)>>) {
    let mut eval = document::eval(GEOLOCATION_WATCH_JS);

    loop {
        match eval.recv::<GeolocationEvent>().await {
            Ok(event) if event.kind == "position" => {
                if let (Some(lat), Some(lon)) = (event.lat, event.lon) {
                    user_gps.set(Some((lat, lon)));
                }
            }
            Ok(event) if event.kind == "error" => {
                if let Some(message) = event.message {
                    eprintln!("GPS watch error: {message}");
                }
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!("GPS watch stopped: {err}");
                break;
            }
        }
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
