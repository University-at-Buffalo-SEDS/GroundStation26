use super::*;
use crate::comms::OrdinaryLaneState;

#[test]
fn typed_legacy_reply_is_ordinary_but_raw_opaque_and_controls_are_not() {
    let mut mesh = Mesh::new(Role::Primary, 42);
    assert_eq!(mesh.ordinary_background_state(), OrdinaryLaneState::Empty);
    let now = mesh.now();
    assert!(mesh.queue_operating_mode_reply(b"TC\x04legacy-display-reply", now));
    assert_eq!(mesh.queue.back().unwrap().settings_origin, SettingsOrigin::Reply);
    assert_eq!(mesh.ordinary_background_state(), OrdinaryLaneState::Ready);
    let tx = mesh.begin_mode_reply_transmission(mesh.now()).unwrap();
    assert!(tx.ordinary_background_service_bytes() > 0);
    mesh.finish_bounded_transmission(tx, true);
    assert_eq!(mesh.ordinary_background_state(), OrdinaryLaneState::Empty);

    mesh.enqueue(Outgoing { command:true, bytes:encode(SETTINGS,b"TC\x04unowned") },0,mesh.now());
    assert_eq!(mesh.queue.back().unwrap().settings_origin,SettingsOrigin::None);
    assert_eq!(mesh.ordinary_background_state(),OrdinaryLaneState::Blocked);
    let raw=mesh.begin_transmission(mesh.now()).unwrap();
    assert_eq!(raw.ordinary_background_service_bytes(),0);
    mesh.finish_bounded_transmission(raw,true);

    mesh.enqueue(Outgoing {command:true,bytes:encode(HOST_CONTROL,b"not-a-validated-command")},254,mesh.now());
    assert_ne!(mesh.ordinary_background_state(),OrdinaryLaneState::Ready);
    let protected=mesh.begin_transmission(mesh.now()).unwrap();
    assert_eq!(protected.ordinary_background_service_bytes(),0);
    mesh.finish_bounded_transmission(protected,true);
}

#[test]
fn framed_parent_remains_owned_and_blocked_after_probe_and_refusal() {
    let mut mesh=Mesh::new(Role::Primary,43);
    mesh.settings_peer_seen=Some(mesh.now());
    mesh.enable_settings_framing();
    assert!(mesh.queue_settings(&vec![0x55;357],mesh.now()));
    assert_eq!(mesh.queue.back().unwrap().settings_origin,SettingsOrigin::Settings);
    assert!(mesh.has_settings_parent_for_test());
    assert_eq!(mesh.ordinary_background_state(),OrdinaryLaneState::Ready);
    let refused=mesh.begin_transmission(mesh.now()).unwrap();
    let original=refused.queued.frame.bytes.as_ptr();
    mesh.note_settings_physical_prefix(&refused,3);
    mesh.finish_bounded_transmission(refused,false);
    assert_eq!(mesh.queue.front().unwrap().frame.bytes.as_ptr(),original);
    assert!(mesh.has_settings_parent_for_test());
    assert_ne!(mesh.ordinary_background_state(),OrdinaryLaneState::Empty);
    let probe=mesh.begin_transmission(mesh.now()).unwrap();
    mesh.note_settings_counted_drain(&probe,None);
    mesh.finish_bounded_transmission(probe,true);
    assert!(mesh.has_settings_parent_for_test());
    assert_eq!(mesh.ordinary_background_state(),OrdinaryLaneState::Blocked);
}

#[test]
fn settings_origin_reuses_the_existing_bool_layout_slot() {
    // Layout-only mirror of the immediately preceding private queue shape.
    #[allow(dead_code)]
    struct PreviousQueued { mode_reply:bool, frame:Outgoing, at:u64, priority:u8,
        independent_discovery:bool, sdt_kind:u8, sdt_template:u32 }
    assert_eq!(std::mem::size_of::<SettingsOrigin>(),std::mem::size_of::<bool>());
    assert_eq!(std::mem::size_of::<Queued>(),std::mem::size_of::<PreviousQueued>());
    assert_eq!(std::mem::size_of::<Option<Queued>>(),std::mem::size_of::<Option<PreviousQueued>>());
    println!("QUEUED_ORIGIN_LAYOUT queued={} option={}",std::mem::size_of::<Queued>(),std::mem::size_of::<Option<Queued>>());
}
