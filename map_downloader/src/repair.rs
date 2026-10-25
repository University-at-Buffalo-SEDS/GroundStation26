//! Targeted replacement retains each old tile until a validated new image commits.
use crate::{TileOutcome, network::TileClient, profile::Profile};
use anyhow::{Result, ensure};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    tiles: &BTreeSet<(u32, u32, u32)>,
    path: &Path,
    legacy: &Path,
    client: &TileClient,
    profile: Profile,
    direct: bool,
    attempts: u32,
    base_url: &str,
) -> Result<()> {
    ensure!(
        !tiles.is_empty() && tiles.len() <= 32,
        "repair requires 1..=32 selected tiles"
    );
    ensure!(
        !direct || crate::should_build_bundle(),
        "direct repair requires bundle generation enabled"
    );
    let bytes = Arc::new(AtomicU64::new(0));
    let limiter = crate::max_bandwidth_mibps()
        .map(crate::BandwidthLimiter::new)
        .map(Arc::new);
    let mut failed = 0;
    println!(
        "Targeted map repair: {} tiles only; existing data retained until each valid replacement commits",
        tiles.len()
    );
    if direct {
        let budget = if profile == Profile::Rrls {
            Some(crate::budget::DatasetBudget::open(path)?)
        } else {
            None
        };
        let pool = crate::init_direct_bundle_pool_with_budget(path, true, budget.clone()).await?;
        let directory = path.parent().unwrap_or(Path::new("."));
        let reserve = crate::storage::minimum_free_bytes()?;
        crate::storage::ensure_space(directory, reserve.saturating_add(16 * 1024 * 1024))?;
        let (writer, task) = crate::storage::spawn_with_budget(
            pool.clone(),
            directory.to_path_buf(),
            reserve,
            budget.clone(),
        );
        for &(z, x, y) in tiles {
            if crate::fetch_tile_with_retries_to_db(
                z,
                x,
                y,
                client,
                &writer,
                &bytes,
                None,
                limiter.as_ref(),
                attempts,
                base_url,
            )
            .await
                != TileOutcome::Complete
            {
                failed += 1;
            }
        }
        let error = writer.failure();
        drop(writer);
        task.await?;
        if let Some(error) = error {
            anyhow::bail!("targeted map writer failed: {error}")
        }
        crate::finalize_direct_bundle_pool(&pool, budget.as_deref()).await?;
        pool.close().await;
    } else {
        for &(z, x, y) in tiles {
            let dir = legacy.join(z.to_string());
            std::fs::create_dir_all(dir.join(x.to_string()))?;
            crate::storage::ensure_space(
                &dir,
                crate::storage::minimum_free_bytes()?
                    .saturating_add(crate::network::MAX_TILE_BYTES as u64),
            )?;
            if crate::fetch_tile_with_retries(
                z,
                x,
                y,
                &dir,
                client,
                &bytes,
                None,
                limiter.as_ref(),
                attempts,
                base_url,
                true,
            )
            .await
                != TileOutcome::Complete
            {
                failed += 1;
            }
        }
    }
    ensure!(
        failed == 0,
        "targeted map repair failed for {failed} tiles; their previous cached data was retained"
    );
    println!(
        "Targeted map repair complete: {} tiles, {} validated image bytes",
        tiles.len(),
        bytes.load(Ordering::Relaxed)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    fn server(replies: Vec<Vec<u8>>) -> (String, std::thread::JoinHandle<usize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            let mut count = 0;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            for body in replies {
                loop {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "repair did not issue expected request"
                    );
                    let Ok((mut socket, _)) = listener.accept() else {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        continue;
                    };
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    while !request.windows(4).any(|s| s == b"\r\n\r\n") && request.len() < 8192 {
                        let mut chunk = [0; 512];
                        let n = socket.read(&mut chunk).unwrap();
                        assert!(n > 0);
                        request.extend_from_slice(&chunk[..n]);
                    }
                    assert!(
                        request.starts_with(b"GET /0/0/0 HTTP/1.1"),
                        "repair requested an unselected tile"
                    );
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    socket.write_all(&body).unwrap();
                    count += 1;
                    break;
                }
            }
            count
        });
        (url, worker)
    }
    async fn check(direct: bool, success: bool) {
        let root = std::env::temp_dir().join(format!(
            "gs-targeted-repair-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("tiles.sqlite3");
        let legacy = root.join("tiles");
        let valid = crate::raster::image();
        let mut old = valid.clone();
        old[42] ^= 128;
        assert!(crate::raster::cached_header_ok(
            &old[..32],
            &old[old.len() - 12..],
            old.len() as u64
        ));
        assert!(
            crate::raster::validate(&old).is_err(),
            "fixture must exercise corruption beyond cheap header checks"
        );
        if direct {
            let db = crate::init_direct_bundle_pool(&path, true).await.unwrap();
            crate::upsert_tile_to_bundle(&db, 0, 0, 0, &old)
                .await
                .unwrap();
            crate::upsert_tile_to_bundle(&db, 1, 1, 1, &valid)
                .await
                .unwrap();
            db.close().await;
        } else {
            std::fs::create_dir_all(legacy.join("0/0")).unwrap();
            std::fs::create_dir_all(legacy.join("1/1")).unwrap();
            std::fs::write(legacy.join("0/0/0.jpg"), &old).unwrap();
            std::fs::write(legacy.join("1/1/1.jpg"), &valid).unwrap();
        }
        let bad = b"<html>server temporarily busy</html>".to_vec();
        let (url, worker) = server(vec![bad.clone(), if success { valid.clone() } else { bad }]);
        let client = TileClient::from(
            reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap(),
        );
        let result = run(
            &BTreeSet::from([(0, 0, 0)]),
            &path,
            &legacy,
            &client,
            Profile::GroundStation,
            direct,
            2,
            &url,
        )
        .await;
        assert_eq!(result.is_ok(), success);
        assert_eq!(worker.join().unwrap(), 2);
        let expected = if success { &valid } else { &old };
        if direct {
            let db = crate::init_direct_bundle_pool(&path, true).await.unwrap();
            let get = |z, x, y| {
                sqlx::query_scalar::<_,Vec<u8>>("SELECT b.image FROM tiles t JOIN tile_blobs b ON b.id=t.blob_id WHERE z=? AND x=? AND y=?").bind(z).bind(x).bind(y)
            };
            assert_eq!(&get(0, 0, 0).fetch_one(&db).await.unwrap(), expected);
            assert_eq!(get(1, 1, 1).fetch_one(&db).await.unwrap(), valid);
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tiles")
                    .fetch_one(&db)
                    .await
                    .unwrap(),
                2
            );
            db.close().await;
        } else {
            assert_eq!(&std::fs::read(legacy.join("0/0/0.jpg")).unwrap(), expected);
            assert_eq!(std::fs::read(legacy.join("1/1/1.jpg")).unwrap(), valid);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn selected_db_repair_retries_html_then_replaces_valid_header_corruption() {
        check(true, true).await;
    }
    #[tokio::test]
    async fn failed_selected_db_repair_preserves_old_and_unselected_rows() {
        check(true, false).await;
    }
    #[tokio::test]
    async fn selected_legacy_repair_retries_html_then_replaces_valid_header_corruption() {
        check(false, true).await;
    }
    #[tokio::test]
    async fn failed_selected_legacy_repair_preserves_old_and_unselected_files() {
        check(false, false).await;
    }
}
