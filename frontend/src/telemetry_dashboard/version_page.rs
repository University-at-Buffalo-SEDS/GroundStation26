use dioxus::prelude::*;
use once_cell::sync::Lazy;

const FRONTEND_CARGO_TOML: &str = include_str!("../../Cargo.toml");
const DIOXUS_TOML: &str = include_str!("../../Dioxus.toml");
const WORKSPACE_CARGO_LOCK: &str = include_str!("../../../Cargo.lock");

static VERSION_INFO: Lazy<VersionInfo> = Lazy::new(VersionInfo::load);

struct VersionInfo {
    app_version: String,
    build_number: String,
    app_name: String,
    app_title: String,
    target_os: &'static str,
    target_arch: &'static str,
    critical_packages: Vec<(&'static str, String)>,
}

impl VersionInfo {
    fn load() -> Self {
        Self {
            app_version: parse_toml_value(FRONTEND_CARGO_TOML, "package", "version")
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
            build_number: parse_toml_value(DIOXUS_TOML, "application", "build")
                .unwrap_or_else(|| "unknown".to_string()),
            app_name: parse_toml_value(DIOXUS_TOML, "application", "name")
                .unwrap_or_else(|| "Telemetry Client".to_string()),
            app_title: parse_toml_value(DIOXUS_TOML, "application", "title")
                .unwrap_or_else(|| "Telemetry Dashboard".to_string()),
            target_os: std::env::consts::OS,
            target_arch: std::env::consts::ARCH,
            critical_packages: critical_packages(),
        }
    }
}

fn parse_toml_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_section = line == format!("[{section}]");
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some(rest) = line.strip_prefix(&format!("{key} =")) {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn parse_lock_version(package_name: &str) -> Option<String> {
    for chunk in WORKSPACE_CARGO_LOCK.split("[[package]]") {
        let mut found_name = false;
        let mut version = None::<String>;
        for raw_line in chunk.lines() {
            let line = raw_line.trim();
            if let Some(name) = line.strip_prefix("name = ") {
                found_name = name.trim().trim_matches('"') == package_name;
            } else if let Some(v) = line.strip_prefix("version = ") {
                version = Some(v.trim().trim_matches('"').to_string());
            }
        }
        if found_name {
            return version;
        }
    }
    None
}

fn critical_packages() -> Vec<(&'static str, String)> {
    [
        ("dioxus", "Dioxus UI"),
        ("dioxus-desktop", "Dioxus Desktop"),
        ("tokio", "Tokio"),
        ("reqwest", "Reqwest"),
        ("tokio-tungstenite", "Tokio Tungstenite"),
        ("native-tls", "native-tls"),
        ("wry", "Wry"),
        ("axum", "Axum"),
    ]
    .into_iter()
    .filter_map(|(crate_name, label)| parse_lock_version(crate_name).map(|v| (label, v)))
    .collect()
}

#[component]
pub fn VersionTab() -> Element {
    let info = &*VERSION_INFO;

    rsx! {
        div { style: "padding:16px; overflow:visible; font-family:system-ui, -apple-system, BlinkMacSystemFont; color:inherit;",
            h2 { style: "margin:0 0 14px 0;", "Version & Credits" }

            div { style: "display:grid; grid-template-columns:repeat(auto-fit, minmax(260px, 1fr)); gap:12px;",
                SectionCard {
                    title: "Build",
                    rows: vec![
                        ("App", info.app_name.clone()),
                        ("Title", info.app_title.clone()),
                        ("Version", info.app_version.clone()),
                        ("Build", info.build_number.clone()),
                        ("Platform", format!("{} / {}", info.target_os, info.target_arch)),
                    ],
                }
                SectionCard {
                    title: "Credits",
                    rows: vec![
                        ("Project", info.app_title.clone()),
                        ("UI Mapping", "Leaflet".to_string()),
                        ("Runtime", "Rust + Dioxus".to_string()),
                        ("Packaging", "Dioxus".to_string()),
                    ],
                }
            }

            div { style: "margin-top:12px; padding:14px; border:1px solid #334155; border-radius:14px; background:#0b1220;",
                div { style: "font-size:14px; color:#94a3b8; margin-bottom:10px;", "Critical Package Info" }
                div { style: "display:grid; grid-template-columns:repeat(auto-fit, minmax(220px, 1fr)); gap:10px;",
                    for (name, version) in info.critical_packages.iter() {
                        div { style: "padding:10px 12px; border:1px solid #1f2937; border-radius:10px; background:#020617;",
                            div { style: "font-size:12px; color:#94a3b8; text-transform:uppercase; letter-spacing:0.05em;", "{name}" }
                            div { style: "margin-top:4px; font-size:15px; color:#e2e8f0; font-weight:700;", "{version}" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn SectionCard(title: &'static str, rows: Vec<(&'static str, String)>) -> Element {
    rsx! {
        div { style: "padding:14px; border:1px solid #334155; border-radius:14px; background:#0b1220; font-family:system-ui, -apple-system, BlinkMacSystemFont; color:inherit;",
            div { style: "font-size:14px; color:#94a3b8; margin-bottom:10px;", "{title}" }
            div { style: "display:flex; flex-direction:column; gap:10px;",
                for (label, value) in rows {
                    div { style: "padding:10px 12px; border:1px solid #1f2937; border-radius:10px; background:#020617;",
                        div { style: "font-size:12px; color:#94a3b8; text-transform:uppercase; letter-spacing:0.05em;", "{label}" }
                        div { style: "margin-top:4px; font-size:15px; color:#e2e8f0; font-weight:700; word-break:break-word;", "{value}" }
                    }
                }
            }
        }
    }
}
