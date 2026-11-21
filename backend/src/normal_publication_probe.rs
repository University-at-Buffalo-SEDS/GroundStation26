//! NONSHIPPING Normal-mode causal scheduling points. No SDK/provider/clock calls.
//! Each point is outside application mutexes; CountedTick deliberately retains
//! its existing inflight lease, which MUST block actual reset epoch advancement.
use crate::state::AppState;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Clone, Copy)]
pub(crate) enum Site {
    CountedTick = 0,
    SafetyReady = 1,
}
type CapturedRefusal = ((u64, u64, u64), ClaimRefusal);
type SiteSlot = Mutex<Option<(usize, Arc<Inner>)>>;
struct Inner {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    seen: AtomicBool,
    lost: AtomicBool,
    calls: AtomicU64,
    refusal: Mutex<Option<CapturedRefusal>>,
}
static SLOTS: [SiteSlot; 2] = [const { Mutex::new(None) }; 2];
pub(crate) struct Pause {
    site: Site,
    key: usize,
    inner: Arc<Inner>,
}
impl Pause {
    pub(crate) fn install(state: &Arc<AppState>, site: Site) -> Self {
        let key = Arc::as_ptr(state) as usize;
        let inner = Arc::new(Inner {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            seen: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            calls: AtomicU64::new(0),
            refusal: Mutex::new(None),
        });
        let mut slot = SLOTS[site as usize].lock().unwrap();
        assert!(slot.is_none(), "one owned test schedule per site");
        *slot = Some((key, inner.clone()));
        Self { site, key, inner }
    }
    pub(crate) async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let notified = self.inner.entered.notified();
                if self.inner.seen.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("actual production branch did not reach causal scheduling point");
        assert!(
            !self.inner.lost.load(Ordering::Acquire),
            "hook timed out; trace inconclusive"
        );
    }
    pub(crate) fn publication_refusal(&self) -> Option<((u64,u64,u64),ClaimRefusal)> {
        assert!(!self.inner.lost.load(Ordering::Acquire), "refusal evidence lost");
        *self.inner.refusal.lock().unwrap()
    }
    pub(crate) fn calls(&self) -> u64 {
        self.inner.calls.load(Ordering::Acquire)
    }
    pub(crate) fn release(&self) {
        self.inner.release.notify_one();
    }
}
impl Drop for Pause {
    fn drop(&mut self) {
        self.release();
        let mut slot = SLOTS[self.site as usize].lock().unwrap();
        if slot.as_ref().is_some_and(|(key, _)| *key == self.key) {
            *slot = None;
        }
    }
}
pub(crate) async fn point(state: &Arc<AppState>, site: Site, _observed_state: u8) {
    let item = SLOTS[site as usize].try_lock().ok().and_then(|slot| {
        slot.as_ref()
            .filter(|(key, _)| *key == Arc::as_ptr(state) as usize)
            .map(|(_, inner)| inner.clone())
    });
    let Some(inner) = item else {
        return;
    };
    // No state/SDK locks and no serialization at this pause. Test snapshots are
    // captured separately outside owners. An armed point is one-shot.
    inner.calls.fetch_add(1, Ordering::AcqRel);
    if inner.seen.swap(true, Ordering::AcqRel) {
        return;
    }
    inner.entered.notify_one();
    if tokio::time::timeout(Duration::from_secs(5), inner.release.notified())
        .await
        .is_err()
    {
        inner.lost.store(true, Ordering::Release);
        panic!("bounded causal hook release missing; no runtime conclusion");
    }
}

// BEFORE claim acquisition and before any observations/provider call. A
// scheduler may preempt here without any inflight lease. Never drop a real
// acquired claim to manufacture this case. Pointer is an opaque key only.
struct TicketSlot {
    key: usize,
    ready: Arc<AtomicBool>,
    ticket: Option<(u64, u64, u64)>,
    released: bool,
    thread: Option<std::thread::ThreadId>,
    refusal: Option<ClaimRefusal>,
}
static TICKET: Mutex<Option<TicketSlot>> = Mutex::new(None);
// Elect exactly one matching before-claim capture. Never contend with the
// harness while it waits, or consume another AppState's registered pause.
static TICKET_CAPTURE_KEY: AtomicUsize = AtomicUsize::new(0);
static TICKET_CV: std::sync::Condvar = std::sync::Condvar::new();
static TICKET_LOST: AtomicBool = AtomicBool::new(false);
pub(crate) struct TicketPause {
    key: usize,
    ready: Arc<AtomicBool>,
}
impl TicketPause {
    pub(crate) fn install(state: &Arc<AppState>) -> Self {
        let key = Arc::as_ptr(state) as usize;
        let mut slot = TICKET.lock().unwrap();
        assert!(slot.is_none());
        TICKET_LOST.store(false, Ordering::Release);
        let ready = Arc::new(AtomicBool::new(false));
        *slot = Some(TicketSlot {
            key,
            ready: ready.clone(),
            ticket: None,
            released: false,
            thread: None,
            refusal: None,
        });
        drop(slot);
        TICKET_CAPTURE_KEY.store(key, Ordering::Release);
        Self { key, ready }
    }
    pub(crate) async fn entered(&self) -> (u64, u64, u64) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                assert!(
                    !TICKET_LOST.load(Ordering::Acquire),
                    "ticket diagnostic loss; premise inconclusive"
                );
                if self.ready.load(Ordering::Acquire) {
                    let slot = TICKET.lock().unwrap();
                    let current = slot.as_ref().filter(|s| s.key == self.key && Arc::ptr_eq(&s.ready, &self.ready))
                        .expect("original ticket registration was retired");
                    return current.ticket.expect("Release-ready follows original ticket/thread capture");
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual before-claim production stage missing")
    }
    pub(crate) fn refusal(&self) -> Option<ClaimRefusal> {
        assert!(!TICKET_LOST.load(Ordering::Acquire), "ticket refusal evidence lost");
        TICKET.lock().unwrap().as_ref()
            .filter(|slot| slot.key == self.key && Arc::ptr_eq(&slot.ready, &self.ready))
            .and_then(|slot| slot.refusal)
    }
    pub(crate) fn release(&self) {
        if let Some(slot) = TICKET.lock().unwrap().as_mut()
            && slot.key == self.key && Arc::ptr_eq(&slot.ready, &self.ready) {
            slot.released = true;
        }
        TICKET_CV.notify_all();
    }
}
impl Drop for TicketPause {
    fn drop(&mut self) {
        self.release();
        let mut slot = TICKET.lock().unwrap();
        if slot.as_ref().is_some_and(|s| s.key == self.key && Arc::ptr_eq(&s.ready, &self.ready)) {
            let _ = TICKET_CAPTURE_KEY.compare_exchange(self.key, 0, Ordering::AcqRel, Ordering::Acquire);
            *slot = None;
        }
    }
}
pub(crate) fn before_claim(state: &AppState, ticket: crate::gse::LaunchAdmission) {
    let key = state as *const AppState as usize;
    if TICKET_CAPTURE_KEY.load(Ordering::Acquire) != key {
        return;
    }
    let Ok(mut slot) = TICKET.try_lock() else {
        TICKET_LOST.store(true, Ordering::Release);
        return;
    };
    if !slot
        .as_ref()
        .is_some_and(|s| s.key == key && s.ticket.is_none())
    {
        return;
    }
    // Registration validation, election and capture share the same owner:
    // an elected old callback cannot fill a replacement same-App slot.
    if TICKET_CAPTURE_KEY.compare_exchange(key, 0, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    slot.as_mut().unwrap().ticket = Some(ticket.original_ownership());
    slot.as_mut().unwrap().thread = Some(std::thread::current().id());
    let registration = slot.as_ref().unwrap().ready.clone();
    registration.store(true, Ordering::Release);
    while slot.as_ref().is_some_and(|s| s.key == key && Arc::ptr_eq(&s.ready, &registration) && !s.released) {
        let (next, timeout) = TICKET_CV
            .wait_timeout(slot, Duration::from_secs(5))
            .unwrap();
        slot = next;
        assert!(
            !timeout.timed_out(),
            "bounded before-claim scheduler pause timed out; not runtime RED"
        );
    }
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) enum ClaimRefusal {GenerationChanged,AdmissionClosed}
type PublicationContext=(usize,(u64,u64,u64));
thread_local! { static PUBLICATION:std::cell::Cell<Option<PublicationContext>>=const {std::cell::Cell::new(None)}; }
pub(crate) struct PublicationClaim(Option<PublicationContext>);
impl PublicationClaim {
    pub(crate) fn enter(state:&AppState,ticket:crate::gse::LaunchAdmission)->Self {
        Self(PUBLICATION.replace(Some((state as *const AppState as usize,ticket.original_ownership()))))
    }
}
impl Drop for PublicationClaim {fn drop(&mut self){PUBLICATION.set(self.0);}}
/// Called ONLY at the original refusal return while the SAME Dispatch guard
/// still excludes epoch/barrier mutation. Test-only atomic rereads are stable
/// under that owner; no provider/SDK/clock/I/O/formatting or blocking capture.
pub(crate) fn claim_refused(state:&AppState,requested:u64) {
    let actual=crate::gse::automatic_generation(state);
    let reason=if requested!=actual {ClaimRefusal::GenerationChanged}
        else if state.operator_admission_barrier.load(Ordering::Acquire)!=0 {ClaimRefusal::AdmissionClosed}
        else {TICKET_LOST.store(true,Ordering::Release);return;};
    let key=state as *const AppState as usize;
    if let Some((owner,ticket))=PUBLICATION.get().filter(|(owner,_)|*owner==key) {
        let _=owner;
        if let Ok(slot)=SLOTS[Site::SafetyReady as usize].try_lock()
            && let Some((active,inner))=slot.as_ref().filter(|(active,inner)|*active==key && inner.seen.load(Ordering::Acquire)) {
                let _=active;
                match inner.refusal.try_lock() {
                    Ok(mut recorded)=>*recorded=Some((ticket,reason)),
                    Err(_)=>inner.lost.store(true,Ordering::Release),
                }
        }
    }
    match TICKET.try_lock() {
        Ok(mut slot)=>if let Some(slot)=slot.as_mut().filter(|s|s.key==key
            && s.ticket.is_some_and(|t|t.0==requested)
            && s.thread==Some(std::thread::current().id())) {slot.refusal=Some(reason);},
        Err(_)=>TICKET_LOST.store(true,Ordering::Release),
    }
}

// N6 ONLY: fixed branch counters. No clocks/providers/SDK/locks/formatting in record.
static N6_OWNER: AtomicUsize = AtomicUsize::new(0);
static N6_COUNTS: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];
pub(crate) struct N6Outcomes;
impl N6Outcomes {
    pub(crate) fn install(state: &Arc<AppState>) -> Self {
        assert_eq!(N6_OWNER.load(Ordering::Acquire), 0);
        for count in &N6_COUNTS { count.store(0, Ordering::Relaxed); }
        N6_OWNER.store(Arc::as_ptr(state) as usize, Ordering::Release);
        Self
    }
    pub(crate) fn snapshot(&self) -> serde_json::Value {
        let labels = ["observation_none", "observation_without_capture", "observation_captured",
            "publish_entered", "publication_claim_refused", "original_ownership_refused",
            "router_missing", "packet_preparation_refused", "SDK_preparation_refused",
            "commit_OK", "commit_Busy", "commit_Retired", "commit_StateChanged",
            "commit_StoreChanged", "commit_PolicyChanged", "commit_Ineligible_or_SDK"];
        let counts = labels.into_iter().zip(N6_COUNTS.iter())
            .map(|(label,count)| (label.to_owned(),serde_json::json!(count.load(Ordering::Relaxed))))
            .collect::<serde_json::Map<_,_>>();
        serde_json::json!({"counts":counts,"scope":"N6 actual branch counters only; final generic SDK category is not a specific lock or delivery diagnosis; no new clock/provider reads"})
    }
}
impl Drop for N6Outcomes {
    fn drop(&mut self) { N6_OWNER.store(0, Ordering::Release); }
}
pub(crate) fn n6_record(state: &AppState, code: usize) {
    if N6_OWNER.load(Ordering::Acquire) == state as *const AppState as usize {
        N6_COUNTS[code].fetch_add(1, Ordering::Relaxed);
    }
}
