//! Reconcile persisted edits with typed runtime state without racing an API save.
//! Runtime adapters must compare-and-apply each key under its own settings lock.
use crate::{Clock, Document, Revision, Store};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Observed {
    value: Value,
    clock: Option<Clock>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Coordinator {
    station: String,
    store: Store,
    observed: BTreeMap<String, Observed>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub key: String,
    pub expected_local: Value,
    pub revision: Revision,
}
impl Coordinator {
    /// Only the primary bootstraps existing settings. RRLS records its current
    /// defaults as observed, so they cannot overwrite imported calibration.
    pub fn new(
        installation: String,
        station: String,
        snapshot: BTreeMap<String, Value>,
        bootstrap: bool,
        peers: &[String],
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            crate::identity(&station)
                && peers.contains(&station)
                && snapshot.len() <= crate::MAX_DOCUMENTS,
            "invalid mission coordinator identity/catalog"
        );
        let mut this = Self {
            station,
            store: Store::new(installation)?,
            observed: BTreeMap::new(),
        };
        for (key, value) in snapshot {
            let clock = if bootstrap {
                Some(
                    this.store
                        .edit(&key, None, value.clone(), &this.station, peers)?
                        .heads[0]
                        .clock
                        .clone(),
                )
            } else {
                None
            };
            this.observed.insert(key, Observed { value, clock });
        }
        Ok(this)
    }
    pub fn store(&self) -> &Store {
        &self.store
    }
    /// The caller durably checkpoints the returned state before applying any
    /// pending value. A crash after application but before its final checkpoint
    /// is recognized by the already-imported matching revision on next scan.
    pub fn observe(
        &mut self,
        snapshot: &BTreeMap<String, Value>,
        peers: &[String],
    ) -> anyhow::Result<bool> {
        let keys: BTreeSet<_> = snapshot
            .keys()
            .chain(self.observed.keys())
            .cloned()
            .collect();
        anyhow::ensure!(
            keys.len() <= crate::MAX_DOCUMENTS,
            "mission observation catalog full"
        );
        let mut next = self.clone();
        let mut changed = false;
        for key in keys {
            let value = snapshot.get(&key).cloned().unwrap_or(Value::Null);
            if let Some(observed) = next.observed.get_mut(&key)
                && observed.value == value
            {
                // Importing a revision whose value equals the local default
                // still advances observed context; a later edit is causal.
                if let Some(head) = next
                    .store
                    .document(&key)
                    .and_then(|d| d.heads.iter().find(|h| h.value == value))
                    && observed.clock.as_ref() != Some(&head.clock)
                {
                    observed.clock = Some(head.clock.clone());
                    changed = true;
                }
                continue;
            }
            // Recovery after a file/application write succeeded but the final
            // checkpoint did not. This adopts context, never resolves a conflict.
            let matching = next
                .store
                .document(&key)
                .and_then(|d| d.heads.iter().find(|h| h.value == value))
                .cloned();
            let revision = if let Some(revision) = matching {
                revision
            } else {
                let base = next.observed.get(&key).and_then(|v| {
                    v.clock.as_ref().map(|c| Revision {
                        clock: c.clone(),
                        value: v.value.clone(),
                    })
                });
                let doc =
                    next.store
                        .edit(&key, base.as_ref(), value.clone(), &next.station, peers)?;
                doc.heads
                    .into_iter()
                    .find(|h| h.value == value)
                    .ok_or_else(|| anyhow::anyhow!("local mission edit lost its revision"))?
            };
            next.observed.insert(
                key,
                Observed {
                    value,
                    clock: Some(revision.clock),
                },
            );
            changed = true;
        }
        if changed {
            *self = next;
        }
        Ok(changed)
    }
    pub fn merge(
        &mut self,
        installation: &str,
        key: &str,
        doc: &Document,
        peers: &[String],
    ) -> anyhow::Result<bool> {
        self.store.merge(installation, key, doc, peers)
    }
    pub fn resolve(
        &mut self,
        key: &str,
        expected: &Document,
        value: Value,
        peers: &[String],
    ) -> anyhow::Result<Document> {
        self.store
            .resolve(key, expected, value, &self.station, peers)
    }
    pub fn pending(&self) -> Vec<Pending> {
        self.store
            .documents()
            .iter()
            .filter_map(|(key, doc)| {
                if doc.heads.len() != 1 {
                    return None;
                }
                let observed = self.observed.get(key);
                (observed.is_none_or(|v| v.value != doc.heads[0].value)).then(|| Pending {
                    key: key.clone(),
                    expected_local: observed.map(|v| v.value.clone()).unwrap_or(Value::Null),
                    revision: doc.heads[0].clone(),
                })
            })
            .collect()
    }
    /// Call only after the adapter validated the typed value, verified a safe
    /// application boundary, and atomically compared expected_local with runtime.
    pub fn applied(&mut self, pending: &Pending) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.store
                .document(&pending.key)
                .is_some_and(|d| d.heads.len() == 1 && d.heads[0] == pending.revision),
            "mission value changed during application"
        );
        self.observed.insert(
            pending.key.clone(),
            Observed {
                value: pending.revision.value.clone(),
                clock: Some(pending.revision.clock.clone()),
            },
        );
        Ok(())
    }
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        use std::io::Write;
        let bytes = serde_json::to_vec(self)?;
        anyhow::ensure!(
            bytes.len() <= crate::MAX_STORE_BYTES * 3,
            "mission checkpoint exceeds budget"
        );
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        let temp = path.with_extension("mission-pending");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(temp, path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
    pub fn load(
        path: &std::path::Path,
        installation: &str,
        station: &str,
        peers: &[String],
    ) -> anyhow::Result<Self> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take((crate::MAX_STORE_BYTES * 3 + 1) as u64)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= crate::MAX_STORE_BYTES * 3,
            "mission checkpoint exceeds budget"
        );
        let this: Self = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            this.station == station
                && this.store.installation == installation
                && this.store.version == 1
                && peers.contains(&this.station)
                && this.store.documents.len() <= crate::MAX_DOCUMENTS
                && this.observed.len() <= crate::MAX_DOCUMENTS,
            "mission checkpoint identity/version mismatch"
        );
        for (key, doc) in &this.store.documents {
            crate::validate_document(key, doc, peers)?;
        }
        anyhow::ensure!(
            serde_json::to_vec(&this.store)?.len() <= crate::MAX_STORE_BYTES,
            "mission store exceeds budget"
        );
        for (key, observed) in &this.observed {
            anyhow::ensure!(
                !key.is_empty()
                    && key.len() <= 128
                    && serde_json::to_vec(&observed.value)?.len() <= crate::MAX_DOCUMENT_BYTES,
                "invalid mission observation"
            );
            if let Some(clock) = &observed.clock {
                anyhow::ensure!(
                    !clock.is_empty()
                        && clock.len() <= crate::MAX_HEADS
                        && clock.iter().all(|(n, v)| *v > 0 && peers.contains(n))
                        && this.store.document(key).is_some_and(|d| d
                            .heads
                            .iter()
                            .any(|h| crate::dominates(&h.clock, clock))),
                    "invalid mission observed context"
                );
            }
        }
        Ok(this)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn peers() -> Vec<String> {
        vec!["gs".into(), "rrls".into()]
    }
    fn snapshot(value: i64) -> BTreeMap<String, Value> {
        BTreeMap::from([("calibration:KG50".into(), json!(value))])
    }
    #[test]
    fn rrls_defaults_do_not_overwrite_primary_and_remote_application_is_explicit() {
        let p = peers();
        let gs = Coordinator::new("launch".into(), "gs".into(), snapshot(12), true, &p).unwrap();
        let mut r =
            Coordinator::new("launch".into(), "rrls".into(), snapshot(1), false, &p).unwrap();
        assert!(r.store.documents().is_empty());
        r.observe(&snapshot(1), &p).unwrap();
        assert!(r.store.documents().is_empty());
        r.merge(
            "launch",
            "calibration:KG50",
            gs.store.document("calibration:KG50").unwrap(),
            &p,
        )
        .unwrap();
        let pending = r.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].expected_local, json!(1));
        r.applied(&pending[0]).unwrap();
        assert!(r.pending().is_empty());
        assert!(!r.observe(&snapshot(12), &p).unwrap());
    }
    #[test]
    fn local_edit_after_remote_import_before_apply_becomes_conflict() {
        let p = peers();
        let mut gs = Coordinator::new("launch".into(), "gs".into(), snapshot(1), true, &p).unwrap();
        let mut r =
            Coordinator::new("launch".into(), "rrls".into(), snapshot(1), false, &p).unwrap();
        r.merge(
            "launch",
            "calibration:KG50",
            gs.store.document("calibration:KG50").unwrap(),
            &p,
        )
        .unwrap();
        gs.observe(&snapshot(2), &p).unwrap();
        r.merge(
            "launch",
            "calibration:KG50",
            gs.store.document("calibration:KG50").unwrap(),
            &p,
        )
        .unwrap();
        r.observe(&snapshot(3), &p).unwrap();
        assert_eq!(r.store.document("calibration:KG50").unwrap().heads.len(), 2);
        assert!(r.pending().is_empty());
    }
    #[test]
    fn restart_between_application_and_checkpoint_does_not_create_a_new_edit() {
        let p = peers();
        let gs = Coordinator::new("launch".into(), "gs".into(), snapshot(12), true, &p).unwrap();
        let mut r =
            Coordinator::new("launch".into(), "rrls".into(), snapshot(1), false, &p).unwrap();
        r.merge(
            "launch",
            "calibration:KG50",
            gs.store.document("calibration:KG50").unwrap(),
            &p,
        )
        .unwrap();
        let before = r.store.clone();
        // Application/file now contains12, but the observed checkpoint still says1.
        assert!(r.observe(&snapshot(12), &p).unwrap());
        assert_eq!(r.store, before);
        assert!(r.pending().is_empty());
        let path =
            std::env::temp_dir().join(format!("mission-coordinator-{}.json", std::process::id()));
        r.save(&path).unwrap();
        let loaded = Coordinator::load(&path, "launch", "rrls", &p).unwrap();
        assert!(loaded.pending().is_empty());
        assert_eq!(loaded.store, r.store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn imported_value_equal_to_default_still_advances_edit_context() {
        let p = peers();
        let gs = Coordinator::new("launch".into(), "gs".into(), snapshot(1), true, &p).unwrap();
        let mut r =
            Coordinator::new("launch".into(), "rrls".into(), snapshot(1), false, &p).unwrap();
        r.merge(
            "launch",
            "calibration:KG50",
            gs.store.document("calibration:KG50").unwrap(),
            &p,
        )
        .unwrap();
        assert!(r.observe(&snapshot(1), &p).unwrap());
        r.observe(&snapshot(3), &p).unwrap();
        assert_eq!(r.store.document("calibration:KG50").unwrap().heads.len(), 1);
        assert_eq!(
            r.store.document("calibration:KG50").unwrap().heads[0]
                .clock
                .get("gs"),
            Some(&1)
        );
    }
    #[test]
    fn delayed_apply_cannot_acknowledge_a_newer_revision() {
        let p = peers();
        let mut gs = Coordinator::new("launch".into(), "gs".into(), snapshot(1), true, &p).unwrap();
        let mut r =
            Coordinator::new("launch".into(), "rrls".into(), snapshot(0), false, &p).unwrap();
        r.merge(
            "launch",
            "calibration:KG50",
            gs.store.document("calibration:KG50").unwrap(),
            &p,
        )
        .unwrap();
        let pending = r.pending().remove(0);
        gs.observe(&snapshot(2), &p).unwrap();
        r.merge(
            "launch",
            "calibration:KG50",
            gs.store.document("calibration:KG50").unwrap(),
            &p,
        )
        .unwrap();
        assert!(r.applied(&pending).is_err());
        assert_eq!(r.pending()[0].revision.value, json!(2));
    }
}
