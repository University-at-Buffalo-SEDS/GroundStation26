use std::{env, fs, io::Write, path::Path, path::PathBuf};

fn main() {
    // Re-run if this file changes
    println!("cargo:rerun-if-changed=build.rs");

    // Allow changing Leaflet version via env if you ever want
    println!("cargo:rerun-if-env-changed=LEAFLET_VERSION");

    let version = env::var("LEAFLET_VERSION").unwrap_or_else(|_| "1.9.4".to_string());

    // Path to frontend/dist/vendor/leaflet relative to this crate
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let leaflet_dir = manifest_dir.join("dist").join("vendor").join("leaflet");

    if let Err(e) = fs::create_dir_all(&leaflet_dir) {
        eprintln!("Failed to create Leaflet vendor dir {leaflet_dir:?}: {e}");
        return;
    }

    // Download CSS and JS
    if let Err(e) = download_leaflet_file(&leaflet_dir, &version, "css") {
        eprintln!("Failed to download Leaflet CSS: {e}");
    }
    if let Err(e) = download_leaflet_file(&leaflet_dir, &version, "js") {
        eprintln!("Failed to download Leaflet JS: {e}");
    }
}

fn download_leaflet_file(
    leaflet_dir: &Path,
    version: &str,
    kind: &str, // "css" or "js"
) -> Result<(), Box<dyn std::error::Error>> {
    let filename = format!("leaflet.{kind}");
    let out_path = leaflet_dir.join(&filename);

    // If file already exists, don't redownload every build
    if out_path.exists() {
        return Ok(());
    }

    let url = format!(
        "https://unpkg.com/leaflet@{version}/dist/leaflet.{kind}",
    );
    println!("Downloading {url} -> {}", out_path.display());

    let resp = reqwest::blocking::get(&url)?;
    if !resp.status().is_success() {
        return Err(format!("HTTP error: {}", resp.status()).into());
    }

    let bytes = resp.bytes()?;
    let mut file = fs::File::create(&out_path)?;
    file.write_all(&bytes)?;
    file.flush()?;

    Ok(())
}
