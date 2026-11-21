//! Test-only actual clock producer/consumer. Fresh unpaired display nonce and
//! live Mesh session, not a signing-key or operator-authority fixture.
use super::*;

pub(crate) struct Fixture {
    primary: Runtime,
    observer: Runtime,
    clock: ClockRecord,
    store: crate::launch_history::Store,
    session: u32,
    query: Vec<u8>,
    pub(crate) query_received: bool,
    primary_outbox: crate::mission_sync::wire::NativeOutbox,
    observer_outbox: crate::mission_sync::wire::NativeOutbox,
    pub(crate) offered: bool,
    pub(crate) received: usize,
    pub(crate) applied: usize,
    pub(crate) original_reply: Vec<u8>,
}
impl Fixture {
    pub(crate) fn new(directory: &std::path::Path, session:u32, now:u64)->Self {
        let mut source=crate::launch_history::Store::ephemeral();
        let clock=source.update_primary(crate::telemetry_db::LaunchClockMsg{
            kind:crate::telemetry_db::LaunchClockKind::TPlus,
            anchor_timestamp_ms:Some(9000),duration_ms:None,
        },10000,now).unwrap();
        let mut observer=Runtime::new(None,true).unwrap();observer.connect(true,Some(session));
        let mut query=None;
        observer.offer(None,&clock,now,|_,wire|{assert!(query.is_none());query=Some(wire.to_vec());true}).unwrap();
        let mut primary=Runtime::new(None,false).unwrap();primary.connect(false,Some(session));
        let query=query.unwrap();
        let original_reply=Vec::new();
        let store=crate::launch_history::Store::open(directory.join("observer-clock.json"),true,None,10000,now).unwrap();
        Self{primary,observer,clock,store,session,query,query_received:false,primary_outbox:Default::default(),observer_outbox:Default::default(),offered:false,received:0,applied:0,original_reply}
    }
    pub(crate) fn queue_query(&mut self){assert!(self.observer_outbox.offer(crate::mission_sync::wire::Class::ClockQuery,&self.query));}
    pub(crate) fn flush_observer(&mut self,mesh:&crate::rrls::Shared,now:u64){self.observer_outbox.flush(mesh,now);}
    pub(crate) fn flush_primary(&mut self,mesh:&crate::rrls::Shared,now:u64){self.primary_outbox.flush(mesh,now);}
    pub(crate) fn accept_query(&mut self,wire:&[u8],now:u64){
        assert_eq!(wire,self.query);assert!(!self.query_received);self.primary.receive(None,wire,now,&self.clock).unwrap();self.original_reply=self.primary.reply.as_ref().unwrap().clone();self.query_received=true;
    }
    pub(crate) fn offer(&mut self, now:u64) {
        self.primary.offer(None,&self.clock,now,|validated,wire|{
            let reply=wire==self.original_reply;
            if reply{assert!(validated,"actual ClockReply keeps validated metadata path");}
            let class=if validated{crate::mission_sync::wire::Class::ClockReply}else{crate::mission_sync::wire::Class::ClockQuery};
            let accepted=self.primary_outbox.offer(class,wire);
            if reply{self.offered|=accepted;}else{assert!(!accepted,"same real Outbox slot must backpressure following hint while reply remains owned");}
            accepted
        }).unwrap();
    }
    pub(crate) async fn consume(&mut self, wire:&[u8], now:u64, live_session:Option<u32>, fresh:bool) {
        self.observer.connect(fresh,live_session);
        assert_eq!(live_session,Some(self.session));assert!(fresh);
        assert_eq!(wire,self.original_reply,"original nonce and whole application bytes preserved");
        match self.observer.receive(None,wire,now,&self.store.snapshot()).unwrap() {
            Some(Outcome::Clock(record))=>{
                assert_eq!(record,self.clock);self.received+=1;
                if self.store.apply_primary(record,true,10000,now).unwrap(){self.applied+=1;}
                crate::launch_history::flush(self.store.waiter()).await.unwrap();
                assert_eq!(self.store.snapshot(),self.clock);
            },
            _=>panic!("actual fresh-nonce receiver must produce clock outcome"),
        }
    }
    pub(crate) fn summary(&self)->serde_json::Value {
        serde_json::json!({"query_received_through_mesh":self.query_received,"offered":self.offered,"received_valid_nonce":self.received,"applied_and_flushed":self.applied,
            "wire_bytes":self.original_reply.len(),"application_nonce_ttl_ms":TTL,"transport_path":"mission_sync::wire::Class::ClockReply→queue_operating_mode_reply(existing5s)",
            "scope":"Single actual producer tick (following hint retained under real Outbox backpressure, no full periodicworker claim); actual Runtime consumer plus private persistent observerStore; unpaired display mode authenticated only by fresh current-session challenge, NOT Ed25519/operator authority."})
    }
}
