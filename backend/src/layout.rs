use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const DEFAULT_LAYOUT_PATH: &str = "layout/layout.json";
#[cfg(feature = "hitl_mode")]
const DEFAULT_HITL_LAYOUT_PATH: &str = "layout/layout_hitl.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutConfig {
    pub version: u32,
    #[serde(default = "default_main_tabs")]
    pub main_tabs: Vec<String>,
    pub connection_tab: ConnectionTabLayout,
    #[serde(default)]
    pub network_tab: NetworkTabLayout,
    pub actions_tab: ActionsTabLayout,
    pub data_tab: DataTabLayout,
    pub state_tab: StateTabLayout,
    #[serde(default)]
    pub battery: BatteryLayoutConfig,
}

fn default_main_tabs() -> Vec<String> {
    vec![
        "state".to_string(),
        "connection-status".to_string(),
        "map".to_string(),
        "actions".to_string(),
        "calibration".to_string(),
        "notifications".to_string(),
        "warnings".to_string(),
        "errors".to_string(),
        "data".to_string(),
        "network-topology".to_string(),
        "detailed".to_string(),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionTabLayout {
    pub sections: Vec<ConnectionSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionSection {
    pub kind: ConnectionSectionKind,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkTabLayout {
    #[serde(default)]
    pub enabled: bool,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionSectionKind {
    BoardStatus,
    Latency,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataTabLayout {
    pub tabs: Vec<DataTabSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataTabSpec {
    pub id: String,
    pub label: String,
    pub channels: Vec<String>,
    pub chart: Option<DataTabChart>,
    pub boolean_labels: Option<BooleanLabels>,
    pub channel_boolean_labels: Option<Vec<BooleanLabels>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BatteryLayoutConfig {
    #[serde(default)]
    pub estimator: BatteryEstimatorConfig,
    #[serde(default)]
    pub sources: Vec<BatterySourceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryEstimatorConfig {
    #[serde(default = "default_battery_window_seconds")]
    pub window_seconds: u64,
    #[serde(default = "default_battery_min_drop_rate_v_per_min")]
    pub min_drop_rate_v_per_min: f32,
}

impl Default for BatteryEstimatorConfig {
    fn default() -> Self {
        Self {
            window_seconds: default_battery_window_seconds(),
            min_drop_rate_v_per_min: default_battery_min_drop_rate_v_per_min(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatterySourceConfig {
    pub id: String,
    pub label: String,
    pub sender_id: String,
    #[serde(default = "default_battery_input_data_type")]
    pub input_data_type: String,
    pub percent_data_type: String,
    pub drop_rate_data_type: String,
    pub remaining_minutes_data_type: String,
    pub empty_voltage: f32,
    pub full_voltage: f32,
    #[serde(default = "default_battery_curve_exponent")]
    pub curve_exponent: f32,
}

fn default_battery_window_seconds() -> u64 {
    300
}

fn default_battery_min_drop_rate_v_per_min() -> f32 {
    0.005
}

fn default_battery_input_data_type() -> String {
    "BATTERY_VOLTAGE".to_string()
}

fn default_battery_curve_exponent() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataTabChart {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionsTabLayout {
    #[serde(default)]
    pub disable_actions_by_default: bool,
    pub actions: Vec<ActionSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSpec {
    pub label: String,
    pub cmd: String,
    pub border: String,
    pub bg: String,
    pub fg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateTabLayout {
    pub states: Vec<StateLayout>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateLayout {
    pub states: Vec<String>,
    pub sections: Vec<StateSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSection {
    pub title: Option<String>,
    pub widgets: Vec<StateWidget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateWidget {
    pub kind: StateWidgetKind,
    pub data_type: Option<String>,
    pub items: Option<Vec<SummaryItem>>,
    pub chart_title: Option<String>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub actions: Option<Vec<String>>,
    pub valves: Option<Vec<SummaryItem>>,
    pub valve_colors: Option<ValveColorSet>,
    pub boolean_labels: Option<BooleanLabels>,
    pub valve_labels: Option<Vec<BooleanLabels>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BooleanLabels {
    pub true_label: String,
    pub false_label: String,
    pub unknown_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValveColor {
    pub bg: String,
    pub border: String,
    pub fg: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValveColorSet {
    pub open: Option<ValveColor>,
    pub closed: Option<ValveColor>,
    pub unknown: Option<ValveColor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryItem {
    pub label: String,
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateWidgetKind {
    BoardStatus,
    Summary,
    Chart,
    ValveState,
    Map,
    Actions,
}

pub fn layout_path() -> PathBuf {
    if let Ok(path) = std::env::var("GS_LAYOUT_PATH") {
        return PathBuf::from(path);
    }

    #[cfg(feature = "hitl_mode")]
    {
        let hitl = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_HITL_LAYOUT_PATH);
        if hitl.exists() {
            return hitl;
        }
    }

    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_LAYOUT_PATH)
}

pub fn load_layout() -> Result<LayoutConfig, String> {
    let path = layout_path();
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read layout file {path:?}: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("Invalid layout JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_json_is_valid() {
        let layout = load_layout().expect("layout should parse");
        assert!(layout.version >= 1);
        assert!(!layout.connection_tab.sections.is_empty());
        assert!(!layout.actions_tab.actions.is_empty());
        assert!(!layout.data_tab.tabs.is_empty());
        assert!(!layout.state_tab.states.is_empty());
    }
}
