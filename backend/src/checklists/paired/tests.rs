use super::*;
#[test]
fn complete_backend_pairing_lifecycle_is_isolated() {
    if std::env::var_os("GS_PAIRED_CHECKLIST_CHILD").is_some() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(suite());
        return;
    }
    let root = std::env::temp_dir().join(format!(
        "gs-paired-checklist-{}-{}",
        std::process::id(),
        rand::random::<u128>()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "checklists::paired::tests::complete_backend_pairing_lifecycle_is_isolated",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("GS_PAIRED_CHECKLIST_CHILD", "1")
        .env("GS_PAIRED_CHECKLIST_ROOT", &root)
        .env("GS_CHECKLISTS_FILE", root.join("legacy.json"))
        .env("GS_GSE_CONFIG", root.join("gse.json"))
        .env("GS_GSE_QUALIFICATION", root.join("qualification.json"))
        .env("GS_AUTO_ZERO_CONFIG", root.join("auto-zero.json"))
        .env("GS_FLIGHT_SETUP_CONFIG", root.join("flight.json"))
        .env(
            "GS_LOADCELL_CALIBRATION_PATH",
            root.join("calibration.json"),
        )
        .env(
            "GS_CALIBRATION_CONFIG_PATH",
            root.join("calibration-config.json"),
        )
        .env_remove("GS_STATION_PAIRING_FILE")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > std::time::Duration::from_secs(120) {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("paired checklist lifecycle timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_dir_all(root).unwrap();
}
fn legacy() -> Document {
    Document {
        revision: 42,
        signoffs: Vec::new(),
        completion_events: Vec::new(),
        enforce_before_launch: true,
        checklists: vec![Checklist {
            id: "setup".into(),
            title: "Setup".into(),
            required_before_launch: true,
            items: vec![
                Item {
                    id: "manual".into(),
                    text: "Pressure sensor inspected".into(),
                    checked: false,
                    complete_on: None,
                    completion_source: None,
                    completed_by: None,
                    completed_at_ms: None,
                },
                Item {
                    id: "nitrogen".into(),
                    text: "Cold-flow test passed".into(),
                    checked: false,
                    complete_on: Some(Sequence::NitrogenTest),
                    completion_source: None,
                    completed_by: None,
                    completed_at_ms: None,
                },
            ],
        }],
    }
}

async fn synchronize(
    a: &mut Service,
    b: &mut Service,
    sa: &Arc<AppState>,
    sb: &Arc<AppState>,
    ra: &mut JournalRadio,
    rb: &mut JournalRadio,
    window: (u64, u64),
) {
    let (start, duration) = window;
    let signing = ring::signature::Ed25519KeyPair::from_seed_unchecked(&[17; 32]).unwrap();
    let mut count = 0u64;
    for now in (start..start + duration).step_by(250) {
        let mut outbound = Vec::new();
        ra.poll(&a.journal, now, &signing, |_, wire| {
            outbound.push(wire.to_vec());
            true
        })
        .await
        .unwrap();
        for wire in outbound {
            count += 1;
            if !count.is_multiple_of(11) {
                rb.receive(
                    &b.journal,
                    &wire,
                    now,
                    signing.public_key().as_ref(),
                    model::validate_document,
                )
                .await
                .unwrap();
            }
        }
        let mut outbound = Vec::new();
        rb.poll(&b.journal, now, &signing, |_, wire| {
            outbound.push(wire.to_vec());
            true
        })
        .await
        .unwrap();
        for wire in outbound {
            count += 1;
            if !count.is_multiple_of(13) {
                ra.receive(
                    &a.journal,
                    &wire,
                    now,
                    signing.public_key().as_ref(),
                    model::validate_document,
                )
                .await
                .unwrap();
            }
        }
        if now.is_multiple_of(1000) {
            a.changed(sa).await.unwrap();
            b.changed(sb).await.unwrap();
        }
    }
    a.changed(sa).await.unwrap();
    b.changed(sb).await.unwrap();
}
async fn stage(service: &Service, next: &Document) {
    let values = export(next, &service.generations).unwrap();
    let old = export(&service.document, &service.generations).unwrap();
    for (key, value) in values {
        if old.get(&key) != Some(&value) {
            service
                .journal
                .edit(&key, value, &service.station, model::validate_document)
                .await
                .unwrap();
        }
    }
}
async fn suite() {
    preflight();
    assert!(!requested());
    let root = PathBuf::from(std::env::var_os("GS_PAIRED_CHECKLIST_ROOT").unwrap());
    let state_a = crate::state::tests::test_app_state().await;
    let state_b = crate::state::tests::test_app_state().await;
    // Local approvals survive paired materialization/restart and are invalidated
    // by causal progress updates; replication never invents remote qualification.
    let mut approval = Service::open(
        root.join("approval.sqlite3"),
        root.join("approval.selector"),
        "test".into(),
        "gs".into(),
        vec!["gs".into(), "rrls".into()],
        legacy(),
        true,
    )
    .await
    .unwrap();
    approval.publish().await.unwrap();
    for item_id in ["manual", "nitrogen"] {
        approval
            .check(
                &Check {
                    checklist_id: "setup".into(),
                    item_id: item_id.into(),
                    checked: true,
                },
                "operator",
                10,
            )
            .await
            .unwrap();
    }
    let request = Sign {
        checklist_id: "setup".into(),
        revision: approval.document.revision,
    };
    access(|store| store.update(|doc| doc.sign(&request, "reviewer", 20))).unwrap();
    approval.publish().await.unwrap();
    assert!(
        approval
            .document
            .signed_off(&approval.document.checklists[0])
    );
    let audit_path = access(|store| Ok(store.path.clone())).unwrap();
    assert_eq!(
        Store::open(audit_path).unwrap().document.signoffs[0].signed_by,
        "reviewer"
    );
    approval.journal.close().await;
    approval.storage.pool.close().await;
    let mut approval = Service::open(
        root.join("approval.sqlite3"),
        root.join("approval.selector"),
        "test".into(),
        "gs".into(),
        vec!["gs".into(), "rrls".into()],
        legacy(),
        true,
    )
    .await
    .unwrap();
    approval.publish().await.unwrap();
    assert!(
        approval
            .document
            .signed_off(&approval.document.checklists[0])
    );
    approval
        .check(
            &Check {
                checklist_id: "setup".into(),
                item_id: "manual".into(),
                checked: false,
            },
            "operator",
            30,
        )
        .await
        .unwrap();
    assert!(
        !approval
            .document
            .signed_off(&approval.document.checklists[0])
    );
    assert!(approval.document.signoffs[0].invalidated_at_ms.is_some());
    approval.journal.close().await;
    approval.storage.pool.close().await;
    // A failed first primary seed has a durable migration marker. Restart
    // finishes that same selected catalog before accepting any remote frames.
    let seed = Service::open(
        root.join("seed.sqlite3"),
        root.join("seed.selector"),
        "test".into(),
        "gs".into(),
        vec!["gs".into(), "rrls".into()],
        legacy(),
        false,
    )
    .await
    .unwrap();
    seed.storage.set_seed_pending(true).await.unwrap();
    let (key, value) = export(&seed.document, &seed.generations)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    seed.journal
        .edit(&key, value, "gs", model::validate_document)
        .await
        .unwrap();
    seed.journal.close().await;
    seed.storage.pool.close().await;
    let seeded = Service::open(
        root.join("seed.sqlite3"),
        root.join("seed.selector"),
        "test".into(),
        "gs".into(),
        vec!["gs".into(), "rrls".into()],
        legacy(),
        true,
    )
    .await
    .unwrap();
    assert!(!seeded.storage.seed_pending().await.unwrap());
    assert!(model::project(&seeded.journal.snapshot().await.unwrap().documents).is_ok());
    seeded.journal.close().await;
    seeded.storage.pool.close().await;
    let mut a = Service::open(
        root.join("a.sqlite3"),
        root.join("a.selector"),
        "test".into(),
        "gs".into(),
        vec!["gs".into(), "rrls".into()],
        legacy(),
        true,
    )
    .await
    .unwrap();
    a.publish().await.unwrap();
    assert!(
        a.document.revision > 42,
        "first migration must preserve UI revision monotonicity"
    );
    let original_generation = a.selector.generation.clone();
    let definition_bytes: Vec<u8> =
        sqlx::query_scalar("SELECT document FROM checklist_generations WHERE id=?")
            .bind(&original_generation)
            .fetch_one(&a.storage.pool)
            .await
            .unwrap();
    // A failed durable materialization must retain the previous visible view,
    // preserve its raw edit for retry and lock procedure starts until recovered.
    sqlx::query("CREATE TRIGGER deny_progress BEFORE INSERT ON checklist_progress BEGIN SELECT RAISE(FAIL,'fake disk failure'); END")
        .execute(&a.storage.pool).await.unwrap();
    assert!(
        a.check(
            &Check {
                checklist_id: "setup".into(),
                item_id: "manual".into(),
                checked: true
            },
            "operator",
            90
        )
        .await
        .is_err()
    );
    assert!(!a.document.checklists[0].items[0].checked);
    assert!(binding().lock().unwrap().durability_pending);
    let manual_key = model::progress_key("setup", "manual").unwrap();
    assert!(a.journal.document(&manual_key).await.unwrap().is_some());
    sqlx::query("DROP TRIGGER deny_progress")
        .execute(&a.storage.pool)
        .await
        .unwrap();
    a.changed(&state_a).await.unwrap();
    assert!(a.document.checklists[0].items[0].checked);
    assert!(!binding().lock().unwrap().durability_pending);
    a.check(
        &Check {
            checklist_id: "setup".into(),
            item_id: "manual".into(),
            checked: false,
        },
        "operator",
        95,
    )
    .await
    .unwrap();
    let before_raw = a.journal.metadata().await.unwrap().revision;
    a.check(
        &Check {
            checklist_id: "setup".into(),
            item_id: "manual".into(),
            checked: true,
        },
        "operator",
        100,
    )
    .await
    .unwrap();
    assert!(a.document.checklists[0].items[0].checked);
    assert_eq!(
        sqlx::query_scalar::<_, Vec<u8>>("SELECT document FROM checklist_generations WHERE id=?")
            .bind(&original_generation)
            .fetch_one(&a.storage.pool)
            .await
            .unwrap(),
        definition_bytes,
        "checkbox must not rewrite the definition snapshot"
    );
    assert_eq!(
        a.journal.changed_page(before_raw, 64).await.unwrap().len(),
        1,
        "checkbox must update one raw progress document"
    );
    let event = CompletionEvent {
        id: format!("{}0000000000000001", a.selector.digest),
        sequence: Sequence::NitrogenTest,
        at_ms: 200,
        action: CompletionAction::Passed,
    };
    a.event(&event).await.unwrap();
    assert!(a.document.checklists[0].items[1].checked);
    a.check(
        &Check {
            checklist_id: "setup".into(),
            item_id: "nitrogen".into(),
            checked: false,
        },
        "operator",
        300,
    )
    .await
    .unwrap();
    a.event(&event).await.unwrap();
    assert!(
        !a.document.checklists[0].items[1].checked,
        "replaying a saved engine event must not undo a later manual clear"
    );
    let revision = a.document.revision;
    assert!(
        a.event(&CompletionEvent {
            id: "0123456789abcdef0123456789abcdef".into(),
            ..event.clone()
        })
        .await
        .is_err(),
        "unbound legacy event cannot complete a newly paired template"
    );
    a.journal.close().await;
    a.storage.pool.close().await;
    let mut a = Service::open(
        root.join("a.sqlite3"),
        root.join("a.selector"),
        "test".into(),
        "gs".into(),
        vec!["gs".into(), "rrls".into()],
        legacy(),
        true,
    )
    .await
    .unwrap();
    a.publish().await.unwrap();
    assert!(
        a.document.revision > revision,
        "restart must not strand clients behind a smaller revision"
    );
    assert!(a.document.checklists[0].items[0].checked);
    assert!(!a.document.checklists[0].items[1].checked);
    let mut b = Service::open(
        root.join("b.sqlite3"),
        root.join("b.selector"),
        "test".into(),
        "rrls".into(),
        vec!["gs".into(), "rrls".into()],
        legacy(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        b.journal.metadata().await.unwrap().documents,
        0,
        "RRLS defaults must not overwrite primary settings"
    );
    assert_eq!(
        a.generations, b.generations,
        "first migration of the same installation/IDs shares baseline generations"
    );
    // A separate fresh RRLS can explicitly edit its whole template offline.
    // This must publish a complete catalog, not an incomplete changed manifest.
    let mut offline = Service::open(
        root.join("offline.sqlite3"),
        root.join("offline.selector"),
        "test".into(),
        "rrls".into(),
        vec!["gs".into(), "rrls".into()],
        legacy(),
        false,
    )
    .await
    .unwrap();
    let mut request = offline.document.clone();
    request.checklists[0].title = "Offline setup".into();
    offline.stage_template(request).await.unwrap();
    offline.activate(&state_b).await.unwrap();
    assert!(offline.pending_reason.is_none());
    assert_eq!(offline.document.checklists[0].title, "Offline setup");
    assert!(
        offline
            .journal
            .observed(model::POLICY_KEY)
            .await
            .unwrap()
            .is_some()
    );
    offline.journal.close().await;
    offline.storage.pool.close().await;
    // A real offline check before first contact remains meaningful, unlike a
    // fabricated default-unchecked revision which would create a false conflict.
    b.check(
        &Check {
            checklist_id: "setup".into(),
            item_id: "nitrogen".into(),
            checked: true,
        },
        "field operator",
        400,
    )
    .await
    .unwrap();
    let mut ra = JournalRadio::new("test".into(), "gs".into(), vec!["rrls".into()]).unwrap();
    let mut rb = JournalRadio::new("test".into(), "rrls".into(), vec!["gs".into()]).unwrap();
    synchronize(
        &mut a,
        &mut b,
        &state_a,
        &state_b,
        &mut ra,
        &mut rb,
        (0, 600_000),
    )
    .await;
    assert_eq!(
        a.journal.snapshot().await.unwrap().documents,
        b.journal.snapshot().await.unwrap().documents
    );
    assert!(b.document.checklists[0].items[0].checked);
    assert!(
        state_b.gse.lock().unwrap().calibration_import_allowed(),
        "imported checklist progress must not earn operational qualification"
    );
    // Offline edits of the same item remain two heads and cannot satisfy launch.
    a.check(
        &Check {
            checklist_id: "setup".into(),
            item_id: "manual".into(),
            checked: false,
        },
        "alice",
        500,
    )
    .await
    .unwrap();
    b.check(
        &Check {
            checklist_id: "setup".into(),
            item_id: "manual".into(),
            checked: true,
        },
        "bob",
        501,
    )
    .await
    .unwrap();
    synchronize(
        &mut a,
        &mut b,
        &state_a,
        &state_b,
        &mut ra,
        &mut rb,
        (600_000, 300_000),
    )
    .await;
    let key = model::progress_key("setup", "manual").unwrap();
    let conflict = b.journal.document(&key).await.unwrap().unwrap();
    assert_eq!(conflict.heads.len(), 2);
    assert!(!a.document.checklists[0].items[0].checked);
    assert!(!b.document.checklists[0].items[0].checked);
    let chosen = conflict
        .heads
        .iter()
        .find(|head| head.value.get("checked") == Some(&serde_json::Value::Bool(true)))
        .unwrap()
        .value
        .clone();
    b.journal
        .resolve(
            &key,
            &conflict,
            chosen.clone(),
            "rrls",
            model::validate_document,
        )
        .await
        .unwrap();
    b.refresh_progress(&key, None).await.unwrap();
    assert!(
        b.journal
            .resolve(&key, &conflict, chosen, "rrls", model::validate_document)
            .await
            .is_err()
    );
    synchronize(
        &mut a,
        &mut b,
        &state_a,
        &state_b,
        &mut ra,
        &mut rb,
        (900_000, 300_000),
    )
    .await;
    assert!(a.document.checklists[0].items[0].checked);
    assert!(b.document.checklists[0].items[0].checked);
    // Definitions are staged while active, and selection changes only at Idle.
    state_a.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Paused;
    let old_selector = a.selector.clone();
    let old_selector_bytes = std::fs::read(root.join("a.selector")).unwrap();
    let mut request = a.document.clone();
    request.checklists[0].items[0].text = "Inspect the replacement pressure sensor".into();
    let mut next = a.document.clone();
    next.edit(request).unwrap();
    stage(&a, &next).await;
    a.activate(&state_a).await.unwrap();
    assert_eq!(a.selector, old_selector);
    assert_eq!(
        std::fs::read(root.join("a.selector")).unwrap(),
        old_selector_bytes
    );
    assert!(a.pending_reason.is_some());
    state_a.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Idle;
    a.activate(&state_a).await.unwrap();
    assert_ne!(a.selector.digest, old_selector.digest);
    assert!(!a.document.checklists[0].items[0].checked);
    assert_eq!(a.storage.selected().unwrap().unwrap(), a.selector);
    assert!(!a.selection_pending);
    synchronize(
        &mut a,
        &mut b,
        &state_a,
        &state_b,
        &mut ra,
        &mut rb,
        (1_200_000, 600_000),
    )
    .await;
    assert_eq!(a.selector.digest, b.selector.digest);
    assert_eq!(
        a.document.checklists[0].items[0].text,
        b.document.checklists[0].items[0].text
    );
    assert!(state_b.gse.lock().unwrap().calibration_import_allowed());
    // The local view can still be updated in Fire/recovery; it never dispatches.
    *state_b.state.lock().unwrap() = crate::types::FlightState::Launch;
    state_b.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Paused;
    b.check(
        &Check {
            checklist_id: "setup".into(),
            item_id: "manual".into(),
            checked: true,
        },
        "recovery operator",
        600,
    )
    .await
    .unwrap();
    assert!(b.document.checklists[0].items[0].checked);
    assert_eq!(
        *state_b.state.lock().unwrap(),
        crate::types::FlightState::Launch
    );
    // Selector failure preserves old runtime/template and permits a later retry.
    let mut request = a.document.clone();
    request.checklists[0].title = "Another setup".into();
    let mut next = a.document.clone();
    next.edit(request).unwrap();
    stage(&a, &next).await;
    let selected = a.selector.clone();
    let path = a.storage.selector.clone();
    let bad = root.join("selector-is-directory");
    std::fs::create_dir(&bad).unwrap();
    a.storage.selector = bad;
    assert!(a.activate(&state_a).await.is_err());
    assert_eq!(a.selector, selected);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        serde_json::to_vec(&selected).unwrap()
    );
    a.storage.selector = path;
    a.activate(&state_a).await.unwrap();
    assert_ne!(a.selector.digest, selected.digest);
    // After rename, directory fsync failure is an uncertain durable selection:
    // retain the new matching runtime, block procedure starts and finish on retry.
    let mut request = a.document.clone();
    request.checklists[0].title = "Fsync recovery setup".into();
    a.stage_template(request).await.unwrap();
    a.storage.fail_directory_sync.store(true, Ordering::Relaxed);
    assert!(a.activate(&state_a).await.is_err());
    assert_eq!(a.storage.selected().unwrap().unwrap(), a.selector);
    assert_eq!(a.document.checklists[0].title, "Fsync recovery setup");
    assert!(a.selection_pending);
    assert!(binding().lock().unwrap().durability_pending);
    a.storage
        .fail_directory_sync
        .store(false, Ordering::Relaxed);
    a.activate(&state_a).await.unwrap();
    assert!(!a.selection_pending);
    assert!(!binding().lock().unwrap().durability_pending);
    // Persisted complete generation restores after a crash after selector rename,
    // including a revision increase; no imported qualification is synthesized.
    let reloaded = a.storage.load(&a.selector).await.unwrap();
    assert_eq!(reloaded.0.checklists[0].title, "Fsync recovery setup");
    REQUESTED.store(true, Ordering::Release);
    READY.store(true, Ordering::Release);
    {
        let mut state = binding().lock().unwrap();
        state.digest = Some(a.selector.digest.clone());
        state.definition_pending = false;
        state.durability_pending = false;
    }
    assert!(!qualification_allowed(Some(&old_selector.digest), true));
    assert!(!qualification_allowed(None, true));
    assert!(qualification_allowed(None, false));
    binding().lock().unwrap().durability_pending = true;
    assert!(!qualification_allowed(Some(&a.selector.digest), true));
    binding().lock().unwrap().durability_pending = false;
    assert!(completion_event_id().starts_with(&a.selector.digest));
    // A crash after a durable selector rename but before runtime publication
    // restores the matching immutable generation; no partially edited template.
    let mut next = a.document.clone();
    next.checklists[0].title = "Selected before crash".into();
    let prepared = a
        .storage
        .prepare(
            next,
            a.generations.clone(),
            &a.progress,
            a.selector.revision + 1,
        )
        .await
        .unwrap();
    a.storage.select(&prepared).unwrap();
    a.storage.sync_selector().unwrap();
    assert_ne!(a.selector.generation, prepared.selector.generation);
    drop(prepared);
    a.journal.close().await;
    a.storage.pool.close().await;
    let restored = Service::open(
        root.join("a.sqlite3"),
        root.join("a.selector"),
        "test".into(),
        "gs".into(),
        vec!["gs".into(), "rrls".into()],
        legacy(),
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        restored.document.checklists[0].title,
        "Selected before crash"
    );
    restored.journal.close().await;
    restored.storage.pool.close().await;
    // Local qualification files retain the exact template binding and pending
    // local reset receipts; incoming journal heads never call those functions.
    a.journal.close().await;
    a.storage.pool.close().await;
    b.journal.close().await;
    b.storage.pool.close().await;
    // Exercise the real background worker, shared Mesh envelope and durable
    // local-event handoff with an isolated fake pairing (never hardware IO).
    std::fs::write(root.join("fake-seed"), [17u8; 32]).unwrap();
    std::fs::write(root.join("pairing.json"),serde_json::to_vec(&serde_json::json!({"version":1,"installation_id":"test","station_id":"gs","signing_seed_file":"fake-seed","peers":[{"id":"rrls","role":"rrls","urls":[]}]})).unwrap()).unwrap();
    unsafe {
        std::env::set_var("GS_STATION_PAIRING_FILE", root.join("pairing.json"));
    }
    let pairing = Arc::new(crate::station_pairing::Pairing::load().unwrap().unwrap());
    unsafe {
        std::env::remove_var("GS_STATION_PAIRING_FILE");
    }
    let mesh = Arc::new(Mutex::new(crate::rrls::Mesh::new(
        crate::rrls::Role::Primary,
        7,
    )));
    mesh.lock().unwrap().authenticated_settings_peer(0);
    READY.store(false, Ordering::Release);
    *binding().lock().unwrap() = BindingState::default();
    let worker = start(state_a.clone(), pairing, mesh.clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !ready() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let event = CompletionEvent {
        id: completion_event_id(),
        sequence: Sequence::NitrogenTest,
        at_ms: 900,
        action: CompletionAction::Passed,
    };
    assert!(!deliver_event(&event).unwrap());
    let mut signed_wire = false;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            // Simulate the normal continuous UART drain; low-priority Mesh
            // frames intentionally expire if a consumer stops for250ms.
            {
                let mut link = mesh.lock().unwrap();
                let now = link.now();
                while let Some(frame) = link.pop(now) {
                    if frame.bytes.len() >= 8
                        && JournalRadio::recognizes(&frame.bytes[4..frame.bytes.len() - 4])
                    {
                        signed_wire = true;
                    }
                }
            }
            if deliver_event(&event).unwrap() && signed_wire {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let live = SERVICE.get().unwrap().lock().await;
    assert!(live.storage.event_saved(&event).await.unwrap());
    assert!(live.document.checklists[0].items[1].checked);
    drop(live);
    state_a.request_shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap();
    println!("PAIRED_CHECKLIST_LIVE_LIFECYCLE_PASS");
}
