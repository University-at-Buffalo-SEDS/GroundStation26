//! Exercise signed settings reconciliation through the shared radio allowance.
use paired_settings::{Store, radio::RadioSync};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::json;

#[test]
fn complete_mission_catalog_converges_with_bounded_assembly_slots() {
    let peers = vec!["gs".into(), "rrls".into()];
    let signer = Ed25519KeyPair::from_seed_unchecked(&[47; 32]).unwrap();
    let mut stores = [
        Store::new("launch".into()).unwrap(),
        Store::new("launch".into()).unwrap(),
    ];
    let names = [
        "calibration/ch1",
        "calibration/full_mass_kg",
        "calibration/iadc",
        "calibration/kg50",
        "mission/auto_zero_preference",
        "mission/fill_targets",
        "mission/gse_limits",
        "mission/saved_flight_setup",
        "mission/saved_station_positions",
    ];
    for (index, name) in names.iter().enumerate() {
        stores[0]
            .edit(
                name,
                None,
                json!({"value": "x".repeat(if index == 7 { 1500 } else { 200 })}),
                "gs",
                &peers,
            )
            .unwrap();
    }
    let mut radios = [
        RadioSync::new("launch".into(), "gs".into(), vec!["rrls".into()]).unwrap(),
        RadioSync::new("launch".into(), "rrls".into(), vec!["gs".into()]).unwrap(),
    ];
    let mut deferred = Vec::new();
    let mut charged_bytes = [0_u64; 2];
    let mut last_frame: [Option<(u64, u64)>; 2] = [None; 2];
    for tick in 0..1200 {
        let now = tick * 500;
        for direction in 0..2 {
            let mut offered = None;
            radios[direction]
                .poll(&stores[direction], now, &signer, |_, wire| {
                    if let Some((previous_at, previous_bytes)) = last_frame[direction] {
                        assert!(now - previous_at >= (previous_bytes * 1000_u64).div_ceil(256));
                    }
                    last_frame[direction] = Some((now, wire.len() as u64));
                    charged_bytes[direction] += wire.len() as u64;
                    offered = Some(wire.to_vec());
                    true
                })
                .unwrap();
            if let Some(wire) = offered {
                let to = 1 - direction;
                match radios[to].receive(&stores[to], &wire, now, signer.public_key().as_ref()) {
                    Ok(Some((name, document))) => {
                        stores[to]
                            .merge("launch", &name, &document, &peers)
                            .unwrap();
                    }
                    Ok(None) => {}
                    Err(error) => {
                        // A complete catalog exceeds the four concurrent sender
                        // slots. Refusal must recover through anti-entropy,
                        // without enlarging queues or masking validation errors.
                        assert_eq!(error.to_string(), "mission transfer budget exhausted");
                        deferred.push(error.to_string());
                    }
                }
            }
        }
        if stores[0] == stores[1] {
            assert!(
                !deferred.is_empty(),
                "fixture must exercise bounded refusal"
            );
            for direction in 0..2 {
                assert!(
                    charged_bytes[direction] <= now * 256 / 1000 + last_frame[direction].unwrap().1
                );
            }
            return;
        }
    }
    panic!(
        "complete catalog failed to converge; received={:?}; deferred={deferred:?}",
        stores[1].documents().keys().collect::<Vec<_>>()
    );
}

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
