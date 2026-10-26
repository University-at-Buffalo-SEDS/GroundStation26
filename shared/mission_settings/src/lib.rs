//! Bounded, causal mission-setting history. Concurrent edits remain explicit heads.
//! Transport authentication and typed runtime validation belong at the boundary.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

type Clock = BTreeMap<String, u64>;
const MAX_DOCUMENTS: usize = 256;
const MAX_HEADS: usize = 9;
const MAX_DOCUMENT_BYTES: usize = 65_536;
const MAX_STORE_BYTES: usize = 2 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Revision {
    pub clock: Clock,
    pub value: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Document {
    pub heads: Vec<Revision>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Store {
    version: u8,
    installation: String,
    documents: BTreeMap<String, Document>,
}
fn identity(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}
fn dominates(a: &Clock, b: &Clock) -> bool {
    b.iter()
        .all(|(node, n)| a.get(node).copied().unwrap_or(0) >= *n)
}
fn validate_document(key: &str, doc: &Document, peers: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !key.is_empty()
            && key.len() <= 128
            && doc.heads.len() <= MAX_HEADS
            && !doc.heads.is_empty(),
        "invalid mission document bounds"
    );
    anyhow::ensure!(
        serde_json::to_vec(doc)?.len() <= MAX_DOCUMENT_BYTES,
        "mission document exceeds budget"
    );
    for (i, head) in doc.heads.iter().enumerate() {
        anyhow::ensure!(
            !head.clock.is_empty() && head.clock.len() <= MAX_HEADS,
            "invalid mission revision clock"
        );
        anyhow::ensure!(
            head.clock
                .iter()
                .all(|(id, n)| *n > 0 && peers.contains(id)),
            "unpaired mission revision"
        );
        for other in &doc.heads[..i] {
            anyhow::ensure!(
                !dominates(&head.clock, &other.clock) && !dominates(&other.clock, &head.clock),
                "noncanonical mission heads"
            );
        }
    }
    Ok(())
}
impl Store {
    pub fn new(installation: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            identity(&installation),
            "invalid mission installation identity"
        );
        Ok(Self {
            version: 1,
            installation,
            documents: BTreeMap::new(),
        })
    }
    pub fn installation(&self) -> &str {
        &self.installation
    }
    pub fn documents(&self) -> &BTreeMap<String, Document> {
        &self.documents
    }
    pub fn document(&self, key: &str) -> Option<&Document> {
        self.documents.get(key)
    }
    pub fn load(path: &Path, installation: &str, peers: &[String]) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)?;
        anyhow::ensure!(
            file.metadata()?.len() <= MAX_STORE_BYTES as u64,
            "mission store exceeds budget"
        );
        use std::io::Read;
        let mut bytes = Vec::new();
        file.take(MAX_STORE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= MAX_STORE_BYTES,
            "mission store exceeds budget"
        );
        let store: Self = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            store.version == 1
                && store.installation == installation
                && store.documents.len() <= MAX_DOCUMENTS,
            "mission store identity/version mismatch"
        );
        for (key, doc) in &store.documents {
            validate_document(key, doc, peers)?;
        }
        Ok(store)
    }
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        use std::io::Write;
        let bytes = serde_json::to_vec(self)?;
        anyhow::ensure!(
            bytes.len() <= MAX_STORE_BYTES,
            "mission store exceeds budget"
        );
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let temp = path.with_extension("settings-pending");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
    /// Import authenticated heads, preserving concurrency and rejecting equivocation.
    /// The candidate is fully validated before changing any current document.
    pub fn merge(
        &mut self,
        installation: &str,
        key: &str,
        incoming: &Document,
        peers: &[String],
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            self.installation == installation,
            "unpaired mission installation"
        );
        validate_document(key, incoming, peers)?;
        let old = self.documents.get(key);
        anyhow::ensure!(
            old.is_some() || self.documents.len() < MAX_DOCUMENTS,
            "mission catalog full"
        );
        let mut heads = old.map(|d| d.heads.clone()).unwrap_or_default();
        for revision in &incoming.heads {
            for h in &heads {
                anyhow::ensure!(
                    h.clock != revision.clock || h.value == revision.value,
                    "mission revision equivocation"
                );
            }
            if heads.iter().any(|h| dominates(&h.clock, &revision.clock)) {
                continue;
            }
            heads.retain(|h| !dominates(&revision.clock, &h.clock));
            heads.push(revision.clone());
        }
        heads.sort_by(|a, b| a.clock.cmp(&b.clock));
        let merged = Document { heads };
        validate_document(key, &merged, peers)?;
        if old == Some(&merged) {
            return Ok(false);
        }
        // Enforce the aggregate bound before committing to memory as well as disk.
        let mut next = self.clone();
        next.documents.insert(key.into(), merged);
        anyhow::ensure!(
            serde_json::to_vec(&next)?.len() <= MAX_STORE_BYTES,
            "mission store exceeds budget"
        );
        *self = next;
        Ok(true)
    }
    /// The UI supplies exactly the heads it reviewed. An intervening edit cannot
    /// be overwritten by a stale conflict-resolution click.
    pub fn resolve(
        &mut self,
        key: &str,
        expected: &Document,
        value: Value,
        station: &str,
        peers: &[String],
    ) -> anyhow::Result<Document> {
        anyhow::ensure!(
            self.documents.get(key) == Some(expected),
            "mission setting changed; review its current versions"
        );
        let mut clock = Clock::new();
        for head in &expected.heads {
            for (id, n) in &head.clock {
                let slot = clock.entry(id.clone()).or_default();
                *slot = (*slot).max(*n);
            }
        }
        let n = clock
            .get(station)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("mission revision exhausted"))?;
        anyhow::ensure!(
            peers.iter().any(|p| p == station),
            "unpaired editing station"
        );
        clock.insert(station.into(), n);
        let doc = Document {
            heads: vec![Revision { clock, value }],
        };
        self.merge(&self.installation.clone(), key, &doc, peers)?;
        Ok(doc)
    }
    /// New local edits use only their observed context. They must not implicitly
    /// resolve a concurrent version that the operator has never reviewed.
    pub fn edit(
        &mut self,
        key: &str,
        observed: Option<&Revision>,
        value: Value,
        station: &str,
        peers: &[String],
    ) -> anyhow::Result<Document> {
        anyhow::ensure!(
            peers.iter().any(|p| p == station),
            "unpaired editing station"
        );
        if let Some(base) = observed {
            validate_document(
                key,
                &Document {
                    heads: vec![base.clone()],
                },
                peers,
            )?;
            anyhow::ensure!(
                self.documents
                    .get(key)
                    .is_some_and(|d| d.heads.iter().any(|head| head == base
                        || (head.clock != base.clock && dominates(&head.clock, &base.clock)))),
                "mission edit context is unknown or ahead of its history"
            );
            if base.value == value {
                return Ok(self.documents[key].clone());
            }
        } else {
            anyhow::ensure!(
                self.documents
                    .get(key)
                    .is_none_or(|d| d.heads.iter().all(|h| !h.clock.contains_key(station))),
                "mission edit requires its persisted observed revision"
            );
        }
        let mut clock = observed.map(|h| h.clock.clone()).unwrap_or_default();
        let own = self
            .documents
            .get(key)
            .into_iter()
            .flat_map(|d| &d.heads)
            .filter_map(|h| h.clock.get(station))
            .copied()
            .max()
            .unwrap_or(0);
        clock.insert(
            station.into(),
            own.checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("mission revision exhausted"))?,
        );
        let incoming = Document {
            heads: vec![Revision { clock, value }],
        };
        self.merge(&self.installation.clone(), key, &incoming, peers)?;
        Ok(self.documents[key].clone())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn peers() -> Vec<String> {
        vec!["gs".into(), "rrls".into()]
    }
    fn stores() -> (Store, Store) {
        (
            Store::new("launch".into()).unwrap(),
            Store::new("launch".into()).unwrap(),
        )
    }
    #[test]
    fn offline_independent_channels_merge_and_replay_is_idempotent() {
        let (mut gs, mut r) = stores();
        let p = peers();
        let kg = gs
            .edit("calibration:KG50", None, json!({"slope":12}), "gs", &p)
            .unwrap();
        let pt = r
            .edit("calibration:PT", None, json!({"offset":3}), "rrls", &p)
            .unwrap();
        assert!(gs.merge("launch", "calibration:PT", &pt, &p).unwrap());
        assert!(r.merge("launch", "calibration:KG50", &kg, &p).unwrap());
        assert_eq!(gs, r);
        assert!(!gs.merge("launch", "calibration:PT", &pt, &p).unwrap());
    }
    #[test]
    fn concurrent_same_channel_requires_review_and_survives_reconnect() {
        let (mut gs, mut r) = stores();
        let p = peers();
        let initial = gs
            .edit("calibration:KG50", None, json!(1), "gs", &p)
            .unwrap();
        r.merge("launch", "calibration:KG50", &initial, &p).unwrap();
        let a = gs
            .edit(
                "calibration:KG50",
                Some(&initial.heads[0]),
                json!(2),
                "gs",
                &p,
            )
            .unwrap();
        let b = r
            .edit(
                "calibration:KG50",
                Some(&initial.heads[0]),
                json!(3),
                "rrls",
                &p,
            )
            .unwrap();
        gs.merge("launch", "calibration:KG50", &b, &p).unwrap();
        r.merge("launch", "calibration:KG50", &a, &p).unwrap();
        assert_eq!(gs, r);
        let conflict = gs.document("calibration:KG50").unwrap().clone();
        assert_eq!(conflict.heads.len(), 2);
        let resolved = gs
            .resolve("calibration:KG50", &conflict, json!(3), "gs", &p)
            .unwrap();
        r.merge("launch", "calibration:KG50", &resolved, &p)
            .unwrap();
        r.merge("launch", "calibration:KG50", &b, &p).unwrap();
        assert_eq!(gs, r);
        assert!(
            gs.resolve("calibration:KG50", &conflict, json!(2), "gs", &p)
                .is_err()
        );
    }
    #[test]
    fn local_edit_does_not_silently_resolve_unseen_remote_change() {
        let (mut a, mut b) = stores();
        let p = peers();
        let x = a.edit("target:nitrous", None, json!(8), "gs", &p).unwrap();
        b.merge("launch", "target:nitrous", &x, &p).unwrap();
        let y = b
            .edit("target:nitrous", Some(&x.heads[0]), json!(9), "rrls", &p)
            .unwrap();
        a.merge("launch", "target:nitrous", &y, &p).unwrap();
        let branch = a
            .edit("target:nitrous", Some(&x.heads[0]), json!(10), "gs", &p)
            .unwrap();
        assert_eq!(branch.heads.len(), 2);
        assert!(branch.heads.iter().any(|h| h.value == json!(9)));
        assert!(branch.heads.iter().any(|h| h.value == json!(10)));
        // This is an offline branch, not resolution of the newly received value.
        b.merge("launch", "target:nitrous", &branch, &p).unwrap();
        assert_eq!(a, b);
    }
    #[test]
    fn first_local_edit_can_race_the_first_remote_import_without_being_lost() {
        let (mut a, mut b) = stores();
        let p = peers();
        let x = a.edit("calibration:PT", None, json!(1), "gs", &p).unwrap();
        b.merge("launch", "calibration:PT", &x, &p).unwrap();
        let y = b
            .edit("calibration:PT", None, json!(2), "rrls", &p)
            .unwrap();
        assert_eq!(y.heads.len(), 2);
        a.merge("launch", "calibration:PT", &y, &p).unwrap();
        assert_eq!(a, b);
        assert!(a.edit("calibration:PT", None, json!(3), "gs", &p).is_err());
        let mut future = x.heads[0].clone();
        future.clock.insert("gs".into(), 100);
        assert!(
            a.edit("calibration:PT", Some(&future), json!(3), "gs", &p)
                .is_err()
        );
    }
    #[test]
    fn wrong_installation_peer_equivocation_and_oversize_leave_store_unchanged() {
        let (mut a, _) = stores();
        let p = peers();
        let x = a.edit("target:nitrogen", None, json!(8), "gs", &p).unwrap();
        let before = a.clone();
        assert!(a.merge("other", "target:nitrogen", &x, &p).is_err());
        let mut bad = x.clone();
        bad.heads[0].value = json!(99);
        assert!(a.merge("launch", "target:nitrogen", &bad, &p).is_err());
        bad.heads[0].clock = BTreeMap::from([("stranger".into(), 1)]);
        assert!(a.merge("launch", "target:nitrogen", &bad, &p).is_err());
        bad.heads[0].clock = BTreeMap::from([("gs".into(), 2)]);
        bad.heads[0].value = json!("x".repeat(MAX_DOCUMENT_BYTES));
        assert!(a.merge("launch", "target:nitrogen", &bad, &p).is_err());
        assert_eq!(a, before);
    }
    #[test]
    fn causal_history_and_conflicts_persist_across_service_restarts() {
        let (mut a, mut b) = stores();
        let p = peers();
        let x = a.edit("gse:limits", None, json!(20), "gs", &p).unwrap();
        let y = b.edit("gse:limits", None, json!(25), "rrls", &p).unwrap();
        a.merge("launch", "gse:limits", &y, &p).unwrap();
        a.merge("launch", "gse:limits", &x, &p).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "paired-setting-core-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("settings.json");
        a.save(&path).unwrap();
        assert_eq!(Store::load(&path, "launch", &p).unwrap(), a);
        assert!(Store::load(&path, "other", &p).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

pub mod transport;

pub mod auth;

pub mod coordinator;
