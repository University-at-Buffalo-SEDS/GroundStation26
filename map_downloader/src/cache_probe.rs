//! Inspect only the few selected --test coordinates, without opening a writable DB.
use anyhow::{Context, Result, bail};
use sqlx::{
    Connection,
    sqlite::{SqliteConnectOptions, SqliteConnection},
};
use std::path::Path;

pub(crate) async fn check(bundle: &Path, tiles: &Path, samples: &[(u32, u32, u32)]) -> Result<()> {
    let mut database = if bundle.exists() {
        let wal = bundle.with_file_name(format!(
            "{}-wal",
            bundle.file_name().unwrap_or_default().to_string_lossy()
        ));
        if wal.metadata().is_ok_and(|m| m.len() > 0) {
            bail!(
                "Cached map check deferred: bundle has a nonempty WAL; stop the downloader after its checkpoint before running --test. No cache data was changed."
            )
        }
        Some(
            SqliteConnection::connect_with(
                &SqliteConnectOptions::new()
                    .filename(bundle)
                    .read_only(true)
                    .immutable(true),
            )
            .await
            .context("read-only cached map bundle check failed")?,
        )
    } else {
        None
    };
    let mut found = 0;
    let mut invalid = 0;
    for &(z, x, y) in samples {
        let saved = if let Some(db) = database.as_mut() {
            sqlx::query_as::<_,(i64,Option<Vec<u8>>)>("SELECT length(b.image),CASE WHEN length(b.image)<=524288 THEN b.image ELSE NULL END FROM tiles t JOIN tile_blobs b ON b.id=t.blob_id WHERE t.z=? AND t.x=? AND t.y=?")
                .bind(i64::from(z)).bind(i64::from(x)).bind(i64::from(y)).fetch_optional(db).await.context("read-only cached tile lookup failed")?
        } else {
            None
        };
        let file = tiles.join(format!("{z}/{x}/{y}.jpg"));
        let bytes = if let Some((_, bytes)) = saved {
            Some(bytes)
        } else if file.exists() {
            Some(
                if file.metadata()?.len() <= crate::network::MAX_TILE_BYTES as u64 {
                    Some(tokio::fs::read(&file).await?)
                } else {
                    None
                },
            )
        } else {
            None
        };
        if let Some(bytes) = bytes {
            found += 1;
            let valid = if let Some(bytes) = bytes {
                crate::raster::validate_download(bytes).await.is_ok()
            } else {
                false
            };
            if valid {
                println!("Cached tile z={z} x={x} y={y}: OK PNG/JPEG");
            } else {
                invalid += 1;
                eprintln!(
                    "Cached tile z={z} x={x} y={y}: FAILED invalid/truncated PNG/JPEG; repeat the configured download command with --repair-tile {z}/{x}/{y} instead of --test. No tile was deleted."
                );
            }
        }
    }
    if let Some(db) = database {
        db.close().await?;
    }
    println!(
        "Read-only cache sample: {found} cached tiles inspected, {invalid} invalid; uncached sample tiles need downloading."
    );
    if invalid > 0 {
        bail!("cached map sample contains {invalid} invalid tiles")
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn samples_only_use_readonly_cache_and_report_corrupt_selected_tiles() {
        let dir = std::env::temp_dir().join(format!(
            "gs-cache-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiles.sqlite3");
        let db = crate::init_direct_bundle_pool(&path, true).await.unwrap();
        crate::upsert_tile_to_bundle(&db, 0, 0, 0, &crate::raster::image())
            .await
            .unwrap();
        crate::upsert_tile_to_bundle(&db, 1, 0, 0, b"<html>busy</html>")
            .await
            .unwrap();
        crate::upsert_tile_to_bundle(&db, 1, 1, 0, b"\xff\xd8\xfftruncated")
            .await
            .unwrap();
        crate::upsert_tile_to_bundle(&db, 1, 1, 1, b"\x89PNG\r\n\x1a\n")
            .await
            .unwrap();
        crate::upsert_tile_to_bundle(&db, 1, 0, 1, &crate::raster::image())
            .await
            .unwrap();
        let mut pending = std::collections::HashSet::from([(0, 0), (1, 0), (1, 1), (0, 1)]);
        crate::remove_saved_coords(&db, 1, &mut pending)
            .await
            .unwrap();
        assert_eq!(
            pending,
            std::collections::HashSet::from([(0, 0), (1, 0), (1, 1)]),
            "resume skips valid data and queues only bad cached headers"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tiles")
                .fetch_one(&db)
                .await
                .unwrap(),
            5,
            "resume scan must not delete cached data"
        );
        db.close().await;
        let before = std::fs::read(&path).unwrap();
        let entries = std::fs::read_dir(&dir).unwrap().count();
        check(&path, &dir.join("tiles"), &[(0, 0, 0)])
            .await
            .unwrap();
        assert!(
            check(
                &path,
                &dir.join("tiles"),
                &[(1, 0, 0), (1, 1, 0), (1, 1, 1)]
            )
            .await
            .is_err()
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "read-only probe changed database"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            entries,
            "probe created sidecars"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn nonempty_wal_is_reported_without_opening_or_mutating_cache() {
        let root = std::env::temp_dir().join(format!("gs-map-probe-wal-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("tiles.sqlite3");
        let wal = root.join("tiles.sqlite3-wal");
        std::fs::write(&db, b"fixture database not opened").unwrap();
        std::fs::write(&wal, b"uncheckpointed fixture").unwrap();
        let error = check(&db, &root.join("tiles"), &[(0, 0, 0)])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("nonempty WAL"));
        assert_eq!(std::fs::read(&wal).unwrap(), b"uncheckpointed fixture");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn missing_cache_probe_creates_no_directory_or_database() {
        let root = std::env::temp_dir().join(format!(
            "gs-map-probe-must-not-create-{}",
            std::process::id()
        ));
        assert!(!root.exists());
        check(
            &root.join("tiles.sqlite3"),
            &root.join("tiles"),
            &[(0, 0, 0)],
        )
        .await
        .unwrap();
        assert!(!root.exists());
    }
}
