//! Called only by the existing telemetry DB writer, outside live registry locks.
//! Unlike a recording, this file is always durable while the station runs.
use crate::alert_conditions::{
    ConditionState, HISTORY_PER_SEVERITY, JournalEntry, Key, MAX_KEYS, Record, Snapshot,
};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::path::Path;
const TRANSITION_RETENTION: i64 = 4096;
pub struct Store {
    pool: SqlitePool,
    authority: String,
}
impl Store {
    pub async fn open(path: &Path, authority: String) -> Result<Self, String> {
        if authority.is_empty() || authority.len() > 128 {
            return Err("Invalid condition journal authority".into());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| e.to_string())?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(std::time::Duration::from_secs(1));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|e| e.to_string())?;
        for sql in [
            "PRAGMA wal_autocheckpoint=64",
            "CREATE TABLE IF NOT EXISTS condition_integrity (id INTEGER PRIMARY KEY CHECK(id=1), marker_json TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS condition_writer (id INTEGER PRIMARY KEY CHECK(id=1), authority TEXT NOT NULL, checkpoint_revision INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE IF NOT EXISTS condition_journal (id INTEGER PRIMARY KEY AUTOINCREMENT, authority TEXT NOT NULL, revision INTEGER NOT NULL, occurrence_id TEXT NOT NULL, record_json TEXT NOT NULL, UNIQUE(authority,revision))",
            "CREATE TABLE IF NOT EXISTS condition_latest (occurrence_id TEXT PRIMARY KEY, authority TEXT NOT NULL, revision INTEGER NOT NULL, state TEXT NOT NULL, severity TEXT NOT NULL, updated_at_ms INTEGER NOT NULL, last_order INTEGER NOT NULL, record_json TEXT NOT NULL)",
        ] {
            sqlx::query(sql)
                .execute(&pool)
                .await
                .map_err(|e| e.to_string())?;
        }
        sqlx::query("INSERT INTO condition_writer(id,authority) VALUES(1,?) ON CONFLICT(id) DO UPDATE SET authority=excluded.authority, checkpoint_revision=0").bind(&authority).execute(&pool).await.map_err(|e|e.to_string())?;
        Ok(Self { pool, authority })
    }
    pub async fn restore(&self) -> Result<Vec<Record>, String> {
        let rows = sqlx::query(
            "SELECT record_json FROM condition_latest ORDER BY last_order,occurrence_id LIMIT ?",
        )
        .bind((HISTORY_PER_SEVERITY * 2 + MAX_KEYS + 1) as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        if rows.len() > HISTORY_PER_SEVERITY * 2 + MAX_KEYS {
            return Err("Condition journal exceeds bounded restore history".into());
        }
        rows.into_iter()
            .map(|row| {
                serde_json::from_str::<Record>(row.get::<&str, _>("record_json"))
                    .map_err(|e| format!("Invalid condition record: {e}"))
            })
            .collect()
    }
    pub async fn restore_integrity(
        &self,
    ) -> Result<Option<crate::alert_conditions::IntegrityMarker>, String> {
        let value: Option<String> =
            sqlx::query_scalar("SELECT marker_json FROM condition_integrity WHERE id=1")
                .fetch_optional(&self.pool)
                .await
                .map_err(|error| error.to_string())?;
        value
            .map(|json| {
                let marker: crate::alert_conditions::IntegrityMarker =
                    serde_json::from_str(&json).map_err(|error| error.to_string())?;
                marker.validate()?;
                Ok(marker)
            })
            .transpose()
    }
    pub async fn persist_integrity(
        &self,
        marker: &crate::alert_conditions::IntegrityMarker,
    ) -> Result<(), String> {
        marker.validate()?;
        let json = serde_json::to_string(marker).map_err(|error| error.to_string())?;
        let mut tx = self.pool.begin().await.map_err(|error| error.to_string())?;
        let authority: String =
            sqlx::query_scalar("SELECT authority FROM condition_writer WHERE id=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| error.to_string())?;
        if authority != self.authority {
            return Err("Condition writer authority was superseded".into());
        }
        // The marker is deliberately sticky. A later empty RAM snapshot or
        // checkpoint never deletes or replaces the first persisted gap.
        sqlx::query("INSERT INTO condition_integrity(id,marker_json) VALUES(1,?) ON CONFLICT(id) DO NOTHING").bind(json).execute(&mut *tx).await.map_err(|error|error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())
    }
    /// Writer may batch up to the registry's bounded pending capacity. Reusing a
    /// token with different contents fails, rather than silently acknowledging it.
    pub async fn append(&self, entries: &[JournalEntry]) -> Result<(), String> {
        if entries.len() > 64 {
            return Err("Condition journal batch exceeds64 entries".into());
        }
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        let current: String =
            sqlx::query_scalar("SELECT authority FROM condition_writer WHERE id=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        if current != self.authority {
            return Err("Condition writer authority was superseded".into());
        }
        let barrier: i64 =
            sqlx::query_scalar("SELECT checkpoint_revision FROM condition_writer WHERE id=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        for entry in entries {
            if entry.authority != self.authority
                || Key::from_wire(&entry.record.key).is_none()
                || entry.record.message.len() > 4096
                || entry.record.occurrence_id.is_empty()
                || entry.record.occurrence_id.len() > 160
                || (entry.record.state == ConditionState::Resolved)
                    != entry.record.resolved_at_ms.is_some()
            {
                return Err("Invalid condition journal entry".into());
            }
            let revision = i64::try_from(entry.revision)
                .map_err(|_| "Condition revision exceeds SQLite range")?;
            if revision <= barrier {
                return Err("Condition entry superseded by durable checkpoint".into());
            }
            let order: i64 =
                sqlx::query_scalar("SELECT COALESCE(MAX(last_order),0)+1 FROM condition_latest")
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|e| e.to_string())?;
            let json = serde_json::to_string(&entry.record).map_err(|e| e.to_string())?;
            let prior: Option<String> = sqlx::query_scalar(
                "SELECT record_json FROM condition_journal WHERE authority=? AND revision=?",
            )
            .bind(&entry.authority)
            .bind(revision)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
            if prior.as_ref().is_some_and(|prior| prior != &json) {
                return Err("Condition journal token has conflicting contents".into());
            }
            sqlx::query("INSERT INTO condition_journal(authority,revision,occurrence_id,record_json) VALUES(?,?,?,?) ON CONFLICT(authority,revision) DO NOTHING")
                .bind(&entry.authority).bind(revision).bind(&entry.record.occurrence_id).bind(&json).execute(&mut *tx).await.map_err(|e|e.to_string())?;
            let state = match entry.record.state {
                ConditionState::Active => "active",
                ConditionState::Unknown => "unknown",
                ConditionState::Resolved => "resolved",
            };
            let severity = match entry.record.severity {
                crate::alert_conditions::Severity::Warning => "warning",
                crate::alert_conditions::Severity::Error => "error",
            };
            sqlx::query("INSERT INTO condition_latest(occurrence_id,authority,revision,state,severity,updated_at_ms,last_order,record_json) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(occurrence_id) DO UPDATE SET authority=excluded.authority,revision=excluded.revision,state=excluded.state,severity=excluded.severity,updated_at_ms=excluded.updated_at_ms,last_order=excluded.last_order,record_json=excluded.record_json WHERE condition_latest.authority!=excluded.authority OR condition_latest.revision<excluded.revision")
                .bind(&entry.record.occurrence_id).bind(&entry.authority).bind(revision).bind(state).bind(severity).bind(entry.record.updated_at_ms).bind(order).bind(&json).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        }
        // Current unresolved occurrences are never evicted by history retention.
        for severity in ["warning", "error"] {
            sqlx::query("DELETE FROM condition_latest WHERE state='resolved' AND severity=? AND occurrence_id NOT IN(SELECT occurrence_id FROM condition_latest WHERE state='resolved' AND severity=? ORDER BY last_order DESC,occurrence_id DESC LIMIT ?)")
                .bind(severity).bind(severity).bind(HISTORY_PER_SEVERITY as i64).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        }
        sqlx::query("DELETE FROM condition_journal WHERE id NOT IN(SELECT id FROM condition_journal ORDER BY id DESC LIMIT ?)").bind(TRANSITION_RETENTION).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())
    }
    /// Explicit repair after queue/write loss. A full current registry snapshot
    /// replaces retained latest rows atomically; stale queued tokens cannot
    /// overwrite it. Caller clears incomplete only if its revision is still current.
    pub async fn checkpoint(&self, snapshot: &Snapshot) -> Result<(), String> {
        if snapshot.authority != self.authority
            || snapshot.records.len() > HISTORY_PER_SEVERITY * 2 + MAX_KEYS
        {
            return Err("Invalid condition checkpoint".into());
        }
        let revision = i64::try_from(snapshot.revision)
            .map_err(|_| "Condition checkpoint revision exceeds SQLite range")?;
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        let current: String =
            sqlx::query_scalar("SELECT authority FROM condition_writer WHERE id=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        if current != self.authority {
            return Err("Condition writer authority was superseded".into());
        }
        if let Some(marker) = &snapshot.history_integrity {
            marker.validate()?;
            let json = serde_json::to_string(marker).map_err(|error| error.to_string())?;
            sqlx::query("INSERT INTO condition_integrity(id,marker_json) VALUES(1,?) ON CONFLICT(id) DO NOTHING")
                .bind(json).execute(&mut *tx).await.map_err(|error|error.to_string())?;
        }
        // Validate every row before deleting anything. Registry validates IDs,
        // keys/source bindings and history bounds on this exact snapshot data.
        let mut validation =
            crate::alert_conditions::Registry::new("checkpoint-validator".into(), []);
        validation.restore(snapshot.records.clone())?;
        sqlx::query("DELETE FROM condition_latest")
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        for (order, record) in snapshot.records.iter().enumerate() {
            let state = match record.state {
                ConditionState::Active => "active",
                ConditionState::Unknown => "unknown",
                ConditionState::Resolved => "resolved",
            };
            let severity = match record.severity {
                crate::alert_conditions::Severity::Warning => "warning",
                crate::alert_conditions::Severity::Error => "error",
            };
            sqlx::query("INSERT INTO condition_latest(occurrence_id,authority,revision,state,severity,updated_at_ms,last_order,record_json) VALUES(?,?,?,?,?,?,?,?)")
                .bind(&record.occurrence_id).bind(&self.authority).bind(revision).bind(state).bind(severity).bind(record.updated_at_ms).bind(order as i64+1).bind(serde_json::to_string(record).map_err(|e|e.to_string())?).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        }
        sqlx::query("UPDATE condition_writer SET checkpoint_revision=? WHERE id=1")
            .bind(revision)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())
    }
    pub async fn close(self) {
        self.pool.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert_conditions::{BoardKey, ConditionState, Registry};
    fn path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "seds-condition-journal-{label}-{}-{}.sqlite3",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    async fn cleanup(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = tokio::fs::remove_file(format!("{}{suffix}", path.display())).await;
        }
    }
    #[tokio::test]
    async fn durable_restart_is_recording_independent_and_restored_active_requires_fresh_evidence()
    {
        let path = path("restart");
        let store = Store::open(&path, "first".into()).await.unwrap();
        let mut r = Registry::new("first".into(), []);
        r.restore(store.restore().await.unwrap()).unwrap();
        let e = r
            .observe(
                Key::BoardOffline(BoardKey::Daq),
                ConditionState::Active,
                "DAQ offline".into(),
                100,
            )
            .journal
            .unwrap();
        store.append(std::slice::from_ref(&e)).await.unwrap();
        r.journal_succeeded(&e.authority, e.revision);
        store.close().await;
        let reopened = Store::open(&path, "second".into()).await.unwrap();
        let mut next = Registry::new("second".into(), []);
        next.restore(reopened.restore().await.unwrap()).unwrap();
        assert_eq!(next.snapshot().records[0].state, ConditionState::Unknown);
        assert!(!next.snapshot().complete);
        let clear = next
            .observe(
                Key::BoardOffline(BoardKey::Daq),
                ConditionState::Resolved,
                "Fresh DAQ response".into(),
                200,
            )
            .journal
            .unwrap();
        reopened.append(std::slice::from_ref(&clear)).await.unwrap();
        next.journal_succeeded(&clear.authority, clear.revision);
        assert!(next.snapshot().complete);
        reopened.close().await;
        cleanup(&path).await;
    }
    #[tokio::test]
    async fn reused_token_conflict_and_superseded_writer_fail_without_false_success() {
        let path = path("authority");
        let old = Store::open(&path, "old".into()).await.unwrap();
        let mut r = Registry::new("old".into(), []);
        r.restore(Vec::new()).unwrap();
        let e = r
            .observe(
                Key::GatewayLowVoltage,
                ConditionState::Active,
                "Low".into(),
                1,
            )
            .journal
            .unwrap();
        old.append(std::slice::from_ref(&e)).await.unwrap();
        old.append(std::slice::from_ref(&e)).await.unwrap();
        let mut conflict = e.clone();
        conflict.record.message = "Conflicting content".into();
        assert!(old.append(&[conflict]).await.is_err());
        let next = Store::open(&path, "next".into()).await.unwrap();
        assert!(old.append(&[e]).await.is_err());
        assert_eq!(next.restore().await.unwrap().len(), 1);
        old.close().await;
        next.close().await;
        cleanup(&path).await;
    }
    #[tokio::test]
    async fn checkpoint_fences_late_work_and_retention_preserves_current_faults_across_clock_backsteps()
     {
        let path = path("checkpoint");
        let store = Store::open(&path, "epoch".into()).await.unwrap();
        let mut r = Registry::new("epoch".into(), []);
        r.restore(Vec::new()).unwrap();
        let old = r
            .observe(
                Key::GatewayLowVoltage,
                ConditionState::Active,
                "Low".into(),
                10000,
            )
            .journal
            .unwrap();
        store.append(std::slice::from_ref(&old)).await.unwrap();
        r.journal_succeeded(&old.authority, old.revision);
        let mut writes = Vec::new();
        for index in 0..550 {
            for state in [ConditionState::Active, ConditionState::Resolved] {
                let e = r
                    .observe(
                        Key::StationHeartbeatEnqueue,
                        state,
                        "Enqueue state".into(),
                        10000 - index,
                    )
                    .journal
                    .unwrap();
                r.journal_succeeded(&e.authority, e.revision);
                writes.push(e);
                if writes.len() == 64 {
                    store.append(&writes).await.unwrap();
                    writes.clear();
                }
            }
        }
        store.append(&writes).await.unwrap();
        let retained = store.restore().await.unwrap();
        assert_eq!(retained.len(), HISTORY_PER_SEVERITY + 1);
        assert!(
            retained
                .iter()
                .any(|record| record.occurrence_id == old.record.occurrence_id
                    && record.state == ConditionState::Active)
        );
        assert!(
            retained
                .iter()
                .filter(|record| record.state == ConditionState::Resolved)
                .all(|record| record.updated_at_ms <= 9950),
            "retention follows durable write order, not wall time"
        );
        let snapshot = r.snapshot();
        store.checkpoint(&snapshot).await.unwrap();
        assert!(r.checkpoint_succeeded("epoch", snapshot.revision));
        assert!(store.append(&[old]).await.is_err());
        assert_eq!(
            store.restore().await.unwrap().len(),
            HISTORY_PER_SEVERITY + 1
        );
        store.close().await;
        cleanup(&path).await;
    }

    #[tokio::test]
    async fn late_old_revision_does_not_resurrect_resolved_latest_record() {
        let path = path("ordering");
        let store = Store::open(&path, "epoch".into()).await.unwrap();
        let mut r = Registry::new("epoch".into(), []);
        r.restore(Vec::new()).unwrap();
        let active = r
            .observe(
                Key::GatewayLowVoltage,
                ConditionState::Active,
                "Low".into(),
                1,
            )
            .journal
            .unwrap();
        let clear = r
            .observe(
                Key::GatewayLowVoltage,
                ConditionState::Resolved,
                "Recovered".into(),
                2,
            )
            .journal
            .unwrap();
        store.append(&[clear]).await.unwrap();
        store.append(&[active]).await.unwrap();
        assert_eq!(
            store.restore().await.unwrap()[0].state,
            ConditionState::Resolved
        );
        store.close().await;
        cleanup(&path).await;
    }
    #[tokio::test]
    async fn bootstrap_history_integrity_survives_checkpoint_and_actual_sqlite_restart() {
        use crate::alert_conditions::{IntegrityKind, IntegrityMarker};
        let path = path("bootstrap-integrity");
        let store = Store::open(&path, "first".into()).await.unwrap();
        let marker = IntegrityMarker {
            kind: IntegrityKind::BootstrapHistoryIncomplete,
            reason: "Condition history incomplete: startup receipt budget exceeded".into(),
        };
        store.persist_integrity(&marker).await.unwrap();
        let mut empty = Registry::new("first".into(), []);
        empty.restore(Vec::new()).unwrap();
        store.checkpoint(&empty.snapshot()).await.unwrap();
        assert_eq!(
            store.restore_integrity().await.unwrap(),
            Some(marker.clone()),
            "empty RAM checkpoint cannot clear durable gap"
        );
        store.close().await;
        let reopened = Store::open(&path, "second".into()).await.unwrap();
        let mut next = Registry::new("second".into(), []);
        next.restore_integrity(reopened.restore_integrity().await.unwrap())
            .unwrap();
        next.restore(reopened.restore().await.unwrap()).unwrap();
        assert!(!next.snapshot().complete);
        assert_eq!(next.snapshot().history_integrity, Some(marker.clone()));
        reopened.checkpoint(&next.snapshot()).await.unwrap();
        assert_eq!(reopened.restore_integrity().await.unwrap(), Some(marker));
        reopened.close().await;
        cleanup(&path).await;
    }
}
