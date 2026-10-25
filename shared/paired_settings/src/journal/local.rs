//! Local causal context is persisted with its edit, never inferred from an
//! unseen imported head. Activation acknowledges remote context explicitly.
use super::*;
use crate::Revision;
use serde_json::Value;
const OBSERVED_LIMIT: usize = 16 * 1024 * 1024;
impl Journal {
    pub(super) async fn local_tables(&self) -> anyhow::Result<()> {
        sqlx::query("CREATE TABLE IF NOT EXISTS paired_observed(key TEXT PRIMARY KEY,document BLOB NOT NULL,encoded_bytes INTEGER NOT NULL)").execute(&self.pool).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS paired_observation_quota(singleton INTEGER PRIMARY KEY CHECK(singleton=1),documents INTEGER NOT NULL,encoded_bytes INTEGER NOT NULL)").execute(&self.pool).await?;
        sqlx::query("INSERT OR IGNORE INTO paired_observation_quota VALUES(1,0,0)")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    /// Applied context, never inferred from an unreviewed remote head.
    pub async fn observed(&self, key: &str) -> anyhow::Result<Option<Revision>> {
        let row: Option<Option<Vec<u8>>> = sqlx::query_scalar("SELECT CASE WHEN length(document)<=65536 THEN document ELSE NULL END FROM paired_observed WHERE key=?")
            .bind(key).fetch_optional(&self.pool).await?;
        match row {
            None => Ok(None),
            Some(Some(bytes)) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Some(None) => anyhow::bail!("paired observation exceeds quota"),
        }
    }
    async fn set_observed(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        key: &str,
        revision: &Revision,
    ) -> anyhow::Result<()> {
        let body = serde_json::to_vec(revision)?;
        anyhow::ensure!(
            body.len() <= DOCUMENT_LIMIT,
            "paired observation exceeds quota"
        );
        let old: Option<i64> =
            sqlx::query_scalar("SELECT encoded_bytes FROM paired_observed WHERE key=?")
                .bind(key)
                .fetch_optional(&mut **transaction)
                .await?;
        let row = sqlx::query(
            "SELECT documents,encoded_bytes FROM paired_observation_quota WHERE singleton=1",
        )
        .fetch_one(&mut **transaction)
        .await?;
        let count = row
            .try_get::<i64, _>("documents")?
            .checked_add(i64::from(old.is_none()))
            .ok_or_else(|| anyhow::anyhow!("observation quota overflow"))?;
        let bytes = row
            .try_get::<i64, _>("encoded_bytes")?
            .checked_sub(old.unwrap_or(0))
            .and_then(|n| n.checked_add((key.len() + body.len()) as i64))
            .ok_or_else(|| anyhow::anyhow!("observation quota overflow"))?;
        anyhow::ensure!(
            count >= 0
                && count <= self.limits.documents as i64
                && bytes >= 0
                && bytes <= OBSERVED_LIMIT as i64,
            "paired observation quota exceeded"
        );
        sqlx::query("INSERT INTO paired_observed VALUES(?,?,?) ON CONFLICT(key) DO UPDATE SET document=excluded.document,encoded_bytes=excluded.encoded_bytes").bind(key).bind(body).bind((key.len()+serde_json::to_vec(revision)?.len()) as i64).execute(&mut **transaction).await?;
        sqlx::query(
            "UPDATE paired_observation_quota SET documents=?,encoded_bytes=? WHERE singleton=1",
        )
        .bind(count)
        .bind(bytes)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }
    /// Only a fully applied, exact single head may advance local context.
    pub async fn acknowledge(&self, key: &str, expected: &Revision) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT CASE WHEN length(document)<=65536 THEN document ELSE NULL END AS document,hash FROM paired_documents WHERE key=?").bind(key).fetch_one(&mut *tx).await?;
        let body: Option<Vec<u8>> = row.try_get("document")?;
        let body = body.ok_or_else(|| anyhow::anyhow!("paired document exceeds quota"))?;
        let doc = self.decode(key, &body, &row.try_get::<String, _>("hash")?)?;
        anyhow::ensure!(
            doc.heads.as_slice() == std::slice::from_ref(expected),
            "paired value changed before application acknowledgement"
        );
        let observed:Option<Option<Vec<u8>>>=sqlx::query_scalar("SELECT CASE WHEN length(document)<=65536 THEN document ELSE NULL END FROM paired_observed WHERE key=?").bind(key).fetch_optional(&mut *tx).await?;
        if let Some(Some(bytes)) = observed
            && serde_json::from_slice::<Revision>(&bytes)? == *expected
        {
            tx.commit().await?;
            return Ok(());
        }
        self.set_observed(&mut tx, key, expected).await?;
        tx.commit().await?;
        Ok(())
    }
    /// An explicit local API or successful local engine event creates this edit.
    /// Raw remote reception must use merge instead and never mint a local head.
    pub async fn edit(
        &self,
        key: &str,
        value: Value,
        station: &str,
        validate: impl Fn(&str, &Document) -> anyhow::Result<()>,
    ) -> anyhow::Result<Document> {
        anyhow::ensure!(
            self.peers.iter().any(|peer| peer == station),
            "unpaired editing station"
        );
        let mut tx = self.pool.begin().await?;
        let metadata=self.metadata_row(&sqlx::query("SELECT incarnation,revision,documents,encoded_bytes FROM paired_metadata WHERE singleton=1").fetch_one(&mut *tx).await?)?;
        let old=sqlx::query("SELECT CASE WHEN length(document)<=65536 THEN document ELSE NULL END AS document,hash,encoded_bytes FROM paired_documents WHERE key=?").bind(key).fetch_optional(&mut *tx).await?;
        let mut temporary = Store::new(self.installation.clone())?;
        let old_size = if let Some(row) = &old {
            let bytes: Option<Vec<u8>> = row.try_get("document")?;
            let bytes = bytes.ok_or_else(|| anyhow::anyhow!("paired document exceeds quota"))?;
            let document = self.decode(key, &bytes, &row.try_get::<String, _>("hash")?)?;
            temporary.merge(&self.installation, key, &document, &self.peers)?;
            key.len() + bytes.len()
        } else {
            0
        };
        let observed:Option<Option<Vec<u8>>>=sqlx::query_scalar("SELECT CASE WHEN length(document)<=65536 THEN document ELSE NULL END FROM paired_observed WHERE key=?").bind(key).fetch_optional(&mut *tx).await?;
        let observed = observed
            .map(|bytes| -> anyhow::Result<Revision> {
                Ok(serde_json::from_slice(&bytes.ok_or_else(|| {
                    anyhow::anyhow!("paired observation exceeds quota")
                })?)?)
            })
            .transpose()?;
        let before = temporary.document(key).cloned();
        let document =
            temporary.edit(key, observed.as_ref(), value.clone(), station, &self.peers)?;
        validate(key, &document)?;
        if before.as_ref() == Some(&document) {
            tx.commit().await?;
            return Ok(document);
        }
        self.save_local_document(&mut tx, key, &document, old_size, &metadata)
            .await?;
        let own = document
            .heads
            .iter()
            .filter_map(|revision| revision.clock.get(station))
            .copied()
            .max()
            .unwrap_or(0);
        let revision = document
            .heads
            .iter()
            .find(|revision| revision.value == value && revision.clock.get(station) == Some(&own))
            .ok_or_else(|| anyhow::anyhow!("local paired edit lost its revision"))?;
        self.set_observed(&mut tx, key, revision).await?;
        tx.commit().await?;
        Ok(document)
    }
    async fn save_local_document(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        key: &str,
        document: &Document,
        old_size: usize,
        metadata: &Metadata,
    ) -> anyhow::Result<()> {
        let bytes = transport::document_bytes(document);
        let hash = transport::document_hash(document);
        let size = key.len() + bytes.len();
        let count = metadata.documents + usize::from(old_size == 0);
        let total = metadata
            .encoded_bytes
            .checked_sub(old_size)
            .and_then(|n| n.checked_add(size))
            .ok_or_else(|| anyhow::anyhow!("paired journal quota overflow"))?;
        anyhow::ensure!(
            count <= self.limits.documents && total <= self.limits.encoded_bytes,
            "paired journal quota exceeded"
        );
        let next = metadata
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("paired revision exhausted"))?;
        sqlx::query("INSERT INTO paired_documents VALUES(?,?,?,?,?) ON CONFLICT(key) DO UPDATE SET document=excluded.document,hash=excluded.hash,revision=excluded.revision,encoded_bytes=excluded.encoded_bytes").bind(key).bind(bytes).bind(hash).bind(next).bind(size as i64).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO paired_document_heads VALUES(?,?) ON CONFLICT(key) DO UPDATE SET heads=excluded.heads").bind(key).bind(document.heads.len() as i64).execute(&mut **tx).await?;
        sqlx::query(
            "UPDATE paired_metadata SET revision=?,documents=?,encoded_bytes=? WHERE singleton=1",
        )
        .bind(next)
        .bind(count as i64)
        .bind(total as i64)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
    /// An administrator resolves precisely the heads they reviewed.
    pub async fn resolve(
        &self,
        key: &str,
        expected: &Document,
        value: Value,
        station: &str,
        validate: impl Fn(&str, &Document) -> anyhow::Result<()>,
    ) -> anyhow::Result<Document> {
        let mut tx = self.pool.begin().await?;
        let metadata=self.metadata_row(&sqlx::query("SELECT incarnation,revision,documents,encoded_bytes FROM paired_metadata WHERE singleton=1").fetch_one(&mut *tx).await?)?;
        let row=sqlx::query("SELECT CASE WHEN length(document)<=65536 THEN document ELSE NULL END AS document,hash FROM paired_documents WHERE key=?").bind(key).fetch_one(&mut *tx).await?;
        let body: Option<Vec<u8>> = row.try_get("document")?;
        let body = body.ok_or_else(|| anyhow::anyhow!("paired document exceeds quota"))?;
        let current = self.decode(key, &body, &row.try_get::<String, _>("hash")?)?;
        anyhow::ensure!(
            current == *expected,
            "paired document changed; review its current versions"
        );
        let mut temporary = Store::new(self.installation.clone())?;
        temporary.merge(&self.installation, key, &current, &self.peers)?;
        let resolved = temporary.resolve(key, expected, value, station, &self.peers)?;
        validate(key, &resolved)?;
        self.save_local_document(&mut tx, key, &resolved, key.len() + body.len(), &metadata)
            .await?;
        self.set_observed(&mut tx, key, &resolved.heads[0]).await?;
        tx.commit().await?;
        Ok(resolved)
    }
}
