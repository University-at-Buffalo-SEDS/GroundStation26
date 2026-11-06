use super::*;
#[tokio::test]
async fn recovery_ignores_continuous_unrelated_sdk_and_software_traffic_but_retains_controller_owner(){
 let state=crate::state::tests::test_app_state().await;let router=sedsnet::router::Router::new(Default::default());
 let token=Token::capture(&state,77);let activity=&state.transport_tx_activity;let mut held=Vec::new();
 for _ in 0..100 {
  let pkt=Packet::new(crate::telemetry_schema::data_type("KG50"),&[crate::telemetry_schema::endpoint("GROUND_STATION")],"DAQ",123,Arc::from([0u8;4])).unwrap();
  let bytes=sedsnet::wire_format::pack_packet(&pkt).to_vec();
  activity.try_enqueue_owned(crate::telemetry_task::is_recovery_control_frame(&bytes),bytes,|b|{held.push(b);Ok(())}).unwrap();
  router.try_tx_queue(pkt).unwrap();
 }
 assert!(state.has_pending_transport_tx());assert!(router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("KG50")));
 assert!(!sdk_pending(&state,&router));wait_sdk(&state,&router,token).await.unwrap();
 let packet=Packet::new(crate::telemetry_schema::data_type("VALVE_COMMAND"),&[crate::telemetry_schema::endpoint("VALVE_BOARD")],"GS",124,Arc::from(&b"\x00"[..])).unwrap();
 let bytes=sedsnet::wire_format::pack_packet(&packet).to_vec();let mut command=None;
 activity.try_enqueue_owned(true,bytes,|b|{command=Some(b);Ok(())}).unwrap();assert!(sdk_pending(&state,&router));
 let bytes=command.take().unwrap();assert!(sdk_pending(&state,&router),"queue pop is not a drained physical write");
 activity.complete_frame(&bytes);assert!(!sdk_pending(&state,&router));
 state.operator_queue.advance_relief_epoch(token.relief_epoch);assert!(wait_sdk(&state,&router,token).await.is_err());
 let token=Token::capture(&state,77);state.manual_abort_intent.request_untimed();assert!(wait_sdk(&state,&router,token).await.is_err());
 for bytes in &held{activity.complete_frame(bytes);}
}
