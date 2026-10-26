//! Exercise the signed, bounded protocol and persisted coordinator together.
//! Runtime typed adapters and real radio qualification are separate gates.
use mission_settings::{auth, coordinator::Coordinator, transport::Protocol};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};
use std::collections::BTreeMap;

struct Station {
    id: &'static str,
    coordinator: Coordinator,
    protocol: Protocol,
    live: BTreeMap<String, Value>,
}
fn peers() -> Vec<String> {
    vec!["gs".into(), "rrls".into()]
}
fn station(id: &'static str, primary: bool) -> Station {
    let live = BTreeMap::from([
        ("calibration:KG50".into(), json!({"slope": 12, "offset": 0})),
        (
            "calibration:KG1000".into(),
            json!({"slope": 31, "offset": 2}),
        ),
    ]);
    Station {
        id,
        coordinator: Coordinator::new("launch".into(), id.into(), live.clone(), primary, &peers())
            .unwrap(),
        protocol: Protocol::default(),
        live,
    }
}
fn receive(to: &mut Station, wire: &[u8], now: u64, key: &Ed25519KeyPair) {
    let verified = auth::open(wire, "launch", to.id, &peers(), key.public_key().as_ref()).unwrap();
    if let Some((name, document)) = to
        .protocol
        .receive(to.coordinator.store(), verified.message, now)
        .unwrap()
    {
        to.coordinator
            .merge("launch", &name, &document, &peers())
            .unwrap();
    }
}
fn exchange(a: &mut Station, b: &mut Station, key: &Ed25519KeyPair, base: u64) {
    // Deterministic loss, duplication and reordering; no direct Store copying.
    let mut delayed: Vec<(bool, Vec<u8>)> = Vec::new();
    for tick in 0..360 {
        let now = base + tick * 500;
        for direction in [true, false] {
            let (from, to) = if direction {
                (&mut *a, &mut *b)
            } else {
                (&mut *b, &mut *a)
            };
            if let Some(message) = from.protocol.poll(from.coordinator.store(), now) {
                let wire = auth::seal(key, "launch", from.id, to.id, message).unwrap();
                assert!(wire.len() <= 3072);
                if tick % 11 == 0 {
                    continue;
                }
                if tick % 7 == 0 {
                    delayed.push((direction, wire));
                } else {
                    receive(to, &wire, now, key);
                    if tick % 5 == 0 {
                        receive(to, &wire, now, key);
                    }
                }
            }
        }
        if tick % 3 == 0 {
            for (direction, wire) in delayed.drain(..).rev() {
                receive(if direction { &mut *b } else { &mut *a }, &wire, now, key);
            }
        }
    }
    assert_eq!(a.coordinator.store(), b.coordinator.store());
}
fn apply(station: &mut Station) {
    for pending in station.coordinator.pending() {
        assert_eq!(
            station.live.get(&pending.key),
            Some(&pending.expected_local)
        );
        // The production adapter must perform this CAS under its settings lock.
        station
            .live
            .insert(pending.key.clone(), pending.revision.value.clone());
        station.coordinator.applied(&pending).unwrap();
    }
}
#[test]
fn paired_offline_calibrations_survive_loss_restart_and_explicit_conflict_resolution() {
    let key = Ed25519KeyPair::from_seed_unchecked(&[37; 32]).unwrap();
    let mut gs = station("gs", true);
    let mut rrls = station("rrls", false);
    exchange(&mut gs, &mut rrls, &key, 0);
    apply(&mut rrls);
    rrls.coordinator.observe(&rrls.live, &peers()).unwrap();

    // Independent offline channel edits must both survive reconnect.
    gs.live
        .insert("calibration:KG50".into(), json!({"slope": 14, "offset": 1}));
    rrls.live.insert(
        "calibration:KG1000".into(),
        json!({"slope": 33, "offset": 3}),
    );
    gs.coordinator.observe(&gs.live, &peers()).unwrap();
    rrls.coordinator.observe(&rrls.live, &peers()).unwrap();
    exchange(&mut gs, &mut rrls, &key, 200_000);
    apply(&mut gs);
    apply(&mut rrls);
    assert_eq!(gs.live, rrls.live);
    assert_eq!(gs.live["calibration:KG50"]["slope"], 14);
    assert_eq!(gs.live["calibration:KG1000"]["slope"], 33);

    // Same-channel concurrent edits cannot overwrite one another silently.
    gs.live
        .insert("calibration:KG50".into(), json!({"slope": 15, "offset": 1}));
    rrls.live
        .insert("calibration:KG50".into(), json!({"slope": 16, "offset": 1}));
    gs.coordinator.observe(&gs.live, &peers()).unwrap();
    rrls.coordinator.observe(&rrls.live, &peers()).unwrap();
    exchange(&mut gs, &mut rrls, &key, 400_000);
    let reviewed = gs
        .coordinator
        .store()
        .document("calibration:KG50")
        .unwrap()
        .clone();
    assert_eq!(reviewed.heads.len(), 2);
    assert!(gs.coordinator.pending().is_empty());
    assert!(rrls.coordinator.pending().is_empty());
    let path = std::env::temp_dir().join(format!(
        "paired-mission-reconnect-{}.json",
        std::process::id()
    ));
    rrls.coordinator.save(&path).unwrap();
    rrls.coordinator = Coordinator::load(&path, "launch", "rrls", &peers()).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(
        rrls.coordinator
            .store()
            .document("calibration:KG50")
            .unwrap()
            .heads
            .len(),
        2
    );
    let chosen = reviewed
        .heads
        .iter()
        .find(|h| h.value["slope"] == 16)
        .unwrap()
        .value
        .clone();
    gs.coordinator
        .resolve("calibration:KG50", &reviewed, chosen, &peers())
        .unwrap();
    assert!(
        gs.coordinator
            .resolve("calibration:KG50", &reviewed, json!(0), &peers())
            .is_err()
    );
    exchange(&mut gs, &mut rrls, &key, 600_000);
    apply(&mut gs);
    apply(&mut rrls);
    assert_eq!(gs.live, rrls.live);
    assert_eq!(gs.live["calibration:KG50"]["slope"], 16);
    assert_eq!(gs.live["calibration:KG1000"]["slope"], 33);
}
