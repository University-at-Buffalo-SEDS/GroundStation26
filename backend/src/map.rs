use std::path::PathBuf;
use tokio::fs;
use tower_http::services::ServeDir;

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

/// Service that serves `/tiles/{z}/{x}/{y}.png` for a region.
pub fn tile_service(region: &str) -> ServeDir {
    let tiles_dir = format!("./backend/data/maps/{region}/tiles");
    ServeDir::new(tiles_dir)
}
