//! Inert saved settings only. Never commands, qualification, QD or abort state.
use crate::{auto_zero, fill_targets, flight_setup};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const FILL: &str = "mission/fill_targets";
pub const LIMITS: &str = "mission/gse_limits";
pub const FLIGHT: &str = "mission/saved_flight_setup";
pub const AUTO_ZERO: &str = "mission/auto_zero_preference";

#[derive(Clone)]
pub struct Settings {
    pub fill: fill_targets::FillTargetsConfig,
    pub limits: gse_sequence::Config,
    pub flight: flight_setup::FlightSetupConfig,
    pub auto_zero: auto_zero::Settings,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    nitrogen_target_psi: f32,
    pressure_step_psi: f32,
    pressure_ceiling_psi: Option<f32>,
    maximum_zero_offset_psi: Option<f32>,
    grouped_panel: bool,
}
impl Limits {
    fn from_config(c: &gse_sequence::Config) -> Self {
        Self {
            nitrogen_target_psi: c.nitrogen_target_psi,
            pressure_step_psi: c.pressure_step_psi,
            pressure_ceiling_psi: c.pressure_ceiling_psi,
            maximum_zero_offset_psi: c.maximum_zero_offset_psi,
            grouped_panel: c.grouped_panel,
        }
    }
    fn install(self, c: &mut gse_sequence::Config) {
        c.nitrogen_target_psi = self.nitrogen_target_psi;
        c.pressure_step_psi = self.pressure_step_psi;
        c.pressure_ceiling_psi = self.pressure_ceiling_psi;
        c.maximum_zero_offset_psi = self.maximum_zero_offset_psi;
        c.grouped_panel = self.grouped_panel;
    }
}
// Preserve f32's shortest wire representation rather than expanding it to f64.
fn wire_value(value: impl Serialize) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&serde_json::to_string(&value)?)?)
}
impl Settings {
    pub fn snapshot(&self) -> anyhow::Result<BTreeMap<String, Value>> {
        // Raw saved targets, never flight_sim::effective_fill_targets.
        let values: BTreeMap<String, Value> = BTreeMap::from([
            (FILL.into(), wire_value(&self.fill)?),
            (
                LIMITS.into(),
                wire_value(Limits::from_config(&self.limits))?,
            ),
            (FLIGHT.into(), wire_value(&self.flight)?),
            (AUTO_ZERO.into(), wire_value(self.auto_zero)?),
        ]);
        for (key, value) in &values {
            self.decode(key, value)?;
        }
        Ok(values)
    }
    fn decode(&self, key: &str, value: &Value) -> anyhow::Result<Self> {
        let mut next = self.clone();
        let canonical = match key {
            FILL => {
                let cfg: fill_targets::FillTargetsConfig = serde_json::from_value(value.clone())?;
                next.fill = fill_targets::canonical_settings(cfg);
                wire_value(&next.fill)?
            }
            LIMITS => {
                let limits: Limits = serde_json::from_value(value.clone())?;
                limits.install(&mut next.limits);
                next.limits
                    .validate_settings()
                    .map_err(anyhow::Error::msg)?;
                wire_value(Limits::from_config(&next.limits))?
            }
            FLIGHT => {
                let cfg: flight_setup::FlightSetupConfig = serde_json::from_value(value.clone())?;
                next.flight = flight_setup::canonical_settings(cfg);
                wire_value(&next.flight)?
            }
            AUTO_ZERO => {
                next.auto_zero = serde_json::from_value(value.clone())?;
                wire_value(next.auto_zero)?
            }
            _ => anyhow::bail!("Unsupported inert mission document"),
        };
        // Reject missing/unknown fields and values that saving would silently change.
        anyhow::ensure!(&canonical == value, "Noncanonical saved mission settings");
        Ok(next)
    }
    pub fn validate_document(
        &self,
        key: &str,
        document: &paired_settings::Document,
    ) -> anyhow::Result<()> {
        for revision in &document.heads {
            self.decode(key, &revision.value)?;
        }
        Ok(())
    }
    pub fn replace(&self, pending: &paired_settings::coordinator::Pending) -> anyhow::Result<Self> {
        let current = self.snapshot()?;
        anyhow::ensure!(
            current.get(&pending.key) == Some(&pending.expected_local),
            "Saved mission settings changed locally"
        );
        self.decode(&pending.key, &pending.revision.value)
    }
    /// Caller holds safe-state and per-setting locks through save and replacement.
    /// A failed save leaves memory unchanged and the incoming revision pending.
    pub fn apply_and_save(
        &mut self,
        pending: &paired_settings::coordinator::Pending,
        import_allowed: bool,
        auto_zero_pending: bool,
        save: impl FnOnce(&str, &Self) -> anyhow::Result<()>,
    ) -> anyhow::Result<bool> {
        if !import_allowed || auto_zero_pending {
            return Ok(false);
        }
        let next = self.replace(pending)?;
        save(&pending.key, &next)?;
        *self = next;
        Ok(true)
    }
}
pub fn can_view(key: &str, view_data: bool, calibration_view: bool) -> bool {
    view_data
        && if key.starts_with("calibration/") {
            calibration_view
        } else {
            matches!(key, FILL | LIMITS | FLIGHT | AUTO_ZERO)
        }
}
pub fn can_resolve(
    key: &str,
    view_data: bool,
    calibration_edit: bool,
    send_commands: bool,
) -> bool {
    view_data
        && if key.starts_with("calibration/") {
            calibration_edit
        } else {
            matches!(key, FILL | LIMITS | FLIGHT | AUTO_ZERO) && send_commands
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paired_settings::coordinator::Coordinator;
    use serde_json::json;
    fn model() -> Settings {
        Settings {
            fill: Default::default(),
            limits: Default::default(),
            flight: Default::default(),
            auto_zero: Default::default(),
        }
    }
    fn peers() -> Vec<String> {
        vec!["gs".into(), "rrls".into()]
    }
    fn coordinator(id: &str, s: &Settings, bootstrap: bool) -> Coordinator {
        Coordinator::new(
            "test".into(),
            id.into(),
            s.snapshot().unwrap(),
            bootstrap,
            &peers(),
        )
        .unwrap()
    }
    #[test]
    fn catalog_contains_only_inert_fields_and_preserves_local_dry_confirmation() {
        let mut a = model();
        a.limits.dry_self_test_confirmed = true;
        let docs = a.snapshot().unwrap();
        assert_eq!(docs.len(), 4);
        assert!(
            !docs[LIMITS]
                .as_object()
                .unwrap()
                .contains_key("dry_self_test_confirmed")
        );
        assert!(docs.keys().all(|k| !k.starts_with("calibration/")));
        let mut next = docs[LIMITS].clone();
        next["pressure_step_psi"] = json!(25.0);
        let p = paired_settings::coordinator::Pending {
            key: LIMITS.into(),
            expected_local: docs[LIMITS].clone(),
            revision: paired_settings::Revision {
                clock: Default::default(),
                value: next,
            },
        };
        assert!(a.replace(&p).unwrap().limits.dry_self_test_confirmed);
        let mut bad = p;
        bad.revision.value["dry_self_test_confirmed"] = json!(false);
        assert!(a.replace(&bad).is_err());
    }
    #[test]
    fn offline_edits_conflict_and_deferred_import_survives_restart_without_echo() {
        let mut a = model();
        let mut b = model();
        let mut ca = coordinator("gs", &a, true);
        let mut cb = coordinator("rrls", &b, false);
        for (k, d) in ca.store().documents() {
            cb.merge("test", k, d, &peers()).unwrap();
        }
        for p in cb.pending() {
            b.apply_and_save(&p, true, false, |_, _| Ok(())).unwrap();
            cb.applied(&p).unwrap();
        }
        a.fill.nitrous.target_mass_kg = 21.;
        b.fill.nitrous.target_mass_kg = 24.;
        ca.observe(&a.snapshot().unwrap(), &peers()).unwrap();
        cb.observe(&b.snapshot().unwrap(), &peers()).unwrap();
        ca.merge("test", FILL, cb.store().document(FILL).unwrap(), &peers())
            .unwrap();
        assert_eq!(ca.store().document(FILL).unwrap().heads.len(), 2);
        let reviewed = ca.store().document(FILL).unwrap().clone();
        ca.resolve(
            FILL,
            &reviewed,
            b.snapshot().unwrap()[FILL].clone(),
            &peers(),
        )
        .unwrap();
        let path = std::env::temp_dir().join(format!("stage1-{}.json", std::process::id()));
        ca.save(&path).unwrap();
        let mut ca = Coordinator::load(&path, "test", "gs", &peers()).unwrap();
        let pending = ca.pending().into_iter().find(|p| p.key == FILL).unwrap();
        let doc = ca.store().document(FILL).unwrap().clone();
        assert!(
            !a.apply_and_save(&pending, false, false, |_, _| panic!("must defer"))
                .unwrap()
        );
        for _ in 0..3 {
            ca.observe(&a.snapshot().unwrap(), &peers()).unwrap();
            assert_eq!(ca.store().document(FILL).unwrap(), &doc);
        }
        assert!(
            a.apply_and_save(&pending, true, false, |_, _| Ok(()))
                .unwrap()
        );
        ca.applied(&pending).unwrap();
        ca.save(&path).unwrap();
        let mut ca = Coordinator::load(&path, "test", "gs", &peers()).unwrap();
        let before = ca.store().documents().clone();
        let changed = ca.observe(&a.snapshot().unwrap(), &peers()).unwrap();
        if changed {
            for (k, d) in ca.store().documents() {
                if before.get(k) != Some(d) {
                    eprintln!("changed key {k}: before={:?} after={:?}", before.get(k), d);
                }
            }
        }
        assert!(!changed);
        assert!(ca.pending().is_empty());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn save_failure_pending_autozero_and_local_cas_never_mutate_settings() {
        let mut a = model();
        let mut c = coordinator("gs", &a, true);
        let value = json!({"enabled":false});
        let old = c.store().document(AUTO_ZERO).unwrap().clone();
        c.resolve(AUTO_ZERO, &old, value, &peers()).unwrap();
        let p = c
            .pending()
            .into_iter()
            .find(|p| p.key == AUTO_ZERO)
            .unwrap();
        assert!(
            !a.apply_and_save(&p, true, true, |_, _| panic!("pending capture"))
                .unwrap()
        );
        assert!(
            a.apply_and_save(&p, true, false, |_, _| anyhow::bail!("disk full"))
                .is_err()
        );
        assert!(a.auto_zero.enabled);
        a.auto_zero.enabled = false;
        assert!(a.replace(&p).is_err());
    }
    #[test]
    fn unknown_fields_bad_limits_and_noncanonical_profiles_are_rejected() {
        let a = model();
        for (key, mut v) in a.snapshot().unwrap() {
            v["abort"] = json!(true);
            assert!(a.decode(&key, &v).is_err());
        }
        let mut v = a.snapshot().unwrap()[LIMITS].clone();
        v["pressure_step_psi"] = json!(-1);
        assert!(a.decode(LIMITS, &v).is_err());
        let mut v = a.snapshot().unwrap()[FLIGHT].clone();
        v["selected_profile_id"] = json!("missing");
        assert!(a.decode(FLIGHT, &v).is_err());
        let mut v = a.snapshot().unwrap()[FILL].clone();
        v["nitrous"]["target_mass_kg"] = json!(0);
        assert!(a.decode(FILL, &v).is_err());
    }
    #[test]
    fn conflict_permissions_match_existing_setting_apis() {
        assert!(can_view(FILL, true, false));
        assert!(!can_view("calibration/ch1", true, false));
        assert!(!can_resolve(FILL, true, true, false));
        assert!(can_resolve(FILL, true, false, true));
        assert!(!can_resolve("calibration/ch1", true, false, true));
        assert!(can_resolve("calibration/ch1", true, true, false));
        assert!(!can_resolve("operational/abort", true, true, true));
    }
}
