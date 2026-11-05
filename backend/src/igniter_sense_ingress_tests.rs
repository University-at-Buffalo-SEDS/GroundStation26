// Included in the existing actual telemetry-task test fixture; no hardware sends.
#[tokio::test]
async fn igniter_sense_actual_wire_ingress_records_original_signal_and_exact_metadata() {
    let (db_tx, mut db_rx)=mpsc::channel(64);
    let (state,mut commands)=test_app_state_with_cmd_rx(db_tx.clone()).await;
    let overflow=test_db_overflow();
    let now=recording_network_utc_ms().and_then(|n|n.try_into().ok()).unwrap_or_else(get_current_timestamp_ms);
    let words=[1_u32,11,u32::MAX-1,0xfedcba98,u32::MAX-2,500,0,2048,3300,20,100,1];
    let payload:Vec<u8>=words.into_iter().flat_map(u32::to_le_bytes).collect();
    let make=|sender:&str|Packet::new(crate::telemetry_schema::data_type("AB_IGNITER_SENSE"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],sender,now,payload.clone().into()).unwrap();
    let original=make("AB");
    let packed=sedsnet::wire_format::pack_packet(&original);
    let decoded=sedsnet::wire_format::unpack_packet(&packed).unwrap();
    assert_eq!(decoded.data_as_u32().unwrap(),words);
    let rows=handle_packet(&state,&db_tx,&overflow,decoded).await;
    let status=rows.iter().find(|row|row.data_type=="IGNITER_SENSE_STATUS").unwrap();
    assert_eq!(status.values.len(),14);
    assert_eq!(status.values[3],Some(1.0));
    let observed_utc=status.values[10..14].iter().enumerate().fold(0_u64,|clock,(at,value)| {
        let word=value.unwrap();assert!(word.is_finite()&&word.fract()==0.0&&(0.0..=65535.0).contains(&word));
        clock|((word as u64)<<(at*16))
    });
    assert!(observed_utc.abs_diff(now)<1000,"derived status must retain exact backend observation UTC");
    let signal=rows.iter().find(|row|row.data_type=="IGNITER_SENSE_SIGNAL").unwrap();
    assert_eq!(signal.timestamp_ms,now as i64-20);
    assert_eq!(signal.values[0],Some(2048.0));
    assert!((signal.values[1].unwrap()-1.6504).abs()<0.001);
    assert_eq!(signal.values[2],None,"no unsupported amperes may be synthesized");
    assert_eq!(&signal.values[3..],&[Some(65534.0),Some(65535.0),Some(47768.0),Some(65244.0),Some(65533.0),Some(65535.0)]);
    let mut raw=false;let mut stored_signal=false;
    while let Ok(item)=db_rx.try_recv(){
        if let DbQueueItem::Write(DbWrite::Telemetry{data_type,values_json,payload_json,source_timestamp_ms,..})=item {
            if data_type=="AB_IGNITER_SENSE" {
                assert!(values_json.is_none());assert_eq!(serde_json::from_str::<Vec<u8>>(&payload_json).unwrap(),payload);raw=true;
            }else if data_type=="IGNITER_SENSE_SIGNAL" {
                let values:Vec<Option<f32>>=serde_json::from_str(&values_json.unwrap()).unwrap();
                assert_eq!(values[2],None);assert_eq!(source_timestamp_ms,Some(now as i64-20));stored_signal=true;
            }
        }
    }
    assert!(raw&&stored_signal,"recording must retain exact wire and measured rows");
    let duplicate=make("AB");assert_ne!(duplicate.packet_id(),original.packet_id());
    let rows=handle_packet(&state,&db_tx,&overflow,duplicate).await;
    assert!(rows.iter().all(|row|row.data_type!="IGNITER_SENSE_SIGNAL"),"new packet nonce is not a new ADC sample");
    assert!(handle_packet(&state,&db_tx,&overflow,make("RF")).await.is_empty());
    *state.state.lock().unwrap()=FlightState::Launch;
    let mut fault=words;fault[1]|=16;fault[4]=1;fault[5]+=50;
    let packet=Packet::new(crate::telemetry_schema::data_type("AB_IGNITER_SENSE"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],"AB",now,fault.into_iter().flat_map(u32::to_le_bytes).collect::<Vec<_>>().into()).unwrap();
    let rows=handle_packet(&state,&db_tx,&overflow,packet).await;
    assert_eq!(rows.iter().find(|row|row.data_type=="IGNITER_SENSE_STATUS").unwrap().values[0],Some(3.0));
    assert_eq!(*state.state.lock().unwrap(),FlightState::Launch);
    assert!(commands.try_recv().is_err(),"read-only sense/ADC errors emit no commands");
}
