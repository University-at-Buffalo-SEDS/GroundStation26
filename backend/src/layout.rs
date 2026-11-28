use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const DEFAULT_LAYOUT_PATH: &str = "layout/layout.json";
const DEFAULT_HITL_LAYOUT_PATH: &str = "layout/layout_hitl.json";
const DEFAULT_TEST_FIRE_LAYOUT_PATH: &str = "layout/layout_test_fire.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutConfig {
    #[serde(default)]
    pub flight_phase_version: u8,
    #[serde(default)]
    pub test_fire_mode: bool,
    /// Integrated ground test (served layout only; omitted when false).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub igt_mode: bool,
    /// Live-diagram view per station mode (`normal`, `hitl`, `test_fire`,
    /// `testing`, `integrated_ground_test`): a topology id from
    /// `backend/layout/diagrams/`. A mode not listed gets
    /// [`crate::diagrams::default_view`]. docs/live-diagrams.md.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub live_diagrams: std::collections::BTreeMap<String, String>,
    /// The view for the mode this layout is served in (resolved from
    /// `live_diagrams`); the dashboard's Live diagrams tab opens it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_diagram: Option<String>,
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
    /// Which telemetry rows are GPS fixes and where latitude, longitude and
    /// altitude sit in them (map tracking, vehicle view, replay track).
    /// Omitted: the UI's built-in GPS types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps: Option<GpsLayout>,
    /// Telemetry bindings of the watch companion's items, by item id
    /// (`docs/backend/watch-direct.md`); an item not listed keeps its built-in
    /// channels. The watch status (`/api/watch/status`) reads the same list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watch_items: Vec<WatchItemBinding>,
    /// The few readings the phone app shows on its lock-screen live status
    /// (iOS Live Activity, Android ongoing notification), in order, at most
    /// [`MAX_LIVE_SUMMARY_ITEMS`]. The station chooses them, with their labels
    /// and units, so the app names no vehicle channel itself. Omitted: the
    /// live status shows state, T-clock and abort only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub live_summary: Vec<LiveSummaryItem>,
}

/// Readings a lock-screen live status has room for.
pub const MAX_LIVE_SUMMARY_ITEMS: usize = 4;

/// One reading on the phone's lock-screen live status: the newest finite
/// value of `channels`, shown as `label` `value` `unit` with `decimals`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveSummaryItem {
    pub label: String,
    /// Display unit of the channel values as stored (no conversion).
    #[serde(default)]
    pub unit: String,
    #[serde(default = "default_live_summary_decimals")]
    pub decimals: u8,
    pub channels: Vec<WatchChannel>,
}

fn default_live_summary_decimals() -> u8 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpsLayout {
    /// Data types carrying a fix, in preference order.
    pub data_types: Vec<String>,
    #[serde(default)]
    pub lat_index: usize,
    #[serde(default = "default_gps_lon_index")]
    pub lon_index: usize,
    #[serde(default = "default_gps_alt_index")]
    pub alt_index: usize,
}

fn default_gps_lon_index() -> usize {
    1
}

fn default_gps_alt_index() -> usize {
    2
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchItemBinding {
    pub id: String,
    /// The newest finite value of these channels is shown.
    pub channels: Vec<WatchChannel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchChannel {
    pub data_type: String,
    /// Only rows from this board; omitted: any sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
    #[serde(default)]
    pub index: usize,
}

fn default_main_tabs() -> Vec<String> {
    vec![
        "state".to_string(),
        "connection-status".to_string(),
        "map".to_string(),
        "mission".to_string(),
        "vehicle".to_string(),
        "actions".to_string(),
        "firmware-update".to_string(),
        "calibration".to_string(),
        "messages".to_string(),
        "notifications".to_string(),
        "warnings".to_string(),
        "errors".to_string(),
        "data".to_string(),
        "network-topology".to_string(),
        "detailed".to_string(),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BrandingConfig {
    pub app_name: Option<String>,
    pub dashboard_title: Option<String>,
    #[serde(default)]
    pub tab_labels: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        ("firmware-update".to_string(), "#22d3ee".to_string()),
        ("calibration".to_string(), "#14b8a6".to_string()),
        ("messages".to_string(), "#64748b".to_string()),
        ("notifications".to_string(), "#3b82f6".to_string()),
        ("warnings".to_string(), "#facc15".to_string()),
        ("errors".to_string(), "#ef4444".to_string()),
        ("data".to_string(), "#f97316".to_string()),
        ("network-topology".to_string(), "#8b5cf6".to_string()),
    ])
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
    #[serde(default)]
    pub expected_boards: Vec<String>,
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
    #[serde(default)]
    pub sender_split_data_types: Vec<String>,
    #[serde(default)]
    pub default_display_filter: Option<DataDisplayFilter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub show_min_max: Option<bool>,
    #[serde(default)]
    pub display_filter: Option<DataDisplayFilter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub show_min_max: Option<bool>,
    #[serde(default)]
    pub display_filter: Option<DataDisplayFilter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataChartGroup {
    pub title: Option<String>,
    pub data_type: Option<String>,
    pub sender_id: Option<String>,
    pub labels: Option<Vec<String>>,
    pub channels: Vec<usize>,
    pub chart_series: Option<Vec<ChartSeriesSpec>>,
    pub scale_mode: Option<DataChartScaleMode>,
    #[serde(default)]
    pub display_filter: Option<DataDisplayFilter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataChartScaleMode {
    Shared,
    PerSeries,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataSummaryItem {
    pub label: String,
    pub data_type: String,
    pub index: usize,
    pub sender_id: Option<String>,
    pub formatter: Option<ValueFormatter>,
    pub boolean_labels: Option<BooleanLabels>,
    pub show_min_max: Option<bool>,
    #[serde(default)]
    pub display_filter: Option<DataDisplayFilter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataDisplayFilter {
    #[serde(default = "default_data_display_filter_enabled")]
    pub enabled: bool,
    pub kind: DataDisplayFilterKind,
    #[serde(default)]
    pub window_ms: Option<u64>,
    #[serde(default)]
    pub cutoff_hz: Option<f32>,
    #[serde(default)]
    pub alpha: Option<f32>,
    #[serde(default)]
    pub deadband: Option<f32>,
    #[serde(default)]
    pub max_rate_per_sec: Option<f32>,
}

fn default_data_display_filter_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataDisplayFilterKind {
    Raw,
    TimeAverage,
    LowPass,
    HighPass,
    ExponentialAverage,
    Median,
    MinMax,
    Deadband,
    RateLimit,
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
    0.000001
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
    #[serde(default = "default_show_flight_setup")]
    pub show_flight_setup: bool,
    #[serde(default = "default_show_fill_targets")]
    pub show_fill_targets: bool,
    #[serde(default = "default_fill_targets_require_actions_enabled")]
    pub fill_targets_require_actions_enabled: bool,
    pub actions: Vec<ActionSpec>,
    /// Display names of controls that are not buttons in `actions` (the
    /// HITL interlock toggles), by command name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controls: Vec<ControlLabel>,
    /// The fluids of `/api/fill_targets` the fill-target editors show, in
    /// order, with their names. Omitted: the UI's built-in list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fill_target_fluids: Vec<FillTargetFluid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FillTargetFluid {
    /// The fluid's key in the fill-targets document (`nitrogen`, `nitrous`).
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlLabel {
    pub cmd: String,
    pub label: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSpec {
    #[serde(default)]
    pub group: String,
    pub label: String,
    pub cmd: String,
    pub border: String,
    pub bg: String,
    pub fg: String,
    #[serde(default)]
    pub illuminated: bool,
    #[serde(default)]
    pub spacer_before: bool,
    #[serde(default)]
    pub spacer_after: bool,
    #[serde(default)]
    pub new_row_before: bool,
    #[serde(default)]
    pub new_row_after: bool,
    #[serde(default)]
    pub spacer_row_before: bool,
    #[serde(default)]
    pub spacer_row_after: bool,
    /// Actions-tab section, assigned at load from
    /// [`crate::sequences::control_section`]; a layout file cannot override it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<crate::sequences::ControlSection>,
    /// The confirmation the client shows before sending this command,
    /// assigned at load (for example [`crate::fc_kalman::confirmation`]); a
    /// layout file cannot override it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<ActionConfirm>,
}

/// A command's confirmation dialog: title, message and confirm button.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionConfirm {
    pub title: String,
    pub message: String,
    pub button: String,
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
    pub style: Option<StateSectionStyle>,
    #[serde(default)]
    pub value_layout: StateSectionValueLayout,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StateSectionValueLayout {
    #[default]
    Auto,
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateWidget {
    pub kind: StateWidgetKind,
    pub data_type: Option<String>,
    pub chart_series: Option<Vec<ChartSeriesSpec>>,
    pub items: Option<Vec<SummaryItem>>,
    pub chart_title: Option<String>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    #[serde(default)]
    pub full_width: bool,
    #[serde(default)]
    pub width_fraction: Option<f64>,
    pub actions: Option<Vec<String>>,
    pub valves: Option<Vec<SummaryItem>>,
    pub valve_colors: Option<ValveColorSet>,
    pub boolean_labels: Option<BooleanLabels>,
    pub valve_labels: Option<Vec<BooleanLabels>>,
    pub summary_style: Option<SummaryCardStyle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChartSeriesSpec {
    pub data_type: String,
    pub index: usize,
    pub sender_id: Option<String>,
    pub label: Option<String>,
    #[serde(default)]
    pub display_filter: Option<DataDisplayFilter>,
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
    pub formatter: Option<ValueFormatter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSectionStyle {
    pub background: Option<String>,
    pub border: Option<String>,
    pub title_color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryCardStyle {
    pub background: Option<String>,
    pub border: Option<String>,
    pub label_color: Option<String>,
    pub value_color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueFormatter {
    pub kind: Option<ValueFormatKind>,
    pub precision: Option<usize>,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ValueFormatKind {
    #[default]
    Number,
    Integer,
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

    default_layout_path(presentation_mode())
}

// A recovery station displays the primary's current/last-known mode. This
// never changes local execution capabilities or the RRLS read-only command gate.
fn presentation_mode() -> crate::station_mode::Mode {
    if crate::rrls::is_rrls() {
        crate::station_mode::snapshot()
            .effective_mode
            .unwrap_or(crate::station_mode::Mode::Normal)
    } else {
        crate::station_mode::local_mode()
    }
}
pub(crate) fn default_layout_path(mode: crate::station_mode::Mode) -> PathBuf {
    let relative = match mode {
        crate::station_mode::Mode::Hitl => DEFAULT_HITL_LAYOUT_PATH,
        crate::station_mode::Mode::TestFire => DEFAULT_TEST_FIRE_LAYOUT_PATH,
        _ => DEFAULT_LAYOUT_PATH,
    };
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

#[cfg(test)]
mod observer_mode_layout_tests {
    use super::*;
    #[test]
    fn primary_mode_selects_actions_independently_of_observer_build() {
        use crate::station_mode::Mode;
        for (mode, file) in [
            (Mode::Hitl, DEFAULT_HITL_LAYOUT_PATH),
            (Mode::Normal, DEFAULT_LAYOUT_PATH),
            (Mode::TestFire, DEFAULT_TEST_FIRE_LAYOUT_PATH),
        ] {
            let path = default_layout_path(mode);
            assert!(path.ends_with(file));
            let layout: LayoutConfig =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert!(!layout.actions_tab.actions.is_empty());
        }
    }
}

pub fn load_layout() -> Result<LayoutConfig, String> {
    load_layout_file(&layout_path(), presentation_mode())
}

/// The layout a station in `mode` presents, for a saved-data replay of a
/// recording made in that mode. The station's own (possibly custom) layout is
/// used when `mode` is the mode it presents now; otherwise the shipped
/// default for `mode`. Never changes the station mode or its live layout.
pub(crate) fn load_layout_for_mode(mode: crate::station_mode::Mode) -> Result<LayoutConfig, String> {
    if mode == presentation_mode() {
        return load_layout();
    }
    load_layout_file(&default_layout_path(mode), mode)
}

fn load_layout_file(path: &std::path::Path, mode: crate::station_mode::Mode) -> Result<LayoutConfig, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read layout file {path:?}: {e}"))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("Invalid layout JSON: {e}"))?;
    canonicalize_legacy_type_names(&mut value);
    let mut layout: LayoutConfig =
        serde_json::from_value(value).map_err(|e| format!("Invalid layout JSON: {e}"))?;
    normalize_flight_phases(&mut layout)?;
    normalize_live_summary(&mut layout.live_summary);
    apply_runtime_layout_overrides(&mut layout, mode);
    assign_control_sections(&mut layout);
    Ok(layout)
}

/// Keeps the live-status readings a phone can show: a label and at least one
/// named channel each, at most [`MAX_LIVE_SUMMARY_ITEMS`], up to 6 decimals.
fn normalize_live_summary(items: &mut Vec<LiveSummaryItem>) {
    for item in items.iter_mut() {
        item.label = item.label.trim().to_string();
        item.unit = item.unit.trim().to_string();
        item.decimals = item.decimals.min(6);
        item.channels.retain(|c| !c.data_type.trim().is_empty());
    }
    items.retain(|item| !item.label.is_empty() && !item.channels.is_empty());
    items.truncate(MAX_LIVE_SUMMARY_ITEMS);
}

/// Renames legacy telemetry type names (and the matching human label) in a
/// layout, diagram or saved view: a custom layout, SD import manifest or
/// portable replay saved before the ox tank PT rename names
/// FUEL_TANK_PRESSURE, which the station now publishes as OX_TANK_PRESSURE.
/// Only whole strings are replaced. Returns whether anything changed.
pub(crate) fn canonicalize_legacy_type_names(value: &mut serde_json::Value) -> bool {
    fn renamed(text: &str) -> Option<&'static str> {
        match text {
            crate::telemetry_schema::LEGACY_FUEL_TANK_PRESSURE => Some(crate::telemetry_schema::OX_TANK_PRESSURE),
            "Fuel Tank Pressure" => Some("Ox Tank Pressure"),
            "Fuel tank pressure" => Some("Ox tank pressure"),
            "FUEL TANK PRESSURE" => Some("OX TANK PRESSURE"),
            _ => None,
        }
    }
    fn walk(v: &mut serde_json::Value, changed: &mut bool) {
        match v {
            serde_json::Value::String(text) => {
                if let Some(new) = renamed(text) {
                    *text = new.to_string();
                    *changed = true;
                }
            }
            serde_json::Value::Object(map) => {
                // Keys too: maps keyed by data type (e.g. per-type settings).
                let legacy: Vec<String> = map.keys().filter(|k| renamed(k).is_some()).cloned().collect();
                for key in legacy {
                    let new = renamed(&key).unwrap().to_string();
                    if let Some(item) = map.remove(&key) {
                        map.entry(new).or_insert(item);
                        *changed = true;
                    }
                }
                map.values_mut().for_each(|v| walk(v, changed));
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(|v| walk(v, changed)),
            _ => {}
        }
    }
    let mut changed = false;
    walk(value, &mut changed);
    changed
}

/// BAROMETER_DATA is (altitude, pressure, temperature) in every recording:
/// the FC always sent that order, and only the station's labels were wrong
/// before the fix. A layout saved before it (inside an SD import manifest or
/// a portable replay) labels the channels (pressure, temperature, altitude);
/// this puts every BAROMETER_DATA label back on the value it names. Returns
/// whether anything changed.
pub(crate) fn relabel_legacy_barometer(layout: &mut serde_json::Value) -> bool {
    fn index_for(label: &str) -> Option<u64> {
        let l = label.to_ascii_lowercase();
        if l.contains("alt") || l == "barometer" {
            Some(0)
        } else if l.contains("press") || l.contains("prs") {
            Some(1)
        } else if l.contains("temp") || l.contains("tmp") {
            Some(2)
        } else {
            None
        }
    }
    fn walk(v: &mut serde_json::Value, changed: &mut bool) {
        match v {
            serde_json::Value::Object(map) => {
                let baro = |map: &serde_json::Map<String, serde_json::Value>, key: &str| {
                    map.get(key).and_then(|v| v.as_str()) == Some("BAROMETER_DATA")
                };
                if baro(map, "id")
                    && let Some(channels) = map.get_mut("channels").and_then(|c| c.as_array_mut())
                    && channels.len() == 3
                {
                    let labels: Vec<String> = channels.iter().filter_map(|c| c.as_str().map(str::to_owned)).collect();
                    let mut ordered: Vec<Option<String>> = vec![None, None, None];
                    for label in &labels {
                        if let Some(i) = index_for(label) {
                            ordered[i as usize] = Some(label.clone());
                        }
                    }
                    if ordered.iter().all(Option::is_some) && ordered.iter().flatten().ne(labels.iter()) {
                        *channels = ordered.into_iter().flatten().map(serde_json::Value::from).collect();
                        *changed = true;
                    }
                }
                if baro(map, "data_type") {
                    if let Some(items) = map.get_mut("items").and_then(|c| c.as_array_mut()) {
                        for item in items {
                            let want = item["label"].as_str().and_then(index_for);
                            if let Some(want) = want
                                && item["index"].as_u64() != Some(want)
                            {
                                item["index"] = want.into();
                                *changed = true;
                            }
                        }
                    }
                    let want = map.get("label").and_then(|l| l.as_str()).and_then(index_for);
                    if let Some(want) = want
                        && map.get("index").and_then(|i| i.as_u64()).is_some_and(|i| i != want)
                    {
                        map.insert("index".into(), want.into());
                        *changed = true;
                    }
                }
                map.values_mut().for_each(|v| walk(v, changed));
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(|v| walk(v, changed)),
            _ => {}
        }
    }
    let mut changed = false;
    walk(layout, &mut changed);
    changed
}

/// Every action carries the section its command belongs to, from the one
/// backend table beside the per-mode command catalogs.
fn assign_control_sections(layout: &mut LayoutConfig) {
    for action in &mut layout.actions_tab.actions {
        action.section = crate::sequences::control_section(&action.cmd);
        action.confirm = crate::fc_kalman::confirmation(&action.cmd);
    }
}

fn normalize_flight_phases(layout: &mut LayoutConfig) -> Result<(), String> {
    let version = layout.flight_phase_version;
    if version > crate::types::FLIGHT_PHASE_VERSION {
        return Err("Unsupported flight phase version".into());
    }
    for group in &mut layout.state_tab.states {
        for phase in &mut group.states {
            *phase = crate::types::canonical_phase_key(phase, version).to_owned();
        }
    }
    layout.flight_phase_version = crate::types::FLIGHT_PHASE_VERSION;
    Ok(())
}

fn apply_runtime_layout_overrides(layout: &mut LayoutConfig, mode: crate::station_mode::Mode) {
    layout.test_fire_mode = mode == crate::station_mode::Mode::TestFire;
    layout.igt_mode = mode == crate::station_mode::Mode::IntegratedGroundTest;
    layout.live_diagram = Some(live_diagram_for(layout, mode));
    if crate::rrls::is_rrls() {
        layout.branding.app_name = Some("RRLS".into());
        layout.branding.dashboard_title = Some("RRLS".into());
    }
    ensure_gse_sequence_actions(layout);
    for (cmd, label) in [
        ("ToggleAvBayUnderglow", "AV Bay Underglow"),
        ("ToggleFlightBuzzer", "Flight Buzzer"),
        ("ResetTClock", "Reset T-clock"),
        ("ResetFlightState", "Reset flight state to Idle"),
        ("RecoverStation", "Recover station"),
    ] {
        if cmd == "ResetFlightState" && mode != crate::station_mode::Mode::Hitl {
            continue;
        }
        if !layout
            .actions_tab
            .actions
            .iter()
            .any(|action| action.cmd == cmd)
        {
            layout.actions_tab.actions.push(
                serde_json::from_value(serde_json::json!({
                    "cmd": cmd, "label": label, "group": "Operator recovery",
                    "border": "#f59e0b", "bg": "#451a03", "fg": "#fef3c7"
                }))
                .expect("valid operator recovery action"),
            );
        }
    }
    // HITL lists the Ground test controls too; they are refused (and the
    // client hides them) unless the HITL replay is on (IgtStatus.igt_active).
    if matches!(mode, crate::station_mode::Mode::IntegratedGroundTest | crate::station_mode::Mode::Hitl) {
        ensure_igt_actions(layout);
    }
    // The FC sensor, Kalman and SD controls wherever the FC is present
    // (every mode but test fire); Kalman: start in HITL only.
    if mode != crate::station_mode::Mode::TestFire {
        ensure_fc_operator_actions(layout, mode == crate::station_mode::Mode::Hitl);
        ensure_fc_sd_retry_action(layout);
    }
    // Radio test mode (radio_reject_test): an HTTP action on the primary,
    // listed under Testing in every mode.
    if !crate::rrls::is_rrls()
        && !layout
            .actions_tab
            .actions
            .iter()
            .any(|action| action.cmd == crate::radio_reject_test::COMMAND)
    {
        layout.actions_tab.actions.push(
            serde_json::from_value(serde_json::json!({
                "cmd": crate::radio_reject_test::COMMAND, "label": "Radio test: reject AV-bay link",
                "group": "Radio test", "border": "#ef4444", "bg": "#450a0a", "fg": "#fee2e2"
            }))
            .expect("valid radio test action"),
        );
    }
    if mode != crate::station_mode::Mode::Testing {
        return;
    }
    if layout
        .actions_tab
        .actions
        .iter()
        .any(|action| action.cmd == "ResetSim")
    {
        return;
    }
    layout.actions_tab.actions.insert(
        0,
        ActionSpec {
            group: "Recording".into(),
            label: "Reset Sim".to_string(),
            cmd: "ResetSim".to_string(),
            border: "#38bdf8".to_string(),
            bg: "#082f49".to_string(),
            fg: "#e0f2fe".to_string(),
            illuminated: false,
            spacer_before: false,
            spacer_after: false,
            new_row_before: false,
            new_row_after: true,
            spacer_row_before: false,
            spacer_row_after: true,
            section: None,
            confirm: None,
        },
    );
}

/// The live-diagram view a layout names for `mode`, else the built-in one.
pub(crate) fn live_diagram_for(layout: &LayoutConfig, mode: crate::station_mode::Mode) -> String {
    let key = serde_json::to_value(mode).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    layout
        .live_diagrams
        .get(&key)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map_or_else(|| crate::diagrams::default_view(mode).to_string(), str::to_owned)
}

/// The integrated ground test uses the flight layout (AV bay, FC, recovery
/// and fill content) with the Ground test controls added first.
fn ensure_igt_actions(layout: &mut LayoutConfig) {
    for (index, (cmd, label)) in IGT_ACTIONS.into_iter().enumerate() {
        if layout.actions_tab.actions.iter().any(|action| action.cmd == cmd) {
            continue;
        }
        let (border, bg, fg) = if cmd == "IgtLivePyro" {
            ("#ef4444", "#450a0a", "#fee2e2")
        } else {
            ("#f97316", "#431407", "#ffedd5")
        };
        layout.actions_tab.actions.insert(
            index,
            serde_json::from_value(serde_json::json!({
                "cmd": cmd, "label": label, "group": "Ground test",
                "border": border, "bg": bg, "fg": fg, "illuminated": true
            }))
            .expect("valid ground test action"),
        );
    }
}

/// The FC operator controls every mode with an FC offers (labels as the
/// HITL layout names them), appended after the last Flight computer action.
const FC_OPERATOR_ACTIONS: [(&str, &str); 6] = [
    ("ReinitSensors", "Reinit Sensors"),
    ("ReinitBarometer", "Reinit Barometer"),
    ("ReinitIMU", "Reinit IMU"),
    ("DisableIMU", "Disable IMU"),
    (crate::fc_kalman::STOP, "Kalman: stop"),
    (crate::fc_kalman::RESET, "Kalman: reset"),
];

fn ensure_fc_operator_actions(layout: &mut LayoutConfig, hitl: bool) {
    let start = [(crate::fc_kalman::START, "Kalman: start (HITL)")];
    let wanted = FC_OPERATOR_ACTIONS.iter().chain(if hitl { start.iter() } else { [].iter() });
    for (cmd, label) in wanted {
        let actions = &mut layout.actions_tab.actions;
        if actions.iter().any(|action| action.cmd == *cmd) {
            continue;
        }
        let last_fc = actions.iter().rposition(|action| {
            crate::sequences::control_section(&action.cmd) == Some(crate::sequences::ControlSection::FlightComputer)
        });
        let (index, group) = match last_fc {
            Some(index) => (index + 1, actions[index].group.clone()),
            None => (actions.len(), "Flight controls".to_string()),
        };
        actions.insert(
            index,
            serde_json::from_value(serde_json::json!({
                "cmd": cmd, "label": label, "group": group,
                "border": "#38bdf8", "bg": "#0b1220", "fg": "#e0f2fe"
            }))
            .expect("valid FC operator action"),
        );
    }
}

/// `FcSdRetry` after the last Flight computer action, in its group.
fn ensure_fc_sd_retry_action(layout: &mut LayoutConfig) {
    let actions = &mut layout.actions_tab.actions;
    if actions.iter().any(|action| action.cmd == "FcSdRetry") {
        return;
    }
    let last_fc = actions.iter().rposition(|action| {
        crate::sequences::control_section(&action.cmd) == Some(crate::sequences::ControlSection::FlightComputer)
    });
    let (index, group) = match last_fc {
        Some(index) => (index + 1, actions[index].group.clone()),
        None => (actions.len(), "Flight controls".to_string()),
    };
    actions.insert(
        index,
        serde_json::from_value(serde_json::json!({
            "cmd": "FcSdRetry", "label": "Retry FC SD", "group": group,
            "border": "#38bdf8", "bg": "#082f49", "fg": "#e0f2fe"
        }))
        .expect("valid FC SD retry action"),
    );
}

const IGT_ACTIONS: [(&str, &str); 4] = [
    ("IgtEnter", "Enter ground test"),
    ("IgtLivePyro", "LIVE PYRO"),
    ("IgtDryRun", "Dry-run"),
    ("IgtExit", "Exit ground test"),
];

const GSE_SEQUENCE_ACTIONS: [(&str, &str); 6] = [
    ("StartFill", "Start fill"),
    ("PauseFill", "Pause fill"),
    ("CancelFill", "Cancel fill"),
    ("NitrogenTest", "Nitrogen test"),
    ("Turbocharge", "Turbocharge"),
    ("ValveSelfTest", "Valve self-test"),
];

fn is_ground_action_state(state: &str) -> bool {
    matches!(
        state,
        "Startup" | "Idle" | "PreFill" | "FillTest" | "NitrogenFill" | "NitrousFill" | "Armed"
    )
}

/// All clients (including embedded state widgets) resolve commands from this
/// catalog. Frontend-only injection into ActionsTab leaves those widgets empty.
fn ensure_gse_sequence_actions(layout: &mut LayoutConfig) {
    let mut sequence = Vec::new();
    for (cmd, label) in GSE_SEQUENCE_ACTIONS {
        let mut spec =
            if let Some(index) = layout.actions_tab.actions.iter().position(|a| a.cmd == cmd) {
                layout.actions_tab.actions.remove(index)
            } else {
                serde_json::from_value::<ActionSpec>(serde_json::json!({
                    "cmd": cmd, "label": label, "border": "#38bdf8",
                    "bg": "#082f49", "fg": "#e0f2fe", "illuminated": true
                }))
                .expect("valid built-in GSE action")
            };
        spec.group = "GSE sequence actions".into();
        sequence.push(spec);
    }
    sequence.append(&mut layout.actions_tab.actions);
    layout.actions_tab.actions = sequence;
    // Stock layouts share sections between filling and Aborted. Split those
    // selections so adding prelaunch commands does not alter abort/flight views.
    let mut ground_layouts = Vec::new();
    for state in &mut layout.state_tab.states {
        if state.states.iter().any(|s| is_ground_action_state(s))
            && !state.states.iter().all(|s| is_ground_action_state(s))
        {
            let mut ground = state.clone();
            ground.states.retain(|s| is_ground_action_state(s));
            state.states.retain(|s| !is_ground_action_state(s));
            ground_layouts.push(ground);
        }
    }
    layout.state_tab.states.extend(ground_layouts);
    for state in &mut layout.state_tab.states {
        // Do not introduce ground commands into flight/recovery views.
        if !state.states.iter().all(|s| {
            matches!(
                s.as_str(),
                "Startup"
                    | "Idle"
                    | "PreFill"
                    | "FillTest"
                    | "NitrogenFill"
                    | "NitrousFill"
                    | "Armed"
            )
        }) {
            continue;
        }
        for section in &mut state.sections {
            for widget in &mut section.widgets {
                let Some(actions) = widget.actions.as_mut() else {
                    continue;
                };
                if !actions.iter().any(|cmd| {
                    matches!(
                        cmd.as_str(),
                        "Dump" | "NormallyOpen" | "Pilot" | "Nitrogen" | "Nitrous" | "StartFill"
                    )
                }) {
                    continue;
                }
                let mut grouped: Vec<String> = GSE_SEQUENCE_ACTIONS
                    .iter()
                    .map(|(cmd, _)| (*cmd).into())
                    .collect();
                grouped.extend(
                    actions
                        .iter()
                        .filter(|cmd| !GSE_SEQUENCE_ACTIONS.iter().any(|(new, _)| new == cmd))
                        .cloned(),
                );
                *actions = grouped;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A custom layout saved before the ox tank PT rename names
    /// FUEL_TANK_PRESSURE; it loads with OX_TANK_PRESSURE and the new label,
    /// and the shipped layouts no longer name the legacy type.
    #[test]
    fn legacy_fuel_tank_pressure_layout_loads_as_ox_tank_pressure() {
        let shipped = [
            include_str!("../layout/layout.json"),
            include_str!("../layout/layout_hitl.json"),
            include_str!("../layout/layout_test_fire.json"),
        ];
        for source in shipped {
            assert!(!source.contains("FUEL_TANK_PRESSURE") && !source.contains("Fuel Tank"));
            assert!(source.contains("\"OX_TANK_PRESSURE\""));
        }
        let legacy = shipped[0]
            .replace("OX_TANK_PRESSURE", "FUEL_TANK_PRESSURE")
            .replace("\"Ox Tank Pressure\"", "\"Fuel Tank Pressure\"");
        let path = std::env::temp_dir().join(format!("gs-legacy-ox-layout-{}.json", std::process::id()));
        std::fs::write(&path, &legacy).unwrap();
        let loaded = load_layout_file(&path, crate::station_mode::Mode::Normal);
        let _ = std::fs::remove_file(&path);
        let loaded = loaded.unwrap();
        let expected = load_layout_file(
            &default_layout_path(crate::station_mode::Mode::Normal),
            crate::station_mode::Mode::Normal,
        )
        .unwrap();
        assert_eq!(serde_json::to_value(&loaded).unwrap(), serde_json::to_value(&expected).unwrap());
        let tab = loaded.data_tab.tabs.iter().find(|t| t.id == "OX_TANK_PRESSURE").unwrap();
        assert_eq!(tab.label, "Ox Tank Pressure");

        let mut view = serde_json::json!({"FUEL_TANK_PRESSURE": {"index": 0},
            "items": [{"data_type": "FUEL_TANK_PRESSURE", "label": "Fuel tank pressure"}],
            "note": "FUEL_TANK_PRESSURE is not renamed inside longer text"});
        assert!(canonicalize_legacy_type_names(&mut view));
        assert_eq!(view["OX_TANK_PRESSURE"]["index"], 0);
        assert_eq!(view["items"][0]["data_type"], "OX_TANK_PRESSURE");
        assert_eq!(view["items"][0]["label"], "Ox tank pressure");
        assert_eq!(view["note"], "FUEL_TANK_PRESSURE is not renamed inside longer text");
        assert!(!canonicalize_legacy_type_names(&mut view));
    }

    #[test]
    fn legacy_layout_phase_groups_keep_their_wire_meaning() {
        let mut value: serde_json::Value = serde_json::from_str(include_str!("../layout/layout.json")).unwrap();
        value.as_object_mut().unwrap().remove("flight_phase_version");
        value["state_tab"]["states"][0]["states"] = serde_json::json!(["ParachuteDeploy", "Descent"]);
        let mut layout: LayoutConfig = serde_json::from_value(value).unwrap();
        let original_data = serde_json::to_value(&layout.data_tab).unwrap();
        normalize_flight_phases(&mut layout).unwrap();
        assert_eq!(layout.state_tab.states[0].states, ["Descent", "Reefing"]);
        assert_eq!(layout.flight_phase_version, 1);
        assert_eq!(serde_json::to_value(&layout.data_tab).unwrap(), original_data);
        normalize_flight_phases(&mut layout).unwrap();
        assert_eq!(layout.state_tab.states[0].states, ["Descent", "Reefing"]);
    }

    #[test]
    fn gse_commands_are_available_in_all_layouts_and_prelaunch_valve_widgets() {
        for source in [
            include_str!("../layout/layout.json"),
            include_str!("../layout/layout_hitl.json"),
            include_str!("../layout/layout_test_fire.json"),
        ] {
            let mut layout: LayoutConfig = serde_json::from_str(source).unwrap();
            ensure_gse_sequence_actions(&mut layout);
            let once = serde_json::to_value(&layout).unwrap();
            ensure_gse_sequence_actions(&mut layout);
            assert_eq!(once, serde_json::to_value(&layout).unwrap());
            for (cmd, _) in GSE_SEQUENCE_ACTIONS {
                assert_eq!(
                    layout
                        .actions_tab
                        .actions
                        .iter()
                        .filter(|a| a.cmd == cmd)
                        .count(),
                    1
                );
            }
            let mut checked = 0;
            for state in &layout.state_tab.states {
                if !state.states.iter().all(|s| {
                    matches!(
                        s.as_str(),
                        "Startup"
                            | "Idle"
                            | "PreFill"
                            | "FillTest"
                            | "NitrogenFill"
                            | "NitrousFill"
                            | "Armed"
                    )
                }) {
                    continue;
                }
                for widget in state.sections.iter().flat_map(|s| &s.widgets) {
                    if let Some(actions) = &widget.actions
                        && actions.iter().any(|s| s == "Nitrous") {
                            checked += 1;
                            for (cmd, _) in GSE_SEQUENCE_ACTIONS {
                                assert!(actions.iter().any(|s| s == cmd));
                            }
                        }
                }
            }
            assert!(checked > 0);
        }
    }

    #[test]
    fn each_mode_serves_its_live_diagram_view() {
        use crate::station_mode::Mode;
        let shipped = crate::diagrams::load(&crate::diagrams::diagrams_dir());
        let ids: Vec<&str> = shipped.diagrams.iter().filter_map(|d| d["id"].as_str()).collect();
        for (mode, view) in [
            (Mode::Normal, "flight"),
            (Mode::Hitl, "flight"),
            (Mode::Testing, "flight"),
            (Mode::TestFire, "test_fire"),
            (Mode::IntegratedGroundTest, "ground_test"),
        ] {
            let layout = load_layout_file(&default_layout_path(mode), mode).unwrap();
            assert_eq!(layout.live_diagram.as_deref(), Some(view), "{mode:?}");
            assert_eq!(crate::diagrams::default_view(mode), view, "{mode:?}: the files agree with the built-in default");
            assert!(ids.contains(&view), "{view} is served");
            let served = serde_json::to_value(&layout).unwrap();
            assert_eq!(served["live_diagram"], view);
        }
        // A layout without the map (a custom or older file) still gets one.
        let mut layout: LayoutConfig = serde_json::from_str(include_str!("../layout/layout_test_fire.json")).unwrap();
        layout.live_diagrams.clear();
        assert_eq!(live_diagram_for(&layout, Mode::TestFire), "test_fire");
        layout.live_diagrams.insert("test_fire".into(), "network".into());
        assert_eq!(live_diagram_for(&layout, Mode::TestFire), "network", "the layout chooses");
    }

    #[test]
    fn layout_json_is_valid() {
        let layout = load_layout().expect("layout should parse");
        assert!(layout.version >= 1);
        assert!(!layout.connection_tab.sections.is_empty());
        assert!(!layout.actions_tab.actions.is_empty());
        assert!(!layout.data_tab.tabs.is_empty());
        assert!(!layout.state_tab.states.is_empty());
    }

    #[test]
    fn fc_operator_controls_per_mode_with_kalman_confirmations() {
        use crate::station_mode::Mode;
        for mode in [Mode::Normal, Mode::Testing, Mode::IntegratedGroundTest, Mode::Hitl, Mode::TestFire] {
            let layout = load_layout_file(&default_layout_path(mode), mode).unwrap();
            let action = |cmd: &str| layout.actions_tab.actions.iter().find(|a| a.cmd == cmd).cloned();
            let fc = mode != Mode::TestFire;
            for cmd in ["ReinitSensors", "ReinitBarometer", "ReinitIMU", "DisableIMU", "KalmanStop", "KalmanReset", "FcSdRetry"] {
                assert_eq!(action(cmd).is_some(), fc, "{mode:?} {cmd}");
            }
            assert_eq!(action("KalmanStart").is_some(), mode == Mode::Hitl, "{mode:?} KalmanStart");
            // Deploy drogue / main stay HITL only.
            assert_eq!(action("DeployParachute").is_some(), mode == Mode::Hitl, "{mode:?} DeployParachute");
            if fc {
                for cmd in ["KalmanStop", "KalmanReset"] {
                    let confirm = action(cmd).unwrap().confirm.expect("Kalman stop/reset ask first");
                    assert!(!confirm.title.is_empty() && !confirm.button.is_empty(), "{cmd}");
                }
                assert!(action("ReinitSensors").unwrap().confirm.is_none());
                assert_eq!(action("KalmanStop").unwrap().label, "Kalman: stop");
            }
        }
        let hitl = load_layout_file(&default_layout_path(Mode::Hitl), Mode::Hitl).unwrap();
        let start = hitl.actions_tab.actions.iter().find(|a| a.cmd == "KalmanStart").unwrap();
        assert_eq!(start.label, "Kalman: start (HITL)");
        assert!(start.confirm.is_none());
    }

    #[test]
    fn every_control_in_every_mode_has_exactly_one_section() {
        use crate::sequences::{ControlSection, control_section};
        use crate::station_mode::Mode;
        let mut names: Vec<&str> = crate::sequences::every_mode_catalog().concat();
        for mode in [Mode::Normal, Mode::Hitl, Mode::TestFire, Mode::Testing, Mode::IntegratedGroundTest] {
            names.extend(crate::types::client_command_names(mode));
        }
        names.extend(GSE_SEQUENCE_ACTIONS.iter().map(|(cmd, _)| *cmd));
        names.extend(["ResetSim", "AutoColdFlow", "ResetFlightState", "ResetTClock", "RecoverStation", "RadioRejectTest"]);
        for name in &names {
            assert!(control_section(name).is_some(), "{name} has no Actions-tab section");
        }
        assert_eq!(control_section("Abort"), Some(ControlSection::Safety));
        assert_eq!(control_section("NotACommand"), None);
        // Only Abort is a Safety control.
        assert_eq!(
            names.iter().filter(|n| control_section(n) == Some(ControlSection::Safety)).count(),
            names.iter().filter(|n| **n == "Abort").count()
        );
        // The served layout of every mode assigns each action its section.
        for mode in [Mode::Normal, Mode::Hitl, Mode::TestFire, Mode::Testing, Mode::IntegratedGroundTest] {
            let _mode = crate::runtime_mode::test_support::thread_mode(mode);
            let layout = load_layout().expect("layout should parse");
            for action in &layout.actions_tab.actions {
                assert_eq!(action.section, control_section(&action.cmd), "{mode:?} {}", action.cmd);
                assert!(action.section.is_some(), "{mode:?} {} has no section", action.cmd);
            }
            let wire = serde_json::to_value(&layout.actions_tab).unwrap();
            assert!(wire["actions"].as_array().unwrap().iter().all(|a| a["section"].is_string()));
        }
    }

    #[test]
    fn testing_layout_includes_reset_sim_action() {
        crate::mode_suite!(Testing);
        let layout = load_layout().expect("layout should parse");
        assert!(
            layout
                .actions_tab
                .actions
                .iter()
                .any(|action| action.cmd == "ResetSim")
        );
    }
}

#[cfg(test)]
mod vehicle_binding_tests {
    use super::*;

    /// The SEDS layouts name what the UI used to hard-code: the HITL
    /// interlock labels, the GPS types and the fill-target fluids.
    #[test]
    fn shipped_layouts_name_the_seds_bindings() {
        for file in [DEFAULT_LAYOUT_PATH, DEFAULT_HITL_LAYOUT_PATH, DEFAULT_TEST_FIRE_LAYOUT_PATH] {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(file);
            let layout: LayoutConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let labels: Vec<(&str, &str)> =
                layout.actions_tab.controls.iter().map(|c| (c.cmd.as_str(), c.label.as_str())).collect();
            // The interlock toggles exist only in HITL (only that build has the
            // commands), so only the HITL layout names them.
            if file == DEFAULT_HITL_LAYOUT_PATH {
                assert_eq!(
                    labels,
                    [
                        ("TogglePlumbingDelayInterlock", "Retraction minimum 25 seconds"),
                        ("ToggleChecklistInterlock", "Checklist enforcement"),
                        ("ToggleSupplyInterlock", "Nitrogen/nitrous one at a time"),
                        ("ToggleBoardStateProofInterlock", "Board state checks"),
                    ],
                );
            } else {
                assert!(labels.is_empty(), "{file}");
            }
            assert_eq!(
                layout.gps,
                Some(GpsLayout {
                    data_types: vec!["GPS_DATA".into(), "GPS".into(), "ROCKET_GPS".into()],
                    lat_index: 0,
                    lon_index: 1,
                    alt_index: 2,
                }),
                "{file}"
            );
            let fluids: Vec<(&str, &str)> = layout
                .actions_tab
                .fill_target_fluids
                .iter()
                .map(|f| (f.id.as_str(), f.label.as_str()))
                .collect();
            assert_eq!(fluids, [("nitrogen", "Nitrogen"), ("nitrous", "Nitrous")], "{file}");
        }
    }

    /// A layout without the optional bindings still loads and serves none of
    /// them, so the UI keeps its built-in values.
    #[test]
    fn optional_bindings_are_omitted_when_absent() {
        let mut value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_LAYOUT_PATH)).unwrap(),
        )
        .unwrap();
        let root = value.as_object_mut().unwrap();
        root.remove("gps");
        root.remove("watch_items");
        root.remove("live_summary");
        let actions = root["actions_tab"].as_object_mut().unwrap();
        actions.remove("controls");
        actions.remove("fill_target_fluids");
        let layout: LayoutConfig = serde_json::from_value(value).unwrap();
        let served = serde_json::to_value(&layout).unwrap();
        for key in ["gps", "watch_items", "live_summary"] {
            assert!(served.get(key).is_none(), "{key}");
        }
        for key in ["controls", "fill_target_fluids"] {
            assert!(served["actions_tab"].get(key).is_none(), "{key}");
        }
    }

    /// Every shipped layout names its lock-screen readings, with labels and
    /// units, from channels it already binds for the watch.
    #[test]
    fn shipped_layouts_name_their_live_summary_readings() {
        for file in [DEFAULT_LAYOUT_PATH, DEFAULT_HITL_LAYOUT_PATH, DEFAULT_TEST_FIRE_LAYOUT_PATH] {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(file);
            let layout: LayoutConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let items = &layout.live_summary;
            assert!(!items.is_empty() && items.len() <= MAX_LIVE_SUMMARY_ITEMS, "{file}: {}", items.len());
            let watched: Vec<&WatchChannel> = layout.watch_items.iter().flat_map(|w| &w.channels).collect();
            for item in items {
                assert!(!item.label.trim().is_empty() && !item.unit.trim().is_empty(), "{file}: {item:?}");
                assert!(item.decimals <= 6, "{file}: {item:?}");
                assert!(!item.channels.is_empty(), "{file}: {}", item.label);
                for channel in &item.channels {
                    assert!(watched.contains(&channel), "{file}: {} reads {channel:?}, which no watch item binds", item.label);
                }
            }
            let mut normalized = items.clone();
            normalize_live_summary(&mut normalized);
            assert_eq!(&normalized, items, "{file} is already normal");
        }
    }

    #[test]
    fn live_summary_drops_unusable_readings_and_keeps_four() {
        let channel = |dt: &str| WatchChannel { data_type: dt.into(), sender_id: None, index: 0 };
        let item = |label: &str, channels: Vec<WatchChannel>| LiveSummaryItem {
            label: label.into(),
            unit: " psi ".into(),
            decimals: 9,
            channels,
        };
        let mut items = vec![
            item("  ", vec![channel("A")]),
            item("No channel", vec![channel(" ")]),
            item(" One ", vec![channel("A"), channel("")]),
            item("Two", vec![channel("B")]),
            item("Three", vec![channel("C")]),
            item("Four", vec![channel("D")]),
            item("Five", vec![channel("E")]),
        ];
        normalize_live_summary(&mut items);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["One", "Two", "Three", "Four"]);
        assert_eq!(items[0].channels, vec![channel("A")]);
        assert_eq!((items[0].unit.as_str(), items[0].decimals), ("psi", 6));
        // Decimals default to one when a layout leaves them out.
        let parsed: LiveSummaryItem =
            serde_json::from_value(serde_json::json!({"label": "Tank", "channels": [{"data_type": "A"}]})).unwrap();
        assert_eq!((parsed.unit.as_str(), parsed.decimals, parsed.channels[0].index), ("", 1, 0));
    }
}
