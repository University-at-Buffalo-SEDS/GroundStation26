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
    #[serde(default)]
    pub pending_checklists: Vec<crate::checklists::CompletionEvent>,
    #[serde(default)]
    pub checklist_digest: Option<String>,
}
impl Saved {
    fn validate(&self) -> Result<(), String> {
        if self.checklist_digest.as_ref().is_some_and(|digest| {
            digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err("Invalid checklist qualification binding".into());
        }
        if self.pending_checklists.len() > 3 {
            return Err("Too many pending sequence completion events".into());
        }
        let mut ids = std::collections::HashSet::new();
        for event in &self.pending_checklists {
            let milestone = match event.sequence {
                crate::checklists::Sequence::ValveSelfTest => Milestone::SelfTest,
                crate::checklists::Sequence::NitrogenTest => Milestone::NitrogenTest,
                crate::checklists::Sequence::FillTarget => Milestone::FillTarget,
            };
            if event.id.is_empty()
                || event.id.len() > 80
                || !event.id.bytes().all(|b| b.is_ascii_hexdigit())
                || !ids.insert(&event.id)
                || match &event.action {
                    crate::checklists::CompletionAction::Passed => {
                        !self.qualification.completed(milestone)
                    }
                    crate::checklists::CompletionAction::Override { operator } => {
                        event.sequence != crate::checklists::Sequence::NitrogenTest
                            || self
                                .qualification
                                .nitrogen_override()
                                .is_none_or(|e| &e.operator != operator)
                    }
                    crate::checklists::CompletionAction::Clear => {
                        !((event.sequence == crate::checklists::Sequence::NitrogenTest
                            && self.qualification.stage()
                                == gse_sequence::qualification::MissionStage::Fire)
                            || (self.qualification.stage()
                                == gse_sequence::qualification::MissionStage::Idle
                                && !self.qualification.completed(milestone)))
                    }
                }
            {
                return Err("Invalid pending sequence completion event".into());
            }
        }
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
        if self.qualification.nitrogen_satisfied() && self.baseline.is_none() {
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
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
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
        self.bind_checklists()?;
        if self.saved.pending_checklists.len() >= 3 {
            return Err("Save pending checklist progress before another test".into());
        }
        self.saved
            .qualification
            .complete(milestone, self.run_generation)
            .map_err(str::to_string)?;
        self.saved
            .pending_checklists
            .push(crate::checklists::CompletionEvent {
                id: crate::checklists::paired::completion_event_id(),
                sequence: match milestone {
                    Milestone::SelfTest => crate::checklists::Sequence::ValveSelfTest,
                    Milestone::NitrogenTest => crate::checklists::Sequence::NitrogenTest,
                    Milestone::FillTarget => crate::checklists::Sequence::FillTarget,
                },
                at_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
                action: crate::checklists::CompletionAction::Passed,
            });
        self.persist()
    }
    pub fn begin_fill(&mut self) -> Result<(), String> {
        self.bind_checklists()?;
        self.saved
            .qualification
            .begin_fill(self.run_generation)
            .map_err(str::to_string)?;
        self.persist()
    }
    pub fn override_cold_flow(
        &mut self,
        evidence: gse_sequence::qualification::ColdFlowOverride,
    ) -> Result<(), String> {
        self.bind_checklists()?;
        if !self.saved.pending_checklists.is_empty() {
            return Err("Save pending checklist progress first".into());
        }
        self.saved
            .qualification
            .override_cold_flow(evidence.clone())
            .map_err(str::to_string)?;
        self.saved
            .pending_checklists
            .push(crate::checklists::CompletionEvent {
                id: crate::checklists::paired::completion_event_id(),
                sequence: crate::checklists::Sequence::NitrogenTest,
                at_ms: evidence.at_ms,
                action: crate::checklists::CompletionAction::Override {
                    operator: evidence.operator,
                },
            });
        self.persist()?;
        self.flush_checklists()
    }
    pub fn commit_fire(&mut self) -> Result<(), String> {
        self.bind_checklists()?;
        if !self.saved.pending_checklists.is_empty() {
            return Err("Save pending checklist progress first".into());
        }
        let mut next = self.saved.clone();
        next.qualification.commit_fire().map_err(str::to_string)?;
        next.pending_checklists
            .push(crate::checklists::CompletionEvent {
                id: crate::checklists::paired::completion_event_id(),
                sequence: crate::checklists::Sequence::NitrogenTest,
                at_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
                action: crate::checklists::CompletionAction::Clear,
            });
        let result = self
            .path
            .as_ref()
            .ok_or_else(|| "Qualification storage is not initialized".to_string())
            .and_then(|path| next.save(path));
        if let Err(error) = &result {
            self.saved.qualification.latch(Fault::MissingTelemetry);
            self.persist_error = Some(error.clone());
        }
        result?;
        self.saved = next;
        Ok(())
    }
    pub fn flush_checklists(&mut self) -> Result<(), String> {
        if crate::checklists::paired::requested() {
            while let Some(event) = self.saved.pending_checklists.first() {
                if !crate::checklists::paired::deliver_event(event)? {
                    break;
                }
                self.saved.pending_checklists.remove(0);
                self.persist()?;
            }
            return Ok(());
        }
        self.flush_checklists_with(crate::checklists::sequence_completed)
    }
    fn bind_checklists(&mut self) -> Result<(), String> {
        if !crate::checklists::paired::qualification_allowed(
            self.saved.checklist_digest.as_deref(),
            self.has_checklist_evidence(),
        ) {
            return Err("Checklist template changed or is not durably ready; return to Idle and reset qualification".into());
        }
        if let Some(digest) = crate::checklists::paired::qualification_binding()? {
            self.saved.checklist_digest = Some(digest);
        }
        Ok(())
    }
    pub fn has_checklist_evidence(&self) -> bool {
        [
            Milestone::SelfTest,
            Milestone::NitrogenTest,
            Milestone::FillTarget,
        ]
        .into_iter()
        .any(|milestone| self.saved.qualification.completed(milestone))
            || self.saved.qualification.nitrogen_override().is_some()
    }
    pub fn queue_checklist_reset(&mut self) {
        if !crate::checklists::paired::requested() {
            return;
        }
        for sequence in [
            crate::checklists::Sequence::ValveSelfTest,
            crate::checklists::Sequence::NitrogenTest,
            crate::checklists::Sequence::FillTarget,
        ] {
            self.saved
                .pending_checklists
                .push(crate::checklists::CompletionEvent {
                    id: crate::checklists::paired::completion_event_id(),
                    sequence,
                    at_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
                    action: crate::checklists::CompletionAction::Clear,
                });
        }
    }
    fn flush_checklists_with(
        &mut self,
        mut deliver: impl FnMut(&crate::checklists::CompletionEvent) -> Result<(), String>,
    ) -> Result<(), String> {
        while let Some(event) = self.saved.pending_checklists.first() {
            if let Err(error) = deliver(event) {
                self.persist_error = Some(error.clone());
                return Err(error);
            }
            self.saved.pending_checklists.remove(0);
            self.persist()?;
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_template_binding_and_local_reset_receipts_survive_restart() {
        let path = std::env::temp_dir().join(format!(
            "gse-paired-binding-{}.json",
            rand::random::<u128>()
        ));
        let digest = "a".repeat(64);
        let mut saved = Saved {
            checklist_digest: Some(digest.clone()),
            ..Default::default()
        };
        saved
            .qualification
            .complete(Milestone::SelfTest, 0)
            .unwrap();
        saved.save(&path).unwrap();
        let restored = Saved::load(&path).unwrap();
        assert_eq!(restored.checklist_digest.as_deref(), Some(digest.as_str()));
        assert!(restored.qualification.completed(Milestone::SelfTest));
        // Explicit local reset is data delivered to matching checklist items,
        // while the operational milestone itself is already locally cleared.
        let mut reset = Saved::default();
        for (index, sequence) in [
            crate::checklists::Sequence::ValveSelfTest,
            crate::checklists::Sequence::NitrogenTest,
            crate::checklists::Sequence::FillTarget,
        ]
        .into_iter()
        .enumerate()
        {
            reset
                .pending_checklists
                .push(crate::checklists::CompletionEvent {
                    id: format!("{}{:016x}", digest, index),
                    sequence,
                    at_ms: 100,
                    action: crate::checklists::CompletionAction::Clear,
                });
        }
        reset.save(&path).unwrap();
        let restored = Saved::load(&path).unwrap();
        assert_eq!(restored.pending_checklists.len(), 3);
        assert!(!restored.qualification.completed(Milestone::SelfTest));
        assert!(restored.checklist_digest.is_none());
        let mut invalid = restored;
        invalid.checklist_digest = Some("bad".into());
        assert!(invalid.save(&path).is_err());
        assert!(Saved::load(&path).unwrap().checklist_digest.is_none());
        std::fs::remove_file(path).unwrap();
    }
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

#[cfg(test)]
mod checklist_outbox_tests {
    use super::*;
    #[test]
    fn restart_retries_the_same_saved_event_and_storage_failure_never_acks_it() {
        let path = std::env::temp_dir().join(format!("gse-outbox-{}.json", rand::random::<u128>()));
        let mut op = Operational {
            path: Some(path.clone()),
            ..Default::default()
        };
        op.complete(Milestone::SelfTest).unwrap();
        let event = op.saved.pending_checklists[0].clone();
        assert!(
            op.flush_checklists_with(|_| Err("checklist disk unavailable".into()))
                .is_err()
        );
        assert_eq!(
            Saved::load(&path).unwrap().pending_checklists,
            vec![event.clone()]
        );
        let mut restored = Operational {
            path: Some(path.clone()),
            saved: Saved::load(&path).unwrap(),
            ..Default::default()
        };
        let mut calls = 0;
        restored
            .flush_checklists_with(|received| {
                assert_eq!(received, &event);
                calls += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert!(Saved::load(&path).unwrap().pending_checklists.is_empty());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn unfinished_or_stale_runs_cannot_enqueue_a_success() {
        let mut op = Operational::default();
        op.run_generation = 1;
        assert!(op.complete(Milestone::SelfTest).is_err());
        assert!(op.saved.pending_checklists.is_empty());
        assert!(!op.saved.qualification.completed(Milestone::SelfTest));
    }
}

#[cfg(test)]
mod fire_persistence_tests {
    use super::*;
    #[test]
    fn failed_fire_save_never_consumes_the_armed_evidence_in_memory() {
        let path =
            std::env::temp_dir().join(format!("gse-fire-blocked-{}", rand::random::<u128>()));
        std::fs::create_dir(&path).unwrap();
        let mut op = Operational {
            path: Some(path.clone()),
            ..Default::default()
        };
        for stage in [
            Milestone::SelfTest,
            Milestone::NitrogenTest,
            Milestone::FillTarget,
        ] {
            op.saved.qualification.complete(stage, 0).unwrap();
        }
        op.saved.baseline = Some(Baseline {
            average_psi: 0.0,
            min_psi: 0.0,
            max_psi: 0.0,
            noise_psi: 0.1,
            samples: 50,
        });
        op.saved.fill_target_kg = Some(8.0);
        assert!(op.commit_fire().is_err());
        assert_eq!(
            op.saved.qualification.stage(),
            gse_sequence::qualification::MissionStage::Armed
        );
        assert!(op.saved.qualification.nitrogen_satisfied());
        assert!(op.saved.pending_checklists.is_empty());
        assert!(op.persist_error.is_some());
        assert!(!path.with_extension("upload").exists());
        assert_eq!(
            op.saved.qualification.fault(),
            Some(Fault::MissingTelemetry)
        );
        std::fs::remove_dir(path).unwrap();
    }
}
