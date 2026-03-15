use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::types::FlightState;

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
    ]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionTabLayout {
    pub sections: Vec<ConnectionSection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionSection {
    pub kind: ConnectionSectionKind,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NetworkTabLayout {
    #[serde(default)]
    pub enabled: bool,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionSectionKind {
    BoardStatus,
    Latency,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataTabLayout {
    pub tabs: Vec<DataTabSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataTabSpec {
    pub id: String,
    pub label: String,
    pub channels: Vec<String>,
    pub chart: Option<DataTabChart>,
    pub boolean_labels: Option<BooleanLabels>,
    pub channel_boolean_labels: Option<Vec<BooleanLabels>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BatteryLayoutConfig {
    #[serde(default)]
    pub estimator: BatteryEstimatorConfig,
    #[serde(default)]
    pub sources: Vec<BatterySourceConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataTabChart {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionsTabLayout {
    pub actions: Vec<ActionSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionSpec {
    pub label: String,
    pub cmd: String,
    pub border: String,
    pub bg: String,
    pub fg: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateTabLayout {
    pub states: Vec<StateLayout>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateLayout {
    pub states: Vec<FlightState>,
    pub sections: Vec<StateSection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSection {
    pub title: Option<String>,
    pub widgets: Vec<StateWidget>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryItem {
    pub label: String,
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StateWidgetKind {
    BoardStatus,
    Summary,
    Chart,
    ValveState,
    Map,
    Actions,
}

impl LayoutConfig {
    pub fn validate(&self) -> Result<(), String> {
        let mut main_tab_ids = HashSet::new();
        for tab in &self.main_tabs {
            let trimmed = tab.trim();
            if trimmed.is_empty() {
                return Err("layout contains an empty main tab id".to_string());
            }
            let known = matches!(
                trimmed,
                "state"
                    | "connection-status"
                    | "map"
                    | "actions"
                    | "calibration"
                    | "notifications"
                    | "warnings"
                    | "errors"
                    | "data"
                    | "network-topology"
            );
            if !known {
                return Err(format!("layout contains unknown main tab id '{trimmed}'"));
            }
            if !main_tab_ids.insert(trimmed.to_string()) {
                return Err(format!("layout contains duplicate main tab id '{trimmed}'"));
            }
        }

        let mut tab_ids = HashSet::new();
        for tab in &self.data_tab.tabs {
            if tab.id.trim().is_empty() {
                return Err("layout contains a data tab with an empty id".to_string());
            }
            if !tab_ids.insert(tab.id.clone()) {
                return Err(format!(
                    "layout contains duplicate data tab id '{}'",
                    tab.id
                ));
            }
            if tab.label.trim().is_empty() {
                return Err(format!("data tab '{}' has an empty label", tab.id));
            }
            if let Some(channel_labels) = &tab.channel_boolean_labels
                && channel_labels.len() > tab.channels.len()
            {
                return Err(format!(
                    "data tab '{}' has more channel boolean labels than channels",
                    tab.id
                ));
            }
        }

        for (state_idx, state) in self.state_tab.states.iter().enumerate() {
            for (section_idx, section) in state.sections.iter().enumerate() {
                for (widget_idx, widget) in section.widgets.iter().enumerate() {
                    if matches!(widget.kind, StateWidgetKind::Summary)
                        && widget.items.as_ref().is_none_or(Vec::is_empty)
                    {
                        return Err(format!(
                            "state layout entry {state_idx}, section {section_idx}, widget {widget_idx} is a summary with no items"
                        ));
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::LayoutConfig;

    #[test]
    fn parses_layout_endpoint_payload() {
        let payload = r##"{
            "version": 1,
            "connection_tab": {
                "sections": [
                    { "kind": "board_status", "title": "Board Status" }
                ]
            },
            "actions_tab": {
                "actions": [
                    {
                        "label": "Launch",
                        "cmd": "Launch",
                        "border": "#22c55e",
                        "bg": "#022c22",
                        "fg": "#bbf7d0"
                    }
                ]
            },
            "data_tab": {
                "tabs": [
                    {
                        "id": "GYRO_DATA",
                        "label": "GYRO_DATA",
                        "channels": ["Roll", "Pitch", "Yaw"],
                        "chart": { "enabled": true }
                    }
                ]
            },
            "state_tab": {
                "states": [
                    {
                        "states": ["Startup"],
                        "sections": [
                            {
                                "title": "Connected Devices",
                                "widgets": [
                                    { "kind": "board_status" }
                                ]
                            }
                        ]
                    }
                ]
            }
        }"##;

        let layout: LayoutConfig = serde_json::from_str(payload).expect("valid layout payload");

        assert_eq!(layout.version, 1);
        assert_eq!(layout.connection_tab.sections.len(), 1);
        assert_eq!(layout.actions_tab.actions.len(), 1);
        assert_eq!(layout.data_tab.tabs[0].channels.len(), 3);
        assert_eq!(layout.state_tab.states.len(), 1);
    }
}
