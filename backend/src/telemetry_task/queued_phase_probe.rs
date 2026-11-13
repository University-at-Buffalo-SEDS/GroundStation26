
//! Private test-only observation/gate before the actual bulk processing entry.
use crate::{state::AppState,types::FlightState};
use sedsnet::packet::Packet;
use std::sync::{Arc,Mutex,atomic::{AtomicU64,Ordering}};
use tokio::sync::Notify;
pub(crate) struct Hold {
 pub(crate) entered:Notify,pub(crate) released:Notify,pub(crate) processed:Notify,
 pub(crate) ingress_generation:AtomicU64,pub(crate) ingress_utc:AtomicU64,
 pub(crate) entry_generation:AtomicU64,pub(crate) carried_generation:AtomicU64,pub(crate) processed_phase:AtomicU64,
}
struct Entry {key:usize,id:u64,hold:Arc<Hold>}
static HOLDS:Mutex<Vec<Entry>>=Mutex::new(Vec::new());
pub(crate) fn install(state:&Arc<AppState>,id:u64)->Arc<Hold>{
 let hold=Arc::new(Hold{entered:Notify::new(),released:Notify::new(),processed:Notify::new(),
  ingress_generation:AtomicU64::new(0),ingress_utc:AtomicU64::new(0),entry_generation:AtomicU64::new(0),carried_generation:AtomicU64::new(0),processed_phase:AtomicU64::new(255)});
 HOLDS.lock().unwrap().push(Entry{key:Arc::as_ptr(state)as usize,id,hold:hold.clone()});hold
}
pub(crate) fn observe_sdk_ingress(state:&Arc<AppState>,packet:&Packet,generation:u64)->bool{
 let hold={HOLDS.lock().unwrap().iter().find(|e|e.key==Arc::as_ptr(state)as usize&&e.id==packet.packet_id()).map(|e|e.hold.clone())};
 if let Some(h)=hold {h.ingress_generation.store(generation,Ordering::Release);
  h.ingress_utc.store(packet.timestamp(),Ordering::Release);true}else{false}
}
pub(crate) async fn before_bulk_entry(state:&Arc<AppState>,packet:&Packet,generation:u64){
 let hold={HOLDS.lock().unwrap().iter().find(|e|e.key==Arc::as_ptr(state)as usize&&e.id==packet.packet_id()).map(|e|e.hold.clone())};
 if let Some(h)=hold {h.carried_generation.store(generation,Ordering::Release);h.entered.notify_one();h.released.notified().await;
  h.entry_generation.store(crate::gse::automatic_generation(state),Ordering::Release);}
}
pub(crate) fn after_bulk_entry(state:&Arc<AppState>,id:u64){
 let hold={let mut entries=HOLDS.lock().unwrap();entries.iter().position(|e|e.key==Arc::as_ptr(state)as usize&&e.id==id).map(|i|entries.remove(i).hold)};
 if let Some(h)=hold {let phase:FlightState=state.local_flight_state_snapshot();h.processed_phase.store(phase as u64,Ordering::Release);h.processed.notify_one();}
}

pub(crate) fn withdraw_uninjected(state:&Arc<AppState>,id:u64) {
 HOLDS.lock().unwrap().retain(|e|e.key!=Arc::as_ptr(state)as usize||e.id!=id);
}
