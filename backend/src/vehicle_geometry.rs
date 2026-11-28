//! Vehicle geometry (docs/vehicle-geometry.md): the airframe the dashboard
//! draws, generated from the station's OpenRocket design.
//!
//! The source is, in order: the `.ork` an administrator imported into the
//! apogee planner, the file named by `GS_VEHICLE_ORK`, or the team's design
//! built into the station (`tools/flight_profile/vehicle/SWEET_V6.ork`). It is
//! generated at startup and again whenever the planner's vehicle changes, so a
//! new design reaches every client without touching the UI.
//!
//! `GET /api/vehicle/geometry` (ViewData) answers [`Geometry`]: an outer-mold
//! radius profile along the axis, the sections, fins, internal tubes and tanks,
//! recovery devices, separation points, the motor, a nozzle estimate, colours,
//! and the named parts the 3D renderer animates. Lengths are metres along the
//! axis from the nose tip toward the tail; radii are metres from the axis.
use crate::apogee_planner::ork::{self, Element};
use crate::auth::Permission;
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

pub const PATH: &str = "/api/vehicle/geometry";
pub const SCHEMA: &str = "gs26.vehicle_geometry";
/// Bump on a breaking change; clients reject versions they do not know.
pub const VERSION: u32 = 1;
/// The team's design, built in as the default.
pub const DEFAULT_ORK: &[u8] = include_bytes!("../../tools/flight_profile/vehicle/SWEET_V6.ork");
pub const DEFAULT_ORK_NAME: &str = "SWEET_V6.ork";

const NOSE_SAMPLES: usize = 32;
const CURVE_SAMPLES: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Source {
    /// `default` (built in), `env` (`GS_VEHICLE_ORK`) or `import` (planner upload).
    pub kind: String,
    pub file: String,
    /// FNV-1a 64 of the `.ork` bytes, as `fnv1a64:<hex>`; also the HTTP ETag.
    pub hash: String,
    pub rocket: String,
    pub creator: String,
    pub designer: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Section {
    pub name: String,
    /// `nosecone`, `bodytube` or `transition`.
    pub kind: String,
    /// `nose`, `avionics`, `payload`, `recovery`, `tank`, `motor`, `boattail` or `airframe`.
    pub role: String,
    pub x0_m: f64,
    pub x1_m: f64,
    pub r0_m: f64,
    pub r1_m: f64,
    pub shape: Option<String>,
    pub shape_parameter: Option<f64>,
    pub material: Option<String>,
    /// `#rrggbb` from the design's appearance, if it has one.
    pub color: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Nose {
    pub shape: String,
    pub shape_parameter: f64,
    pub length_m: f64,
    pub radius_m: f64,
    pub shoulder_length_m: f64,
    pub shoulder_radius_m: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FinSet {
    pub name: String,
    pub count: u32,
    /// Leading edge of the root chord.
    pub x_m: f64,
    /// Body radius at the root.
    pub root_radius_m: f64,
    pub root_chord_m: f64,
    pub tip_chord_m: f64,
    pub span_m: f64,
    pub sweep_m: f64,
    pub thickness_m: f64,
    /// `square`, `rounded` or `airfoil`.
    pub cross_section: String,
    pub cant_deg: f64,
    /// Rotation of the first fin about the axis.
    pub angle_offset_deg: f64,
    /// Planform `[axial from the root LE, radial from the root]` (m), closed implicitly.
    pub planform: Vec<[f64; 2]>,
    pub material: Option<String>,
    pub color: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Internal {
    pub name: String,
    /// `innertube`, `tubecoupler`, `bulkhead`, `centeringring`, `engineblock`.
    pub kind: String,
    /// `motor_mount`, `tank`, `coupler`, `bulkhead`, `centering_ring` or `tube`.
    pub role: String,
    pub x0_m: f64,
    pub x1_m: f64,
    pub outer_radius_m: f64,
    pub inner_radius_m: f64,
    pub material: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Tank {
    pub name: String,
    /// True when the tank is part of the outer airframe.
    pub external: bool,
    pub x0_m: f64,
    pub x1_m: f64,
    pub radius_m: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Recovery {
    pub name: String,
    /// `drogue` (the first device, normally at apogee) or `main`.
    pub role: String,
    pub kind: String,
    /// The packed device.
    pub x0_m: f64,
    pub x1_m: f64,
    pub packed_radius_m: f64,
    pub diameter_m: f64,
    pub cd: Option<f64>,
    pub deploy_event: String,
    pub deploy_altitude_m: Option<f64>,
    pub deploy_delay_s: Option<f64>,
    pub line_length_m: Option<f64>,
    pub line_count: Option<u32>,
    /// The separation it deploys through.
    pub separation_x_m: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Separation {
    pub x_m: f64,
    pub fore: String,
    pub aft: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Motor {
    pub designation: Option<String>,
    pub manufacturer: Option<String>,
    pub diameter_m: f64,
    pub length_m: f64,
    pub x0_m: f64,
    pub x1_m: f64,
    pub mount: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Nozzle {
    /// The exit plane.
    pub x_m: f64,
    pub exit_radius_m: f64,
    pub throat_radius_m: f64,
    pub length_m: f64,
    /// OpenRocket designs carry no nozzle: sized from the motor mount.
    pub estimated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Point {
    pub name: String,
    pub x_m: f64,
}

/// A part the renderer names (see docs/vehicle-geometry.md, "Named parts").
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Part {
    pub name: String,
    pub x0_m: f64,
    pub x1_m: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Svg {
    /// `min_x min_y width height` (m): nose at x = 0, tail at +x, axis at y = 0.
    pub view_box: [f64; 4],
    /// The body outline.
    pub body: String,
    /// One fin above and the projections of the others below.
    pub fins: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Geometry {
    pub schema: String,
    pub version: u32,
    pub source: Source,
    pub length_m: f64,
    /// Largest body diameter (fins excluded).
    pub diameter_m: f64,
    /// Largest extent across the fins (body diameter + 2 spans).
    pub span_m: f64,
    pub cg_m: Option<f64>,
    pub cp_m: Option<f64>,
    pub stability_cal: Option<f64>,
    /// Outer mold line `[x, r]` from the nose tip to the tail.
    pub profile: Vec<[f64; 2]>,
    pub sections: Vec<Section>,
    pub nose: Option<Nose>,
    pub fins: Vec<FinSet>,
    pub internals: Vec<Internal>,
    pub tanks: Vec<Tank>,
    pub recovery: Vec<Recovery>,
    pub separations: Vec<Separation>,
    pub rail_guides: Vec<Point>,
    pub motor: Option<Motor>,
    pub nozzle: Option<Nozzle>,
    pub parts: Vec<Part>,
    pub svg: Svg,
    pub warnings: Vec<String>,
}

/// To 0.01 mm (and no negative zero), which keeps the JSON readable.
fn round(x: f64) -> f64 {
    (x * 1e5).round() / 1e5 + 0.0
}

fn tidy(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Number(n) if n.is_f64() => {
            if let Some(r) = n.as_f64().map(|x| (x * 1e6).round() / 1e6 + 0.0).and_then(serde_json::Number::from_f64) {
                *n = r;
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(tidy),
        serde_json::Value::Object(o) => o.values_mut().for_each(tidy),
        _ => {}
    }
}

pub fn fnv1a64(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{h:016x}")
}

// ------------------------------------------------------------ shapes

/// Radius of an OpenRocket nose shape `x` from its tip (radius `r` at `l`).
pub fn shape_radius(shape: &str, param: f64, x: f64, r: f64, l: f64) -> f64 {
    use std::f64::consts::PI;
    if l <= 0.0 {
        return r;
    }
    let x = x.clamp(0.0, l);
    let t = x / l;
    let v = match shape {
        "ogive" if param >= 0.001 => {
            let big = ((l * l + r * r) * (((2.0 - param) * l).powi(2) + (param * r).powi(2)) / (4.0 * (param * r).powi(2))).sqrt();
            let ll = l / param;
            let y0 = (big * big - ll * ll).max(0.0).sqrt();
            (big * big - (ll - x).powi(2)).max(0.0).sqrt() - y0
        }
        "ellipsoid" => r * (2.0 * t - t * t).max(0.0).sqrt(),
        "power" => {
            if param <= 1e-5 {
                if x > 0.0 { r } else { 0.0 }
            } else {
                r * t.powf(param)
            }
        }
        "parabolic" => r * (2.0 * t - param * t * t) / (2.0 - param),
        "haack" => {
            let th = (1.0 - 2.0 * t).clamp(-1.0, 1.0).acos();
            r * ((th - (2.0 * th).sin() / 2.0 + param * th.sin().powi(3)) / PI).max(0.0).sqrt()
        }
        _ => r * t,
    };
    v.clamp(0.0, r)
}

/// Radius of a transition `x` from its fore end, as OpenRocket draws it.
fn transition_radius(shape: &str, param: f64, clipped: bool, x: f64, len: f64, r1: f64, r2: f64) -> f64 {
    if (r1 - r2).abs() < 1e-12 || len <= 0.0 {
        return r1;
    }
    let (small, big, x) = if r1 < r2 { (r1, r2, x) } else { (r2, r1, len - x) };
    let curved = !matches!(shape, "conical");
    if clipped && curved && small > 0.0 {
        // The full shape of radius `big`, its tip cut where it is `small` wide.
        let (mut lo, mut hi) = (0.0_f64, len * 1e3);
        for _ in 0..80 {
            let clip = 0.5 * (lo + hi);
            if shape_radius(shape, param, clip, big, clip + len) < small { lo = clip } else { hi = clip }
        }
        let clip = 0.5 * (lo + hi);
        return shape_radius(shape, param, clip + x, big, clip + len);
    }
    small + shape_radius(shape, param, x, big - small, len)
}

// ------------------------------------------------------------ walk

fn text(el: &Element, name: &str) -> Option<String> {
    el.child(name).map(|c| c.text.trim().to_string()).filter(|s| !s.is_empty())
}

fn flag(el: &Element, name: &str) -> bool {
    el.child(name).is_some_and(|c| c.text.trim() == "true")
}

fn name_of(el: &Element) -> String {
    text(el, "name").unwrap_or_else(|| el.name.clone())
}

fn color_of(el: &Element) -> Option<String> {
    let p = el.child("appearance")?.child("paint")?;
    let c = |k: &str| p.attr(k).and_then(|v| v.parse::<f64>().ok()).map(|v| v.clamp(0.0, 255.0).round() as u8);
    Some(format!("#{:02x}{:02x}{:02x}", c("red")?, c("green")?, c("blue")?))
}

/// A numeric child; a bare `auto` (or nothing) takes `fallback`.
fn num_or(el: &Element, name: &str, fallback: f64) -> f64 {
    el.num(name).unwrap_or(fallback)
}

fn section_role(el: &Element, kind: &str, has_mount: bool) -> String {
    let n = name_of(el).to_ascii_lowercase();
    let has = |w: &[&str]| w.iter().any(|k| n.contains(k));
    if kind == "nosecone" {
        "nose"
    } else if has(&["avionic", "av bay", "electronics", "ebay", "e-bay"]) {
        "avionics"
    } else if has(&["payload"]) {
        "payload"
    } else if has(&["recovery", "parachute", "chute"]) {
        "recovery"
    } else if has(&["oxidi", "tank", "n2o", "nitrous", "lox"]) {
        "tank"
    } else if has_mount || has(&["motor", "engine", "combustion", "fin can"]) {
        "motor"
    } else if has(&["boattail", "boat tail"]) {
        "boattail"
    } else {
        "airframe"
    }
    .into()
}

fn internal_role(el: &Element) -> String {
    let n = name_of(el).to_ascii_lowercase();
    if el.child("motormount").is_some() {
        "motor_mount"
    } else if ["tank", "oxidi", "n2o", "nitrous"].iter().any(|k| n.contains(k)) {
        "tank"
    } else {
        match el.name.as_str() {
            "tubecoupler" => "coupler",
            "bulkhead" | "engineblock" => "bulkhead",
            "centeringring" => "centering_ring",
            _ if n.contains("coupler") => "coupler",
            _ => "tube",
        }
    }
    .into()
}

#[derive(Default)]
struct Walk {
    sections: Vec<Section>,
    profile: Vec<[f64; 2]>,
    nose: Option<Nose>,
    fins: Vec<FinSet>,
    internals: Vec<Internal>,
    recovery: Vec<(Recovery, usize)>,
    rail_guides: Vec<Point>,
    motor: Option<Motor>,
    warnings: Vec<String>,
}

impl Walk {
    fn push_profile(&mut self, x: f64, r: f64) {
        let p = [round(x), round(r)];
        if self.profile.last() != Some(&p) {
            self.profile.push(p);
        }
    }

    /// An outer component (nose, tube, transition) at `x`; returns its aft radius.
    fn outer(&mut self, el: &Element, x: f64, fore_r: f64, ref_r: f64) -> f64 {
        let len = ork::length_of(el);
        let kind = el.name.as_str();
        let shape = text(el, "shape");
        let param = el.num("shapeparameter");
        let (r0, r1) = match kind {
            "nosecone" => (0.0, num_or(el, "aftradius", ref_r)),
            "transition" => (num_or(el, "foreradius", fore_r), num_or(el, "aftradius", fore_r)),
            _ => {
                let r = num_or(el, "radius", fore_r);
                (r, r)
            }
        };
        let s = shape.clone().unwrap_or_else(|| "conical".into());
        let p = param.unwrap_or(0.0);
        match kind {
            "nosecone" => {
                for i in 0..=NOSE_SAMPLES {
                    // Denser near the tip, where the curvature is.
                    let u = i as f64 / NOSE_SAMPLES as f64;
                    let xi = len * (1.0 - (u * std::f64::consts::FRAC_PI_2).cos());
                    self.push_profile(x + xi, shape_radius(&s, p, xi, r1, len));
                }
                if self.nose.is_none() {
                    self.nose = Some(Nose {
                        shape: s.clone(),
                        shape_parameter: p,
                        length_m: len,
                        radius_m: r1,
                        shoulder_length_m: el.num("aftshoulderlength").unwrap_or(0.0),
                        shoulder_radius_m: el.num("aftshoulderradius").unwrap_or(0.0),
                    });
                }
            }
            "transition" if s != "conical" => {
                let clipped = flag(el, "shapeclipped");
                for i in 0..=CURVE_SAMPLES {
                    let xi = len * i as f64 / CURVE_SAMPLES as f64;
                    self.push_profile(x + xi, transition_radius(&s, p, clipped, xi, len, r0, r1));
                }
            }
            _ => {
                self.push_profile(x, r0);
                self.push_profile(x + len, r1);
            }
        }
        let has_mount = has_motor_mount(el);
        self.sections.push(Section {
            name: name_of(el),
            kind: kind.into(),
            role: if kind == "transition" && r1 < r0 && len > 0.02 { "boattail".into() } else { section_role(el, kind, has_mount) },
            x0_m: x,
            x1_m: x + len,
            r0_m: r0,
            r1_m: r1,
            shape: if kind == "bodytube" { None } else { shape },
            shape_parameter: if kind == "bodytube" { None } else { param },
            material: text(el, "material"),
            color: color_of(el),
        });
        let section = self.sections.len() - 1;
        let inner = r0.min(r1) - el.num("thickness").unwrap_or(0.002);
        self.children(el, x, len, inner.max(0.0), r0.max(r1), section);
        r1
    }

    fn children(&mut self, el: &Element, start: f64, len: f64, inner_r: f64, body_r: f64, section: usize) {
        let mut after = start;
        for c in el.subcomponents() {
            let clen = ork::length_of(c);
            let x = ork::child_start(c, start, len, clen, &mut after);
            self.inner(c, x, clen, inner_r, body_r, section);
        }
    }

    fn inner(&mut self, el: &Element, x: f64, len: f64, inner_r: f64, body_r: f64, section: usize) {
        let kind = el.name.as_str();
        let lower = name_of(el).to_ascii_lowercase();
        match kind {
            "innertube" | "tubecoupler" | "bulkhead" | "centeringring" | "engineblock" => {
                let ro = num_or(el, "outerradius", inner_r);
                let ri = match kind {
                    "bulkhead" => 0.0,
                    "centeringring" => el.num("innerradius").unwrap_or(0.0),
                    _ => (ro - el.num("thickness").unwrap_or(0.002)).max(0.0),
                };
                self.internals.push(Internal {
                    name: name_of(el),
                    kind: kind.into(),
                    role: internal_role(el),
                    x0_m: x,
                    x1_m: x + len,
                    outer_radius_m: ro,
                    inner_radius_m: ri,
                    material: text(el, "material"),
                });
                if let Some(mm) = el.child("motormount") {
                    let m = mm.child("motor");
                    let md = m.and_then(|m| m.num("diameter")).unwrap_or(2.0 * ri);
                    let ml = m.and_then(|m| m.num("length")).unwrap_or(len);
                    let x1 = x + len + mm.num("overhang").unwrap_or(0.0);
                    if self.motor.is_none() {
                        self.motor = Some(Motor {
                            designation: m.and_then(|m| text(m, "designation")),
                            manufacturer: m.and_then(|m| text(m, "manufacturer")),
                            diameter_m: md,
                            length_m: ml,
                            x0_m: x1 - ml,
                            x1_m: x1,
                            mount: name_of(el),
                        });
                    }
                }
                self.children(el, x, len, ri, body_r, section);
            }
            "trapezoidfinset" => {
                let root = el.num("rootchord").unwrap_or(0.1);
                let tip = el.num("tipchord").unwrap_or(0.05);
                let span = el.num("height").unwrap_or(0.05);
                let sweep = el.num("sweeplength").unwrap_or(0.0);
                self.fins.push(FinSet {
                    name: name_of(el),
                    count: el.num("fincount").unwrap_or(3.0).round().max(1.0) as u32,
                    x_m: x,
                    root_radius_m: body_r,
                    root_chord_m: root,
                    tip_chord_m: tip,
                    span_m: span,
                    sweep_m: sweep,
                    thickness_m: el.num("thickness").unwrap_or(0.003),
                    cross_section: text(el, "crosssection").unwrap_or_else(|| "square".into()),
                    cant_deg: el.num("cant").unwrap_or(0.0),
                    angle_offset_deg: el.num("angleoffset").or_else(|| el.num("rotation")).unwrap_or(0.0),
                    planform: vec![[0.0, 0.0], [round(sweep), round(span)], [round(sweep + tip), round(span)], [round(root), 0.0]],
                    material: text(el, "material"),
                    color: color_of(el),
                });
            }
            "ellipticalfinset" | "freeformfinset" | "tubefinset" => {
                self.warnings.push(format!("{}: {kind} is not drawn (trapezoidal fins only).", name_of(el)));
            }
            "parachute" | "streamer" => {
                let event = text(el, "deployevent").unwrap_or_else(|| "ejection".into());
                self.recovery.push((
                    Recovery {
                        name: name_of(el),
                        role: String::new(),
                        kind: kind.into(),
                        x0_m: x,
                        x1_m: x + len,
                        packed_radius_m: el.num("packedradius").unwrap_or(inner_r),
                        diameter_m: el.num("diameter").unwrap_or(0.0),
                        cd: el.num("cd"),
                        deploy_altitude_m: if event == "altitude" { el.num("deployaltitude") } else { None },
                        deploy_event: event,
                        deploy_delay_s: el.num("deploydelay"),
                        line_length_m: el.num("linelength"),
                        line_count: el.num("linecount").map(|n| n.round().max(0.0) as u32),
                        separation_x_m: 0.0,
                    },
                    section,
                ));
            }
            "railbutton" | "launchlug" => self.rail_guides.push(Point { name: name_of(el), x_m: x + len / 2.0 }),
            "masscomponent" if lower.contains("rail button") || lower.contains("launch lug") || lower.contains("rail guide") => {
                self.rail_guides.push(Point { name: name_of(el), x_m: x + len / 2.0 })
            }
            _ => self.children(el, x, len, inner_r, body_r, section),
        }
    }
}

fn has_motor_mount(el: &Element) -> bool {
    el.subcomponents().iter().any(|c| c.child("motormount").is_some() || has_motor_mount(c))
}

fn path(points: &[[f64; 2]]) -> String {
    let mut out = String::new();
    for (i, p) in points.iter().enumerate() {
        out.push_str(&format!("{}{} {}", if i == 0 { "M" } else { "L" }, round(p[0]), round(p[1])));
    }
    out.push('Z');
    out
}

/// Generates the geometry from `.ork` bytes (ZIP, gzip or plain XML).
pub fn generate(bytes: &[u8], kind: &str, file: &str) -> Result<Geometry, String> {
    let root = ork::parse_xml(&ork::rocket_xml(bytes)?)?;
    if root.name != "openrocket" {
        return Err("Not an OpenRocket file (no <openrocket> root).".into());
    }
    let rocket = root.child("rocket").ok_or("The OpenRocket file has no <rocket>.")?;
    let stages: Vec<&Element> = rocket.subcomponents().iter().filter(|s| s.name == "stage").collect();
    if stages.is_empty() {
        return Err("The OpenRocket file has no stages.".into());
    }
    let ref_r = stages
        .iter()
        .flat_map(|s| s.subcomponents())
        .filter_map(|c| c.num("radius").or_else(|| c.num("aftradius")))
        .fold(0.0_f64, f64::max);
    let mut w = Walk::default();
    if stages.len() > 1 {
        w.warnings.push(format!("{} stages: drawn as one vehicle.", stages.len()));
    }
    let (mut x, mut fore_r) = (0.0, ref_r);
    for s in &stages {
        for c in s.subcomponents() {
            if matches!(c.name.as_str(), "nosecone" | "bodytube" | "transition") {
                fore_r = w.outer(c, x, fore_r, ref_r);
                x += ork::length_of(c);
            } else {
                w.warnings.push(format!("{}: a pod or unknown outer component is not drawn.", name_of(c)));
            }
        }
    }
    let length = x;
    if length <= 0.1 || w.profile.len() < 2 {
        return Err("The OpenRocket rocket has no length.".into());
    }
    let body_r = w.sections.iter().map(|s| s.r0_m.max(s.r1_m)).fold(0.0_f64, f64::max);

    // Separations: a recovery device deploys through the nearer end of its section.
    let mut separations: Vec<Separation> = Vec::new();
    let mut recovery = Vec::new();
    let mut drogue_taken = false;
    let mut items = std::mem::take(&mut w.recovery);
    // Apogee devices first: the first is the drogue, the rest are mains.
    items.sort_by_key(|(r, _)| if r.deploy_event == "apogee" { 0 } else { 1 });
    for (mut r, si) in items {
        let s = &w.sections[si];
        let mid = 0.5 * (r.x0_m + r.x1_m);
        let at_aft = mid > 0.5 * (s.x0_m + s.x1_m) || si == 0;
        let sx = if at_aft { s.x1_m } else { s.x0_m };
        let (fore, aft) = if at_aft { (si, si + 1) } else { (si.saturating_sub(1), si) };
        if !separations.iter().any(|p| (p.x_m - sx).abs() < 1e-6) && aft < w.sections.len() {
            separations.push(Separation { x_m: sx, fore: w.sections[fore].name.clone(), aft: w.sections[aft].name.clone() });
        }
        r.separation_x_m = sx;
        r.role = if !drogue_taken { "drogue".into() } else { "main".into() };
        drogue_taken = true;
        recovery.push(r);
    }
    separations.sort_by(|a, b| a.x_m.total_cmp(&b.x_m));

    let tanks: Vec<Tank> = w
        .sections
        .iter()
        .filter(|s| s.role == "tank")
        .map(|s| Tank { name: s.name.clone(), external: true, x0_m: s.x0_m, x1_m: s.x1_m, radius_m: s.r0_m.max(s.r1_m) })
        .chain(w.internals.iter().filter(|i| i.role == "tank").map(|i| Tank {
            name: i.name.clone(),
            external: false,
            x0_m: i.x0_m,
            x1_m: i.x1_m,
            radius_m: i.outer_radius_m,
        }))
        .collect();

    let mount = w.internals.iter().find(|i| i.role == "motor_mount").cloned();
    let aft_r = w.profile.last().map(|p| p[1]).unwrap_or(body_r);
    let nozzle = mount.as_ref().map(|m| {
        let exit = (0.78 * m.inner_radius_m).min(aft_r.max(m.inner_radius_m * 0.5));
        let exit_x = m.x1_m.max(length - 0.005).min(length + 0.05);
        Nozzle { x_m: exit_x, exit_radius_m: exit, throat_radius_m: exit * 0.45, length_m: (2.2 * exit).min(m.x1_m - m.x0_m), estimated: true }
    });

    // Named parts (the renderer's animation and highlight contract).
    let nose_len = w.nose.as_ref().map_or(0.0, |n| n.length_m);
    let split = separations.first().map_or(length * 0.5, |s| s.x_m);
    let join = w
        .internals
        .iter()
        .filter(|i| i.role == "coupler" && i.x0_m < split && i.x1_m > split)
        .map(|i| (i.x0_m, i.x1_m))
        .fold(None, |a: Option<(f64, f64)>, b| Some(a.map_or(b, |a| (a.0.min(b.0), a.1.max(b.1)))))
        .unwrap_or((split - 0.03, split + 0.03));
    let mut parts = vec![
        Part { name: "stage-1".into(), x0_m: 0.0, x1_m: length },
        Part { name: "nose".into(), x0_m: 0.0, x1_m: nose_len },
        Part { name: "airframe-upper".into(), x0_m: nose_len, x1_m: split },
        Part { name: "airframe-join".into(), x0_m: join.0, x1_m: join.1 },
        Part { name: "airframe-lower".into(), x0_m: split, x1_m: length },
    ];
    if let Some(m) = &mount {
        parts.push(Part { name: "gimbal-pivot".into(), x0_m: m.x0_m, x1_m: m.x0_m });
        parts.push(Part { name: "engine".into(), x0_m: m.x0_m, x1_m: m.x1_m });
    }
    if let Some(n) = &nozzle {
        parts.push(Part { name: "motor-flame".into(), x0_m: n.x_m, x1_m: n.x_m });
    }
    for (k, f) in w.fins.iter().take(1).flat_map(|f| (0..f.count).map(move |k| (k, f))) {
        parts.push(Part { name: format!("fin-pivot-{k}"), x0_m: f.x_m, x1_m: f.x_m + f.root_chord_m });
    }
    for r in &recovery {
        let name = format!("{}-parachute", r.role);
        if !parts.iter().any(|p| p.name == name) {
            parts.push(Part { name, x0_m: r.separation_x_m, x1_m: r.separation_x_m });
        }
    }

    // CG and CP from the planner's import (the same stage overrides and Barrowman CP).
    let (cg, cp, cal) = match ork::import(bytes, &crate::apogee_planner::flight::Config::reference().vehicle) {
        Ok(i) => (Some(i.cg_from_nose_m), Some(i.cp_from_nose_m), Some((i.cp_from_nose_m - i.cg_from_nose_m) / (2.0 * body_r.max(1e-6)))),
        Err(e) => {
            w.warnings.push(format!("CG/CP unavailable: {e}"));
            (None, None, None)
        }
    };

    // 2D side profile, horizontal, nose at the origin.
    let mut outline: Vec<[f64; 2]> = w.profile.iter().map(|p| [p[0], -p[1]]).collect();
    outline.extend(w.profile.iter().rev().map(|p| [p[0], p[1]]));
    let mut fins_path = String::new();
    let mut span = 2.0 * body_r;
    for f in &w.fins {
        let pts: Vec<[f64; 2]> = f.planform.iter().map(|p| [f.x_m + p[0], f.root_radius_m + p[1]]).collect();
        span = span.max(2.0 * (f.root_radius_m + f.span_m));
        // The first fin up, the others projected at their angles.
        for k in 0..f.count {
            let a = (f.angle_offset_deg.to_radians()) + std::f64::consts::TAU * k as f64 / f.count as f64;
            let c = a.cos();
            if c.abs() < 1e-3 {
                continue;
            }
            let shown: Vec<[f64; 2]> = pts.iter().map(|p| [p[0], -p[1] * c]).collect();
            fins_path.push_str(&path(&shown));
        }
    }
    let half = 0.5 * span;
    let svg = Svg {
        view_box: [0.0, round(-half), round(length.max(nozzle.as_ref().map_or(0.0, |n| n.x_m))), round(span)],
        body: path(&outline),
        fins: fins_path,
    };
    let source = Source {
        kind: kind.into(),
        file: file.into(),
        hash: fnv1a64(bytes),
        rocket: text(rocket, "name").unwrap_or_else(|| "Rocket".into()),
        creator: root.attr("creator").unwrap_or("").into(),
        designer: text(rocket, "designer").unwrap_or_default(),
    };
    let g = Geometry {
        schema: SCHEMA.into(),
        version: VERSION,
        source,
        length_m: round(length),
        diameter_m: round(2.0 * body_r),
        span_m: round(span),
        cg_m: cg.map(round),
        cp_m: cp.map(round),
        stability_cal: cal.map(|c| (c * 100.0).round() / 100.0),
        profile: w.profile,
        sections: w.sections,
        nose: w.nose,
        fins: w.fins,
        internals: w.internals,
        tanks,
        recovery,
        separations,
        rail_guides: w.rail_guides,
        motor: w.motor,
        nozzle,
        parts,
        svg,
        warnings: w.warnings,
    };
    // Every length to the micrometre: the design's own numbers, without float noise.
    let mut v = serde_json::to_value(&g).map_err(|e| e.to_string())?;
    tidy(&mut v);
    serde_json::from_value(v).map_err(|e| e.to_string())
}

// ------------------------------------------------------------ station

/// The built-in design's geometry (generated once).
pub fn default_geometry() -> &'static Arc<Geometry> {
    static DEFAULT: OnceLock<Arc<Geometry>> = OnceLock::new();
    DEFAULT.get_or_init(|| {
        if let Some(path) = std::env::var_os("GS_VEHICLE_ORK") {
            let file = std::path::Path::new(&path).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
            match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| generate(&b, "env", &file)) {
                Ok(g) => return Arc::new(g),
                Err(e) => log::warn!("GS_VEHICLE_ORK {}: {e}; using the built-in {DEFAULT_ORK_NAME}.", path.to_string_lossy()),
            }
        }
        Arc::new(generate(DEFAULT_ORK, "default", DEFAULT_ORK_NAME).expect("the built-in .ork generates"))
    })
}

/// The station's current geometry: the planner's imported design, else the default.
pub fn current(state: &AppState) -> Arc<Geometry> {
    crate::apogee_planner::vehicle_geometry(state).unwrap_or_else(|| default_geometry().clone())
}

async fn get_geometry(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return r;
    }
    let g = current(&state);
    let etag = format!("\"{}\"", g.source.hash);
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    ([(header::CACHE_CONTROL, "no-cache".to_string()), (header::ETAG, etag)], Json(g.as_ref().clone())).into_response()
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route(PATH, get(get_geometry))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64, tol: f64, what: &str) {
        assert!((a - b).abs() <= tol, "{what}: {a} vs {b}");
    }

    /// The key numbers straight from SWEET_V6.ork (OpenRocket 24.12).
    #[test]
    fn sweet_v6_geometry_matches_the_ork() {
        let g = generate(DEFAULT_ORK, "default", DEFAULT_ORK_NAME).unwrap();
        assert_eq!((g.schema.as_str(), g.version), (SCHEMA, VERSION));
        assert_eq!(g.source.rocket, "SWEET_V6");
        // Sum of the outer components: nose 0.5334 + tubes and transitions.
        let sum = 0.5334 + 0.3857625 + 0.5588 + 0.57785 + 0.6969125 + 0.00254 + 0.8128 + 0.00254 + 0.66675 + 0.149225;
        near(g.length_m, sum, 1e-4, "length");
        near(g.length_m / 0.3048, 14.4, 0.05, "about 15 ft");
        near(g.diameter_m, 2.0 * 0.078232, 1e-5, "6.16 in body");
        let n = g.nose.as_ref().unwrap();
        assert_eq!(n.shape, "haack");
        near(n.length_m, 0.5334, 1e-9, "nose length");
        near(n.shoulder_length_m, 0.0508, 1e-9, "nose shoulder");
        assert_eq!(g.fins.len(), 1);
        let f = &g.fins[0];
        assert_eq!(f.count, 3);
        near(f.span_m, 0.174625, 1e-9, "fin span");
        near(f.root_chord_m, 0.250825, 1e-6, "root chord");
        near(f.tip_chord_m, 0.1381125, 1e-6, "tip chord");
        near(f.sweep_m, 0.08255, 1e-9, "sweep");
        near(f.thickness_m, 0.006604, 1e-9, "fin thickness");
        // Bottom of the lower body tube, 25.4 mm up, root chord ahead of that.
        let lbt = g.sections.iter().find(|s| s.name == "Lower Body Tube").unwrap();
        near(f.x_m, lbt.x1_m - 0.0254 - 0.250825, 1e-6, "fin position");
        assert_eq!(lbt.role, "motor");
        let ox = g.sections.iter().find(|s| s.name == "Oxidizer Tank").unwrap();
        assert_eq!(ox.role, "tank");
        near(ox.x1_m - ox.x0_m, 0.8128, 1e-9, "ox tank length");
        near(ox.r0_m, 0.0762, 1e-9, "ox tank radius");
        assert!(g.tanks.iter().any(|t| t.name == "Oxidizer Tank" && t.external));
        let cc = g.internals.iter().find(|i| i.name == "Combustion Chamber").unwrap();
        assert_eq!(cc.role, "motor_mount");
        near(cc.x1_m - cc.x0_m, 0.720725, 1e-9, "chamber length");
        near(cc.outer_radius_m, 0.0507, 1e-9, "chamber radius");
        near(cc.x1_m, g.length_m, 1e-3, "the chamber ends at the tail");
        let m = g.motor.as_ref().unwrap();
        assert_eq!(m.designation.as_deref(), Some("Midland"));
        near(m.diameter_m, 0.101, 1e-9, "motor diameter");
        near(m.length_m, 1.73, 1e-9, "motor length");
        // Both chutes pack at the aft end of the recovery tube and leave through its aft joint.
        let rt = g.sections.iter().find(|s| s.name == "Recovery Tube").unwrap();
        assert_eq!(rt.role, "recovery");
        assert_eq!(g.separations.len(), 1);
        near(g.separations[0].x_m, rt.x1_m, 1e-9, "separation");
        let drogue = g.recovery.iter().find(|r| r.role == "drogue").unwrap();
        let main = g.recovery.iter().find(|r| r.role == "main").unwrap();
        assert_eq!(drogue.name, "Reefed Parachute");
        near(drogue.diameter_m, 1.778, 1e-9, "reefed canopy");
        near(main.diameter_m, 3.556, 1e-9, "main canopy");
        near(main.deploy_altitude_m.unwrap(), 457.2, 1e-6, "main altitude");
        assert_eq!(g.rail_guides.len(), 2);
        // The stage CG override (2.667 m) and a stable CP behind it.
        near(g.cg_m.unwrap(), 2.667, 1e-3, "CG override");
        assert!(g.cp_m.unwrap() > g.cg_m.unwrap());
        assert_eq!(g.sections.iter().filter(|s| s.color.as_deref() == Some("#0000ff")).count(), 5);
        assert_eq!(g.sections.last().unwrap().role, "boattail");
        // The profile starts at the tip and ends at the boattail's aft radius.
        assert_eq!(g.profile[0], [0.0, 0.0]);
        near(g.profile.last().unwrap()[0], g.length_m, 1e-5, "profile end");
        near(g.profile.last().unwrap()[1], 0.053975, 1e-5, "boattail aft radius");
        near(g.span_m, 2.0 * (0.078232 + 0.174625), 1e-5, "span");
        for name in ["stage-1", "nose", "airframe-upper", "airframe-join", "airframe-lower", "engine", "gimbal-pivot", "motor-flame", "fin-pivot-0", "fin-pivot-2", "drogue-parachute", "main-parachute"] {
            assert!(g.parts.iter().any(|p| p.name == name), "{name}");
        }
        assert!(g.svg.body.starts_with("M0 0L"));
    }

    #[test]
    fn haack_and_ogive_shapes_end_at_the_base_radius() {
        for (shape, p) in [("haack", 0.0), ("haack", 1.0 / 3.0), ("ogive", 1.0), ("ogive", 0.5), ("ellipsoid", 0.0), ("power", 0.5), ("parabolic", 1.0), ("conical", 0.0)] {
            near(shape_radius(shape, p, 0.0, 0.08, 0.5), 0.0, 1e-9, shape);
            near(shape_radius(shape, p, 0.5, 0.08, 0.5), 0.08, 1e-6, shape);
            assert!(shape_radius(shape, p, 0.25, 0.08, 0.5) > 0.0);
        }
        // A clipped transition still meets both radii.
        near(transition_radius("haack", 0.0, true, 0.0, 0.15, 0.078, 0.054), 0.078, 1e-6, "fore");
        near(transition_radius("haack", 0.0, true, 0.15, 0.15, 0.078, 0.054), 0.054, 1e-6, "aft");
    }

    #[test]
    fn bad_files_are_refused() {
        assert!(generate(b"<notrocket/>", "import", "x.ork").is_err());
        assert!(generate(b"<openrocket><rocket><subcomponents></subcomponents></rocket></openrocket>", "import", "x.ork").is_err());
    }
}
