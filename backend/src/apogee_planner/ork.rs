//! OpenRocket `.ork` import (docs/apogee-planner.md section 1): geometry,
//! masses, CG and a Barrowman CP from the design file, so the planner flies
//! the team's OpenRocket model instead of the hand assumptions.
//!
//! `.ork` files are ZIP archives holding `rocket.ork` (XML); older ones are
//! gzip'd or plain XML. All three are read. No Java.
use super::flight::{Fins, Vehicle};
use serde::{Deserialize, Serialize};
use std::io::Read;

const MAX_XML_BYTES: usize = 32 * 1024 * 1024;

// ------------------------------------------------------------ container

fn le16(b: &[u8], i: usize) -> Option<usize> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?) as usize)
}
fn le32(b: &[u8], i: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize)
}

fn inflate(data: &[u8], size_hint: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(size_hint.min(MAX_XML_BYTES));
    flate2::read::DeflateDecoder::new(data)
        .take(MAX_XML_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("The .ork archive could not be inflated: {e}"))?;
    Ok(out)
}

/// The rocket XML inside a ZIP: the `.ork`/`.xml` entry (normally `rocket.ork`).
fn unzip(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "The .ork archive is damaged.".to_string();
    let eocd = (0..bytes.len().saturating_sub(21))
        .rev()
        .take(65_557)
        .find(|&i| bytes[i..].starts_with(&[0x50, 0x4b, 0x05, 0x06]))
        .ok_or_else(bad)?;
    let count = le16(bytes, eocd + 10).ok_or_else(bad)?;
    let mut at = le32(bytes, eocd + 16).ok_or_else(bad)?;
    let mut best: Option<(usize, usize, usize, usize, String)> = None;
    for _ in 0..count {
        if !bytes.get(at..).is_some_and(|b| b.starts_with(&[0x50, 0x4b, 0x01, 0x02])) {
            return Err(bad());
        }
        let method = le16(bytes, at + 10).ok_or_else(bad)?;
        let csize = le32(bytes, at + 20).ok_or_else(bad)?;
        let usize_ = le32(bytes, at + 24).ok_or_else(bad)?;
        let nlen = le16(bytes, at + 28).ok_or_else(bad)?;
        let elen = le16(bytes, at + 30).ok_or_else(bad)?;
        let clen = le16(bytes, at + 32).ok_or_else(bad)?;
        let local = le32(bytes, at + 42).ok_or_else(bad)?;
        let name = String::from_utf8_lossy(bytes.get(at + 46..at + 46 + nlen).ok_or_else(bad)?).to_string();
        let lower = name.to_ascii_lowercase();
        let rank = if lower == "rocket.ork" { 0 } else if lower.ends_with(".ork") || lower.ends_with(".xml") { 1 } else { 9 };
        if rank < 9 && best.as_ref().is_none_or(|b| rank < if b.4.eq_ignore_ascii_case("rocket.ork") { 0 } else { 1 }) {
            best = Some((method, csize, usize_, local, name));
        }
        at += 46 + nlen + elen + clen;
    }
    let (method, csize, usize_, local, _) = best.ok_or("The .ork archive holds no rocket XML.")?;
    if !bytes.get(local..).is_some_and(|b| b.starts_with(&[0x50, 0x4b, 0x03, 0x04])) {
        return Err(bad());
    }
    let start = local + 30 + le16(bytes, local + 26).ok_or_else(bad)? + le16(bytes, local + 28).ok_or_else(bad)?;
    let data = bytes.get(start..start + csize).ok_or_else(bad)?;
    if usize_ > MAX_XML_BYTES {
        return Err("The rocket XML is too large.".into());
    }
    match method {
        0 => Ok(data.to_vec()),
        8 => inflate(data, usize_),
        m => Err(format!("Unsupported .ork compression method {m}.")),
    }
}

/// The rocket XML from a `.ork` file in any of its three forms.
pub fn rocket_xml(bytes: &[u8]) -> Result<String, String> {
    let xml = if bytes.starts_with(&[0x50, 0x4b, 0x03, 0x04]) {
        unzip(bytes)?
    } else if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes)
            .take(MAX_XML_BYTES as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|e| format!("The .ork file could not be decompressed: {e}"))?;
        out
    } else {
        bytes.to_vec()
    };
    if xml.len() > MAX_XML_BYTES {
        return Err("The rocket XML is too large.".into());
    }
    let text = String::from_utf8(xml).map_err(|_| "The rocket XML is not UTF-8.".to_string())?;
    Ok(text.trim_start_matches('\u{feff}').to_string())
}

// ------------------------------------------------------------ XML

#[derive(Debug, Default, Clone)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Element>,
}

impl Element {
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    /// A numeric child; OpenRocket writes "auto 0.0762" for automatic sizes.
    pub fn num(&self, name: &str) -> Option<f64> {
        let t = self.child(name)?.text.trim();
        t.strip_prefix("auto").map(str::trim).unwrap_or(t).parse::<f64>().ok().filter(|x| x.is_finite())
    }
    pub fn is_auto(&self, name: &str) -> bool {
        self.child(name).is_some_and(|c| c.text.trim().starts_with("auto"))
    }
    fn subcomponents(&self) -> &[Element] {
        self.child("subcomponents").map(|s| s.children.as_slice()).unwrap_or(&[])
    }
}

fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else { break };
        let ent = &rest[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if ent.starts_with("#x") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
            _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => out.push(c),
            None => out.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// A small non-validating XML parser: elements, attributes and text;
/// declarations, comments, CDATA and processing instructions are handled.
pub fn parse_xml(text: &str) -> Result<Element, String> {
    let b = text.as_bytes();
    let mut i = 0;
    let mut stack: Vec<Element> = vec![Element { name: "#document".into(), ..Default::default() }];
    let err = |m: &str| Err(format!("The rocket XML is malformed: {m}."));
    while i < b.len() {
        if b[i] != b'<' {
            let end = text[i..].find('<').map(|x| i + x).unwrap_or(b.len());
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&decode(&text[i..end]));
            }
            i = end;
            continue;
        }
        let rest = &text[i..];
        if rest.starts_with("<!--") {
            i += rest.find("-->").map(|x| x + 3).ok_or("The rocket XML is malformed: open comment.")?;
        } else if rest.starts_with("<![CDATA[") {
            let end = rest.find("]]>").ok_or("The rocket XML is malformed: open CDATA.")?;
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&rest[9..end]);
            }
            i += end + 3;
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            i += rest.find('>').map(|x| x + 1).ok_or("The rocket XML is malformed: open declaration.")?;
        } else if rest.starts_with("</") {
            let end = rest.find('>').ok_or("The rocket XML is malformed: open end tag.")?;
            let name = rest[2..end].trim();
            let el = stack.pop().ok_or("The rocket XML is malformed: stray end tag.")?;
            if el.name != name || stack.is_empty() {
                return err(&format!("</{name}> closes <{}>", el.name));
            }
            stack.last_mut().unwrap().children.push(el);
            i += end + 1;
        } else {
            // Start tag; quoted attribute values may contain '>'.
            let mut j = 1;
            let mut quote: Option<u8> = None;
            while i + j < b.len() {
                let c = b[i + j];
                match quote {
                    Some(q) if c == q => quote = None,
                    Some(_) => {}
                    None if c == b'"' || c == b'\'' => quote = Some(c),
                    None if c == b'>' => break,
                    None => {}
                }
                j += 1;
            }
            if i + j >= b.len() {
                return err("open start tag");
            }
            let inner = &text[i + 1..i + j];
            let self_closing = inner.ends_with('/');
            let inner = inner.trim_end_matches('/');
            let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
            let mut el = Element { name: inner[..name_end].to_string(), ..Default::default() };
            let mut a = &inner[name_end..];
            loop {
                a = a.trim_start();
                let Some(eq) = a.find('=') else { break };
                let key = a[..eq].trim().to_string();
                let v = a[eq + 1..].trim_start();
                let Some(q) = v.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                    return err("unquoted attribute");
                };
                let close = v[1..].find(q).ok_or("The rocket XML is malformed: open attribute.")?;
                el.attrs.push((key, decode(&v[1..1 + close])));
                a = &v[close + 2..];
            }
            if self_closing {
                stack.last_mut().unwrap().children.push(el);
            } else {
                stack.push(el);
            }
            i += j + 1;
        }
    }
    if stack.len() != 1 {
        return err("unclosed elements");
    }
    stack.pop().unwrap().children.into_iter().next().ok_or_else(|| "The rocket XML is empty.".to_string())
}

// ------------------------------------------------------------ rocket

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Imported {
    pub name: String,
    pub vehicle: Vehicle,
    pub cg_from_nose_m: f64,
    pub cp_from_nose_m: f64,
    pub motor_mount: Option<MotorMount>,
    pub components: Vec<ComponentMass>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MotorMount {
    pub length_m: f64,
    pub diameter_m: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ComponentMass {
    pub kind: String,
    pub name: String,
    pub mass_kg: f64,
    pub cg_from_nose_m: f64,
    pub overridden: bool,
}

/// OpenRocket surface finishes (roughness, m).
fn roughness(finish: &str) -> Option<f64> {
    match finish.trim() {
        "rough" => Some(500e-6),
        "unfinished" => Some(150e-6),
        "normal" => Some(60e-6),
        "smooth" => Some(20e-6),
        "finishpolished" | "polished" => Some(2e-6),
        _ => None,
    }
}

struct Walker {
    components: Vec<ComponentMass>,
    warnings: Vec<String>,
    nose: Option<(f64, f64, String)>,
    fins: Option<(Fins, f64, f64)>,
    max_radius: f64,
    motor_mount: Option<MotorMount>,
    finish: Option<f64>,
}

fn density(el: &Element) -> Option<(f64, String)> {
    let m = el.child("material")?;
    Some((m.attr("density")?.parse().ok()?, m.attr("type").unwrap_or("bulk").to_string()))
}

/// Axial start of a child component (from the nose tip).
fn child_start(el: &Element, parent_start: f64, parent_len: f64, len: f64, after: &mut f64) -> f64 {
    let (method, off) = match el.child("axialoffset").or_else(|| el.child("position")) {
        Some(p) => (
            p.attr("method").or_else(|| p.attr("type")).unwrap_or("top").to_string(),
            p.text.trim().parse::<f64>().unwrap_or(0.0),
        ),
        None => ("after".to_string(), 0.0),
    };
    let x = match method.as_str() {
        "bottom" => parent_start + parent_len - len + off,
        "middle" => parent_start + (parent_len - len) / 2.0 + off,
        "absolute" => off,
        "after" => *after + off,
        _ => parent_start + off,
    };
    *after = x + len;
    x
}

impl Walker {
    fn push(&mut self, el: &Element, kind: &str, mass: f64, cg: f64, start: f64) {
        let (mass, overridden) = match el.num("overridemass") {
            Some(m) if el.child("overridemass").is_some() => (m, true),
            _ => (mass, false),
        };
        // An override CG is measured from the component's front.
        let cg = el.num("overridecg").map_or(cg, |c| start + c);
        if mass > 0.0 {
            self.components.push(ComponentMass {
                kind: kind.into(),
                name: el.child("name").map(|n| n.text.trim().to_string()).unwrap_or_else(|| kind.into()),
                mass_kg: mass,
                cg_from_nose_m: cg,
                overridden,
            });
        }
    }

    /// A component and its children; `start`/`len` is its own axial extent.
    fn component(&mut self, el: &Element, start: f64, parent_radius: f64) {
        let pi = std::f64::consts::PI;
        let len = el.num("length").unwrap_or(0.0);
        let kind = el.name.as_str();
        let mut inner_radius = parent_radius;
        match kind {
            "nosecone" | "transition" => {
                let r = el.num("aftradius").unwrap_or(parent_radius);
                let t = el.num("thickness").unwrap_or(0.002);
                let shape = el.child("shape").map(|s| s.text.trim().to_string()).unwrap_or_else(|| "ogive".into());
                let (wet, cg_frac) = match shape.as_str() {
                    "conical" => (pi * r * (r * r + len * len).sqrt(), 2.0 / 3.0),
                    _ => (0.75 * pi * 2.0 * r * len, 0.6),
                };
                let rho = density(el).map(|d| d.0).unwrap_or(1850.0);
                self.push(el, kind, wet * t * rho, start + cg_frac * len, start);
                if kind == "nosecone" && self.nose.is_none() {
                    self.nose = Some((len, r, shape));
                }
                self.max_radius = self.max_radius.max(r);
                inner_radius = r - t;
                if self.finish.is_none() {
                    self.finish = el.child("finish").and_then(|f| roughness(&f.text));
                }
            }
            "bodytube" => {
                let r = el.num("radius").unwrap_or(parent_radius);
                let t = el.num("thickness").unwrap_or(0.002);
                let rho = density(el).map(|d| d.0).unwrap_or(1850.0);
                self.push(el, kind, pi * (r * r - (r - t).max(0.0).powi(2)) * len * rho, start + len / 2.0, start);
                self.max_radius = self.max_radius.max(r);
                inner_radius = r - t;
                if self.finish.is_none() {
                    self.finish = el.child("finish").and_then(|f| roughness(&f.text));
                }
            }
            "innertube" => {
                let r = el.num("outerradius").unwrap_or(parent_radius * 0.5);
                let t = el.num("thickness").unwrap_or(0.002);
                let rho = density(el).map(|d| d.0).unwrap_or(1850.0);
                self.push(el, kind, pi * (r * r - (r - t).max(0.0).powi(2)) * len * rho, start + len / 2.0, start);
                if el.child("motormount").is_some() {
                    self.motor_mount = Some(MotorMount { length_m: len, diameter_m: 2.0 * (r - t) });
                }
                inner_radius = r - t;
            }
            "centeringring" | "bulkhead" | "engineblock" => {
                let ro = if el.is_auto("outerradius") || el.num("outerradius").is_none() { parent_radius } else { el.num("outerradius").unwrap() };
                let ri = if kind == "bulkhead" { 0.0 } else { el.num("innerradius").unwrap_or(0.0) };
                let rho = density(el).map(|d| d.0).unwrap_or(700.0);
                self.push(el, kind, pi * (ro * ro - ri * ri) * len * rho, start + len / 2.0, start);
            }
            "trapezoidfinset" => {
                let n = el.num("fincount").unwrap_or(3.0);
                let root = el.num("rootchord").unwrap_or(0.1);
                let tip = el.num("tipchord").unwrap_or(0.05);
                let span = el.num("height").unwrap_or(0.05);
                let sweep = el.num("sweeplength").unwrap_or(0.0);
                let t = el.num("thickness").unwrap_or(0.003);
                let rho = density(el).map(|d| d.0).unwrap_or(1850.0);
                let area = 0.5 * (root + tip) * span;
                let xc = (sweep * (root + 2.0 * tip) + root * root + root * tip + tip * tip) / (3.0 * (root + tip));
                self.push(el, kind, n * area * t * rho, start + xc, start);
                if self.fins.is_none() {
                    self.fins = Some((Fins { count: n, root_m: root, tip_m: tip, span_m: span, thickness_m: t }, sweep, start));
                }
            }
            "masscomponent" => {
                let m = el.num("mass").unwrap_or(0.0);
                self.push(el, kind, m, start + len / 2.0, start);
            }
            "parachute" | "streamer" => {
                let area = match kind {
                    "parachute" => pi * (el.num("diameter").unwrap_or(0.0) / 2.0).powi(2),
                    _ => el.num("striplength").unwrap_or(0.0) * el.num("stripwidth").unwrap_or(0.0),
                };
                let m = density(el).filter(|d| d.1 == "surface").map(|d| d.0 * area).unwrap_or(0.0);
                self.push(el, kind, m, start + len / 2.0, start);
            }
            "shockcord" => {
                let m = density(el).filter(|d| d.1 == "line").map(|d| d.0 * el.num("cordlength").unwrap_or(0.0)).unwrap_or(0.0);
                self.push(el, kind, m, start + len / 2.0, start);
            }
            "ellipticalfinset" | "freeformfinset" | "tubefinset" => {
                self.warnings.push(format!("{kind}: only its override mass is used; drag and CP use trapezoidal fins only."));
                self.push(el, kind, 0.0, start + len / 2.0, start);
            }
            _ => {
                // Launch lugs, rail buttons and anything else: override mass only.
                self.push(el, kind, 0.0, start + len / 2.0, start);
            }
        }
        let mut after = start;
        for c in el.subcomponents() {
            let clen = if c.name == "trapezoidfinset" { c.num("rootchord").unwrap_or(0.0) } else { c.num("length").unwrap_or(0.0) };
            let cs = child_start(c, start, len, clen, &mut after);
            self.component(c, cs, inner_radius.max(0.0));
        }
    }
}

/// Barrowman normal-force slope of the fin set and its CP (from the nose).
fn barrowman_fins(f: &Fins, sweep: f64, le_x: f64, body_r: f64) -> (f64, f64) {
    let (cr, ct, s, n) = (f.root_m, f.tip_m, f.span_m, f.count);
    let d = 2.0 * body_r;
    let lm = (s * s + (sweep + ct / 2.0 - cr / 2.0).powi(2)).sqrt();
    let cna = (4.0 * n * (s / d).powi(2)) / (1.0 + (1.0 + (2.0 * lm / (cr + ct)).powi(2)).sqrt()) * (1.0 + body_r / (s + body_r));
    let xf = le_x + sweep * (cr + 2.0 * ct) / (3.0 * (cr + ct)) + (cr + ct - cr * ct / (cr + ct)) / 6.0;
    (cna, xf)
}

/// Imports a `.ork` (ZIP, gzip or plain XML) into planner vehicle parameters,
/// keeping the reference vehicle's values for what the file cannot give.
pub fn import(bytes: &[u8], reference: &Vehicle) -> Result<Imported, String> {
    let root = parse_xml(&rocket_xml(bytes)?)?;
    if root.name != "openrocket" {
        return Err("Not an OpenRocket file (no <openrocket> root).".into());
    }
    let rocket = root.child("rocket").ok_or("The OpenRocket file has no <rocket>.")?;
    let name = rocket.child("name").map(|n| n.text.trim().to_string()).unwrap_or_else(|| "Rocket".into());
    let mut w = Walker { components: Vec::new(), warnings: Vec::new(), nose: None, fins: None, max_radius: 0.0, motor_mount: None, finish: None };
    let stages: Vec<&Element> = rocket.subcomponents().iter().filter(|s| s.name == "stage").collect();
    if stages.is_empty() {
        return Err("The OpenRocket file has no stages.".into());
    }
    if stages.len() > 1 {
        w.warnings.push(format!("{} stages: all are flown as one vehicle.", stages.len()));
    }
    // Auto radii follow the largest tube.
    let radius = stages
        .iter()
        .flat_map(|s| s.subcomponents())
        .filter_map(|c| c.num("radius").or_else(|| c.num("aftradius")))
        .fold(0.0_f64, f64::max);
    let radius = if radius > 0.0 { radius } else { reference.diameter_m / 2.0 };
    let mut x = 0.0;
    for s in &stages {
        for c in s.subcomponents() {
            let len = c.num("length").unwrap_or(0.0);
            w.component(c, x, radius);
            x += len;
        }
    }
    let length = x;
    if length <= 0.1 {
        return Err("The OpenRocket rocket has no length.".into());
    }
    let mut dry: f64 = w.components.iter().map(|c| c.mass_kg).sum();
    let mut cg = if dry > 0.0 { w.components.iter().map(|c| c.mass_kg * c.cg_from_nose_m).sum::<f64>() / dry } else { length / 2.0 };
    for el in std::iter::once(rocket).chain(stages.iter().copied()) {
        if let Some(m) = el.num("overridemass") {
            dry = m;
            w.warnings.push(format!("{} has an override mass of {m:.2} kg: used as the dry mass.", el.name));
        }
        if let Some(c) = el.num("overridecg") {
            cg = c;
        }
    }
    if dry <= 0.5 {
        return Err("The OpenRocket rocket has no mass (no materials or mass components).".into());
    }
    let r = if w.max_radius > 0.0 { w.max_radius } else { radius };
    let (nose_len, _, shape) = w.nose.clone().unwrap_or((reference.nose_length_m, r, "ogive".into()));
    let xn = match shape.as_str() {
        "conical" => 2.0 / 3.0 * nose_len,
        "ellipsoid" => 0.333 * nose_len,
        "haack" | "power" | "parabolic" => 0.5 * nose_len,
        _ => 0.466 * nose_len,
    };
    let (fins, cna_f, xf) = match w.fins.clone() {
        Some((f, sweep, le)) => {
            let (cna, xf) = barrowman_fins(&f, sweep, le, r);
            (f, cna, xf)
        }
        None => {
            w.warnings.push("No trapezoidal fin set: the reference fins are kept for drag and stability.".into());
            (reference.fins.clone(), reference.cn_alpha_per_rad - 2.0, cg + reference.static_margin_cal * 2.0 * r)
        }
    };
    let cna = 2.0 + cna_f;
    let cp = (2.0 * xn + cna_f * xf) / cna;
    let margin = (cp - cg) / (2.0 * r);
    if margin < 1.0 {
        w.warnings.push(format!("Static margin {margin:.2} cal (without the propellant) is below 1 cal."));
    }
    if w.motor_mount.is_none() {
        w.warnings.push("No inner tube is marked as the motor mount.".into());
    }
    let vehicle = Vehicle {
        length_m: length,
        diameter_m: 2.0 * r,
        dry_mass_kg: dry,
        nose_length_m: nose_len,
        fins,
        surface_roughness_m: w.finish.unwrap_or(reference.surface_roughness_m),
        cn_alpha_per_rad: cna,
        static_margin_cal: margin.clamp(0.5, 6.0),
        ..reference.clone()
    };
    Ok(Imported { name, vehicle, cg_from_nose_m: cg, cp_from_nose_m: cp, motor_mount: w.motor_mount, components: w.components, warnings: w.warnings })
}
