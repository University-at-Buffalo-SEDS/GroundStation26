use sqlx::Row;
use std::path::PathBuf;
use tokio::fs;

/// Default region for map tiles. Change this to switch regions in code.
pub const DEFAULT_MAP_REGION: &str = "north_america";

pub fn map_region_base_dir(region: &str) -> PathBuf {
    PathBuf::from(format!("./backend/data/maps/{region}"))
}

pub fn tile_bundle_path(region: &str) -> PathBuf {
    map_region_base_dir(region).join("tiles.sqlite")
}

/// Ensure tiles for a given region are available locally.
/// Directory layout after success:
///   ./data/maps/<region>/tiles/{z}/{x}/{y}.png
pub async fn ensure_map_data(region: &str) -> anyhow::Result<()> {
    let base_dir = map_region_base_dir(region);
    let tiles_dir = base_dir.join("tiles");
    let bundle_path = tile_bundle_path(region);

    if fs::try_exists(&bundle_path).await.unwrap_or(false) {
        return Ok(());
    }

    if fs::try_exists(&tiles_dir).await.unwrap_or(false) {
        let mut entries = fs::read_dir(&tiles_dir).await?;
        if entries.next_entry().await?.is_some() {
            // Tiles exist, all good.
            return Ok(());
        }
    }

    anyhow::bail!(
        "No map data found (expected {} or {}). Run `python3 download_map.py` to generate offline map data.",
        tiles_dir.display(),
        bundle_path.display()
    );
}

/// Detect the highest zoom level present under ./backend/data/maps/<region>/tiles/<z>/...
/// Returns None when no numeric zoom directories are found.
pub async fn detect_max_native_zoom(region: &str) -> anyhow::Result<Option<u32>> {
    let bundle_path = tile_bundle_path(region);
    if fs::try_exists(&bundle_path).await.unwrap_or(false) {
        let db_url = format!("sqlite://{}?mode=ro", bundle_path.to_string_lossy());
        if let Ok(pool) = sqlx::SqlitePool::connect(&db_url).await {
            let max_zoom: Option<i64> = match sqlx::query("SELECT MAX(z) AS max_zoom FROM tiles")
                .fetch_optional(&pool)
                .await
            {
                Ok(Some(row)) => row.try_get("max_zoom").ok(),
                Ok(None) => None,
                Err(_) => None,
            };
            pool.close().await;
            if let Some(v) = max_zoom.and_then(|v| u32::try_from(v).ok()) {
                return Ok(Some(v));
            }
        }
    }

    let tiles_dir = map_region_base_dir(region).join("tiles");
    if !fs::try_exists(&tiles_dir).await.unwrap_or(false) {
        return Ok(None);
    }

    let mut max_zoom: Option<u32> = None;
    let mut entries = fs::read_dir(&tiles_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let file_type = entry.file_type().await?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(z) = name.parse::<u32>() else {
            continue;
        };
        max_zoom = Some(max_zoom.map_or(z, |prev| prev.max(z)));
    }

    Ok(max_zoom)
}
