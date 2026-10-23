//! Exercise signed settings reconciliation through the shared radio allowance.
use paired_settings::{Store, radio::RadioSync};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::json;

#[test]
fn paced_signed_sync_recovers_from_backpressure_loss_reorder_and_runtime_restart() {
    let peers = vec!["gs".into(), "rrls".into()];
    let signer = Ed25519KeyPair::from_seed_unchecked(&[39; 32]).unwrap();
    let mut stores = [
        Store::new("launch".into()).unwrap(),
        Store::new("launch".into()).unwrap(),
    ];
    stores[0]
        .edit(
            "calibration/kg50",
            None,
            json!({"gain": 12, "offset": 3}),
            "gs",
            &peers,
        )
        .unwrap();
    stores[1]
        .edit(
            "calibration/kg1000",
            None,
            json!({"gain": 41, "offset": 7}),
            "rrls",
            &peers,
        )
        .unwrap();
    let fresh = || {
        [
            RadioSync::new("launch".into(), "gs".into(), vec!["rrls".into()]).unwrap(),
            RadioSync::new("launch".into(), "rrls".into(), vec!["gs".into()]).unwrap(),
        ]
    };
    let mut radios = fresh();
    let mut attempts = [0; 2];
    let mut bytes = [0; 2];
    let mut delayed = Vec::new();
    let mut failures = 0;
    let mut loss = 0;
    for tick in 0..7200 {
        let now = tick * 50;
        if tick == 300 {
            // Volatile transport state can vanish; persisted revision history
            // must converge again without copying another station's Store.
            radios = fresh();
            delayed.clear();
        }
        for direction in 0..2 {
            radios[direction]
                .poll(&stores[direction], now, &signer, |_, wire| {
                    attempts[direction] += 1;
                    if attempts[direction] % 7 == 0 {
                        failures += 1;
                        return false;
                    }
                    bytes[direction] += wire.len();
                    if attempts[direction] % 11 == 0 {
                        loss += 1;
                        return true;
                    }
                    delayed.push((1 - direction, wire.to_vec()));
                    true
                })
                .unwrap();
        }
        if tick % 17 == 0 {
            for (to, wire) in delayed.drain(..).rev() {
                if let Some((name, document)) = radios[to]
                    .receive(&stores[to], &wire, now, signer.public_key().as_ref())
                    .unwrap()
                {
                    stores[to]
                        .merge("launch", &name, &document, &peers)
                        .unwrap();
                }
                // Replay does not manufacture a new revision or duplicate value.
                if let Some((name, document)) = radios[to]
                    .receive(&stores[to], &wire, now, signer.public_key().as_ref())
                    .unwrap()
                {
                    stores[to]
                        .merge("launch", &name, &document, &peers)
                        .unwrap();
                }
            }
        }
    }
    assert!(failures > 0 && loss > 0);
    assert_eq!(stores[0], stores[1]);
    assert_eq!(stores[0].documents().len(), 2);
    for count in bytes {
        // One initial frame per process lifetime is the only permitted burst.
        assert!(count <= 360 * 256 + 2 * 2117);
    }
}
