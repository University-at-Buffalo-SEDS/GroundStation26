use super::*;
use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
fn active() -> ClockRecord {
    let mut store = crate::launch_history::Store::ephemeral();
    store
        .update_primary(
            LaunchClockMsg {
                kind: LaunchClockKind::TPlus,
                anchor_timestamp_ms: Some(9000),
                duration_ms: None,
            },
            10000,
            0,
        )
        .unwrap()
}
#[test]
fn current_session_challenge_required_and_queries_bound_legacy_peer() {
    let clock = active();
    let mut rr = Runtime::new(None, true).unwrap();
    rr.connect(true, Some(42));
    let mut sent = Vec::new();
    for now in [0, 1, 4999, 5000, 10000, 15000, 60000] {
        rr.offer(None, &clock, now, |_, wire| {
            sent.push(wire.to_vec());
            true
        })
        .unwrap();
    }
    assert_eq!(sent.len(), 3);
    let mut primary = Runtime::new(None, false).unwrap();
    primary.connect(false, Some(42));
    primary.receive(None, &sent[2], 10001, &clock).unwrap();
    let reply = primary.reply.take().unwrap();
    assert!(rr.receive(None, &reply, 10002, &clock).unwrap().is_some());
    assert!(rr.receive(None, &reply, 10003, &clock).is_err());
    rr.connect(true, Some(43));
    assert!(rr.receive(None, &reply, 10004, &clock).is_err());
}
#[test]
fn unknown_trailing_oversized_and_unsigned_configured_frames_are_rejected() {
    let clock = active();
    let query = seal(
        None,
        "",
        42,
        Message::Query {
            ack_supported: true,
            nonce: "a".repeat(32),
        },
    )
    .unwrap();
    let mut with_tail = query.clone();
    with_tail.push(0);
    assert!(open(None, &with_tail).is_err());
    assert!(open(None, &vec![0; MAX_WIRE + 1]).is_err());
    assert!(!recognizes(b"OM\x02legacy"));
    assert!(recognizes(b"TC\x01bad"));
    let mut rr = Runtime::new(None, true).unwrap();
    rr.connect(true, Some(42));
    assert!(rr.receive(None, &query, 0, &clock).is_err());
}
#[test]
fn mesh_clock_extension_is_not_sdk_or_document_and_never_refreshes_lease() {
    use crate::rrls::Role;
    let mut primary = crate::rrls::Mesh::new(Role::Primary, 42);
    let mut rr = crate::rrls::Mesh::new(Role::Rrls, 99);
    primary.authenticated_settings_peer(0);
    let wire = seal(
        None,
        "",
        42,
        Message::Hint {
            authority: "a".repeat(32),
            revision: 1,
        },
    )
    .unwrap();
    assert!(primary.queue_settings(&wire, 0));
    let frame = primary.pop(0).unwrap();
    assert!(rr.incoming(frame.command, &frame.bytes, 0).is_none());
    assert!(rr.take_settings().is_none());
    assert!(rr.take_operating_mode().is_none());
    assert_eq!(rr.take_launch_clock(), Some(wire));
    assert_eq!(rr.operating_mode_session(0), None);
    assert!(!rr.settings_available(0));
    assert!(!rr.active(0));
}
fn paired(root: &std::path::Path, id: &str, seed: u8) -> Pairing {
    let seed_name = format!("seed-{id}");
    std::fs::write(root.join(&seed_name), [seed; 32]).unwrap();
    let path = root.join(format!("{id}.json"));
    std::fs::write(&path,serde_json::json!({"version":1,"installation_id":"launch-test","station_id":id,"signing_seed_file":seed_name,"peers":[{"id":if id=="gs"{"rrls"}else{"gs"},"role":if id=="gs"{"rrls"}else{"primary"}}]}).to_string()).unwrap();
    Pairing::from_file(&path).unwrap()
}
#[test]
fn paired_clock_requires_signature_primary_identity_and_current_challenge() {
    let root = crate::recording_export::tests::TestDirectory::new();
    let primary = paired(&root.0, "gs", 7);
    let observer = paired(&root.0, "rrls", 7);
    let impostor = paired(&root.0, "gs", 8);
    let clock = active();
    let mut rr = Runtime::new(Some(&observer), true).unwrap();
    let mut gs = Runtime::new(Some(&primary), false).unwrap();
    rr.connect(true, Some(42));
    gs.connect(false, Some(42));
    let mut query = Vec::new();
    rr.offer(Some(&observer), &clock, 0, |_, w| {
        query = w.to_vec();
        true
    })
    .unwrap();
    gs.receive(Some(&primary), &query, 1, &clock).unwrap();
    let reply = gs.reply.take().unwrap();
    let Message::Query { nonce, .. } = open(Some(&primary), &query).unwrap().message else {
        panic!()
    };
    let bad = seal(
        Some(&impostor),
        "rrls",
        42,
        Message::Reply {
            ack_supported: true,
            nonce: nonce.clone(),
            clock: clock.clone(),
        },
    )
    .unwrap();
    assert!(rr.receive(Some(&observer), &bad, 2, &clock).is_err());
    let unsigned = seal(
        None,
        "",
        42,
        Message::Reply {
            ack_supported: true,
            nonce: nonce.clone(),
            clock: clock.clone(),
        },
    )
    .unwrap();
    assert!(rr.receive(Some(&observer), &unsigned, 2, &clock).is_err());
    assert!(
        rr.receive(Some(&observer), &reply, 2, &clock)
            .unwrap()
            .is_some()
    );
    rr.connect(false, None);
    rr.connect(true, Some(43));
    assert!(rr.receive(Some(&observer), &reply, 3, &clock).is_err());
}
#[tokio::test]
async fn observer_reset_roundtrip_requires_expected_contract_durable_primary_and_keeps_offline_clock()
 {
    *broker().lock().unwrap() = ResetBroker::default();
    let state = crate::state::tests::test_app_state().await;
    state.set_launch_clock(LaunchClockMsg {
        kind: LaunchClockKind::TPlus,
        anchor_timestamp_ms: Some(9000),
        duration_ms: None,
    });
    let clock = state.launch_history.lock().unwrap().snapshot();
    let nonce = queue_reset(clock.clone()).unwrap();
    let mut rr = Runtime::new(None, true).unwrap();
    rr.connect(false, None);
    rr.offer_reset(None, &clock, 0, |_| panic!("offline reset must not send"))
        .unwrap();
    assert!(reset_status().pending);
    assert_eq!(state.launch_history.lock().unwrap().snapshot(), clock);
    rr.connect(true, Some(42));
    let mut request = Vec::new();
    rr.offer_reset(None, &clock, 1, |w| {
        request = w.to_vec();
        true
    })
    .unwrap();
    let mut gs = Runtime::new(None, false).unwrap();
    gs.connect(false, Some(42));
    let Some(Outcome::Reset { expected, .. }) = gs.receive(None, &request, 2, &clock).unwrap()
    else {
        panic!("missing reset request")
    };
    state.try_reset_expected_launch_clock(&expected).unwrap();
    let reset = state.launch_history.lock().unwrap().snapshot();
    assert_eq!(reset.clock, LaunchClockMsg::idle());
    assert_ne!(reset.session, clock.session);
    let reply = seal(
        None,
        "",
        42,
        Message::ResetReply {
            nonce: nonce.clone(),
            clock: reset.clone(),
            error: None,
            durable: true,
        },
    )
    .unwrap();
    assert!(
        matches!(rr.receive(None,&reply,3,&clock).unwrap(),Some(Outcome::Clock(value)) if value==reset)
    );
    assert!(!reset_status().pending);
    assert!(
        await_existing_reset().await.unwrap(),
        "duplicate HTTP reset observes the existing durable result without creating another reset"
    );
    assert!(broker().lock().unwrap().result.as_ref().unwrap().1.is_ok());
    state.confirm_new_launch_cycle();
    state.set_launch_clock(LaunchClockMsg {
        kind: LaunchClockKind::TMinus,
        anchor_timestamp_ms: Some(10000),
        duration_ms: Some(1000),
    });
    state.set_launch_clock(LaunchClockMsg {
        kind: LaunchClockKind::TPlus,
        anchor_timestamp_ms: Some(11000),
        duration_ms: None,
    });
    let new_launch = state.launch_history.lock().unwrap().snapshot();
    assert!(state.try_reset_expected_launch_clock(&expected).is_err());
    assert_eq!(state.launch_history.lock().unwrap().snapshot(), new_launch);
    let nonce = queue_reset(new_launch.clone()).unwrap();
    rr.offer_reset(None, &new_launch, 4, |_| true).unwrap();
    let failed = seal(
        None,
        "",
        42,
        Message::ResetReply {
            nonce,
            clock: reset,
            error: Some("disk failed".into()),
            durable: false,
        },
    )
    .unwrap();
    assert!(
        rr.receive(None, &failed, 5, &new_launch).is_err(),
        "older reset reply never clears a new launch"
    );
    rr.offer_reset(None, &clock, 6, |_| {
        panic!("superseded request must not send")
    })
    .unwrap();
    assert!(!reset_status().pending);
    assert!(reset_status().error.is_some());
    let nonce = queue_reset(new_launch.clone()).unwrap();
    rr.offer_reset(None, &new_launch, 7, |_| true).unwrap();
    let failed = seal(
        None,
        "",
        42,
        Message::ResetReply {
            nonce,
            clock: new_launch.clone(),
            error: Some("disk failed".into()),
            durable: false,
        },
    )
    .unwrap();
    assert!(rr.receive(None, &failed, 8, &new_launch).unwrap().is_none());
    assert!(broker().lock().unwrap().result.as_ref().unwrap().1.is_err());
}
#[test]
fn clock_query_and_hints_cannot_bypass_a_failed_reset_write() {
    let mut journal = crate::launch_history::Store::ephemeral();
    journal
        .update_primary(
            LaunchClockMsg {
                kind: LaunchClockKind::TPlus,
                anchor_timestamp_ms: Some(9000),
                duration_ms: None,
            },
            10000,
            0,
        )
        .unwrap();
    let reset = journal.reset_primary(11000, 1000).unwrap();
    let mut primary = Runtime::new(None, false).unwrap();
    primary.connect(true, Some(42));
    primary.primary_durable = false;
    let query = seal(
        None,
        "",
        42,
        Message::Query {
            ack_supported: true,
            nonce: "a".repeat(32),
        },
    )
    .unwrap();
    assert!(primary.receive(None, &query, 0, &reset).is_err());
    assert!(primary.reply.is_none());
    primary
        .offer(None, &reset, 0, |_, _| {
            panic!("unpersisted generation cannot leak through a hint")
        })
        .unwrap();
    primary.primary_durable = true;
    assert!(primary.receive(None, &query, 1, &reset).unwrap().is_none());
    let reply = primary.reply.take().unwrap();
    let Message::Reply { clock, .. } = open(None, &reply).unwrap().message else {
        panic!()
    };
    assert_eq!(clock, reset);
}

#[test]
fn a_new_health_session_or_changed_hint_requires_fresh_verification() {
    let clock = active();
    let mut rr = Runtime::new(None, true).unwrap();
    rr.connect(true, Some(42));
    assert!(rr.verification_pending());
    let mut query = Vec::new();
    rr.offer(None, &clock, 0, |_, wire| {
        query = wire.to_vec();
        true
    })
    .unwrap();
    let mut primary = Runtime::new(None, false).unwrap();
    primary.connect(false, Some(42));
    primary.receive(None, &query, 1, &clock).unwrap();
    rr.receive(None, &primary.reply.take().unwrap(), 2, &clock)
        .unwrap();
    assert!(!rr.verification_pending());
    rr.connect(true, Some(43));
    assert!(
        rr.verification_pending(),
        "a changed HEALTH session is not already verified"
    );
    rr.remaining = 0;
    rr.pending = None;
    let hint = seal(
        None,
        "",
        43,
        Message::Hint {
            authority: clock.authority.clone(),
            revision: clock.revision + 1,
        },
    )
    .unwrap();
    rr.receive(None, &hint, 3, &clock).unwrap();
    assert!(
        rr.verification_pending(),
        "a known primary revision change retains the clock but requires a new snapshot"
    );
}
#[test]
fn negotiated_revision_survives_lost_hints_reply_and_busy_queue_then_goes_silent() {
    let old = active();
    let mut gs = Runtime::new(None, false).unwrap();
    let mut rr = Runtime::new(None, true).unwrap();
    gs.connect(false, Some(42));
    rr.connect(true, Some(42));
    let mut frames = Vec::new();
    rr.offer(None, &old, 0, |_, wire| {
        frames.push(wire.to_vec());
        true
    })
    .unwrap();
    gs.receive(None, &frames.remove(0), 0, &old).unwrap();
    assert!(gs.peer_ack_supported);
    assert!(
        !gs.connected,
        "direct primary does not acquire a SETTINGS/READY lease from clock traffic"
    );
    gs.offer(None, &old, 1, |_, wire| {
        frames.push(wire.to_vec());
        true
    })
    .unwrap();
    let reply = frames
        .iter()
        .find(|wire| matches!(open(None, wire).unwrap().message, Message::Reply { .. }))
        .unwrap();
    rr.receive(None, reply, 2, &old).unwrap();
    frames.clear();
    rr.acknowledge(None, &old).unwrap();
    rr.offer(None, &old, 3, |_, wire| {
        frames.push(wire.to_vec());
        true
    })
    .unwrap();
    gs.receive(None, &frames.remove(0), 3, &old).unwrap();
    assert!(gs.confirmed);
    gs.offer(None, &old, 90000, |_, _| {
        panic!("stable confirmed peer must be silent")
    })
    .unwrap();
    let mut new = old.clone();
    new.revision += 1;
    new.session = "b".repeat(32);
    new.clock = LaunchClockMsg::idle();
    new.t0_utc_ms = None;
    for _ in 0..3 {
        gs.offer(None, &new, 100000, |_, _| false).unwrap();
        assert_eq!(gs.hint_attempts, 0);
        assert_eq!(gs.hint_remaining, 3);
    }
    for now in [100000, 105000, 115000] {
        frames.clear();
        gs.offer(None, &new, now, |_, wire| {
            frames.push(wire.to_vec());
            true
        })
        .unwrap();
        assert_eq!(frames.len(), 1); /* all three hints are lost */
    }
    frames.clear();
    gs.offer(None, &new, 135000, |_, wire| {
        frames.push(wire.to_vec());
        true
    })
    .unwrap();
    assert_eq!(
        frames.len(),
        1,
        "negotiated dirty revision must outlive the initial bounded hints"
    );
    rr.receive(None, &frames.remove(0), 135001, &old).unwrap();
    rr.offer(None, &old, 135002, |_, _| false).unwrap();
    assert_eq!(rr.remaining, 3, "refusal cannot consume a query attempt");
    rr.offer(None, &old, 135003, |_, wire| {
        frames.push(wire.to_vec());
        true
    })
    .unwrap();
    gs.receive(None, &frames.remove(0), 135004, &new).unwrap();
    gs.offer(None, &new, 135005, |_, wire| {
        frames.push(wire.to_vec());
        true
    })
    .unwrap();
    assert!(
        frames
            .iter()
            .any(|wire| matches!(open(None, wire).unwrap().message, Message::Reply { .. }))
    );
    frames.clear(); /* first reply lost */
    rr.offer(None, &old, 140003, |_, wire| {
        frames.push(wire.to_vec());
        true
    })
    .unwrap();
    gs.receive(None, &frames.remove(0), 140004, &new).unwrap();
    gs.offer(None, &new, 140005, |_, wire| {
        frames.push(wire.to_vec());
        true
    })
    .unwrap();
    let reply = frames
        .iter()
        .find(|wire| matches!(open(None, wire).unwrap().message, Message::Reply { .. }))
        .unwrap();
    assert!(
        matches!(rr.receive(None,reply,140006,&old).unwrap(),Some(Outcome::Clock(clock)) if clock==new)
    );
    frames.clear();
    rr.acknowledge(None, &new).unwrap();
    rr.offer(None, &new, 140007, |_, _| false).unwrap();
    assert!(rr.ack.is_some(), "ACK ownership stays pending on refusal");
    rr.offer(None, &new, 140008, |_, wire| {
        frames.push(wire.to_vec());
        true
    })
    .unwrap();
    gs.receive(None, &frames.remove(0), 140009, &new).unwrap();
    assert!(gs.confirmed);
    for now in [170009, 1000000, 3600000] {
        gs.offer(None, &new, now, |_, _| {
            panic!("no stable continuous resync")
        })
        .unwrap();
        rr.offer(None, &new, now, |_, _| {
            panic!("observer stable after reply and ACK")
        })
        .unwrap();
    }
}
#[test]
fn old_profiles_and_unnegotiated_peers_have_no_endless_probe_or_stale_ack_override() {
    let clock = active();
    let mut gs = Runtime::new(None, false).unwrap();
    gs.connect(true, Some(42));
    let mut old = seal(
        None,
        "",
        42,
        Message::Query {
            nonce: "a".repeat(32),
            ack_supported: true,
        },
    )
    .unwrap();
    old[2] = 2;
    assert!(recognizes(&old));
    assert!(gs.receive(None, &old, 0, &clock).is_err());
    let unsupported = seal(
        None,
        "",
        42,
        Message::Query {
            nonce: "a".repeat(32),
            ack_supported: false,
        },
    )
    .unwrap();
    assert!(gs.receive(None, &unsupported, 0, &clock).is_err());
    assert!(!gs.peer_ack_supported);
    let ack = seal(
        None,
        "",
        42,
        Message::Ack {
            authority: clock.authority.clone(),
            revision: clock.revision,
        },
    )
    .unwrap();
    assert!(gs.receive(None, &ack, 0, &clock).is_err());
    let mut count = 0;
    for now in [0, 5000, 15000, 35000, 65000, 100000, 1000000] {
        gs.offer(None, &clock, now, |_, _| {
            count += 1;
            true
        })
        .unwrap();
    }
    assert_eq!(
        count, 3,
        "unknown/legacy peer stops after bounded capability discovery"
    );
    gs.peer_ack_supported = true;
    let mut newer = clock.clone();
    newer.revision += 1;
    gs.offer(None, &newer, 1000001, |_, _| false).unwrap();
    assert_eq!(
        gs.hint.as_ref(),
        Some(&(newer.authority.clone(), newer.revision))
    );
    assert!(gs.receive(None, &ack, 1000002, &newer).is_err());
    assert!(
        !gs.confirmed,
        "old positive ACK must not confirm the newest revision"
    );
}
#[tokio::test]
async fn revision_ack_waits_for_observer_durability_and_retries_same_snapshot() {
    let root = crate::recording_export::tests::TestDirectory::new();
    let path = root.0.join("clock.json");
    let saved = root.0.join("saved.json");
    let mut observer =
        crate::launch_history::Store::open(path.clone(), true, None, 10000, 0).unwrap();
    let clock = active();
    observer
        .apply_primary(clock.clone(), true, 10000, 0)
        .unwrap();
    crate::launch_history::flush(observer.waiter())
        .await
        .unwrap();
    std::fs::rename(&path, &saved).unwrap();
    std::fs::create_dir(&path).unwrap();
    let mut next = clock.clone();
    next.revision += 1;
    next.session = "b".repeat(32);
    next.clock = LaunchClockMsg::idle();
    next.t0_utc_ms = None;
    observer
        .apply_primary(next.clone(), true, 11000, 1000)
        .unwrap();
    let mut state = crate::state::tests::test_app_state().await;
    Arc::get_mut(&mut state).unwrap().launch_history = Arc::new(std::sync::Mutex::new(observer));
    let mut runtime = Runtime::new(None, true).unwrap();
    runtime.connect(true, Some(42));
    assert!(
        acknowledge_current(&state, &mut runtime, None)
            .await
            .is_err()
    );
    assert!(
        runtime.ack.is_none(),
        "failed write cannot advertise completion"
    );
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&saved, &path).unwrap();
    acknowledge_current(&state, &mut runtime, None)
        .await
        .unwrap();
    let ack = runtime.ack.take().unwrap();
    assert!(
        matches!(open(None,&ack).unwrap().message,Message::Ack{authority,revision} if authority==next.authority && revision==next.revision)
    );
    let restored = crate::launch_history::Store::open(path, true, None, 12000, 0).unwrap();
    assert_eq!(restored.snapshot(), next);
}
#[test]
fn negotiated_dirty_hint_crosses_real_direct_mesh_without_creating_a_lease() {
    let clock = active();
    let mut runtime = Runtime::new(None, false).unwrap();
    runtime.connect(false, Some(42));
    let query = seal(
        None,
        "",
        42,
        Message::Query {
            nonce: "a".repeat(32),
            ack_supported: true,
        },
    )
    .unwrap();
    runtime.receive(None, &query, 0, &clock).unwrap();
    runtime.reply = None;
    let mut primary = crate::rrls::Mesh::new(crate::rrls::Role::Primary, 42);
    let mut observer = crate::rrls::Mesh::new(crate::rrls::Role::Rrls, 99);
    assert!(!primary.settings_available(0));
    runtime
        .offer(None, &clock, 0, |validated, wire| {
            assert!(
                validated,
                "known direct peer hint uses bounded validated metadata admission"
            );
            primary.queue_operating_mode_reply(wire, 0)
        })
        .unwrap();
    assert!(!primary.settings_available(0));
    assert!(!primary.active(0));
    let frame = primary.pop(0).unwrap();
    assert!(observer.incoming(frame.command, &frame.bytes, 0).is_none());
    assert!(observer.take_settings().is_none());
    assert!(matches!(
        open(None, &observer.take_launch_clock().unwrap())
            .unwrap()
            .message,
        Message::Hint { .. }
    ));
    assert_eq!(observer.operating_mode_session(0), None);
    assert!(!observer.settings_available(0));
    assert!(!observer.active(0));
}
