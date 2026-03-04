use std::path::PathBuf;
use tokio::fs;

/// Default region for map tiles. Change this to switch regions in code.
pub const DEFAULT_MAP_REGION: &str = "north_america";

/// Ensure tiles for a given region are available locally.
/// Directory layout after success:
///   ./data/maps/<region>/tiles/{z}/{x}/{y}.png
pub async fn ensure_map_data(region: &str) -> anyhow::Result<()> {
    let base_dir = PathBuf::from(format!("./backend/data/maps/{region}"));
    let tiles_dir = base_dir.join("tiles");

    if fs::try_exists(&tiles_dir).await.unwrap_or(false) {
        let mut entries = fs::read_dir(&tiles_dir).await?;
        if entries.next_entry().await?.is_some() {
            // Tiles exist, all good.
            return Ok(());
        }
    }

    anyhow::bail!(
        "No tiles found in {}. Run `groundstation_maps bootstrap-{region}` to generate offline tiles.",
        tiles_dir.display()
    );
}

/// Detect the highest zoom level present under ./backend/data/maps/<region>/tiles/<z>/...
/// Returns None when no numeric zoom directories are found.
pub async fn detect_max_native_zoom(region: &str) -> anyhow::Result<Option<u32>> {
    let tiles_dir = PathBuf::from(format!("./backend/data/maps/{region}/tiles"));
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
