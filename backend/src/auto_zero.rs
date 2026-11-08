//! Bounded, monotonic-clock tare capture. Never blocks the telemetry receiver.
use crate::{loadcell, state::AppState, types::TelemetryCommand};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, sync::Arc, time::Duration};

mod capture_clock;
use capture_clock::CaptureClock;
pub(crate) mod before_launch;

const COUNT: usize = 200;
// The live DAQ stream is nominally 40 Hz (39.86 Hz in the incident recording).
// 200 samples consume five seconds before transport/scheduler jitter. Retain
// the same sample/quality minimum with two bounded seconds of delivery margin.
const NOMINAL_SAMPLES_PER_SECOND: u64 = 40;
const TRANSPORT_MARGIN_MS: u64 = 2_000;
const WINDOW: Duration = Duration::from_millis(
    (COUNT as u64 * 1_000).div_ceil(NOMINAL_SAMPLES_PER_SECOND) + TRANSPORT_MARGIN_MS,
);
const ZERO_KG: f32 = 0.8;

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct Settings {
    pub enabled: bool,
    /// Opt-in: zero KG1000 and apply it to the DAQ before Launch is sent.
    /// Omitted from the wire while off so existing peers see no change.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub before_launch: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true, before_launch: false }
    }
}
/// Partial operator update; an omitted field keeps its saved value.
#[derive(Clone, Copy, Default, Deserialize)]
pub struct SettingsUpdate {
    pub enabled: Option<bool>,
    pub before_launch: Option<bool>,
}
impl Settings {
    pub fn updated(self, update: SettingsUpdate) -> Self {
        Self {
            enabled: update.enabled.unwrap_or(self.enabled),
            before_launch: update.before_launch.unwrap_or(self.before_launch),
        }
    }
}
impl From<Settings> for SettingsUpdate {
    fn from(settings: Settings) -> Self {
        Self { enabled: Some(settings.enabled), before_launch: Some(settings.before_launch) }
    }
}

#[derive(Clone, Copy)]
struct Sample {
    at: CaptureClock,
    raw: f32,
    packet_id: Option<u64>,
}
#[derive(Default)]
pub struct Runtime {
    settings: Option<Settings>,
    kg50: VecDeque<Sample>,
    kg1000: VecDeque<Sample>,
    generation: u64,
    pending: bool,
    fill_pending: bool,
    // Authority observation only, never a measurement freshness timestamp.
    last_capture_clock: Option<CaptureClock>,
    launch: before_launch::Hold,
}
fn path() -> std::path::PathBuf {
    std::env::var_os("GS_AUTO_ZERO_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/auto_zero.json")
        })
}
impl Runtime {
    pub fn reset_kg50_input(&mut self) {
        self.cancel();
        self.kg50.clear();
    }

    pub fn pending(&self) -> bool {
        self.pending
    }
    pub fn settings(&mut self) -> Settings {
        *self
            .settings
            .get_or_insert_with(|| match std::fs::read(path()) {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                    log::error!("Invalid automatic-zero settings: {e}; automatic zero disabled");
                    Settings { enabled: false, before_launch: false }
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
                Err(e) => {
                    log::error!("Reading automatic-zero settings: {e}");
                    Settings { enabled: false, before_launch: false }
                }
            })
    }
    pub fn configure(&mut self, settings: Settings) -> Result<(), String> {
        self.configure_at(settings, &path())
    }

    fn configure_at(&mut self, settings: Settings, path: &std::path::Path) -> Result<(), String> {
        if self.pending {
            return Err("Wait for automatic zeroing to finish or cancel the action first".into());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec(&settings).unwrap())
            .map_err(|e| e.to_string())?;
        std::fs::rename(temporary, path).map_err(|e| e.to_string())?;
        self.settings = Some(settings);
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = false;
    }
    pub(crate) fn invalidate_captures_for_verified_reset(&mut self) {
        self.cancel();
        self.kg50.clear();
        self.kg1000.clear();
    }
    #[cfg(test)]
    pub(crate) fn capture_identity_for_test(&self) -> (u64, bool, bool) {
        (self.generation, self.pending, self.fill_pending)
    }
    pub(crate) fn fill_capture_pending(&self) -> bool {
        self.pending && self.fill_pending
    }
    pub(crate) fn completed_fill_capture(&self, token: u64) -> bool {
        self.generation == token && !self.pending && self.fill_pending
    }
    pub fn cancel_for_command(&mut self, command: &TelemetryCommand) {
        if matches!(
            command,
            TelemetryCommand::Abort | TelemetryCommand::CancelFill | TelemetryCommand::PauseFill
        ) {
            self.launch.cancel_requested();
        }
        if (self.pending && self.fill_pending && !matches!(command, TelemetryCommand::StartFill))
            || matches!(
                command,
                TelemetryCommand::Abort
                    | TelemetryCommand::CancelFill
                    | TelemetryCommand::PauseFill
            )
        {
            self.cancel();
        }
    }
    #[cfg(test)]
    pub fn testing(enabled: bool) -> Self {
        Self {
            settings: Some(Settings { enabled, before_launch: false }),
            ..Self::default()
        }
    }
    #[cfg(test)]
    pub(crate) fn testing_settings(settings: Settings) -> Self {
        Self {
            settings: Some(settings),
            ..Self::default()
        }
    }
    #[cfg(test)]
    pub fn observe(&mut self, sensor: &str, raw: f32) {
        self.observe_sample(sensor, raw, None);
    }
    #[cfg(test)]
    pub(crate) fn captured_source_counts_for_test(&self) -> [usize; 2] {
        [self.kg50.len(), self.kg1000.len()]
    }
    pub fn observe_packet(&mut self, sensor: &str, raw: f32, packet_id: u64) {
        self.observe_sample(sensor, raw, Some(packet_id));
    }
    fn invalidate_sample_eligibility(&mut self) {
        // Keep raw/ID/clock diagnostics, but never count these receipts again.
        // Raw telemetry and SD/recording rows are untouched by this capture policy.
        for sample in self.kg50.iter_mut().chain(self.kg1000.iter_mut()) { sample.at.invalidate(); }
    }
    fn observe_sample(&mut self, sensor: &str, raw: f32, packet_id: Option<u64>) {
        if !raw.is_finite() || !matches!(sensor, "KG50" | "KG1000") { return; }
        self.observe_captured(sensor, raw, packet_id, CaptureClock::now());
    }
    fn observe_captured(&mut self, sensor: &str, raw: f32, packet_id: Option<u64>, at: CaptureClock) {
        if !raw.is_finite() || !matches!(sensor, "KG50" | "KG1000") { return; }
        let changed = self.last_capture_clock.is_some_and(|previous| !previous.same_domain(at));
        if !at.is_valid() || changed {
            self.invalidate_sample_eligibility();
            if self.pending { self.cancel(); }
        }
        self.last_capture_clock = Some(at);
        let samples = match sensor {
            "KG50" => &mut self.kg50,
            "KG1000" => &mut self.kg1000,
            _ => return,
        };
        // A repeated network owner is not another independent stability
        // observation and must not renew this stream's last-receipt clock.
        if packet_id.is_some() && samples.iter().any(|sample| sample.packet_id == packet_id) {
            return;
        }
        samples.push_back(Sample {
            at,
            raw,
            packet_id,
        });
        while samples.len() > COUNT {
            samples.pop_front();
        }
    }
    fn samples(&self, sensor: &str, after: CaptureClock) -> Vec<f32> {
        let samples = if sensor == "KG50" {
            &self.kg50
        } else {
            &self.kg1000
        };
        samples
            .iter()
            .filter(|v| v.at.at_or_after(after))
            .map(|v| v.raw)
            .collect()
    }
}

/// Trim at most 5% on each tail. Require the remaining 90% to be within a
/// 0.8 kg-wide band; do not mistake sustained motion for isolated outliers.
fn stable(
    raw: &[f32],
    cfg: &loadcell::LoadcellCalibrationFile,
    sensor: &str,
) -> Result<(f32, bool), String> {
    stable_count(raw, cfg, sensor, COUNT)
}

fn stable_count(
    raw: &[f32],
    cfg: &loadcell::LoadcellCalibrationFile,
    sensor: &str,
    count: usize,
) -> Result<(f32, bool), String> {
    if count < 20 || raw.len() != count {
        return Err(format!("Need {count} fresh loadcell samples"));
    }
    if sensor == "KG50" && !cfg.extra_channels.contains_key("kg50") {
        return Err("Save a 50 kg loadcell calibration before automatic zeroing".into());
    }
    let mut values: Vec<(f32, f32)> = raw
        .iter()
        .map(|raw| {
            loadcell::calibrated_weight_kg(cfg, sensor, *raw)
                .filter(|kg| kg.is_finite())
                .map(|kg| (kg, *raw))
                .ok_or_else(|| "Loadcell calibration is invalid".to_string())
        })
        .collect::<Result<_, _>>()?;
    values.sort_by(|a, b| a.0.total_cmp(&b.0));
    let kept = &values[count / 20..count - count / 20];
    if kept.last().unwrap().0 - kept[0].0 > ZERO_KG {
        return Err("Loadcell is moving or unstable; no automatic zero was applied".into());
    }
    let mean_kg = kept.iter().map(|v| v.0 as f64).sum::<f64>() / kept.len() as f64;
    if mean_kg.abs() > ZERO_KG as f64 {
        return Err(
            "Load exceeds the ±0.8 kg automatic-zero limit; inspect the load and tare manually"
                .into(),
        );
    }
    Ok((
        (kept.iter().map(|v| v.1 as f64).sum::<f64>() / kept.len() as f64) as f32,
        kept.iter().all(|v| v.0.abs() <= ZERO_KG),
    ))
}

/// Returns true when Start Fill was intercepted. Resume never erases fill mass.
pub fn before_fill(state: &Arc<AppState>, cmd: &TelemetryCommand) -> bool {
    if !matches!(cmd, TelemetryCommand::StartFill) || crate::gse::active(state) {
        return false;
    }
    begin(
        state,
        state.fill_targets_snapshot().fill_source.sensor(),
        true,
    )
}

/// Starts concurrently with launch dispatch, never adding to countdown time.
#[cfg(any(feature = "hitl_mode", feature = "test_fire_mode", test))]
pub fn launch(state: &Arc<AppState>) {
    begin(state, "KG1000", false);
}


#[cfg(test)]
type CommitSaveHook = (usize, Box<dyn FnOnce() + Send>);
#[cfg(test)]
static COMMIT_SAVE_HOOK: std::sync::Mutex<Option<CommitSaveHook>> = std::sync::Mutex::new(None);
#[cfg(test)]
fn pause_commit_save_for_test(state: &AppState) {
    let mut slot = COMMIT_SAVE_HOOK.lock().unwrap();
    let hook = if slot
        .as_ref()
        .is_some_and(|(owner, _)| *owner == state as *const AppState as usize)
    {
        slot.take().map(|(_, hook)| hook)
    } else {
        None
    };
    drop(slot);
    if let Some(hook) = hook {
        hook();
    }
}

/// The ORIGINAL capture clock and the bounded budget it was admitted with.
#[derive(Clone, Copy)]
struct CaptureBudget {
    start: CaptureClock,
    window: Duration,
}

#[derive(Debug)]
enum TareCommitError {
    NotPublished(String),
    PublishedUncertain(String),
}
impl From<String> for TareCommitError {
    fn from(message: String) -> Self { Self::NotPublished(message) }
}
impl From<&str> for TareCommitError {
    fn from(message: &str) -> Self { Self::NotPublished(message.into()) }
}
impl std::fmt::Display for TareCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { Self::NotPublished(message) | Self::PublishedUncertain(message) => f.write_str(message) }
    }
}
/// Existing direct guard fixtures admit a fresh capture at this test boundary.
/// Production always supplies the ORIGINAL begin() clock to the captured path.
#[cfg(test)]
fn commit_tare(
    state: &AppState, cfg: &loadcell::LoadcellCalibrationFile,
    sensor: &str, token: u64, arm_generation: u64, raw: f32,
) -> Result<(), String> {
    commit_tare_captured(state, cfg, sensor, token, arm_generation, raw, CaptureBudget { start: CaptureClock::now(), window: WINDOW })
        .map_err(|error| error.to_string())
}

fn commit_tare_captured(
    state: &AppState,
    cfg: &loadcell::LoadcellCalibrationFile,
    sensor: &str,
    token: u64,
    arm_generation: u64,
    raw: f32,
    budget: CaptureBudget,
) -> Result<(), TareCommitError> {
    let CaptureBudget { start: original_capture, window } = budget;
    if !original_capture.within(window) {
        return Err("Original automatic-zero capture deadline expired or its clock changed".into());
    }
    // Acquire before zero Runtime; retain the lease across save without holding DISPATCH.
    let _claim = crate::gse::claim_operator_dispatch(state, arm_generation)
        .ok_or_else(|| "Automatic zero cancelled by closed admission/reset".to_string())?;
    if crate::gse::manual_stop_pending(state) {
        return Err("Automatic zero cancelled by manual Stop".into());
    }
    if !original_capture.within(window) {
        return Err("Original automatic-zero capture deadline expired while checking admission".into());
    }
    // Lock order: zero Runtime -> calibration. No DISPATCH/GSE acquisition or
    // SDK calls while these locks are held. Verified reset must cancel and
    // advance the global epoch while holding this same zero Runtime mutex.
    let runtime = state.auto_zero.lock().unwrap();
    if matches!(arm_generation, 0 | u64::MAX)
        || crate::gse::automatic_generation(state) != arm_generation
        || runtime.generation != token
    {
        return Err("Automatic zero cancelled by reset".into());
    }
    let mut current = state.loadcell_calibration.lock().unwrap();
    if serde_json::to_value(&*current).ok() != serde_json::to_value(cfg).ok() {
        return Err("Calibration changed during automatic zero; retry".into());
    }
    let mut updated = current.clone();
    loadcell::capture_zero(
        &mut updated,
        if sensor == "KG50" { "kg50" } else { "ch1" },
        raw,
    );
    #[cfg(test)]
    pause_commit_save_for_test(state);
    if !original_capture.within(window) {
        return Err("Original automatic-zero capture deadline expired before persistence".into());
    }
    // CaptureClock may read the explicit model clock file. These are the
    // EXISTING zero/calibration persistence locks, never DISPATCH/Stop/GSE.
    // The final guard executes immediately before atomic file replacement.
    let saved = loadcell::save_guarded(&updated, |_| {
        if original_capture.within(window) { Ok(()) }
        else { Err("Original automatic-zero deadline expired before calibration replacement".into()) }
    })?;
    // A successful rename is actual publication, even if directory fsync
    // finishes after the deadline. Mirror that truth rather than fake rollback.
    *current = updated;
    match saved {
        loadcell::GuardedSaveOutcome::Durable => Ok(()),
        loadcell::GuardedSaveOutcome::PublishedUncertain(error) => Err(TareCommitError::PublishedUncertain(error)),
    }
}

fn cancel_after_capture_failure(state: &Arc<AppState>, arm_generation: u64) {
    crate::gse::pause_fill_for_recoverable_fault_for_generation(
        state,
        "automatic zero failed",
        arm_generation,
    );
}

fn begin(state: &Arc<AppState>, sensor: &'static str, fill: bool) -> bool {
    let arm_generation = crate::gse::automatic_generation(state);
    let stop_pending = crate::gse::manual_stop_pending(state);
    let cfg = state.loadcell_calibration.lock().unwrap().clone();
    let start = CaptureClock::now();
    let (token, original_continuation) = {
        let mut rt = state.auto_zero.lock().unwrap();
        if !rt.settings().enabled {
            return false;
        }
        if matches!(arm_generation, 0 | u64::MAX) || stop_pending {
            return true;
        }
        if !start.is_valid() || rt.last_capture_clock.is_some_and(|previous| !previous.same_domain(start)) {
            rt.invalidate_sample_eligibility();
            if rt.pending { rt.cancel(); }
        }
        rt.last_capture_clock = Some(start);
        if rt.pending {
            return true;
        }
        if fill {
            let recent = rt.samples(sensor, start.earlier(WINDOW));
            // Require a live stream, not merely a recently stopped sensor.
            let history = if sensor == "KG50" {
                &rt.kg50
            } else {
                &rt.kg1000
            };
            let fresh = history
                .back()
                .is_some_and(|v| start.at_or_after(v.at) && start.duration_since(v.at) < Duration::from_millis(250));
            if fresh && matches!(stable(&recent, &cfg, sensor), Ok((_, true))) {
                return false;
            }
        }
        rt.cancel();
        let original_continuation = if fill {
            let prepared = state.fill_recording.continuation.lock().unwrap().clone();
            let Some(prepared) = prepared else {
                return true;
            };
            if prepared.origin.generation() != arm_generation
                || prepared.origin.remaining().is_err()
                || !state.fill_recording.current(&prepared.receipt)
            {
                return true;
            }
            prepared
                .zero_generation
                .store(rt.generation, std::sync::atomic::Ordering::Release);
            Some(prepared)
        } else { None };
        rt.pending = true;
        rt.fill_pending = fill;
        (rt.generation, original_continuation)
    };
    state.add_notification_with_persistence(
        format!(
            "Collecting 200 fresh {sensor} samples for automatic zero (up to {} seconds)",
            WINDOW.as_secs()
        ),
        false,
    );
    let state = state.clone();
    let mut shutdown = state.shutdown_subscribe();
    tokio::spawn(async move {
        #[cfg(test)]
        let _task_terminal = terminal_sync_tests::task_terminal(&state);
        let result = loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(10)) => {},
                _ = shutdown.recv() => {
                    let mut runtime = state.auto_zero.lock().unwrap();
                    if runtime.generation == token { runtime.cancel(); }
                    return;
                }
            }
            if crate::gse::automatic_generation(&state) != arm_generation
                || crate::gse::manual_stop_pending(&state)
            {
                let mut rt = state.auto_zero.lock().unwrap();
                if rt.generation == token {
                    rt.cancel();
                }
                return;
            }
            let raw = {
                let rt = state.auto_zero.lock().unwrap();
                if rt.generation != token {
                    return;
                }
                rt.samples(sensor, start)
            };
            // Never extend a capture by retrying or accept samples after its bounded budget.
            if !start.within(WINDOW) {
                break Err(format!(
                    "200 stable samples were not captured within the {}-second acquisition budget",
                    WINDOW.as_secs()
                ));
            }
            if raw.len() == COUNT {
                break stable(&raw, &cfg, sensor).map(|v| v.0);
            }
        };
        // The first step was admitted before its original5s deadline. This
        // accepted capture keeps WINDOW (7s), but never adopts new authority.
        if fill {
            let mut slice = std::time::Instant::now();
            let authorization = loop {
                if !start.within(WINDOW) { break Err(crate::fill_recording::Error::Stale); }
                let outcome = crate::fill_recording::authorize_zero_continuation(&state, arm_generation).await;
                if !start.within(WINDOW) { break Err(crate::fill_recording::Error::Stale); }
                match outcome {
                    Err(crate::fill_recording::Error::PendingBusy) => {
                        let Some(remaining) = WINDOW.checked_sub(start.elapsed()).filter(|v| !v.is_zero()) else {
                            break Err(crate::fill_recording::Error::Stale);
                        };
                        // Keep the SAME accepted continuation and its original
                        // capture clock; Busy is neither manual Stop nor revoke.
                        tokio::time::sleep(remaining.min(Duration::from_millis(5))).await;
                        if slice.elapsed() >= Duration::from_millis(100) {
                            tokio::task::yield_now().await;
                            slice = std::time::Instant::now();
                        }
                    }
                    outcome => break outcome,
                }
            };
            if let Err(error) = authorization {
                let mut rt = state.auto_zero.lock().unwrap();
                if rt.generation == token { rt.cancel(); }
                drop(rt);
                crate::fill_recording::notify_failure(&state, &error);
                return;
            }
        }
        let result = result.map_err(TareCommitError::NotPublished)
            .and_then(|raw| commit_tare_captured(&state, &cfg, sensor, token, arm_generation, raw, CaptureBudget { start, window: WINDOW }));
        {
            let mut rt = state.auto_zero.lock().unwrap();
            if rt.generation != token {
                return;
            }
            if crate::gse::automatic_generation(&state) != arm_generation {
                rt.cancel();
                return;
            }
            if result.is_err() { rt.cancel(); }
            else { rt.pending = false; }
        }
        if crate::gse::automatic_generation(&state) != arm_generation
            || crate::gse::manual_stop_pending(&state)
        {
            return;
        }
        match result {
            Ok(()) => {
                state.broadcast_fill_targets_snapshot();
                let cfg = state.loadcell_calibration.lock().unwrap().clone();
                if let Err(e) =
                    crate::gse::sync_calibration_after_automatic_zero(&state, arm_generation, &cfg)
                {
                    #[cfg(test)]
                    terminal_sync_tests::after_actual_sync_error(&state).await;
                    // The tare is already saved. Retire only this original
                    // capture/Fill owner; a newer run must remain untouched.
                    {
                        let mut rt = state.auto_zero.lock().unwrap();
                        if rt.generation != token
                            || crate::gse::automatic_generation(&state) != arm_generation
                        {
                            return;
                        }
                        rt.cancel();
                    }
                    if let Some(original) = original_continuation.as_ref() {
                        let removed = {
                            let mut current = state.fill_recording.continuation.lock().unwrap();
                            if current.as_ref().is_some_and(|owner| {
                                Arc::ptr_eq(owner, original)
                                    && owner.origin.generation() == arm_generation
                                    && owner.zero_generation.load(std::sync::atomic::Ordering::Acquire) == token
                            }) {
                                current.take()
                            } else { None }
                        };
                        if let Some(removed) = removed { removed.origin.retire(); }
                    }
                    state.add_notification(format!(
                        "Zero saved locally, but DAQ calibration sync failed: {e}. Fill was not started."
                    ));
                    return;
                }
                state.add_notification_with_persistence(
                    format!("{sensor} automatic zero saved"),
                    false,
                );
                if fill && state.is_command_allowed(&TelemetryCommand::StartFill) {
                    crate::gse::start_fill_after_automatic_zero(&state, arm_generation);
                }
            }
            Err(e) => {
                state.add_notification(format!(
                    "Automatic zero failed: {e}. {}",
                    match &e {
                        TareCommitError::PublishedUncertain(_) => "The new tare was applied, but its persistence is uncertain; verify calibration before use. Fill was not started.",
                        TareCommitError::NotPublished(_) if fill => "Fill was not started.",
                        TareCommitError::NotPublished(_) => "Previous tare retained; manual zero required.",
                    }
                ));
                cancel_after_capture_failure(&state, arm_generation);
            }
        }
    });
    true
}

#[cfg(test)]
pub(crate) fn acquisition_budget_for_test() -> Duration {
    WINDOW
}#[cfg(all(test, any(feature = "hitl_mode", feature = "firmware_sim")))]
pub(crate) async fn verify_paused_capture_cancellation_and_shutdown(state: &Arc<AppState>) {
    assert!(
        crate::simulation_clock_ms().is_some(),
        "This contract requires an explicit paused model"
    );
    *state.auto_zero.lock().unwrap() = Runtime::testing(true);
    launch(state);
    assert!(state.auto_zero.lock().unwrap().pending());
    state.auto_zero.lock().unwrap().cancel();
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!state.auto_zero.lock().unwrap().pending());
    launch(state);
    assert!(state.auto_zero.lock().unwrap().pending());
    state.request_shutdown();
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!state.auto_zero.lock().unwrap().pending());

}



#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> loadcell::LoadcellCalibrationFile {
        let mut cfg = loadcell::LoadcellCalibrationFile::default();
        cfg.ch1.m = Some(1.0);
        cfg.ch1.b = Some(0.0);
        cfg.ch1_zero_raw = None;
        cfg
    }
    #[test]
    fn replayed_network_owner_does_not_renew_quality_or_freshness() {
        let mut runtime = Runtime::default();
        runtime.observe_packet("KG1000", 0.2, 0);
        let original = runtime.kg1000[0].at;
        runtime.observe_packet("KG1000", 0.7, 0);
        assert_eq!(runtime.kg1000.len(), 1);
        assert_eq!(runtime.kg1000[0].raw, 0.2);
        assert_eq!(
            runtime.kg1000[0].at.duration_since(original),
            Duration::ZERO
        );
        // One logical packet may legitimately contain independent channels.
        runtime.observe_packet("KG50", 0.3, 0);
        assert_eq!(runtime.kg50.len(), 1);
    }

    #[tokio::test]
    async fn computed_zero_from_previous_reset_generation_cannot_save_or_start_fill() {
        let state = crate::state::tests::test_app_state().await;
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "auto_zero/fixtures/incident-2026-10-31-kg1000.json"
        ))
        .unwrap();
        let cfg: loadcell::LoadcellCalibrationFile =
            serde_json::from_value(fixture["calibration"].clone()).unwrap();
        let samples: Vec<f32> = fixture["samples"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["raw"].as_f64().unwrap() as f32)
            .collect();
        let (computed_raw, _) = stable(&samples, &cfg, "KG1000").unwrap();
        *state.loadcell_calibration.lock().unwrap() = cfg.clone();
        let token = {
            let mut runtime = state.auto_zero.lock().unwrap();
            *runtime = Runtime::testing(true);
            runtime.pending = true;
            runtime.observe("KG50", 0.1);
            runtime.observe("KG1000", 0.2);
            runtime.generation
        };
        let calibration_file = loadcell::calibration_file_for_test();
        let before_disk = std::fs::read(&calibration_file).ok();
        let armed = crate::gse::automatic_generation(&state);
        // The result has ALREADY passed the real 200-sample quality check.
        // Hold its actual commit on another thread while the real staged reset
        // helper wins DISPATCH -> zeroRuntime and invalidates the epoch/token.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let late_state = state.clone();
        let late_cfg = cfg.clone();
        let late = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            resume_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            commit_tare(&late_state, &late_cfg, "KG1000", token, armed, computed_raw)
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let next = crate::gse::reset_generation::invalidate_capture_epoch(&state, armed).unwrap();
        assert_eq!(next, armed + 1);
        {
            let runtime = state.auto_zero.lock().unwrap();
            assert!(!runtime.pending && runtime.kg50.is_empty() && runtime.kg1000.is_empty());
        }
        resume_tx.send(()).unwrap();
        assert!(late.join().unwrap().is_err());
        let token_after_reset = state.auto_zero.lock().unwrap().generation;
        assert!(
            commit_tare(
                &state,
                &cfg,
                "KG1000",
                token_after_reset,
                armed,
                computed_raw
            )
            .is_err(),
            "the global arm guard must reject even a matching local token"
        );
        assert!(
            commit_tare(&state, &cfg, "KG1000", token, next, computed_raw).is_err(),
            "the local arm guard must reject even a matching global epoch"
        );
        assert_eq!(
            serde_json::to_value(&*state.loadcell_calibration.lock().unwrap()).unwrap(),
            serde_json::to_value(&cfg).unwrap()
        );
        assert_eq!(std::fs::read(&calibration_file).ok(), before_disk);
        *state.latest_fill_mass_kg.lock().unwrap() = Some(42.0);
        crate::gse::start_fill_after_automatic_zero(&state, armed);
        assert_eq!(*state.latest_fill_mass_kg.lock().unwrap(), Some(42.0));
        assert!(!crate::gse::active(&state));
        // The current epoch also cannot commit while a real retained Stop owns safety.
        state.manual_abort_intent.request();
        assert!(
            commit_tare(
                &state,
                &cfg,
                "KG1000",
                token,
                crate::gse::automatic_generation(&state),
                computed_raw
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&calibration_file).ok(), before_disk);
    }

    #[test]
    fn zero_band_ignores_isolated_outliers() {
        let mut raw = vec![0.2; COUNT];
        raw[0] = 900.;
        raw[1] = -900.;
        let (mean, zero) = stable(&raw, &config(), "KG1000").unwrap();
        assert!(zero);
        assert!((mean - 0.2).abs() < 0.001);
    }
    #[test]
    fn movement_load_and_missing_samples_are_rejected() {
        assert!(stable(&vec![2.; COUNT], &config(), "KG1000").is_err());
        assert!(stable(&vec![0.; COUNT - 1], &config(), "KG1000").is_err());
        let raw: Vec<_> = (0..COUNT)
            .map(|i| if i % 2 == 0 { -0.7 } else { 0.7 })
            .collect();
        assert!(stable(&raw, &config(), "KG1000").is_err());
    }
    #[test]
    fn history_is_bounded_and_capture_excludes_old_samples() {
        let mut rt = Runtime::default();
        for _ in 0..500 {
            rt.observe("KG1000", 0.0);
        }
        assert_eq!(rt.kg1000.len(), COUNT);
        // Consecutive Instant::now() calls can share a clock tick. Use an
        // explicit cutoff so this test does not depend on host clock precision.
        let latest = rt.kg1000.back().unwrap().at;
        assert!(!rt.samples("KG1000", latest).is_empty());
        assert!(
            rt.samples("KG1000", latest.later(Duration::from_nanos(1)))
                .is_empty()
        );
        rt.pending = true;
        let old = rt.generation;
        rt.cancel();
        assert!(!rt.pending);
        assert_ne!(old, rt.generation);
    }
    #[test]
    fn manual_actions_cancel_a_pending_fill_but_duplicate_start_does_not() {
        let mut rt = Runtime::testing(true);
        rt.pending = true;
        rt.fill_pending = true;
        rt.cancel_for_command(&TelemetryCommand::StartFill);
        assert!(rt.pending);
        rt.cancel_for_command(&TelemetryCommand::Dump);
        assert!(!rt.pending);
        rt.pending = true;
        rt.fill_pending = false;
        rt.cancel_for_command(&TelemetryCommand::Abort);
        assert!(!rt.pending);
    }
    #[test]
    fn zero_band_boundaries_and_sustained_outliers() {
        assert!(stable(&vec![0.8; COUNT], &config(), "KG1000").unwrap().1);
        assert!(stable(&vec![-0.8; COUNT], &config(), "KG1000").unwrap().1);
        let mut values = vec![0.0; COUNT];
        values[..20].fill(2.0);
        assert!(stable(&values, &config(), "KG1000").is_err());
        assert!(stable(&values, &config(), "KG50").is_err());
    }

    #[test]
    fn toggle_is_written_to_disk_and_pending_capture_cannot_change_it() {
        let dir = std::env::temp_dir().join(format!(
            "seds-auto-zero-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("auto_zero.json");
        let mut rt = Runtime::testing(true);
        rt.configure_at(Settings { enabled: false, before_launch: false }, &path).unwrap();
        let loaded: Settings = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(!loaded.enabled);
        rt.pending = true;
        assert!(rt.configure_at(Settings { enabled: true, before_launch: false }, &path).is_err());
        let loaded: Settings = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(!loaded.enabled);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
    struct ReleaseCommitSave(Option<std::sync::mpsc::Sender<()>>, usize);
    impl Drop for ReleaseCommitSave {
        fn drop(&mut self) {
            let mut slot = COMMIT_SAVE_HOOK.lock().unwrap();
            if slot.as_ref().is_some_and(|(owner, _)| *owner == self.1) {
                slot.take();
            }
            drop(slot);
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    #[tokio::test]
    async fn closed_commit_tare_does_not_change_actual_file_or_calibration() {
        if crate::operator_queue::isolated_case(
            module_path!(),
            "closed_commit_tare_does_not_change_actual_file_or_calibration",
        ) {
            return;
        }
        let state = crate::state::tests::test_app_state().await;
        let cfg = config();
        *state.loadcell_calibration.lock().unwrap() = cfg.clone();
        let token = state.auto_zero.lock().unwrap().generation;
        let armed = crate::gse::automatic_generation(&state);
        let path = loadcell::calibration_file_for_test();
        let before = std::fs::read(&path).ok();
        state
            .operator_admission_barrier
            .store(77, std::sync::atomic::Ordering::Release);
        let result = commit_tare(&state, &cfg, "KG1000", token, armed, 0.123);
        let after = std::fs::read(&path).ok();
        if let Some(bytes) = &before {
            std::fs::write(&path, bytes).unwrap();
        } else {
            let _ = std::fs::remove_file(&path);
        }
        assert!(
            result.is_err(),
            "closed worker must not start disk/RAM effects"
        );
        assert_eq!(before, after);
        assert_eq!(
            serde_json::to_value(&*state.loadcell_calibration.lock().unwrap()).unwrap(),
            serde_json::to_value(&cfg).unwrap()
        );
    }
    #[tokio::test]
    async fn held_save_worker_claim_survives_awaiter_cancellation_and_blocks_drain() {
        if crate::operator_queue::isolated_case(
            module_path!(),
            "held_save_worker_claim_survives_awaiter_cancellation_and_blocks_drain",
        ) {
            return;
        }
        let state = crate::state::tests::test_app_state().await;
        let cfg = config();
        *state.loadcell_calibration.lock().unwrap() = cfg.clone();
        let token = state.auto_zero.lock().unwrap().generation;
        let armed = crate::gse::automatic_generation(&state);
        let path = loadcell::calibration_file_for_test();
        let before = std::fs::read(&path).ok();
        let (ready, reached) = tokio::sync::oneshot::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let release = ReleaseCommitSave(Some(release), Arc::as_ptr(&state) as usize);
        *COMMIT_SAVE_HOOK.lock().unwrap() = Some((
            Arc::as_ptr(&state) as usize,
            Box::new(move || {
                let _ = ready.send(());
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
            }),
        ));
        let (finished, done) = tokio::sync::oneshot::channel();
        let worker_state = state.clone();
        let outer = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let result = commit_tare(&worker_state, &cfg, "KG1000", token, armed, 0.123);
                let _ = finished.send(result);
            })
            .await
            .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(2), reached)
            .await
            .unwrap()
            .unwrap();
        outer.abort();
        let _ = outer.await;
        state
            .operator_admission_barrier
            .store(77, std::sync::atomic::Ordering::Release);
        let fence = crate::operator_queue::Token::capture(&state, 77);
        let inflight = state
            .operator_dispatch_inflight
            .load(std::sync::atomic::Ordering::Acquire);
        let drain_allowed = crate::gse::with_closed_operator_queue(&state, fence, || ()).is_some();
        let disk_while_paused = std::fs::read(&path).ok();
        state.manual_abort_intent.request();
        drop(release); // RAII releases on every assertion/early-return path too
        let outcome = tokio::time::timeout(Duration::from_secs(2), done)
            .await
            .unwrap()
            .unwrap();
        let after = std::fs::read(&path).ok();
        if let Some(bytes) = &before {
            std::fs::write(&path, bytes).unwrap();
        } else {
            let _ = std::fs::remove_file(&path);
        }
        outcome.unwrap();
        assert_eq!(
            inflight, 1,
            "claim must belong to real blocking save worker"
        );
        assert!(!drain_allowed, "cancelled awaiter is not worker quiescence");
        assert_eq!(disk_while_paused, before);
        assert_ne!(after, before, "actual save still completes under its lease");
        assert_eq!(
            state
                .operator_dispatch_inflight
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
        assert!(state.manual_abort_intent.pending_epoch().is_some());
        assert!(crate::gse::with_closed_operator_queue(&state, fence, || ()).is_none());
    }

    #[tokio::test]
    async fn late_capture_error_cannot_cancel_new_run_but_fresh_error_still_cancels() {
        if crate::operator_queue::isolated_case(
            module_path!(),
            "late_capture_error_cannot_cancel_new_run_but_fresh_error_still_cancels",
        ) {
            return;
        }
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(crate::types::FlightState::Idle);
        let old = crate::gse::automatic_generation(&state);
        state
            .automatic_generation
            .store(old + 1, std::sync::atomic::Ordering::Release);
        state.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Filling;
        cancel_after_capture_failure(&state, old);
        let after_old = state.gse.lock().unwrap().engine.status.phase;
        // Also exercise the unchanged fresh/current fault path even in oldRED.
        state.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Filling;
        cancel_after_capture_failure(&state, old + 1);
        let after_fresh = state.gse.lock().unwrap().engine.status.phase;
        assert_ne!(after_fresh, gse_sequence::Phase::Filling);
        assert_eq!(after_old, gse_sequence::Phase::Filling);
    }
}

#[cfg(test)]
#[path = "auto_zero/deadline_tests.rs"]
pub(crate) mod deadline_tests;

#[cfg(test)]
#[path = "auto_zero/terminal_sync_tests.rs"]
mod terminal_sync_tests;
