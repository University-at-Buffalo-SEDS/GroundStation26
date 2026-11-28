//! A Fill owns its original admission while the sole recording actor prepares.
//! Nothing here changes recording tables, calibration, or a control timestamp.
use crate::{
    auth::{AuthPrincipal, Permission},
    gse::LaunchAdmission,
    state::AppState,
};
use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, oneshot};

const PREPARATION_LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Error {
    Stale,
    PendingBusy,
    Unauthorized,
    Busy,
    Unavailable,
    Storage(String),
    Validation(String),
    Exhausted,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stale => "Fill recording preparation expired or was retired; press Fill again",
            Self::PendingBusy => "Network admission is temporarily busy; the original Fill owner is retained",
            Self::Unauthorized => "Fill authority is no longer current",
            Self::Busy => "A Fill recording preparation is already pending",
            Self::Unavailable => "Recording owner is unavailable; Fill was not started",
            Self::Storage(_) => "Recording could not be made ready; Fill was not started",
            Self::Validation(message) => message,
            Self::Exhausted => "Recording identity is exhausted; Fill was not started",
        })
    }
}

/// This value is captured before authentication, never reconstructed afterward.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Captured {
    started: Instant,
    ticket: LaunchAdmission,
}
impl Captured {
    pub(crate) fn capture(state: &AppState) -> Self {
        Self {
            started: Instant::now(),
            ticket: crate::gse::plumbing_admission_ticket(state),
        }
    }
    pub(crate) fn bind(
        self,
        authority: Authority,
        principal: Option<&AuthPrincipal>,
    ) -> Arc<Origin> {
        Arc::new(Origin {
            captured: self,
            authority,
            original_auth_expiry: principal.and_then(|p| p.expires_at_ms),
            alive: AtomicBool::new(true),
        })
    }
}

pub(crate) enum Authority {
    Session {
        token: Option<String>,
        actor: Option<String>,
        admission_current: Arc<dyn Fn() -> crate::gse::CurrentState + Send + Sync>,
    },
    Physical {
        current: Arc<dyn Fn() -> bool + Send + Sync>,
    },
    Proxy {
        actor: String,
        admission_current: Arc<dyn Fn() -> bool + Send + Sync>,
        session_current: Arc<dyn Fn() -> bool + Send + Sync>,
    },
}
impl fmt::Debug for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<retained Fill authority>")
    }
}
#[derive(Debug)]
pub struct Origin {
    captured: Captured,
    authority: Authority,
    original_auth_expiry: Option<i64>,
    alive: AtomicBool,
}
impl Origin {
    pub(crate) fn ticket(&self) -> LaunchAdmission {
        self.captured.ticket
    }
    pub(crate) fn generation(&self) -> u64 {
        self.captured.ticket.original_ownership().0
    }
    pub(crate) fn live(&self) -> Result<(), Error> {
        if matches!(self.generation(), 0 | u64::MAX) {
            return Err(Error::Stale);
        }
        if !self.alive.load(Ordering::Acquire) {
            return Err(Error::Stale);
        }
        if self.original_auth_expiry.is_some_and(|expiry| {
            expiry <= crate::telemetry_task::get_current_timestamp_ms() as i64
        }) {
            return Err(Error::Unauthorized);
        }
        Ok(())
    }
    pub(crate) fn remaining(&self) -> Result<Duration, Error> {
        self.live()?;
        let remaining = PREPARATION_LIMIT
            .checked_sub(self.captured.started.elapsed())
            .filter(|v| !v.is_zero())
            .ok_or(Error::Stale)?;
        if let Some(expiry) = self.original_auth_expiry {
            let auth_left =
                expiry.saturating_sub(crate::telemetry_task::get_current_timestamp_ms() as i64);
            if auth_left <= 0 {
                return Err(Error::Unauthorized);
            }
            return Ok(remaining.min(Duration::from_millis(auth_left as u64)));
        }
        Ok(remaining)
    }
    pub(crate) fn retire(&self) {
        self.alive.store(false, Ordering::Release);
    }
    pub(crate) fn ram_current(&self, state: &AppState) -> crate::gse::CurrentState {
        if self.live().is_err() { return crate::gse::CurrentState::Invalid; }
        crate::gse::fill_origin_current(state, self.captured.ticket)
    }
    pub(crate) async fn authorize(
        &self,
        state: &AppState,
        command: &'static str,
    ) -> Result<(), Error> {
        let mut slice = Instant::now();
        loop {
            self.remaining()?;
            match self.authorize_once(state, command).await {
                Err(Error::PendingBusy) => {
                    // No authority or deadline is renewed. The caller owns the
                    // same Origin while this nonblocking inspection is busy.
                    let wait = self.remaining()?.min(Duration::from_millis(5));
                    tokio::time::sleep(wait).await;
                    if slice.elapsed() >= Duration::from_millis(100) {
                        tokio::task::yield_now().await;
                        slice = Instant::now();
                    }
                }
                result => { result?; self.remaining()?; return Ok(()); }
            }
        }
    }
    /// One actor attempt; Busy is a resumable owner state, never invalidation.
    pub(crate) async fn authorize_once(&self, state: &AppState, command: &'static str) -> Result<(), Error> {
        self.remaining()?;
        self.authorize_inner(state, command, false).await?;
        self.remaining()?;
        Ok(())
    }
    /// An accepted zero uses its existing capture deadline, not a renewed
    /// human intent and not the initial preparation timer. Actor/session and
    /// original Stop ownership still remain mandatory.
    pub(crate) async fn authorize_continuation(
        &self,
        state: &AppState,
        command: &'static str,
    ) -> Result<(), Error> {
        self.authorize_inner(state, command, true).await
    }
    async fn authorize_inner(
        &self,
        state: &AppState,
        command: &'static str,
        accepted: bool,
    ) -> Result<(), Error> {
        self.live()?;
        match self.ram_current(state) {
            crate::gse::CurrentState::Current => {},
            crate::gse::CurrentState::PendingBusy => return Err(Error::PendingBusy),
            crate::gse::CurrentState::Invalid => return Err(Error::Stale),
        }
        #[cfg(test)] if command=="AutoColdFlow" && !accepted {
            coldflow_test_hooks::pause(state,coldflow_test_hooks::Stage::OriginCurrent);
        }
        let allowed = match &self.authority {
            Authority::Session {
                token,
                actor,
                admission_current,
            } => {
                if !accepted {
                    match admission_current() {
                        crate::gse::CurrentState::Current => {},
                        crate::gse::CurrentState::PendingBusy => return Err(Error::PendingBusy),
                        crate::gse::CurrentState::Invalid => return Err(Error::Stale),
                    }
                }
                #[cfg(test)]
                if accepted { crate::auto_zero::deadline_tests::accepted_auth_started(state); }
                let allowed = state
                    .auth
                    .authorize_token(&state.auth_db, token.as_deref(), Permission::SendCommands)
                    .await
                    .is_ok_and(|p| {
                        p.username == *actor
                            && p.permissions.send_commands
                            && p.allows_command_name(command)
                    });
                allowed && (accepted || match admission_current() {
                    crate::gse::CurrentState::Current => true,
                    crate::gse::CurrentState::PendingBusy => return Err(Error::PendingBusy),
                    crate::gse::CurrentState::Invalid => false,
                })
            }
            Authority::Physical { current } => current(),
            Authority::Proxy {
                actor,
                admission_current,
                session_current,
            } => {
                let current = if accepted {
                    session_current
                } else {
                    admission_current
                };
                if !current() {
                    return Err(Error::Stale);
                }
                let auth = state.auth.clone();
                let actor = actor.clone();
                tokio::task::spawn_blocking(move || {
                    auth.paired_proxy_actor(&actor, Permission::SendCommands)
                })
                .await
                .is_ok_and(|p| {
                    p.is_ok_and(|p| p.permissions.send_commands && p.allows_command_name(command))
                }) && current()
            }
        };
        self.live()?;
        if !allowed {
            return Err(Error::Unauthorized);
        }
        match self.ram_current(state) {
            crate::gse::CurrentState::Current => Ok(()),
            crate::gse::CurrentState::PendingBusy => Err(Error::PendingBusy),
            crate::gse::CurrentState::Invalid => Err(Error::Stale),
        }
    }
}

/// Retire only unfinished work when its original handler/future is canceled.
pub(crate) struct Cancellation {
    origin: Arc<Origin>,
    transferred: bool,
}
impl Cancellation {
    pub(crate) fn new(origin: Arc<Origin>) -> Self {
        Self {
            origin,
            transferred: false,
        }
    }
    pub(crate) fn transfer(mut self) {
        self.transferred = true;
    }
}
impl Drop for Cancellation {
    fn drop(&mut self) {
        if !self.transferred {
            self.origin.retire();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    pub(crate) instance: u64,
    pub(crate) path: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Disposition {
    FullNoop,
    MayStart,
    AlreadyFilling,
}
#[derive(Debug)]
pub(crate) struct Prepared {
    pub(crate) origin: Arc<Origin>,
    pub(crate) receipt: Receipt,
    pub(crate) initial: Disposition,
    pub(crate) zero_generation: AtomicU64,
}

pub(crate) struct Runtime {
    next_instance: AtomicU64,
    ready: AtomicU64,
    pub(crate) preparation: Arc<Semaphore>,
    pub(crate) continuation: Mutex<Option<Arc<Prepared>>>,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            next_instance: AtomicU64::new(1),
            ready: AtomicU64::new(0),
            preparation: Arc::new(Semaphore::new(1)),
            continuation: Mutex::new(None),
        }
    }
}
impl Runtime {
    /// Called only inside the existing DISPATCH ordering, before owner loss.
    pub(crate) fn revoke_under_dispatch(&self) {
        self.ready.store(0, Ordering::SeqCst);
    }
    pub(crate) fn reserve_instance(&self) -> Result<u64, Error> {
        let mut current = self.next_instance.load(Ordering::SeqCst);
        loop {
            let next = current
                .checked_add(1)
                .filter(|n| *n != u64::MAX)
                .ok_or(Error::Exhausted)?;
            match self.next_instance.compare_exchange(
                current,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(current),
                Err(observed) => current = observed,
            }
        }
    }
    /// All pool/schema/owner antecedents are established before this last store.
    pub(crate) fn publish_under_dispatch(&self, instance: u64) {
        self.ready.store(instance, Ordering::SeqCst);
    }
    pub(crate) fn current(&self, receipt: &Receipt) -> bool {
        !matches!(receipt.instance, 0 | u64::MAX)
            && self.ready.load(Ordering::SeqCst) == receipt.instance
    }
    pub(crate) fn ready_instance(&self) -> u64 {
        self.ready.load(Ordering::SeqCst)
    }
    #[cfg(test)]
    pub(crate) fn next_instance_for_test(&self) -> u64 { self.next_instance.load(Ordering::SeqCst) }
}

pub(crate) struct EnsureRequest {
    pub(crate) origin: Arc<Origin>,
    pub(crate) command: &'static str,
    reply: Mutex<Option<oneshot::Sender<Result<Receipt, Error>>>>,
}
impl fmt::Debug for EnsureRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EnsureRecording(<original authority>)")
    }
}
impl EnsureRequest {
    pub(crate) fn new(
        origin: Arc<Origin>,
        command: &'static str,
    ) -> (Arc<Self>, oneshot::Receiver<Result<Receipt, Error>>) {
        let (reply, receive) = oneshot::channel();
        (
            Arc::new(Self {
                origin,
                command,
                reply: Mutex::new(Some(reply)),
            }),
            receive,
        )
    }
    pub(crate) fn pending(&self) -> bool {
        self.reply
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|r| !r.is_closed())
            && self.origin.remaining().is_ok()
    }
    pub(crate) fn reply(&self, value: Result<Receipt, Error>) {
        if let Some(reply) = self.reply.lock().unwrap().take() {
            let _ = reply.send(value);
        }
    }
}

/// Fixed one-shot RAM publication. Busy never consumes or reconstructs this
/// original request/instance; all pool ownership stays in the sole DB actor.
pub(crate) struct Publication {
    request: Arc<EnsureRequest>,
    instance: u64,
    applied: bool,
}
impl Publication {
    pub(crate) fn new(request: Arc<EnsureRequest>, instance: u64) -> Self {
        Self { request, instance, applied: false }
    }
    pub(crate) fn apply(&mut self, state: &AppState) -> Result<(), Error> {
        if self.applied { return Err(Error::Unavailable); }
        if !self.request.pending() { return Err(Error::Stale); }
        state.fill_recording.publish_under_dispatch(self.instance);
        self.applied = true;
        Ok(())
    }
    pub(crate) fn applied(&self) -> bool { self.applied }
}

/// The initial recording wait is bounded by the ORIGINAL pre-auth deadline.
/// The actor keeps ownership of any staged file if this receiver is canceled.
pub(crate) async fn ensure(
    state: &Arc<AppState>,
    origin: Arc<Origin>,
    command: &'static str,
) -> Result<Receipt, Error> {
    origin.authorize(state, command).await?;
    let (request, receive) = EnsureRequest::new(origin.clone(), command);
    tokio::time::timeout(
        origin.remaining()?,
        state
            .db_queue_tx
            .send(crate::telemetry_db::DbQueueItem::EnsureRecording(request)),
    )
    .await
    .map_err(|_| Error::Stale)?
    .map_err(|_| Error::Unavailable)?;
    let receipt = tokio::time::timeout(origin.remaining()?, receive)
        .await
        .map_err(|_| Error::Stale)?
        .map_err(|_| Error::Unavailable)??;
    origin.authorize(state, command).await?;
    if !state.fill_recording.current(&receipt) {
        return Err(Error::Unavailable);
    }
    Ok(receipt)
}

pub(crate) async fn start(state: Arc<AppState>, origin: Arc<Origin>) -> Result<(), Error> {
    let cancellation = Cancellation::new(origin.clone());
    let _permit = state
        .fill_recording
        .preparation
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    let _claim =
        crate::gse::claim_operator_dispatch(&state, origin.generation()).ok_or(Error::Stale)?;
    origin.authorize(&state, "StartFill").await?;
    let mut classify_slice = Instant::now();
    let initial = loop {
        match crate::gse::classify_fill_for_recording(&state, &origin) {
            Err(Error::PendingBusy) => {
                tokio::time::sleep(origin.remaining()?.min(Duration::from_millis(5))).await;
                if classify_slice.elapsed() >= Duration::from_millis(100) {
                    tokio::task::yield_now().await;
                    classify_slice = Instant::now();
                }
            }
            result => break result?,
        }
    };
    let receipt = ensure(&state, origin.clone(), "StartFill").await?;
    let prepared = Arc::new(Prepared {
        origin,
        receipt,
        initial,
        zero_generation: AtomicU64::new(0),
    });
    let mut slice = Instant::now();
    loop {
        match crate::gse::apply_recorded_fill(&state, prepared.clone()) {
            Err(Error::PendingBusy) => {
                let wait = prepared.origin.remaining()?.min(Duration::from_millis(5));
                tokio::time::sleep(wait).await;
                if slice.elapsed() >= Duration::from_millis(100) {
                    tokio::task::yield_now().await;
                    slice = Instant::now();
                }
            }
            result => { result?; break; }
        }
    }
    cancellation.transfer();
    Ok(())
}

/// Capture continuations never create a fresh command, timestamp or admission.
pub(crate) async fn authorize_zero_continuation(
    state: &AppState,
    generation: u64,
) -> Result<Arc<Prepared>, Error> {
    let prepared = state
        .fill_recording
        .continuation
        .lock()
        .unwrap()
        .clone()
        .ok_or(Error::Stale)?;
    if prepared.origin.generation() != generation
        || !state.fill_recording.current(&prepared.receipt)
    {
        return Err(Error::Stale);
    }
    prepared
        .origin
        .authorize_continuation(state, "StartFill")
        .await?;
    if !state.fill_recording.current(&prepared.receipt) {
        return Err(Error::Unavailable);
    }
    Ok(prepared)
}

pub(crate) fn notify_failure(state: &AppState, error: &Error) {
    state.add_temporary_notification(format!("Fill recording [{error:?}]: {error}"));
}

pub(crate) fn spawn(state: Arc<AppState>, origin: Arc<Origin>) {
    let Some(lease) = state.control_ingress.handler() else {
        origin.retire();
        return;
    };
    let runtime = state.db_write_runtime.clone();
    runtime.spawn(lease.run(async move {
        if let Err(error) = start(state.clone(), origin).await {
            notify_failure(&state, &error);
        }
    }));
}

pub(crate) fn http_result(result: Result<(), Error>) -> (axum::http::StatusCode, String) {
    use axum::http::StatusCode;
    match result {
        Ok(()) => (StatusCode::OK, "Fill admitted with recording ready".into()),
        Err(error) => {
            let status = match error {
                Error::Unauthorized => StatusCode::FORBIDDEN,
                Error::Stale | Error::Validation(_) => StatusCode::CONFLICT,
                Error::Storage(_) => StatusCode::INSUFFICIENT_STORAGE,
                _ => StatusCode::SERVICE_UNAVAILABLE,
            };
            (status, serde_json::json!({"error":"fill_recording","reason":error,"message":error.to_string()}).to_string())
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;

/// The actual ordinary receiver and GPIO fixture share this exact bounded
/// generation-checked handoff. No trusted private variant is deserialized.
pub(crate) fn dispatch_queued(
    state: &Arc<AppState>,
    command: &crate::types::TelemetryCommand,
) -> bool {
    let crate::types::TelemetryCommand::QueuedOperator {
        generation,
        command,
        recovery_origin,
        ..
    } = command
    else {
        return false;
    };
    let crate::types::TelemetryCommand::FillWithRecording { origin } = command.as_ref() else {
        return false;
    };
    let Some(_claim) = crate::gse::claim_manual_dispatch(state, *generation, *recovery_origin) else {
        return true;
    };
    spawn(state.clone(), origin.clone());
    true
}


/// Deterministic preparation pauses exist only in the native regression build.
/// Every callsite is outside App/SDK/dispatch guards; no trace/policy is shipped.
#[cfg(test)]
pub(crate) mod coldflow_test_hooks {
    use crate::{sequence_progress::RunKey, state::AppState};
    use std::sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    };
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Stage {
        Initial,
        OriginCurrent,
        Final,
    }
    struct Hook {
        owner: usize,
        stage: Stage,
        used: AtomicBool,
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
        keys: Mutex<Vec<RunKey>>,
        key_count: AtomicUsize,
    }
    static HOOK: OnceLock<Mutex<Option<Arc<Hook>>>> = OnceLock::new();
    pub(crate) struct Guard {
        hook: Arc<Hook>,
        pub(crate) entered: mpsc::Receiver<()>,
        resume: mpsc::Sender<()>,
    }
    impl Guard {
        pub(crate) fn install(state: &AppState, stage: Stage) -> Self {
            let (send, entered) = mpsc::channel();
            let (resume, receive) = mpsc::channel();
            let hook = Arc::new(Hook {
                owner: state as *const AppState as usize,
                stage,
                used: AtomicBool::new(false),
                entered: send,
                resume: Mutex::new(receive),
                keys: Mutex::new(Vec::with_capacity(4)),
                key_count: AtomicUsize::new(0),
            });
            assert!(
                HOOK.get_or_init(Default::default)
                    .lock()
                    .unwrap()
                    .replace(hook.clone())
                    .is_none()
            );
            Self {
                hook,
                entered,
                resume,
            }
        }
        pub(crate) fn release(&self) {
            let _ = self.resume.send(());
        }
        pub(crate) fn keys(&self) -> (usize, Vec<RunKey>) {
            (
                self.hook.key_count.load(Ordering::SeqCst),
                self.hook.keys.lock().unwrap().clone(),
            )
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            self.release();
            let mut hook = HOOK.get().unwrap().lock().unwrap();
            if hook.as_ref().is_some_and(|h| Arc::ptr_eq(h, &self.hook)) {
                *hook = None;
            }
        }
    }
    fn selected(state: &AppState) -> Option<Arc<Hook>> {
        HOOK.get()?
            .lock()
            .unwrap()
            .as_ref()
            .filter(|h| h.owner == state as *const AppState as usize)
            .cloned()
    }
    pub(crate) fn pause(state: &AppState, stage: Stage) {
        if let Some(hook) = selected(state)
            && hook.stage == stage
            && !hook.used.swap(true, Ordering::SeqCst)
        {
            let _ = hook.entered.send(());
            hook.resume
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("outside-owner preparation pause release");
        }
    }
    pub(crate) fn reserved(state: &AppState, key: &RunKey) {
        if let Some(hook) = selected(state) {
            hook.key_count.fetch_add(1, Ordering::SeqCst);
            let mut keys = hook.keys.lock().unwrap();
            if keys.len() < 4 {
                keys.push(key.clone());
            }
        }
    }
}
