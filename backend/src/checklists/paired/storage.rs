use super::*;
use sqlx::{
    Row,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::io::{Read, Write};
const MAX_SELECTOR: usize = 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Selector {
    pub version: u8,
    pub installation: String,
    pub generation: String,
    pub digest: String,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Overlay {
    pub progress: model::Progress,
    pub issue: Option<model::ProgressIssue>,
}
#[derive(Clone)]
pub(super) struct Storage {
    pub pool: sqlx::SqlitePool,
    pub selector: PathBuf,
    pub installation: String,
    #[cfg(test)]
    pub fail_directory_sync: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
pub(super) struct Prepared {
    pub selector: Selector,
    pub temporary: PathBuf,
    pub document: Document,
    pub generations: BTreeMap<String, String>,
}
pub(super) fn definition_digest(
    bytes: &[u8],
    generations: &BTreeMap<String, String>,
) -> Result<String, String> {
    use ring::digest::{Context, SHA256};
    let generation_bytes = serde_json::to_vec(generations).map_err(|e| e.to_string())?;
    let mut digest = Context::new(&SHA256);
    digest.update(b"gs-checklist-active-v1\0");
    digest.update(&(bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
    digest.update(&generation_bytes);
    Ok(digest
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
impl Storage {
    pub async fn open(
        path: &std::path::Path,
        selector: PathBuf,
        installation: String,
    ) -> anyhow::Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Full),
            )
            .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS checklist_generations(id TEXT PRIMARY KEY,digest TEXT NOT NULL,document BLOB NOT NULL,generations BLOB NOT NULL)").execute(&pool).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS checklist_progress(generation TEXT NOT NULL,key TEXT NOT NULL,value BLOB NOT NULL,PRIMARY KEY(generation,key))").execute(&pool).await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS checklist_events(id TEXT PRIMARY KEY,event BLOB NOT NULL)",
        )
        .execute(&pool)
        .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS checklist_view_revision(singleton INTEGER PRIMARY KEY CHECK(singleton=1),revision INTEGER NOT NULL)").execute(&pool).await?;
        sqlx::query("INSERT OR IGNORE INTO checklist_view_revision VALUES(1,0)")
            .execute(&pool)
            .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS checklist_migration(singleton INTEGER PRIMARY KEY CHECK(singleton=1),seed_pending INTEGER NOT NULL CHECK(seed_pending IN (0,1)))").execute(&pool).await?;
        Ok(Self {
            pool,
            selector,
            installation,
            #[cfg(test)]
            fail_directory_sync: Default::default(),
        })
    }
    pub async fn seed_pending(&self) -> anyhow::Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT seed_pending FROM checklist_migration WHERE singleton=1",
        )
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(0)
            != 0)
    }
    pub async fn set_seed_pending(&self, pending: bool) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO checklist_migration VALUES(1,?) ON CONFLICT(singleton) DO UPDATE SET seed_pending=excluded.seed_pending")
            .bind(i64::from(pending)).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn next_revision(&self, floor: u64) -> Result<u64, String> {
        let floor = i64::try_from(floor).map_err(|_| "Checklist revision exhausted".to_string())?;
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        let saved: i64 =
            sqlx::query_scalar("SELECT revision FROM checklist_view_revision WHERE singleton=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        let next = saved
            .max(floor)
            .checked_add(1)
            .ok_or("Checklist revision exhausted")?;
        sqlx::query("UPDATE checklist_view_revision SET revision=? WHERE singleton=1")
            .bind(next)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(next as u64)
    }
    pub fn selected(&self) -> Result<Option<Selector>, String> {
        let file = match std::fs::File::open(&self.selector) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_SELECTOR as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > MAX_SELECTOR {
            return Err("Checklist selector exceeds quota".into());
        }
        let selector: Selector = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if selector.version != 1
            || selector.installation != self.installation
            || selector.generation.len() != 32
            || selector.digest.len() != 64
        {
            return Err("Checklist selector identity/version mismatch".into());
        }
        Ok(Some(selector))
    }
    pub async fn load(
        &self,
        selector: &Selector,
    ) -> anyhow::Result<(
        Document,
        BTreeMap<String, String>,
        BTreeMap<String, Overlay>,
    )> {
        let row=sqlx::query("SELECT CASE WHEN length(document)<=1048576 THEN document ELSE NULL END AS document,CASE WHEN length(generations)<=16384 THEN generations ELSE NULL END AS generations,digest FROM checklist_generations WHERE id=?").bind(&selector.generation).fetch_one(&self.pool).await?;
        let bytes: Option<Vec<u8>> = row.try_get("document")?;
        let bytes = bytes.ok_or_else(|| anyhow::anyhow!("Checklist generation exceeds1MiB"))?;
        let generations: Option<Vec<u8>> = row.try_get("generations")?;
        let generations: BTreeMap<String, String> = serde_json::from_slice(
            &generations.ok_or_else(|| anyhow::anyhow!("Checklist generations exceed quota"))?,
        )?;
        anyhow::ensure!(generations.len() <= 32, "Too many checklist generations");
        let digest = definition_digest(&bytes, &generations).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            digest == selector.digest && digest == row.try_get::<String, _>("digest")?,
            "Checklist generation digest mismatch"
        );
        let document: Document = serde_json::from_slice(&bytes)?;
        document.validate().map_err(anyhow::Error::msg)?;
        let rows=sqlx::query("SELECT key,CASE WHEN length(value)<=2048 THEN value ELSE NULL END AS value FROM checklist_progress WHERE generation=? LIMIT 1025").bind(&selector.generation).fetch_all(&self.pool).await?;
        anyhow::ensure!(rows.len() <= 1024, "Too many checklist progress rows");
        let mut progress = BTreeMap::new();
        for row in rows {
            let key: String = row.try_get("key")?;
            let value: Option<Vec<u8>> = row.try_get("value")?;
            let overlay: Overlay = serde_json::from_slice(
                &value.ok_or_else(|| anyhow::anyhow!("Checklist progress exceeds quota"))?,
            )?;
            overlay.progress.validate()?;
            progress.insert(key, overlay);
        }
        Ok((document, generations, progress))
    }
    pub async fn prepare(
        &self,
        mut document: Document,
        generations: BTreeMap<String, String>,
        progress: &BTreeMap<String, Overlay>,
        revision: u64,
    ) -> anyhow::Result<Prepared> {
        for item in document
            .checklists
            .iter_mut()
            .flat_map(|list| &mut list.items)
        {
            item.checked = false;
            item.completion_source = None;
            item.completed_by = None;
            item.completed_at_ms = None;
        }
        document.completion_events.clear();
        document.revision = 0;
        document.validate().map_err(anyhow::Error::msg)?;
        let bytes = serialize_document(&document).map_err(anyhow::Error::msg)?;
        let digest = definition_digest(&bytes, &generations).map_err(anyhow::Error::msg)?;
        let id = format!("{:032x}", rand::random::<u128>());
        let selector = Selector {
            version: 1,
            installation: self.installation.clone(),
            generation: id.clone(),
            digest,
            revision,
        };
        let mut tx = self.pool.begin().await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM checklist_generations")
            .fetch_one(&mut *tx)
            .await?;
        anyhow::ensure!(
            count < 8,
            "Uncollected prepared checklist generations require recovery"
        );
        sqlx::query("INSERT INTO checklist_generations VALUES(?,?,?,?)")
            .bind(&id)
            .bind(&selector.digest)
            .bind(bytes)
            .bind(serde_json::to_vec(&generations)?)
            .execute(&mut *tx)
            .await?;
        for (key, overlay) in progress {
            sqlx::query("INSERT INTO checklist_progress VALUES(?,?,?)")
                .bind(&id)
                .bind(key)
                .bind(serde_json::to_vec(overlay)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        let parent = self
            .selector
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".checklist-selector-{}.tmp", rand::random::<u64>()));
        let encoded = serde_json::to_vec(&selector)?;
        anyhow::ensure!(
            encoded.len() <= MAX_SELECTOR,
            "Checklist selector exceeds quota"
        );
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        Ok(Prepared {
            selector,
            temporary,
            document,
            generations,
        })
    }
    /// Call only after atomically selecting this fully prepared generation.
    pub fn select(&self, prepared: &Prepared) -> Result<(), String> {
        std::fs::rename(&prepared.temporary, &self.selector).map_err(|error| error.to_string())
    }
    pub fn sync_selector(&self) -> Result<(), String> {
        #[cfg(test)]
        if self.fail_directory_sync.load(Ordering::Relaxed) {
            return Err("Injected directory sync failure".into());
        }
        let parent = self
            .selector
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        std::fs::File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())
    }
    pub async fn progress(
        &self,
        generation: &str,
        values: &BTreeMap<String, Overlay>,
        event: Option<&CompletionEvent>,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for (key, value) in values {
            value.progress.validate()?;
            anyhow::ensure!(
                serde_json::to_vec(value)?.len() <= 2048,
                "Checklist progress exceeds quota"
            );
            sqlx::query("INSERT INTO checklist_progress VALUES(?,?,?) ON CONFLICT(generation,key) DO UPDATE SET value=excluded.value").bind(generation).bind(key).bind(serde_json::to_vec(value)?).execute(&mut *tx).await?;
        }
        if let Some(event) = event {
            let bytes = serde_json::to_vec(event)?;
            anyhow::ensure!(bytes.len() <= 4096, "Checklist event exceeds quota");
            if let Some(old) =
                sqlx::query_scalar::<_, Option<Vec<u8>>>("SELECT CASE WHEN length(event)<=4096 THEN event ELSE NULL END FROM checklist_events WHERE id=?")
                    .bind(&event.id)
                    .fetch_optional(&mut *tx)
                    .await?
            {
                anyhow::ensure!(old.as_deref() == Some(bytes.as_slice()), "Checklist event ID equivocation or oversized receipt");
            } else {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM checklist_events")
                    .fetch_one(&mut *tx)
                    .await?;
                anyhow::ensure!(count < 8192, "Checklist local receipt quota exhausted");
                sqlx::query("INSERT INTO checklist_events VALUES(?,?)")
                    .bind(&event.id)
                    .bind(bytes)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn event_saved(&self, event: &CompletionEvent) -> anyhow::Result<bool> {
        if let Some(bytes) =
            sqlx::query_scalar::<_, Option<Vec<u8>>>("SELECT CASE WHEN length(event)<=4096 THEN event ELSE NULL END FROM checklist_events WHERE id=?")
                .bind(&event.id)
                .fetch_optional(&self.pool)
                .await?
        {
            anyhow::ensure!(
                bytes.as_deref() == Some(serde_json::to_vec(event)?.as_slice()),
                "Checklist event ID equivocation"
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }
    pub async fn collect(&self, active: &str, previous: Option<&str>) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM checklist_progress WHERE generation<>? AND generation<>?")
            .bind(active)
            .bind(previous.unwrap_or(active))
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM checklist_generations WHERE id<>? AND id<>?")
            .bind(active)
            .bind(previous.unwrap_or(active))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temporary);
    }
}
