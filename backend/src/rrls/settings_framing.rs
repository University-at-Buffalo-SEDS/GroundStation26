//! Host-only SETTINGS framing. Codec capability is never application authority.
//! No physical write, permission, HEALTH, mode or recovery-budget mutation here.
use ring::{digest, hmac, rand::{SecureRandom, SystemRandom}};
use std::collections::VecDeque;

pub(super) const KIND: u8 = 15;
const PROBE: u8 = 0x10;
const OFFER: u8 = 0x11;
const FRAGMENT: u8 = 0x12;
pub(super) const QUANTUM: usize = 92; // TOTAL UART bytes, not an inner payload MTU.
const OUTER: usize = 12; // RR8 + UART4.
const FRAGMENT_HEADER: usize = 18; // tag + full receiver nonce + index.
const DATA: usize = QUANTUM - OUTER - FRAGMENT_HEADER;
const MAX_INNER: usize = paired_settings::auth::MAX_WIRE;
const PROBE_MS: u64 = 750;
const RETRY_MS: u64 = 375;
const UNUSED_MS: u64 = 2_000;
const RX_MS: u64 = 5_000;
const COOLDOWN_MS: u64 = 30_000;
const PROBE_CHARGE: usize = 75; //47-byte body + RR/UART12 + air overhead16.
const OFFER_CHARGE: usize = 73;
const SETTINGS_KIND: u8 = 10;
/// Allocation identity is stable while the original Vec moves through queue /
/// active selection. No address is dereferenced and identical bytes are NOT identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OwnerId(usize,usize);
impl OwnerId { pub fn of(wire:&[u8])->Self { Self(wire.as_ptr() as usize,wire.len()) } }
const _: () = assert!(MAX_INNER.div_ceil(DATA) < 64);
type Hash = [u8; 16];
type Nonce = [u8; 16];

#[derive(Clone, Copy, Debug)]
pub(super) struct Context {
    pub mesh_ms: u64,
    pub continuous_ms: Option<u64>,
    pub session: Option<u32>,
    pub sequence: u32,
    pub peer_fresh: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Admission { Legacy, Fragmented, Busy }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action { Probe, Fragment(u8), Legacy }
#[derive(Debug)]
pub(super) struct Physical { pub body: Vec<u8>, pub action: Action }
#[derive(Default)]
pub(super) struct Incoming { pub reply: Option<Vec<u8>>, pub complete: Option<Vec<u8>> }
#[derive(Default)]
pub(super) struct Counters {
    pub rejected: u64,
    pub expired: u64,
    pub completed: u64,
    pub physical: u64,
    pub legacy: u64,
}
struct Nonces {
    key: Option<hmac::Key>,
    counter: u64,
    recent: VecDeque<Nonce>,
}
impl Nonces {
    fn new() -> Self {
        let mut seed = [0; 32];
        let key = SystemRandom::new().fill(&mut seed).ok()
            .map(|()| hmac::Key::new(hmac::HMAC_SHA256, &seed));
        Self { key, counter: 0, recent: VecDeque::with_capacity(16) }
    }
    fn next(&mut self, session: u32) -> Option<Nonce> {
        self.counter = self.counter.checked_add(1)?;
        let mut input = b"GS-SETTINGS-FRAME-v1\0".to_vec();
        input.extend(session.to_le_bytes());
        input.extend(self.counter.to_le_bytes());
        let tag = hmac::sign(self.key.as_ref()?, &input);
        let value: Nonce = tag.as_ref()[..16].try_into().ok()?;
        if value == [0; 16] || self.recent.contains(&value) {
            self.key = None;
            return None;
        }
        if self.recent.len() == 16 { self.recent.pop_front(); }
        self.recent.push_back(value);
        Some(value)
    }
}
struct Tx {
    owner: OwnerId,
    at_mesh: u64,
    deadline_continuous: u64,
    context_expires: u64,
    own_bytes_s: u64,
    peer_bytes_s: u64,
    session: u32,
    sequence: u32,
    hash: Hash,
    length: usize,
    declared_remaining: u16,
    nonce: Nonce,
    receiver: Option<Nonce>,
    probes_completed: u8,
    probe_completed_at: Option<u64>,
    probe_frozen: bool,
    early_offer: Option<(Nonce,u64)>,
    counted_drain: Option<(Action,u64)>,
    next: u8,
    started: bool,
    failed: bool,
    legacy: bool,
}
struct Rx {
    session: u32,
    sequence: u32,
    sender: Nonce,
    receiver: Nonce,
    hash: Hash,
    length: usize,
    expires: u64,
    unused_expires: u64,
    started: bool,
    bitmap: u64,
    data: Vec<u8>,
    offer: Vec<u8>,
}
/// One logical TX parent, and RX reservations SHARING existing SETTINGS inbox slots.
pub(super) struct State {
    tx: Option<Box<Tx>>,
    rx: Vec<Rx>,
    nonces: Nonces,
    last: Option<(u64, u64, Option<u32>)>,
    unsupported: Option<(u32, u64)>,
    local_primary: bool,
    pub counters: Counters,
}
fn hash(bytes: &[u8]) -> Hash {
    digest::digest(&digest::SHA256, bytes).as_ref()[..16].try_into().unwrap()
}
fn fragments(length: usize) -> usize { length.div_ceil(DATA) }
pub(super) fn wrap(body: &[u8]) -> Vec<u8> {
    let mut wire=Vec::with_capacity(body.len()+8);
    wire.extend([b'R',b'R',1,KIND]);wire.extend(body);
    wire.extend(crc32fast::hash(&wire).to_le_bytes());wire
}

fn u32_at(body: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(body.get(offset..offset+4)?.try_into().ok()?))
}
fn u16_at(body: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(body.get(offset..offset+2)?.try_into().ok()?))
}
fn nonce_at(body: &[u8], offset: usize) -> Option<Nonce> {
    body.get(offset..offset+16)?.try_into().ok()
}
impl State {
    pub fn new(local_primary: bool) -> Self {
        Self { tx: None, rx: Vec::with_capacity(4), nonces: Nonces::new(), last: None,
            unsupported: None, local_primary, counters: Counters::default() }
    }
    /// All epoch comparisons here use LOCAL continuous readings. Mesh at values
    /// are compared only with mesh values, then converted through a duration.
    pub fn observe(&mut self, c: Context) -> bool {
        let Some(now) = c.continuous_ms else { self.fail_closed(); return false; };
        let valid = c.peer_fresh && c.session.is_some() && self.last.is_none_or(|(mesh, continuous, session)| {
            c.mesh_ms >= mesh && now >= continuous && session == c.session
                // Suspend/discontinuous mesh clock must not extend logical validity.
                && now.saturating_sub(continuous).abs_diff(c.mesh_ms.saturating_sub(mesh)) <= 250
        });
        if !valid { self.fail_closed(); }
        if self.last.is_some_and(|(_,_,session)| session != c.session) { self.unsupported = None; }
        self.last = Some((c.mesh_ms, now, c.session));
        let before = self.rx.len();
        self.rx.retain(|r| now < r.expires && (r.started || now < r.unused_expires));
        self.counters.expired += (before-self.rx.len()) as u64;
        if self.unsupported.is_some_and(|(session, until)| Some(session) != c.session || now >= until) {
            self.unsupported = None;
        }
        self.release_early_offer(now);
        valid
    }
    fn release_early_offer(&mut self,now:u64){
        let Some(tx)=self.tx.as_mut()else{return;};
        let Some((receiver,arrived))=tx.early_offer else{return;};
        if tx.failed || now>=tx.deadline_continuous || now.saturating_sub(arrived)>=PROBE_MS {tx.early_offer=None;self.counters.rejected+=1;return;}
        let Some(complete)=tx.probe_completed_at.filter(|at|now>=*at)else{return;};
        tx.early_offer=None;
        if now<complete.saturating_add(PROBE_MS) && !tx.legacy{tx.receiver=Some(receiver);}else{self.counters.rejected+=1;}
    }
    fn fail_closed(&mut self) {
        if let Some(tx) = self.tx.as_mut() { tx.failed = true; }
        self.counters.expired += self.rx.len() as u64;
        self.rx.clear();
    }
    /// Minimum cost is an admission necessary condition, NEVER a service promise.
    /// Sender and peer budgets include both handshake owners and every frame16B.
    pub fn minimum_ms(length: usize, own_bytes_s: u64, peer_bytes_s: u64) -> Option<u64> {
        if own_bytes_s == 0 || peer_bytes_s == 0 { return None; }
        let own = length.checked_add(fragments(length).checked_mul(OUTER+FRAGMENT_HEADER+16)?)?
            .checked_add(PROBE_CHARGE)?;
        Some((own as u64*1000).div_ceil(own_bytes_s)
            .checked_add((OFFER_CHARGE as u64*1000).div_ceil(peer_bytes_s))?)
    }
    pub fn admit(&mut self, inner: &[u8], owner: OwnerId, at_mesh: u64, lifetime: u64,
        c: Context, own_bytes_s: u64, peer_bytes_s: u64) -> Admission {
        // Any accepted metadata parent, including an active one outside the
        // queue, excludes another parent even at identical timestamp/hash.
        if self.tx.is_some() { return Admission::Busy; }
        if inner.len()+OUTER <= QUANTUM || inner.len() > MAX_INNER { return Admission::Legacy; }
        if !self.observe(c) { return Admission::Busy; }
        let now = c.continuous_ms.unwrap();
        let session = c.session.unwrap();
        if self.unsupported.is_some_and(|(s,until)| s == session && now < until) { return Admission::Legacy; }
        let Some(elapsed) = c.mesh_ms.checked_sub(at_mesh) else { return Admission::Busy; };
        let Some(remaining) = lifetime.checked_sub(elapsed).filter(|v| *v > 0) else { return Admission::Busy; };
        if Self::minimum_ms(inner.len(),own_bytes_s,peer_bytes_s).is_none_or(|n| n > remaining.min(RX_MS)) {
            return Admission::Legacy; // valid large message keeps legacy behavior/known limits.
        }
        let Some(nonce) = self.nonces.next(session) else { return Admission::Busy; };
        let Some(deadline) = now.checked_add(remaining) else { return Admission::Busy; };
        self.tx = Some(Box::new(Tx { owner, at_mesh, deadline_continuous: deadline, context_expires: now.saturating_add(remaining.min(RX_MS)),
            own_bytes_s,peer_bytes_s,
            session, sequence: c.sequence, hash: hash(inner), length: inner.len(), declared_remaining: remaining.min(u64::from(u16::MAX)) as u16, nonce,
            receiver: None, probes_completed: 0, probe_completed_at:None, probe_frozen:false, early_offer:None, counted_drain:None, next: 0, started: false, failed: false, legacy: false }));
        Admission::Fragmented
    }
    pub fn cancel_unadmitted(&mut self, owner: OwnerId) {
        if self.tx.as_ref().is_some_and(|t| t.owner == owner && !t.started) { self.tx = None; }
    }
    pub fn owns(&self, owner: OwnerId) -> bool { self.tx.as_ref().is_some_and(|t| t.owner == owner) }
    pub fn started(&self, owner: OwnerId) -> bool { self.tx.as_ref().is_some_and(|t| t.owner == owner && t.started) }
    pub fn failed(&self, owner: OwnerId, continuous: Option<u64>) -> bool {
        self.tx.as_ref().is_some_and(|t| t.owner == owner && (t.failed || continuous.is_none_or(|n| n >= t.deadline_continuous)))
    }
    pub fn selectable(&self, owner: OwnerId) -> bool {
        let Some(tx) = self.tx.as_ref().filter(|t| t.owner == owner) else { return true; };
        let Some((_,now,_)) = self.last else { return false; };
        tx.failed || tx.legacy || tx.receiver.is_some() || tx.probes_completed == 0
            || tx.probe_completed_at.is_some_and(|at|now>=at.saturating_add(PROBE_MS))
            || (tx.probes_completed == 1 && tx.probe_completed_at.is_some_and(|at|now>=at.saturating_add(RETRY_MS)))
    }
    pub fn physical(&mut self, inner: &[u8], owner: OwnerId, c: Context) -> Option<Physical> {
        if !self.observe(c) { return None; }
        let now = c.continuous_ms?;
        // A receiver context cannot outlive its own<=5s lease. Re-probe under
        // a NEW nonce and replay the complete opaque owner, never slide either
        // context or the original application deadline (clock may still be30s).
        let renew = self.tx.as_ref().is_some_and(|t|t.owner == owner && t.receiver.is_some() && now>=t.context_expires && !t.failed);
        if renew {
            let tx = self.tx.as_mut().unwrap();
            let remaining = tx.deadline_continuous.saturating_sub(now);
            if Self::minimum_ms(tx.length,tx.own_bytes_s,tx.peer_bytes_s).is_none_or(|n|n>remaining.min(RX_MS)) {
                tx.legacy=true;
            } else if let Some(nonce) = self.nonces.next(tx.session) {
                tx.nonce=nonce;tx.receiver=None;tx.next=0;tx.probes_completed=0;tx.probe_completed_at=None;tx.probe_frozen=false;tx.early_offer=None;tx.counted_drain=None;
                tx.sequence=c.sequence;
                tx.context_expires=now.saturating_add(remaining.min(RX_MS));
                tx.declared_remaining=remaining.min(u64::from(u16::MAX)) as u16;
            } else { tx.failed=true; }
        }
        let tx = self.tx.as_mut().filter(|t| t.owner == owner)?;
        if tx.failed || c.mesh_ms < tx.at_mesh || now >= tx.deadline_continuous || tx.hash != hash(inner) { tx.failed = true; return None; }
        if tx.receiver.is_none() && tx.probe_completed_at.is_some_and(|at|now>=at.saturating_add(PROBE_MS)) {
            tx.legacy = true;
            self.unsupported = Some((tx.session,tx.probe_completed_at.unwrap().saturating_add(PROBE_MS+COOLDOWN_MS)));
        }
        if tx.legacy { return Some(Physical { body: Vec::new(), action: Action::Legacy }); }
        if let Some(receiver) = tx.receiver {
            let offset = usize::from(tx.next)*DATA;
            if offset >= inner.len() { return None; }
            let mut body = Vec::with_capacity(FRAGMENT_HEADER+DATA);
            body.push(FRAGMENT); body.extend(receiver); body.push(tx.next);
            body.extend(&inner[offset..(offset+DATA).min(inner.len())]);
            return Some(Physical { body, action: Action::Fragment(tx.next) });
        }
        if tx.probes_completed >= 2 || (tx.probes_completed == 1 && tx.probe_completed_at.is_none_or(|at|now<at.saturating_add(RETRY_MS))) { return None; }
        if !tx.probe_frozen {
            let remaining=tx.deadline_continuous.saturating_sub(now).min(u64::from(u16::MAX)) as u16;
            if tx.sequence!=c.sequence || tx.declared_remaining!=remaining{tx.early_offer=None;}
            tx.sequence=c.sequence;
            tx.declared_remaining=remaining;
        }
        let origin_kind = SETTINGS_KIND | if self.local_primary { 0 } else { 0x80 };
        let mut body = vec![PROBE,origin_kind];
        body.extend(tx.session.to_le_bytes()); body.extend(tx.sequence.to_le_bytes());
        body.extend(tx.nonce); body.extend((tx.length as u16).to_le_bytes());
        body.extend(tx.hash); body.push(QUANTUM as u8); body.extend(tx.declared_remaining.to_le_bytes());
        Some(Physical { body, action: Action::Probe })
    }
    /// Called ONLY after a complete counted physical write+drain. Partial error
    /// never advances index/attempt count and remains the same physical owner.
    pub fn complete_physical(&mut self, owner: OwnerId, action: Action, success: bool, c: Context) -> bool {
        self.observe(c);
        let Some(now)=c.continuous_ms else{return false;};
        let Some(tx) = self.tx.as_mut().filter(|t| t.owner == owner) else { return false; };
        if !success || tx.failed || now>=tx.deadline_continuous { return false; }
        let completed=match tx.counted_drain.take(){Some((kind,at))if kind==action=>at,Some(_)=>{tx.failed=true;return false;},None=>now};
        if completed>=tx.deadline_continuous{tx.failed=true;return false;}
        tx.started = true; self.counters.physical += 1;
        match action {
            Action::Probe => {
                if tx.probes_completed>=2{tx.failed=true;return false;}
                tx.probe_frozen=true;
                if tx.probe_completed_at.is_none(){tx.probe_completed_at=Some(completed);}
                tx.probes_completed+=1;
                self.release_early_offer(now);false
            }
            Action::Fragment(index) if index == tx.next => {
                tx.next += 1;
                usize::from(tx.next) == fragments(tx.length)
            }
            Action::Legacy => { self.counters.legacy += 1; true },
            _ => { tx.failed = true; false }
        }
    }
    pub fn note_counted_drain(&mut self,owner:OwnerId,action:Action,completed_at:u64){
        if let Some(tx)=self.tx.as_mut().filter(|t|t.owner==owner){tx.counted_drain=Some((action,completed_at));}
    }
    pub fn note_accepted_prefix(&mut self, owner: OwnerId, accepted: usize, action:Action) {
        if accepted > 0 {
            if let Some(tx) = self.tx.as_mut().filter(|t|t.owner == owner) { tx.started = true;if action==Action::Probe{tx.probe_frozen=true;} }
        }
    }
    pub fn retire(&mut self, owner: OwnerId) {
        if self.owns(owner) { self.tx = None; }
    }
    pub fn reserved_bytes(&self) -> usize {
        let tx = if self.tx.is_some() { std::mem::size_of::<Tx>() } else { 0 };
        // Reserve simultaneous TX body/wrapped UART copy and RX OFFER reply/
        // wrapped copy. Completed data moves from RX into the inbox; it is
        // never cloned. This reservation remains inside the shared byte cap.
        std::mem::size_of::<Self>() + 4*QUANTUM
            + self.rx.capacity()*std::mem::size_of::<Rx>()
            + self.nonces.recent.capacity()*std::mem::size_of::<Nonce>() + tx
            + self.rx.iter().map(|r| r.data.capacity()+r.offer.capacity()).sum::<usize>()
    }
    pub fn rx_slots(&self) -> usize { self.rx.len() }
    pub fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({"enabled":true,"settings_only":true,"maximum_uart_bytes":QUANTUM,
            "rx_reservations":self.rx_slots(),"reserved_bytes":self.reserved_bytes(),
            "rejected":self.counters.rejected,"expired":self.counters.expired,
            "completed":self.counters.completed,"physical_frames":self.counters.physical,
            "legacy_fallbacks":self.counters.legacy})
    }
    pub fn incoming(&mut self, body: &[u8], c: Context, inbox_slots: usize, free_bytes: usize) -> Incoming {
        if !self.observe(c) { return Incoming::default(); }
        let result = match body.first().copied() {
            Some(PROBE) => self.probe(body,c,inbox_slots,free_bytes),
            Some(OFFER) => { self.offer(body,c); Incoming::default() }
            Some(FRAGMENT) => self.fragment(body,c),
            _ => { self.counters.rejected += 1; Incoming::default() }
        };
        result
    }
    fn probe(&mut self, b: &[u8], c: Context, inbox_slots: usize, free_bytes: usize) -> Incoming {
        let expected_origin = SETTINGS_KIND | if self.local_primary { 0x80 } else { 0 };
        if b.len()!=47 || b[1]!=expected_origin || b[44]!=QUANTUM as u8 { self.counters.rejected+=1; return Incoming::default(); }
        let session = u32_at(b,2).unwrap(); let sequence=u32_at(b,6).unwrap();
        let sender=nonce_at(b,10).unwrap(); let length=usize::from(u16_at(b,26).unwrap());
        let hash:Hash=b[28..44].try_into().unwrap(); let remaining=u64::from(u16_at(b,45).unwrap());
        if Some(session)!=c.session || c.sequence.wrapping_sub(sequence)>4 || sender==[0;16]
            || length+OUTER<=QUANTUM || length>MAX_INNER || remaining==0 {
            self.counters.rejected+=1; return Incoming::default();
        }
        if let Some(old)=self.rx.iter().find(|r| r.session==session && r.sender==sender) {
            if old.hash==hash && old.length==length && old.sequence==sequence {
                return Incoming { reply:Some(old.offer.clone()),complete:None };
            }
            self.counters.rejected+=1;return Incoming::default();
        }
        let needed=length+45;
        if inbox_slots+self.rx.len()>=4 || needed>free_bytes { self.counters.rejected+=1;return Incoming::default(); }
        let Some(receiver)=self.nonces.next(session)else{return Incoming::default();};
        let now=c.continuous_ms.unwrap(); let lifetime=remaining.min(RX_MS);
        let mut offer=Vec::with_capacity(45);offer.extend([OFFER,b[1]]);offer.extend(session.to_le_bytes());offer.extend(sequence.to_le_bytes());
        offer.extend(sender);offer.extend(receiver);offer.push(QUANTUM as u8);offer.extend((length as u16).to_le_bytes());
        self.rx.push(Rx { session,sequence,sender,receiver,hash,length,expires:now.saturating_add(lifetime),
            unused_expires:now.saturating_add(UNUSED_MS.min(lifetime)),started:false,bitmap:0,data:vec![0;length],offer:offer.clone() });
        Incoming { reply:Some(offer),complete:None }
    }
    fn offer(&mut self,b:&[u8],c:Context){
        let expected_origin=SETTINGS_KIND|if self.local_primary{0}else{0x80};
        if b.len()!=45 || b[1]!=expected_origin || b[42]!=QUANTUM as u8{self.counters.rejected+=1;return;}
        let session=u32_at(b,2).unwrap();let sequence=u32_at(b,6).unwrap();let nonce=nonce_at(b,10).unwrap();let receiver=nonce_at(b,26).unwrap();let length=usize::from(u16_at(b,43).unwrap());
        let Some(tx)=self.tx.as_mut()else{return;};let now=c.continuous_ms.unwrap();
        if tx.failed || tx.legacy || Some(session)!=c.session || tx.session!=session || tx.sequence!=sequence || tx.nonce!=nonce || tx.length!=length || receiver==[0;16] || now>=tx.deadline_continuous || tx.probe_completed_at.is_some_and(|at|now>=at.saturating_add(PROBE_MS)){
            self.counters.rejected+=1;return;
        }
        if tx.receiver.is_some_and(|old|old!=receiver) || tx.early_offer.is_some_and(|(old,_)|old!=receiver){self.counters.rejected+=1;return;}
        if tx.probe_completed_at.is_none_or(|at|now<at){
            // One bounded candidate, never support/fragment admission before
            // our own complete counted write AND physical-drain/model fence.
            if tx.early_offer.is_none(){tx.early_offer=Some((receiver,now));}
        }else{tx.receiver=Some(receiver);}
    }
    fn fragment(&mut self,b:&[u8],c:Context)->Incoming {
        if !(FRAGMENT_HEADER+1..=QUANTUM-OUTER).contains(&b.len()) { self.counters.rejected+=1;return Incoming::default(); }
        let receiver=nonce_at(b,1).unwrap();let index=usize::from(b[17]);
        let Some(position)=self.rx.iter().position(|r| r.receiver==receiver && Some(r.session)==c.session)else{self.counters.rejected+=1;return Incoming::default();};
        let rx=&mut self.rx[position];let count=fragments(rx.length);
        let offset=index*DATA;let expected=(rx.length.saturating_sub(offset)).min(DATA);
        if index>=count || b.len()!=FRAGMENT_HEADER+expected { self.counters.rejected+=1;return Incoming::default(); }
        let bit=1u64<<index;
        if rx.bitmap&bit!=0 {
            if rx.data[offset..offset+expected]!=b[FRAGMENT_HEADER..] { self.rx.remove(position);self.counters.rejected+=1; }
            return Incoming::default();
        }
        rx.started=true;rx.data[offset..offset+expected].copy_from_slice(&b[FRAGMENT_HEADER..]);rx.bitmap|=bit;
        if rx.bitmap!=(1u64<<count)-1 { return Incoming::default(); }
        let rx=self.rx.remove(position);
        if hash(&rx.data)!=rx.hash { self.counters.rejected+=1;return Incoming::default(); }
        self.counters.completed+=1;
        Incoming { reply:None,complete:Some(rx.data) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(mesh:u64,epoch:u64)->Context {
        Context {mesh_ms:mesh,continuous_ms:Some(epoch+mesh),session:Some(42),sequence:0,peer_fresh:true}
    }
    fn payload()->Vec<u8> { (0..363).map(|i|(i%251)as u8).collect() }
    fn negotiated()->(State,State,Vec<u8>) {
        let mut sender=State::new(true);let mut receiver=State::new(false);let data=payload();
        assert_eq!(sender.admit(&data,OwnerId::of(&data),0,30_000,context(0,1000000),480,1400),Admission::Fragmented);
        let probe=sender.physical(&data,OwnerId::of(&data),context(0,1000000)).unwrap();
        assert_eq!(probe.action,Action::Probe);assert_eq!(probe.body.len(),47);
        sender.complete_physical(OwnerId::of(&data),probe.action,true,context(0,1000000));
        let offer=receiver.incoming(&probe.body,context(10,9000000),0,60000).reply.unwrap();
        assert_eq!(offer.len(),45);
        sender.incoming(&offer,context(20,1000000),0,60000);
        (sender,receiver,data)
    }
    #[test]
    fn exact_opaque_bytes_complete_once_without_cross_epoch_comparisons() {
        let(mut sender,mut receiver,data)=negotiated();let mut output=None;let mut count=0;
        for now in 30..40 {
            let Some(p)=sender.physical(&data,OwnerId::of(&data),context(now,1000000))else{break;};
            assert!(wrap(&p.body).len()+4<=QUANTUM);
            let result=receiver.incoming(&p.body,context(now+1,9000000),0,60000);
            if result.complete.is_some(){assert!(output.is_none());output=result.complete;}
            count+=1;
            if sender.complete_physical(OwnerId::of(&data),p.action,true,context(now,1000000)){sender.retire(OwnerId::of(&data));break;}
        }
        assert_eq!(count,6);assert_eq!(output,Some(data));assert_eq!(receiver.rx_slots(),0);
        assert_eq!(receiver.counters.completed,1);
    }
    #[test]
    fn duplicate_and_reordered_fragments_preserve_one_exact_message() {
        let(mut sender,mut receiver,data)=negotiated();let mut pieces=Vec::new();
        for now in 30..40 {
            let p=sender.physical(&data,OwnerId::of(&data),context(now,1000000)).unwrap();pieces.push(p.body);
            if sender.complete_physical(OwnerId::of(&data),p.action,true,context(now,1000000)){break;}
        }
        assert!(receiver.incoming(&pieces[2],context(50,9000000),0,60000).complete.is_none());
        assert!(receiver.incoming(&pieces[2],context(51,9000000),0,60000).complete.is_none());
        let mut output=None;
        for(step,p)in pieces.iter().rev().enumerate(){
            if let Some(v)=receiver.incoming(p,context(60+step as u64,9000000),0,60000).complete {output=Some(v);}
        }
        assert_eq!(output,Some(data));assert_eq!(receiver.counters.completed,1);
    }
    #[test]
    fn probes_retry_identically_once_without_extending_original_deadline() {
        let mut sender=State::new(true);let data=payload();
        assert_eq!(sender.admit(&data,OwnerId::of(&data),0,5000,context(0,1000000),480,1400),Admission::Fragmented);
        let first=sender.physical(&data,OwnerId::of(&data),context(0,1000000)).unwrap();
        sender.complete_physical(OwnerId::of(&data),first.action,true,context(0,1000000));
        assert!(sender.physical(&data,OwnerId::of(&data),context(374,1000000)).is_none());
        let second=sender.physical(&data,OwnerId::of(&data),context(375,1000000)).unwrap();assert_eq!(first.body,second.body);
        sender.complete_physical(OwnerId::of(&data),second.action,true,context(375,1000000));
        assert!(sender.physical(&data,OwnerId::of(&data),context(749,1000000)).is_none());
        assert_eq!(sender.physical(&data,OwnerId::of(&data),context(750,1000000)).unwrap().action,Action::Legacy);
        assert_eq!(sender.tx.as_ref().unwrap().deadline_continuous,1005000);
        sender.retire(OwnerId::of(&data));
        assert_eq!(sender.admit(&data,OwnerId::of(&data),751,5000,context(751,1000000),480,1400),Admission::Legacy);
    }
    #[test]
    fn full_inbox_and_memory_refuse_before_payload_allocation() {
        let mut s=State::new(true);let mut r=State::new(false);let data=payload();
        s.admit(&data,OwnerId::of(&data),0,5000,context(0,1000000),480,1400);
        let p=s.physical(&data,OwnerId::of(&data),context(0,1000000)).unwrap();
        assert!(r.incoming(&p.body,context(1,9000000),4,60000).reply.is_none());
        assert_eq!(r.rx_slots(),0);
        assert!(r.incoming(&p.body,context(2,9000000),0,1).reply.is_none());
        assert_eq!(r.rx_slots(),0);
    }
    #[test]
    fn partial_error_retries_same_fragment_without_cursor_advance() {
        let(mut s,_r,data)=negotiated();let p=s.physical(&data,OwnerId::of(&data),context(30,1000000)).unwrap();
        s.note_accepted_prefix(OwnerId::of(&data),7,p.action);
        assert!(!s.complete_physical(OwnerId::of(&data),p.action,false,context(30,1000000)));
        let retry=s.physical(&data,OwnerId::of(&data),context(31,1000000)).unwrap();assert_eq!(retry.body,p.body);
        assert_eq!(s.tx.as_ref().unwrap().next,0);assert!(s.started(OwnerId::of(&data)));
    }
    #[test]
    fn oversized_cost_remains_legacy_and_clock_keeps_original_thirty_seconds() {
        let mut s=State::new(true);
        assert!(State::minimum_ms(MAX_INNER,480,1400).unwrap()>5000);
        assert_eq!(s.admit(&vec![0;MAX_INNER],OwnerId(1,MAX_INNER),0,5000,context(0,1000000),480,1400),Admission::Legacy);
        let data=payload();assert_eq!(s.admit(&data,OwnerId::of(&data),1,30000,context(1,1000000),480,1400),Admission::Fragmented);
        assert_eq!(s.tx.as_ref().unwrap().deadline_continuous,1030001);
        assert_eq!(s.tx.as_ref().unwrap().context_expires,1005001);
    }
    #[test]
    fn wrong_direction_and_session_do_not_allocate_or_grant_freshness() {
        let mut s=State::new(true);let mut wrong=State::new(true);let data=payload();
        s.admit(&data,OwnerId::of(&data),0,5000,context(0,1000000),480,1400);
        let p=s.physical(&data,OwnerId::of(&data),context(0,1000000)).unwrap();
        assert!(wrong.incoming(&p.body,context(1,9000000),0,60000).reply.is_none());
        assert_eq!(wrong.rx_slots(),0);
        let mut c=context(2,9000000);c.session=Some(43);
        let mut r=State::new(false);assert!(r.incoming(&p.body,c,0,60000).reply.is_none());
        assert_eq!(r.rx_slots(),0);
    }
    #[test]
    fn receiver_expiry_is_fixed_and_clock_discontinuity_fails_closed() {
        let(mut s,mut r,data)=negotiated();
        let p=s.physical(&data,OwnerId::of(&data),context(30,1000000)).unwrap();
        assert!(r.incoming(&p.body,context(31,9000000),0,60000).complete.is_none());
        let expiry=r.rx[0].expires;
        r.incoming(&p.body,context(1000,9000000),0,60000);
        assert_eq!(r.rx[0].expires,expiry);
        r.observe(context(5010,9000000));assert_eq!(r.rx_slots(),0);
        let mut c=context(32,1000000);c.continuous_ms=Some(1001032);
        assert!(!s.observe(c));assert!(s.failed(OwnerId::of(&data),c.continuous_ms));
    }
    #[test]
    fn simultaneous_identical_and_distinct_parents_cannot_alias_active_owner() {
        let mut s=State::new(true);let first=payload();let identical=first.clone();let mut distinct=first.clone();distinct[0]^=1;
        let owner=OwnerId::of(&first);
        assert_ne!(owner,OwnerId::of(&identical));
        assert_eq!(s.admit(&first,owner,100,30000,context(100,1000000),480,1400),Admission::Fragmented);
        let p=s.physical(&first,owner,context(100,1000000)).unwrap();
        s.complete_physical(owner,p.action,true,context(100,1000000));
        for other in [&identical,&distinct] {
            let wrong=OwnerId::of(other);
            assert_eq!(s.admit(other,wrong,100,30000,context(101,1000000),480,1400),Admission::Busy);
            s.cancel_unadmitted(wrong);s.retire(wrong);s.note_accepted_prefix(wrong,1,Action::Probe);
            assert!(!s.complete_physical(wrong,Action::Legacy,true,context(101,1000000)));
            assert!(s.physical(other,wrong,context(101,1000000)).is_none());
            assert!(s.owns(owner));assert!(!s.owns(wrong));
        }
        assert!(s.started(owner));assert_eq!(s.tx.as_ref().unwrap().probes_completed,1);
    }
    #[test]
    fn lost_offer_same_probe_retry_preserves_original_reservation_and_peak_budget() {
        let mut s=State::new(true);let mut r=State::new(false);let data=payload();
        s.admit(&data,OwnerId::of(&data),0,30000,context(0,1000000),480,1400);
        let p=s.physical(&data,OwnerId::of(&data),context(0,1000000)).unwrap();
        s.complete_physical(OwnerId::of(&data),p.action,true,context(0,1000000));
        let base=r.reserved_bytes();
        let lost=r.incoming(&p.body,context(1,9000000),0,60000).reply.unwrap();
        let reserved=r.reserved_bytes();let expires=r.rx[0].expires;
        assert_eq!(reserved-base,data.capacity()+lost.capacity());
        assert!(reserved >= std::mem::size_of::<State>()+4*QUANTUM+data.len()+lost.len());
        // The first OFFER is lost or rejected by Mesh admission. Same-byte
        // retry may reproduce it without a new context/expiry or extra payload.
        let retry=s.physical(&data,OwnerId::of(&data),context(375,1000000)).unwrap();
        assert_eq!(retry.body,p.body);
        let accepted=r.incoming(&retry.body,context(376,9000000),0,60000).reply.unwrap();
        assert_eq!(accepted,lost);assert_eq!(r.rx_slots(),1);assert_eq!(r.rx[0].expires,expires);
        assert_eq!(r.reserved_bytes(),reserved);
        s.incoming(&accepted,context(377,1000000),0,60000);
        let mut complete=None;
        for t in 400..410 {
            let f=s.physical(&data,OwnerId::of(&data),context(t,1000000)).unwrap();
            let result=r.incoming(&f.body,context(t,9000000),0,60000);
            if result.complete.is_some(){complete=result.complete;}
            if s.complete_physical(OwnerId::of(&data),f.action,true,context(t,1000000)){break;}
        }
        assert_eq!(complete,Some(data));assert_eq!(r.rx_slots(),0);
        assert_eq!(r.reserved_bytes(),base);
    }
    #[test]
    fn corrupt_message_and_stale_offer_cannot_complete_or_resurrect_session() {
        let(mut s,mut r,data)=negotiated();
        let mut fragments=Vec::new();
        for t in 30..40 {
            let f=s.physical(&data,OwnerId::of(&data),context(t,1000000)).unwrap();fragments.push(f.body);
            if s.complete_physical(OwnerId::of(&data),f.action,true,context(t,1000000)){break;}
        }
        fragments.last_mut().unwrap()[FRAGMENT_HEADER]^=1;
        for(t,f)in fragments.iter().enumerate(){assert!(r.incoming(f,context(100+t as u64,9000000),0,60000).complete.is_none());}
        assert_eq!(r.counters.completed,0);assert_eq!(r.rx_slots(),0);
        let mut changed=context(200,1000000);changed.session=Some(99);
        assert!(!s.observe(changed));assert!(s.failed(OwnerId::of(&data),changed.continuous_ms));
    }

    #[test]
    fn queue_wait_is_not_negotiation_age_and_parent_deadline_never_slides(){
        let mut s=State::new(true);let mut r=State::new(false);let data=payload();let owner=OwnerId::of(&data);
        assert_eq!(s.admit(&data,owner,0,5000,context(0,1000000),480,1400),Admission::Fragmented);
        let p=s.physical(&data,owner,context(677,1000000)).unwrap();assert_eq!(p.action,Action::Probe);
        assert_eq!(u16_at(&p.body,45),Some(4323));
        s.note_accepted_prefix(owner,p.body.len(),p.action);s.note_counted_drain(owner,p.action,1000683);
        s.complete_physical(owner,p.action,true,context(677,1000000));
        let offer=r.incoming(&p.body,context(690,9000000),0,60000).reply.unwrap();
        s.incoming(&offer,context(966,1000000),0,60000);
        assert!(s.tx.as_ref().unwrap().receiver.is_some());assert_eq!(s.tx.as_ref().unwrap().probe_completed_at,Some(1000683));
        assert_eq!(s.tx.as_ref().unwrap().deadline_continuous,1005000);
        assert!(matches!(s.physical(&data,owner,context(967,1000000)).unwrap().action,Action::Fragment(0)));
    }
    #[test]
    fn partial_probe_freezes_bytes_without_arming_or_advancing_counter(){
        let mut s=State::new(true);let data=payload();let owner=OwnerId::of(&data);
        s.admit(&data,owner,0,5000,context(0,1000000),480,1400);
        let first=s.physical(&data,owner,context(200,1000000)).unwrap();s.note_accepted_prefix(owner,7,Action::Probe);
        assert!(!s.complete_physical(owner,Action::Probe,false,context(200,1000000)));
        let mut later=context(1000,1000000);later.sequence=2;
        let retry=s.physical(&data,owner,later).unwrap();assert_eq!(first.body,retry.body);
        assert_eq!(s.tx.as_ref().unwrap().probe_completed_at,None);assert_eq!(s.tx.as_ref().unwrap().probes_completed,0);
        s.note_counted_drain(owner,Action::Probe,1001010);s.complete_physical(owner,Action::Probe,true,later);
        assert_eq!(s.tx.as_ref().unwrap().probe_completed_at,Some(1001010));assert_eq!(s.tx.as_ref().unwrap().probes_completed,1);
        assert_eq!(s.tx.as_ref().unwrap().deadline_continuous,1005000);
    }
    #[test]
    fn early_offer_cannot_activate_before_counted_drain_and_model_fence(){
        let mut s=State::new(true);let mut r=State::new(false);let data=payload();let owner=OwnerId::of(&data);
        s.admit(&data,owner,0,5000,context(0,1000000),480,1400);let p=s.physical(&data,owner,context(0,1000000)).unwrap();
        let offer=r.incoming(&p.body,context(1,9000000),0,60000).reply.unwrap();
        s.incoming(&offer,context(2,1000000),0,60000);assert!(s.tx.as_ref().unwrap().receiver.is_none());assert!(s.tx.as_ref().unwrap().early_offer.is_some());
        let mut wrong=offer.clone();wrong[26]^=1;s.incoming(&wrong,context(3,1000000),0,60000);assert_eq!(s.counters.rejected,1);
        s.note_accepted_prefix(owner,59,Action::Probe);s.note_counted_drain(owner,Action::Probe,1000600);
        s.complete_physical(owner,Action::Probe,true,context(500,1000000));
        s.observe(context(599,1000000));assert!(s.tx.as_ref().unwrap().receiver.is_none());
        s.observe(context(600,1000000));assert!(s.tx.as_ref().unwrap().receiver.is_some());
        assert_eq!(s.tx.as_ref().unwrap().probes_completed,1);
    }
    #[test]
    fn retry_and_unsupported_expiry_remain_fixed_from_first_complete_probe(){
        let mut s=State::new(true);let data=payload();let owner=OwnerId::of(&data);
        s.admit(&data,owner,0,5000,context(0,1000000),480,1400);let p=s.physical(&data,owner,context(400,1000000)).unwrap();
        s.note_counted_drain(owner,Action::Probe,1000450);s.complete_physical(owner,Action::Probe,true,context(400,1000000));
        assert!(s.physical(&data,owner,context(824,1000000)).is_none());
        let retry=s.physical(&data,owner,context(825,1000000)).unwrap();assert_eq!(p.body,retry.body);
        s.note_counted_drain(owner,Action::Probe,1000830);s.complete_physical(owner,Action::Probe,true,context(825,1000000));
        assert_eq!(s.tx.as_ref().unwrap().probe_completed_at,Some(1000450));assert_eq!(s.tx.as_ref().unwrap().probes_completed,2);
        assert!(s.physical(&data,owner,context(1199,1000000)).is_none());
        assert_eq!(s.physical(&data,owner,context(1200,1000000)).unwrap().action,Action::Legacy);
        assert_eq!(s.unsupported,Some((42,1031200)));assert_eq!(s.tx.as_ref().unwrap().deadline_continuous,1005000);
    }
    #[test]
    fn stale_early_offer_wrong_drain_and_deadline_never_manufacture_completion(){
        let mut s=State::new(true);let mut r=State::new(false);let data=payload();let owner=OwnerId::of(&data);
        s.admit(&data,owner,0,5000,context(0,1000000),480,1400);let p=s.physical(&data,owner,context(0,1000000)).unwrap();let offer=r.incoming(&p.body,context(1,9000000),0,60000).reply.unwrap();s.incoming(&offer,context(2,1000000),0,60000);
        s.note_counted_drain(owner,Action::Probe,1000800);s.complete_physical(owner,Action::Probe,true,context(800,1000000));
        assert!(s.tx.as_ref().unwrap().receiver.is_none());assert!(s.tx.as_ref().unwrap().early_offer.is_none());
        s.note_counted_drain(owner,Action::Fragment(0),1000900);assert!(!s.complete_physical(owner,Action::Probe,true,context(900,1000000)));assert!(s.failed(owner,Some(1000900)));
        s.retire(owner);s.admit(&data,owner,1000,5000,context(1000,1000000),480,1400);s.note_counted_drain(owner,Action::Probe,1006000);
        assert!(!s.complete_physical(owner,Action::Probe,true,context(5999,1000000)));assert!(s.failed(owner,Some(1006000)));
    }
    #[test]
    fn zero_prefix_can_refresh_probe_but_session_change_discards_early_candidate(){
        let mut s=State::new(true);let data=payload();let owner=OwnerId::of(&data);
        s.admit(&data,owner,0,5000,context(0,1000000),480,1400);let p=s.physical(&data,owner,context(0,1000000)).unwrap();s.note_accepted_prefix(owner,0,Action::Probe);
        let mut c=context(600,1000000);c.sequence=1;let next=s.physical(&data,owner,c).unwrap();assert_ne!(p.body,next.body);assert_eq!(&p.body[10..26],&next.body[10..26]);assert_eq!(u16_at(&next.body,45),Some(4400));
        c.session=Some(43);assert!(!s.observe(c));assert!(s.failed(owner,c.continuous_ms));assert!(s.physical(&data,owner,c).is_none());
    }

}
