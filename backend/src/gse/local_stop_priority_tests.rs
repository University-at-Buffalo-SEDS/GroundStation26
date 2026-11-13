
use super::*;
use sedsnet::router::{Router,RouterConfig,RouterSideOptions};
use std::sync::{Arc,Mutex,mpsc as sync_mpsc};
#[tokio::test]
async fn local_stop_priority_actual_wire_precedes_dispatch_host_bookkeeping(){
 let state=crate::state::tests::test_app_state().await;
 let router=Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
 state.topology_router.set(router.clone()).unwrap();
 state.set_preexisting_flight_state_for_test(FlightState::Recovery);
 let captured=Arc::new(Mutex::new(Vec::new()));let output=captured.clone();
 let (sent,received)=sync_mpsc::channel();
 let side=router.add_side_packed_with_options("native-actual-stop-wire",move|wire|{
  if let Ok(packet)=sedsnet::wire_format::unpack_packet(wire)
   && packet.data_type()==crate::telemetry_schema::data_type("ABORT")
   && packet.payload().starts_with(b"SEDS_MANUAL_ABORT_V1:") {
    output.lock().unwrap().push((packet.packet_id(),packet.timestamp(),packet.payload().to_vec()));
    let _=sent.send(());
  }
  Ok(())
 },RouterSideOptions{reliable_enabled:false,..Default::default()});
 router.rx_from_side(&sedsnet::discovery::build_discovery_announce("VB",1,
  &[crate::telemetry_schema::endpoint("ABORT")]).unwrap(),side).unwrap();
 router.process_all_queues_with_timeout(0).unwrap();
 state.manual_abort_intent.request();let mut claim=state.manual_abort_intent.try_claim().unwrap();
 let gate=dispatch_gate();let worker_state=state.clone();let worker_router=router.clone();
 let worker=std::thread::spawn(move||{
  let admitted=crate::manual_abort_utc::publish_local_claim(&worker_state,&worker_router,&mut claim).unwrap();
  if admitted {crate::telemetry_task::flush_command_tx(&worker_router,"native main priority flush");claim.accepted();}
  admitted
 });
 let before_release=received.recv_timeout(std::time::Duration::from_secs(1)).is_ok();
 drop(gate);
 assert!(worker.join().unwrap());
 let all=captured.lock().unwrap();assert!(!all.is_empty(),"actual SDK original Stop must eventually reach transport callback");
 eprintln!("LOCAL_STOP_PRIORITY_WIRE {}",serde_json::json!({"before_dispatch_release":before_release,"logical_stop_packet_ids":all.iter().map(|r|r.0).collect::<Vec<_>>(),"after_release_count":all.len(),"scope":"actual publicSDKqueue/transportcodec callback; no physical radio or output claim"}));
 assert!(before_release,"manual Stop reached SDK ownership but its actual wire callback waited for ordinary DISPATCH bookkeeping");
}
/// DISPATCH_GATE guards no data. A panic while another path held it must not
/// stop a later manual Stop from reaching the wire and latching Aborted.
#[tokio::test]
async fn local_stop_dispatches_after_a_panic_poisoned_the_dispatch_gate(){
 // Own process: this poisons a process-wide gate on purpose.
 if crate::operator_queue::isolated_case(module_path!(),"local_stop_dispatches_after_a_panic_poisoned_the_dispatch_gate"){return;}
 let _=std::thread::spawn(||{let _held=dispatch_gate();panic!("panic while holding the dispatch gate");}).join();
 assert!(DISPATCH_GATE.is_poisoned());
 let state=crate::state::tests::test_app_state().await;
 let router=Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
 state.topology_router.set(router.clone()).unwrap();
 state.set_preexisting_flight_state_for_test(FlightState::Recovery);
 let captured=Arc::new(Mutex::new(0usize));let output=captured.clone();
 let side=router.add_side_packed_with_options("poisoned-gate-stop-wire",move|wire|{
  if let Ok(packet)=sedsnet::wire_format::unpack_packet(wire)
   && packet.data_type()==crate::telemetry_schema::data_type("ABORT")
   && packet.payload().starts_with(b"SEDS_MANUAL_ABORT_V1:") { *output.lock().unwrap()+=1; }
  Ok(())
 },RouterSideOptions{reliable_enabled:false,..Default::default()});
 router.rx_from_side(&sedsnet::discovery::build_discovery_announce("VB",1,
  &[crate::telemetry_schema::endpoint("ABORT")]).unwrap(),side).unwrap();
 router.process_all_queues_with_timeout(0).unwrap();
 state.manual_abort_intent.request();let mut claim=state.manual_abort_intent.try_claim().unwrap();
 let worker_state=state.clone();let worker_router=router.clone();
 let admitted=std::thread::spawn(move||{
  let admitted=crate::manual_abort_utc::publish_local_claim(&worker_state,&worker_router,&mut claim).unwrap();
  if admitted {crate::telemetry_task::flush_command_tx(&worker_router,"poisoned gate stop flush");claim.accepted();}
  admitted
 }).join().expect("Stop dispatch must not panic on a poisoned dispatch gate");
 assert!(admitted);
 assert!(*captured.lock().unwrap()>0,"manual Stop must reach the wire");
 assert_eq!(state.local_flight_state_snapshot(),FlightState::Aborted);
 assert!(state.abort_indicator_latched());
 // Ordinary operator admission keeps working too.
 assert!(capture_operator_origin(&state).is_some());
}
