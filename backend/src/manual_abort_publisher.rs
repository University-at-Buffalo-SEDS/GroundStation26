//! Publishes a retained operator Abort, independent of the ordinary command
//! channel and of the station's runtime: on its own thread, so nothing the
//! runtime's few workers are waiting for (a disk, a lock held across a disk
//! write) delays it. Local SDK callbacks retain observations and never wait
//! on GSE.
use crate::{
    manual_abort_intent::{Claim, Recording},
    manual_abort_utc::Published,
    state::AppState,
};
use sedsnet::router::Router;
use std::sync::Arc;

/// Run `service` on a thread and runtime of its own.
fn on_own_thread(name: &str, service: impl Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build()?;
    std::thread::Builder::new().name(name.to_string()).spawn(move || runtime.block_on(service)).map(drop)
}

/// The publisher, its recorder and the re-send of an Abort the radio lost.
/// The handle finishes when the publisher and the recorder have.
pub(crate) fn spawn(state: Arc<AppState>, router: Arc<Router>) -> std::io::Result<tokio::task::JoinHandle<()>> {
    // Records wait on disks and logs: never on the publisher.
    let (records, owed) = std::sync::mpsc::channel::<(Unrecorded, Recording)>();
    let recorder = state.clone();
    let (working, finished) = tokio::sync::oneshot::channel::<()>();
    std::thread::Builder::new().name("abort_record".to_string()).spawn(move || {
        let _working = working;
        // Ends with the publisher, which owns the sender.
        for (unrecorded, recording) in owed {
            unrecorded.record(&recorder);
            drop(recording);
        }
    })?;
    on_own_thread(
        "abort_resend",
        crate::manual_abort_resend::service(state.clone(), router.clone(), state.shutdown_subscribe()),
    )?;
    let (intent, shutdown) = (state.manual_abort_intent.clone(), state.shutdown_subscribe());
    on_own_thread(
        "abort_publish",
        crate::manual_abort_intent::service_recording(
            intent,
            shutdown,
            move |claim| publish(&state, &router, claim),
            move |unrecorded, recording| {
                let _ = records.send((unrecorded, recording));
            },
        ),
    )?;
    Ok(tokio::spawn(async move {
        let _ = finished.await;
    }))
}

struct Unrecorded {
    published: Published,
    epoch: u64,
    av_bay_routed: bool,
}
impl Unrecorded {
    fn record(self, state: &Arc<AppState>) {
        self.published.record(state);
        let (epoch, current) = (self.epoch, state.manual_abort_intent.epoch());
        if self.av_bay_routed {
            log::info!("manual Abort accepted by SDK epoch={epoch} current={current}");
        } else {
            // The SDK sends ABORT only along fresh discovery routes:
            // with no AV-bay route it went to the umbilical or nowhere.
            log::error!(
                "manual Abort accepted by SDK epoch={epoch} current={current}, but NOT sent to the AV bay: \
                 no fresh discovery route to RF/FC/PB; held for re-send once the AV bay is rediscovered"
            );
        }
    }
}

/// What is left to record once the SDK owns this claim's Abort. None: refused,
/// and the bounded RAM intent is kept and retried without any I/O.
fn publish(state: &Arc<AppState>, router: &Arc<Router>, claim: &mut Claim) -> Option<Unrecorded> {
    // Read before the publish: a route that appears in between costs at
    // most one extra ABORT, never a missing one.
    let mark = crate::manual_abort_resend::send_mark(router);
    let av_bay_routed = mark.av_bay_routed();
    let published = crate::manual_abort_utc::publish_local_claim_unrecorded(state, router, claim, |claim| {
        state.board_abort.request_abort_confirmation();
        if let Some(wire) = claim.wire() {
            let recovery = crate::board_recovery::coordinator::recovery_activity(state).flatten();
            state.abort_resend.arm(claim.epoch(), wire, recovery, mark);
        }
    })
    .ok()??;
    Some(Unrecorded { published, epoch: claim.epoch(), av_bay_routed })
}
