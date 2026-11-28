//! Optional analytics journal, deliberately separate from the safety clock file
//! and signed clock wire shape. Failure only makes the aggregate anchor unknown.
use crate::launch_history::ClockRecord;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Record {
    authority: String,
    session: String,
    action_clock_revision: u64,
    launch_action_utc_ms: i64,
}
#[derive(Default)]
struct State {
    record: Option<Record>,
    durable: bool,
}
#[derive(Default)]
pub(crate) struct Journal {
    path: Option<PathBuf>,
    state: Arc<Mutex<State>>,
}
impl Journal {
    pub(crate) fn open(path: PathBuf) -> Self {
        let record = std::fs::read(&path)
            .ok()
            .filter(|bytes| bytes.len() <= 4096)
            .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok());
        Self {
            path: Some(path),
            state: Arc::new(Mutex::new(State {
                durable: record.is_some(),
                record,
            })),
        }
    }
    pub(crate) fn action(&self, clock: &ClockRecord) -> Option<i64> {
        let state = self.state.lock().ok()?;
        let record = state.record.as_ref()?;
        (state.durable
            && record.authority == clock.authority
            && record.session == clock.session
            && record.action_clock_revision <= clock.revision)
            .then_some(record.launch_action_utc_ms)
    }
    pub(crate) fn record(&self, clock: &ClockRecord, anchor: i64) {
        let record = Record {
            authority: clock.authority.clone(),
            session: clock.session.clone(),
            action_clock_revision: clock.revision,
            launch_action_utc_ms: anchor,
        };
        if let Ok(mut state) = self.state.lock() {
            state.record = Some(record.clone());
            state.durable = self.path.is_none();
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        let state = self.state.clone();
        // Never wait for disk while admitting Launch or alter its result.
        let _ = std::thread::Builder::new()
            .name("launch-analytics".into())
            .spawn(move || {
                let saved = (|| -> std::io::Result<()> {
                    use std::io::Write;
                    let parent = path.parent().unwrap_or(std::path::Path::new("."));
                    std::fs::create_dir_all(parent)?;
                    let temporary = path.with_extension(format!(
                        "analytics-pending-{}-{}",
                        std::process::id(),
                        record.action_clock_revision
                    ));
                    let mut file = std::fs::File::create(&temporary)?;
                    file.write_all(&serde_json::to_vec(&record)?)?;
                    file.sync_all()?;
                    std::fs::rename(&temporary, &path)?;
                    std::fs::File::open(parent)?.sync_all()?;
                    Ok(())
                })()
                .is_ok();
                if let Ok(mut state) = state.lock()
                    && state.record.as_ref() == Some(&record)
                {
                    state.durable = saved;
                }
            });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_by_default_identity_bound_and_no_t0_guess() {
        let mut store = crate::launch_history::Store::ephemeral();
        let plus = store
            .update_primary(
                crate::telemetry_db::LaunchClockMsg {
                    kind: crate::telemetry_db::LaunchClockKind::TPlus,
                    anchor_timestamp_ms: Some(50_000),
                    duration_ms: None,
                },
                50_000,
                0,
            )
            .unwrap();
        let journal = Journal::default();
        assert_eq!(journal.action(&plus), None);
        journal.record(&plus, 40_000);
        assert_eq!(journal.action(&plus), Some(40_000));
        let reset = store.reset_primary(60_000, 10_000).unwrap();
        assert_eq!(journal.action(&reset), None);
    }
}
