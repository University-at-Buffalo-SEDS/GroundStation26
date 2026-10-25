//! Host-only per-document durable staging for large paired catalogs.
//!
//! This is deliberately separate from runtime activation. It never acquires a
//! sequence lock, writes the active checklist file or dispatches outputs. Each
//! mutation validates and hashes one document, then changes one row and small
//! quota metadata in a FULL-synchronous SQLite transaction. Tombstones count
//! against quotas and are never silently collected.
use crate::{Document, Store, transport};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::{collections::BTreeMap, path::Path, time::Duration};

const FORMAT_VERSION: i64 = 2;
const DOCUMENT_LIMIT: usize = 65_536;
const PAGE_LIMIT: usize = 64;
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub documents: usize,
    pub encoded_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            documents: crate::checklists::MAX_HISTORY_DOCUMENTS,
            encoded_bytes: crate::checklists::MAX_HISTORY_BYTES,
        }
    }
}
impl Limits {
    fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.documents > 0
                && self.documents <= crate::checklists::MAX_HISTORY_DOCUMENTS
                && self.encoded_bytes > 0
                && self.encoded_bytes <= crate::checklists::MAX_HISTORY_BYTES,
            "invalid paired journal limits"
        );
        Ok(())
    }
}
#[derive(Clone)]
pub struct Journal {
    pool: SqlitePool,
    installation: String,
    peers: Vec<String>,
    limits: Limits,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryEntry {
    pub key: String,
    pub hash: String,
    pub revision: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub incarnation: String,
    pub revision: i64,
    pub documents: usize,
    pub encoded_bytes: usize,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScanCursor {
    Full { after: String },
    Changes { after: i64 },
}
#[derive(Debug)]
pub struct ScanPage {
    pub metadata: Metadata,
    pub entry: Option<InventoryEntry>,
}
#[derive(Debug)]
pub struct Snapshot {
    pub metadata: Metadata,
    pub documents: BTreeMap<String, Document>,
}
impl Journal {
    /// `path` belongs to this installation, not the active operational snapshot.
    /// A separate pool keeps slow storage independent of sequence/abort locks.
    pub async fn open(
        path: &Path,
        installation: String,
        peers: Vec<String>,
        limits: Limits,
    ) -> anyhow::Result<Self> {
        limits.validate()?;
        anyhow::ensure!(
            crate::identity(&installation)
                && !peers.is_empty()
                && peers.len() <= 9
                && peers.iter().all(|peer| crate::identity(peer))
                && peers
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == peers.len(),
            "invalid paired journal identity"
        );
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(3));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS paired_metadata (singleton INTEGER PRIMARY KEY CHECK(singleton=1), format INTEGER NOT NULL, installation TEXT NOT NULL, incarnation TEXT NOT NULL, revision INTEGER NOT NULL, documents INTEGER NOT NULL, encoded_bytes INTEGER NOT NULL)").execute(&pool).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS paired_documents (key TEXT PRIMARY KEY, document BLOB NOT NULL, hash TEXT NOT NULL, revision INTEGER NOT NULL, encoded_bytes INTEGER NOT NULL)").execute(&pool).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS paired_changes ON paired_documents(revision)")
            .execute(&pool)
            .await?;
        use base64::Engine as _;
        use ring::rand::SecureRandom;
        let mut entropy = [0u8; 32];
        ring::rand::SystemRandom::new()
            .fill(&mut entropy)
            .map_err(|_| anyhow::anyhow!("paired journal entropy unavailable"))?;
        let incarnation = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(entropy);
        sqlx::query("INSERT OR IGNORE INTO paired_metadata VALUES(1,?,?,?,0,0,0)")
            .bind(FORMAT_VERSION)
            .bind(&installation)
            .bind(incarnation)
            .execute(&pool)
            .await?;
        let this = Self {
            pool,
            installation,
            peers,
            limits,
        };
        let meta = sqlx::query("SELECT format,installation FROM paired_metadata WHERE singleton=1")
            .fetch_one(&this.pool)
            .await?;
        anyhow::ensure!(
            meta.try_get::<i64, _>("format")? == FORMAT_VERSION
                && meta.try_get::<String, _>("installation")? == this.installation,
            "paired journal installation/version mismatch"
        );
        // Validate totals once on open, before any potentially large body fetch.
        let actual=sqlx::query("SELECT COUNT(*) AS documents,COALESCE(SUM(length(document)+length(CAST(key AS BLOB))),0) AS encoded_bytes,COALESCE(MAX(length(document)),0) AS largest FROM paired_documents").fetch_one(&this.pool).await?;
        let metadata = this.metadata().await?;
        anyhow::ensure!(
            actual.try_get::<i64, _>("documents")? == metadata.documents as i64
                && actual.try_get::<i64, _>("encoded_bytes")? == metadata.encoded_bytes as i64
                && actual.try_get::<i64, _>("largest")? <= DOCUMENT_LIMIT as i64,
            "paired journal quota metadata mismatch"
        );
        this.local_tables().await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS paired_document_heads(key TEXT PRIMARY KEY,heads INTEGER NOT NULL)").execute(&this.pool).await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS paired_conflicts ON paired_document_heads(heads,key)",
        )
        .execute(&this.pool)
        .await?;
        // Derived cache, reconstructed only on upgrade/recovery, never per poll.
        let indexed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paired_document_heads")
            .fetch_one(&this.pool)
            .await?;
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paired_documents")
            .fetch_one(&this.pool)
            .await?;
        if indexed != total {
            let mut transaction = this.pool.begin().await?;
            sqlx::query("DELETE FROM paired_document_heads")
                .execute(&mut *transaction)
                .await?;
            let mut cursor = String::new();
            loop {
                let rows=sqlx::query("SELECT key,CASE WHEN length(document)<=65536 THEN document ELSE NULL END AS document,hash FROM paired_documents WHERE key>? ORDER BY key LIMIT 64").bind(&cursor).fetch_all(&mut *transaction).await?;
                if rows.is_empty() {
                    break;
                }
                for row in rows {
                    let key: String = row.try_get("key")?;
                    let body: Option<Vec<u8>> = row.try_get("document")?;
                    let document = this.decode(
                        &key,
                        &body.ok_or_else(|| anyhow::anyhow!("paired document exceeds quota"))?,
                        &row.try_get::<String, _>("hash")?,
                    )?;
                    sqlx::query("INSERT INTO paired_document_heads VALUES(?,?) ON CONFLICT(key) DO UPDATE SET heads=excluded.heads").bind(&key).bind(document.heads.len() as i64).execute(&mut *transaction).await?;
                    cursor = key;
                }
            }
            transaction.commit().await?;
        }

        Ok(this)
    }
    fn metadata_row(&self, row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Metadata> {
        let metadata = Metadata {
            incarnation: row.try_get("incarnation")?,
            revision: row.try_get("revision")?,
            documents: usize::try_from(row.try_get::<i64, _>("documents")?)?,
            encoded_bytes: usize::try_from(row.try_get::<i64, _>("encoded_bytes")?)?,
        };
        anyhow::ensure!(
            metadata.incarnation.len() == 43
                && metadata.revision >= 0
                && metadata.documents <= self.limits.documents
                && metadata.encoded_bytes <= self.limits.encoded_bytes,
            "paired journal quota exceeded"
        );
        Ok(metadata)
    }
    pub fn installation(&self) -> &str {
        &self.installation
    }
    pub fn peers(&self) -> &[String] {
        &self.peers
    }
    pub fn limits(&self) -> Limits {
        self.limits
    }
    pub async fn digest(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT hash FROM paired_documents WHERE key=?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }
    pub async fn metadata(&self) -> anyhow::Result<Metadata> {
        self.metadata_row(
            &sqlx::query(
                "SELECT incarnation,revision,documents,encoded_bytes FROM paired_metadata WHERE singleton=1",
            )
            .fetch_one(&self.pool)
            .await?,
        )
    }
    fn decode(&self, key: &str, bytes: &[u8], expected_hash: &str) -> anyhow::Result<Document> {
        anyhow::ensure!(
            bytes.len() <= DOCUMENT_LIMIT,
            "paired journal document exceeds quota"
        );
        let document: Document = serde_json::from_slice(bytes)?;
        crate::validate_document(key, &document, &self.peers)?;
        anyhow::ensure!(
            transport::document_hash(&document) == expected_hash,
            "paired journal document digest mismatch"
        );
        Ok(document)
    }
    /// The caller supplies the typed boundary validator used for both local and
    /// remote values. Authentication must precede this method. No-op replay does
    /// not increment metadata or rewrite a row.
    pub async fn merge(
        &self,
        key: &str,
        incoming: &Document,
        validate: impl Fn(&str, &Document) -> anyhow::Result<()>,
    ) -> anyhow::Result<Option<InventoryEntry>> {
        crate::validate_document(key, incoming, &self.peers)?;
        validate(key, incoming)?;
        let mut transaction = self.pool.begin().await?;
        let metadata = self.metadata_row(
            &sqlx::query(
                "SELECT incarnation,revision,documents,encoded_bytes FROM paired_metadata WHERE singleton=1",
            )
            .fetch_one(&mut *transaction)
            .await?,
        )?;
        let old=sqlx::query("SELECT CASE WHEN length(document)<=65536 THEN document ELSE NULL END AS document,hash,encoded_bytes FROM paired_documents WHERE key=?").bind(key).fetch_optional(&mut *transaction).await?;
        let mut temporary = Store::new(self.installation.clone())?;
        let old_size = if let Some(row) = &old {
            let bytes: Option<Vec<u8>> = row.try_get("document")?;
            let bytes =
                bytes.ok_or_else(|| anyhow::anyhow!("paired journal document exceeds quota"))?;
            let old = self.decode(key, &bytes, &row.try_get::<String, _>("hash")?)?;
            temporary.merge(&self.installation, key, &old, &self.peers)?;
            let count: usize = usize::try_from(row.try_get::<i64, _>("encoded_bytes")?)?;
            anyhow::ensure!(
                count == key.len() + bytes.len(),
                "paired journal row size mismatch"
            );
            count
        } else {
            0
        };
        if !temporary.merge(&self.installation, key, incoming, &self.peers)? {
            return Ok(None);
        }
        let document = temporary.document(key).expect("merged document exists");
        validate(key, document)?;
        let bytes = transport::document_bytes(document);
        let hash = transport::document_hash(document);
        let size = key.len() + bytes.len();
        let documents = metadata.documents + usize::from(old.is_none());
        let encoded_bytes = metadata
            .encoded_bytes
            .checked_sub(old_size)
            .and_then(|n| n.checked_add(size))
            .ok_or_else(|| anyhow::anyhow!("paired journal quota overflow"))?;
        anyhow::ensure!(
            documents <= self.limits.documents && encoded_bytes <= self.limits.encoded_bytes,
            "paired journal quota exceeded"
        );
        let revision = metadata
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("paired journal revision exhausted"))?;
        sqlx::query("INSERT INTO paired_documents VALUES(?,?,?,?,?) ON CONFLICT(key) DO UPDATE SET document=excluded.document,hash=excluded.hash,revision=excluded.revision,encoded_bytes=excluded.encoded_bytes")
            .bind(key).bind(&bytes).bind(&hash).bind(revision).bind(i64::try_from(size)?).execute(&mut *transaction).await?;
        sqlx::query("INSERT INTO paired_document_heads VALUES(?,?) ON CONFLICT(key) DO UPDATE SET heads=excluded.heads").bind(key).bind(document.heads.len() as i64).execute(&mut *transaction).await?;
        sqlx::query(
            "UPDATE paired_metadata SET revision=?,documents=?,encoded_bytes=? WHERE singleton=1",
        )
        .bind(revision)
        .bind(i64::try_from(documents)?)
        .bind(i64::try_from(encoded_bytes)?)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(Some(InventoryEntry {
            key: key.into(),
            hash,
            revision,
        }))
    }
    pub async fn document(&self, key: &str) -> anyhow::Result<Option<Document>> {
        let row=sqlx::query("SELECT CASE WHEN length(document)<=65536 THEN document ELSE NULL END AS document,hash FROM paired_documents WHERE key=?").bind(key).fetch_optional(&self.pool).await?;
        row.map(|row| {
            let bytes: Option<Vec<u8>> = row.try_get("document")?;
            self.decode(
                key,
                &bytes.ok_or_else(|| anyhow::anyhow!("paired journal document exceeds quota"))?,
                &row.try_get::<String, _>("hash")?,
            )
        })
        .transpose()
    }
    /// A catalog page contains keys and cached digests, never document text.
    /// `revision` can be used as a bounded change cursor after an initial scan.
    pub async fn catalog_page(
        &self,
        after: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<InventoryEntry>> {
        anyhow::ensure!(
            limit > 0 && limit <= PAGE_LIMIT && after.len() <= 128,
            "invalid paired journal page bounds"
        );
        sqlx::query(
            "SELECT key,hash,revision FROM paired_documents WHERE key>? ORDER BY key LIMIT ?",
        )
        .bind(after)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(InventoryEntry {
                key: row.try_get("key")?,
                hash: row.try_get("hash")?,
                revision: row.try_get("revision")?,
            })
        })
        .collect()
    }
    pub async fn conflict_count(&self) -> anyhow::Result<usize> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM paired_document_heads WHERE heads>1")
                .fetch_one(&self.pool)
                .await?;
        let count = usize::try_from(count)?;
        anyhow::ensure!(
            count <= self.limits.documents,
            "Conflict catalog exceeds quota"
        );
        Ok(count)
    }
    pub async fn conflicts_page(
        &self,
        after: &str,
        limit: usize,
    ) -> anyhow::Result<BTreeMap<String, Document>> {
        anyhow::ensure!(
            after.len() <= 128 && limit > 0 && limit <= 8,
            "invalid conflict page bounds"
        );
        let rows=sqlx::query("SELECT d.key,CASE WHEN length(d.document)<=65536 THEN d.document ELSE NULL END AS document,d.hash FROM paired_document_heads h JOIN paired_documents d ON d.key=h.key WHERE h.heads>1 AND h.key>? ORDER BY h.key LIMIT ?").bind(after).bind(limit as i64).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                let key: String = row.try_get("key")?;
                let bytes: Option<Vec<u8>> = row.try_get("document")?;
                let document = self.decode(
                    &key,
                    &bytes.ok_or_else(|| anyhow::anyhow!("paired document exceeds quota"))?,
                    &row.try_get::<String, _>("hash")?,
                )?;
                Ok((key, document))
            })
            .collect()
    }
    pub async fn changed_page(
        &self,
        after_revision: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<InventoryEntry>> {
        anyhow::ensure!(
            after_revision >= 0 && limit > 0 && limit <= PAGE_LIMIT,
            "invalid paired journal change bounds"
        );
        sqlx::query("SELECT key,hash,revision FROM paired_documents WHERE revision>? ORDER BY revision LIMIT ?")
            .bind(after_revision).bind(limit as i64).fetch_all(&self.pool).await?.into_iter()
            .map(|row|Ok(InventoryEntry {key:row.try_get("key")?,hash:row.try_get("hash")?,revision:row.try_get("revision")?})).collect()
    }
    /// Metadata and one cached entry share a read snapshot. A replication
    /// cursor must not skip writes racing an empty catalog/change response.
    pub async fn scan(&self, cursor: &ScanCursor) -> anyhow::Result<ScanPage> {
        let mut transaction = self.pool.begin().await?;
        let metadata = self.metadata_row(&sqlx::query("SELECT incarnation,revision,documents,encoded_bytes FROM paired_metadata WHERE singleton=1")
            .fetch_one(&mut *transaction).await?)?;
        let row = match cursor {
            ScanCursor::Full { after } => {
                anyhow::ensure!(after.len() <= 128, "invalid journal full cursor");
                sqlx::query("SELECT key,hash,revision FROM paired_documents WHERE key>? ORDER BY key LIMIT 1")
                    .bind(after).fetch_optional(&mut *transaction).await?
            }
            ScanCursor::Changes { after } => {
                anyhow::ensure!(
                    *after >= 0 && *after <= metadata.revision,
                    "invalid journal change cursor"
                );
                sqlx::query("SELECT key,hash,revision FROM paired_documents WHERE revision>? ORDER BY revision LIMIT 1")
                    .bind(after).fetch_optional(&mut *transaction).await?
            }
        };
        let entry = row
            .map(|row| -> anyhow::Result<_> {
                Ok(InventoryEntry {
                    key: row.try_get("key")?,
                    hash: row.try_get("hash")?,
                    revision: row.try_get("revision")?,
                })
            })
            .transpose()?;
        transaction.commit().await?;
        Ok(ScanPage { metadata, entry })
    }
    /// Snapshot allocation is bounded by the stored byte/count quotas. Read in
    /// small pages to avoid an additional whole-history SQL result allocation.
    pub async fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let mut transaction = self.pool.begin().await?;
        let metadata = self.metadata_row(
            &sqlx::query(
                "SELECT incarnation,revision,documents,encoded_bytes FROM paired_metadata WHERE singleton=1",
            )
            .fetch_one(&mut *transaction)
            .await?,
        )?;
        let mut documents = BTreeMap::new();
        let mut cursor = String::new();
        let mut total = 0usize;
        loop {
            let rows=sqlx::query("SELECT key,CASE WHEN length(document)<=65536 THEN document ELSE NULL END AS document,hash,encoded_bytes FROM paired_documents WHERE key>? ORDER BY key LIMIT 64")
                .bind(&cursor).fetch_all(&mut *transaction).await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                let key: String = row.try_get("key")?;
                let bytes: Option<Vec<u8>> = row.try_get("document")?;
                let bytes = bytes
                    .ok_or_else(|| anyhow::anyhow!("paired journal document exceeds quota"))?;
                let size = key.len() + bytes.len();
                anyhow::ensure!(
                    row.try_get::<i64, _>("encoded_bytes")? == size as i64,
                    "paired journal row size mismatch"
                );
                total = total
                    .checked_add(size)
                    .ok_or_else(|| anyhow::anyhow!("paired journal quota overflow"))?;
                anyhow::ensure!(
                    total <= self.limits.encoded_bytes && documents.len() < self.limits.documents,
                    "paired journal quota exceeded"
                );
                let document = self.decode(&key, &bytes, &row.try_get::<String, _>("hash")?)?;
                cursor = key.clone();
                documents.insert(key, document);
            }
        }
        anyhow::ensure!(
            total == metadata.encoded_bytes && documents.len() == metadata.documents,
            "paired journal snapshot metadata mismatch"
        );
        transaction.commit().await?;
        Ok(Snapshot {
            metadata,
            documents,
        })
    }
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

#[cfg(test)]
mod tests;

#[path = "journal/local.rs"]
mod local;
