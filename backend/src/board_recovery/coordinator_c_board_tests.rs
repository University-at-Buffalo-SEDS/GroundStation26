//! Opt-in real SDK packet admission → actual AB/VB application C protocol.
//! GPIO/NVM/ThreadX are inert fixtures; no hardware or physical output claim.
use super::*;
use std::{io::{BufRead,BufReader,Write},process::{Child,ChildStdin,ChildStdout,Command,Stdio},sync::Mutex};
struct Bridge { child:Child,input:ChildStdin,output:BufReader<ChildStdout>,now:u64 }
impl Bridge {
 fn start(board:Board)->Self {
  let dir=std::env::var("GS_RECOVERY_C_BRIDGES").expect("Build backend/tests/build_recovery_c_bridge.py and set GS_RECOVERY_C_BRIDGES");
  let mut child=Command::new(std::path::Path::new(&dir).join(if board==Board::ActuatorBoard{"ab"}else{"vb"}))
   .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().unwrap();
  let input=child.stdin.take().unwrap();let mut output=BufReader::new(child.stdout.take().unwrap());
  let mut ready=String::new();output.read_line(&mut ready).unwrap();assert_eq!(ready.trim(),"READY");
  Self{child,input,output,now:0}
 }
 fn packet(&mut self,p:&Packet)->Vec<u8> {
  self.now=self.now.max(p.timestamp());
  let hex=p.payload().iter().map(|b|format!("{b:02x}")).collect::<String>();
  writeln!(self.input,"{} {} {}",self.now,p.timestamp(),hex).unwrap();self.input.flush().unwrap();
  let mut line=String::new();self.output.read_line(&mut line).unwrap();let line=line.trim();assert_eq!(line.len(),80,"actual C bridge response");
  (0..40).map(|i|u8::from_str_radix(&line[i*2..i*2+2],16).unwrap()).collect()
 }
}
impl Drop for Bridge {fn drop(&mut self){let _=self.child.kill();let _=self.child.wait();}}
fn sdk(board:Board)->(Arc<sedsnet::router::Router>,Arc<Mutex<Vec<Vec<u8>>>>) {
 let bridge=Arc::new(Mutex::new(Bridge::start(board)));let replies=Arc::new(Mutex::new(Vec::new()));let out=replies.clone();
 let router=Arc::new(sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([
  sedsnet::router::EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint(endpoint(board)),move|p|{
   out.lock().unwrap().push(bridge.lock().unwrap().packet(p));Ok(())
  })]).with_sender("GS")));
 router.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(1_793_945_600_000)).unwrap();
 (router,replies)
}
fn deliver(router:&sedsnet::router::Router,replies:&Mutex<Vec<Vec<u8>>>,packet:Packet)->Vec<u8> {
 let before=replies.lock().unwrap().len();router.try_tx_queue(packet).unwrap();
 for _ in 0..16{router.process_all_queues_with_timeout(0).unwrap();if replies.lock().unwrap().len()>before{break;}}
 let rows=replies.lock().unwrap();assert_eq!(rows.len(),before+1,"actual SDK handler must execute once");rows.last().unwrap().clone()
}
fn query(router:&sedsnet::router::Router,board:Board,nonce:u64)->Packet {
 let mut wire=[0u8;32];wire[0]=1;wire[4..12].copy_from_slice(&nonce.to_le_bytes());wire[24..32].copy_from_slice(&77u64.to_le_bytes());
 Packet::new(crate::telemetry_schema::data_type(QUERY_TYPE),&[crate::telemetry_schema::endpoint(endpoint(board))],"GS",
  crate::system_clock::required_control_network_utc_ms(router).unwrap(),Arc::from(wire)).unwrap()
}
fn target(board:Board,row:Observation)->Target {
 Target{board,desired:0,context:row.value.clone(),prepared_query:row,intent:None,original_first:None,uncertain_first:None,first_binding:None}
}
#[test]
#[ignore="requires sibling firmware C sources and prebuilt inert bridges; run qualification explicitly"]
fn actual_sdk_c_completed_recovery_preserves_query_before_immutable_confirm() {
 for board in [Board::ActuatorBoard,Board::ValveBoard] {
  for old_order in [true,false] {
   let(router,replies)=sdk(board);let at=Stamp::now();let a=query(&router,board,101);
   let row=Readback::decode(&deliver(&router,&replies,a.clone())).unwrap();assert_eq!(row.state,Some(7));assert_eq!(row.result,0);
   let mut original=target(board,Observation{value:row,received:at,observed_after:at});
   std::thread::sleep(Duration::from_millis(2));let b=super::super::exchange::ConfirmBoundary::capture(&router).unwrap();assert!(b.utc()>a.timestamp());
   if old_order {std::thread::sleep(Duration::from_millis(2));let c=query(&router,board,102);assert!(c.timestamp()>b.utc());deliver(&router,&replies,c);}
   let prepared=normal_pre_exchange_query(&original,Stamp::now()).unwrap().unwrap();
   let mut exchange=super::super::exchange::Exchange::prepare_at(board,&prepared,103,0,Stamp::now(),router.clone(),b).unwrap();
   let request=exchange.packet(Stamp::now()).unwrap().clone();assert_eq!(request.timestamp(),b.utc());
   if !old_order {
    for offset in [12usize,20,24] { // boot, epoch, issuer cannot be rebound by a fresh SDK ID.
     let mut wire=request.payload().to_vec();wire[offset]=wire[offset].wrapping_add(1);
     let wrong=Packet::new(request.data_type(),request.endpoints(),request.sender(),request.timestamp(),Arc::from(wire)).unwrap();
     assert_eq!(deliver(&router,&replies,wrong)[1],4,"actual C stale context must leave original proof usable");
    }
   }
   let response=deliver(&router,&replies,request);
   assert_eq!(response[1],if old_order {4}else{0},"actual C protocol, not a Rust board model");
   let response=Packet::new(crate::telemetry_schema::data_type(STATUS_TYPE),&[crate::telemetry_schema::endpoint("GROUND_STATION")],board.sender_id(),b.utc(),Arc::from(response)).unwrap();
   assert_eq!(exchange.observe(&response,Stamp::now()).unwrap(),if old_order {super::super::exchange::Outcome::Rejected(4)}else{super::super::exchange::Outcome::Applied});
   if !old_order {let final_row=Readback::decode(&deliver(&router,&replies,query(&router,board,104))).unwrap();assert_eq!(final_row.state,Some(0));assert_eq!(final_row.epoch,original.context.epoch+1);}
   let before=replies.lock().unwrap().len();original.prepared_query.observed_after.wall-=Duration::from_secs(6);
   assert!(normal_pre_exchange_query(&original,Stamp::now()).is_err());assert_eq!(replies.lock().unwrap().len(),before,"expired proof sends no SDK packet");
  }
 }
}
#[tokio::test]
async fn external_query_fence_blocks_sealed_operation_and_reopens_only_terminal() {
 let state=crate::state::tests::test_app_state().await;
 assert_eq!(admit_external_query(&state,||1),Some(1));
 state.board_recovery.coordinator.0.lock().unwrap().operation=Some(json!({"completed":false}));
 assert!(admit_external_query(&state,||panic!("new query would overwrite sealed proof")).is_none());
 state.board_recovery.coordinator.0.lock().unwrap().operation=Some(json!({"completed":true}));
 assert_eq!(admit_external_query(&state,||2),Some(2));
}

#[tokio::test]
async fn query_sdk_offer_and_confirm_seal_share_one_serialized_owner() {
 let state=crate::state::tests::test_app_state().await;
 let (entered_tx,entered_rx)=std::sync::mpsc::channel();let (release_tx,release_rx)=std::sync::mpsc::channel();
 let work=state.clone();let task=tokio::task::spawn_blocking(move||admit_external_query(&work,||{
  entered_tx.send(()).unwrap();release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
 }));
 entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
 assert!(state.board_recovery.coordinator.0.try_lock().is_err(),"Confirm cannot seal while an external SDK offer owns the query gate");
 release_tx.send(()).unwrap();assert!(task.await.unwrap().is_some());
 assert!(state.board_recovery.coordinator.0.try_lock().is_ok());
}
