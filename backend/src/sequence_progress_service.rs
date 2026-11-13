//! Bounded observational publication. Called after producer/control locks are released.
use super::sequence_progress::{Error, Journal, RunKey, Snapshot, VERSION};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub const MAX_RESULT_LINKS: usize = 200; // Same bounded lifetime as notification history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultLink {
    pub key: RunKey,
    pub notification_id: u64,
    pub timestamp_ms: i64,
    pub warning: bool,
}
/// Mutable presentation metadata, separate from immutable run/watch revisions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoardDisplay {
    pub connected: bool,
    pub known: bool,
    pub boot: Option<String>,
    pub epoch: Option<u32>,
    pub outputs: Option<u32>,
    pub outputs_age_ms: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u16,
    pub backend_instance: String,
    pub reset_epoch: String,
    /// Only versioned, run-correlated cancellation is advertised.
    pub cancel_api_version: u16,
    pub active: Vec<Snapshot>,
    pub terminal: Vec<Snapshot>,
    pub reporting_error: Option<String>,
    #[serde(default)]
    pub result_links: Vec<ResultLink>,
    #[serde(default)]
    pub board_display: std::collections::BTreeMap<String, BoardDisplay>,
    /// Live Pause/Resume/Cancel admission for the current run, if any.
    #[serde(default)]
    pub controls: Vec<super::sequence_progress::RunControls>,
}
pub struct Service {
    instance: String,
    #[cfg(test)]
    serial: AtomicU64,
    publication_gate: Arc<Mutex<()>>,
    result_links: VecDeque<ResultLink>,
    journal: Journal,
    observations: VecDeque<(RunKey, u64)>,
    reporting_error: Option<String>,
}
impl Service {
    /// The random instance nonce is created once outside every control lock.
    pub fn new(instance: u128) -> Self {
        Self {
            instance: format!("{instance:032x}"),
            #[cfg(test)]
            serial: AtomicU64::new(0),
            publication_gate: Arc::new(Mutex::new(())),
            result_links: VecDeque::with_capacity(MAX_RESULT_LINKS),
            journal: Journal::default(),
            observations: VecDeque::with_capacity(36),
            reporting_error: None,
        }
    }
    pub fn publication_gate(&self) -> Arc<Mutex<()>> {
        self.publication_gate.clone()
    }
    pub fn report_error(&mut self, error: Error) {
        self.reporting_error = Some(format!("Sequence reporting unavailable: {error:?}"));
    }
    #[cfg(test)]
    pub fn reserve(&self, reset_epoch: u64) -> Result<RunKey, Error> {
        let mut previous = self.serial.load(Ordering::Relaxed);
        let serial = loop {
            let next = previous.checked_add(1).ok_or(Error::Bounds)?;
            match self.serial.compare_exchange_weak(
                previous,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break next,
                Err(current) => previous = current,
            }
        };
        Ok(RunKey {
            backend_instance: self.instance.clone(),
            reset_epoch: format!("{reset_epoch:016x}"),
            run_id: format!(
                "{:016x}{serial:016x}",
                u64::from_str_radix(&self.instance[..16], 16).map_err(|_| Error::Invalid)?
            ),
        })
    }
    /// Returns a terminal notification intent exactly once after actual acceptance.
    /// The caller inserts it using the existing notifier, then binds its real ID/time.
    pub fn publish(&mut self, snapshot: Snapshot, now_ms: u64) -> Result<Option<Snapshot>, Error> {
        let result = if snapshot.key.backend_instance != self.instance {
            Err(Error::Stale)
        } else if snapshot.revision == 1 {
            self.journal.accepted_start(snapshot.clone())
        } else {
            self.journal.observe(snapshot.clone())
        };
        if let Err(error) = result {
            self.reporting_error = Some(format!("Sequence reporting unavailable: {error:?}"));
            return Err(error);
        }
        self.observations.retain(|(key, _)| {
            self.journal
                .active()
                .iter()
                .chain(self.journal.terminal().iter())
                .any(|s| &s.key == key)
        });
        if let Some(row) = self
            .observations
            .iter_mut()
            .find(|(key, _)| key == &snapshot.key)
        {
            row.1 = now_ms;
        } else {
            if self.observations.len() == 36 {
                self.observations.pop_front();
            }
            self.observations.push_back((snapshot.key.clone(), now_ms));
        }
        Ok(snapshot.outcome.terminal().then_some(snapshot))
    }
    pub fn bind_notification(&mut self, key: &RunKey, id: u64, at_ms: i64) -> Result<(), Error> {
        if self
            .result_links
            .iter()
            .any(|old| old.key != *key && old.notification_id == id && old.timestamp_ms == at_ms)
        {
            return Err(Error::Conflict);
        }
        self.journal.bind_notification(key, id, at_ms)?;
        let link = ResultLink {
            key: key.clone(),
            notification_id: id,
            timestamp_ms: at_ms,
            warning: self.journal.result(key)?.outcome != super::sequence_progress::Outcome::Passed,
        };
        if let Some(existing) = self.result_links.iter().find(|old| &old.key == key) {
            return if existing == &link {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        if self
            .result_links
            .iter()
            .any(|old| old.notification_id == id && old.timestamp_ms == at_ms)
        {
            return Err(Error::Conflict);
        }
        if self.result_links.len() == MAX_RESULT_LINKS {
            self.result_links.pop_front();
        }
        self.result_links.push_back(link);
        Ok(())
    }
    /// True while any run is not terminal. Used to refuse radio reconfiguration.
    pub fn has_active_run(&self) -> bool {
        !self.journal.active().is_empty()
    }
    pub fn result(&self, key: &RunKey) -> Result<Snapshot, Error> {
        self.journal.result(key).cloned()
    }
    pub fn envelope(&self, reset_epoch: u64, now_ms: u64) -> Envelope {
        let decorate = |mut value: Snapshot| {
            value.observed_age_ms = self
                .observations
                .iter()
                .find(|(key, _)| key == &value.key)
                .and_then(|(_, at)| now_ms.checked_sub(*at))
                .unwrap_or(u64::MAX);
            if !value.outcome.terminal() && value.key.reset_epoch != format!("{reset_epoch:016x}") {
                value.reporting_error = Some("The run's reset epoch is no longer current".into());
                value.cancel_allowed = false;
            }
            if let Some(error) = &self.reporting_error {
                value.reporting_error = Some(error.clone());
                value.cancel_allowed = false;
            }
            value
        };
        Envelope {
            board_display: Default::default(),
            controls: Vec::new(),
            version: VERSION,
            backend_instance: self.instance.clone(),
            reset_epoch: format!("{reset_epoch:016x}"),
            cancel_api_version: VERSION,
            active: self
                .journal
                .active()
                .iter()
                .cloned()
                .map(decorate)
                .collect(),
            terminal: self
                .journal
                .terminal()
                .iter()
                .map(Snapshot::compact_result)
                .map(decorate)
                .collect(),
            reporting_error: self.reporting_error.clone(),
            result_links: self.result_links.iter().cloned().collect(),
        }
    }
}

/// Separate reporting receipt clock; never a firmware/launch/control clock.
pub fn now_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// IDs are allocated without waiting for the journal/projector mutex. The latter
/// are used only by post-control reporting and HTTP readers.
pub struct Shared {
    inner: Mutex<Service>,
    instance: u128,
    serial: AtomicU64,
    pub projector: Mutex<crate::sequence_progress_stage::Projector>,
}
impl Shared {
    pub fn new(instance: u128) -> Self {
        Self {
            inner: Mutex::new(Service::new(instance)),
            instance,
            serial: AtomicU64::new(0),
            projector: Mutex::new(Default::default()),
        }
    }
    pub fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, Service>> {
        self.inner.lock()
    }
    pub fn backend_instance(&self) -> String {
        format!("{:032x}", self.instance)
    }
    pub fn reserve(&self, epoch: u64) -> Result<RunKey, Error> {
        let mut current = self.serial.load(Ordering::Relaxed);
        let run = loop {
            let next = current.checked_add(1).ok_or(Error::Bounds)?;
            match self.serial.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break next,
                Err(now) => current = now,
            }
        };
        Ok(RunKey {
            backend_instance: format!("{:032x}", self.instance),
            reset_epoch: format!("{epoch:016x}"),
            run_id: format!("{run:032x}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequence_progress::{Kind, Outcome};
    fn accepted(key: RunKey) -> Snapshot {
        Snapshot {
            version: 1,
            key,
            revision: 1,
            kind: Kind::Fill,
            outcome: Outcome::Running,
            origin_client: None,
            title: "Fill".into(),
            message: "Accepted".into(),
            current_step: None,
            next_step: None,
            steps: vec![],
            observed_age_ms: 0,
            valid_for_ms: 2000,
            cancel_allowed: true,
            cold_flow_release_allowed: false,
            notification_id: None,
            notification_timestamp_ms: None,
            reporting_error: None,
            watch_history_truncated: false,
            details_complete: true,
        }
    }
    #[test]
    fn only_actual_published_starts_exist_and_result_binds_real_notification() {
        let mut service = Service::new(31);
        let key = service.reserve(4).unwrap();
        assert!(service.envelope(4, 100).active.is_empty());
        let mut value = accepted(key.clone());
        assert!(service.publish(value.clone(), 100).unwrap().is_none());
        let observed = service.envelope(4, 300);
        assert_eq!(observed.active[0].observed_age_ms, 200);
        value.revision += 1;
        value.outcome = Outcome::Cancelled;
        value.cancel_allowed = false;
        let intent = service.publish(value, 400).unwrap().unwrap();
        assert_eq!(intent.key, key);
        service.bind_notification(&key, 27, 12345).unwrap();
        assert_eq!(service.result(&key).unwrap().notification_id, Some(27));
        assert_eq!(
            service.envelope(4, 900).terminal[0].notification_timestamp_ms,
            Some(12345)
        );
    }
    #[test]
    fn terminal_polling_is_compact_while_full_result_keeps_every_step() {
        let mut service = Service::new(34);
        let key = service.reserve(1).unwrap();
        let mut snapshot = accepted(key.clone());
        snapshot.steps = (0..160)
            .map(|i| crate::sequence_progress::Step {
                id: format!("step-{i}"),
                label: "Recorded actual step".repeat(3),
                state: crate::sequence_progress::StepState::Passed,
                watch: vec![],
            })
            .collect();
        service.publish(snapshot.clone(), 100).unwrap();
        snapshot.revision = 2;
        snapshot.outcome = Outcome::Passed;
        snapshot.cancel_allowed = false;
        service.publish(snapshot, 200).unwrap();
        let header = service.envelope(1, 300);
        assert!(!header.terminal[0].details_complete);
        assert!(header.terminal[0].steps.is_empty());
        assert!(serde_json::to_vec(&header).unwrap().len() < 2048);
        let result = service.result(&key).unwrap();
        assert!(result.details_complete);
        assert_eq!(result.steps.len(), 160);
        assert!(serde_json::to_vec(&result).unwrap().len() > 10_000);
    }
    #[test]
    fn pruned_full_results_keep_bounded_authoritative_notification_links() {
        let mut service = Service::new(35);
        let mut first = None;
        for i in 1..=201 {
            let key = service.reserve(1).unwrap();
            if i == 2 {
                first = Some(key.clone());
            }
            let mut value = accepted(key.clone());
            service.publish(value.clone(), i).unwrap();
            value.revision = 2;
            value.outcome = Outcome::Passed;
            value.cancel_allowed = false;
            service.publish(value, i).unwrap();
            service.bind_notification(&key, i, i as i64).unwrap();
        }
        let envelope = service.envelope(1, 300);
        assert_eq!(envelope.terminal.len(), 32);
        assert_eq!(envelope.result_links.len(), MAX_RESULT_LINKS);
        let key = first.unwrap();
        assert_eq!(service.result(&key), Err(Error::Unavailable));
        assert!(
            envelope
                .result_links
                .iter()
                .any(|link| link.key == key && link.notification_id == 2)
        );
        assert!(
            !envelope
                .result_links
                .iter()
                .any(|link| link.notification_id == 1)
        );
        assert!(
            serde_json::to_vec(&envelope).unwrap().len() < 96 * 1024,
            "no full result arrays are polled"
        );
    }
    #[test]
    fn epoch_change_or_reporting_gap_disables_cancellation_without_control_effects() {
        let mut service = Service::new(32);
        let key = service.reserve(4).unwrap();
        let mut value = accepted(key.clone());
        service.publish(value.clone(), 100).unwrap();
        let reset = service.envelope(5, 200);
        assert!(!reset.active[0].cancel_allowed);
        assert!(reset.active[0].reporting_error.is_some());
        value.revision = 3;
        assert_eq!(service.publish(value, 300), Err(Error::RevisionGap));
        let after = service.envelope(4, 400);
        assert_eq!(after.active[0].revision, 1);
        assert!(!after.active[0].cancel_allowed);
        assert!(after.reporting_error.is_some());
        assert_ne!(service.reserve(4).unwrap(), key);
        let restarted = Service::new(33);
        assert_eq!(restarted.result(&key), Err(Error::Unavailable));
    }
}
