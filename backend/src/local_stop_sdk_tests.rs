
use super::*;
use crate::{types::FlightState,operator_queue::Token};
use sedsnet::router::RouterConfig;
async fn host() -> (Arc<AppState>,Arc<Router>) {
 let state=crate::state::tests::test_app_state().await;
 let router=Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
 state.topology_router.set(router.clone()).unwrap();
 state.set_preexisting_flight_state_for_test(FlightState::Recovery);
 (state,router)
}
#[tokio::test]
async fn local_stop_sdk_current_owner_newer_epoch_and_stale_generation() {
 let (state,router)=host().await;
 state.manual_abort_intent.request();let mut old=state.manual_abort_intent.try_claim().unwrap();
 let receipt=publish_claim_receipt(&state,&router,&mut old).unwrap().unwrap();
 state.manual_abort_intent.request();
 assert!(!crate::gse::commit_local_manual_stop(&state,&old,&receipt));
 assert_eq!(state.local_flight_state_snapshot(),FlightState::Recovery);
 old.accepted();
 let mut new=state.manual_abort_intent.try_claim().unwrap();
 let newer=publish_claim_receipt(&state,&router,&mut new).unwrap().unwrap();
 assert!(!crate::gse::commit_local_manual_stop(&state,&new,&receipt),"old packet receipt cannot borrow a new explicit epoch");
 assert!(crate::gse::commit_local_manual_stop(&state,&new,&newer));
 assert_eq!(state.local_flight_state_snapshot(),FlightState::Aborted);
 new.accepted();
 // A completed generation boundary cannot be replayed through a later claim.
 let generation=crate::gse::automatic_generation(&state);
 crate::gse::reset_generation::invalidate_capture_epoch(&state,generation).unwrap();
 state.set_preexisting_flight_state_for_test(FlightState::Startup);
 state.manual_abort_intent.request();let next=state.manual_abort_intent.try_claim().unwrap();
 assert!(!crate::gse::commit_local_manual_stop(&state,&next,&newer));
 assert_eq!(state.local_flight_state_snapshot(),FlightState::Startup);
}
#[tokio::test]
async fn local_stop_sdk_closed_recovery_blocks_begin_but_still_coherently_stops() {
 let (state,router)=host().await;
 let owner=45;let token=Token::capture(&state,owner);
 crate::gse::reset_generation::close_operator_admission(&state,token.generation,owner).unwrap();
 let journal=std::env::temp_dir().join(format!("gs-local-stop-closed-{}.json",rand::random::<u128>()));
 *state.launch_history.lock().unwrap()=crate::launch_history::Store::open(journal,false,None,10000,0).unwrap();
 let fence={let mut history=state.launch_history.lock().unwrap();
  history.inhibit_for_recovery().unwrap();history.close_for_recovery(owner).unwrap()};
 state.manual_abort_intent.request_untimed();let mut claim=state.manual_abort_intent.try_claim().unwrap();
 assert_eq!(crate::gse::reset_generation::advance_verified_recovery(&state,token,&fence),Err("Manual Stop superseded recovery"));
 assert_eq!(crate::gse::automatic_generation(&state),token.generation);
 assert!(publish_local_claim(&state,&router,&mut claim).unwrap());
 assert_eq!(state.local_flight_state_snapshot(),FlightState::Aborted);
 assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire),owner);
 assert_eq!(claim.wire(),Some(WirePolicy::Untimed),"authenticated secondary-original intent remains untimed");
 claim.accepted();
}
#[tokio::test]
async fn local_stop_sdk_queue_refusal_has_no_host_phase_effect() {
 let (state,router)=host().await;
 let mut full=false;
 for _ in 0..10000 {
  if crate::telemetry_task::queue_manual_abort_wire(&router,WirePolicy::Untimed).is_err(){full=true;break;}
 }
 assert!(full,"real finite SDK backpressure must be reached");
 state.manual_abort_intent.request();let mut claim=state.manual_abort_intent.try_claim().unwrap();
 assert!(publish_local_claim(&state,&router,&mut claim).is_err());
 assert_eq!(state.local_flight_state_snapshot(),FlightState::Recovery);
 assert!(!state.abort_indicator_latched());
 drop(claim);assert!(state.manual_abort_intent.pending_epoch().is_some());
}
#[tokio::test]
async fn local_stop_sdk_foreign_owner_and_shutdown_do_not_commit() {
 let (state,router)=host().await;
 state.manual_abort_intent.request();let mut claim=state.manual_abort_intent.try_claim().unwrap();
 let receipt=publish_claim_receipt(&state,&router,&mut claim).unwrap().unwrap();
 let (foreign,_)=host().await;
 assert!(!crate::gse::commit_local_manual_stop(&foreign,&claim,&receipt));
 assert_eq!(foreign.local_flight_state_snapshot(),FlightState::Recovery);
 state.request_shutdown();
 assert!(!crate::gse::commit_local_manual_stop(&state,&claim,&receipt));
 assert_eq!(state.local_flight_state_snapshot(),FlightState::Recovery);
}
