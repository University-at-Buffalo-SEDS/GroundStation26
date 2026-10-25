//! Purpose clock for explicit simulator procedures. UI receipt/authentication and
//! media-retention wall clocks remain independent of this context.
use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Context {
    wall_ms: u64,
    model: Option<Model>,
}
#[derive(Clone, Copy, Debug)]
struct Model {
    monotonic_ms: u64,
    utc_ms: u64,
}
static EPOCH: OnceLock<u64> = OnceLock::new();

impl Context {
    fn from_samples(
        wall_ms: u64,
        virtual_ms: Option<u64>,
        configured_epoch: Option<u64>,
        epoch: &OnceLock<u64>,
    ) -> Self {
        let model = virtual_ms.map(|monotonic_ms| Model {
            monotonic_ms,
            utc_ms: super::periodic::source_utc_ms(
                wall_ms,
                Some(monotonic_ms),
                configured_epoch,
                epoch,
            ),
        });
        Self { wall_ms, model }
    }
    pub(crate) fn utc_ms(self) -> u64 {
        self.model.map_or(self.wall_ms, |model| model.utc_ms)
    }
    pub(crate) fn timestamp_ms(self) -> i64 {
        self.utc_ms().min(i64::MAX as u64) as i64
    }
    pub(crate) fn model_monotonic_ms(self) -> Option<u64> {
        self.model.map(|model| model.monotonic_ms)
    }
    /// Transport-only snapshot. Never feed it back into the canonical clock or
    /// DAQ publisher: a sampled duration deliberately has no UTC anchor.
    pub(crate) fn presentation(self, clock: &LaunchClockMsg) -> LaunchClockMsg {
        if self.model.is_none() || clock.kind == LaunchClockKind::Idle {
            return clock.clone();
        }
        let duration_ms = match (clock.kind, clock.anchor_timestamp_ms, clock.duration_ms) {
            (LaunchClockKind::TMinus, Some(anchor), Some(duration)) => duration
                .saturating_sub(self.timestamp_ms().saturating_sub(anchor))
                .max(0),
            (LaunchClockKind::TMinus, Some(target), None) => {
                target.saturating_sub(self.timestamp_ms()).max(0)
            }
            (LaunchClockKind::TPlus, Some(anchor), _) => {
                self.timestamp_ms().saturating_sub(anchor).max(0)
            }
            _ => return clock.clone(),
        };
        LaunchClockMsg {
            kind: clock.kind,
            anchor_timestamp_ms: None,
            duration_ms: Some(duration_ms),
        }
    }
}

/// Per-client UI deduplication only. It has no router/DAQ publisher or state
/// mutation callbacks, so a paused model cannot create protocol traffic.
#[derive(Default)]
pub(crate) struct HeldSamples {
    last: Option<LaunchClockMsg>,
}
impl HeldSamples {
    pub(crate) fn remember(&mut self, clock: &LaunchClockMsg) {
        self.last = Some(clock.clone());
    }
    pub(crate) fn changed(&mut self, clock: LaunchClockMsg) -> Option<LaunchClockMsg> {
        if self.last.as_ref() == Some(&clock) {
            return None;
        }
        self.remember(&clock);
        Some(clock)
    }
}

pub(crate) fn for_wall(wall_ms: u64) -> Context {
    let virtual_ms = crate::simulation_clock_ms();
    // A configured cohort epoch is relevant only to an explicit model clock.
    let configured_epoch = virtual_ms
        .and_then(|_| std::env::var_os("GS_SIM_NETWORK_EPOCH_MS"))
        .map(|value| {
            value
                .to_str()
                .expect("simulation UTC epoch must be UTF-8")
                .parse::<u64>()
                .expect("simulation UTC epoch must be milliseconds since Unix epoch")
        });
    Context::from_samples(wall_ms, virtual_ms, configured_epoch, &EPOCH)
}
pub(crate) fn now() -> Context {
    for_wall(super::get_current_timestamp_ms())
}
pub(crate) fn timestamp_ms() -> i64 {
    now().timestamp_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    const EPOCH_MS: u64 = 1_767_225_600_000;
    fn model(wall: u64, mono: u64) -> Context {
        Context::from_samples(wall, Some(mono), Some(EPOCH_MS), &OnceLock::new())
    }
    #[test]
    fn slow_and_paused_models_export_held_samples_without_mutating_anchors() {
        let start = model(1_792_000_000_000, 100_000);
        let clock = crate::state::launch_countdown_clock(start.timestamp_ms());
        let slow = model(1_792_000_010_000, 100_833);
        assert_eq!(slow.presentation(&clock).duration_ms, Some(9167));
        assert_eq!(slow.presentation(&clock).anchor_timestamp_ms, None);
        let paused = model(1_792_000_120_000, 100_833);
        assert_eq!(slow.presentation(&clock), paused.presentation(&clock));
        assert_eq!(clock.anchor_timestamp_ms, Some((EPOCH_MS + 100_000) as i64));
        let plus = LaunchClockMsg {
            kind: LaunchClockKind::TPlus,
            anchor_timestamp_ms: Some((EPOCH_MS + 110_000) as i64),
            duration_ms: None,
        };
        assert_eq!(
            model(1_792_001_000_000, 112_500)
                .presentation(&plus)
                .duration_ms,
            Some(2500)
        );
        assert_eq!(
            model(1_792_002_000_000, 112_500)
                .presentation(&plus)
                .duration_ms,
            Some(2500)
        );
    }
    #[test]
    fn normal_and_ordinary_hitl_clocks_preserve_exact_wire_values() {
        let clock = crate::state::launch_countdown_clock(1000);
        let context = Context::from_samples(2000, None, Some(EPOCH_MS), &OnceLock::new());
        assert_eq!(context.timestamp_ms(), 2000);
        assert_eq!(context.model_monotonic_ms(), None);
        assert_eq!(context.presentation(&clock), clock);
        assert_eq!(
            context.presentation(&LaunchClockMsg::idle()),
            LaunchClockMsg::idle()
        );
    }
    #[test]
    fn paused_ui_ticks_are_deduplicated_and_never_change_canonical_clock() {
        let clock = crate::state::launch_countdown_clock((EPOCH_MS + 100_000) as i64);
        let mut publisher = HeldSamples::default();
        assert!(
            publisher
                .changed(model(1_792_000_000_000, 100_000).presentation(&clock))
                .is_some()
        );
        for wall_tick in 1..=240 {
            // one minute of250ms wall polls, frozen model
            assert!(
                publisher
                    .changed(
                        model(1_792_000_000_000 + wall_tick * 250, 100_000).presentation(&clock)
                    )
                    .is_none()
            );
        }
        assert!(
            publisher
                .changed(model(1_792_000_060_000, 100_833).presentation(&clock))
                .is_some()
        );
        assert_eq!(clock.anchor_timestamp_ms, Some((EPOCH_MS + 100_000) as i64));
    }
}

#[cfg(all(test, feature = "hitl_mode"))]
mod file_tests;
