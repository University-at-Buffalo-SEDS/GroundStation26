//! FC SD card health and `FcSdRetry` (doc sections 6.1a and 7.5), mirroring
//! the DAQ's SD health and `DaqSdRetry` (crate::daq_sd).
//!
//! FC BOARD_HEALTH word 6: bits 0..23 are the FC's watchdog task mask, bits
//! 24..28 its command gate (crate::fc_command_gate masks exactly those), bit
//! 29 "this image serves the SD read service (P2P port 4511)", bit 30 "SD not
//! ready" and bit 31 "an FcSdRetry is pending". Both images report them; older
//! FC firmware leaves them clear (bit 29 clear then reads as "no service").
use super::*;

/// Word 6 bit 30, shifted down by 30.
pub(crate) const NOT_READY: u32 = 1;
/// Word 6 bit 31, shifted down by 30.
pub(crate) const RETRY_PENDING: u32 = 2;
pub(crate) const COMMAND_NAME: &str = "FcSdRetry";
pub(crate) const ALERT: &str = "FC SD card not mounted / not writable";
const CLEARED: &str = "FC SD card mounted and writable again.";
pub(crate) const COOLDOWN: Duration = Duration::from_secs(5);

/// The FC's SD bits (word 6 bits 30..31, shifted down) of a BOARD_HEALTH.
pub(crate) fn bits_from_health(words: &[u32]) -> Option<u32> {
    (words.len() == 24).then(|| words[6] >> 30)
}

/// Word 6 bit 29 of an FC BOARD_HEALTH: the image serves SD reads.
pub(crate) fn service_from_health(words: &[u32]) -> Option<bool> {
    (words.len() == 24).then(|| words[6] & SERVICE_BIT != 0)
}
const SERVICE_BIT: u32 = 1 << 29;

/// Whether the FC's image serves the SD read service (port 4511), from a
/// fresh FC BOARD_HEALTH; `None` without one.
pub(crate) fn service(state: &AppState) -> Option<bool> {
    let inner = state.igt.inner.lock().unwrap();
    inner
        .sd_service
        .filter(|(_, at)| Instant::now().saturating_duration_since(*at) <= REPORT_FRESH)
        .map(|(service, _)| service)
}

/// The SD bits of a fresh FC report.
pub(super) fn fresh_bits(inner: &Inner, now: Instant) -> Option<u32> {
    inner
        .sd
        .filter(|(_, at)| now.saturating_duration_since(*at) <= REPORT_FRESH)
        .map(|(bits, _)| bits)
}

/// A test fire has no AV bay: the FC's SD is not alerted there.
fn quiet() -> bool {
    crate::station_mode::local_mode() == crate::station_mode::Mode::TestFire
}

pub(super) fn observe_at(state: &AppState, words: &[u32], now: Instant) {
    let Some(bits) = bits_from_health(words) else {
        return;
    };
    let mut inner = state.igt.inner.lock().unwrap();
    inner.sd = Some((bits, now));
    inner.sd_service = service_from_health(words).map(|service| (service, now));
    let raised = inner.sd_alert.is_some();
    let not_ready = bits & NOT_READY != 0 && !quiet();
    if not_ready == raised {
        return;
    }
    if not_ready {
        // Reserve the slot before raising so a racing report does not raise twice.
        inner.sd_alert = Some(0);
        drop(inner);
        log::warn!("{ALERT}");
        crate::web::emit_warning(state, ALERT);
        let id = state.add_notification(ALERT);
        state.igt.inner.lock().unwrap().sd_alert = Some(id);
    } else {
        let id = inner.sd_alert.take();
        drop(inner);
        if let Some(id) = id {
            let _ = state.dismiss_notification(id);
        }
        log::info!("{CLEARED}");
        state.add_temporary_notification(CLEARED);
    }
}

/// An FC BOARD_HEALTH report arrived.
pub(super) fn observe_health(state: &AppState, words: &[u32]) {
    observe_at(state, words, Instant::now());
}

/// The station-side gate of `FcSdRetry`: the FC is present in this mode, a
/// fresh FC report, no retry already pending (bit 31), and 5 s since the last
/// request. The `FcSdRetry` command permission is checked by the dispatcher.
// Safety catalog (About safety): fc-sd-retry. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn retry_gate(state: &AppState, now: Instant) -> Result<(), String> {
    if crate::runtime_mode::is_test_fire() {
        return Err(crate::runtime_mode::unavailable(COMMAND_NAME));
    }
    if crate::rrls::is_rrls() {
        return Err("Retry FC SD is sent only by the primary GroundStation.".into());
    }
    let inner = state.igt.inner.lock().unwrap();
    let Some(bits) = fresh_bits(&inner, now) else {
        return Err("No recent FC BOARD_HEALTH; Retry FC SD needs the FC's current SD status.".into());
    };
    if bits & RETRY_PENDING != 0 {
        return Err("An FC SD retry is already pending.".into());
    }
    if inner
        .sd_last_retry
        .is_some_and(|at| now.saturating_duration_since(at) < COOLDOWN)
    {
        return Err("Wait five seconds between FC SD retry requests.".into());
    }
    Ok(())
}

/// Queue `FcSdRetry` (FC byte 33) after its gate; starts the cooldown.
pub(crate) fn send(state: &AppState, router: &sedsnet::router::Router) -> Result<(), String> {
    let now = Instant::now();
    retry_gate(state, now)?;
    state.igt.inner.lock().unwrap().sd_last_retry = Some(now);
    let byte = crate::rocket_commands::FlightComputerCommands::FcSdRetry as u8;
    crate::telemetry_task::queue_locally_routed_flight_command(state, router, "FcSdRetry command", &[byte])
        .map_err(|error| format!("FcSdRetry was not queued: {error:?}"))?;
    crate::telemetry_task::flush_command_tx(router, "FcSdRetry command tx");
    log::info!("FcSdRetry command sent to the flight computer ({byte})");
    Ok(())
}

/// Test fixtures: the FC reported its SD bits at `at`.
#[cfg(test)]
pub(crate) fn observe_for_test(state: &AppState, bits: u32, at: Instant) {
    let mut words = [0u32; 24];
    words[6] = bits << 30;
    observe_at(state, &words, at);
}

/// Test fixtures: the FC reported (or not) the SD read service at `at`.
#[cfg(test)]
pub(crate) fn observe_service_for_test(state: &AppState, service: bool, at: Instant) {
    let mut words = [0u32; 24];
    words[6] = if service { SERVICE_BIT } else { 0 };
    observe_at(state, &words, at);
}

#[cfg(test)]
pub(crate) fn note_retry_for_test(state: &AppState, at: Instant) {
    state.igt.inner.lock().unwrap().sd_last_retry = Some(at);
}
