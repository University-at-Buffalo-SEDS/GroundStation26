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
 assert!(!sdk_pending(&state,&router));wait_sdk(&state,&router,token,QuiesceScope::All).await.unwrap();
 let packet=Packet::new(crate::telemetry_schema::data_type("VALVE_COMMAND"),&[crate::telemetry_schema::endpoint("VALVE_BOARD")],"GS",124,Arc::from(&b"\x00"[..])).unwrap();
 let bytes=sedsnet::wire_format::pack_packet(&packet).to_vec();let mut command=None;
 activity.try_enqueue_owned(true,bytes,|b|{command=Some(b);Ok(())}).unwrap();assert!(sdk_pending(&state,&router));
 let bytes=command.take().unwrap();assert!(sdk_pending(&state,&router),"queue pop is not a drained physical write");
 activity.complete_frame(&bytes);assert!(!sdk_pending(&state,&router));
 state.operator_queue.advance_relief_epoch(token.relief_epoch);assert!(wait_sdk(&state,&router,token,QuiesceScope::All).await.is_err());
 let token=Token::capture(&state,77);state.manual_abort_intent.request_untimed();assert!(wait_sdk(&state,&router,token,QuiesceScope::All).await.is_err());
 for bytes in &held{activity.complete_frame(bytes);}
}
/// Station router with a radio side and an umbilical side that both refuse
/// every frame, so whatever is queued stays undelivered.
fn stalled_router() -> (sedsnet::router::Router, usize, usize) {
 let router=sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
 let opts=sedsnet::router::RouterSideOptions{reliable_enabled:true,..Default::default()};
 let radio=router.add_side_packed_with_options("rocket_comms",|_|Err(sedsnet::TelemetryError::Io("side tx busy")),opts.clone());
 let umbilical=router.add_side_packed_with_options("umbilical_comms",|_|Err(sedsnet::TelemetryError::Io("side tx busy")),opts);
 let announce=|board:&str,endpoints:&[&str]|sedsnet::discovery::build_discovery_announce(board,1,
  &endpoints.iter().map(|e|crate::telemetry_schema::endpoint(e)).collect::<Vec<_>>()).unwrap();
 router.rx_from_side(&announce("FC",&["FLIGHT_CONTROLLER","ABORT"]),radio).unwrap();
 router.rx_from_side(&announce("VB",&["VALVE_BOARD","ABORT"]),umbilical).unwrap();
 router.process_all_queues_with_timeout(0).unwrap();
 (router,radio,umbilical)
}
fn queue(router:&sedsnet::router::Router,ty:&str,endpoint:&str,payload:&[u8]){
 router.tx_queue(Packet::new(crate::telemetry_schema::data_type(ty),&[crate::telemetry_schema::endpoint(endpoint)],"GS",1,Arc::from(payload)).unwrap()).unwrap();
 let _=router.process_all_queues_with_timeout(0);
 assert!(router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(ty)),"{ty} stays undelivered");
}
#[tokio::test]
async fn av_bay_backlog_does_not_hold_a_fill_only_reset(){
 let state=crate::state::tests::test_app_state().await;let (router,_,_)=stalled_router();
 queue(&router,"FLIGHT_COMMAND","FLIGHT_CONTROLLER",&[1]);
 // A controller frame still owned by the radio writer, too.
 let bytes=sedsnet::wire_format::pack_packet(&Packet::new(crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
  &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],"GS",2,Arc::from(&[1u8][..])).unwrap()).to_vec();
 let mut held=None;state.radio_tx_activity.try_enqueue_owned(true,bytes,|b|{held=Some(b);Ok(())}).unwrap();
 assert_eq!(sdk_pending_reason(&state,&router,QuiesceScope::FillOnly),None,"nothing is in flight to a fill board");
 let token=Token::capture(&state,77);
 let started=WallInstant::now();
 wait_sdk(&state,&router,token,QuiesceScope::FillOnly).await.unwrap();
 assert!(started.elapsed()<Duration::from_secs(1));
 // A reset that targets the AV bay must still wait, and says why.
 let reason=sdk_pending_reason(&state,&router,QuiesceScope::All).unwrap();
 assert!(reason.contains("radio"),"{reason}");
 state.radio_tx_activity.complete_frame(&held.unwrap());
 let reason=sdk_pending_reason(&state,&router,QuiesceScope::All).unwrap();
 assert!(reason.contains("FLIGHT_COMMAND") && reason.contains("AV bay"),"{reason}");
}
#[tokio::test]
async fn pending_abort_still_holds_a_fill_only_reset(){
 let state=crate::state::tests::test_app_state().await;let (router,_,_)=stalled_router();
 queue(&router,"ABORT","VALVE_BOARD",b"Manual Abort Command Issued");
 let reason=sdk_pending_reason(&state,&router,QuiesceScope::FillOnly).unwrap();
 assert!(reason.contains("ABORT"),"{reason}");
 let reason=sdk_pending_reason(&state,&router,QuiesceScope::All).unwrap();
 assert!(reason.contains("ABORT"),"{reason}");
}
#[tokio::test]
async fn umbilical_writer_ownership_holds_every_scope(){
 let state=crate::state::tests::test_app_state().await;let router=sedsnet::router::Router::new(Default::default());
 let bytes=sedsnet::wire_format::pack_packet(&Packet::new(crate::telemetry_schema::data_type("VALVE_COMMAND"),
  &[crate::telemetry_schema::endpoint("VALVE_BOARD")],"GS",3,Arc::from(&b"\x00"[..])).unwrap()).to_vec();
 let mut held=None;state.transport_tx_activity.try_enqueue_owned(true,bytes,|b|{held=Some(b);Ok(())}).unwrap();
 for scope in [QuiesceScope::FillOnly,QuiesceScope::All] {
  assert!(sdk_pending_reason(&state,&router,scope).unwrap().contains("umbilical"));
 }
 state.transport_tx_activity.complete_frame(&held.unwrap());
 assert_eq!(sdk_pending_reason(&state,&router,QuiesceScope::FillOnly),None);
}
/// Stalled router where only the named boards listen for FLIGHT_STATE.
fn flight_state_router(listeners:&[(&str,bool)])->sedsnet::router::Router {
 let router=sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
 let opts=sedsnet::router::RouterSideOptions{reliable_enabled:true,..Default::default()};
 let radio=router.add_side_packed_with_options("rocket_comms",|_|Err(sedsnet::TelemetryError::Io("side tx busy")),opts.clone());
 let umbilical=router.add_side_packed_with_options("umbilical_comms",|_|Err(sedsnet::TelemetryError::Io("side tx busy")),opts);
 for (board,on_radio) in listeners {
  let announce=sedsnet::discovery::build_discovery_announce(board,1,&[crate::telemetry_schema::endpoint("FLIGHT_STATE")]).unwrap();
  router.rx_from_side(&announce,if *on_radio{radio}else{umbilical}).unwrap();
 }
 router.process_all_queues_with_timeout(0).unwrap();
 router
}
#[tokio::test]
async fn phase_publication_owed_only_to_the_av_bay_does_not_hold_a_fill_reset(){
 let state=crate::state::tests::test_app_state().await;
 let router=flight_state_router(&[("FC",true),("RF",true)]);
 queue(&router,"FLIGHT_STATE","FLIGHT_STATE",&[1]);
 let rows=router.export_reliable_pending_stats();
 let owners=rows.iter().flat_map(|row|row.pending_destinations.iter().map(|owner|pending_destination_board(&router,*owner))).collect::<Vec<_>>();
 assert!(!owners.is_empty() && owners.iter().all(|board|matches!(board,Some(Board::FlightComputer|Board::RFBoard))),
  "SEDSnet destination keys still name the boards: {owners:?}");
 assert_eq!(sdk_pending_reason(&state,&router,QuiesceScope::FillOnly),None);
 assert!(sdk_pending_reason(&state,&router,QuiesceScope::All).unwrap().contains("FLIGHT_STATE"));
}
#[tokio::test]
async fn phase_publication_owed_to_a_fill_board_holds_a_fill_reset(){
 let state=crate::state::tests::test_app_state().await;
 let router=flight_state_router(&[("FC",true),("VB",false)]);
 queue(&router,"FLIGHT_STATE","FLIGHT_STATE",&[1]);
 let rows=router.export_reliable_pending_stats();
 assert!(rows.iter().flat_map(|row|row.pending_destinations.iter()).any(|owner|pending_destination_board(&router,*owner)==Some(Board::ValveBoard)),
  "the Valve Board copy is attributed");
 let reason=sdk_pending_reason(&state,&router,QuiesceScope::FillOnly).unwrap();
 assert!(reason.contains("FLIGHT_STATE"),"{reason}");
}
