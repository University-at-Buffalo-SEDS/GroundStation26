use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutConfig {
    pub version: u32,
    #[serde(default)]
    pub branding: BrandingConfig,
    #[serde(default)]
    pub theme: ThemeConfig,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BrandingConfig {
    pub app_name: Option<String>,
    pub dashboard_title: Option<String>,
    #[serde(default)]
    pub tab_labels: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default = "default_app_background")]
    pub app_background: String,
    #[serde(default = "default_panel_background")]
    pub panel_background: String,
    #[serde(default = "default_panel_background_alt")]
    pub panel_background_alt: String,
    #[serde(default = "default_overlay_background")]
    pub overlay_background: String,
    #[serde(default = "default_border")]
    pub border: String,
    #[serde(default = "default_border_strong")]
    pub border_strong: String,
    #[serde(default = "default_border_soft")]
    pub border_soft: String,
    #[serde(default = "default_text_primary")]
    pub text_primary: String,
    #[serde(default = "default_text_secondary")]
    pub text_secondary: String,
    #[serde(default = "default_text_muted")]
    pub text_muted: String,
    #[serde(default = "default_text_soft")]
    pub text_soft: String,
    #[serde(default = "default_button_background")]
    pub button_background: String,
    #[serde(default = "default_button_border")]
    pub button_border: String,
    #[serde(default = "default_button_text")]
    pub button_text: String,
    #[serde(default = "default_tab_shell_background")]
    pub tab_shell_background: String,
    #[serde(default = "default_tab_shell_border")]
    pub tab_shell_border: String,
    #[serde(default = "default_info_accent")]
    pub info_accent: String,
    #[serde(default = "default_info_background")]
    pub info_background: String,
    #[serde(default = "default_info_text")]
    pub info_text: String,
    #[serde(default = "default_success_text")]
    pub success_text: String,
    #[serde(default = "default_warning_background")]
    pub warning_background: String,
    #[serde(default = "default_warning_border")]
    pub warning_border: String,
    #[serde(default = "default_warning_text")]
    pub warning_text: String,
    #[serde(default = "default_error_background")]
    pub error_background: String,
    #[serde(default = "default_error_border")]
    pub error_border: String,
    #[serde(default = "default_error_text")]
    pub error_text: String,
    #[serde(default = "default_notification_background")]
    pub notification_background: String,
    #[serde(default = "default_notification_border")]
    pub notification_border: String,
    #[serde(default = "default_notification_text")]
    pub notification_text: String,
    #[serde(default = "default_main_tab_accents")]
    pub main_tab_accents: HashMap<String, String>,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            app_background: default_app_background(),
            panel_background: default_panel_background(),
            panel_background_alt: default_panel_background_alt(),
            overlay_background: default_overlay_background(),
            border: default_border(),
            border_strong: default_border_strong(),
            border_soft: default_border_soft(),
            text_primary: default_text_primary(),
            text_secondary: default_text_secondary(),
            text_muted: default_text_muted(),
            text_soft: default_text_soft(),
            button_background: default_button_background(),
            button_border: default_button_border(),
            button_text: default_button_text(),
            tab_shell_background: default_tab_shell_background(),
            tab_shell_border: default_tab_shell_border(),
            info_accent: default_info_accent(),
            info_background: default_info_background(),
            info_text: default_info_text(),
            success_text: default_success_text(),
            warning_background: default_warning_background(),
            warning_border: default_warning_border(),
            warning_text: default_warning_text(),
            error_background: default_error_background(),
            error_border: default_error_border(),
            error_text: default_error_text(),
            notification_background: default_notification_background(),
            notification_border: default_notification_border(),
            notification_text: default_notification_text(),
            main_tab_accents: default_main_tab_accents(),
        }
    }
}

fn default_app_background() -> String {
    "#020617".to_string()
}
fn default_panel_background() -> String {
    "#0b1220".to_string()
}
fn default_panel_background_alt() -> String {
    "#0f172a".to_string()
}
fn default_overlay_background() -> String {
    "#020617ee".to_string()
}
fn default_border() -> String {
    "#334155".to_string()
}
fn default_border_strong() -> String {
    "#4b5563".to_string()
}
fn default_border_soft() -> String {
    "#1f2937".to_string()
}
fn default_text_primary() -> String {
    "#e5e7eb".to_string()
}
fn default_text_secondary() -> String {
    "#cbd5e1".to_string()
}
fn default_text_muted() -> String {
    "#94a3b8".to_string()
}
fn default_text_soft() -> String {
    "#9ca3af".to_string()
}
fn default_button_background() -> String {
    "#111827".to_string()
}
fn default_button_border() -> String {
    "#334155".to_string()
}
fn default_button_text() -> String {
    "#e5e7eb".to_string()
}
fn default_tab_shell_background() -> String {
    "#020617ee".to_string()
}
fn default_tab_shell_border() -> String {
    "#4b5563".to_string()
}
fn default_info_accent() -> String {
    "#60a5fa".to_string()
}
fn default_info_background() -> String {
    "#0b1a33".to_string()
}
fn default_info_text() -> String {
    "#bfdbfe".to_string()
}
fn default_success_text() -> String {
    "#22c55e".to_string()
}
fn default_warning_background() -> String {
    "#451a03".to_string()
}
fn default_warning_border() -> String {
    "#f59e0b".to_string()
}
fn default_warning_text() -> String {
    "#fde68a".to_string()
}
fn default_error_background() -> String {
    "#450a0a".to_string()
}
fn default_error_border() -> String {
    "#ef4444".to_string()
}
fn default_error_text() -> String {
    "#fecaca".to_string()
}
fn default_notification_background() -> String {
    "#0b1f4d".to_string()
}
fn default_notification_border() -> String {
    "#2563eb".to_string()
}
fn default_notification_text() -> String {
    "#bfdbfe".to_string()
}
fn default_main_tab_accents() -> HashMap<String, String> {
    HashMap::from([
        ("state".to_string(), "#38bdf8".to_string()),
        ("connection-status".to_string(), "#06b6d4".to_string()),
        ("detailed".to_string(), "#0ea5e9".to_string()),
        ("map".to_string(), "#22c55e".to_string()),
        ("actions".to_string(), "#a78bfa".to_string()),
        ("calibration".to_string(), "#14b8a6".to_string()),
        ("notifications".to_string(), "#3b82f6".to_string()),
        ("warnings".to_string(), "#facc15".to_string()),
        ("errors".to_string(), "#ef4444".to_string()),
        ("data".to_string(), "#f97316".to_string()),
        ("network-topology".to_string(), "#8b5cf6".to_string()),
    ])
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
    #[serde(default)]
    pub expected_boards: Vec<String>,
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
    pub chart_groups: Option<Vec<DataChartGroup>>,
    pub subtabs: Option<Vec<DataSubtabSpec>>,
    pub boolean_labels: Option<BooleanLabels>,
    pub channel_boolean_labels: Option<Vec<BooleanLabels>>,
    pub channel_formatters: Option<Vec<ValueFormatter>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataSubtabSpec {
    pub id: String,
    pub label: String,
    pub data_type: Option<String>,
    pub sender_id: Option<String>,
    pub channels: Option<Vec<String>>,
    pub chart: Option<DataTabChart>,
    pub chart_groups: Option<Vec<DataChartGroup>>,
    pub summary_items: Option<Vec<DataSummaryItem>>,
    pub boolean_labels: Option<BooleanLabels>,
    pub channel_boolean_labels: Option<Vec<BooleanLabels>>,
    pub channel_formatters: Option<Vec<ValueFormatter>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataChartGroup {
    pub title: Option<String>,
    pub data_type: Option<String>,
    pub sender_id: Option<String>,
    pub labels: Option<Vec<String>>,
    pub channels: Vec<usize>,
    pub scale_mode: Option<DataChartScaleMode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataChartScaleMode {
    Shared,
    PerSeries,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataSummaryItem {
    pub label: String,
    pub data_type: String,
    pub index: usize,
    pub sender_id: Option<String>,
    pub formatter: Option<ValueFormatter>,
    pub boolean_labels: Option<BooleanLabels>,
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
    #[serde(default)]
    pub disable_actions_by_default: bool,
    #[serde(default = "default_show_flight_setup")]
    pub show_flight_setup: bool,
    #[serde(default = "default_show_fill_targets")]
    pub show_fill_targets: bool,
    #[serde(default = "default_fill_targets_require_actions_enabled")]
    pub fill_targets_require_actions_enabled: bool,
    pub actions: Vec<ActionSpec>,
}

fn default_show_flight_setup() -> bool {
    true
}

fn default_show_fill_targets() -> bool {
    true
}

fn default_fill_targets_require_actions_enabled() -> bool {
    true
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
    pub states: Vec<String>,
    pub sections: Vec<StateSection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSection {
    pub title: Option<String>,
    pub widgets: Vec<StateWidget>,
    pub style: Option<StateSectionStyle>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateWidget {
    pub kind: StateWidgetKind,
    pub data_type: Option<String>,
    pub chart_series: Option<Vec<ChartSeriesSpec>>,
    pub items: Option<Vec<SummaryItem>>,
    pub chart_title: Option<String>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub actions: Option<Vec<String>>,
    pub valves: Option<Vec<SummaryItem>>,
    pub valve_colors: Option<ValveColorSet>,
    pub boolean_labels: Option<BooleanLabels>,
    pub valve_labels: Option<Vec<BooleanLabels>>,
    pub summary_style: Option<SummaryCardStyle>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartSeriesSpec {
    pub data_type: String,
    pub index: usize,
    pub label: Option<String>,
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
    pub formatter: Option<ValueFormatter>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSectionStyle {
    pub background: Option<String>,
    pub border: Option<String>,
    pub title_color: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryCardStyle {
    pub background: Option<String>,
    pub border: Option<String>,
    pub label_color: Option<String>,
    pub value_color: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueFormatter {
    pub kind: Option<ValueFormatKind>,
    pub precision: Option<usize>,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ValueFormatKind {
    #[default]
    Number,
    Integer,
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
                    | "detailed"
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
            if let Some(channel_formatters) = &tab.channel_formatters
                && channel_formatters.len() > tab.channels.len()
            {
                return Err(format!(
                    "data tab '{}' has more channel formatters than channels",
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

        for board_id in &self.network_tab.expected_boards {
            let trimmed = board_id.trim();
            if trimmed.is_empty() {
                return Err("layout contains an empty expected board id".to_string());
            }
            if !matches!(
                trimmed,
                "FC" | "RF" | "PB" | "VB" | "GW" | "AB" | "DAQ" | "GS"
            ) {
                return Err(format!(
                    "layout contains unknown expected board id '{trimmed}'"
                ));
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
