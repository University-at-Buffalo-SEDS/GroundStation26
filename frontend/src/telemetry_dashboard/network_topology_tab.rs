use dioxus::prelude::*;
use dioxus_signals::Signal;
use std::collections::{BTreeMap, HashMap, HashSet};

const GRAPH_VIEWPORT_ID: &str = "network-topology-viewport";
const GRAPH_SURFACE_ID: &str = "network-topology-surface";
const GRAPH_CANVAS_ID: &str = "network-topology-canvas";
const GRAPH_VIEWPORT_FULLSCREEN_ID: &str = "network-topology-viewport-fullscreen";
const GRAPH_SURFACE_FULLSCREEN_ID: &str = "network-topology-surface-fullscreen";
const GRAPH_CANVAS_FULLSCREEN_ID: &str = "network-topology-canvas-fullscreen";

use super::js_eval;
use super::layout::NetworkTabLayout;
use super::types::{
    NetworkTopologyLink, NetworkTopologyMsg, NetworkTopologyNode, NetworkTopologyNodeKind,
    NetworkTopologyStatus,
};

#[derive(Clone, Copy)]
struct NodePlacement {
    x: i32,
    y: i32,
    size: i32,
}

#[derive(Clone)]
struct GraphLayout {
    width: i32,
    height: i32,
    placements: HashMap<String, NodePlacement>,
}

const GRAPH_MIN_WIDTH: i32 = 1080;
const GRAPH_MIN_HEIGHT: i32 = 720;
const EMBEDDED_GRAPH_MIN_HEIGHT: i32 = 520;
const ZOOM_MIN: f32 = 0.12;
const ZOOM_MAX: f32 = 2.2;
const ZOOM_STEP: f32 = 0.2;

#[component]
pub fn NetworkTopologyTab(
    topology: Signal<NetworkTopologyMsg>,
    layout: NetworkTabLayout,
) -> Element {
    let snapshot = topology.read().clone();
    let expanded_node_id = use_signal(|| None::<String>);
    let mut is_fullscreen = use_signal(|| false);
    let title = layout
        .title
        .unwrap_or_else(|| "Network Topology".to_string());
    let visible_node_ids = snapshot
        .nodes
        .iter()
        .filter(|node| {
            !matches!(
                node.kind,
                NetworkTopologyNodeKind::Endpoint | NetworkTopologyNodeKind::Side
            )
        })
        .map(|node| node.id.as_str())
        .collect::<HashSet<_>>();
    let graph_nodes = snapshot
        .nodes
        .iter()
        .filter(|node| visible_node_ids.contains(node.id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let graph_links = snapshot
        .links
        .iter()
        .filter(|link| {
            visible_node_ids.contains(link.source.as_str())
                && visible_node_ids.contains(link.target.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    let graph_layout = compute_graph_layout(&graph_nodes, &graph_links);
    let endpoint_rows = collect_endpoint_rows(&snapshot.nodes);
    let viewport_id = if *is_fullscreen.read() {
        GRAPH_VIEWPORT_FULLSCREEN_ID
    } else {
        GRAPH_VIEWPORT_ID
    };
    let surface_id = if *is_fullscreen.read() {
        GRAPH_SURFACE_FULLSCREEN_ID
    } else {
        GRAPH_SURFACE_ID
    };
    let canvas_id = if *is_fullscreen.read() {
        GRAPH_CANVAS_FULLSCREEN_ID
    } else {
        GRAPH_CANVAS_ID
    };

    {
        let is_fullscreen = is_fullscreen;
        use_effect(move || {
            let fullscreen = *is_fullscreen.read();
            let viewport_id = if fullscreen {
                GRAPH_VIEWPORT_FULLSCREEN_ID
            } else {
                GRAPH_VIEWPORT_ID
            };
            let surface_id = if fullscreen {
                GRAPH_SURFACE_FULLSCREEN_ID
            } else {
                GRAPH_SURFACE_ID
            };
            let canvas_id = if fullscreen {
                GRAPH_CANVAS_FULLSCREEN_ID
            } else {
                GRAPH_CANVAS_ID
            };
            install_drag_handlers(
                fullscreen,
                viewport_id,
                surface_id,
                canvas_id,
                graph_layout.width,
                graph_layout.height,
            );
        });
    }

    let fullscreen_state = *is_fullscreen.read();

    let on_toggle_fullscreen = move |_| {
        let next = !*is_fullscreen.read();
        is_fullscreen.set(next);
        let viewport_id = if next {
            GRAPH_VIEWPORT_FULLSCREEN_ID
        } else {
            GRAPH_VIEWPORT_ID
        };
        let surface_id = if next {
            GRAPH_SURFACE_FULLSCREEN_ID
        } else {
            GRAPH_SURFACE_ID
        };
        let canvas_id = if next {
            GRAPH_CANVAS_FULLSCREEN_ID
        } else {
            GRAPH_CANVAS_ID
        };
        let graph_width = graph_layout.width;
        let graph_height = graph_layout.height;
        spawn(async move {
            #[cfg(target_arch = "wasm32")]
            gloo_timers::future::TimeoutFuture::new(20).await;

            #[cfg(not(target_arch = "wasm32"))]
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            install_drag_handlers(
                next,
                viewport_id,
                surface_id,
                canvas_id,
                graph_width,
                graph_height,
            );
        });
    };

    rsx! {
        if *is_fullscreen.read() {
            div {
                key: "network-fullscreen-{fullscreen_state}",
                style: "position:fixed; inset:0; z-index:9999; padding:16px; background:rgba(2, 6, 23, 0.96); display:flex; flex-direction:column; gap:12px;",
                div {
                    style: "display:flex; align-items:center; gap:12px; flex-wrap:wrap; justify-content:space-between;",
                    h2 { style: "margin:0; color:#8b5cf6;", "{title}" }
                    div {
                        style: "display:flex; align-items:center; gap:10px; color:#cbd5e1; flex-wrap:wrap;",
                        button {
                            style: zoom_button_style(),
                            onclick: move |_| graph_zoom_delta(-ZOOM_STEP),
                            "Zoom Out"
                        }
                        button {
                            style: zoom_button_style(),
                            onclick: move |_| graph_zoom_reset(),
                            "Reset"
                        }
                        button {
                            style: zoom_button_style(),
                            onclick: move |_| graph_zoom_delta(ZOOM_STEP),
                            "Zoom In"
                        }
                        button {
                            style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                            onclick: on_toggle_fullscreen,
                            "Exit Fullscreen"
                        }
                    }
                }
                p {
                    style: "margin:0; color:#94a3b8; font-size:0.95rem;",
                    if snapshot.simulated {
                        "Topology graph is running in testing-mode simulation."
                    } else {
                        "Topology graph is built from backend topology and live node/link status."
                    }
                }
                div {
                    style: "flex:1; min-height:0; border:1px solid #334155; border-radius:20px; background:radial-gradient(circle at top, #122033 0%, #0b1220 45%, #020617 100%); overflow:auto; cursor:grab; user-select:none; touch-action:none; overscroll-behavior:contain; box-shadow:0 24px 60px rgba(0,0,0,0.45);",
                    id: "{viewport_id}",
                    div {
                        id: "{surface_id}",
                        style: "position:relative; width:{graph_layout.width}px; height:{graph_layout.height}px; min-width:{graph_layout.width}px; min-height:{graph_layout.height}px;",
                        div {
                            id: "{canvas_id}",
                            style: "position:absolute; inset:0 auto auto 0; width:{graph_layout.width}px; height:{graph_layout.height}px; transform:scale(1); transform-origin:top left;",
                            svg {
                                width: "{graph_layout.width}",
                                height: "{graph_layout.height}",
                                view_box: "0 0 {graph_layout.width} {graph_layout.height}",
                                style: "position:absolute; inset:0; overflow:visible;",
                                for link in graph_links.iter() {
                                    {render_link(link, &snapshot.nodes, &graph_layout.placements)}
                                }
                            }

                            for node in graph_nodes.iter() {
                                {render_node(
                                    node,
                                    &graph_links,
                                    &snapshot.nodes,
                                    &graph_layout.placements,
                                    graph_layout.width,
                                    expanded_node_id,
                                )}
                            }
                        }
                    }
                }
            }
        } else {
            div {
                key: "network-embedded-{fullscreen_state}",
                style: "padding:10px 14px 14px 14px; display:flex; flex-direction:column; gap:12px; height:100%; min-height:{EMBEDDED_GRAPH_MIN_HEIGHT}px; overflow-y:auto;",
                h2 { style: "margin:0; color:#e5e7eb;", "{title}" }
                p {
                    style: "margin:0; color:#94a3b8; font-size:0.95rem;",
                    if snapshot.simulated {
                        "Router graph is running in testing-mode simulation."
                    } else {
                        "Router graph is built from the backend SEDSprintf topology and live board/link status."
                    }
                }
                div {
                    style: "display:flex; align-items:center; gap:10px; color:#cbd5e1; flex-wrap:wrap;",
                    button {
                        style: zoom_button_style(),
                        onclick: move |_| graph_zoom_delta(-ZOOM_STEP),
                        "Zoom Out"
                    }
                    button {
                        style: zoom_button_style(),
                        onclick: move |_| graph_zoom_reset(),
                        "Reset"
                    }
                    button {
                        style: zoom_button_style(),
                        onclick: move |_| graph_zoom_delta(ZOOM_STEP),
                        "Zoom In"
                    }
                    button {
                        style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                        onclick: on_toggle_fullscreen,
                        "Fullscreen"
                    }
                    span {
                        style: "font-size:0.85rem; color:#94a3b8;",
                        "Pinch or drag to navigate"
                    }
                }

                div {
                    id: "{viewport_id}",
                    style: "padding:8px; border:1px solid #334155; border-radius:18px; background:radial-gradient(circle at top, #122033 0%, #0b1220 45%, #020617 100%); overflow:auto; min-height:{EMBEDDED_GRAPH_MIN_HEIGHT}px; max-height:calc(100vh - 260px); cursor:grab; user-select:none; touch-action:none; overscroll-behavior:contain;",
                    div {
                        id: "{surface_id}",
                        style: "position:relative; width:{graph_layout.width}px; height:{graph_layout.height}px; min-width:{graph_layout.width}px; min-height:{graph_layout.height}px;",
                        div {
                            id: "{canvas_id}",
                            style: "position:absolute; inset:0 auto auto 0; width:{graph_layout.width}px; height:{graph_layout.height}px; transform:scale(1); transform-origin:top left;",
                            svg {
                                width: "{graph_layout.width}",
                                height: "{graph_layout.height}",
                                view_box: "0 0 {graph_layout.width} {graph_layout.height}",
                                style: "position:absolute; inset:0; overflow:visible;",
                                for link in graph_links.iter() {
                                    {render_link(link, &snapshot.nodes, &graph_layout.placements)}
                                }
                            }

                            for node in graph_nodes.iter() {
                                {render_node(
                                    node,
                                    &graph_links,
                                    &snapshot.nodes,
                                    &graph_layout.placements,
                                    graph_layout.width,
                                    expanded_node_id,
                                )}
                            }
                        }
                    }
                }
                {render_endpoint_section(&endpoint_rows)}
            }
        }
    }
}

fn render_endpoint_section(endpoint_rows: &[(String, Vec<String>)]) -> Element {
    rsx! {
        div {
            style: "border:1px solid #334155; border-radius:18px; background:#07121f; padding:14px;",
            h3 { style: "margin:0 0 10px 0; color:#e5e7eb;", "Endpoint Ownership" }
            p {
                style: "margin:0 0 12px 0; color:#94a3b8; font-size:0.9rem;",
                "One row per endpoint, with every board or local node that currently owns or advertises it."
            }
            table { style: "width:100%; border-collapse:collapse; font-size:0.92rem;",
                thead {
                    tr {
                        th { style: topology_th_style(), "Endpoint" }
                        th { style: topology_th_style(), "Owners" }
                    }
                }
                tbody {
                    for (endpoint, owners) in endpoint_rows.iter() {
                        tr {
                            td { style: topology_td_style(true), "{endpoint}" }
                            td { style: topology_td_style(false), "{owners.join(\", \")}" }
                        }
                    }
                }
            }
        }
    }
}

fn zoom_button_style() -> &'static str {
    "padding:6px 10px; border-radius:10px; border:1px solid #334155; background:#0f172a; color:#e2e8f0; font-size:0.82rem; cursor:pointer;"
}

fn graph_zoom_delta(delta: f32) {
    js_eval(&format!(
        r#"
        (function() {{
          if (typeof window.__gs26NetworkGraphZoomDelta === "function") {{
            window.__gs26NetworkGraphZoomDelta({delta});
          }}
        }})();
        "#
    ));
}

fn graph_zoom_reset() {
    js_eval(
        r#"
        (function() {
          if (typeof window.__gs26NetworkGraphZoomReset === "function") {
            window.__gs26NetworkGraphZoomReset();
          }
        })();
        "#,
    );
}

fn install_drag_handlers(
    _fullscreen: bool,
    viewport_id: &str,
    surface_id: &str,
    canvas_id: &str,
    graph_width: i32,
    graph_height: i32,
) {
    js_eval(&format!(
        r#"
        (function() {{
          const viewport = document.getElementById({viewport_id:?});
          const surface = document.getElementById({surface_id:?});
          const canvas = document.getElementById({canvas_id:?});
          if (!viewport || !surface || !canvas) return;
          const state = window.__gs26NetworkGraphState || {{
            scale: 1.0,
            drag: null,
            suppressNextClick: false,
            pointers: new Map(),
            pinchDistance: null,
            pinchScale: 1.0,
            padX: 0,
            padY: 0,
            autoFitted: false,
            listenersInstalled: false,
          }};
          state.viewport = viewport;
          state.surface = surface;
          state.canvas = canvas;
          window.__gs26NetworkGraphState = state;

          const setCursor = (value) => {{
            state.viewport.style.cursor = value;
          }};

          const clamp = (value, min, max) => Math.max(min, Math.min(max, value));
          const distance = (a, b) => Math.hypot(a.x - b.x, a.y - b.y);
          const fitScale = () => {{
            const availW = Math.max(state.viewport.clientWidth - 120, 240);
            const availH = Math.max(state.viewport.clientHeight - 120, 240);
            return clamp(Math.min(availW / {graph_width}, availH / {graph_height}), {zoom_min}, {zoom_max});
          }};
          const refreshSurfaceFrame = () => {{
            const scaledWidth = Math.round({graph_width} * state.scale);
            const scaledHeight = Math.round({graph_height} * state.scale);
            state.padX = Math.max(Math.round((state.viewport.clientWidth - scaledWidth) / 2), 90);
            state.padY = Math.max(Math.round((state.viewport.clientHeight - scaledHeight) / 2), 60);
            state.surface.style.width = `${{scaledWidth + state.padX * 2}}px`;
            state.surface.style.height = `${{scaledHeight + state.padY * 2}}px`;
            state.surface.style.minWidth = state.surface.style.width;
            state.surface.style.minHeight = state.surface.style.height;
            state.canvas.style.left = `${{state.padX}}px`;
            state.canvas.style.top = `${{state.padY}}px`;
          }};
          const centerGraph = () => {{
            const scaledWidth = Math.round({graph_width} * state.scale);
            const scaledHeight = Math.round({graph_height} * state.scale);
            state.viewport.scrollLeft = Math.max(0, state.padX + Math.round((scaledWidth - state.viewport.clientWidth) / 2));
            state.viewport.scrollTop = Math.max(0, state.padY + Math.round((scaledHeight - state.viewport.clientHeight) / 2));
          }};
          const applyScale = (nextScale, clientX, clientY) => {{
            const scale = clamp(nextScale, {zoom_min}, {zoom_max});
            const rect = state.viewport.getBoundingClientRect();
            const localX = clientX - rect.left;
            const localY = clientY - rect.top;
            const contentX = (state.viewport.scrollLeft + localX - state.padX) / state.scale;
            const contentY = (state.viewport.scrollTop + localY - state.padY) / state.scale;
            state.scale = scale;
            state.canvas.style.transform = `scale(${{scale}})`;
            refreshSurfaceFrame();
            state.viewport.scrollLeft = Math.max(0, contentX * scale + state.padX - localX);
            state.viewport.scrollTop = Math.max(0, contentY * scale + state.padY - localY);
          }};

          window.__gs26NetworkGraphZoomDelta = (delta) => {{
            const rect = state.viewport.getBoundingClientRect();
            applyScale(state.scale + delta, rect.left + rect.width / 2, rect.top + rect.height / 2);
          }};

          window.__gs26NetworkGraphZoomReset = () => {{
            state.scale = fitScale();
            state.canvas.style.transform = `scale(${{state.scale}})`;
            refreshSurfaceFrame();
            centerGraph();
          }};

          window.__gs26NetworkGraphRefresh = () => {{
            refreshSurfaceFrame();
            centerGraph();
          }};

          state.scale = fitScale();
          state.canvas.style.transform = `scale(${{state.scale}})`;
          refreshSurfaceFrame();
          centerGraph();
          window.requestAnimationFrame(() => {{
            if (typeof window.__gs26NetworkGraphRefresh === "function") {{
              window.__gs26NetworkGraphRefresh();
            }}
          }});
          window.setTimeout(() => {{
            if (typeof window.__gs26NetworkGraphRefresh === "function") {{
              window.__gs26NetworkGraphRefresh();
            }}
          }}, 60);
          if (state.listenersInstalled) return;
          state.listenersInstalled = true;

          window.addEventListener("resize", () => {{
            state.scale = fitScale();
            state.canvas.style.transform = `scale(${{state.scale}})`;
            refreshSurfaceFrame();
            centerGraph();
          }});

          document.addEventListener("pointerdown", (evt) => {{
            if (evt.target !== state.surface && !state.surface.contains(evt.target)) return;
            const target = evt.target;
            if (target && typeof target.closest === "function" && target.closest("button")) {{
              return;
            }}
            if (target && typeof target.closest === "function" && target.closest("[data-network-node='true'], [data-network-panel='true']")) {{
              return;
            }}
            state.pointers.set(evt.pointerId, {{ x: evt.clientX, y: evt.clientY }});
            state.suppressNextClick = false;
            if (state.pointers.size === 1) {{
              state.drag = {{
                x: evt.clientX,
                y: evt.clientY,
                moved: false,
              }};
            }} else if (state.pointers.size === 2) {{
              const [a, b] = Array.from(state.pointers.values());
              state.drag = null;
              state.pinchDistance = distance(a, b);
              state.pinchScale = state.scale;
            }}
            try {{
              state.surface.setPointerCapture(evt.pointerId);
            }} catch (_err) {{}}
            setCursor("grabbing");
          }});

          window.addEventListener("pointermove", (evt) => {{
            if (!state.pointers.has(evt.pointerId)) return;
            state.pointers.set(evt.pointerId, {{ x: evt.clientX, y: evt.clientY }});
            if (state.pointers.size >= 2) {{
              const [a, b] = Array.from(state.pointers.values());
              const nextDistance = distance(a, b);
              if (state.pinchDistance && nextDistance > 0) {{
                const centerX = (a.x + b.x) / 2;
                const centerY = (a.y + b.y) / 2;
                applyScale(state.pinchScale * (nextDistance / state.pinchDistance), centerX, centerY);
                state.suppressNextClick = true;
              }}
              evt.preventDefault();
              return;
            }}
            if (!state.drag) return;
            const dx = state.drag.x - evt.clientX;
            const dy = state.drag.y - evt.clientY;
            state.viewport.scrollLeft += dx;
            state.viewport.scrollTop += dy;
            state.drag = {{
              x: evt.clientX,
              y: evt.clientY,
              moved: state.drag.moved || Math.abs(dx) > 2 || Math.abs(dy) > 2,
            }};
            evt.preventDefault();
          }}, {{ passive: false }});

          window.addEventListener("pointerup", (evt) => {{
            if (!state.pointers.has(evt.pointerId)) return;
            const dragged = !!(state.drag && state.drag.moved);
            state.suppressNextClick = state.suppressNextClick || dragged;
            state.pointers.delete(evt.pointerId);
            if (state.pointers.size === 1) {{
              const [remaining] = Array.from(state.pointers.values());
              state.drag = {{
                x: remaining.x,
                y: remaining.y,
                moved: true,
              }};
              state.pinchDistance = null;
              state.pinchScale = state.scale;
            }} else if (state.pointers.size === 0) {{
              state.drag = null;
              state.pinchDistance = null;
              state.pinchScale = state.scale;
            }}
            setCursor("grab");
            try {{
              state.surface.releasePointerCapture(evt.pointerId);
            }} catch (_err) {{}}
          }});

          document.addEventListener("click", (evt) => {{
            if (evt.target !== state.surface && !state.surface.contains(evt.target)) return;
            if (!state.suppressNextClick) return;
            state.suppressNextClick = false;
            evt.preventDefault();
            evt.stopPropagation();
          }}, true);
        }})();
        "#,
        viewport_id = viewport_id,
        surface_id = surface_id,
        canvas_id = canvas_id,
        zoom_min = ZOOM_MIN,
        zoom_max = ZOOM_MAX,
        graph_width = graph_width,
        graph_height = graph_height,
    ));
}

fn collect_endpoint_rows(nodes: &[NetworkTopologyNode]) -> Vec<(String, Vec<String>)> {
    let mut by_endpoint = BTreeMap::<String, Vec<String>>::new();
    for node in nodes {
        let Some(owner) = endpoint_owner_label(node) else {
            continue;
        };
        for endpoint in &node.endpoints {
            by_endpoint
                .entry(endpoint.clone())
                .or_default()
                .push(owner.clone());
        }
    }

    by_endpoint
        .into_iter()
        .map(|(endpoint, mut owners)| {
            owners.sort();
            owners.dedup();
            (endpoint, owners)
        })
        .collect()
}

fn endpoint_owner_label(node: &NetworkTopologyNode) -> Option<String> {
    match node.kind {
        NetworkTopologyNodeKind::Router => Some(node.label.clone()),
        NetworkTopologyNodeKind::Board => Some(node.label.clone()),
        NetworkTopologyNodeKind::Endpoint | NetworkTopologyNodeKind::Side => None,
    }
}

fn render_link(
    link: &NetworkTopologyLink,
    nodes: &[NetworkTopologyNode],
    placements: &HashMap<String, NodePlacement>,
) -> Element {
    let Some(source) = placement_for(&link.source, placements) else {
        return rsx! { g {} };
    };
    let Some(target) = placement_for(&link.target, placements) else {
        return rsx! { g {} };
    };
    let (stroke, glow, dash) = link_style(link.status);
    let stroke = link_color(link, stroke);
    let glow = link_color(link, glow);
    let source_label = node_label(&link.source, nodes);
    let target_label = node_label(&link.target, nodes);

    rsx! {
        g {
            line {
                x1: "{source.x}",
                y1: "{source.y}",
                x2: "{target.x}",
                y2: "{target.y}",
                stroke: "{glow}",
                stroke_width: "10",
                stroke_opacity: "0.15",
                stroke_linecap: "round",
            }
            line {
                x1: "{source.x}",
                y1: "{source.y}",
                x2: "{target.x}",
                y2: "{target.y}",
                stroke: "{stroke}",
                stroke_width: "3",
                stroke_dasharray: "{dash}",
                stroke_linecap: "round",
            }
            title { "{source_label} -> {target_label}" }
        }
    }
}

fn render_node(
    node: &NetworkTopologyNode,
    links: &[NetworkTopologyLink],
    nodes: &[NetworkTopologyNode],
    placements: &HashMap<String, NodePlacement>,
    graph_width: i32,
    expanded_node_id: Signal<Option<String>>,
) -> Element {
    let Some(placement) = placement_for(&node.id, placements) else {
        return rsx! { div {} };
    };
    let (ring, bg, fg, chip_bg, chip_fg, status_label) = node_style(node.status);
    let neighbors = neighbor_labels(node, links, nodes);
    let is_expanded = expanded_node_id
        .read()
        .as_ref()
        .map(|id| id == &node.id)
        .unwrap_or(false);
    let kind = match node.kind {
        NetworkTopologyNodeKind::Router => "Router",
        NetworkTopologyNodeKind::Endpoint => "Endpoint",
        NetworkTopologyNodeKind::Side => "Side",
        NetworkTopologyNodeKind::Board => "Board",
    };
    let outline = if is_expanded {
        "3px solid rgba(255,255,255,0.18)"
    } else {
        "none"
    };
    let panel_left = if placement.x > (graph_width / 2) {
        "auto"
    } else {
        "calc(100% + 14px)"
    };
    let panel_right = if placement.x > (graph_width / 2) {
        "calc(100% + 14px)"
    } else {
        "auto"
    };
    let node_z_index = if is_expanded { "20" } else { "2" };

    rsx! {
        div {
            "data-network-node": "true",
            style: "position:absolute; left:{placement.x}px; top:{placement.y}px; width:{placement.size}px; height:{placement.size}px; transform:translate(-50%, -50%); \
                    border-radius:999px; border:2px solid {ring}; background:{bg}; color:{fg}; box-shadow:0 24px 50px rgba(2, 6, 23, 0.48); \
                    display:flex; flex-direction:column; align-items:center; justify-content:center; text-align:center; padding:14px; gap:6px; cursor:pointer; \
                    outline:{outline}; z-index:{node_z_index};",
            onclick: {
                let node_id = node.id.clone();
                let mut expanded_node_id = expanded_node_id;
                move |_| {
                    let next = match expanded_node_id.read().as_ref() {
                        Some(current) if current == &node_id => None,
                        _ => Some(node_id.clone()),
                    };
                    expanded_node_id.set(next);
                }
            },
            div { style: "font-size:0.95rem; font-weight:800; line-height:1.1;", "{node.label}" }
            if let Some(sender_id) = &node.sender_id {
                div { style: "font-size:0.72rem; color:#93c5fd; text-transform:uppercase; letter-spacing:0.08em;", "{sender_id}" }
            } else {
                div { style: "font-size:0.72rem; color:#94a3b8; text-transform:uppercase; letter-spacing:0.08em;", "{kind}" }
            }
            span {
                style: "padding:2px 8px; border-radius:999px; background:{chip_bg}; color:{chip_fg}; font-size:0.7rem; font-weight:700;",
                "{status_label}"
            }
            div {
                style: "font-size:0.68rem; color:#94a3b8; max-width:100%; line-height:1.2;",
                if node.endpoints.is_empty() {
                    "Tap for details"
                } else {
                    "{node.endpoints.len()} endpoint(s)"
                }
            }
            if is_expanded {
                div {
                    "data-network-panel": "true",
                    style: "position:absolute; left:{panel_left}; right:{panel_right}; top:50%; transform:translateY(-50%); width:240px; padding:12px 14px; border-radius:14px; \
                            border:1px solid #334155; background:#020617; box-shadow:0 20px 40px rgba(2, 6, 23, 0.55); z-index:4; text-align:left;",
                    div { style: "font-size:0.73rem; color:#94a3b8; text-transform:uppercase; letter-spacing:0.08em;", "{kind} details" }
                    div { style: "font-size:0.95rem; color:#e2e8f0; font-weight:700; margin:4px 0 10px 0;", "{node.label}" }
                    div { style: "font-size:0.73rem; color:#94a3b8; text-transform:uppercase; letter-spacing:0.08em; margin-bottom:8px;", "Connected to" }
                    if neighbors.is_empty() {
                        div { style: "font-size:0.82rem; color:#64748b; margin-bottom:12px;", "No active links." }
                    } else {
                        div { style: "display:flex; flex-wrap:wrap; gap:6px; margin-bottom:12px;",
                            for neighbor in neighbors.iter() {
                                span {
                                    style: "padding:4px 8px; border-radius:999px; background:rgba(15, 23, 42, 0.6); border:1px solid rgba(148, 163, 184, 0.22); color:#cbd5e1; font-size:0.72rem;",
                                    "{neighbor}"
                                }
                            }
                        }
                    }
                    div { style: "font-size:0.73rem; color:#94a3b8; text-transform:uppercase; letter-spacing:0.08em; margin-bottom:8px;", "Endpoints" }
                    if node.endpoints.is_empty() {
                        div { style: "font-size:0.82rem; color:#64748b;", "No discovered endpoints for this node." }
                    } else {
                        div { style: "display:flex; flex-direction:column; gap:6px; max-height:240px; overflow-y:auto; padding-right:4px;",
                            for endpoint in node.endpoints.iter() {
                                div {
                                    style: "padding:6px 8px; border-radius:10px; border:1px solid #1f2937; background:#0b1220; color:#dbeafe; font-size:0.8rem;",
                                    "{endpoint}"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn placement_for(id: &str, placements: &HashMap<String, NodePlacement>) -> Option<NodePlacement> {
    placements.get(id).copied()
}

fn node_label(id: &str, nodes: &[NetworkTopologyNode]) -> String {
    nodes
        .iter()
        .find(|node| node.id == id)
        .map(|node| node.label.clone())
        .unwrap_or_else(|| id.to_string())
}

fn neighbor_labels(
    node: &NetworkTopologyNode,
    links: &[NetworkTopologyLink],
    nodes: &[NetworkTopologyNode],
) -> Vec<String> {
    let mut labels = Vec::new();
    for link in links {
        let other = if link.source == node.id {
            Some(link.target.as_str())
        } else if link.target == node.id {
            Some(link.source.as_str())
        } else {
            None
        };
        if let Some(other) = other {
            labels.push(node_label(other, nodes));
        }
    }
    labels.sort();
    labels.dedup();
    if labels.len() > 4 {
        let remaining = labels.len() - 4;
        labels.truncate(4);
        labels.push(format!("+{remaining} more"));
    }
    labels
}

fn link_style(status: NetworkTopologyStatus) -> (&'static str, &'static str, &'static str) {
    match status {
        NetworkTopologyStatus::Online => ("#38bdf8", "#67e8f9", ""),
        NetworkTopologyStatus::Offline => ("#ef4444", "#fca5a5", "8 8"),
        NetworkTopologyStatus::Simulated => ("#8b5cf6", "#c4b5fd", "14 10"),
    }
}

fn link_color(link: &NetworkTopologyLink, default: &'static str) -> &'static str {
    let _ = link;
    default
}

fn topology_th_style() -> &'static str {
    "text-align:left; color:#8fb3c9; border-bottom:1px solid #243447; padding:8px 6px;"
}

fn topology_td_style(mono: bool) -> &'static str {
    if mono {
        "padding:8px 6px; border-bottom:1px solid #132738; color:#dbe7f3; font-family: ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,monospace;"
    } else {
        "padding:8px 6px; border-bottom:1px solid #132738; color:#dbe7f3;"
    }
}

fn node_style(
    status: NetworkTopologyStatus,
) -> (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
) {
    match status {
        NetworkTopologyStatus::Online => (
            "#22c55e",
            "radial-gradient(circle at 30% 30%, #14532d 0%, #0b1220 72%)",
            "#dcfce7",
            "rgba(34, 197, 94, 0.18)",
            "#bbf7d0",
            "Online",
        ),
        NetworkTopologyStatus::Offline => (
            "#ef4444",
            "radial-gradient(circle at 30% 30%, #4c0519 0%, #0b1220 72%)",
            "#fee2e2",
            "rgba(239, 68, 68, 0.18)",
            "#fecaca",
            "Offline",
        ),
        NetworkTopologyStatus::Simulated => (
            "#8b5cf6",
            "radial-gradient(circle at 30% 30%, #312e81 0%, #0b1220 72%)",
            "#ede9fe",
            "rgba(139, 92, 246, 0.18)",
            "#ddd6fe",
            "Simulated",
        ),
    }
}

fn node_size(kind: NetworkTopologyNodeKind) -> i32 {
    match kind {
        NetworkTopologyNodeKind::Router => 220,
        NetworkTopologyNodeKind::Side => 144,
        NetworkTopologyNodeKind::Board => 164,
        NetworkTopologyNodeKind::Endpoint => 120,
    }
}

fn estimated_label_lines(node: &NetworkTopologyNode) -> i32 {
    let chars_per_line = match node.kind {
        NetworkTopologyNodeKind::Router => 18,
        NetworkTopologyNodeKind::Side => 14,
        NetworkTopologyNodeKind::Board => 13,
        NetworkTopologyNodeKind::Endpoint => 12,
    };
    let mut lines = 1_i32;
    let mut current = 0_i32;
    for word in node.label.split_whitespace() {
        let word_len = word.chars().count() as i32;
        if current == 0 {
            current = word_len;
            continue;
        }
        if current + 1 + word_len > chars_per_line {
            lines += 1;
            current = word_len;
        } else {
            current += 1 + word_len;
        }
    }
    lines.max(1)
}

fn node_diameter(node: &NetworkTopologyNode) -> i32 {
    let base = node_size(node.kind);
    let extra_label_height = (estimated_label_lines(node) - 2).max(0) * 18;
    let endpoint_extra = if node.endpoints.is_empty() { 0 } else { 10 };
    let sender_extra = if node.sender_id.is_some() { 0 } else { 6 };
    base + extra_label_height + endpoint_extra + sender_extra
}

fn stack_height(nodes: &[&NetworkTopologyNode], node_gap: i32) -> i32 {
    if nodes.is_empty() {
        return 0;
    }
    nodes.iter().map(|node| node_diameter(node)).sum::<i32>() + node_gap * (nodes.len() as i32 - 1)
}

fn compute_graph_layout(
    nodes: &[NetworkTopologyNode],
    links: &[NetworkTopologyLink],
) -> GraphLayout {
    let horizontal_gap = 320_i32;
    let node_gap = 40_i32;
    let margin_x = 160_i32;
    let margin_y = 120_i32;

    if nodes.is_empty() {
        return GraphLayout {
            width: GRAPH_MIN_WIDTH,
            height: GRAPH_MIN_HEIGHT,
            placements: HashMap::new(),
        };
    }

    let mut adjacency = HashMap::<String, Vec<String>>::new();
    let mut indegree = HashMap::<String, usize>::new();
    let mut kind_by_id = HashMap::<String, NetworkTopologyNodeKind>::new();

    for node in nodes {
        adjacency.entry(node.id.clone()).or_default();
        indegree.entry(node.id.clone()).or_insert(0);
        kind_by_id.insert(node.id.clone(), node.kind);
    }
    for link in links {
        adjacency
            .entry(link.source.clone())
            .or_default()
            .push(link.target.clone());
        adjacency
            .entry(link.target.clone())
            .or_default()
            .push(link.source.clone());
        *indegree.entry(link.target.clone()).or_insert(0) += 1;
    }

    let mut roots = nodes
        .iter()
        .filter(|node| indegree.get(&node.id).copied().unwrap_or(0) == 0)
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        roots = nodes
            .iter()
            .filter(|node| node.kind == NetworkTopologyNodeKind::Router)
            .map(|node| node.id.clone())
            .collect();
    }
    if roots.is_empty() {
        roots.push(nodes[0].id.clone());
    }

    let mut layer_map = HashMap::<String, usize>::new();
    let mut queue = std::collections::VecDeque::<String>::new();
    for root in roots {
        if layer_map.insert(root.clone(), 0).is_none() {
            queue.push_back(root);
        }
    }
    while let Some(node_id) = queue.pop_front() {
        let current_layer = layer_map.get(&node_id).copied().unwrap_or(0);
        if let Some(neighbors) = adjacency.get(&node_id) {
            for neighbor in neighbors {
                let next_layer = current_layer + 1;
                let update = match layer_map.get(neighbor) {
                    Some(existing) => next_layer < *existing,
                    None => true,
                };
                if update {
                    layer_map.insert(neighbor.clone(), next_layer);
                    queue.push_back(neighbor.clone());
                }
            }
        }
    }

    let mut unplaced = nodes
        .iter()
        .filter(|node| !layer_map.contains_key(&node.id))
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    while let Some(id) = unplaced.pop() {
        let start_layer = layer_map.values().copied().max().unwrap_or(0) + 1;
        layer_map.insert(id.clone(), start_layer);
        queue.push_back(id.clone());
        while let Some(node_id) = queue.pop_front() {
            let current_layer = layer_map.get(&node_id).copied().unwrap_or(start_layer);
            if let Some(neighbors) = adjacency.get(&node_id) {
                for neighbor in neighbors {
                    if layer_map.contains_key(neighbor) {
                        continue;
                    }
                    layer_map.insert(neighbor.clone(), current_layer + 1);
                    queue.push_back(neighbor.clone());
                }
            }
        }
        unplaced.retain(|node_id| !layer_map.contains_key(node_id));
    }

    let root_router_ids = nodes
        .iter()
        .filter(|node| node.kind == NetworkTopologyNodeKind::Router)
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();

    let mut branch_roots = nodes
        .iter()
        .filter(|node| node.kind == NetworkTopologyNodeKind::Side)
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();

    if branch_roots.is_empty() {
        branch_roots = root_router_ids
            .iter()
            .flat_map(|router_id| {
                adjacency
                    .get(router_id)
                    .into_iter()
                    .flat_map(|neighbors| neighbors.iter())
                    .filter(|neighbor| *neighbor != router_id)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect();
        branch_roots.sort();
        branch_roots.dedup();
    }

    let branch_root_set = branch_roots
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let root_router_set = root_router_ids
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let mut branch_index_by_node = HashMap::<String, usize>::new();

    for (branch_idx, branch_root) in branch_roots.iter().enumerate() {
        let mut queue = std::collections::VecDeque::<String>::new();
        queue.push_back(branch_root.clone());
        branch_index_by_node
            .entry(branch_root.clone())
            .or_insert(branch_idx);
        while let Some(node_id) = queue.pop_front() {
            if let Some(neighbors) = adjacency.get(&node_id) {
                for neighbor in neighbors {
                    if root_router_set.contains(neighbor) {
                        continue;
                    }
                    if branch_root_set.contains(neighbor) && neighbor != branch_root {
                        continue;
                    }
                    if branch_index_by_node.contains_key(neighbor) {
                        continue;
                    }
                    branch_index_by_node.insert(neighbor.clone(), branch_idx);
                    queue.push_back(neighbor.clone());
                }
            }
        }
    }

    let max_layer = layer_map.values().copied().max().unwrap_or(0);
    let mut layers = vec![Vec::<&NetworkTopologyNode>::new(); max_layer + 1];
    for node in nodes {
        let layer = layer_map.get(&node.id).copied().unwrap_or(0);
        layers[layer].push(node);
    }

    for layer_nodes in &mut layers {
        layer_nodes.sort_by(|a, b| {
            let kind_rank = |kind: NetworkTopologyNodeKind| match kind {
                NetworkTopologyNodeKind::Router => 0,
                NetworkTopologyNodeKind::Side => 1,
                NetworkTopologyNodeKind::Board => 2,
                NetworkTopologyNodeKind::Endpoint => 3,
            };
            kind_rank(a.kind)
                .cmp(&kind_rank(b.kind))
                .then_with(|| a.label.cmp(&b.label))
        });
    }

    let branch_count = branch_roots.len().max(1) as i32;
    let max_branch_stack_height = layers
        .iter()
        .map(|layer_nodes| {
            let mut counts = HashMap::<Option<usize>, Vec<&NetworkTopologyNode>>::new();
            for node in layer_nodes {
                let branch = branch_index_by_node.get(&node.id).copied();
                counts.entry(branch).or_default().push(*node);
            }
            counts
                .values()
                .map(|branch_nodes| stack_height(branch_nodes, node_gap))
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
        .max(220);
    let branch_gap = max_branch_stack_height + 96;
    let content_height = ((branch_count - 1).max(0) * branch_gap) + max_branch_stack_height + 120;
    let total_height = (content_height + margin_y * 2).max(GRAPH_MIN_HEIGHT);
    let mut placements = HashMap::<String, NodePlacement>::new();
    let graph_center_y = total_height / 2;
    let branch_center_offset = (branch_count - 1) as f32 / 2.0;

    for (layer_idx, layer_nodes) in layers.iter().enumerate() {
        if layer_nodes.is_empty() {
            continue;
        }

        let mut by_branch = HashMap::<Option<usize>, Vec<&NetworkTopologyNode>>::new();
        for node in layer_nodes {
            let branch = branch_index_by_node.get(&node.id).copied();
            by_branch.entry(branch).or_default().push(*node);
        }

        let mut branch_keys = by_branch.keys().copied().collect::<Vec<_>>();
        branch_keys.sort_by(|a, b| match (a, b) {
            (Some(x), Some(y)) => x.cmp(y),
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });

        for branch_key in branch_keys {
            let Some(branch_nodes) = by_branch.get_mut(&branch_key) else {
                continue;
            };
            branch_nodes.sort_by(|a, b| a.label.cmp(&b.label));
            let branch_center_y = match branch_key {
                Some(branch_idx) => {
                    graph_center_y
                        + ((branch_idx as f32 - branch_center_offset) * branch_gap as f32) as i32
                }
                None => graph_center_y,
            };
            let stack_height = stack_height(branch_nodes, node_gap);
            let mut cursor_y = branch_center_y - (stack_height / 2);

            for node in branch_nodes.iter() {
                let size = node_diameter(node);
                let x = margin_x + layer_idx as i32 * horizontal_gap;
                let y = cursor_y + (size / 2);
                placements.insert(
                    node.id.clone(),
                    NodePlacement {
                        x,
                        y,
                        size,
                    },
                );
                cursor_y += size + node_gap;
            }
        }
    }

    let rightmost = placements
        .values()
        .map(|placement| placement.x + placement.size / 2)
        .max()
        .unwrap_or(GRAPH_MIN_WIDTH - margin_x);
    let width = (rightmost + margin_x).max(GRAPH_MIN_WIDTH);

    GraphLayout {
        width,
        height: total_height,
        placements,
    }
}
