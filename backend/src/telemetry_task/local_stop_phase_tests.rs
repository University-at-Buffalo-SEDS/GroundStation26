
use super::*;
use std::sync::Mutex;
struct Pause { entered:tokio::sync::Notify, released:tokio::sync::Notify }
static NEXT:Mutex<Vec<(usize,bool,Arc<Pause>)>>=Mutex::new(Vec::new());
pub(super) async fn after_original_entry(state:&Arc<AppState>) { pause_at(state,false).await; }
pub(super) async fn after_phase_snapshot(state:&Arc<AppState>) { pause_at(state,true).await; }
async fn pause_at(state:&Arc<AppState>,after_snapshot:bool) {
 let key=Arc::as_ptr(state) as usize;
 let pause={let mut slot=NEXT.lock().unwrap();
  slot.iter().position(|(owner,stage,_)|*owner==key&&*stage==after_snapshot).map(|at|slot.remove(at).2)};
 if let Some(p)=pause {p.entered.notify_one();p.released.notified().await;}
}
async fn delayed_case(reset_boundary:bool) {
 let state=crate::state::tests::test_app_state().await;
 let router=Arc::new(Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS")));
 state.topology_router.set(router.clone()).unwrap();
 for board in Board::ALL {state.mark_board_seen(board.sender_id(),get_current_timestamp_ms());}
 state.set_preexisting_flight_state_for_test(FlightState::Recovery);
 let clock=state.launch_clock_snapshot();
 let pause=Arc::new(Pause{entered:tokio::sync::Notify::new(),released:tokio::sync::Notify::new()});
 NEXT.lock().unwrap().push((Arc::as_ptr(&state)as usize,!reset_boundary,pause.clone()));
 let (tx,mut rx)=mpsc::channel(16);let cloned=state.clone();
 let packet=Packet::new(crate::telemetry_schema::data_type("FLIGHT_STATE"),
  &[crate::telemetry_schema::endpoint("GROUND_STATION")],"FC",1234,Arc::from([FlightState::Ascent as u8])).unwrap();
 let task=tokio::spawn(async move {handle_packet(&cloned,&tx,&DbOverflow,packet).await});
 tokio::time::timeout(Duration::from_secs(2),pause.entered.notified()).await.unwrap();
 if reset_boundary {
  // Test-only boundary model. Full actual coordinator publication is separately
  // exercised by the HTTP/SDK Stop→Reset regression, not claimed by this control.
  crate::gse::reset_generation::invalidate_capture_epoch(&state,crate::gse::automatic_generation(&state)).unwrap();
  state.set_preexisting_flight_state_for_test(FlightState::Startup);
 } else {
  state.manual_abort_intent.request();let mut claim=state.manual_abort_intent.try_claim().unwrap();
  assert!(crate::manual_abort_utc::publish_local_claim(&state,&router,&mut claim).unwrap());
  claim.accepted();assert_eq!(state.local_flight_state_snapshot(),FlightState::Aborted);
 }
 pause.released.notify_one();tokio::time::timeout(Duration::from_secs(2),task).await.unwrap().unwrap();
 assert_eq!(state.local_flight_state_snapshot(),if reset_boundary{FlightState::Startup}else{FlightState::Aborted});
 assert_eq!(state.launch_clock_snapshot(),clock,"rejected delayed FC phase must not mutate T0");
 while let Ok(item)=rx.try_recv(){
  assert!(!matches!(item,crate::telemetry_db::DbQueueItem::Write(crate::telemetry_db::DbWrite::FlightState{..})),"rejected phase must not enter flight-state recording");
 }
}
#[tokio::test]
async fn local_stop_phase_stale_fc_snapshot_cannot_overwrite_admitted_stop(){delayed_case(false).await;}
#[tokio::test]
async fn local_stop_phase_original_fc_ingress_cannot_freshen_after_reset_boundary(){delayed_case(true).await;}
#[tokio::test]
async fn local_stop_phase_received_manual_and_automatic_are_not_local_aggregate_owners(){
 for automatic in [false,true] {
  let state=crate::state::tests::test_app_state().await;
  state.set_preexisting_flight_state_for_test(FlightState::Recovery);
  let (worker,drain)=crate::deferred_abort::spawn(state.clone());
  let payload:&[u8]=if automatic{b"SEDS_AUTO_ABORT_V1:remote"}else{b"SEDS_MANUAL_ABORT_V1:K"};
  let packet=Packet::new(crate::telemetry_schema::data_type("ABORT"),
   &[crate::telemetry_schema::endpoint("ABORT")],"VB",1234,Arc::from(payload)).unwrap();
  crate::deferred_abort::observe(&state,&packet,crate::gse::automatic_generation(&state));
  tokio::time::timeout(Duration::from_secs(2),async {
   while state.deferred_abort.manual_pending(){tokio::time::sleep(Duration::from_millis(2)).await;}
  }).await.unwrap();
  // Actual owner-drain waits for automatic observations as well as manual ones.
  let _=drain.send(());tokio::time::timeout(Duration::from_secs(2),worker).await.unwrap().unwrap();
  assert_eq!(state.local_flight_state_snapshot(),FlightState::Recovery);
  assert_eq!(state.manual_abort_intent.epoch(),0,"received packet cannot manufacture a local human intent");
 }
}

#[tokio::test]
async fn local_stop_phase_actual_rrls_accepts_primary_startup_after_abort() {
 const FLAG:&str="GS_LOCAL_STOP_RRLS_CONTROL";
 if std::env::var(FLAG).as_deref()!=Ok("1") {
  let name=format!("{}::local_stop_phase_actual_rrls_accepts_primary_startup_after_abort",module_path!());
  let exact=name.split_once("::").unwrap().1;
  let output=std::process::Command::new(std::env::current_exe().unwrap())
   .args(["--exact",exact,"--nocapture","--test-threads=1"])
   .env(FLAG,"1").env("GS_RRLS_ROLE","rrls").env("GS_STATION_ROLE","rrls").output().unwrap();
  assert!(output.status.success(),"actual RRLS role control failed: {} {}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
  assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored"));
  return;
 }
 assert!(crate::rrls::is_rrls());
 let state=crate::state::tests::test_app_state().await;
 let router=Arc::new(Router::new(sedsnet::router::RouterConfig::new([]).with_sender("RRLS")));
 state.topology_router.set(router).unwrap();
 for board in Board::ALL {state.mark_board_seen(board.sender_id(),get_current_timestamp_ms());}
 state.set_preexisting_flight_state_for_test(FlightState::Aborted);
 let (tx,_rx)=mpsc::channel(16);
 let packet=Packet::new(crate::telemetry_schema::data_type("FLIGHT_STATE"),
  &[crate::telemetry_schema::endpoint("GROUND_STATION")],"GS",1234,Arc::from([FlightState::Startup as u8])).unwrap();
 handle_packet(&state,&tx,&DbOverflow,packet).await;
 assert_eq!(state.local_flight_state_snapshot(),FlightState::Startup,"primary reset authority must survive the local Stop race fence");
}
