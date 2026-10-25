//! Audience-only telemetry/scene rundown. Operational dashboards use their own data.
use super::contract::{Binding, BroadcastStat};
use crate::{fill_targets::FillSource, types::FlightState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Rundown {
    pub profiles: BTreeMap<String, Profile>,
    pub phase_profiles: BTreeMap<String, String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Profile {
    pub title: String,
    pub cycle_seconds: u32,
    pub stats_per_page: usize,
    /// inherit keeps the operator's current camera layout.
    pub scene: String,
    pub stats: Vec<Stat>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Stat {
    pub label: String,
    pub binding: Binding,
    #[serde(default)]
    pub unit: String,
    #[serde(default = "precision")]
    pub precision: usize,
    #[serde(default = "telemetry")]
    pub source: String,
    #[serde(default = "number")]
    pub format: String,
}
fn precision() -> usize {
    1
}
fn telemetry() -> String {
    "telemetry".into()
}
fn number() -> String {
    "number".into()
}
fn stat(label: &str, ty: &str, sender: Option<&str>, index: usize, unit: &str) -> Stat {
    Stat {
        label: label.into(),
        binding: Binding {
            data_type: ty.into(),
            sender_id: sender.map(str::to_owned),
            index,
            scale: 1.,
            offset: 0.,
        },
        unit: unit.into(),
        precision: 1,
        source: telemetry(),
        format: number(),
    }
}
fn ground() -> Vec<Stat> {
    let mut mass = stat("Fill mass", "LOADCELL_50_WEIGHT_KG", Some("DAQ"), 0, "kg");
    mass.source = "selected_fill_mass".into();
    let mut stats = vec![
        mass,
        stat(
            "Tank pressure",
            "PRESSURE_TRANSDUCER_CALIBRATED",
            None,
            0,
            "psi",
        ),
        stat("Fill level", "LOADCELL_FILL_PERCENT", Some("DAQ"), 0, "%"),
        stat("Fill battery", "BATTERY_VOLTAGE", Some("GB"), 0, "V"),
        stat("Valve battery", "BATTERY_VOLTAGE", Some("VB"), 0, "V"),
    ];
    for (i, label) in ["Pilot", "Vent", "Nitrous", "Dump", "Nitrogen"]
        .into_iter()
        .enumerate()
    {
        let mut item = stat(label, "VALVE_STATE", None, i, "");
        item.format = "valve".into();
        stats.push(item);
    }
    stats
}
fn gps() -> Vec<Stat> {
    let mut sats = stat("GPS satellites", "GPS_SATELLITE_NUMBER", Some("RF"), 0, "");
    sats.precision = 0;
    // Do not infer a lock from satellite count or stale/zero coordinates. Until
    // a node reports an explicit fix type this stays unavailable.
    let mut lock = stat("GPS lock", "GPS_FIX", Some("RF"), 0, "");
    lock.format = "boolean".into();
    lock.precision = 0;
    vec![sats, lock]
}
impl Default for Profile {
    fn default() -> Self {
        Self {
            title: "Programme".into(),
            cycle_seconds: 5,
            stats_per_page: 3,
            scene: "inherit".into(),
            stats: Vec::new(),
        }
    }
}
impl Default for Rundown {
    fn default() -> Self {
        let mut fill = ground();
        fill.extend(gps());
        let mut flight = vec![
            stat("Altitude", "GPS_DATA", Some("RF"), 2, "m"),
            stat("Velocity", "VEHICLE_SPEED", None, 0, "m/s"),
            stat("Rocket battery", "BATTERY_VOLTAGE", Some("PB"), 0, "V"),
        ];
        let mut lat = stat("Latitude", "GPS_DATA", Some("RF"), 0, "°");
        lat.precision = 5;
        let mut lon = stat("Longitude", "GPS_DATA", Some("RF"), 1, "°");
        lon.precision = 5;
        flight.extend([lat, lon]);
        flight.extend(gps());
        let mut fire = ground();
        let mut thrust = stat("Thrust", "LOADCELL_WEIGHT_KG", Some("DAQ"), 0, "N");
        thrust.binding.scale = 9.80665;
        fire.insert(0, thrust);
        let profile = |title: &str, stats| Profile {
            title: title.into(),
            stats,
            ..Default::default()
        };
        Self {
            profiles: BTreeMap::from([
                ("ground".into(), profile("Fill and preparation", fill)),
                ("flight".into(), profile("Flight", flight.clone())),
                ("recovery".into(), profile("Recovery", flight)),
                ("test_fire".into(), profile("Test fire", fire)),
            ]),
            phase_profiles: BTreeMap::new(),
        }
    }
}
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 32
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
pub(super) fn valid_scene(scene: &str) -> bool {
    [
        "inherit",
        "hero",
        "grid",
        "model",
        "grid-model",
        "interview",
        "map",
        "recorded-replay",
    ]
    .contains(&scene)
}
impl Rundown {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.profiles.len() > 8
            || ["ground", "flight", "recovery", "test_fire"]
                .iter()
                .any(|k| !self.profiles.contains_key(*k))
            || self.phase_profiles.len() > 32
        {
            return Err(
                "Keep the four default profiles and at most8 profiles/32 phase assignments",
            );
        }
        for (key, p) in &self.profiles {
            if !valid_key(key)
                || p.title.is_empty()
                || p.title.len() > 96
                || !(1..=30).contains(&p.cycle_seconds)
                || !(1..=6).contains(&p.stats_per_page)
                || !valid_scene(&p.scene)
                || p.stats.len() > 24
            {
                return Err("Invalid programme profile, scene or timing limits");
            }
            for s in &p.stats {
                let b = &s.binding;
                if s.label.is_empty()
                    || s.label.len() > 96
                    || s.unit.len() > 24
                    || s.precision > 6
                    || b.data_type.is_empty()
                    || b.data_type.len() > 96
                    || b.sender_id
                        .as_ref()
                        .is_some_and(|s| s.is_empty() || s.len() > 96)
                    || b.index >= 64
                    || !b.scale.is_finite()
                    || !b.offset.is_finite()
                    || !["telemetry", "selected_fill_mass"].contains(&s.source.as_str())
                    || !["number", "boolean", "valve"].contains(&s.format.as_str())
                {
                    return Err("Invalid audience metric binding or display format");
                }
            }
        }
        for (phase, key) in &self.phase_profiles {
            if serde_json::from_value::<FlightState>(serde_json::json!(phase)).is_err()
                || !self.profiles.contains_key(key)
            {
                return Err("Unknown flight phase or programme profile");
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| "Invalid programme configuration")?
            .len()
            > 128 * 1024
        {
            return Err("Programme configuration exceeds128KiB");
        }
        Ok(())
    }
    pub fn profile(&self, phase: FlightState, test_fire: bool) -> (&str, &Profile) {
        let phase = format!("{phase:?}");
        let id = if test_fire {
            "test_fire"
        } else if let Some(id) = self.phase_profiles.get(&phase) {
            id.as_str()
        } else {
            match phase.as_str() {
                "Startup" | "Idle" | "PreFill" | "FillTest" | "NitrogenFill" | "NitrousFill"
                | "Armed" | "Aborted" => "ground",
                "Landed" | "Recovery" => "recovery",
                _ => "flight",
            }
        };
        (
            id,
            self.profiles
                .get(id)
                .expect("validated default programme profiles"),
        )
    }
}
impl Stat {
    pub fn binding_for_fill(&self, source: FillSource) -> (Binding, bool) {
        let mut b = self.binding.clone();
        let absolute = self.source == "selected_fill_mass" && source == FillSource::Kg1000Absolute;
        if self.source == "selected_fill_mass" {
            b.data_type = if absolute {
                "LOADCELL_WEIGHT_KG"
            } else {
                "LOADCELL_50_WEIGHT_KG"
            }
            .into();
            b.index = 0;
            b.scale = 1.;
            b.offset = 0.;
        }
        (b, absolute)
    }
}
impl From<BroadcastStat> for Stat {
    fn from(s: BroadcastStat) -> Self {
        Self {
            label: s.label,
            binding: s.binding,
            unit: s.unit,
            precision: s.precision,
            source: telemetry(),
            format: number(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_profiles_keep_fill_out_of_flight_and_thrust_in_test_fire() {
        let r = Rundown::default();
        r.validate().unwrap();
        let (_, fill) = r.profile(FlightState::NitrousFill, false);
        assert!(
            !fill
                .stats
                .iter()
                .any(|s| s.label == "Altitude" || s.label == "Velocity")
        );
        let (_, flight) = r.profile(FlightState::Launch, false);
        assert!(
            !flight
                .stats
                .iter()
                .any(|s| s.source == "selected_fill_mass" || s.binding.data_type == "VALVE_STATE")
        );
        let (id, fire) = r.profile(FlightState::Launch, true);
        assert_eq!(id, "test_fire");
        assert!(fire.stats.iter().any(|s| s.label == "Thrust"));
        assert!(fire.stats.iter().any(|s| s.source == "selected_fill_mass"));
        assert!(
            !fire
                .stats
                .iter()
                .any(|s| s.binding.sender_id.as_deref() == Some("RF"))
        );
        assert_eq!(r.profile(FlightState::Recovery, false).0, "recovery");
    }
    #[test]
    fn edits_roundtrip_and_invalid_bounds_fail() {
        let mut r = Rundown::default();
        let p = r.profiles.get_mut("ground").unwrap();
        p.cycle_seconds = 9;
        p.stats_per_page = 2;
        p.stats.reverse();
        p.scene = "map".into();
        r.phase_profiles.insert("Landed".into(), "ground".into());
        let bytes = serde_json::to_vec(&r).unwrap();
        let saved: Rundown = serde_json::from_slice(&bytes).unwrap();
        saved.validate().unwrap();
        assert_eq!(serde_json::to_vec(&saved).unwrap(), bytes);
        assert_eq!(saved.profile(FlightState::Landed, false).0, "ground");
        for bad in [0, 31] {
            let mut x = r.clone();
            x.profiles.get_mut("ground").unwrap().cycle_seconds = bad;
            assert!(x.validate().is_err());
        }
        let mut x = r.clone();
        x.profiles.get_mut("flight").unwrap().stats[0].binding.scale = f32::INFINITY;
        assert!(x.validate().is_err());
        let mut x = r;
        x.phase_profiles.insert("imaginary".into(), "ground".into());
        assert!(x.validate().is_err());
    }
}

#[cfg(test)]
mod persistence_tests {
    use crate::media::{MediaState, contract};
    use std::sync::Arc;
    use tokio::sync::Mutex;
    fn media(app: Arc<crate::state::AppState>, root: &std::path::Path) -> Arc<MediaState> {
        Arc::new(MediaState {
            app,
            client: reqwest::Client::new(),
            api_url: "http://127.0.0.1:9".into(),
            webrtc_url: "http://127.0.0.1:9".into(),
            hls_url: "http://127.0.0.1:9".into(),
            relay_password: String::new(),
            model_write: Mutex::new(()),
            models: root.join("models"),
            recordings: root.join("video"),
            voice: Arc::new(crate::voice::VoiceHub::default()),
            presentation_write: Mutex::new(()),
            tickets: Mutex::new(Vec::new()),
            replays: Mutex::new(Default::default()),
            program_history: Mutex::new(Default::default()),
            hls_segments: Mutex::new(Default::default()),
            preview_sessions: Mutex::new(Vec::new()),
            publishers: Mutex::new(Vec::new()),
            recorded_vehicle_cache: Mutex::new(Default::default()),
        })
    }
    #[tokio::test]
    async fn operator_rundown_persists_reloads_and_rejects_stale_or_unauthorized_edits() {
        use crate::recording_export::tests::{TestDirectory, export_state};
        let root = TestDirectory::new();
        let app = export_state(&root.0, true).await;
        std::fs::write(root.0.join("auth.json"),serde_json::json!({"anonymous":{"view_data":true},"users":[{"username":"producer","roles":["stream_master"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}}]}).to_string()).unwrap();
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('rundown-fixture','producer','session',1,0,0,9999999999999)").execute(&app.auth_db).await.unwrap();
        let first = media(app.clone(), &root.0);
        let mut request = serde_json::to_value(contract::Broadcast::default()).unwrap();
        request["rundown"]["profiles"]["ground"]["cycle_seconds"] = serde_json::json!(9);
        request["rundown"]["profiles"]["ground"]["stats_per_page"] = serde_json::json!(2);
        request["rundown"]["profiles"]["ground"]["stats"]
            .as_array_mut()
            .unwrap()
            .reverse();
        request["rundown"]["phase_profiles"]["Landed"] = serde_json::json!("ground");
        let request: contract::Broadcast = serde_json::from_value(request).unwrap();
        let old = request.clone();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer rundown-fixture".parse().unwrap(),
        );
        let saved = contract::control_broadcast(
            axum::extract::State(first.clone()),
            headers.clone(),
            axum::Json(request),
        )
        .await
        .unwrap()
        .0;
        let expected = serde_json::to_value(&saved).unwrap();
        assert_eq!(expected["revision"], 1);
        drop(first);
        let restarted = media(app, &root.0);
        let disk =
            serde_json::to_value(contract::read_presentation(&restarted).await.unwrap()).unwrap();
        assert_eq!(disk["broadcast"], expected);
        assert_eq!(
            contract::control_broadcast(
                axum::extract::State(restarted.clone()),
                headers,
                axum::Json(old)
            )
            .await
            .err()
            .unwrap()
            .status(),
            axum::http::StatusCode::CONFLICT
        );
        assert_eq!(
            contract::control_broadcast(
                axum::extract::State(restarted),
                axum::http::HeaderMap::new(),
                axum::Json(saved)
            )
            .await
            .err()
            .unwrap()
            .status(),
            axum::http::StatusCode::FORBIDDEN
        );
    }
}
