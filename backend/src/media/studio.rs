//! Visual stream manager: layouts (slot geometry on a 16:9 canvas), scenes
//! (a layout plus a source per slot) and the programme scene, taken with a cut
//! or a fade. Sources are the station's: cameras from the relay, the live
//! diagrams from `backend/layout/diagrams`, telemetry views, transparent
//! overlays and graphics. The programme page composes the scene in the
//! browser (docs/media/stream-manager.md), so a telemetry source is a live
//! element of the programme, not a server-side render.
//!
//! Wire schema: Seds-Ground-Station-Frontend `docs/backend-contract.md` 5.12.
use super::*;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};

pub(super) const MAX_LAYOUTS: usize = 32;
pub(super) const MAX_SCENES: usize = 24;
pub(super) const MAX_SLOTS: usize = 12;
const MIN_SLOT: f64 = 5.0;
const MAX_TRANSITION_MS: u32 = 5000;

/// Telemetry views every station offers (beside its diagrams and cameras).
/// `(source id, kind, view, label, transparent)`.
const BUILT_IN_SOURCES: &[(&str, &str, &str, &str, bool)] = &[
    ("telemetry:stats", "telemetry", "stats", "Key telemetry cards", false),
    ("telemetry:plot", "telemetry", "plot", "Telemetry plots", false),
    ("telemetry:tclock", "telemetry", "tclock", "T-clock", false),
    ("telemetry:map", "telemetry", "map", "Recovery map", false),
    ("telemetry:model", "telemetry", "model", "Rocket graphic", false),
    ("telemetry:recorded-replay", "telemetry", "recorded_replay", "Recorded flight replay", false),
    ("overlay:lower-third", "overlay", "lower_third", "T-clock + altitude lower third", true),
    ("overlay:tclock-bug", "overlay", "tclock_bug", "T-clock corner bug", true),
    ("overlay:stats-bar", "overlay", "stats_bar", "Telemetry strip", true),
    ("graphic:title", "graphic", "title", "Mission title card", false),
    ("graphic:slate", "graphic", "slate", "Stand-by slate", false),
];
/// "camera-auto:N" is the Nth visible live camera, so a default scene works
/// before anyone names a camera.
const AUTO_CAMERAS: usize = 4;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Slot {
    pub(super) id: String,
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) width: f64,
    pub(super) height: f64,
    /// Stacking order, 0 (back) to 9 (front).
    #[serde(default)]
    pub(super) z: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Layout {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) slots: Vec<Slot>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scene {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) layout_id: String,
    /// Slot id -> source id. A slot without an entry is empty (transparent).
    #[serde(default)]
    pub(super) sources: BTreeMap<String, String>,
    /// Keep the programme's telemetry bar under this scene.
    #[serde(default = "yes")]
    pub(super) program_bar: bool,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TransitionKind {
    Cut,
    Fade,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Transition {
    pub(super) kind: TransitionKind,
    pub(super) duration_ms: u32,
}

impl Default for Transition {
    fn default() -> Self {
        Self { kind: TransitionKind::Fade, duration_ms: 600 }
    }
}

/// Saved in the broadcast (`broadcast.studio`); only `/api/studio/*` writes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Studio {
    pub(super) layouts: Vec<Layout>,
    pub(super) scenes: Vec<Scene>,
    pub(super) program_scene_id: String,
    /// The transition of the last take (and the operator's default).
    pub(super) transition: Transition,
    /// Increments on every take; the programme starts a transition on change.
    pub(super) take_seq: u64,
}

fn slot(id: &str, x: f64, y: f64, width: f64, height: f64, z: u8) -> Slot {
    Slot { id: id.into(), x, y, width, height, z }
}

/// Aspect presets the layout editor also offers.
pub(super) fn preset(id: &str) -> Option<Vec<Slot>> {
    Some(match id {
        "full" => vec![slot("main", 0., 0., 100., 100., 0)],
        "pip" => vec![slot("main", 0., 0., 100., 100., 0), slot("pip", 68., 4., 28., 28., 1)],
        "side_by_side" => vec![slot("left", 0., 0., 50., 100., 0), slot("right", 50., 0., 50., 100., 0)],
        "grid_2x2" => vec![
            slot("a", 0., 0., 50., 50., 0),
            slot("b", 50., 0., 50., 50., 0),
            slot("c", 0., 50., 50., 50., 0),
            slot("d", 50., 50., 50., 50., 0),
        ],
        "lower_third" => vec![slot("main", 0., 0., 100., 100., 0), slot("lower", 0., 75., 100., 25., 2)],
        _ => return None,
    })
}

impl Default for Studio {
    fn default() -> Self {
        let layout = |id: &str, name: &str| Layout { id: id.into(), name: name.into(), slots: preset(id).unwrap() };
        let mut layouts = vec![
            layout("full", "Full frame"),
            layout("pip", "Picture in picture"),
            layout("side_by_side", "Side by side"),
            layout("grid_2x2", "2 × 2"),
            layout("lower_third", "Full + lower third"),
        ];
        layouts.push(Layout {
            id: "main_side".into(),
            name: "Main + two side".into(),
            slots: vec![slot("main", 0., 0., 66., 100., 0), slot("top", 66., 0., 34., 50., 0), slot("bottom", 66., 50., 34., 50., 0)],
        });
        let scene = |id: &str, name: &str, layout: &str, sources: &[(&str, &str)]| Scene {
            id: id.into(),
            name: name.into(),
            layout_id: layout.into(),
            sources: sources.iter().map(|(s, v)| ((*s).into(), (*v).into())).collect(),
            program_bar: true,
        };
        let scenes = vec![
            scene("pad-camera", "Pad camera", "lower_third", &[("main", "camera-auto:1"), ("lower", "overlay:lower-third")]),
            scene("cameras", "Camera grid", "grid_2x2", &[("a", "camera-auto:1"), ("b", "camera-auto:2"), ("c", "camera-auto:3"), ("d", "camera-auto:4")]),
            scene("camera-diagram", "Camera + diagram", "side_by_side", &[("left", "camera-auto:1"), ("right", "diagram:mode")]),
            scene("diagram", "Live diagram", "lower_third", &[("main", "diagram:mode"), ("lower", "overlay:lower-third")]),
            scene("telemetry", "Telemetry", "main_side", &[("main", "telemetry:plot"), ("top", "telemetry:tclock"), ("bottom", "telemetry:stats")]),
            scene("rocket", "Rocket graphic", "lower_third", &[("main", "telemetry:model"), ("lower", "overlay:lower-third")]),
            scene("interview", "Interview", "pip", &[("main", "camera-auto:1"), ("pip", "camera-auto:2")]),
            scene("map", "Recovery map", "lower_third", &[("main", "telemetry:map"), ("lower", "overlay:lower-third")]),
            scene("slate", "Stand by", "full", &[("main", "graphic:slate")]),
        ];
        Self { layouts, scenes, program_scene_id: String::new(), transition: Transition::default(), take_seq: 0 }
    }
}

/// A parsed source id. Ids are generic `kind:name`; nothing here names a
/// particular camera or diagram.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum SourceRef {
    Camera(String),
    AutoCamera(usize),
    Diagram(String),
    BuiltIn(&'static str),
}

pub(super) fn parse_source(id: &str) -> Option<SourceRef> {
    if let Some(built_in) = BUILT_IN_SOURCES.iter().find(|s| s.0 == id) {
        return Some(SourceRef::BuiltIn(built_in.0));
    }
    let (kind, name) = id.split_once(':')?;
    match kind {
        "camera" if valid_id(name) => Some(SourceRef::Camera(name.into())),
        "camera-auto" => name.parse().ok().filter(|n| (1..=8).contains(n)).map(SourceRef::AutoCamera),
        "diagram" if valid_id(name) => Some(SourceRef::Diagram(name.into())),
        _ => None,
    }
}

fn valid_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty() && trimmed == name && name.chars().count() <= 60 && !name.chars().any(char::is_control)
}

fn valid_key(id: &str) -> bool {
    valid_id(id) && id.len() <= 48
}

pub(super) fn validate_layouts(layouts: &[Layout]) -> Result<(), String> {
    if layouts.len() > MAX_LAYOUTS {
        return Err(format!("At most {MAX_LAYOUTS} layouts"));
    }
    let mut ids = HashSet::new();
    for layout in layouts {
        if !valid_key(&layout.id) || !ids.insert(layout.id.as_str()) {
            return Err(format!("Layout ID {:?} is invalid or repeated", layout.id));
        }
        if !valid_name(&layout.name) {
            return Err("Layout names are 1–60 characters".into());
        }
        if layout.slots.is_empty() || layout.slots.len() > MAX_SLOTS {
            return Err(format!("{}: a layout has 1–{MAX_SLOTS} slots", layout.name));
        }
        let mut slots = HashSet::new();
        for s in &layout.slots {
            let inside = [s.x, s.y, s.width, s.height].iter().all(|v| v.is_finite())
                && s.x >= 0.0
                && s.y >= 0.0
                && s.width >= MIN_SLOT
                && s.height >= MIN_SLOT
                && s.x + s.width <= 100.001
                && s.y + s.height <= 100.001;
            if !valid_key(&s.id) || !slots.insert(s.id.as_str()) || !inside || s.z > 9 {
                return Err(format!("{}: slot {:?} is outside the canvas, smaller than 5 % or repeated", layout.name, s.id));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_scenes(scenes: &[Scene], layouts: &[Layout]) -> Result<(), String> {
    if scenes.len() > MAX_SCENES {
        return Err(format!("At most {MAX_SCENES} scenes"));
    }
    let mut ids = HashSet::new();
    for scene in scenes {
        if !valid_key(&scene.id) || !ids.insert(scene.id.as_str()) {
            return Err(format!("Scene ID {:?} is invalid or repeated", scene.id));
        }
        if !valid_name(&scene.name) {
            return Err("Scene names are 1–60 characters".into());
        }
        let layout = layouts
            .iter()
            .find(|l| l.id == scene.layout_id)
            .ok_or_else(|| format!("{}: layout {:?} does not exist", scene.name, scene.layout_id))?;
        let mut seen = HashSet::new();
        let mut diagrams = 0;
        for (slot, source) in &scene.sources {
            if !layout.slots.iter().any(|s| &s.id == slot) {
                return Err(format!("{}: slot {slot:?} is not in layout {}", scene.name, layout.name));
            }
            let parsed = parse_source(source).ok_or_else(|| format!("{}: unknown source {source:?}", scene.name))?;
            if !seen.insert(source.as_str()) {
                return Err(format!("{}: {source} is in two slots", scene.name));
            }
            if matches!(parsed, SourceRef::Diagram(_)) {
                diagrams += 1;
            }
        }
        // The programme page draws one live diagram at a time.
        if diagrams > 1 {
            return Err(format!("{}: one live diagram per scene", scene.name));
        }
    }
    Ok(())
}

impl Studio {
    pub(super) fn scene(&self, id: &str) -> Option<&Scene> {
        self.scenes.iter().find(|s| s.id == id)
    }
    /// Whether any scene shows a live diagram (the snapshot then carries its inputs).
    pub(super) fn wants_diagram(&self) -> bool {
        self.scenes.iter().flat_map(|s| s.sources.values()).any(|id| id.starts_with("diagram:"))
    }
}

/// One source of the catalogue, with its state now.
#[derive(Clone, Debug, Serialize)]
pub(super) struct Source {
    id: String,
    kind: &'static str,
    view: String,
    label: String,
    /// `live`, `no_signal`, `stale` or `ready` (a graphic needs no input).
    state: &'static str,
    transparent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_id: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    hidden: bool,
    /// A ticketed live preview (WebRTC player page) for a camera tile.
    #[serde(skip_serializing_if = "Option::is_none")]
    preview_url: Option<String>,
}

/// Visible, live cameras in relay order, as logical ids.
fn visible_live(streams: &[StreamInfo], hidden: &[String]) -> Vec<String> {
    streams
        .iter()
        .filter(|s| s.live)
        .map(|s| s.id.strip_prefix("transcoded-").unwrap_or(&s.id).to_owned())
        .filter(|id| !hidden.contains(id))
        .collect()
}

fn telemetry_state(app: &AppState, now: u64) -> &'static str {
    let rows = app.recent_telemetry_cache.lock().unwrap();
    match rows.iter().map(|r| r.timestamp_ms).max() {
        None => "no_signal",
        Some(at) if u64::try_from(at).ok().and_then(|at| now.checked_sub(at)).is_some_and(|age| age < 5000) => "live",
        Some(_) => "stale",
    }
}

/// The station's diagrams as `(id, title)`, plus the station-mode view when
/// the diagram module offers one.
fn diagrams() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = crate::diagrams::load(&crate::diagrams::diagrams_dir())
        .diagrams
        .iter()
        .filter_map(|d| Some((d["id"].as_str()?.to_owned(), d["title"].as_str().unwrap_or("Live diagram").to_owned())))
        .collect();
    if crate::diagrams::scene_topology("diagram-mode").is_some() {
        out.insert(0, ("mode".into(), "Live diagram (station mode)".into()));
    }
    out
}

async fn catalogue(state: &Arc<MediaState>, headers: &HeaderMap, presentation: &contract::Presentation) -> ApiResult<Vec<Source>> {
    let streams = fetch_streams(state).await.map(|s| s.0).unwrap_or_default();
    let hidden = &presentation.broadcast.hidden_stream_ids;
    let now = contract::program_now_ms();
    let telemetry = telemetry_state(&state.app, now);
    let mut out = Vec::new();
    for stream in &streams {
        let logical = stream.id.strip_prefix("transcoded-").unwrap_or(&stream.id).to_owned();
        let ticket = contract::ticket(state, headers, &format!("stream:{}", stream.id)).await?;
        out.push(Source {
            id: format!("camera:{logical}"),
            kind: "camera",
            view: "camera".into(),
            label: presentation.stream_labels.get(&logical).cloned().unwrap_or_else(|| stream.name.clone()),
            state: if stream.live { "live" } else { "no_signal" },
            transparent: false,
            hidden: hidden.contains(&logical),
            preview_url: Some(format!("/api/media-assets/streams/{}?ticket={ticket}", stream.id)),
            stream_id: Some(logical),
        });
    }
    let live = visible_live(&streams, hidden);
    for n in 1..=AUTO_CAMERAS {
        out.push(Source {
            id: format!("camera-auto:{n}"),
            kind: "camera",
            view: "camera".into(),
            label: format!("Auto camera {n}"),
            state: if live.len() >= n { "live" } else { "no_signal" },
            transparent: false,
            hidden: false,
            preview_url: None,
            stream_id: live.get(n - 1).cloned(),
        });
    }
    for (id, title) in diagrams() {
        out.push(Source {
            id: format!("diagram:{id}"),
            kind: "telemetry",
            view: "diagram".into(),
            label: title,
            state: telemetry,
            transparent: false,
            hidden: false,
            preview_url: None,
            stream_id: None,
        });
    }
    for (id, kind, view, label, transparent) in BUILT_IN_SOURCES {
        let state_now = match *kind {
            "graphic" => "ready",
            _ if *view == "recorded_replay" => {
                if presentation.broadcast.recorded_replay.is_some() { "ready" } else { "no_signal" }
            }
            _ => telemetry,
        };
        out.push(Source {
            id: (*id).into(),
            kind,
            view: (*view).into(),
            label: (*label).into(),
            state: state_now,
            transparent: *transparent,
            hidden: false,
            preview_url: None,
            stream_id: None,
        });
    }
    Ok(out)
}

/// The scene the programme page draws: slots in stacking order with each
/// slot's source resolved (an automatic camera becomes the camera it is now).
pub(super) struct Resolved {
    pub(super) value: serde_json::Value,
    /// Logical camera ids the scene shows.
    pub(super) cameras: Vec<String>,
    pub(super) diagram: Option<String>,
}

pub(super) fn resolve(
    scene: &Scene,
    layout: &Layout,
    streams: &[StreamInfo],
    presentation: &contract::Presentation,
) -> Resolved {
    let hidden = &presentation.broadcast.hidden_stream_ids;
    let live = visible_live(streams, hidden);
    let mut cameras = Vec::new();
    let mut diagram = None;
    let mut slots: Vec<&Slot> = layout.slots.iter().collect();
    slots.sort_by_key(|s| s.z);
    let slots: Vec<serde_json::Value> = slots
        .into_iter()
        .map(|s| {
            let source = scene.sources.get(&s.id).and_then(|id| {
                let parsed = parse_source(id)?;
                let (kind, view, label, transparent, stream) = match &parsed {
                    SourceRef::Camera(stream) => {
                        let label = presentation.stream_labels.get(stream).cloned().unwrap_or_else(|| {
                            streams
                                .iter()
                                .find(|i| i.id.strip_prefix("transcoded-").unwrap_or(&i.id) == stream)
                                .map(|i| i.name.clone())
                                .unwrap_or_else(|| stream.clone())
                        });
                        let on = streams.iter().any(|i| i.live && i.id.strip_prefix("transcoded-").unwrap_or(&i.id) == stream);
                        ("camera", "camera".to_owned(), label, false, on.then(|| stream.clone()))
                    }
                    SourceRef::AutoCamera(n) => {
                        let stream = live.get(n - 1).cloned();
                        let label = stream
                            .as_ref()
                            .map(|id| presentation.stream_labels.get(id).cloned().unwrap_or_else(|| id.clone()))
                            .unwrap_or_else(|| format!("Auto camera {n}"));
                        ("camera", "camera".to_owned(), label, false, stream)
                    }
                    SourceRef::Diagram(name) => {
                        diagram.get_or_insert_with(|| name.clone());
                        ("telemetry", "diagram".to_owned(), name.clone(), false, None)
                    }
                    SourceRef::BuiltIn(id) => {
                        let (_, kind, view, label, transparent) = BUILT_IN_SOURCES.iter().find(|b| b.0 == *id)?;
                        (*kind, (*view).to_owned(), (*label).to_owned(), *transparent, None)
                    }
                };
                if let Some(stream) = &stream {
                    cameras.push(stream.clone());
                }
                Some(serde_json::json!({
                    "id": id, "kind": kind, "view": view, "label": label,
                    "transparent": transparent, "stream_id": stream,
                }))
            });
            serde_json::json!({"id": s.id, "x": s.x, "y": s.y, "width": s.width, "height": s.height, "z": s.z, "source": source})
        })
        .collect();
    Resolved {
        value: serde_json::json!({"scene_id": scene.id, "name": scene.name, "program_bar": scene.program_bar, "slots": slots}),
        cameras,
        diagram,
    }
}

/// A virtual one-slot scene showing a single source full frame (the
/// programme page's `source=` mode, e.g. a transparent OBS overlay).
pub(super) fn single_source(id: &str) -> Option<(Scene, Layout)> {
    parse_source(id)?;
    let layout = Layout { id: "_source".into(), name: "Source".into(), slots: preset("full").unwrap() };
    let scene = Scene {
        id: "_source".into(),
        name: id.into(),
        layout_id: layout.id.clone(),
        sources: BTreeMap::from([("main".into(), id.into())]),
        program_bar: false,
    };
    Some((scene, layout))
}

#[derive(Serialize)]
struct View {
    revision: u64,
    program_url: String,
    program_scene_id: String,
    on_air: bool,
    transition: Transition,
    take_seq: u64,
    layouts: Vec<Layout>,
    scenes: Vec<Scene>,
    sources: Vec<Source>,
    presets: Vec<serde_json::Value>,
    limits: serde_json::Value,
}

#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn manager(state: &MediaState, headers: &HeaderMap) -> ApiResult<()> {
    let principal = crate::web::authorize_headers(&state.app, headers, Permission::ViewData).await?;
    if principal.can_manage_stream() {
        Ok(())
    } else {
        Err(error(StatusCode::FORBIDDEN, "Stream-manager access required"))
    }
}

#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn view(state: &Arc<MediaState>, headers: &HeaderMap, presentation: contract::Presentation) -> ApiResult<Response> {
    let sources = catalogue(state, headers, &presentation).await?;
    let program_ticket = contract::ticket(state, headers, "program").await?;
    let studio = presentation.broadcast.studio.clone();
    let presets = ["full", "pip", "side_by_side", "grid_2x2", "lower_third"]
        .iter()
        .map(|id| serde_json::json!({"id": id, "slots": preset(id)}))
        .collect();
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(View {
            revision: presentation.broadcast.revision,
            program_url: format!("/api/media-assets/program?ticket={program_ticket}"),
            on_air: presentation.broadcast.layout == "studio",
            program_scene_id: studio.program_scene_id,
            transition: studio.transition,
            take_seq: studio.take_seq,
            layouts: studio.layouts,
            scenes: studio.scenes,
            sources,
            presets,
            limits: serde_json::json!({"layouts": MAX_LAYOUTS, "scenes": MAX_SCENES, "slots": MAX_SLOTS, "min_slot_percent": MIN_SLOT, "max_transition_ms": MAX_TRANSITION_MS}),
        }),
    )
        .into_response())
}

#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn get_studio(State(state): State<Arc<MediaState>>, headers: HeaderMap) -> ApiResult<Response> {
    manager(&state, &headers).await?;
    let presentation = contract::read_presentation(&state).await?;
    view(&state, &headers, presentation).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutsRequest {
    revision: u64,
    layouts: Vec<Layout>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenesRequest {
    revision: u64,
    scenes: Vec<Scene>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TakeRequest {
    scene_id: String,
    transition: Transition,
}

/// Read, check the revision (when the edit carries one), change, write.
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn edit(
    state: &Arc<MediaState>,
    headers: &HeaderMap,
    revision: Option<u64>,
    change: impl FnOnce(&mut contract::Presentation) -> Result<(), String>,
) -> ApiResult<Response> {
    manager(state, headers).await?;
    let presentation = {
        let _guard = state.presentation_write.lock().await;
        let mut presentation = contract::read_presentation(state).await?;
        if revision.is_some_and(|r| r != presentation.broadcast.revision) {
            return Err(error(StatusCode::CONFLICT, "Studio changed; reload before saving"));
        }
        change(&mut presentation).map_err(|message| error(StatusCode::BAD_REQUEST, &message))?;
        presentation.broadcast.revision = presentation
            .broadcast
            .revision
            .checked_add(1)
            .ok_or_else(|| error(StatusCode::CONFLICT, "Broadcast revision exhausted"))?;
        contract::write_presentation(state, &presentation).await?;
        presentation
    };
    view(state, headers, presentation).await
}

#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn save_layouts(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<LayoutsRequest>,
) -> ApiResult<Response> {
    edit(&state, &headers, Some(request.revision), move |presentation| {
        validate_layouts(&request.layouts)?;
        let studio = &mut presentation.broadcast.studio;
        // Scenes keep their slots: a layout in use stays, and a slot a scene
        // fills stays, so a saved scene never silently loses a source.
        validate_scenes(&studio.scenes, &request.layouts).map_err(|e| format!("A scene still uses this layout or slot ({e})"))?;
        studio.layouts = request.layouts;
        Ok(())
    })
    .await
}

#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn save_scenes(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<ScenesRequest>,
) -> ApiResult<Response> {
    edit(&state, &headers, Some(request.revision), move |presentation| {
        let on_air = presentation.broadcast.layout == "studio";
        let studio = &mut presentation.broadcast.studio;
        validate_scenes(&request.scenes, &studio.layouts)?;
        if on_air && !request.scenes.iter().any(|s| s.id == studio.program_scene_id) {
            return Err("The scene on Program cannot be deleted; take another scene first".into());
        }
        studio.scenes = request.scenes;
        Ok(())
    })
    .await
}

/// A take is live operation: it never waits on a revision, so a cut always
/// goes through; it bumps the revision so a stale broadcast form cannot
/// revert the programme.
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn take(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<TakeRequest>,
) -> ApiResult<Response> {
    edit(&state, &headers, None, move |presentation| {
        if request.transition.duration_ms > MAX_TRANSITION_MS {
            return Err(format!("Transitions last at most {MAX_TRANSITION_MS} ms"));
        }
        let broadcast = &mut presentation.broadcast;
        if broadcast.studio.scene(&request.scene_id).is_none() {
            return Err(format!("Scene {:?} does not exist", request.scene_id));
        }
        broadcast.studio.program_scene_id = request.scene_id;
        broadcast.studio.transition = request.transition;
        broadcast.studio.take_seq = broadcast.studio.take_seq.saturating_add(1);
        broadcast.layout = "studio".into();
        // A take ends a recorded-flight scene; the replay stays selectable as a source.
        Ok(())
    })
    .await
}

pub(super) fn routes() -> Router<Arc<MediaState>> {
    Router::new()
        .route("/api/studio", get(get_studio))
        .route("/api/studio/layouts", post(save_layouts).layer(DefaultBodyLimit::max(256 * 1024)))
        .route("/api/studio/scenes", post(save_scenes).layer(DefaultBodyLimit::max(256 * 1024)))
        .route("/api/studio/take", post(take).layer(DefaultBodyLimit::max(4096)))
}

/// A media state over `root` that starts no background task (tests and the
/// contract fixtures).
#[cfg(test)]
pub(super) fn test_state(app: Arc<AppState>, root: &std::path::Path) -> Arc<MediaState> {
    use tokio::sync::Mutex;
    Arc::new(MediaState {
        app,
        client: reqwest::Client::new(),
        api_url: "http://127.0.0.1:9".into(),
        webrtc_url: "http://127.0.0.1:9".into(),
        hls_url: "http://127.0.0.1:9".into(),
        relay_password: String::new(),
        model_write: Mutex::new(()),
        models: root.join("models"),
        recordings: root.join("video"),
        voice: Arc::new(crate::voice::VoiceHub::default()),
        presentation_write: Mutex::new(()),
        tickets: Mutex::new(Vec::new()),
        replays: Mutex::new(Default::default()),
        program_history: Mutex::new(Default::default()),
        hls_segments: Mutex::new(Default::default()),
        preview_sessions: Mutex::new(Vec::new()),
        publishers: Mutex::new(Vec::new()),
        publish_offer: Mutex::new(()),
        transcoding: Mutex::default(),
        recorded_vehicle_cache: Mutex::new(Default::default()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_cover_every_preset() {
        let studio = Studio::default();
        validate_layouts(&studio.layouts).unwrap();
        validate_scenes(&studio.scenes, &studio.layouts).unwrap();
        for id in ["full", "pip", "side_by_side", "grid_2x2", "lower_third"] {
            assert!(studio.layouts.iter().any(|l| l.id == id && Some(&l.slots) == preset(id).as_ref()), "{id}");
        }
        assert!(studio.scenes.len() <= 9, "every default scene has a hotkey");
        assert!(studio.wants_diagram(), "the station-mode diagram is a default scene");
    }

    #[test]
    fn studio_round_trips_and_old_presentations_get_the_defaults() {
        let studio = Studio::default();
        let wire = serde_json::to_value(&studio).unwrap();
        assert_eq!(serde_json::from_value::<Studio>(wire.clone()).unwrap(), studio);
        assert_eq!(wire["transition"], serde_json::json!({"kind": "fade", "duration_ms": 600}));
        let old: contract::Presentation = serde_json::from_value(serde_json::json!({"broadcast": {"layout": "grid"}})).unwrap();
        assert_eq!(old.broadcast.studio, studio);
    }

    #[test]
    fn source_ids_are_generic_and_checked() {
        assert_eq!(parse_source("camera:pad-wide"), Some(SourceRef::Camera("pad-wide".into())));
        assert_eq!(parse_source("camera-auto:2"), Some(SourceRef::AutoCamera(2)));
        assert_eq!(parse_source("diagram:flight"), Some(SourceRef::Diagram("flight".into())));
        assert_eq!(parse_source("overlay:lower-third"), Some(SourceRef::BuiltIn("overlay:lower-third")));
        for bad in ["camera:../x", "camera-auto:0", "camera-auto:9", "diagram:", "telemetry:nope", "nope", ""] {
            assert_eq!(parse_source(bad), None, "{bad}");
        }
    }

    #[test]
    fn layouts_reject_out_of_canvas_tiny_or_repeated_slots() {
        let ok = Layout { id: "l".into(), name: "L".into(), slots: preset("pip").unwrap() };
        validate_layouts(std::slice::from_ref(&ok)).unwrap();
        let bad = |f: &dyn Fn(&mut Layout)| {
            let mut l = ok.clone();
            f(&mut l);
            validate_layouts(&[l]).is_err()
        };
        assert!(bad(&|l| l.slots[1].x = 80.0));
        assert!(bad(&|l| l.slots[1].width = 4.0));
        assert!(bad(&|l| l.slots[1].y = f64::NAN));
        assert!(bad(&|l| l.slots[1].id = "main".into()));
        assert!(bad(&|l| l.slots[1].z = 10));
        assert!(bad(&|l| l.slots.clear()));
        assert!(bad(&|l| l.name = " padded".into()));
        assert!(validate_layouts(&[ok.clone(), ok]).is_err(), "repeated layout ID");
    }

    #[test]
    fn scenes_need_their_layout_slots_and_one_diagram() {
        let studio = Studio::default();
        let mut scene = studio.scene("camera-diagram").unwrap().clone();
        scene.sources.insert("right".into(), "diagram:flight".into());
        validate_scenes(std::slice::from_ref(&scene), &studio.layouts).unwrap();
        let mut two = scene.clone();
        two.layout_id = "grid_2x2".into();
        two.sources = BTreeMap::from([("a".into(), "diagram:flight".into()), ("b".into(), "diagram:network".into())]);
        assert!(validate_scenes(&[two], &studio.layouts).unwrap_err().contains("one live diagram"));
        let mut missing = scene.clone();
        missing.sources.insert("pip".into(), "camera:x".into());
        assert!(validate_scenes(&[missing], &studio.layouts).is_err());
        let mut repeated = scene.clone();
        repeated.sources.insert("left".into(), "diagram:flight".into());
        assert!(validate_scenes(&[repeated], &studio.layouts).is_err());
        let mut no_layout = scene;
        no_layout.layout_id = "gone".into();
        assert!(validate_scenes(&[no_layout], &studio.layouts).is_err());
    }

    #[test]
    fn resolution_follows_live_visible_cameras_and_stacking() {
        let studio = Studio::default();
        let mut presentation = contract::Presentation::default();
        presentation.broadcast.hidden_stream_ids = vec!["hidden".into()];
        presentation.stream_labels.insert("pad".into(), "Pad wide".into());
        let streams = vec![
            StreamInfo { id: "hidden".into(), name: "hidden".into(), live: true },
            StreamInfo { id: "off".into(), name: "off".into(), live: false },
            StreamInfo { id: "transcoded-pad".into(), name: "pad".into(), live: true },
        ];
        let scene = studio.scene("interview").unwrap();
        let layout = studio.layouts.iter().find(|l| l.id == scene.layout_id).unwrap();
        let resolved = resolve(scene, layout, &streams, &presentation);
        assert_eq!(resolved.cameras, ["pad"]);
        let slots = resolved.value["slots"].as_array().unwrap();
        assert_eq!(slots[0]["id"], "main");
        assert_eq!(slots[0]["source"]["stream_id"], "pad");
        assert_eq!(slots[0]["source"]["label"], "Pad wide");
        assert_eq!(slots[1]["source"]["stream_id"], serde_json::Value::Null, "no second live visible camera");
        let (single, layout) = single_source("overlay:lower-third").unwrap();
        let resolved = resolve(&single, &layout, &streams, &presentation);
        assert_eq!(resolved.value["slots"][0]["source"]["transparent"], true);
        assert_eq!(resolved.value["program_bar"], false);
        assert!(single_source("nope").is_none());
    }

    #[test]
    fn only_diagram_scenes_capture_diagram_inputs() {
        let mut studio = Studio::default();
        for scene in &mut studio.scenes {
            scene.sources.retain(|_, id| !id.starts_with("diagram:"));
        }
        assert!(!studio.wants_diagram());
        studio.scenes[0].sources.insert("main".into(), "diagram:flight".into());
        assert!(studio.wants_diagram());
    }

    /// The routes end to end: manager-only, revision-checked edits that keep
    /// scenes whole, a take that always applies and puts the studio on air,
    /// the programme page's resolved scene (Program, Preview, one source) and
    /// a broadcast form that cannot edit the studio.
    #[tokio::test]
    async fn studio_routes_edit_take_and_feed_the_programme() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt;
        let root = crate::recording_export::tests::TestDirectory::new();
        let app = crate::recording_export::tests::export_state(&root.0, true).await;
        std::fs::write(root.0.join("auth.json"), serde_json::json!({"anonymous":{"view_data":true},"users":[
            {"username":"producer","roles":["stream_master"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"watcher","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}}]}).to_string()).unwrap();
        for (token, user) in [("studio-producer", "producer"), ("studio-watcher", "watcher")] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,0,0,9999999999999)")
                .bind(token).bind(user).execute(&app.auth_db).await.unwrap();
        }
        let state = test_state(app, &root.0);
        let router = contract::routes().merge(routes()).with_state(state.clone());
        let call = |method: &str, path: &str, token: &str, body: serde_json::Value| {
            let router = router.clone();
            let request = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(if body.is_null() { Body::empty() } else { Body::from(body.to_string()) }).unwrap();
            async move {
                let response = router.oneshot(request).await.unwrap();
                let status = response.status();
                let bytes = to_bytes(response.into_body(), 4_000_000).await.unwrap();
                (status, serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into()))
            }
        };
        let null = serde_json::Value::Null;
        // Stream-manager access only.
        assert_eq!(call("GET", "/api/studio", "studio-watcher", null.clone()).await.0, StatusCode::FORBIDDEN);
        assert_eq!(call("POST", "/api/studio/take", "studio-watcher", serde_json::json!({"scene_id":"slate","transition":{"kind":"cut","duration_ms":0}})).await.0, StatusCode::FORBIDDEN);
        let (status, view) = call("GET", "/api/studio", "studio-producer", null.clone()).await;
        assert_eq!(status, StatusCode::OK, "{view}");
        assert_eq!(view["on_air"], false);
        assert_eq!(view["revision"], 0);
        assert_eq!(view["scenes"].as_array().unwrap().len(), Studio::default().scenes.len());
        let ids: Vec<&str> = view["sources"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
        for id in ["camera-auto:1", "telemetry:stats", "telemetry:tclock", "overlay:lower-third", "graphic:slate"] {
            assert!(ids.contains(&id), "{id}");
        }
        assert!(ids.iter().any(|id| id.starts_with("diagram:")), "the station's diagrams are sources");
        for id in ["diagram:mode", "diagram:flight", "diagram:test_fire", "diagram:ground_test"] {
            assert!(ids.contains(&id), "the per-mode view {id} is a source");
        }
        assert_eq!(view["sources"].as_array().unwrap().iter().find(|s| s["id"] == "camera-auto:1").unwrap()["state"], "no_signal");
        assert_eq!(view["sources"].as_array().unwrap().iter().find(|s| s["id"] == "graphic:slate").unwrap()["state"], "ready");
        let diagram = ids.iter().find(|id| id.starts_with("diagram:")).unwrap().to_string();

        // Layouts: a new one saves; removing a slot a scene fills is refused.
        let mut layouts = view["layouts"].clone();
        layouts.as_array_mut().unwrap().push(serde_json::json!({"id":"corner","name":"Corner","slots":[{"id":"main","x":0,"y":0,"width":100,"height":100},{"id":"bug","x":75,"y":5,"width":20,"height":15,"z":3}]}));
        let (status, saved) = call("POST", "/api/studio/layouts", "studio-producer", serde_json::json!({"revision":0,"layouts":layouts})).await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["revision"], 1);
        let (status, _) = call("POST", "/api/studio/layouts", "studio-producer", serde_json::json!({"revision":0,"layouts":layouts})).await;
        assert_eq!(status, StatusCode::CONFLICT, "a stale editor reloads first");
        let mut shrunk = saved["layouts"].clone();
        shrunk.as_array_mut().unwrap().iter_mut().find(|l| l["id"] == "pip").unwrap()["slots"].as_array_mut().unwrap().pop();
        let (status, message) = call("POST", "/api/studio/layouts", "studio-producer", serde_json::json!({"revision":1,"layouts":shrunk})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(message.as_str().unwrap().contains("scene still uses"), "{message}");

        // Scenes: a diagram with a T-clock bug on the new layout.
        let mut scenes = saved["scenes"].clone();
        scenes.as_array_mut().unwrap().push(serde_json::json!({"id":"corner-diagram","name":"Corner diagram","layout_id":"corner","sources":{"main":diagram,"bug":"overlay:tclock-bug"},"program_bar":false}));
        let (status, saved) = call("POST", "/api/studio/scenes", "studio-producer", serde_json::json!({"revision":1,"scenes":scenes})).await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        let mut bad = saved["scenes"].clone();
        bad[0]["sources"]["main"] = "telemetry:nope".into();
        assert_eq!(call("POST", "/api/studio/scenes", "studio-producer", serde_json::json!({"revision":2,"scenes":bad})).await.0, StatusCode::BAD_REQUEST);

        // Take: no revision, always applies, puts the studio on air.
        let (status, taken) = call("POST", "/api/studio/take", "studio-producer", serde_json::json!({"scene_id":"corner-diagram","transition":{"kind":"fade","duration_ms":900}})).await;
        assert_eq!(status, StatusCode::OK, "{taken}");
        assert_eq!((taken["on_air"].clone(), taken["program_scene_id"].clone(), taken["take_seq"].clone()), (true.into(), "corner-diagram".into(), 1.into()));
        assert_eq!(taken["transition"], serde_json::json!({"kind":"fade","duration_ms":900}));
        assert_eq!(call("POST", "/api/studio/take", "studio-producer", serde_json::json!({"scene_id":"gone","transition":{"kind":"cut","duration_ms":0}})).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(call("POST", "/api/studio/take", "studio-producer", serde_json::json!({"scene_id":"slate","transition":{"kind":"fade","duration_ms":60000}})).await.0, StatusCode::BAD_REQUEST);
        // The scene on Program stays.
        let without: Vec<_> = taken["scenes"].as_array().unwrap().iter().filter(|s| s["id"] != "corner-diagram").cloned().collect();
        let (status, message) = call("POST", "/api/studio/scenes", "studio-producer", serde_json::json!({"revision":taken["revision"],"scenes":without})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(message.as_str().unwrap().contains("on Program"));

        // The programme page: Program, the Preview monitor and one source.
        let ticket = taken["program_url"].as_str().unwrap().split("ticket=").nth(1).unwrap().to_owned();
        let (status, program) = call("GET", &format!("/api/media-assets/program/state?ticket={ticket}"), "studio-producer", null.clone()).await;
        assert_eq!(status, StatusCode::OK, "{program}");
        assert_eq!(program["studio"]["scene_id"], "corner-diagram");
        assert_eq!(program["studio"]["mode"], "program");
        assert_eq!(program["studio"]["take_seq"], 1);
        assert_eq!(program["studio"]["program_bar"], false);
        assert_eq!(program["studio"]["slots"][1]["source"]["view"], "tclock_bug");
        assert_eq!(program["studio"]["slots"][1]["source"]["transparent"], true);
        assert!(program["diagram_topology"]["id"].is_string(), "the scene's diagram topology rides along");
        let (_, preview) = call("GET", &format!("/api/media-assets/program/state?ticket={ticket}&scene=slate"), "studio-producer", null.clone()).await;
        assert_eq!((preview["studio"]["scene_id"].clone(), preview["studio"]["mode"].clone()), ("slate".into(), "preview".into()));
        assert_eq!(preview["diagram_topology"], serde_json::Value::Null);
        let (_, single) = call("GET", &format!("/api/media-assets/program/state?ticket={ticket}&source=overlay:lower-third"), "studio-producer", null.clone()).await;
        assert_eq!(single["studio"]["mode"], "source");
        assert_eq!(single["studio"]["slots"][0]["source"]["view"], "lower_third");
        assert_eq!(call("GET", &format!("/api/media-assets/program/state?ticket={ticket}&scene=gone"), "studio-producer", null.clone()).await.0, StatusCode::NOT_FOUND);
        assert_eq!(call("GET", &format!("/api/media-assets/program/state?ticket={ticket}&source=nope"), "studio-producer", null.clone()).await.0, StatusCode::NOT_FOUND);
        // A viewer's ticket shows Program but not an unaired scene.
        let (_, live) = call("GET", "/api/live_streams", "studio-watcher", null.clone()).await;
        let viewer = live["program_url"].as_str().unwrap().split("ticket=").nth(1).unwrap().to_owned();
        assert_eq!(call("GET", &format!("/api/media-assets/program/state?ticket={viewer}"), "studio-watcher", null.clone()).await.1["studio"]["scene_id"], "corner-diagram");
        assert_eq!(call("GET", &format!("/api/media-assets/program/state?ticket={viewer}&scene=slate"), "studio-watcher", null.clone()).await.0, StatusCode::FORBIDDEN);

        // A broadcast form keeps the studio and cannot invent an on-air scene.
        let (_, live) = call("GET", "/api/live_streams", "studio-producer", null.clone()).await;
        let mut form = live["broadcast"].clone();
        form["label"] = "Launch day".into();
        form["studio"] = serde_json::to_value(Studio::default()).unwrap();
        let (status, saved) = call("POST", "/api/live_streams/control", "studio-producer", form).await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["studio"]["program_scene_id"], "corner-diagram");
        assert!(saved["studio"]["scenes"].as_array().unwrap().iter().any(|s| s["id"] == "corner-diagram"));
        assert_eq!(saved["layout"], "studio");
        let saved_presentation = contract::read_presentation(&state).await.unwrap();
        assert!(saved_presentation.broadcast.studio.wants_diagram(), "the snapshot carries diagram inputs");
    }


    /// The station-side cost of telemetry sources: the programme snapshot
    /// (4 Hz) with and without diagram inputs. Run with `--ignored --nocapture`.
    #[tokio::test]
    #[ignore = "measurement"]
    async fn measure_programme_snapshot_cost() {
        let root = crate::recording_export::tests::TestDirectory::new();
        let app = crate::recording_export::tests::export_state(&root.0, true).await;
        {
            let mut rows = app.recent_telemetry_cache.lock().unwrap();
            for i in 0..2000i64 {
                rows.push_back(crate::types::TelemetryRow::marked(i, if i % 2 == 0 { "PRESSURE_TRANSDUCER_CALIBRATED".into() } else { "VALVE_STATE".into() }, "VB".into(), vec![Some(1.0); 7]));
            }
        }
        let mut with = contract::Presentation::default();
        let mut without = contract::Presentation::default();
        for scene in &mut without.broadcast.studio.scenes {
            scene.sources.retain(|_, id| !id.starts_with("diagram:"));
        }
        with.broadcast.studio.scenes[0].sources.insert("main".into(), "diagram:mode".into());
        for (name, p) in [("without diagram", &without), ("with diagram", &with)] {
            let start = std::time::Instant::now();
            for _ in 0..400 {
                std::hint::black_box(contract::program::telemetry_snapshot(&app, p, 2000));
            }
            let per = start.elapsed() / 400;
            println!("programme snapshot {name}: {per:?} per snapshot, {:.3} % of one core at 4 Hz", per.as_secs_f64() * 4.0 * 100.0);
        }
    }

}
