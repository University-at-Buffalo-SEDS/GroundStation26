use dioxus::prelude::*;
use dioxus_signals::Signal;
use std::collections::{HashMap, HashSet};

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

const GRAPH_WIDTH: i32 = 1320;
const GRAPH_HEIGHT: i32 = 700;
const EMBEDDED_GRAPH_MIN_HEIGHT: i32 = 360;
const ZOOM_MIN: f32 = 0.25;
const ZOOM_MAX: f32 = 1.8;
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
        .unwrap_or_else(|| "SEDSprintf Network".to_string());
    let placements = graph_positions();
    let visible_node_ids = snapshot
        .nodes
        .iter()
        .filter(|node| !matches!(node.kind, NetworkTopologyNodeKind::Endpoint))
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
            install_drag_handlers(fullscreen, viewport_id, surface_id, canvas_id);
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
        spawn(async move {
            #[cfg(target_arch = "wasm32")]
            gloo_timers::future::TimeoutFuture::new(20).await;

            #[cfg(not(target_arch = "wasm32"))]
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            install_drag_handlers(next, viewport_id, surface_id, canvas_id);
        });
    };

    rsx! {
        if *is_fullscreen.read() {
            div {
                key: "network-fullscreen-{fullscreen_state}",
                style: "position:fixed; inset:0; z-index:9999; padding:16px; background:#020617; display:flex; flex-direction:column; gap:12px;",
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
                        "Router graph is running in testing-mode simulation."
                    } else {
                        "Router graph is built from the backend SEDSprintf topology and live board/link status."
                    }
                }
                div {
                    style: "flex:1; min-height:0; border:1px solid #334155; border-radius:18px; background:radial-gradient(circle at top, #122033 0%, #0b1220 45%, #020617 100%); overflow:auto; cursor:grab; user-select:none; touch-action:none; overscroll-behavior:contain;",
                    id: "{viewport_id}",
                    div {
                        id: "{surface_id}",
                        style: "position:relative; width:{GRAPH_WIDTH}px; height:{GRAPH_HEIGHT}px; min-width:{GRAPH_WIDTH}px; min-height:{GRAPH_HEIGHT}px;",
                        div {
                            id: "{canvas_id}",
                            style: "position:absolute; inset:0 auto auto 0; width:{GRAPH_WIDTH}px; height:{GRAPH_HEIGHT}px; transform:scale(1); transform-origin:top left;",
                            svg {
                                width: "{GRAPH_WIDTH}",
                                height: "{GRAPH_HEIGHT}",
                                view_box: "0 0 {GRAPH_WIDTH} {GRAPH_HEIGHT}",
                                style: "position:absolute; inset:0; overflow:visible;",
                                for link in graph_links.iter() {
                                    {render_link(link, &snapshot.nodes, &placements)}
                                }
                            }

                            for node in graph_nodes.iter() {
                                {render_node(
                                    node,
                                    &graph_links,
                                    &snapshot.nodes,
                                    &placements,
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
                style: "padding:10px 14px 14px 14px; display:flex; flex-direction:column; gap:10px; height:100%; min-height:{EMBEDDED_GRAPH_MIN_HEIGHT}px; overflow-y:auto;",
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
                    style: "padding:8px; border:1px solid #334155; border-radius:18px; background:radial-gradient(circle at top, #122033 0%, #0b1220 45%, #020617 100%); overflow:auto; min-height:{EMBEDDED_GRAPH_MIN_HEIGHT}px; cursor:grab; user-select:none; touch-action:none; overscroll-behavior:contain;",
                    div {
                        id: "{surface_id}",
                        style: "position:relative; width:{GRAPH_WIDTH}px; height:{GRAPH_HEIGHT}px; min-width:{GRAPH_WIDTH}px; min-height:{GRAPH_HEIGHT}px;",
                        div {
                            id: "{canvas_id}",
                            style: "position:absolute; inset:0 auto auto 0; width:{GRAPH_WIDTH}px; height:{GRAPH_HEIGHT}px; transform:scale(1); transform-origin:top left;",
                            svg {
                                width: "{GRAPH_WIDTH}",
                                height: "{GRAPH_HEIGHT}",
                                view_box: "0 0 {GRAPH_WIDTH} {GRAPH_HEIGHT}",
                                style: "position:absolute; inset:0; overflow:visible;",
                                for link in graph_links.iter() {
                                    {render_link(link, &snapshot.nodes, &placements)}
                                }
                            }

                            for node in graph_nodes.iter() {
                                {render_node(
                                    node,
                                    &graph_links,
                                    &snapshot.nodes,
                                    &placements,
                                    expanded_node_id,
                                )}
                            }
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

fn install_drag_handlers(_fullscreen: bool, viewport_id: &str, surface_id: &str, canvas_id: &str) {
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
          const refreshSurfaceFrame = () => {{
            const scaledWidth = Math.round({graph_width} * state.scale);
            const scaledHeight = Math.round({graph_height} * state.scale);
            state.padX = Math.max(Math.round(state.viewport.clientWidth * 0.35), 180);
            state.padY = Math.max(Math.round(state.viewport.clientHeight * 0.18), 60);
            state.surface.style.width = `${{scaledWidth + state.padX * 2}}px`;
            state.surface.style.height = `${{scaledHeight + state.padY * 2}}px`;
            state.surface.style.minWidth = state.surface.style.width;
            state.surface.style.minHeight = state.surface.style.height;
            state.canvas.style.left = `${{state.padX}}px`;
            state.canvas.style.top = `${{state.padY}}px`;
          }};
          const centerGraph = () => {{
            const scaledWidth = Math.round({graph_width} * state.scale);
            state.viewport.scrollLeft = Math.max(0, state.padX + Math.round((scaledWidth - state.viewport.clientWidth) / 2));
            state.viewport.scrollTop = Math.max(0, state.padY - 24);
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
            state.scale = 1.0;
            state.canvas.style.transform = "scale(1)";
            refreshSurfaceFrame();
            centerGraph();
          }};

          window.__gs26NetworkGraphRefresh = () => {{
            refreshSurfaceFrame();
            centerGraph();
          }};

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
            refreshSurfaceFrame();
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
        graph_width = GRAPH_WIDTH,
        graph_height = GRAPH_HEIGHT,
    ));
}

fn graph_positions() -> HashMap<&'static str, NodePlacement> {
    HashMap::from([
        (
            "router",
            NodePlacement {
                x: 320,
                y: 330,
                size: 220,
            },
        ),
        (
            "board_rf",
            NodePlacement {
                x: 700,
                y: 180,
                size: 146,
            },
        ),
        (
            "board_fc",
            NodePlacement {
                x: 1070,
                y: 100,
                size: 132,
            },
        ),
        (
            "board_pb",
            NodePlacement {
                x: 1070,
                y: 280,
                size: 132,
            },
        ),
        (
            "board_gw",
            NodePlacement {
                x: 700,
                y: 500,
                size: 146,
            },
        ),
        (
            "board_vb",
            NodePlacement {
                x: 1070,
                y: 420,
                size: 132,
            },
        ),
        (
            "board_daq",
            NodePlacement {
                x: 1210,
                y: 520,
                size: 132,
            },
        ),
        (
            "board_ab",
            NodePlacement {
                x: 1070,
                y: 610,
                size: 132,
            },
        ),
    ])
}

fn render_link(
    link: &NetworkTopologyLink,
    nodes: &[NetworkTopologyNode],
    placements: &HashMap<&'static str, NodePlacement>,
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
    placements: &HashMap<&'static str, NodePlacement>,
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
    let panel_left = if placement.x > (GRAPH_WIDTH / 2) {
        "auto"
    } else {
        "calc(100% + 14px)"
    };
    let panel_right = if placement.x > (GRAPH_WIDTH / 2) {
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

fn placement_for(
    id: &str,
    placements: &HashMap<&'static str, NodePlacement>,
) -> Option<NodePlacement> {
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
    match link.label.as_deref() {
        Some("rocket radio") => match link.status {
            NetworkTopologyStatus::Online => "#f59e0b",
            NetworkTopologyStatus::Offline => "#ef4444",
            NetworkTopologyStatus::Simulated => "#c084fc",
        },
        Some("umbilical radio") => match link.status {
            NetworkTopologyStatus::Online => "#14b8a6",
            NetworkTopologyStatus::Offline => "#ef4444",
            NetworkTopologyStatus::Simulated => "#c084fc",
        },
        _ => default,
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
