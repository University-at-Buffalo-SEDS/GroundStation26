//! Initial read-only admission only: no mutating retry or UTC renewal.
use super::*;
use std::cell::Cell;
fn busy() -> Response {
    super::super::rejected_query_offer(sedsnet::TelemetryError::Io("router core busy"))
}
#[tokio::test]
async fn prepare_core_busy_retries_only_nonowned_offer_then_succeeds() {
    let calls=Cell::new(0);
    admit_prepare_query(||Ok(()),|| {
        calls.set(calls.get()+1);
        std::future::ready(if calls.get()<3 {busy()}else{StatusCode::OK.into_response()})
    }).await.unwrap();
    assert_eq!(calls.get(),3);
    let calls=Cell::new(0);
    assert!(admit_prepare_query(||Ok(()),|| {
        calls.set(calls.get()+1);
        std::future::ready(super::super::rejected_query_offer(sedsnet::TelemetryError::Io("other transport failure")))
    }).await.is_err());
    assert_eq!(calls.get(),1,"other SDK failures never retry");
}
#[tokio::test]
async fn prepare_persistent_core_busy_has_bounded_deadline() {
    let began=WallInstant::now();let calls=Cell::new(0);
    assert!(admit_prepare_query(||Ok(()),|| {calls.set(calls.get()+1);std::future::ready(busy())}).await.is_err());
    assert!(began.elapsed()>=Duration::from_millis(290));
    assert!(began.elapsed()<Duration::from_secs(1));
    assert!(calls.get()>1&&calls.get()<=31);
}
#[tokio::test]
async fn prepare_core_busy_never_renews_changed_stop_or_reset_selection() {
    let state=crate::state::tests::test_app_state().await;
    let token=Token::capture(&state,77);let calls=Cell::new(0);
    assert!(admit_prepare_query(||if origin_current(&state,token){Ok(())}else{Err("changed")},|| {
        calls.set(calls.get()+1);state.manual_abort_intent.request_untimed();std::future::ready(busy())
    }).await.is_err());
    assert_eq!(calls.get(),1);
    let state=crate::state::tests::test_app_state().await;
    let selection=super::super::scope::Selection::capture(&state,&[]).unwrap();let calls=Cell::new(0);
    assert!(admit_prepare_query(||selection.current(&state),|| {
        calls.set(calls.get()+1);state.board_abort.observe_abort(Board::ValveBoard,WallInstant::now(),77);std::future::ready(busy())
    }).await.is_err());
    assert_eq!(calls.get(),1);
}
#[test]
fn actual_sdk_core_contention_marks_only_documented_nonadmission() {
    use std::sync::{Condvar,atomic::AtomicBool};
    let paused=Arc::new((std::sync::Mutex::new(false),Condvar::new()));
    struct Release(Arc<(std::sync::Mutex<bool>,Condvar)>);
    impl Drop for Release {fn drop(&mut self){*self.0.0.lock().unwrap()=true;self.0.1.notify_all();}}
    let release=Release(paused.clone());let (entered,reached)=std::sync::mpsc::channel();
    let armed=Arc::new(AtomicBool::new(false));let clock_armed=armed.clone();let clock_paused=paused.clone();
    let router=Arc::new(sedsnet::router::Router::new_with_clock(Default::default(),Box::new(move|| {
        if clock_armed.load(Ordering::SeqCst)&&std::thread::current().name()==Some("prepare-query-core-holder") {
            let _=entered.send(());let mut done=clock_paused.0.lock().unwrap();while !*done {done=clock_paused.1.wait(done).unwrap();}
        }10
    })));
    let ty=crate::telemetry_schema::data_type("FLIGHT_STATE");router.enable_managed_variable(ty).unwrap();
    let packet=Packet::new(ty,&[crate::telemetry_schema::endpoint("FLIGHT_STATE")],"GS",1,Arc::from([0u8])).unwrap();
    let owner=router.clone();let seed=packet.clone();armed.store(true,Ordering::SeqCst);
    let thread=std::thread::Builder::new().name("prepare-query-core-holder".into()).spawn(move||owner.seed_managed_variable(seed)).unwrap();
    reached.recv_timeout(Duration::from_secs(2)).unwrap();
    let reason=router.try_tx_queue(packet.clone()).unwrap_err();
    assert!(matches!(reason,sedsnet::TelemetryError::Io("router core busy")));
    let response=super::super::rejected_query_offer(reason);
    assert!(response.extensions().get::<super::super::QueryAdmissionCoreBusy>().is_some());
    drop(release);thread.join().unwrap().unwrap();
    router.try_tx_queue(packet).unwrap();
    assert!(super::super::rejected_query_offer(sedsnet::TelemetryError::Io("other failure")).extensions().get::<super::super::QueryAdmissionCoreBusy>().is_none());
}
