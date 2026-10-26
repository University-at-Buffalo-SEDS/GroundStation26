//! Low-rate anti-entropy for authenticated peers. The caller signs each message,
//! verifies its installation/issuer, and validates typed values before merging.
use crate::{Document, Store};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
const CHUNK: usize = 512;
const MAX_CHUNKS: usize = 128;
const MAX_TRANSFERS: usize = 4;
const MAX_ASSEMBLIES: usize = 3;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    Inventory {
        root: String,
    },
    Catalog {
        key: String,
        hash: String,
    },
    Request {
        key: String,
        hash: String,
    },
    Chunk {
        key: String,
        hash: String,
        index: u16,
        total: u16,
        bytes: String,
    },
}
fn hash(bytes: &[u8]) -> String {
    B64.encode(digest(&SHA256, bytes).as_ref())
}
fn valid_hash(s: &str) -> bool {
    s.len() == 43 && B64.decode(s).is_ok_and(|b| b.len() == 32)
}
fn key_ok(key: &str) -> bool {
    !key.is_empty() && key.len() <= 128
}
fn canonical_bytes<T: Serialize>(value: &T) -> Vec<u8> {
    fn sort(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                map.sort_keys();
                for value in map.values_mut() {
                    sort(value);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    sort(item);
                }
            }
            _ => {}
        }
    }
    let mut value = serde_json::to_value(value).expect("JSON mission serialization");
    sort(&mut value);
    serde_json::to_vec(&value).expect("JSON mission serialization")
}
fn document_bytes(doc: &Document) -> Vec<u8> {
    canonical_bytes(doc)
}
fn document_hash(doc: &Document) -> String {
    hash(&document_bytes(doc))
}
fn root(store: &Store) -> String {
    hash(&canonical_bytes(store.documents()))
}
struct Transfer {
    key: String,
    hash: String,
    bytes: Vec<u8>,
    index: usize,
}
struct Assembly {
    key: String,
    hash: String,
    parts: Vec<Option<Vec<u8>>>,
    at: u64,
}
#[derive(Default)]
pub struct Protocol {
    last_inventory: Option<u64>,
    requests: VecDeque<(String, String)>,
    transfers: VecDeque<Transfer>,
    assemblies: Vec<Assembly>,
    catalog: Option<(String, VecDeque<(String, String)>)>,
    turn: usize,
    last_sent: Option<u64>,
}
impl Protocol {
    /// One message per500ms per configured peer. Runtime additionally shares a
    /// single radio allowance across peers and puts settings below GPS/commands.
    pub fn poll(&mut self, store: &Store, now: u64) -> Option<Message> {
        if self.last_sent.is_some_and(|n| now.saturating_sub(n) < 500) {
            return None;
        }
        let message = if self
            .last_inventory
            .is_none_or(|n| now.saturating_sub(n) >= 5000)
        {
            self.last_inventory = Some(now);
            Some(Message::Inventory { root: root(store) })
        } else {
            let mut result = None;
            for _ in 0..3 {
                let class = self.turn % 3;
                self.turn += 1;
                result = match class {
                    0 => self
                        .requests
                        .pop_front()
                        .map(|(key, hash)| Message::Request { key, hash }),
                    1 => self
                        .catalog
                        .as_mut()
                        .and_then(|(_, entries)| entries.pop_front())
                        .map(|(key, hash)| Message::Catalog { key, hash }),
                    _ => self.transfers.pop_front().map(|mut transfer| {
                        let total = transfer.bytes.len().div_ceil(CHUNK);
                        let index = transfer.index;
                        let start = index * CHUNK;
                        let message = Message::Chunk {
                            key: transfer.key.clone(),
                            hash: transfer.hash.clone(),
                            index: index as u16,
                            total: total as u16,
                            bytes: B64.encode(
                                &transfer.bytes[start..(start + CHUNK).min(transfer.bytes.len())],
                            ),
                        };
                        transfer.index += 1;
                        if transfer.index < total {
                            self.transfers.push_back(transfer);
                        }
                        message
                    }),
                };
                if result.is_some() {
                    break;
                }
            }
            result
        };
        if message.is_some() {
            self.last_sent = Some(now);
        }
        message
    }
    /// Returns a completed candidate, without applying it. Only authenticated
    /// messages may enter this method; typed validation and persistence precede
    /// the backend's call to Store::merge.
    pub fn receive(
        &mut self,
        store: &Store,
        message: Message,
        now: u64,
    ) -> anyhow::Result<Option<(String, Document)>> {
        self.assemblies
            .retain(|a| now.saturating_sub(a.at) <= 60_000);
        match message {
            Message::Inventory { root: remote } => {
                anyhow::ensure!(valid_hash(&remote), "invalid mission inventory");
                let local = root(store);
                if local != remote
                    && self
                        .catalog
                        .as_ref()
                        .is_none_or(|(h, entries)| h != &local || entries.is_empty())
                {
                    let entries = store
                        .documents()
                        .iter()
                        .map(|(k, d)| (k.clone(), document_hash(d)))
                        .collect();
                    self.catalog = Some((local, entries));
                }
            }
            Message::Catalog { key, hash } => {
                anyhow::ensure!(
                    key_ok(&key) && valid_hash(&hash),
                    "invalid mission catalog entry"
                );
                if store
                    .document(&key)
                    .is_none_or(|d| document_hash(d) != hash)
                    && !self.requests.iter().any(|p| p.0 == key && p.1 == hash)
                {
                    anyhow::ensure!(
                        self.requests.len() < 256,
                        "mission request budget exhausted"
                    );
                    self.requests.push_back((key, hash));
                }
            }
            Message::Request { key, hash } => {
                anyhow::ensure!(key_ok(&key) && valid_hash(&hash), "invalid mission request");
                if let Some(doc) = store.document(&key)
                    && document_hash(doc) == hash
                    && !self
                        .transfers
                        .iter()
                        .any(|t| t.key == key && t.hash == hash)
                {
                    anyhow::ensure!(
                        self.transfers.len() < MAX_TRANSFERS,
                        "mission transfer budget exhausted"
                    );
                    let bytes = document_bytes(doc);
                    anyhow::ensure!(
                        bytes.len() <= CHUNK * MAX_CHUNKS,
                        "mission document exceeds transport budget"
                    );
                    self.transfers.push_back(Transfer {
                        key,
                        hash,
                        bytes,
                        index: 0,
                    });
                }
            }
            Message::Chunk {
                key,
                hash: expected,
                index,
                total,
                bytes,
            } => {
                anyhow::ensure!(
                    key_ok(&key)
                        && valid_hash(&expected)
                        && total > 0
                        && usize::from(total) <= MAX_CHUNKS
                        && index < total
                        && bytes.len() <= CHUNK.div_ceil(3) * 4,
                    "invalid mission chunk bounds"
                );
                let bytes = B64.decode(bytes)?;
                anyhow::ensure!(
                    !bytes.is_empty()
                        && bytes.len() <= CHUNK
                        && (index + 1 == total || bytes.len() == CHUNK),
                    "invalid mission chunk length"
                );
                if store
                    .document(&key)
                    .is_some_and(|d| document_hash(d) == expected)
                {
                    return Ok(None);
                }
                let i = match self
                    .assemblies
                    .iter()
                    .position(|a| a.key == key && a.hash == expected)
                {
                    Some(i) => i,
                    None => {
                        anyhow::ensure!(
                            self.assemblies.len() < MAX_ASSEMBLIES,
                            "mission assembly budget exhausted"
                        );
                        self.assemblies.push(Assembly {
                            key: key.clone(),
                            hash: expected.clone(),
                            parts: vec![None; usize::from(total)],
                            at: now,
                        });
                        self.assemblies.len() - 1
                    }
                };
                let assembly = &mut self.assemblies[i];
                anyhow::ensure!(
                    assembly.parts.len() == usize::from(total),
                    "mission chunk count changed"
                );
                if let Some(old) = &assembly.parts[usize::from(index)] {
                    anyhow::ensure!(old == &bytes, "mission chunk equivocation");
                }
                assembly.parts[usize::from(index)] = Some(bytes);
                assembly.at = now;
                if assembly.parts.iter().all(Option::is_some) {
                    let completed = self.assemblies.remove(i);
                    let bytes: Vec<u8> = completed.parts.into_iter().flatten().flatten().collect();
                    anyhow::ensure!(
                        hash(&bytes) == expected,
                        "mission document checksum mismatch"
                    );
                    let doc = serde_json::from_slice(&bytes)?;
                    return Ok(Some((key, doc)));
                }
            }
        }
        Ok(None)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn peers() -> Vec<String> {
        vec!["gs".into(), "rrls".into()]
    }
    #[test]
    fn dropped_reordered_and_replayed_chunks_recover_without_losing_offline_conflicts() {
        let peers = peers();
        let mut a = Store::new("launch".into()).unwrap();
        let mut b = a.clone();
        a.edit(
            "calibration:KG50",
            None,
            json!({"slope":1,"points":"a".repeat(4000)}),
            "gs",
            &peers,
        )
        .unwrap();
        b.edit(
            "calibration:KG50",
            None,
            json!({"slope":2,"points":"b".repeat(4000)}),
            "rrls",
            &peers,
        )
        .unwrap();
        b.edit("calibration:PT", None, json!({"offset":3}), "rrls", &peers)
            .unwrap();
        let mut pa = Protocol::default();
        let mut pb = Protocol::default();
        let mut drops = 0;
        for step in 0..4000u64 {
            let now = step * 500;
            if let Some(m) = pa.poll(&a, now) {
                if matches!(&m, Message::Chunk { index: 2, .. }) && drops == 0 {
                    drops += 1;
                } else {
                    if let Some((key, doc)) = pb.receive(&b, m.clone(), now).unwrap() {
                        b.merge("launch", &key, &doc, &peers).unwrap();
                    }
                    // Authenticated duplicate must not create a second revision.
                    if let Some((key, doc)) = pb.receive(&b, m, now).unwrap() {
                        b.merge("launch", &key, &doc, &peers).unwrap();
                    }
                }
            }
            if let Some(m) = pb.poll(&b, now)
                && let Some((key, doc)) = pa.receive(&a, m, now).unwrap()
            {
                a.merge("launch", &key, &doc, &peers).unwrap();
            }
            if a == b {
                assert_eq!(drops, 1);
                assert_eq!(a.document("calibration:KG50").unwrap().heads.len(), 2);
                return;
            }
        }
        panic!("bounded anti-entropy did not converge after a lost chunk");
    }
    #[test]
    fn reordered_chunks_preserve_exact_document_and_expire_bounded_incomplete_work() {
        let peers = peers();
        let mut a = Store::new("launch".into()).unwrap();
        let doc = a
            .edit(
                "calibration:KG50",
                None,
                json!({"points":"x".repeat(1200)}),
                "gs",
                &peers,
            )
            .unwrap();
        let bytes = document_bytes(&doc);
        let expected = hash(&bytes);
        let parts = bytes.chunks(CHUNK).collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);
        let empty = Store::new("launch".into()).unwrap();
        let mut p = Protocol::default();
        let message = |i: usize| Message::Chunk {
            key: "calibration:KG50".into(),
            hash: expected.clone(),
            index: i as u16,
            total: parts.len() as u16,
            bytes: B64.encode(parts[i]),
        };
        assert!(p.receive(&empty, message(2), 0).unwrap().is_none());
        assert!(p.receive(&empty, message(0), 500).unwrap().is_none());
        assert!(p.receive(&empty, message(0), 1000).unwrap().is_none());
        assert_eq!(
            p.receive(&empty, message(1), 1500).unwrap(),
            Some(("calibration:KG50".into(), doc))
        );
        assert!(p.assemblies.is_empty());
        assert!(p.receive(&empty, message(0), 2000).unwrap().is_none());
        p.receive(&empty, Message::Inventory { root: root(&empty) }, 62_001)
            .unwrap();
        assert!(p.assemblies.is_empty());
    }
    #[test]
    fn catalog_hashes_use_canonical_nested_object_order() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"z":{"b":1,"a":2},"a":3}"#).unwrap();
        assert_eq!(
            String::from_utf8(canonical_bytes(&value)).unwrap(),
            r#"{"a":3,"z":{"a":2,"b":1}}"#
        );
    }
    #[test]
    fn stable_stations_only_send_small_inventory_and_rate_is_bounded() {
        let a = Store::new("launch".into()).unwrap();
        let mut p = Protocol::default();
        for step in 0..100u64 {
            let now = step * 500;
            if let Some(m) = p.poll(&a, now) {
                assert!(matches!(&m, Message::Inventory { .. }));
                assert!(serde_json::to_vec(&m).unwrap().len() < 100);
                p.receive(&a, m, now).unwrap();
            }
            assert!(p.poll(&a, now).is_none());
        }
    }
    #[test]
    fn oversized_and_inconsistent_chunks_are_rejected_before_unbounded_allocation() {
        let a = Store::new("launch".into()).unwrap();
        let mut p = Protocol::default();
        let h = hash(b"fixture");
        for (index, total, bytes) in [
            (0, 129, B64.encode([1; 512])),
            (1, 1, B64.encode([1; 512])),
            (0, 2, B64.encode([1; 511])),
            (0, 1, "x".repeat(1000)),
        ] {
            assert!(
                p.receive(
                    &a,
                    Message::Chunk {
                        key: "calibration:PT".into(),
                        hash: h.clone(),
                        index,
                        total,
                        bytes
                    },
                    0
                )
                .is_err()
            );
        }
        assert!(p.assemblies.is_empty());
    }
}
