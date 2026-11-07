use super::*;
fn canonical(name:&str)->Vec<u8> {
    crate::telemetry_schema::initialize().unwrap();
    let ty=crate::telemetry_schema::data_type(name);
    let mut payload=vec![0u8;match name {"KG1000"=>4,"BOARD_RECOVERY_REQUEST"=>32,_=>1}];
    if name=="BOARD_RECOVERY_REQUEST" {payload[0]=1;payload[4]=1;payload[24]=1;}
    sedsnet::wire_format::pack_packet(&sedsnet::packet::Packet::new(ty,
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],"GS",100,Arc::from(payload)).unwrap()).to_vec()
}
fn queue(mesh:&mut Mesh,kind:u8,body:&[u8]) {
    mesh.enqueue(Outgoing{command:false,bytes:encode(kind,body)},128,0);
}
fn sdt(kind:u8,body:&[u8])->Vec<u8> {
    let mut bytes=b"SDT".to_vec();bytes.push(kind);bytes.extend_from_slice(body);
    bytes.extend_from_slice(&crc(&bytes).to_le_bytes());bytes
}
#[test]
fn recovery_control_continuous_telemetry_queue_and_active_send_do_not_block() {
    let mut mesh=Mesh::new(Role::Primary,1);
    let data=canonical("KG1000");
    for _ in 0..100 {
        queue(&mut mesh,DOWN_DATA,&data);
        assert!(!mesh.pending_recovery_control());
        let tx=mesh.begin_transmission(0).unwrap();
        assert_eq!(mesh.active_frame_count,1);
        assert!(!mesh.pending_recovery_control(),"active sensor bytes are not controller authority");
        mesh.finish_transmission(tx,true);
    }
    for kind in [HEALTH,READY,POSITION,VOICE,BUDGET_CONTROL,RF_PROFILE_CONTROL] {queue(&mut mesh,kind,b"metadata");}
    assert!(!mesh.queue.is_empty());
    assert!(!mesh.pending_recovery_control());
    assert_eq!(mesh.dropped,0,"typed accounting cannot discard accepted traffic");
}
#[test]
fn recovery_control_owned_commands_remain_pending_across_pop_partial_retry_and_clear() {
    for name in ["VALVE_COMMAND","ACTUATOR_COMMAND","FLIGHT_COMMAND","FLIGHT_STATE","BOARD_RECOVERY_REQUEST"] {
        let mut mesh=Mesh::new(Role::Primary,1);
        queue(&mut mesh,UP_DATA,&canonical(name));
        assert!(mesh.pending_recovery_control(),"{name}");
        let tx=mesh.begin_transmission(0).unwrap();
        assert!(mesh.queue.is_empty());
        assert!(mesh.pending_recovery_control(),"popping a frame is not physical completion");
        let original=tx.frame().bytes.clone();
        assert!(mesh.finish_transmission(tx,false).is_none());
        assert!(mesh.pending_recovery_control(),"failed/partial send still owns its retry");
        let retry=mesh.begin_transmission(1).unwrap();
        assert_eq!(retry.frame().bytes,original);
        mesh.clear();
        assert!(mesh.pending_recovery_control(),"route clear cannot certify an active physical owner");
        mesh.finish_transmission(retry,true);
        assert!(!mesh.pending_recovery_control());
    }
    for kind in [DOWN_COMMAND,UP_COMMAND,AV_BAY_COMMAND,HOST_CONTROL,SETTINGS,255] {
        let mut mesh=Mesh::new(Role::Primary,1);queue(&mut mesh,kind,b"opaque");
        assert!(mesh.pending_recovery_control(),"unknown/control envelope must remain conservative");
    }
}
#[test]
fn recovery_control_sdt_templates_preserve_type_and_namespace_without_guessing_chunks() {
    let mut mesh=Mesh::new(Role::Primary,1);
    let mut telemetry=vec![7];telemetry.extend_from_slice(&canonical("KG1000"));
    queue(&mut mesh,DOWN_DATA,&sdt(1,&telemetry));
    queue(&mut mesh,DOWN_DATA,&sdt(4,&[0,7,12,1,0,0]));
    assert!(!mesh.pending_recovery_control(),"known telemetry compact frames do not own commands");
    let mut control=vec![7];control.extend_from_slice(&canonical("FLIGHT_STATE"));
    queue(&mut mesh,AV_BAY_DATA,&sdt(1,&control));
    assert!(mesh.pending_recovery_control(),"same ID in other namespace is a controller");
    let mut unknown=Mesh::new(Role::Primary,1);queue(&mut unknown,DOWN_DATA,&sdt(4,&[0,77,0]));
    assert!(unknown.pending_recovery_control());
    let mut chunks=Mesh::new(Role::Primary,1);queue(&mut chunks,DOWN_DATA,&sdt(3,&[0;12]));
    assert!(chunks.pending_recovery_control());
}
