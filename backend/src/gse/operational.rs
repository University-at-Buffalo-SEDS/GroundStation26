//! Operational qualification and restart-safe storage. HITL does not use this policy.
use gse_sequence::{
    Baseline,
    qd_wait::QdWait,
    qualification::{Fault, Milestone, Qualification},
};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Saved {
    pub qualification: Qualification,
    pub baseline: Option<Baseline>,
    pub fill_target_kg: Option<f32>,
}
impl Saved {
    fn validate(&self) -> Result<(), String> {
        if let Some(b) = &self.baseline {
            if b.samples < 3
                || ![b.average_psi, b.min_psi, b.max_psi, b.noise_psi]
                    .into_iter()
                    .all(f32::is_finite)
                || b.min_psi > b.average_psi
                || b.average_psi > b.max_psi
                || b.noise_psi < 0.0
            {
                return Err("Invalid saved pressure baseline".into());
            }
        }
        if self.qualification.completed(Milestone::NitrogenTest) && self.baseline.is_none() {
            return Err("Saved nitrogen test has no pressure baseline".into());
        }
        if self
            .fill_target_kg
            .is_some_and(|v| !v.is_finite() || v <= 0.0)
            || (self.qualification.completed(Milestone::FillTarget)
                && self.fill_target_kg.is_none())
        {
            return Err("Invalid saved fill target".into());
        }
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::File::open(path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(16_385)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                if bytes.len() > 16_384 {
                    return Err("Qualification record exceeds size limit".into());
                }
                let record: Self = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                record.validate()?;
                Ok(record)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.to_string()),
        }
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        let parent = path.parent().ok_or("Missing qualification directory")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let temporary = path.with_extension("upload");
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)?;
            std::fs::File::open(parent)?.sync_all()
        })();
        result.map_err(|e| e.to_string())
    }
}
#[derive(Default)]
pub struct Operational {
    pub saved: Saved,
    pub path: Option<PathBuf>,
    pub run_generation: u64,
    pub running: bool,
    pub target_reached: Option<f32>,
    pub last_correction_ms: Option<u64>,
    pub topology_key: String,
    pub stable_since_ms: Option<u64>,
    pub topology_stable: bool,
    pub topology_healthy: bool,
    pub startup_configured: bool,
    pub qd: QdWait,
    pub qd_dispatch_utc_ms: Option<u64>,
    pub persist_error: Option<String>,
}
impl Operational {
    pub fn topology(&mut self, now: u64, healthy: bool, key: String) {
        if !healthy || self.topology_key != key {
            self.stable_since_ms = None;
            self.topology_key = key;
        }
        self.topology_healthy = healthy;
        if healthy && self.stable_since_ms.is_none() {
            self.stable_since_ms = Some(now);
        }
        self.topology_stable = healthy
            && self
                .stable_since_ms
                .is_some_and(|since| now.saturating_sub(since) >= 2_000);
    }
    pub fn persist(&mut self) -> Result<(), String> {
        let result = self
            .path
            .as_ref()
            .ok_or_else(|| "Qualification storage is not initialized".to_string())
            .and_then(|p| self.saved.save(p));
        if let Err(error) = &result {
            self.saved.qualification.latch(Fault::MissingTelemetry);
            self.persist_error = Some(error.clone());
        }
        result
    }
    pub fn complete(&mut self, milestone: Milestone) -> Result<(), String> {
        self.saved
            .qualification
            .complete(milestone, self.run_generation)
            .map_err(str::to_string)?;
        self.persist()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_retains_ordered_tests_but_never_resumes_outputs_or_queued_launch() {
        let path = std::env::temp_dir().join(format!(
            "gse-qualification-{}-{}.json",
            std::process::id(),
            rand::random::<u128>()
        ));
        let mut op = Operational {
            path: Some(path.clone()),
            ..Default::default()
        };
        op.complete(Milestone::SelfTest).unwrap();
        op.saved.baseline = Some(Baseline {
            average_psi: 0.0,
            min_psi: 0.0,
            max_psi: 0.0,
            noise_psi: 0.1,
            samples: 50,
        });
        op.complete(Milestone::NitrogenTest).unwrap();
        op.saved.fill_target_kg = Some(8.0);
        op.complete(Milestone::FillTarget).unwrap();
        op.qd.dispatched(0);
        op.qd.confirm(20);
        op.qd.queue_launch(100).unwrap();
        op.running = true;
        let restored = Operational {
            saved: Saved::load(&path).unwrap(),
            ..Default::default()
        };
        assert!(restored.saved.qualification.launch_qualified());
        assert!(!restored.running);
        assert!(!restored.qd.queued());
        assert!(!restored.topology_stable);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn corrupt_or_missing_baseline_cannot_restore_fill_permission() {
        let invalid = r#"{"qualification":{"version":1,"generation":0,"self_test":true,"nitrogen_test":true,"fill_target":false,"fault":null},"baseline":null,"fill_target_kg":null}"#;
        let record: Saved = serde_json::from_str(invalid).unwrap();
        assert!(record.validate().is_err());
    }
    #[test]
    fn topology_must_settle_again_after_loss_or_route_change() {
        let mut op = Operational::default();
        op.topology(0, true, "a".into());
        op.topology(1999, true, "a".into());
        assert!(!op.topology_stable);
        op.topology(2000, true, "a".into());
        assert!(op.topology_stable);
        op.topology(2001, false, "a".into());
        assert!(!op.topology_stable);
        op.topology(2100, true, "a".into());
        op.topology(4100, true, "a".into());
        assert!(op.topology_stable);
        op.topology(4101, true, "b".into());
        assert!(!op.topology_stable);
    }
}
