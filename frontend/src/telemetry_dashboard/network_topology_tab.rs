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

#[derive(Clone, Copy)]
struct GraphViewportFocus {
    center_x: i32,
    center_y: i32,
    min_x: i32,
    max_x: i32,
    min_y: i32,
    max_y: i32,
    left_extent: i32,
    right_extent: i32,
    top_extent: i32,
    bottom_extent: i32,
}

const GRAPH_MIN_WIDTH: i32 = 1080;
const GRAPH_MIN_HEIGHT: i32 = 720;
const EMBEDDED_GRAPH_MIN_HEIGHT: i32 = 520;
const ZOOM_MIN: f32 = 0.12;
const ZOOM_MAX: f32 = 2.2;
const ZOOM_STEP: f32 = 0.2;
const GRAPH_LINK_CHANNEL_COLOR: &str = "#243447";

fn graph_viewport_style(min_height_px: i32, max_height: Option<&str>, fullscreen: bool) -> String {
    let size_constraints = if fullscreen {
        "flex:1; min-height:0;".to_string()
    } else {
        let mut style = format!("min-height:{min_height_px}px;");
        if let Some(max_height) = max_height {
            style.push_str(&format!(" max-height:{max_height};"));
        }
        style
    };
    format!(
        "{size_constraints} border:1px solid #334155; border-radius:20px; background:radial-gradient(circle at top, #122033 0%, #0b1220 45%, #020617 100%); overflow:auto; cursor:grab; user-select:none; touch-action:none; overscroll-behavior:contain; scrollbar-width:none; -ms-overflow-style:none; box-shadow:0 24px 60px rgba(0,0,0,0.45);"
    )
}

#[component]
pub fn NetworkTopologyTab(
    topology: Signal<NetworkTopologyMsg>,
    layout: NetworkTabLayout,
    flow_animation_enabled: bool,
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
    let graph_links = collapse_visible_links(&snapshot.nodes, &snapshot.links, &visible_node_ids);
    let graph_layout = compute_graph_layout(&graph_nodes, &graph_links);
    let router_placement = graph_nodes
        .iter()
        .find(|node| node.kind == NetworkTopologyNodeKind::Router)
        .and_then(|node| graph_layout.placements.get(&node.id).copied());
    let graph_bounds = graph_layout
        .placements
        .values()
        .fold(None::<(i32, i32, i32, i32)>, |acc, placement| {
            let left = placement.x - placement.size / 2;
            let right = placement.x + placement.size / 2;
            let top = placement.y - placement.size / 2;
            let bottom = placement.y + placement.size / 2;
            match acc {
                Some((min_x, max_x, min_y, max_y)) => Some((
                    min_x.min(left),
                    max_x.max(right),
                    min_y.min(top),
                    max_y.max(bottom),
                )),
                None => Some((left, right, top, bottom)),
            }
        })
        .unwrap_or((0, graph_layout.width, 0, graph_layout.height));
    let (bound_min_x, bound_max_x, bound_min_y, bound_max_y) = graph_bounds;
    let render_width = (bound_max_x - bound_min_x).max(1);
    let render_height = (bound_max_y - bound_min_y).max(1);
    let render_placements = graph_layout
        .placements
        .iter()
        .map(|(id, placement)| {
            (
                id.clone(),
                NodePlacement {
                    x: placement.x - bound_min_x,
                    y: placement.y - bound_min_y,
                    size: placement.size,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    let viewport_focus = router_placement.map(|router| {
        let router_radius = router.size / 2;
        GraphViewportFocus {
            center_x: router.x - bound_min_x,
            center_y: router.y - bound_min_y,
            min_x: 0,
            max_x: bound_max_x - bound_min_x,
            min_y: 0,
            max_y: bound_max_y - bound_min_y,
            left_extent: (router.x - bound_min_x).max(router_radius),
            right_extent: (bound_max_x - router.x).max(router_radius),
            top_extent: (router.y - bound_min_y).max(router_radius),
            bottom_extent: (bound_max_y - router.y).max(router_radius),
        }
    });
    let endpoint_rows = collect_endpoint_rows(&snapshot.nodes, &snapshot.links);
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
                render_width,
                render_height,
                viewport_focus,
            );
            graph_zoom_reset();
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
        let graph_width = render_width;
        let graph_height = render_height;
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
                viewport_focus,
            );
            graph_zoom_reset();
        });
    };

    rsx! {
        style {
            {r#"
            #network-topology-viewport::-webkit-scrollbar,
            #network-topology-viewport-fullscreen::-webkit-scrollbar {
                display: none;
                width: 0;
                height: 0;
            }
            "#}
        }
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
                    style: "{graph_viewport_style(EMBEDDED_GRAPH_MIN_HEIGHT, None, true)}",
                    id: "{viewport_id}",
                    div {
                            id: "{surface_id}",
                        style: "position:relative; width:{render_width}px; height:{render_height}px; min-width:{render_width}px; min-height:{render_height}px;",
                        div {
                            id: "{canvas_id}",
                            style: "position:absolute; inset:0 auto auto 0; width:{render_width}px; height:{render_height}px; transform:scale(1); transform-origin:top left;",
                            svg {
                                width: "{render_width}",
                                height: "{render_height}",
                                view_box: "0 0 {render_width} {render_height}",
                                style: "position:absolute; inset:0; overflow:visible;",
                                for link in graph_links.iter() {
                                    {render_link(link, &snapshot.nodes, &render_placements, flow_animation_enabled)}
                                }
                            }

                            for node in graph_nodes.iter() {
                                {render_node(
                                    node,
                                    &graph_links,
                                    &snapshot.nodes,
                                    &render_placements,
                                    render_width,
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
                    style: "padding:8px; {graph_viewport_style(EMBEDDED_GRAPH_MIN_HEIGHT, Some(\"calc(var(--gs26-app-height) - 260px)\"), false)}",
                    div {
                        id: "{surface_id}",
                        style: "position:relative; width:{render_width}px; height:{render_height}px; min-width:{render_width}px; min-height:{render_height}px;",
                        div {
                            id: "{canvas_id}",
                            style: "position:absolute; inset:0 auto auto 0; width:{render_width}px; height:{render_height}px; transform:scale(1); transform-origin:top left;",
                            svg {
                                width: "{render_width}",
                                height: "{render_height}",
                                view_box: "0 0 {render_width} {render_height}",
                                style: "position:absolute; inset:0; overflow:visible;",
                                for link in graph_links.iter() {
                                    {render_link(link, &snapshot.nodes, &render_placements, flow_animation_enabled)}
                                }
                            }

                            for node in graph_nodes.iter() {
                                {render_node(
                                    node,
                                    &graph_links,
                                    &snapshot.nodes,
                                    &render_placements,
                                    render_width,
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
    viewport_focus: Option<GraphViewportFocus>,
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
            canvasLeft: 0,
            canvasTop: 0,
            padLeft: 0,
            padRight: 0,
            padTop: 0,
            padBottom: 0,
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

          const withinSurface = (target) => {{
            return target === state.surface || state.surface.contains(target);
          }};

          const clamp = (value, min, max) => Math.max(min, Math.min(max, value));
          const distance = (a, b) => Math.hypot(a.x - b.x, a.y - b.y);
          const focus = {viewport_focus};
          const fitScale = () => {{
            const marginX = Math.min(64, Math.max(24, state.viewport.clientWidth * 0.06));
            const marginY = Math.min(64, Math.max(24, state.viewport.clientHeight * 0.06));
            const availW = Math.max(state.viewport.clientWidth - marginX * 2, 240);
            const availH = Math.max(state.viewport.clientHeight - marginY * 2, 240);
            if (!focus) {{
              return clamp(Math.min(availW / {graph_width}, availH / {graph_height}) * 0.92, {zoom_min}, {zoom_max});
            }}
            const fitFromCenter = Math.min(
              (availW / 2) / Math.max(focus.left_extent, focus.right_extent, 1),
              (availH / 2) / Math.max(focus.top_extent, focus.bottom_extent, 1),
            );
            return clamp(fitFromCenter * 0.95, {zoom_min}, {zoom_max});
          }};
          const refreshSurfaceFrame = () => {{
            const minX = focus ? focus.min_x : 0;
            const maxX = focus ? focus.max_x : {graph_width};
            const minY = focus ? focus.min_y : 0;
            const maxY = focus ? focus.max_y : {graph_height};
            const scaledWidth = Math.round((maxX - minX) * state.scale);
            const scaledHeight = Math.round((maxY - minY) * state.scale);
            const basePadX = Math.max(Math.round((state.viewport.clientWidth - scaledWidth) / 2), 48);
            const basePadY = Math.max(Math.round((state.viewport.clientHeight - scaledHeight) / 2), 36);
            if (focus) {{
              state.padLeft = Math.max(basePadX, Math.ceil(state.viewport.clientWidth / 2 - (focus.center_x - focus.min_x) * state.scale + 24));
              state.padRight = Math.max(basePadX, Math.ceil(state.viewport.clientWidth / 2 - (focus.max_x - focus.center_x) * state.scale + 24));
              state.padTop = Math.max(basePadY, Math.ceil(state.viewport.clientHeight / 2 - (focus.center_y - focus.min_y) * state.scale + 24));
              state.padBottom = Math.max(basePadY, Math.ceil(state.viewport.clientHeight / 2 - (focus.max_y - focus.center_y) * state.scale + 24));
            }} else {{
              state.padLeft = basePadX;
              state.padRight = basePadX;
              state.padTop = basePadY;
              state.padBottom = basePadY;
            }}
            state.surface.style.width = `${{scaledWidth + state.padLeft + state.padRight}}px`;
            state.surface.style.height = `${{scaledHeight + state.padTop + state.padBottom}}px`;
            state.surface.style.minWidth = state.surface.style.width;
            state.surface.style.minHeight = state.surface.style.height;
            state.canvasLeft = Math.round(state.padLeft - minX * state.scale);
            state.canvasTop = Math.round(state.padTop - minY * state.scale);
            state.canvas.style.left = `${{state.canvasLeft}}px`;
            state.canvas.style.top = `${{state.canvasTop}}px`;
          }};
          const setViewportScroll = (left, top) => {{
            const maxLeft = Math.max(0, state.viewport.scrollWidth - state.viewport.clientWidth);
            const maxTop = Math.max(0, state.viewport.scrollHeight - state.viewport.clientHeight);
            state.viewport.scrollLeft = clamp(left, 0, maxLeft);
            state.viewport.scrollTop = clamp(top, 0, maxTop);
          }};
          const centerGraph = () => {{
            if (focus) {{
              const localX = state.viewport.clientWidth / 2;
              const localY = state.viewport.clientHeight / 2;
              setViewportScroll(
                (focus.center_x - focus.min_x) * state.scale + state.padLeft - localX,
                (focus.center_y - focus.min_y) * state.scale + state.padTop - localY,
              );
              return;
            }}
            const scaledWidth = Math.round({graph_width} * state.scale);
            const scaledHeight = Math.round({graph_height} * state.scale);
            setViewportScroll(
              state.padLeft + Math.round((scaledWidth - state.viewport.clientWidth) / 2),
              state.padTop + Math.round((scaledHeight - state.viewport.clientHeight) / 2),
            );
          }};
          const applyScale = (nextScale, clientX, clientY) => {{
            const scale = clamp(nextScale, {zoom_min}, {zoom_max});
            const rect = state.viewport.getBoundingClientRect();
            const localX = clientX - rect.left;
            const localY = clientY - rect.top;
            const contentX = (state.viewport.scrollLeft + localX - state.canvasLeft) / state.scale;
            const contentY = (state.viewport.scrollTop + localY - state.canvasTop) / state.scale;
            state.scale = scale;
            state.canvas.style.transform = `scale(${{scale}})`;
            refreshSurfaceFrame();
            setViewportScroll(
              contentX * scale + state.canvasLeft - localX,
              contentY * scale + state.canvasTop - localY,
            );
          }};

          const zoomFromWheel = (evt) => {{
            const delta = Number(evt.deltaY || 0);
            if (!Number.isFinite(delta) || Math.abs(delta) < 0.01) return;
            const intensity = evt.ctrlKey ? 0.0035 : 0.0018;
            const nextScale = state.scale * Math.exp(-delta * intensity);
            applyScale(nextScale, evt.clientX, evt.clientY);
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
          const fitAndCenterGraph = () => {{
            state.viewport.scrollLeft = 0;
            state.viewport.scrollTop = 0;
            state.scale = fitScale();
            state.canvas.style.transform = `scale(${{state.scale}})`;
            refreshSurfaceFrame();
            window.requestAnimationFrame(() => {{
              refreshSurfaceFrame();
              centerGraph();
              window.requestAnimationFrame(() => {{
                refreshSurfaceFrame();
                centerGraph();
              }});
            }});
          }};

          fitAndCenterGraph();
          if (typeof window.__gs26NetworkGraphZoomReset === "function") {{
            window.__gs26NetworkGraphZoomReset();
          }}
          window.requestAnimationFrame(() => {{
            if (typeof window.__gs26NetworkGraphRefresh === "function") {{
              window.__gs26NetworkGraphRefresh();
            }}
            if (typeof window.__gs26NetworkGraphZoomReset === "function") {{
              window.__gs26NetworkGraphZoomReset();
            }}
          }});
          window.setTimeout(() => {{
            if (typeof window.__gs26NetworkGraphRefresh === "function") {{
              window.__gs26NetworkGraphRefresh();
            }}
            if (typeof window.__gs26NetworkGraphZoomReset === "function") {{
              window.__gs26NetworkGraphZoomReset();
            }}
          }}, 60);
          window.setTimeout(() => {{
            fitAndCenterGraph();
            if (typeof window.__gs26NetworkGraphZoomReset === "function") {{
              window.__gs26NetworkGraphZoomReset();
            }}
          }}, 140);
          if (state.listenersInstalled) return;
          state.listenersInstalled = true;

          window.addEventListener("resize", () => {{
            fitAndCenterGraph();
          }});

          document.addEventListener("wheel", (evt) => {{
            if (!withinSurface(evt.target)) return;
            const target = evt.target;
            if (target && typeof target.closest === "function" && target.closest("button")) {{
              return;
            }}
            zoomFromWheel(evt);
            evt.preventDefault();
          }}, {{ passive: false }});

          document.addEventListener("pointerdown", (evt) => {{
            if (!withinSurface(evt.target)) return;
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
            if (!withinSurface(evt.target)) return;
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
        viewport_focus = viewport_focus
            .map(|focus| format!(
                "{{ center_x: {}, center_y: {}, min_x: {}, max_x: {}, min_y: {}, max_y: {}, left_extent: {}, right_extent: {}, top_extent: {}, bottom_extent: {} }}",
                focus.center_x,
                focus.center_y,
                focus.min_x,
                focus.max_x,
                focus.min_y,
                focus.max_y,
                focus.left_extent,
                focus.right_extent,
                focus.top_extent,
                focus.bottom_extent
            ))
            .unwrap_or_else(|| "null".to_string()),
    ));
}

fn collect_endpoint_rows(
    nodes: &[NetworkTopologyNode],
    links: &[NetworkTopologyLink],
) -> Vec<(String, Vec<String>)> {
    let mut by_endpoint = BTreeMap::<String, Vec<String>>::new();
    let mut adjacency = BTreeMap::<String, Vec<String>>::new();
    for link in links {
        adjacency
            .entry(link.source.clone())
            .or_default()
            .push(link.target.clone());
        adjacency
            .entry(link.target.clone())
            .or_default()
            .push(link.source.clone());
    }

    for node in nodes {
        for endpoint in &node.endpoints {
            if let Some(owner) = endpoint_owner_label(node, endpoint) {
                by_endpoint
                    .entry(endpoint.clone())
                    .or_default()
                    .push(owner.clone());
            }
        }

        if node.kind != NetworkTopologyNodeKind::Endpoint {
            continue;
        }

        let endpoint_name = node
            .endpoints
            .first()
            .cloned()
            .unwrap_or_else(|| node.label.clone());
        for owner in endpoint_route_owners(node, nodes, &adjacency, &endpoint_name) {
            by_endpoint
                .entry(endpoint_name.clone())
                .or_default()
                .push(owner);
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

fn endpoint_route_owners(
    endpoint_node: &NetworkTopologyNode,
    nodes: &[NetworkTopologyNode],
    adjacency: &BTreeMap<String, Vec<String>>,
    endpoint_name: &str,
) -> Vec<String> {
    let mut owners = Vec::new();
    let mut queue = std::collections::VecDeque::<String>::new();
    let mut visited = HashSet::<String>::new();
    visited.insert(endpoint_node.id.clone());

    if let Some(neighbors) = adjacency.get(&endpoint_node.id) {
        for neighbor in neighbors {
            queue.push_back(neighbor.clone());
        }
    }

    while let Some(current) = queue.pop_front() {
        if !visited.insert(current.clone()) {
            continue;
        }
        let Some(node) = nodes.iter().find(|node| node.id == current) else {
            continue;
        };
        if let Some(owner) = endpoint_owner_label(node, endpoint_name) {
            owners.push(owner);
            continue;
        }
        if let Some(neighbors) = adjacency.get(&current) {
            for neighbor in neighbors {
                if !visited.contains(neighbor) {
                    queue.push_back(neighbor.clone());
                }
            }
        }
    }

    owners.sort();
    owners.dedup();
    owners
}

fn endpoint_owner_label(node: &NetworkTopologyNode, endpoint_name: &str) -> Option<String> {
    match node.kind {
        NetworkTopologyNodeKind::Router | NetworkTopologyNodeKind::Board
            if node
                .endpoints
                .iter()
                .any(|endpoint| endpoint == endpoint_name) =>
        {
            Some(node.label.clone())
        }
        NetworkTopologyNodeKind::Endpoint | NetworkTopologyNodeKind::Side => None,
        _ => None,
    }
}

fn render_link(
    link: &NetworkTopologyLink,
    nodes: &[NetworkTopologyNode],
    placements: &HashMap<String, NodePlacement>,
    flow_animation_enabled: bool,
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
    let animated = flow_animation_enabled && !matches!(link.status, NetworkTopologyStatus::Offline);
    let dx = (target.x - source.x) as f32;
    let dy = (target.y - source.y) as f32;
    let len = (dx * dx + dy * dy).sqrt().max(1.0);
    let nx = -dy / len;
    let ny = dx / len;
    let lane_offset = if len < 220.0 { 2.2 } else { 2.4 };
    let lane1_x1 = source.x as f32 + nx * lane_offset;
    let lane1_y1 = source.y as f32 + ny * lane_offset;
    let lane1_x2 = target.x as f32 + nx * lane_offset;
    let lane1_y2 = target.y as f32 + ny * lane_offset;
    let lane2_x1 = source.x as f32 - nx * lane_offset;
    let lane2_y1 = source.y as f32 - ny * lane_offset;
    let lane2_x2 = target.x as f32 - nx * lane_offset;
    let lane2_y2 = target.y as f32 - ny * lane_offset;
    let upload_color = match link.status {
        NetworkTopologyStatus::Online => "#38bdf8",
        NetworkTopologyStatus::Offline => "#ef4444",
        NetworkTopologyStatus::Simulated => "#8b5cf6",
    };
    let download_color = match link.status {
        NetworkTopologyStatus::Online => "#22c55e",
        NetworkTopologyStatus::Offline => "#f87171",
        NetworkTopologyStatus::Simulated => "#c084fc",
    };
    let lane_dash = if matches!(link.status, NetworkTopologyStatus::Simulated) {
        "12 16"
    } else {
        "10 18"
    };
    let tooltip = format!(
        "{source_label} -> {target_label}: upload lane\n{target_label} -> {source_label}: download lane"
    );

    rsx! {
        g {
            if !animated {
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
            }
            if !animated {
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
            }
            if animated {
                g {
                    line {
                        x1: "{source.x}",
                        y1: "{source.y}",
                        x2: "{target.x}",
                        y2: "{target.y}",
                        stroke: "{glow}",
                        stroke_width: "8.5",
                        stroke_opacity: "0.12",
                        stroke_linecap: "round",
                    }
                    line {
                        x1: "{source.x}",
                        y1: "{source.y}",
                        x2: "{target.x}",
                        y2: "{target.y}",
                        stroke: "{GRAPH_LINK_CHANNEL_COLOR}",
                        stroke_width: "6",
                        stroke_opacity: "1.0",
                        stroke_linecap: "round",
                    }
                    line {
                        x1: "{lane1_x1}",
                        y1: "{lane1_y1}",
                        x2: "{lane1_x2}",
                        y2: "{lane1_y2}",
                        stroke: "{upload_color}",
                        stroke_width: "2.5",
                        stroke_dasharray: "{lane_dash}",
                        stroke_linecap: "round",
                        stroke_opacity: "0.92",
                        animate {
                            attribute_name: "stroke-dashoffset",
                            from: "0",
                            to: "-28",
                            dur: if matches!(link.status, NetworkTopologyStatus::Simulated) { "1.6s" } else { "1.1s" },
                            repeat_count: "indefinite",
                        }
                        animate {
                            attribute_name: "stroke-opacity",
                            values: "0.35;0.95;0.35",
                            dur: if matches!(link.status, NetworkTopologyStatus::Simulated) { "1.8s" } else { "1.2s" },
                            repeat_count: "indefinite",
                        }
                    }
                    line {
                        x1: "{lane2_x1}",
                        y1: "{lane2_y1}",
                        x2: "{lane2_x2}",
                        y2: "{lane2_y2}",
                        stroke: "{download_color}",
                        stroke_width: "2.5",
                        stroke_dasharray: "{lane_dash}",
                        stroke_linecap: "round",
                        stroke_opacity: "0.92",
                        animate {
                            attribute_name: "stroke-dashoffset",
                            from: "-28",
                            to: "0",
                            dur: if matches!(link.status, NetworkTopologyStatus::Simulated) { "1.8s" } else { "1.25s" },
                            repeat_count: "indefinite",
                        }
                        animate {
                            attribute_name: "stroke-opacity",
                            values: "0.35;0.95;0.35",
                            dur: if matches!(link.status, NetworkTopologyStatus::Simulated) { "2.0s" } else { "1.35s" },
                            repeat_count: "indefinite",
                        }
                    }
                }
            }
            title { "{tooltip}" }
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

fn collapse_visible_links(
    nodes: &[NetworkTopologyNode],
    links: &[NetworkTopologyLink],
    visible_node_ids: &HashSet<&str>,
) -> Vec<NetworkTopologyLink> {
    let visible_nodes = nodes
        .iter()
        .filter(|node| visible_node_ids.contains(node.id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let Some(router) = visible_nodes
        .iter()
        .find(|node| node.kind == NetworkTopologyNodeKind::Router)
    else {
        return Vec::new();
    };

    let mut collapsed = BTreeMap::<(String, String), NetworkTopologyStatus>::new();

    for link in links {
        if !visible_node_ids.contains(link.source.as_str())
            || !visible_node_ids.contains(link.target.as_str())
        {
            continue;
        }
        let key = ordered_link_key(link.source.clone(), link.target.clone());
        collapsed
            .entry(key)
            .and_modify(|existing| *existing = merge_link_status(*existing, link.status))
            .or_insert(link.status);
    }

    let side_by_board = board_side_ids(nodes, links);
    let relay_by_side = relay_board_ids(nodes, &side_by_board);

    for node in visible_nodes
        .iter()
        .filter(|node| node.kind == NetworkTopologyNodeKind::Board)
    {
        let already_connected = collapsed
            .keys()
            .any(|(source, target)| source == &node.id || target == &node.id);
        if already_connected {
            continue;
        }

        let Some(side_id) = side_by_board.get(&node.id) else {
            let key = ordered_link_key(router.id.clone(), node.id.clone());
            collapsed.entry(key).or_insert(node.status);
            continue;
        };

        if let Some(relay_id) = relay_by_side.get(side_id) {
            let relay_status = nodes
                .iter()
                .find(|candidate| candidate.id == *relay_id)
                .map(|relay| relay.status)
                .unwrap_or(node.status);

            let router_key = ordered_link_key(router.id.clone(), relay_id.clone());
            collapsed
                .entry(router_key)
                .and_modify(|existing| *existing = merge_link_status(*existing, relay_status))
                .or_insert(relay_status);

            if relay_id != &node.id {
                let branch_key = ordered_link_key(relay_id.clone(), node.id.clone());
                collapsed
                    .entry(branch_key)
                    .and_modify(|existing| *existing = merge_link_status(*existing, node.status))
                    .or_insert(node.status);
            }
        } else {
            let key = ordered_link_key(router.id.clone(), node.id.clone());
            collapsed
                .entry(key)
                .and_modify(|existing| *existing = merge_link_status(*existing, node.status))
                .or_insert(node.status);
        }
    }

    collapsed
        .into_iter()
        .map(|((source, target), status)| NetworkTopologyLink {
            source,
            target,
            label: None,
            status,
        })
        .collect()
}

fn merge_link_status(a: NetworkTopologyStatus, b: NetworkTopologyStatus) -> NetworkTopologyStatus {
    use NetworkTopologyStatus::{Offline, Online, Simulated};

    match (a, b) {
        (Offline, _) | (_, Offline) => Offline,
        (Simulated, _) | (_, Simulated) => Simulated,
        _ => Online,
    }
}

fn ordered_link_key(a: String, b: String) -> (String, String) {
    if a < b { (a, b) } else { (b, a) }
}

fn board_side_ids(
    nodes: &[NetworkTopologyNode],
    links: &[NetworkTopologyLink],
) -> HashMap<String, String> {
    let side_ids = nodes
        .iter()
        .filter(|node| node.kind == NetworkTopologyNodeKind::Side)
        .map(|node| node.id.clone())
        .collect::<HashSet<_>>();
    let mut out = HashMap::new();
    for link in links {
        if side_ids.contains(&link.source) {
            out.insert(link.target.clone(), link.source.clone());
        } else if side_ids.contains(&link.target) {
            out.insert(link.source.clone(), link.target.clone());
        }
    }
    out
}

fn relay_board_ids(
    nodes: &[NetworkTopologyNode],
    side_by_board: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for node in nodes
        .iter()
        .filter(|node| node.kind == NetworkTopologyNodeKind::Board)
    {
        let sender = node.sender_id.as_deref();
        if !matches!(sender, Some("RF") | Some("GW")) {
            continue;
        }
        let Some(side_id) = side_by_board.get(&node.id) else {
            continue;
        };
        out.insert(side_id.clone(), node.id.clone());
    }
    out
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
    for node in nodes {
        adjacency.entry(node.id.clone()).or_default();
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
    }

    let root_id = nodes
        .iter()
        .find(|node| node.kind == NetworkTopologyNodeKind::Router)
        .map(|node| node.id.clone())
        .unwrap_or_else(|| nodes[0].id.clone());

    let mut layer_map = HashMap::<String, usize>::new();
    let mut first_hop_by_node = HashMap::<String, Option<String>>::new();
    let mut queue = std::collections::VecDeque::<String>::new();
    layer_map.insert(root_id.clone(), 0);
    first_hop_by_node.insert(root_id.clone(), None);
    queue.push_back(root_id.clone());

    while let Some(node_id) = queue.pop_front() {
        let current_layer = layer_map.get(&node_id).copied().unwrap_or(0);
        let current_first_hop = first_hop_by_node.get(&node_id).cloned().unwrap_or(None);
        if let Some(neighbors) = adjacency.get(&node_id) {
            for neighbor in neighbors {
                if layer_map.contains_key(neighbor) {
                    continue;
                }
                let next_layer = current_layer + 1;
                let next_first_hop = if node_id == root_id {
                    Some(neighbor.clone())
                } else {
                    current_first_hop.clone()
                };
                layer_map.insert(neighbor.clone(), next_layer);
                first_hop_by_node.insert(neighbor.clone(), next_first_hop);
                queue.push_back(neighbor.clone());
            }
        }
    }

    let mut extra_roots = nodes
        .iter()
        .filter(|node| !layer_map.contains_key(&node.id))
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();

    for (branch_idx, id) in extra_roots.drain(..).enumerate() {
        let synthetic_root = format!("__detached_{branch_idx}_{}", id);
        let start_layer = 1;
        layer_map.insert(id.clone(), start_layer);
        first_hop_by_node.insert(id.clone(), Some(synthetic_root.clone()));
        queue.push_back(id.clone());
        while let Some(node_id) = queue.pop_front() {
            let current_layer = layer_map.get(&node_id).copied().unwrap_or(start_layer);
            let current_first_hop = first_hop_by_node
                .get(&node_id)
                .cloned()
                .unwrap_or(Some(synthetic_root.clone()));
            if let Some(neighbors) = adjacency.get(&node_id) {
                for neighbor in neighbors {
                    if layer_map.contains_key(neighbor) {
                        continue;
                    }
                    layer_map.insert(neighbor.clone(), current_layer + 1);
                    first_hop_by_node.insert(neighbor.clone(), current_first_hop.clone());
                    queue.push_back(neighbor.clone());
                }
            }
        }
    }

    let mut branch_roots = first_hop_by_node
        .values()
        .filter_map(|value| value.clone())
        .collect::<Vec<_>>();
    branch_roots.sort();
    branch_roots.dedup();
    let mut branch_index_by_node = HashMap::<String, usize>::new();

    for (branch_idx, branch_root) in branch_roots.iter().enumerate() {
        for node in nodes {
            if first_hop_by_node
                .get(&node.id)
                .and_then(|value| value.as_ref())
                == Some(branch_root)
            {
                branch_index_by_node.insert(node.id.clone(), branch_idx);
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
            let branch = if node.id == root_id {
                None
            } else {
                branch_index_by_node.get(&node.id).copied()
            };
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
                placements.insert(node.id.clone(), NodePlacement { x, y, size });
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
