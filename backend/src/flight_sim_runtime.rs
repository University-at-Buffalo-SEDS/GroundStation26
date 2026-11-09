//! Explicit startup-only FlightSim isolation. Ordinary modes are unchanged.
use std::path::{Path, PathBuf};

pub(crate) fn enabled() -> bool {
    std::env::var("GS_EXECUTION_MODE").as_deref() == Ok("flight_sim")
}

const PATHS: &[(&str, &str)] = &[
    ("GS_USERS_FILE", "users/users.json"),
    ("GS_BOARD_ABORT_HISTORY_DIR", "proof/board_abort_history"),
    ("GS_LAUNCH_CLOCK_STATE", "proof/launch_clock.json"),
    ("GS_NETWORK_VARIABLE_CACHE", "config/network_variables.json"),
    ("GS_FILL_TARGETS_CONFIG", "config/fill_targets.json"),
    ("GS_FLIGHT_SETUP_CONFIG", "config/flight_setup.json"),
    (
        "GS_LOADCELL_CALIBRATION_PATH",
        "config/loadcell_calibration.json",
    ),
    (
        "GS_CALIBRATION_CONFIG_PATH",
        "config/calibration_config.json",
    ),
    ("GS_AUTO_ZERO_CONFIG", "config/auto_zero.json"),
    ("GS_GSE_CONFIG", "config/gse_sequence.json"),
    ("GS_GSE_QUALIFICATION", "proof/gse_qualification.json"),
    ("GS_CHECKLISTS_FILE", "config/checklists.json"),
    ("GS_ACTUATOR_PLUMBING_CONFIG", "config/plumbing.json"),
    ("GS_STATION_POSITIONS_PATH", "config/station_positions.json"),
    ("GS_LAYOUT_PATH", "config/layout.json"),
    ("GS_ZERO_CAPTURE_DIR", "calibration/zero_captures"),
    ("GS_LOG_DIR", "logs"),
    ("GS_TLS_DIR", "tls"),
    ("GS_VIDEO_RUNTIME_DIR", "video/runtime"),
    ("GS_VIDEO_RECORDINGS_DIR", "video/recordings"),
    ("GS_STAGE_MODELS_DIR", "stage_models"),
];
const VALUES: &[(&str, &str)] = &[
    ("GS_EXECUTION_MODE", "flight_sim"),
    ("GS_SIM_MODE", "1"),
    ("GS_RRLS_ROLE", "off"),
    ("GS_VIDEO_MANAGED", "0"),
    ("GS_VIDEO_DISCOVERY", "0"),
    ("GS_BIND_ADDRESS", "127.0.0.1:3000"),
    ("GS_HTTPS_BIND_ADDRESS", "off"),
];

fn no_symlinks(root: &Path, path: &Path) -> anyhow::Result<()> {
    let mut current = root.to_path_buf();
    for part in path.strip_prefix(root)?.components() {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "FlightSim path is a symlink: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Must run before logger, GPIO, database, schema/config migration or producers.
/// No runtime mode flip: one process owns one output domain for its lifetime.
pub(crate) fn validate_startup() -> anyhow::Result<()> {
    match std::env::var("GS_EXECUTION_MODE") {
        Err(std::env::VarError::NotPresent) => return Ok(()),
        Ok(value) if value == "flight_sim" => {}
        _ => anyhow::bail!("Unsupported GS_EXECUTION_MODE; use explicit --flight-sim launcher"),
    }
    anyhow::ensure!(
        // RUNTIME-MODE-BOOT: FlightSim is admitted only when the process boots in
        // testing mode; a FlightSim process must never switch mode afterwards.
        crate::runtime_mode::is_testing()
            && !cfg!(any(feature = "raspberry_pi", feature = "firmware_sim")),
        "FlightSim requires testing mode (GS_STATION_MODE=testing) in a build without the raspberry_pi or firmware_sim features"
    );
    let root = PathBuf::from(
        std::env::var_os("GS_FLIGHT_SIM_ROOT")
            .ok_or_else(|| anyhow::anyhow!("Missing FlightSim session root"))?,
    );
    anyhow::ensure!(
        root.is_absolute()
            && root.canonicalize()? == root
            && std::env::current_dir()?.canonicalize()? == root,
        "FlightSim must run in its own canonical session directory"
    );
    no_symlinks(&root, &root.join("flight-sim-session.json"))?;
    let marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("flight-sim-session.json"))?)?;
    anyhow::ensure!(
        marker["version"] == 1
            && marker["execution_mode"] == "flight_sim"
            && marker["output_domain"] == "simulated"
            && marker["physical_outputs"] == false,
        "Invalid FlightSim session marker"
    );
    let assets = PathBuf::from(
        std::env::var_os("GS_FLIGHT_SIM_ASSETS")
            .ok_or_else(|| anyhow::anyhow!("Missing FlightSim asset root"))?,
    );
    anyhow::ensure!(
        assets.is_absolute()
            && root.join("frontend/dist/public").canonicalize()? == assets.canonicalize()?
            && assets.join("index.html").is_file(),
        "FlightSim frontend asset binding is invalid"
    );
    for &(key, relative) in PATHS {
        let expected = root.join(relative);
        anyhow::ensure!(
            std::env::var_os(key).as_deref() == Some(expected.as_os_str()),
            "FlightSim requires isolated {key}"
        );
        no_symlinks(&root, &expected)?;
    }
    for &(key, value) in VALUES {
        anyhow::ensure!(
            std::env::var(key).as_deref() == Ok(value),
            "Unsafe FlightSim option: {key}"
        );
    }
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy();
        if key.starts_with("GS_") {
            anyhow::ensure!(
                key == "GS_FLIGHT_SIM_ROOT"
                    || key == "GS_FLIGHT_SIM_ASSETS"
                    || key == "GS_FRONTEND_SOURCE_DIR"
                    || key == "GS_BROWSER_ASSETS_CACHE"
                    || PATHS.iter().any(|(name, _)| key == *name)
                    || VALUES.iter().any(|(name, _)| key == *name),
                "FlightSim refuses unreviewed option {key}"
            );
        }
    }
    // Seed fresh defaults explicitly: existing loaders may otherwise migrate
    // legacy files from the physical installation when an override is absent.
    let fill = crate::fill_targets::config_path();
    if !fill.exists() {
        crate::fill_targets::save(&Default::default()).map_err(anyhow::Error::msg)?;
    }
    let flight = crate::flight_setup::config_path();
    if !flight.exists() {
        crate::flight_setup::save(&Default::default()).map_err(anyhow::Error::msg)?;
    }
    let gse_path = std::env::var_os("GS_GSE_CONFIG").expect("validated FlightSim config path");
    if !Path::new(&gse_path).exists() {
        crate::gse::persist_settings(&Default::default()).map_err(anyhow::Error::msg)?;
    }
    let layout_path = std::env::var_os("GS_LAYOUT_PATH").expect("validated FlightSim layout path");
    if !Path::new(&layout_path).exists() {
        let template = include_bytes!("../layout/layout.json");
        let _: crate::layout::LayoutConfig = serde_json::from_slice(template)?;
        std::fs::write(layout_path, template)?;
    }
    anyhow::ensure!(
        crate::flight_sim::sim_mode_enabled(),
        "FlightSim model is disabled"
    );
    Ok(())
}

/// Never construct the real media router: construction itself starts relay jobs.
pub(crate) fn unavailable_media_routes() -> axum::Router<std::sync::Arc<crate::state::AppState>> {
    async fn unavailable() -> (axum::http::StatusCode, &'static str) {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Live media is unavailable in isolated FlightSim",
        )
    }
    let mut router = axum::Router::new();
    for prefix in [
        "/api/video",
        "/api/media-assets",
        "/api/stage-models",
        "/api/live_streams",
    ] {
        router = router.route(prefix, axum::routing::any(unavailable)).route(
            &format!("{prefix}/{{*path}}"),
            axum::routing::any(unavailable),
        );
    }
    router
        .route(
            "/api/vehicle_visualization",
            axum::routing::any(unavailable),
        )
        .route("/media", axum::routing::any(unavailable))
}

#[cfg(test)]
mod tests {
    #[test]
    fn namespace_contract_is_not_a_hardware_mode_alias() {
        assert_eq!(
            super::VALUES
                .iter()
                .find(|(k, _)| *k == "GS_RRLS_ROLE")
                .unwrap()
                .1,
            "off"
        );
        assert!(
            super::PATHS
                .iter()
                .any(|(k, _)| *k == "GS_BOARD_ABORT_HISTORY_DIR")
        );
    }
}
