use super::*;
use budget_negotiation::Profile;
fn paired() -> (Mesh, Mesh) {
    let mut p = Mesh::new(Role::Primary, 7);
    let mut r = Mesh::new(Role::Rrls, 9);
    p.budget_test_clock = Some(0);
    r.budget_test_clock = Some(0);
    p.enable_budget(Profile::MATCHED);
    r.enable_budget(Profile::MATCHED);
    (p, r)
}
fn send(from: &mut Mesh, to: &mut Mesh, now: u64) -> Vec<u8> {
    from.budget_test_clock = Some(now);
    to.budget_test_clock = Some(now);
    let tx = from.begin_transmission(now).expect("actual Mesh owner");
    let bytes = tx.frame().bytes.clone();
    if Mesh::budget_kind(&bytes) {
        assert_eq!(tx.completed_ordinary_owner_bytes(), 0);
        assert_eq!(tx.ordinary_background_service_bytes(), 0);
        assert_eq!(tx.successful_ordinary_service_bytes(), 0);
        assert!(tx.single_control_contract().is_none());
    }
    from.finish_transmission(tx, true);
    to.incoming(true, &bytes, now);
    bytes
}
fn reply(query: &[u8]) -> Vec<u8> {
    assert_eq!(query.len(), 26);
    assert_eq!(query[3], 14);
    let mut b = vec![b'R', b'R', 1, 14, 1, 1];
    b.extend_from_slice(&query[6..22]);
    b.extend_from_slice(&115200u32.to_le_bytes());
    b.extend_from_slice(&32000u32.to_le_bytes());
    b.push(35);
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(&100u16.to_le_bytes());
    b.extend_from_slice(&55u16.to_le_bytes());
    b.push(0);
    b.extend_from_slice(&crc(&b).to_le_bytes());
    assert_eq!(b.len(), 42);
    b
}
fn healthy() -> (Mesh, Mesh) {
    let (mut p, mut r) = paired();
    p.direct(0);
    p.tick(0);
    assert_eq!(send(&mut p, &mut r, 0)[3], HEALTH);
    assert_eq!(send(&mut p, &mut r, 0)[3], BUDGET_CONTROL);
    assert!(!r.active(0));
    r.tick(0);
    assert!(
        r.queue.iter().any(|q| q.frame.bytes[3] == BUDGET_CONTROL),
        "inactiveclear retains BASE_ACK"
    );
    assert_eq!(send(&mut r, &mut p, 0)[3], BUDGET_CONTROL);
    assert_eq!(p.negotiated_air_share(0), Some(25));
    assert_eq!(p.recovery_air_share(0), None, "the primary always keeps its profile share");
    r.tick(0);
    (p, r)
}
#[test]
fn healthy_query_is_one_owned_frame_and_never_application_forwarding() {
    let (p, mut r) = healthy();
    let tx = r.begin_transmission(0).unwrap();
    let query = tx.frame().bytes.clone();
    assert_eq!(query[3], RF_PROFILE_CONTROL);
    assert!(r.budget_inflight);
    r.tick(0);
    assert!(
        r.queue.iter().all(|q| !Mesh::budget_kind(&q.frame.bytes)),
        "inflight blocks second protocol queue owner"
    );
    r.finish_transmission(tx, true);
    let health = r.health_received;
    let forwarded = r.relayed_frames;
    assert!(r.incoming(true, &reply(&query), 0).is_none());
    assert_eq!(r.health_received, health);
    assert_eq!(r.relayed_frames, forwarded);
    assert!(!r.active(0));
    for now in 1..20 {
        r.tick(now);
        assert!(
            r.queue.iter().all(|q| !Mesh::budget_kind(&q.frame.bytes)),
            "healthyprofileproof doesn't churn queue"
        );
    }
    assert_eq!(p.negotiated_air_share(19), Some(25));
    assert_eq!(r.recovery_air_share(19), Some(35));
}
#[test]
fn contextual_protocol_owner_survives_paid_650ms_and_no_early_loan() {
    let (mut p, mut r) = paired();
    p.direct(0);
    p.tick(0);
    send(&mut p, &mut r, 0);
    let frame = p
        .queue
        .iter()
        .find(|q| q.frame.bytes[3] == BUDGET_CONTROL)
        .unwrap();
    let original = frame.frame.bytes.clone();
    assert!(
        !p.queued_expired(frame, 650),
        "newprotocolowner outlives fullypaid374B ordinarydebt; originalapplicationTTLs unchanged"
    );
    for now in [100, 250, 400, 650] {
        p.budget_test_clock = Some(now);
        p.tick(now);
        assert_eq!(
            p.queue
                .iter()
                .filter(|q| Mesh::budget_kind(&q.frame.bytes))
                .count(),
            1
        );
    }
    let tx = p.begin_transmission(650).unwrap(); // May be periodic protected HEALTH.
    if tx.frame().bytes[3] == HEALTH {
        let bytes = tx.frame().bytes.clone();
        p.finish_transmission(tx, true);
        r.incoming(true, &bytes, 650);
    } else {
        p.finish_transmission(tx, false);
    }
    let tx = p.begin_transmission(650).unwrap();
    let budget_negotiation::Message::Revoke(before) =
        budget_negotiation::decode(&original).unwrap()
    else {
        panic!()
    };
    let budget_negotiation::Message::Revoke(after) =
        budget_negotiation::decode(&tx.frame().bytes).unwrap()
    else {
        panic!()
    };
    assert_eq!((after.session, after.nonce), (before.session, before.nonce));
    assert_eq!(
        after.seq, 1,
        "never-reserved owner now references fully written HEALTH1"
    );
    assert_eq!(tx.queued.at, 0);
    assert!(tx.queued.budget_ever_reserved());
    let frozen = tx.frame().bytes.clone();
    assert_eq!(tx.completed_ordinary_owner_bytes(), 0);
    p.finish_transmission(tx, false);
    assert_eq!(
        p.queue
            .iter()
            .find(|q| Mesh::budget_kind(&q.frame.bytes))
            .unwrap()
            .frame
            .bytes,
        frozen
    );
    assert_eq!(
        p.queue
            .iter()
            .filter(|q| Mesh::budget_kind(&q.frame.bytes))
            .count(),
        1
    );
}
#[test]
fn unknown_budget_frames_never_renew_presence_or_reach_sdk() {
    let (p, mut r) = paired();
    let before = (
        r.health_received,
        r.last_direct,
        r.relay_seen,
        r.queue.len(),
    );
    for kind in [BUDGET_CONTROL, RF_PROFILE_CONTROL] {
        assert!(
            r.incoming(true, &encode(kind, b"not-a-valid-contract"), 0)
                .is_none()
        );
    }
    assert_eq!(
        (
            r.health_received,
            r.last_direct,
            r.relay_seen,
            r.queue.len()
        ),
        before
    );
    assert_eq!(p.negotiated_air_share(0), Some(15));
    assert_eq!(r.recovery_air_share(0), Some(35));
}
#[test]
fn effective_capacity_mismatch_disables_context_without_silent_upgrade() {
    let (mut p, _) = paired();
    assert!(!p.budget_effective_transport(false));
    assert_eq!(p.negotiated_air_share(0), Some(15));
    assert!(p.budget_transport_fault);
    assert!(p.budget_effective_transport(true));
    assert!(!p.budget_transport_fault);
    assert_eq!(p.negotiated_air_share(0), Some(15));
}

#[test]
fn dropped_initial_health_does_not_pin_owned_revoke_forever() {
    let (mut p, mut r) = paired();
    p.direct(0);
    p.tick(0);
    let dropped = p.begin_transmission(0).unwrap();
    assert_eq!(dropped.frame().bytes[3], HEALTH);
    p.finish_transmission(dropped, true);
    let initial = p.begin_transmission(0).unwrap();
    let initial_bytes = initial.frame().bytes.clone();
    assert_eq!(initial_bytes[3], BUDGET_CONTROL);
    p.finish_transmission(initial, true);
    assert!(r.incoming(true, &initial_bytes, 0).is_none());
    let mut saw_refreshed = false;
    for now in (500..=5000).step_by(500) {
        p.budget_test_clock = Some(now);
        r.budget_test_clock = Some(now);
        p.direct(now);
        p.tick(now);
        for _ in 0..4 {
            let Some(tx) = p.begin_transmission(now) else {
                break;
            };
            let bytes = tx.frame().bytes.clone();
            p.finish_transmission(tx, true);
            if bytes[3] == BUDGET_CONTROL {
                let budget_negotiation::Message::Revoke(c) =
                    budget_negotiation::decode(&bytes).unwrap()
                else {
                    panic!()
                };
                assert!(c.seq > 0);
                saw_refreshed = true;
            }
            r.incoming(true, &bytes, now);
        }
    }
    assert!(saw_refreshed);
    r.tick(5000);
    let mut restored = false;
    for _ in 0..4 {
        let Some(tx) = r.begin_transmission(5000) else {
            break;
        };
        let bytes = tx.frame().bytes.clone();
        r.finish_transmission(tx, true);
        p.incoming(true, &bytes, 5000);
        if p.negotiated_air_share(5000) == Some(25) {
            restored = true;
            break;
        }
    }
    assert!(
        restored,
        "actual Mesh retry and delayed response restore25 after first HEALTH loss"
    );
    assert_eq!(r.recovery_air_share(5000), Some(35));
}

#[test]
fn negotiation_protected_selection_is_paid_and_has_no_control_or_fairness_credit() {
    let (mut p, mut r) = paired();
    p.direct(0);
    p.tick(0);
    send(&mut p, &mut r, 0);
    assert!(
        p.begin_primary_background_transmission(0, 0, false, false)
            .is_none(),
        "budget must never use early critical readiness"
    );
    let tx = p
        .begin_primary_background_transmission(0, 0, true, false)
        .expect("normal-ready protocol must not wait for SDK ordinary permission");
    assert_eq!(tx.frame().bytes[3], BUDGET_CONTROL);
    assert!(!tx.closes_packet_batch());
    assert!(tx.single_control_contract().is_none());
    assert!(tx.batch_contracts().is_none());
    assert_eq!(tx.ordinary_background_service_bytes(), 0);
    assert_eq!(tx.completed_ordinary_owner_bytes(), 0);
    assert_eq!(tx.successful_ordinary_service_bytes(), 0);
    p.finish_transmission(tx, false);
    assert_eq!(
        p.queue
            .iter()
            .filter(|q| Mesh::budget_kind(&q.frame.bytes))
            .count(),
        1,
        "refusal retains one original owner"
    );
}

#[test]
fn expired_protocol_owner_is_accounted_and_retried_without_pinning_service() {
    let (mut p, mut r) = paired();
    p.direct(0);
    p.tick(0);
    send(&mut p, &mut r, 0);
    let original = p
        .queue
        .iter()
        .find(|q| Mesh::budget_kind(&q.frame.bytes))
        .unwrap()
        .frame
        .bytes
        .clone();
    p.budget_test_clock = Some(9001);
    p.direct(9001);
    p.tick(9001);
    let owners: Vec<_> = p
        .queue
        .iter()
        .filter(|q| Mesh::budget_kind(&q.frame.bytes))
        .collect();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].at, 9001);
    assert_eq!(p.dropped, 1);
    let budget_negotiation::Message::Revoke(old) = budget_negotiation::decode(&original).unwrap()
    else {
        panic!()
    };
    let budget_negotiation::Message::Revoke(new) =
        budget_negotiation::decode(&owners[0].frame.bytes).unwrap()
    else {
        panic!()
    };
    assert_eq!(new.nonce, old.nonce);
    assert!(new.seq > old.seq);
    assert!(
        !p.queued_budget_owner(9001),
        "expired completed HEALTH marker cannot authorize a new reservation"
    );
    let hh = p.begin_transmission(9001).unwrap();
    assert_eq!(hh.frame().bytes[3], HEALTH);
    p.finish_transmission(hh, true);
    assert!(p.queued_budget_owner(9001));
    let tx = p
        .begin_paid_budget_transmission(9001)
        .expect("expired original cannot pin Some(incomplete)");
    assert_eq!(tx.ordinary_background_service_bytes(), 0);
    assert!(!tx.closes_packet_batch());
    p.finish_transmission(tx, true);
    assert!(
        !p.queued_budget_owner(9001),
        "full paid retry now waits backoff, SDK service can resume"
    );
}

#[test]
fn unreserved_revoke_refreshes_only_after_full_health_and_preserves_owned_queue_metadata() {
    let (mut p, mut r) = paired();
    p.direct(0);
    p.tick(0);
    assert!(!p.queued_budget_owner(0));
    assert!(p.begin_paid_budget_transmission(0).is_none());
    let hh = p.begin_transmission(0).unwrap();
    assert_eq!(hh.frame().bytes[3], HEALTH);
    p.finish_transmission(hh, false);
    assert!(
        !p.queued_budget_owner(0),
        "reservation/refusal of HEALTH is not a completed marker"
    );
    let initial = p
        .queue
        .iter()
        .find(|q| Mesh::budget_kind(&q.frame.bytes))
        .unwrap();
    let ptr = initial.frame.bytes.as_ptr();
    let old = initial.frame.bytes.clone();
    let at = initial.at;
    let len = initial.frame.bytes.len();
    p.budget_test_clock = Some(500);
    p.direct(500);
    p.tick(500);
    assert!(
        !p.queued_budget_owner(500),
        "new issued/coalesced HEALTH1 not physical yet"
    );
    send(&mut p, &mut r, 500);
    let before_bytes = p.bytes;
    let before_index = p
        .queue
        .iter()
        .position(|q| Mesh::budget_kind(&q.frame.bytes))
        .unwrap();
    assert!(p.queued_budget_owner(500));
    let q = &p.queue[before_index];
    assert_eq!(q.frame.bytes.as_ptr(), ptr);
    assert_eq!(q.frame.bytes.len(), len);
    assert_eq!(q.at, at);
    assert_eq!(p.bytes, before_bytes);
    assert!(!q.budget_ever_reserved());
    let budget_negotiation::Message::Revoke(c0) = budget_negotiation::decode(&old).unwrap() else {
        panic!()
    };
    let budget_negotiation::Message::Revoke(c1) =
        budget_negotiation::decode(&q.frame.bytes).unwrap()
    else {
        panic!()
    };
    assert_eq!(c1.nonce, c0.nonce);
    assert_eq!(c1.session, c0.session);
    assert_eq!(c1.seq, 1);
    let tx = p
        .begin_primary_background_transmission(500, 0, true, false)
        .unwrap();
    assert_eq!(tx.frame().bytes[3], BUDGET_CONTROL);
    assert!(tx.queued.budget_ever_reserved());
    let frozen = tx.frame().bytes.clone();
    p.finish_transmission(tx, false);
    p.budget_test_clock = Some(1000);
    p.direct(1000);
    p.tick(1000);
    send(&mut p, &mut r, 1000);
    p.poll_budget(1000);
    let q = p
        .queue
        .iter()
        .find(|q| Mesh::budget_kind(&q.frame.bytes))
        .unwrap();
    assert!(q.budget_ever_reserved());
    assert_eq!(
        q.frame.bytes, frozen,
        "zero-byte refused generic reservation freezes exact owner forever"
    );
    let tx = p.begin_paid_budget_transmission(1000).unwrap();
    assert!(tx.queued.budget_ever_reserved());
    assert_eq!(tx.frame().bytes, frozen);
    p.finish_transmission(tx, false);
    assert_eq!(
        p.queue
            .iter()
            .find(|q| Mesh::budget_kind(&q.frame.bytes))
            .unwrap()
            .frame
            .bytes,
        frozen
    );
}

#[test]
fn queue_flags_preserve_each_other_and_reserved_bits_without_layout_growth() {
    let (mut p, _) = paired();
    p.direct(0);
    p.tick(0);
    for initial in 0u8..=u8::MAX {
        for independent in [false, true] {
            let q = p.queue.front_mut().unwrap();
            q.flags = initial;
            let old = (
                q.at,
                q.priority,
                q.sdt_kind,
                q.sdt_template,
                q.settings_origin,
                q.frame.bytes.as_ptr(),
            );
            q.set_independent_discovery(independent);
            assert_eq!(q.independent_discovery(), independent);
            assert_eq!(
                q.budget_ever_reserved(),
                initial & Queued::BUDGET_EVER_RESERVED != 0
            );
            assert_eq!(
                q.flags & !Queued::INDEPENDENT_DISCOVERY,
                initial & !Queued::INDEPENDENT_DISCOVERY
            );
            let before = q.flags;
            q.mark_budget_reserved();
            assert!(q.budget_ever_reserved());
            assert_eq!(q.independent_discovery(), independent);
            assert_eq!(q.flags, before | Queued::BUDGET_EVER_RESERVED);
            let marked = q.flags;
            q.mark_budget_reserved();
            assert_eq!(q.flags, marked);
            assert_eq!(
                (
                    q.at,
                    q.priority,
                    q.sdt_kind,
                    q.sdt_template,
                    q.settings_origin,
                    q.frame.bytes.as_ptr()
                ),
                old
            );
        }
    }
    assert_eq!(std::mem::size_of::<Queued>(), 48);
    assert_eq!(std::mem::size_of::<Option<Queued>>(), 48);
}
