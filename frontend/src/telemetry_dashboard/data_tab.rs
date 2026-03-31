use super::layout::{
    BooleanLabels, DataChartGroup, DataChartScaleMode, DataSubtabSpec, DataSummaryItem,
    DataTabLayout, DataTabSpec, ThemeConfig, ValueFormatKind, ValueFormatter,
};
// frontend/src/telemetry_dashboard/data_tab.rs
use dioxus::prelude::*;
use dioxus_signals::{ReadableExt, Signal, WritableExt};
use std::rc::Rc;

use super::data_chart::{
    charts_cache_get, charts_cache_get_channel_minmax, charts_cache_get_subset, charts_cache_get_subset_per_series,
    sender_scoped_chart_key, series_color, ChartCanvas, CHART_GRID_BOTTOM_PAD,
    CHART_GRID_LEFT, CHART_GRID_RIGHT_PAD, CHART_GRID_TOP, CHART_X_LABEL_BOTTOM,
    CHART_X_LABEL_LEFT_INSET, CHART_Y_LABEL_LEFT, CHART_Y_LABEL_MAX_WIDTH,
};
use super::{latest_telemetry_row, latest_telemetry_value, translate_text, TELEMETRY_RENDER_EPOCH};

const _ACTIVE_TAB_STORAGE_KEY: &str = "gs26_active_tab";
const _ACTIVE_SUBTAB_STORAGE_KEY: &str = "gs26_active_data_subtab";
const DATA_TAB_RESPONSIVE_CSS: &str = r#"
.gs26-data-tab-shell, .gs26-data-subtab-shell { min-width: 0; }
.gs26-data-tab-toggle, .gs26-data-subtab-toggle { display:none; }
.gs26-data-tab-nav, .gs26-data-subtab-nav { display:flex; gap:8px; flex-wrap:wrap; align-items:center; }
@media (max-width: 720px), (max-height: 780px) {
  .gs26-data-tab-shell, .gs26-data-subtab-shell {
    display:grid;
    grid-template-columns:1fr;
    gap:0.75rem;
    width:100%;
  }
  .gs26-data-tab-toggle, .gs26-data-subtab-toggle {
    display:inline-flex;
    align-items:center;
    justify-content:center;
    justify-self:start;
    padding:0.55rem 0.9rem;
    border-radius:0.75rem;
    border:1px solid var(--gs26-data-toggle-border);
    background:var(--gs26-data-toggle-background);
    color:var(--gs26-data-toggle-text);
    font:inherit;
    font-weight:800;
    cursor:pointer;
  }
  .gs26-data-tab-nav, .gs26-data-subtab-nav { display:none; width:100%; }
  .gs26-data-tab-shell[data-expanded="true"] .gs26-data-tab-nav,
  .gs26-data-subtab-shell[data-expanded="true"] .gs26-data-subtab-nav {
    display:flex;
    flex-direction:column;
    align-items:stretch;
  }
  .gs26-data-tab-nav button, .gs26-data-subtab-nav button {
    width:100%;
  }
}
"#;

#[cfg(target_arch = "wasm32")]
fn localstorage_get(key: &str) -> Option<String> {
    use web_sys::window;
    let w = window()?;
    let ls = w.local_storage().ok()??;
    ls.get_item(key).ok().flatten()
}

#[cfg(target_arch = "wasm32")]
fn localstorage_set(key: &str, value: &str) {
    use web_sys::window;
    if let Some(w) = window() {
        if let Ok(Some(ls)) = w.local_storage() {
            let _ = ls.set_item(key, value);
        }
    }
}

#[component]
pub fn DataTab(active_tab: Signal<String>, layout: DataTabLayout, theme: ThemeConfig) -> Element {
    let _ = *TELEMETRY_RENDER_EPOCH.read();
    let is_fullscreen = use_signal(|| false);
    let show_chart = use_signal(|| true);
    let active_subtab = use_signal(String::new);
    let tabs_expanded = use_signal(|| false);
    let subtabs_expanded = use_signal(|| false);

    // -------- Restore + persist active tab --------
    let did_restore = use_signal(|| false);
    let last_saved = use_signal(String::new);
    let last_saved_subtab = use_signal(String::new);

    // Restore ONCE
    use_effect({
        let mut active_tab = active_tab;
        let mut did_restore = did_restore;
        let layout_tabs = layout.tabs.clone();

        move || {
            if *did_restore.read() {
                return;
            }
            did_restore.set(true);

            // 1) Try localStorage
            #[cfg(target_arch = "wasm32")]
            if let Some(saved) = localstorage_get(_ACTIVE_TAB_STORAGE_KEY) {
                if !saved.is_empty() {
                    active_tab.set(saved);
                    return;
                }
            }

            // 2) Fallback: if empty, pick first layout tab
            if active_tab.read().is_empty()
                && let Some(first) = layout_tabs.first()
            {
                active_tab.set(first.id.clone());
            }
        }
    });

    // Persist whenever it changes (avoid rewriting same value)
    use_effect({
        let active_tab = active_tab;
        let mut last_saved = last_saved;

        move || {
            let cur = active_tab.read().clone();
            if cur.is_empty() || cur == *last_saved.read() {
                return;
            }
            last_saved.set(cur.clone());

            #[cfg(target_arch = "wasm32")]
            localstorage_set(_ACTIVE_TAB_STORAGE_KEY, &cur);
        }
    });

    #[cfg(target_arch = "wasm32")]
    use_effect({
        let mut active_subtab = active_subtab;
        move || {
            if active_subtab.read().is_empty()
                && let Some(saved) = localstorage_get(_ACTIVE_SUBTAB_STORAGE_KEY)
                && !saved.is_empty()
            {
                active_subtab.set(saved);
            }
        }
    });

    use_effect({
        let active_subtab = active_subtab;
        let mut last_saved_subtab = last_saved_subtab;
        move || {
            let cur = active_subtab.read().clone();
            if cur.is_empty() || cur == *last_saved_subtab.read() {
                return;
            }
            last_saved_subtab.set(cur.clone());

            #[cfg(target_arch = "wasm32")]
            localstorage_set(_ACTIVE_SUBTAB_STORAGE_KEY, &cur);
        }
    });

    // Layout-defined data types (for buttons)
    let types = layout.tabs.clone();
    let current = active_tab.read().clone();
    let current_tab = types.iter().find(|t| t.id == current);
    let current_subtabs = current_tab
        .and_then(|tab| tab.subtabs.as_ref())
        .cloned()
        .unwrap_or_default();
    let selected_subtab = if current_subtabs.is_empty() {
        None
    } else {
        let selected_id = active_subtab.read().clone();
        current_subtabs
            .iter()
            .find(|subtab| subtab.id == selected_id)
            .cloned()
            .or_else(|| current_subtabs.first().cloned())
    };

    use_effect({
        let current_tab_id = current.clone();
        let current_subtabs = current_subtabs.clone();
        let mut active_subtab = active_subtab;
        move || {
            if current_tab_id.is_empty() || current_subtabs.is_empty() {
                return;
            }
            let current = active_subtab.read().clone();
            if !current_subtabs.iter().any(|subtab| subtab.id == current) {
                active_subtab.set(current_subtabs[0].id.clone());
            }
        }
    });

    let effective_source = effective_source(current_tab, selected_subtab.as_ref());
    let labels = effective_labels(current_tab, selected_subtab.as_ref());
    let channel_formatters = effective_channel_formatters(current_tab, selected_subtab.as_ref());
    let boolean_labels = effective_boolean_labels(current_tab, selected_subtab.as_ref());
    let channel_boolean_labels =
        effective_channel_boolean_labels(current_tab, selected_subtab.as_ref());

    let chart_enabled = selected_subtab
        .as_ref()
        .and_then(|subtab| subtab.chart.as_ref().map(|c| c.enabled))
        .or_else(|| current_tab.and_then(|tab| tab.chart.as_ref().map(|c| c.enabled)))
        .unwrap_or(true);
    let latest_row = effective_source
        .as_ref()
        .and_then(|source| latest_telemetry_row(&source.data_type, source.sender_id.as_deref()));
    let summary_items = selected_subtab
        .as_ref()
        .and_then(|subtab| subtab.summary_items.as_ref())
        .cloned()
        .unwrap_or_default();

    let is_valve_state = current == "VALVE_STATE";
    let has_telemetry = if !summary_items.is_empty() {
        summary_items.iter().any(summary_item_has_value)
    } else {
        latest_row.is_some()
    };
    let is_graph_allowed = chart_enabled
        && has_telemetry
        && current != "GPS_DATA"
        && !is_valve_state
        && effective_source.is_some();

    // Viewport constants
    let view_w = 1200.0_f64;
    let view_h = 360.0_f64;
    let view_h_full = fullscreen_view_height().max(260.0);

    let left = CHART_GRID_LEFT;
    let right = view_w - CHART_GRID_RIGHT_PAD;
    let pad_top = CHART_GRID_TOP;
    let pad_bottom = CHART_GRID_BOTTOM_PAD;

    let inner_h = view_h - pad_top - pad_bottom;
    let inner_h_full = view_h_full - pad_top - pad_bottom;

    let chart_key = effective_source
        .as_ref()
        .map(chart_key_for_source)
        .unwrap_or_else(|| current.clone());
    let (_chunks, _y_min, _y_max, _span_min) =
        charts_cache_get(&chart_key, view_w as f32, view_h as f32);
    let (chan_min, chan_max) =
        charts_cache_get_channel_minmax(&chart_key, view_w as f32, view_h as f32);
    let chart_groups = effective_chart_groups(current_tab, selected_subtab.as_ref(), labels.len());
    let data_tabs_toggle_label = if *tabs_expanded.read() {
        "Hide data tabs".to_string()
    } else {
        let label = current_tab
            .map(|tab| translate_text(&tab.label))
            .unwrap_or_else(|| translate_text("Data tabs"));
        format!("Show data tabs ({label})")
    };
    let data_subtabs_toggle_label = if *subtabs_expanded.read() {
        "Hide subtabs".to_string()
    } else {
        let label = selected_subtab
            .as_ref()
            .map(|subtab| translate_text(&subtab.label))
            .unwrap_or_else(|| translate_text("Subtabs"));
        format!("Show subtabs ({label})")
    };
    let summary_content = if !summary_items.is_empty() {
        rsx! {
            div {
                style: "display:grid; gap:10px; align-items:stretch; grid-template-columns:repeat(auto-fit, minmax(150px, 1fr)); width:100%;",
                for (i, item) in summary_items.iter().enumerate() {
                    SummaryCard {
                        label: translate_text(&item.label),
                        min: None,
                        max: None,
                        value: summary_item_value(item),
                        color: summary_color(i),
                    }
                }
            }
        }
    } else {
        match latest_row {
            None => rsx! {
                div { style: "color:{theme.text_muted}; padding:2px 2px;", "{translate_text(\"Waiting for telemetry…\")}" }
            },
            Some(row) => {
                let vals = &row.values;
                rsx! {
                    div {
                        style: "display:grid; gap:10px; align-items:stretch; grid-template-columns:repeat(auto-fit, minmax(110px, 1fr)); width:100%;",
                        for (i, label) in labels.iter().enumerate() {
                            if !label.is_empty() {
                                SummaryCard {
                                    label: label.clone(),
                                    min: if is_graph_allowed { chan_min.get(i).copied().flatten().map(|v| format_value(Some(v), channel_formatters.and_then(|list| list.get(i)))) } else { None },
                                    max: if is_graph_allowed { chan_max.get(i).copied().flatten().map(|v| format_value(Some(v), channel_formatters.and_then(|list| list.get(i)))) } else { None },
                                    value: if let Some(lbls) = channel_boolean_labels
                                        .and_then(|list| list.get(i))
                                    {
                                        boolean_value_text(vals.get(i).copied().flatten(), Some(lbls))
                                    } else if is_valve_state || boolean_labels.is_some() {
                                        boolean_value_text(vals.get(i).copied().flatten(), boolean_labels)
                                    } else {
                                        format_value(vals.get(i).copied().flatten(), channel_formatters.and_then(|list| list.get(i)))
                                    },
                                    color: summary_color(i),
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    rsx! {
        style {
            "{DATA_TAB_RESPONSIVE_CSS}"
        }
        div {
            style: "padding:16px; height:100%; overflow-y:auto; overflow-x:hidden; -webkit-overflow-scrolling:auto; display:flex; flex-direction:column; gap:12px; padding-bottom:10px; --gs26-data-toggle-background:{theme.tab_shell_background}; --gs26-data-toggle-border:{theme.tab_shell_border}; --gs26-data-toggle-text:{theme.button_text};",

            div { style: "display:flex; flex-direction:column; gap:10px;",

                div {
                    class: "gs26-data-tab-shell",
                    "data-expanded": if *tabs_expanded.read() { "true" } else { "false" },
                    button {
                        class: "gs26-data-tab-toggle",
                        onclick: {
                            let mut tabs_expanded = tabs_expanded;
                            move |_| {
                                let next = !*tabs_expanded.read();
                                tabs_expanded.set(next);
                            }
                        },
                        "{data_tabs_toggle_label}"
                    }
                div { class: "gs26-data-tab-nav",
                    for t in types.iter().take(32) {
                        button {
                            style: if t.id == current {
                                {
                                    let accent = theme
                                        .main_tab_accents
                                        .get("data")
                                        .map(String::as_str)
                                        .unwrap_or("#f97316");
                                    format!(
                                        "padding:6px 10px; border-radius:999px; border:1px solid {accent}; background:{}; color:{accent}; cursor:pointer;\
                                         display:inline-flex; align-items:center; justify-content:center;\
                                         font:inherit;\
                                         min-width:0; max-width:100%; text-align:center; line-height:1.2;\
                                         white-space:normal; overflow-wrap:anywhere; word-break:break-word;",
                                        theme.button_background
                                    )
                                }
                            } else {
                                format!(
                                    "padding:6px 10px; border-radius:999px; border:1px solid {}; background:{}; color:{}; cursor:pointer;\
                                     display:inline-flex; align-items:center; justify-content:center;\
                                     font:inherit;\
                                     min-width:0; max-width:100%; text-align:center; line-height:1.2;\
                                     white-space:normal; overflow-wrap:anywhere; word-break:break-word;",
                                    theme.border, theme.panel_background, theme.text_primary
                                )
                            },
                            onclick: {
                                let t = t.id.clone();
                                let mut active_tab2 = active_tab;
                                let mut tabs_expanded = tabs_expanded;
                                let mut subtabs_expanded = subtabs_expanded;
                                move |_| {
                                    active_tab2.set(t.clone());
                                    tabs_expanded.set(false);
                                    subtabs_expanded.set(false);
                                }
                            },
                            "{translate_text(&t.label)}"
                        }
                    }
                }
                }

                if !current_subtabs.is_empty() {
                    div {
                        class: "gs26-data-subtab-shell",
                        "data-expanded": if *subtabs_expanded.read() { "true" } else { "false" },
                        button {
                            class: "gs26-data-subtab-toggle",
                            onclick: {
                                let mut subtabs_expanded = subtabs_expanded;
                                move |_| {
                                    let next = !*subtabs_expanded.read();
                                    subtabs_expanded.set(next);
                                }
                            },
                            "{data_subtabs_toggle_label}"
                        }
                    div { class: "gs26-data-subtab-nav",
                        for subtab in current_subtabs.iter() {
                            button {
                                style: if selected_subtab.as_ref().is_some_and(|active| active.id == subtab.id) {
                                    {
                                        let accent = theme
                                            .main_tab_accents
                                            .get("data")
                                            .map(String::as_str)
                                            .unwrap_or("#f97316");
                                        format!(
                                            "padding:5px 10px; border-radius:999px; border:1px solid {accent}; background:{}; color:{accent}; cursor:pointer; font-size:12px;\
                                             display:inline-flex; align-items:center; justify-content:center;\
                                             font-family:inherit;\
                                             min-width:0; max-width:100%; text-align:center; line-height:1.2;\
                                             white-space:normal; overflow-wrap:anywhere; word-break:break-word;",
                                            theme.button_background
                                        )
                                    }
                                } else {
                                    format!(
                                        "padding:5px 10px; border-radius:999px; border:1px solid {}; background:{}; color:{}; cursor:pointer; font-size:12px;\
                                         display:inline-flex; align-items:center; justify-content:center;\
                                         font-family:inherit;\
                                         min-width:0; max-width:100%; text-align:center; line-height:1.2;\
                                         white-space:normal; overflow-wrap:anywhere; word-break:break-word;",
                                        theme.border_soft, theme.panel_background, theme.text_secondary
                                    )
                                },
                                onclick: {
                                    let id = subtab.id.clone();
                                    let mut active_subtab = active_subtab;
                                    let mut subtabs_expanded = subtabs_expanded;
                                    move |_| {
                                        active_subtab.set(id.clone());
                                        subtabs_expanded.set(false);
                                    }
                                },
                                "{translate_text(&subtab.label)}"
                            }
                        }
                    }
                    }
                }

                {summary_content}
            }

            if is_graph_allowed {
                DataGraphPanel {
                    theme: theme.clone(),
                    chart_groups: chart_groups.clone(),
                    chart_key: chart_key.clone(),
                    labels: labels.clone(),
                    view_w: view_w,
                    view_h: view_h,
                    view_h_full: view_h_full,
                    left: left,
                    right: right,
                    pad_top: pad_top,
                    pad_bottom: pad_bottom,
                    inner_h: inner_h,
                    inner_h_full: inner_h_full,
                    is_fullscreen: is_fullscreen,
                    show_chart: show_chart,
                }
            }
        }
    }
}

fn summary_color(i: usize) -> &'static str {
    series_color(i)
}

fn effective_source(
    tab: Option<&DataTabSpec>,
    subtab: Option<&DataSubtabSpec>,
) -> Option<DataSource> {
    let data_type = subtab
        .and_then(|subtab| subtab.data_type.clone())
        .or_else(|| tab.map(|tab| tab.id.clone()))?;
    let sender_id = subtab.and_then(|subtab| subtab.sender_id.clone());
    Some(DataSource {
        data_type,
        sender_id,
    })
}

fn effective_labels(tab: Option<&DataTabSpec>, subtab: Option<&DataSubtabSpec>) -> Vec<String> {
    if let Some(channels) = subtab.and_then(|subtab| subtab.channels.as_ref()) {
        return channels.iter().map(|label| translate_text(label)).collect();
    }
    tab.map(|tab| {
        tab.channels
            .iter()
            .map(|label| translate_text(label))
            .collect()
    })
    .unwrap_or_default()
}

fn effective_channel_formatters<'a>(
    tab: Option<&'a DataTabSpec>,
    subtab: Option<&'a DataSubtabSpec>,
) -> Option<&'a Vec<ValueFormatter>> {
    subtab
        .and_then(|subtab| subtab.channel_formatters.as_ref())
        .or_else(|| tab.and_then(|tab| tab.channel_formatters.as_ref()))
}

fn effective_boolean_labels<'a>(
    tab: Option<&'a DataTabSpec>,
    subtab: Option<&'a DataSubtabSpec>,
) -> Option<&'a BooleanLabels> {
    subtab
        .and_then(|subtab| subtab.boolean_labels.as_ref())
        .or_else(|| tab.and_then(|tab| tab.boolean_labels.as_ref()))
}

fn effective_channel_boolean_labels<'a>(
    tab: Option<&'a DataTabSpec>,
    subtab: Option<&'a DataSubtabSpec>,
) -> Option<&'a Vec<BooleanLabels>> {
    subtab
        .and_then(|subtab| subtab.channel_boolean_labels.as_ref())
        .or_else(|| tab.and_then(|tab| tab.channel_boolean_labels.as_ref()))
}

fn effective_chart_groups(
    tab: Option<&DataTabSpec>,
    subtab: Option<&DataSubtabSpec>,
    channel_count: usize,
) -> Vec<DataChartGroup> {
    subtab
        .and_then(|subtab| subtab.chart_groups.as_ref())
        .or_else(|| tab.and_then(|tab| tab.chart_groups.as_ref()))
        .cloned()
        .unwrap_or_else(|| {
            vec![DataChartGroup {
                title: None,
                data_type: None,
                sender_id: None,
                labels: None,
                channels: (0..channel_count).collect(),
                scale_mode: None,
            }]
        })
}

fn chart_key_for_group(group: &DataChartGroup, fallback: &str) -> String {
    if let Some(data_type) = group.data_type.as_deref() {
        if let Some(sender_id) = group.sender_id.as_deref() {
            return sender_scoped_chart_key(data_type, sender_id);
        }
        return data_type.to_string();
    }
    fallback.to_string()
}

fn chart_key_for_source(source: &DataSource) -> String {
    source
        .sender_id
        .as_deref()
        .map(|sender_id| sender_scoped_chart_key(&source.data_type, sender_id))
        .unwrap_or_else(|| source.data_type.clone())
}

fn summary_item_has_value(item: &DataSummaryItem) -> bool {
    latest_telemetry_value(&item.data_type, item.sender_id.as_deref(), item.index).is_some()
}

fn summary_item_value(item: &DataSummaryItem) -> String {
    let value = latest_telemetry_value(&item.data_type, item.sender_id.as_deref(), item.index);
    if item.boolean_labels.is_some() {
        boolean_value_text(value, item.boolean_labels.as_ref())
    } else {
        format_value(value, item.formatter.as_ref())
    }
}

#[derive(Clone)]
struct DataSource {
    data_type: String,
    sender_id: Option<String>,
}

#[component]
#[allow(clippy::too_many_arguments)]
fn DataGraphPanel(
    theme: ThemeConfig,
    chart_groups: Vec<DataChartGroup>,
    chart_key: String,
    labels: Vec<String>,
    view_w: f64,
    view_h: f64,
    view_h_full: f64,
    left: f64,
    right: f64,
    pad_top: f64,
    pad_bottom: f64,
    inner_h: f64,
    inner_h_full: f64,
    is_fullscreen: Signal<bool>,
    show_chart: Signal<bool>,
) -> Element {
    let _ = *TELEMETRY_RENDER_EPOCH.read();
    let x_pct = |x: f64, total: f64| format!("{:.4}%", (x / total) * 100.0);
    let y_pct = |y: f64, total: f64| format!("{:.4}%", (y / total) * 100.0);
    let on_toggle_fullscreen = move |_: Event<MouseData>| {
        let next = !*is_fullscreen.read();
        is_fullscreen.set(next);
    };
    let on_toggle_chart = move |_: Event<MouseData>| {
        let next = !*show_chart.read();
        show_chart.set(next);
    };

    let (_chunks, _y_min, _y_max, _span_min) =
        charts_cache_get(&chart_key, view_w as f32, view_h as f32);

    rsx! {
        div { style: "flex:0; width:100%; margin-top:6px;",
            div { style: "width:100%;",
                div { style: "display:flex; justify-content:flex-end; gap:8px; margin-bottom:6px;",
                    button {
                        style: "padding:6px 12px; border-radius:999px; border:1px solid {theme.info_accent}; background:{theme.info_background}; color:{theme.info_text}; font-size:0.85rem; cursor:pointer;",
                        onclick: on_toggle_chart,
                        if *show_chart.read() {
                            "{translate_text(\"Collapse\")}"
                        } else {
                            "{translate_text(\"Expand\")}"
                        }
                    }
                    button {
                        style: "padding:6px 12px; border-radius:999px; border:1px solid {theme.info_accent}; background:{theme.info_background}; color:{theme.info_text}; font-size:0.85rem; cursor:pointer;",
                        onclick: on_toggle_fullscreen,
                        "{translate_text(\"Fullscreen\")}"
                    }
                }

                if *show_chart.read() {
                    div { style: "display:flex; flex-direction:column; gap:12px;",
                        for group in chart_groups.iter() {
                            {render_chart_group(
                                group,
                                &chart_key,
                                &labels,
                                view_w,
                                view_h,
                                left,
                                right,
                                pad_top,
                                pad_bottom,
                                inner_h,
                                &x_pct,
                                &y_pct,
                                &theme,
                            )}
                        }
                    }
                }
            }
        }

        if *is_fullscreen.read() {
            {
                let (_chunks_full, _y_min2, _y_max2, _span_min2) =
                    charts_cache_get(&chart_key, view_w as f32, view_h_full as f32);

                rsx! {
                    div { style: "position:fixed; inset:0; z-index:9998; padding:16px; background:{theme.app_background}; display:flex; flex-direction:column; gap:12px;",
                        div { style: "display:flex; align-items:center; justify-content:space-between; gap:12px;",
                            h2 { style: "margin:0; color:{theme.main_tab_accents.get(\"data\").map(String::as_str).unwrap_or(\"#f97316\")};", "{translate_text(\"Data Graph\")}" }
                            button {
                                style: "padding:6px 12px; border-radius:999px; border:1px solid {theme.info_accent}; background:{theme.info_background}; color:{theme.info_text}; font-size:0.85rem; cursor:pointer;",
                                onclick: on_toggle_fullscreen,
                                "{translate_text(\"Exit Fullscreen\")}"
                            }
                        }

                        div {
                            style: "flex:1; min-height:0; width:100%; overflow-y:auto; display:flex; flex-direction:column; gap:12px;",
                            for group in chart_groups.iter() {
                                {render_chart_group(
                                    group,
                                    &chart_key,
                                    &labels,
                                    view_w,
                                    view_h_full,
                                    left,
                                    right,
                                    pad_top,
                                    pad_bottom,
                                    inner_h_full,
                                    &x_pct,
                                    &y_pct,
                                    &theme,
                                )}
                            }
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_chart_group(
    group: &DataChartGroup,
    fallback_chart_key: &str,
    fallback_labels: &[String],
    view_w: f64,
    view_h: f64,
    left: f64,
    right: f64,
    pad_top: f64,
    pad_bottom: f64,
    inner_h: f64,
    x_pct: &dyn Fn(f64, f64) -> String,
    y_pct: &dyn Fn(f64, f64) -> String,
    theme: &ThemeConfig,
) -> Element {
    let chart_key = chart_key_for_group(group, fallback_chart_key);
    let per_series_scale = matches!(group.scale_mode, Some(DataChartScaleMode::PerSeries));
    let (filtered_chunks, y_min, y_max, span_min, per_series_scales) = if per_series_scale {
        let (chunks, scales, span_min) = charts_cache_get_subset_per_series(
            &chart_key,
            &group.channels,
            view_w as f32,
            view_h as f32,
        );
        let overall_min = scales
            .iter()
            .flatten()
            .map(|(min, _)| *min)
            .fold(f32::INFINITY, f32::min);
        let overall_max = scales
            .iter()
            .flatten()
            .map(|(_, max)| *max)
            .fold(f32::NEG_INFINITY, f32::max);
        (
            chunks,
            if overall_min.is_finite() {
                overall_min
            } else {
                0.0
            },
            if overall_max.is_finite() {
                overall_max
            } else {
                1.0
            },
            span_min,
            scales,
        )
    } else {
        let (chunks, y_min, y_max, span_min) =
            charts_cache_get_subset(&chart_key, &group.channels, view_w as f32, view_h as f32);
        (chunks, y_min, y_max, span_min, Rc::new(Vec::new()))
    };
    if filtered_chunks.is_empty() {
        return rsx! {};
    }
    let x_left_s = fmt_span(span_min);
    let x_mid_s = fmt_span(span_min * 0.5);
    let y_mid = (y_min + y_max) * 0.5;
    let y_max_s = format!("{:.2}", y_max);
    let y_mid_s = format!("{:.2}", y_mid);
    let y_min_s = format!("{:.2}", y_min);
    let legend_source = group.labels.as_deref().unwrap_or(fallback_labels);
    let legend_rows: Vec<(usize, &str)> = group
        .channels
        .iter()
        .enumerate()
        .filter_map(|(group_idx, idx)| {
            legend_source
                .get(*idx)
                .or_else(|| legend_source.get(group_idx))
                .map(|label| (group_idx, label.as_str()))
        })
        .filter(|(_, label)| !label.is_empty())
        .collect();

    rsx! {
        div { style: "width:100%; background:{theme.app_background}; border-radius:14px; border:1px solid {theme.border}; padding:12px; display:flex; flex-direction:column; gap:8px;",
            if let Some(title) = group.title.as_ref() {
                div { style: "font-size:13px; font-weight:600; color:{theme.text_primary};", "{translate_text(title)}" }
            }
            div { style: "display:flex; gap:6px; align-items:stretch;",
                if per_series_scale {
                    div { style: "flex:0 0 96px; width:96px; min-width:96px; display:flex; flex-direction:column; justify-content:space-between; align-items:flex-end; font-size:clamp(8px, 1.8vw, 10px); padding-top:4px; padding-bottom:28px; overflow:hidden;",
                        div { style: "display:flex; justify-content:flex-end; flex-wrap:nowrap; gap:6px; white-space:nowrap; width:100%; text-align:right;",
                            for (i, _) in group.channels.iter().enumerate() {
                                if let Some((_, series_max)) = per_series_scales.get(i).and_then(|scale| *scale) {
                                    div { style: "color:{series_color(i)};", {format!("{:.2}", series_max)} }
                                }
                            }
                        }
                        div { style: "display:flex; justify-content:flex-end; flex-wrap:nowrap; gap:6px; white-space:nowrap; width:100%; text-align:right;",
                            for (i, _) in group.channels.iter().enumerate() {
                                if let Some((series_min, series_max)) = per_series_scales.get(i).and_then(|scale| *scale) {
                                    div { style: "color:{series_color(i)};", {format!("{:.2}", (series_min + series_max) * 0.5)} }
                                }
                            }
                        }
                        div { style: "display:flex; justify-content:flex-end; flex-wrap:nowrap; gap:6px; white-space:nowrap; width:100%; text-align:right;",
                            for (i, _) in group.channels.iter().enumerate() {
                                if let Some((series_min, _)) = per_series_scales.get(i).and_then(|scale| *scale) {
                                    div { style: "color:{series_color(i)};", {format!("{:.2}", series_min)} }
                                }
                            }
                        }
                    }
                }
                div { style: "position:relative; flex:1 1 auto; min-width:0; aspect-ratio:{view_w}/{view_h};",
                    ChartCanvas {
                        view_w: view_w,
                        view_h: view_h,
                        chunks: filtered_chunks,
                        grid_left: Some(left),
                        grid_right: Some(right),
                        grid_top: Some(pad_top),
                        grid_bottom: Some(view_h - pad_bottom),
                        style: "position:absolute; inset:0; width:100%; height:100%; display:block;".to_string(),
                    }
                    div { style: "position:absolute; inset:0; pointer-events:none; font-size:clamp(8px, 1.8vw, 10px); color:{theme.text_muted};",
                        if !per_series_scale {
                            span { style: "position:absolute; left:{CHART_Y_LABEL_LEFT}px; top:{y_pct(pad_top + 6.0, view_h)}; max-width:{CHART_Y_LABEL_MAX_WIDTH}px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;", "{y_max_s}" }
                            span { style: "position:absolute; left:{CHART_Y_LABEL_LEFT}px; top:{y_pct(pad_top + inner_h / 2.0 + 4.0, view_h)}; transform:translateY(-50%); max-width:{CHART_Y_LABEL_MAX_WIDTH}px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;", "{y_mid_s}" }
                            span { style: "position:absolute; left:{CHART_Y_LABEL_LEFT}px; top:{y_pct(view_h - pad_bottom + 1.0, view_h)}; transform:translateY(-100%); max-width:{CHART_Y_LABEL_MAX_WIDTH}px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;", "{y_min_s}" }
                        }
                        span { style: "position:absolute; left:{x_pct(left + CHART_X_LABEL_LEFT_INSET, view_w)}; bottom:{CHART_X_LABEL_BOTTOM}px;", "{x_left_s}" }
                        span { style: "position:absolute; left:{x_pct(view_w * 0.5, view_w)}; bottom:{CHART_X_LABEL_BOTTOM}px; transform:translateX(-50%);", "{x_mid_s}" }
                        span { style: "position:absolute; left:{x_pct(right - 52.0, view_w)}; bottom:{CHART_X_LABEL_BOTTOM}px;", "{translate_text(\"now\")}" }
                    }
                }
            }
            if !legend_rows.is_empty() {
                div { style: "display:flex; flex-wrap:wrap; gap:8px; padding:6px 10px; background:rgba(2,6,23,0.75); border:1px solid {theme.border_soft}; border-radius:10px;",
                    for (i, label) in legend_rows.iter() {
                        div { style: "display:flex; align-items:center; gap:6px; font-size:12px; color:{theme.text_secondary};",
                            svg { width:"26", height:"8", view_box:"0 0 26 8",
                                line { x1:"1", y1:"4", x2:"25", y2:"4", stroke:"{series_color(*i)}", "stroke-width":"2", "stroke-linecap":"round" }
                            }
                            "{translate_text(label)}"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn SummaryCard(
    label: String,
    value: String,
    min: Option<String>,
    max: Option<String>,
    color: &'static str,
) -> Element {
    let mm = match (min.as_deref(), max.as_deref()) {
        (Some(mi), Some(ma)) => Some(format!(
            "{} {mi} • {} {ma}",
            translate_text("min"),
            translate_text("max")
        )),
        _ => None,
    };

    rsx! {
        div { style: "padding:10px; border-radius:12px; background:#0f172a; border:1px solid #334155; width:100%; min-width:0; box-sizing:border-box;",
            div { style: "font-size:12px; color:{color};", "{label}" }
            div { style: "font-size:18px; color:#e5e7eb; line-height:1.1;", "{value}" }
            if let Some(t) = mm {
                div { style: "font-size:11px; color:#94a3b8; margin-top:4px;", "{t}" }
            }
        }
    }
}

fn format_value(v: Option<f32>, formatter: Option<&ValueFormatter>) -> String {
    match v {
        Some(x) => {
            let kind = formatter
                .and_then(|formatter| formatter.kind.clone())
                .unwrap_or(ValueFormatKind::Number);
            let precision = formatter.and_then(|formatter| formatter.precision);
            let prefix = formatter
                .and_then(|formatter| formatter.prefix.as_deref())
                .unwrap_or("");
            let suffix = formatter
                .and_then(|formatter| formatter.suffix.as_deref())
                .unwrap_or("");
            let value = match kind {
                ValueFormatKind::Number => format!("{x:.prec$}", prec = precision.unwrap_or(4)),
                ValueFormatKind::Integer => format!("{}", x.round() as i64),
            };
            format!("{prefix}{value}{suffix}")
        }
        None => "-".to_string(),
    }
}

fn boolean_value_text(v: Option<f32>, labels: Option<&BooleanLabels>) -> String {
    let true_label = labels.map(|l| l.true_label.as_str()).unwrap_or("Open");
    let false_label = labels.map(|l| l.false_label.as_str()).unwrap_or("Closed");
    let unknown_label = labels
        .and_then(|l| l.unknown_label.as_deref())
        .unwrap_or("Unknown");
    match v {
        Some(val) if val >= 0.5 => translate_text(true_label),
        Some(_) => translate_text(false_label),
        None => translate_text(unknown_label),
    }
}

fn fullscreen_view_height() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        let h = web_sys::window()
            .and_then(|w| w.inner_height().ok())
            .and_then(|v| v.as_f64())
            .unwrap_or(700.0);
        (h - 140.0).max(360.0)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        600.0
    }
}

fn fmt_span(span_min: f32) -> String {
    if !span_min.is_finite() || span_min <= 0.0 {
        "-0 s".to_string()
    } else if span_min < 1.0 {
        format!("-{:.0} s", span_min * 60.0)
    } else {
        format!("-{:.1} min", span_min)
    }
}
