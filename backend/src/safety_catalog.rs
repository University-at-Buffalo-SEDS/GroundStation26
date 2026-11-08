//! Read-only "About safety" catalog: every station safety rule and interlock,
//! with the live values that parameterise it and where it is enforced.
//!
//! Each rule names the function that enforces it (`enforced_in`), and every
//! rule that a configuration value parameterises names its key
//! (`config_keys`). A test fails if a listed key disappears from the live
//! configuration, so the page and the enforcement cannot silently drift.
//! When a rule changes, update its entry here and the enforcement comment.
use crate::state::AppState;
use crate::auth::Permission;
use axum::{
    Json,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;

#[derive(Clone, Copy, Serialize, PartialEq, Eq, Debug)]
pub(crate) struct Modes {
    pub flight: bool,
    pub hitl: bool,
    pub test_fire: bool,
}
const ALL: Modes = Modes { flight: true, hitl: true, test_fire: true };
const NOT_HITL: Modes = Modes { flight: true, hitl: false, test_fire: true };
const FLIGHT: Modes = Modes { flight: true, hitl: false, test_fire: false };
const HITL: Modes = Modes { flight: false, hitl: true, test_fire: false };

#[derive(Serialize, Debug)]
pub(crate) struct Rule {
    pub id: &'static str,
    pub title: &'static str,
    pub summary: String,
    pub details: Vec<String>,
    pub modes: Modes,
    pub applies_in_this_build: bool,
    pub enforced_in: &'static [&'static str],
    pub config_keys: &'static [&'static str],
    pub values: Map<String, Value>,
}
#[derive(Serialize, Debug)]
pub(crate) struct Section {
    pub id: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    pub rules: Vec<Rule>,
}
#[derive(Serialize, Debug)]
pub(crate) struct Catalog {
    pub version: u16,
    pub build_mode: &'static str,
    pub sections: Vec<Section>,
}

pub(crate) fn build_mode() -> &'static str {
    if cfg!(feature = "hitl_mode") {
        "hitl"
    } else if cfg!(feature = "test_fire_mode") {
        "test_fire"
    } else {
        "flight"
    }
}
fn applies(modes: Modes) -> bool {
    match build_mode() {
        "hitl" => modes.hitl,
        "test_fire" => modes.test_fire,
        _ => modes.flight,
    }
}

/// Live configuration documents that rules cite by `document.key`.
pub(crate) struct Sources {
    pub docs: Map<String, Value>,
}
impl Sources {
    pub(crate) fn capture(state: &AppState) -> Self {
        let mut docs = Map::new();
        let config = state.gse.lock().unwrap().engine.config.clone();
        docs.insert("gse".into(), serde_json::to_value(&config).unwrap_or(Value::Null));
        docs.insert(
            "fill_targets".into(),
            serde_json::to_value(state.fill_targets_snapshot()).unwrap_or(Value::Null),
        );
        docs.insert("auto_zero".into(), crate::auto_zero::catalog_settings(state));
        docs.insert("station".into(), station_values(state));
        Self { docs }
    }
    /// Resolve `document.key[.nested]`. None when the key no longer exists.
    pub(crate) fn get(&self, path: &str) -> Option<Value> {
        let mut parts = path.split('.');
        let mut value = self.docs.get(parts.next()?)?;
        for part in parts {
            value = value.as_object()?.get(part)?;
        }
        Some(value.clone())
    }
}

/// Station-wide values that are not part of the GSE configuration document.
fn station_values(state: &AppState) -> Value {
    let policy = state.action_policy_snapshot();
    json!({
        "valve_report_fresh_ms": 2000,
        "pressure_fresh_ms": 2000,
        "valve_ack_timeout_ms": gse_sequence::VALVE_ACK_TIMEOUT_MS,
        "unconfirmed_valve_timeout_ms": crate::state::UMBILICAL_PENDING_VALVE_TIMEOUT.as_millis() as u64,
        "board_recent_ms": 3000,
        "refill_noop_kg": 0.2,
        "overpressure_debounce_samples": crate::gse::overpressure::DEBOUNCE_SAMPLES,
        "stuck_supply_pressure_rise_psi": gse_sequence::STUCK_SUPPLY_PRESSURE_RISE_PSI,
        "supply_enforcement_enabled": state.supply_enforcement_enabled(),
        "key_enabled": policy.key_enabled,
        "software_buttons_enabled": policy.software_buttons_enabled,
        "qd_settle_ms": 5000,
        "qd_ack_timeout_ms": 5000,
        "board_offline_abort_ms": 6000,
        "board_offline_display_ms": 12000,
        "plumbing_completion_timeout_ms": plumbing_timeout(state),
        "ignition_pair_proof_enforced": crate::ignition_pair::proof_enforced(state),
        "checklist_enforcement_enabled": state.checklist_enforcement_enabled(),
        "hitl_button_interlock_enabled": hitl_flag(state, 4),
        "hitl_launch_interlock_enabled": hitl_flag(state, 5),
        "hitl_plumbing_delay_interlock_enabled": hitl_flag(state, 0),
        "hitl_checklist_interlock_enabled": hitl_flag(state, 1),
        "hitl_supply_interlock_enabled": hitl_flag(state, 2),
        "hitl_board_proof_interlock_enabled": hitl_flag(state, 3),
    })
}
fn plumbing_timeout(state: &AppState) -> Value {
    #[cfg(not(feature = "hitl_mode"))]
    {
        json!(crate::actuator_plumbing::completion_timeout_ms(state))
    }
    #[cfg(feature = "hitl_mode")]
    {
        let _ = state;
        Value::Null
    }
}
/// HITL switches: 0 plumbing delay, 1 checklists, 2 supply, 3 board proof,
/// 4 all-buttons enable, 5 launch interlock. Null outside HITL (always on).
fn hitl_flag(state: &AppState, which: usize) -> Value {
    #[cfg(feature = "hitl_mode")]
    {
        Value::Bool(match which {
            4 => state.hitl_button_interlock_enabled(),
            5 => state.hitl_launch_interlock_enabled(),
            i => state.hitl_safety_interlock_enabled(i),
        })
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        let _ = (state, which);
        Value::Null
    }
}

struct Spec {
    id: &'static str,
    title: &'static str,
    summary: &'static str,
    details: &'static [&'static str],
    modes: Modes,
    enforced_in: &'static [&'static str],
    config_keys: &'static [&'static str],
}

/// `{key}` in summaries/details is replaced by the live value of that key.
fn render(text: &str, values: &Map<String, Value>) -> String {
    let mut out = text.to_string();
    for (key, value) in values {
        let shown = match value {
            Value::Number(n) => n
                .as_f64()
                .map(|f| if f.fract() == 0.0 { format!("{}", f as i64) } else { format!("{f}") })
                .unwrap_or_else(|| n.to_string()),
            Value::Null => "unset".into(),
            Value::Bool(b) => if *b { "on" } else { "off" }.into(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        out = out.replace(&format!("{{{key}}}"), &shown);
    }
    out
}

fn rule(spec: &Spec, sources: &Sources) -> Rule {
    let mut values = Map::new();
    for key in spec.config_keys {
        values.insert((*key).into(), sources.get(key).unwrap_or(Value::Null));
    }
    Rule {
        id: spec.id,
        title: spec.title,
        summary: render(spec.summary, &values),
        details: spec.details.iter().map(|d| render(d, &values)).collect(),
        modes: spec.modes,
        applies_in_this_build: applies(spec.modes),
        enforced_in: spec.enforced_in,
        config_keys: spec.config_keys,
        values,
    }
}

include!("safety_catalog_rules.rs");

pub(crate) fn catalog(state: &AppState) -> Catalog {
    let sources = Sources::capture(state);
    Catalog {
        version: 1,
        build_mode: build_mode(),
        sections: SECTIONS
            .iter()
            .map(|(id, title, summary, specs)| Section {
                id,
                title,
                summary,
                rules: specs.iter().map(|spec| rule(spec, &sources)).collect(),
            })
            .collect(),
    }
}

pub(crate) async fn about_route(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return error;
    }
    Json(catalog(&state)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_cited_config_key_exists_in_the_live_configuration() {
        let state = crate::state::tests::test_app_state().await;
        let sources = Sources::capture(&state);
        for (_, _, _, specs) in SECTIONS {
            for spec in *specs {
                for key in spec.config_keys {
                    assert!(
                        sources.get(key).is_some(),
                        "safety rule {} cites missing config key {key}",
                        spec.id
                    );
                }
                assert!(!spec.enforced_in.is_empty(), "rule {} has no enforcement citation", spec.id);
            }
        }
    }

    #[tokio::test]
    async fn catalog_renders_live_values_and_mode_applicability() {
        let state = crate::state::tests::test_app_state().await;
        {
            let mut rt = state.gse.lock().unwrap();
            rt.engine.config.overpressure_relief_psi = 820.0;
            rt.engine.config.overpressure_abort_psi = 930.0;
            rt.engine.config.overpressure_pilot_ms = 2500;
        }
        let catalog = catalog(&state);
        assert_eq!(catalog.build_mode, build_mode());
        let all: Vec<&Rule> = catalog.sections.iter().flat_map(|s| s.rules.iter()).collect();
        let relief = all.iter().find(|r| r.id == "overpressure-relief").unwrap();
        assert!(relief.summary.contains("820 psi"), "{}", relief.summary);
        let abort = all.iter().find(|r| r.id == "overpressure-abort").unwrap();
        assert!(abort.summary.contains("930 psi") && abort.summary.contains("2500 ms"), "{}", abort.summary);
        assert!(all.iter().all(|r| !r.summary.contains('{')), "unrendered placeholder");
        let ids: std::collections::BTreeSet<_> = all.iter().map(|r| r.id).collect();
        assert_eq!(ids.len(), all.len(), "rule ids are unique");
        for required in [
            "overpressure-relief", "overpressure-abort", "overpressure-after-t0", "overpressure-debounce",
            "overpressure-latch", "nitrous-overfill", "failure-pause", "failure-dump", "stuck-supply",
            "nitrogen-hold", "nitrogen-hold-choice", "pause-resume-cancel", "launch-board-abort",
            "launch-lock-reset", "launch-turbocharge", "launch-failure-pause", "launch-signoff",
            "turbocharge-limits", "auto-zero-before-launch", "valve-board-autonomy",
            "automatic-abort", "no-automatic-abort-after-launch", "manual-stop", "reset-all-boards",
            "ignition-pair", "hitl-interlocks", "key-interlock", "plumbing-retraction", "checklists",
            "valve-freshness", "supply-interlock", "fill-limits", "qd-settle", "igniter-sense",
            "stale-data", "connectivity-loss", "mission-phases", "fc-owns-landing", "board-heartbeat-abort", "board-vb-overpressure", "board-abort-latch", "board-fc-vigilant",
        ] {
            assert!(ids.contains(required), "missing safety rule {required}");
        }
        let json = serde_json::to_value(&catalog).unwrap();
        assert!(json["sections"].as_array().unwrap().len() >= 6);
    }

    #[tokio::test]
    async fn about_route_requires_authentication() {
        let state = crate::state::tests::test_app_state().await;
        let response = about_route(State(state), HeaderMap::new()).await;
        assert!(!response.status().is_success(), "unauthenticated read refused: {}", response.status());
    }
}

#[cfg(test)]
mod citation_tests {
    use super::*;
    /// Every station-side citation names a function that exists in that file.
    #[test]
    fn every_enforcement_citation_points_at_existing_code() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        for (_, _, _, specs) in SECTIONS {
            for spec in *specs {
                for cite in spec.enforced_in {
                    const FIRMWARE: [&str; 7] = ["ValveBoard26/", "ActuatorBoard26/", "FlightComputer26/", "DAQ-Board/", "gateway-board26/", "RFBoard26/", "PowerBoard26/"];
                    if FIRMWARE.iter().any(|repo| cite.starts_with(repo)) {
                        continue; // firmware lives in its own repository (file:line citations)
                    }
                    let (path, function) = cite.split_once(':').unwrap_or((cite, ""));
                    let source = std::fs::read_to_string(repo.join(path))
                        .unwrap_or_else(|e| panic!("rule {}: {path}: {e}", spec.id));
                    let name = function.rsplit("::").next().unwrap_or(function);
                    if name.is_empty() {
                        continue;
                    }
                    let found = [format!("fn {name}("), format!("fn {name}<"), format!("struct {name}"), format!("mod {name}")]
                        .iter()
                        .any(|needle| source.contains(needle.as_str()));
                    assert!(found, "rule {} cites {cite}, which no longer exists", spec.id);
                }
            }
        }
    }
}
