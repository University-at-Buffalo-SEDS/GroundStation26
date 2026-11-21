//! Opt-in KG1000 zero that runs BEFORE Launch is sent.
//!
//! The operator's Launch press is held while a bounded tare capture runs and
//! the result is applied to the DAQ. Only then is the original Launch handed
//! back to the ordinary send path, which starts T-10 and the DAQ launch file.
//! The held press owns no authority of its own: the send path re-checks the
//! press-time Stop/Abort ticket, board Abort revision, launch lock and
//! Turbocharge rules exactly as an immediate press would.
use super::{CaptureBudget, CaptureClock, Sample, TareCommitError, commit_tare_captured, stable_count};
use crate::{loadcell, state::AppState};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

/// Upper bound on the whole capture, from Launch press to tare decision.
pub(crate) const WINDOW: Duration = Duration::from_secs(10);
/// Never zero from fewer samples than this, whatever the measured rate.
pub(crate) const MIN_SAMPLES: usize = 100;
/// Aim to finish within about five seconds at the measured rate.
const TARGET: Duration = Duration::from_secs(5);
/// Keep one second of the window for transport jitter.
const MARGIN: Duration = Duration::from_secs(1);
const RATE_HISTORY: Duration = Duration::from_secs(5);
const LIVE: Duration = Duration::from_millis(250);
const SENSOR: &str = "KG1000";
pub(crate) const ALREADY_PENDING: &str =
    "Launch is already waiting for the loadcell zero. Press Cancel to stop it.";

static NEXT_HOLD: AtomicU64 = AtomicU64::new(1);

/// The one Launch press held behind a zero, if any.
#[derive(Default)]
pub(super) struct Hold {
    id: u64,
    capture: Option<(u64, CaptureClock)>,
    samples: usize,
    cancel: bool,
}
impl Hold {
    /// An operator Abort, Cancel Fill or Pause Fill cancels the held Launch;
    /// it never turns into "zero failed, launch anyway".
    pub(super) fn cancel_requested(&mut self) {
        if self.id != 0 {
            self.cancel = true;
        }
    }
}

/// Live progress for the UI while a Launch is held.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    pub remaining_s: u64,
    pub samples: u32,
    /// The zero is finished and the Launch is being sent; Cancel no longer applies.
    pub sending: bool,
}

/// What the send path is told about the zero.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// New tare saved and handed to the DAQ.
    Zeroed,
    /// No new tare was applied to the DAQ; the previous calibration is in use.
    Failed(String),
}

pub(crate) fn enabled(state: &AppState) -> bool {
    state.auto_zero.lock().unwrap().settings().before_launch
}

pub(crate) fn held(state: &AppState) -> bool {
    state.auto_zero.lock().unwrap().launch.id != 0
}

/// Presentation only. Never waits on the zero runtime.
pub(crate) fn status(state: &AppState) -> Option<Status> {
    let (capture, samples) = {
        let rt = state.auto_zero.try_lock().ok()?;
        if rt.launch.id == 0 {
            return None;
        }
        (rt.launch.capture, rt.launch.samples)
    };
    let samples = u32::try_from(samples).unwrap_or(u32::MAX);
    Some(match capture {
        Some((_, start)) => Status {
            remaining_s: WINDOW.saturating_sub(start.elapsed()).as_secs_f64().ceil() as u64,
            samples,
            sending: false,
        },
        None => Status { remaining_s: 0, samples, sending: true },
    })
}

/// Operator Cancel. Only the capture phase can be cancelled; once the Launch
/// is being sent, Stop is the control that applies.
pub(crate) fn cancel(state: &AppState) -> Result<(), &'static str> {
    let mut rt = state.auto_zero.lock().unwrap();
    if rt.launch.id == 0 {
        return Err("No Launch is waiting for a loadcell zero");
    }
    let Some((token, _)) = rt.launch.capture else {
        return Err("The loadcell zero is finished and Launch is being sent; use Stop to abort");
    };
    rt.launch.cancel = true;
    if rt.generation == token {
        rt.cancel();
    }
    Ok(())
}

fn measured_rate_hz(samples: &VecDeque<Sample>, now: CaptureClock) -> Result<f64, String> {
    let since = now.earlier(RATE_HISTORY);
    let recent: Vec<CaptureClock> = samples
        .iter()
        .map(|sample| sample.at)
        .filter(|at| at.at_or_after(since) && now.at_or_after(*at))
        .collect();
    let (Some(first), Some(last)) = (recent.first(), recent.last()) else {
        return Err("no live KG1000 samples are arriving from the DAQ".into());
    };
    if now.duration_since(*last) >= LIVE || recent.len() < 2 {
        return Err("no live KG1000 samples are arriving from the DAQ".into());
    }
    let span = last.duration_since(*first).as_secs_f64();
    if span <= 0.0 {
        return Err("the KG1000 sample rate could not be measured".into());
    }
    Ok((recent.len() - 1) as f64 / span)
}

/// About five seconds of samples at the measured rate, never fewer than
/// `MIN_SAMPLES` and never more than the capture history holds.
pub(super) fn sample_count(rate_hz: f64) -> Result<usize, String> {
    if !rate_hz.is_finite() || rate_hz <= 0.0 {
        return Err("the KG1000 sample rate could not be measured".into());
    }
    let count = ((rate_hz * TARGET.as_secs_f64()).floor() as usize).clamp(MIN_SAMPLES, super::COUNT);
    if count as f64 / rate_hz > WINDOW.saturating_sub(MARGIN).as_secs_f64() {
        return Err(format!(
            "KG1000 arrives at {rate_hz:.1} Hz, too slow for {MIN_SAMPLES} samples within {} seconds",
            WINDOW.as_secs()
        ));
    }
    Ok(count)
}

enum Check {
    Continue,
    Cancelled(&'static str),
    Interrupted,
}
fn check(state: &AppState, arm_generation: u64, token: u64) -> Check {
    if crate::gse::automatic_generation(state) != arm_generation {
        return Check::Cancelled("a reset");
    }
    if crate::gse::manual_stop_pending(state) {
        return Check::Cancelled("Stop");
    }
    let rt = state.auto_zero.lock().unwrap();
    if rt.generation == token {
        Check::Continue
    } else if rt.launch.cancel {
        Check::Cancelled("Cancel or Abort")
    } else {
        Check::Interrupted
    }
}

/// Clears the hold however the task ends, including shutdown and panic.
struct Release {
    state: Arc<AppState>,
    id: u64,
}
impl Drop for Release {
    fn drop(&mut self) {
        if let Ok(mut rt) = self.state.auto_zero.lock()
            && rt.launch.id == self.id
        {
            if let Some((token, _)) = rt.launch.capture
                && rt.generation == token
                && rt.pending
            {
                rt.cancel();
            }
            rt.launch = Hold::default();
        }
        self.state.broadcast_action_policy_snapshot();
    }
}

/// Holds this Launch press behind a zero, then calls `send` with the outcome.
/// `send` is not called if the Launch is cancelled (Cancel, Stop, Abort,
/// reset or shutdown). Returns an error, and holds nothing, when another
/// Launch is already held.
pub(crate) fn start<F, Fut>(state: &Arc<AppState>, send: F) -> Result<(), String>
where
    F: FnOnce(Outcome) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let arm_generation = crate::gse::automatic_generation(state);
    if matches!(arm_generation, 0 | u64::MAX) || crate::gse::manual_stop_pending(state) {
        return Err("Manual Stop or reset is pending; Launch was not sent".into());
    }
    let cfg = state.loadcell_calibration.lock().unwrap().clone();
    let start = CaptureClock::now();
    let id = NEXT_HOLD.fetch_add(1, Ordering::Relaxed);
    let (token, plan) = {
        let mut rt = state.auto_zero.lock().unwrap();
        if rt.launch.id != 0 {
            return Err(ALREADY_PENDING.into());
        }
        if !start.is_valid()
            || rt.last_capture_clock.is_some_and(|previous| !previous.same_domain(start))
        {
            rt.invalidate_sample_eligibility();
        }
        rt.last_capture_clock = Some(start);
        let plan = measured_rate_hz(&rt.kg1000, start)
            .and_then(|rate| sample_count(rate).map(|count| (rate, count)));
        // This press owns the only capture from here on.
        rt.cancel();
        let token = rt.generation;
        rt.pending = plan.is_ok();
        rt.fill_pending = false;
        rt.launch = Hold {
            id,
            capture: plan.is_ok().then_some((token, start)),
            samples: plan.as_ref().map_or(0, |(_, count)| *count),
            cancel: false,
        };
        (token, plan)
    };
    if let Ok((rate, count)) = &plan {
        state.add_notification(format!(
            "Zeroing loadcell before Launch: capturing {count} KG1000 samples at {rate:.1} Hz (up to {} s). Press Cancel to stop the Launch.",
            WINDOW.as_secs()
        ));
    }
    state.broadcast_action_policy_snapshot();
    let state = state.clone();
    let mut shutdown = state.shutdown_subscribe();
    tokio::spawn(async move {
        let release = Release { state: state.clone(), id };
        let outcome = match plan {
            Err(reason) => Outcome::Failed(reason),
            Ok((_, count)) => {
                let budget = CaptureBudget { start, window: WINDOW };
                match capture(&state, &cfg, token, arm_generation, budget, count, &mut shutdown).await {
                    Some(outcome) => outcome,
                    None => return,
                }
            }
        };
        {
            let mut rt = state.auto_zero.lock().unwrap();
            if rt.launch.id != id {
                return;
            }
            rt.launch.capture = None;
        }
        state.broadcast_action_policy_snapshot();
        send(outcome).await;
        drop(release);
    });
    Ok(())
}

/// `None` means the Launch was cancelled (already reported) or the station is
/// shutting down.
async fn capture(
    state: &Arc<AppState>,
    cfg: &loadcell::LoadcellCalibrationFile,
    token: u64,
    arm_generation: u64,
    budget: CaptureBudget,
    count: usize,
    shutdown: &mut tokio::sync::broadcast::Receiver<()>,
) -> Option<Outcome> {
    let cancelled = |reason: &str| {
        let mut rt = state.auto_zero.lock().unwrap();
        if rt.generation == token {
            rt.cancel();
        }
        drop(rt);
        state.add_notification(format!(
            "Launch cancelled by {reason} during the loadcell zero. Launch was not sent."
        ));
        None
    };
    let mut shown = u64::MAX;
    let raw = loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(10)) => {},
            _ = shutdown.recv() => {
                let mut rt = state.auto_zero.lock().unwrap();
                if rt.generation == token { rt.cancel(); }
                return None;
            }
        }
        match check(state, arm_generation, token) {
            Check::Continue => {}
            Check::Cancelled(reason) => return cancelled(reason),
            Check::Interrupted => {
                return Some(Outcome::Failed("the capture was interrupted".into()));
            }
        }
        let raw = {
            let rt = state.auto_zero.lock().unwrap();
            rt.samples(SENSOR, budget.start)
        };
        if raw.len() >= count {
            break stable_count(&raw[..count], cfg, SENSOR, count).map(|(raw, _)| raw);
        }
        if !budget.start.within(budget.window) {
            break Err(format!(
                "only {} of {count} KG1000 samples arrived within {} seconds",
                raw.len(),
                budget.window.as_secs()
            ));
        }
        let remaining = budget.window.saturating_sub(budget.start.elapsed()).as_secs();
        if remaining != shown {
            shown = remaining;
            state.broadcast_action_policy_snapshot();
        }
    };
    let committed = raw.map_err(TareCommitError::NotPublished).and_then(|raw| {
        commit_tare_captured(state, cfg, SENSOR, token, arm_generation, raw, budget)
    });
    // A Stop, Abort, Cancel or reset that raced the commit still cancels.
    let interrupted = match check(state, arm_generation, token) {
        Check::Continue => false,
        Check::Cancelled(reason) => return cancelled(reason),
        Check::Interrupted => true,
    };
    {
        let mut rt = state.auto_zero.lock().unwrap();
        if rt.generation == token {
            if committed.is_ok() { rt.pending = false } else { rt.cancel() }
        }
    }
    let error = match committed {
        Ok(()) if !interrupted => None,
        Ok(()) => Some(
            "the capture was interrupted after the tare was saved; it was not sent to the DAQ".to_string(),
        ),
        Err(TareCommitError::PublishedUncertain(error)) => Some(format!(
            "the tare was saved but its persistence is uncertain ({error}); it was not sent to the DAQ"
        )),
        Err(TareCommitError::NotPublished(error)) => Some(error),
    };
    if let Some(error) = error {
        return Some(Outcome::Failed(error));
    }
    state.broadcast_fill_targets_snapshot();
    let applied = state.loadcell_calibration.lock().unwrap().clone();
    if let Err(error) =
        crate::gse::sync_calibration_after_automatic_zero(state, arm_generation, &applied)
    {
        return match check(state, arm_generation, token) {
            Check::Cancelled(reason) => cancelled(reason),
            _ => Some(Outcome::Failed(format!(
                "the zero was saved locally but sending it to the DAQ failed: {error}"
            ))),
        };
    }
    state.add_notification("KG1000 zero saved and sent to the DAQ; sending Launch");
    Some(Outcome::Zeroed)
}

#[cfg(test)]
#[path = "before_launch_tests.rs"]
mod tests;
