//! Station settings a replay bundle snapshots so that the Settings page of an
//! offline replay shows the station's configuration at export, read-only.
//! Each is the body of its own route, served by a client for that route.
//! Live state (sync status, sequences, board status) is not included: those
//! panels need a station.
use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

/// (route, entry path). The translation catalog is the English one, which
/// the UI requests by default.
pub(crate) const ROUTES: [(&str, &str); 8] = [
    ("/api/fill_targets", "config/station/fill_targets.json"),
    ("/api/flight_setup", "config/station/flight_setup.json"),
    ("/api/auto_zero", "config/station/auto_zero.json"),
    ("/api/calibration/kg50_input", "config/station/kg50_input.json"),
    ("/api/gse/config", "config/station/gse_config.json"),
    ("/api/fc/launch-site", "config/station/launch_site.json"),
    ("/api/safety/about", "config/station/safety_about.json"),
    ("/api/i18n/catalog?lang=en", "config/station/i18n_catalog_en.json"),
];

/// The handlers of [`ROUTES`] that are not in `web::bundle_config_routes`.
pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new().merge(crate::gse::bundle_routes()).merge(crate::launch_site::routes())
}
