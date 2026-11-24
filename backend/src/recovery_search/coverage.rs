//! Search coverage on a bounded raster (docs/backend/recovery-search.md).
//!
//! Everything happens in a local east/north frame in metres around a
//! reference point (equirectangular: accurate to well under a metre over the
//! few kilometres a recovery search spans).
//!
//! 1. Each searcher's track is split into segments (a gap of more than
//!    [`GAP_MS`] or [`GAP_M`] breaks it: a lost GPS fix is not coverage).
//! 2. A cell is covered when its centre lies within half the swath width of
//!    any segment (a capsule around the segment). Overlapping tracks set the
//!    same cells, so nothing is counted twice: that is the union.
//! 3. The predicted landing polygon is scan-converted onto its own grid, and
//!    `covered ∩ target` / `target` is the percentage. `target \ covered` is
//!    the uncovered area.
//! 4. Covered and uncovered cells are traced into polygons. A component with
//!    holes is cut by a zero-width vertical bridge from each hole up to its
//!    outer boundary, so every component is one weakly simple ring that any
//!    renderer fills correctly (a hole stays visible as a hole).
//!
//! Both grids are capped at [`MAX_CELLS`] cells: the cell size starts at
//! `swath / 8` clamped to [`MIN_CELL_M`]..[`BASE_CELL_MAX_M`] and grows only
//! when an area would need more cells. The work is O(cells + segments x
//! cells per capsule), a few milliseconds to a few tens of milliseconds on a
//! Raspberry Pi 5 for a multi-hour search.
use std::collections::{HashMap, HashSet, VecDeque};

const EARTH_RADIUS_M: f64 = 6_371_008.8;
/// The largest grid evaluated, per grid (the target grid and the display grid).
pub const MAX_CELLS: usize = 250_000;
pub const MIN_CELL_M: f64 = 2.0;
pub const BASE_CELL_MAX_M: f64 = 5.0;
/// Most points in one output ring (after simplification).
pub const MAX_RING_POINTS: usize = 400;
/// Most polygons of each kind in one overlay (the largest are kept).
pub const MAX_AREAS: usize = 64;
/// A pause longer than this between two fixes breaks the track.
pub const GAP_MS: i64 = 120_000;
/// A jump longer than this between two fixes breaks the track.
pub const GAP_M: f64 = 150.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fix {
    pub lat: f64,
    pub lon: f64,
    pub t_ms: i64,
}

/// Local east/north metres around a reference point.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    lat0: f64,
    lon0: f64,
    kx: f64,
    ky: f64,
}

impl Frame {
    pub fn new(lat0: f64, lon0: f64) -> Self {
        let ky = EARTH_RADIUS_M.to_radians();
        Self { lat0, lon0, kx: ky * lat0.to_radians().cos().max(1e-6), ky }
    }
    pub fn to_local(&self, lat: f64, lon: f64) -> (f64, f64) {
        ((lon - self.lon0) * self.kx, (lat - self.lat0) * self.ky)
    }
    pub fn to_geo(&self, x: f64, y: f64) -> (f64, f64) {
        (self.lat0 + y / self.ky, self.lon0 + x / self.kx)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Coverage {
    /// Area of the target polygon (m², as rasterized). None without a target.
    pub target_m2: Option<f64>,
    pub covered_in_target_m2: Option<f64>,
    pub uncovered_m2: Option<f64>,
    /// `covered_in_target / target`, 0–100. None without a target.
    pub percent: Option<f64>,
    /// Everything swept, inside or outside the target.
    pub swath_m2: f64,
    /// Cell size of the display grid and of the target grid.
    pub cell_m: Option<f64>,
    pub target_cell_m: Option<f64>,
    /// Cells evaluated (both grids): the CPU budget actually used.
    pub cells: usize,
    /// `[lat, lon]` rings, first point not repeated.
    pub swath_polygons: Vec<Vec<[f64; 2]>>,
    pub uncovered_polygons: Vec<Vec<[f64; 2]>>,
}

/// One searcher's fixes broken into continuous segments, in local metres.
fn segments(frame: &Frame, track: &[Fix]) -> Vec<Vec<(f64, f64)>> {
    let mut out: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut last: Option<(f64, f64, i64)> = None;
    for fix in track {
        let (x, y) = frame.to_local(fix.lat, fix.lon);
        let broken = match last {
            None => true,
            Some((lx, ly, lt)) => fix.t_ms - lt > GAP_MS || ((x - lx).powi(2) + (y - ly).powi(2)).sqrt() > GAP_M,
        };
        if broken {
            out.push(Vec::new());
        }
        out.last_mut().unwrap().push((x, y));
        last = Some((x, y, fix.t_ms));
    }
    out
}

struct Grid {
    x0: f64,
    y0: f64,
    cell: f64,
    nx: usize,
    ny: usize,
    bits: Vec<bool>,
}

impl Grid {
    /// A grid over `[x0, x1] x [y0, y1]` with at most [`MAX_CELLS`] cells.
    fn over(x0: f64, y0: f64, x1: f64, y1: f64, base: f64) -> Self {
        let (w, h) = ((x1 - x0).max(base), (y1 - y0).max(base));
        let mut cell = base.max((w * h / MAX_CELLS as f64).sqrt());
        loop {
            let (nx, ny) = ((w / cell).ceil() as usize, (h / cell).ceil() as usize);
            if nx * ny <= MAX_CELLS {
                return Self { x0, y0, cell, nx: nx.max(1), ny: ny.max(1), bits: vec![false; nx.max(1) * ny.max(1)] };
            }
            cell *= 1.02;
        }
    }
    fn get(&self, r: i64, c: i64) -> bool {
        r >= 0 && c >= 0 && (r as usize) < self.ny && (c as usize) < self.nx && self.bits[r as usize * self.nx + c as usize]
    }
    fn count(&self) -> usize {
        self.bits.iter().filter(|b| **b).count()
    }
    /// Marks every cell whose centre is within `radius` of the segment a–b.
    fn stamp_capsule(&mut self, a: (f64, f64), b: (f64, f64), radius: f64) {
        let (lo_x, hi_x) = (a.0.min(b.0) - radius, a.0.max(b.0) + radius);
        let (lo_y, hi_y) = (a.1.min(b.1) - radius, a.1.max(b.1) + radius);
        let c0 = ((lo_x - self.x0) / self.cell - 0.5).ceil().max(0.0) as i64;
        let c1 = (((hi_x - self.x0) / self.cell - 0.5).floor() as i64).min(self.nx as i64 - 1);
        let r0 = ((lo_y - self.y0) / self.cell - 0.5).ceil().max(0.0) as i64;
        let r1 = (((hi_y - self.y0) / self.cell - 0.5).floor() as i64).min(self.ny as i64 - 1);
        if c0 > c1 || r0 > r1 {
            return;
        }
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = dx * dx + dy * dy;
        let r2 = radius * radius;
        for r in r0..=r1 {
            let py = self.y0 + (r as f64 + 0.5) * self.cell;
            let row = r as usize * self.nx;
            for c in c0..=c1 {
                let px = self.x0 + (c as f64 + 0.5) * self.cell;
                let t = if len2 > 0.0 { (((px - a.0) * dx + (py - a.1) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
                let (ex, ey) = (a.0 + t * dx - px, a.1 + t * dy - py);
                if ex * ex + ey * ey <= r2 {
                    self.bits[row + c as usize] = true;
                }
            }
        }
    }
    fn stamp_tracks(&mut self, tracks: &[Vec<Vec<(f64, f64)>>], radius: f64) {
        for segment in tracks.iter().flatten() {
            if segment.len() == 1 {
                self.stamp_capsule(segment[0], segment[0], radius);
            }
            for pair in segment.windows(2) {
                self.stamp_capsule(pair[0], pair[1], radius);
            }
        }
    }
    /// Scan-converts a polygon (even-odd rule on cell centres).
    fn fill_polygon(&mut self, ring: &[(f64, f64)]) {
        let mut xs = Vec::new();
        for r in 0..self.ny {
            let yc = self.y0 + (r as f64 + 0.5) * self.cell;
            xs.clear();
            for i in 0..ring.len() {
                let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
                if (a.1 > yc) != (b.1 > yc) {
                    xs.push(a.0 + (yc - a.1) * (b.0 - a.0) / (b.1 - a.1));
                }
            }
            xs.sort_by(f64::total_cmp);
            for pair in xs.chunks_exact(2) {
                let c0 = ((pair[0] - self.x0) / self.cell - 0.5).ceil().max(0.0) as i64;
                let c1 = (((pair[1] - self.x0) / self.cell - 0.5).floor() as i64).min(self.nx as i64 - 1);
                for c in c0.max(0)..=c1 {
                    self.bits[r * self.nx + c as usize] = true;
                }
            }
        }
    }
}

/// Traces the set cells of `inside` into rings of lattice vertices `(x, y)`
/// (x = column edge, y = row edge), counter-clockwise, one ring per
/// 4-connected component, holes bridged to the outside (see the module docs).
fn trace(nx: usize, ny: usize, inside: impl Fn(i64, i64) -> bool) -> Vec<(Vec<(i64, i64)>, usize)> {
    // 1. Label the unset cells (8-connected) on a grid padded by one cell:
    //    the component touching the padding is the outside, the others are holes.
    let (pw, ph) = (nx + 2, ny + 2);
    let pidx = |r: i64, c: i64| (r + 1) as usize * pw + (c + 1) as usize;
    let mut label = vec![u32::MAX; pw * ph];
    let mut tops: Vec<(i64, i64)> = Vec::new();
    let mut queue = VecDeque::new();
    let mut next = 0u32;
    for start in 0..pw * ph {
        let (sr, sc) = ((start / pw) as i64 - 1, (start % pw) as i64 - 1);
        if label[start] != u32::MAX || inside(sr, sc) {
            continue;
        }
        label[start] = next;
        let mut top = (sr, sc);
        queue.push_back((sr, sc));
        while let Some((r, c)) = queue.pop_front() {
            if r > top.0 || (r == top.0 && c < top.1) {
                top = (r, c);
            }
            for dr in -1..=1 {
                for dc in -1..=1 {
                    let (rr, cc) = (r + dr, c + dc);
                    if rr < -1 || cc < -1 || rr > ny as i64 || cc > nx as i64 {
                        continue;
                    }
                    let i = pidx(rr, cc);
                    if label[i] == u32::MAX && !inside(rr, cc) {
                        label[i] = next;
                        queue.push_back((rr, cc));
                    }
                }
            }
        }
        tops.push(top);
        next += 1;
    }
    // 2. A zero-width wall up from the top of each hole (label 0 is the outside,
    //    labelled first from the padding corner). Wall (x, r): the vertical edge
    //    at column edge x between cells (r, x-1) and (r, x).
    let mut walls: HashSet<(i64, i64)> = HashSet::new();
    for &(r, c) in tops.iter().skip(1) {
        let mut row = r + 1;
        while inside(row, c - 1) && inside(row, c) {
            walls.insert((c, row));
            row += 1;
        }
    }
    // 3. Directed boundary edges, the inside on the left.
    let mut from: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    let mut edges: Vec<((i64, i64), (i64, i64))> = Vec::new();
    let mut add = |a: (i64, i64), b: (i64, i64)| {
        from.entry(a).or_default().push(edges.len());
        edges.push((a, b));
    };
    for r in 0..ny as i64 {
        for c in 0..nx as i64 {
            if !inside(r, c) {
                continue;
            }
            if !inside(r - 1, c) {
                add((c, r), (c + 1, r));
            }
            if !inside(r, c + 1) || walls.contains(&(c + 1, r)) {
                add((c + 1, r), (c + 1, r + 1));
            }
            if !inside(r + 1, c) {
                add((c + 1, r + 1), (c, r + 1));
            }
            if !inside(r, c - 1) || walls.contains(&(c, r)) {
                add((c, r + 1), (c, r));
            }
        }
    }
    // 4. Follow the edges: at each vertex take the left turn, then straight,
    //    then right (keeps diagonal cells apart and walks into each bridge).
    let dir_of = |e: &((i64, i64), (i64, i64))| ((e.1.0 - e.0.0), (e.1.1 - e.0.1));
    let mut used = vec![false; edges.len()];
    let mut rings = Vec::new();
    for first in 0..edges.len() {
        if used[first] {
            continue;
        }
        let mut ring = Vec::new();
        let mut e = first;
        loop {
            used[e] = true;
            ring.push(edges[e].0);
            let (dx, dy) = dir_of(&edges[e]);
            let order = [(-dy, dx), (dx, dy), (dy, -dx), (-dx, -dy)];
            let outs = from.get(&edges[e].1).map(Vec::as_slice).unwrap_or(&[]);
            let next = order.iter().find_map(|d| outs.iter().copied().find(|&o| dir_of(&edges[o]) == *d));
            match next {
                Some(n) if n == first => break,
                Some(n) if !used[n] => e = n,
                _ => break,
            }
        }
        // Collapse collinear runs.
        let n = ring.len();
        let mut simple = Vec::with_capacity(n);
        for i in 0..n {
            let (p, v, q) = (ring[(i + n - 1) % n], ring[i], ring[(i + 1) % n]);
            let d1 = ((v.0 - p.0).signum(), (v.1 - p.1).signum());
            let d2 = ((q.0 - v.0).signum(), (q.1 - v.1).signum());
            if d1 != d2 {
                simple.push(v);
            }
        }
        // Area in cells (shoelace; a bridged ring counts only its real area).
        let area2: i64 = (0..ring.len()).map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            a.0 * b.1 - b.0 * a.1
        }).sum();
        if area2 > 0 && simple.len() >= 3 {
            rings.push((simple, (area2 / 2) as usize));
        }
    }
    rings
}

/// Douglas–Peucker on an open polyline, keeping the end points.
fn simplify_open(points: &[(f64, f64)], tol: f64, out: &mut Vec<(f64, f64)>) {
    if points.len() < 3 {
        out.extend_from_slice(&points[..points.len().saturating_sub(1)]);
        return;
    }
    let (a, b) = (points[0], points[points.len() - 1]);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    let (mut worst, mut at) = (0.0, 0);
    for (i, p) in points.iter().enumerate().take(points.len() - 1).skip(1) {
        let d = if len > 0.0 { ((p.0 - a.0) * dy - (p.1 - a.1) * dx).abs() / len } else { ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt() };
        if d > worst {
            (worst, at) = (d, i);
        }
    }
    if worst > tol {
        simplify_open(&points[..=at], tol, out);
        simplify_open(&points[at..], tol, out);
    } else {
        out.push(a);
    }
}

/// Simplifies a closed ring to at most [`MAX_RING_POINTS`] points.
fn simplify_ring(ring: &[(f64, f64)], cell: f64) -> Vec<(f64, f64)> {
    if ring.len() <= 4 {
        return ring.to_vec();
    }
    // Split at the vertex farthest from the first one.
    let far = (1..ring.len())
        .max_by(|&i, &j| {
            let d = |k: usize| (ring[k].0 - ring[0].0).powi(2) + (ring[k].1 - ring[0].1).powi(2);
            d(i).total_cmp(&d(j))
        })
        .unwrap_or(1);
    let mut tol = cell * 0.5;
    loop {
        let mut out = Vec::new();
        simplify_open(&ring[..=far], tol, &mut out);
        let mut second: Vec<(f64, f64)> = ring[far..].to_vec();
        second.push(ring[0]);
        simplify_open(&second, tol, &mut out);
        if out.len() <= MAX_RING_POINTS || tol > 1e6 {
            return out;
        }
        tol *= 1.5;
    }
}

fn round7(v: f64) -> f64 {
    (v * 1e7).round() / 1e7
}

/// Rings of a grid's set cells as `[lat, lon]` polygons, largest first,
/// components smaller than `min_cells` dropped.
fn polygons(grid: &Grid, frame: &Frame, min_cells: usize) -> Vec<Vec<[f64; 2]>> {
    let mut rings = trace(grid.nx, grid.ny, |r, c| grid.get(r, c));
    rings.retain(|(_, area)| *area >= min_cells);
    rings.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rings.truncate(MAX_AREAS);
    rings
        .into_iter()
        .map(|(ring, _)| {
            let metres: Vec<(f64, f64)> =
                ring.iter().map(|&(x, y)| (grid.x0 + x as f64 * grid.cell, grid.y0 + y as f64 * grid.cell)).collect();
            simplify_ring(&metres, grid.cell)
                .into_iter()
                .map(|(x, y)| {
                    let (lat, lon) = frame.to_geo(x, y);
                    [round7(lat), round7(lon)]
                })
                .collect()
        })
        .collect()
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// The base cell size for a swath width.
pub fn base_cell(swath_m: f64) -> f64 {
    (swath_m / 8.0).clamp(MIN_CELL_M, BASE_CELL_MAX_M)
}

/// Computes the coverage of `tracks` (each ordered by time) with a swath of
/// `swath_m`, against an optional target polygon of `[lat, lon]` points.
pub fn compute(tracks: &[Vec<Fix>], target: Option<&[[f64; 2]]>, swath_m: f64) -> Coverage {
    let target = target.filter(|t| t.len() >= 3);
    let reference = target
        .and_then(|t| t.first().map(|p| (p[0], p[1])))
        .or_else(|| tracks.iter().flatten().next().map(|f| (f.lat, f.lon)));
    let Some((lat0, lon0)) = reference else { return Coverage::default() };
    let frame = Frame::new(lat0, lon0);
    let radius = swath_m / 2.0;
    let base = base_cell(swath_m);
    let local: Vec<Vec<Vec<(f64, f64)>>> = tracks.iter().map(|t| segments(&frame, t)).collect();
    let mut out = Coverage::default();

    // Display grid: everything swept.
    let mut bbox = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for &(x, y) in local.iter().flatten().flatten() {
        bbox = (bbox.0.min(x), bbox.1.min(y), bbox.2.max(x), bbox.3.max(y));
    }
    if bbox.0 <= bbox.2 {
        let pad = radius + base;
        let mut grid = Grid::over(bbox.0 - pad, bbox.1 - pad, bbox.2 + pad, bbox.3 + pad, base);
        grid.stamp_tracks(&local, radius);
        out.swath_m2 = round2(grid.count() as f64 * grid.cell * grid.cell);
        out.cell_m = Some(round2(grid.cell));
        out.cells += grid.nx * grid.ny;
        out.swath_polygons = polygons(&grid, &frame, 1);
    }

    // Target grid: the percentage and the uncovered remainder.
    if let Some(target) = target {
        let ring: Vec<(f64, f64)> = target.iter().map(|p| frame.to_local(p[0], p[1])).collect();
        let mut tb = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for &(x, y) in &ring {
            tb = (tb.0.min(x), tb.1.min(y), tb.2.max(x), tb.3.max(y));
        }
        let mut area = Grid::over(tb.0 - base, tb.1 - base, tb.2 + base, tb.3 + base, base);
        area.fill_polygon(&ring);
        let mut covered = Grid { bits: vec![false; area.bits.len()], ..area_shape(&area) };
        covered.stamp_tracks(&local, radius);
        let cell2 = area.cell * area.cell;
        let (mut target_cells, mut hit) = (0usize, 0usize);
        for (t, c) in area.bits.iter_mut().zip(&covered.bits) {
            if *t {
                target_cells += 1;
                if *c {
                    hit += 1;
                    *t = false; // what remains set is uncovered
                }
            }
        }
        out.cells += area.nx * area.ny;
        out.target_cell_m = Some(round2(area.cell));
        out.target_m2 = Some(round2(target_cells as f64 * cell2));
        out.covered_in_target_m2 = Some(round2(hit as f64 * cell2));
        out.uncovered_m2 = Some(round2((target_cells - hit) as f64 * cell2));
        out.percent = Some(if target_cells == 0 { 0.0 } else { round2(100.0 * hit as f64 / target_cells as f64) });
        // Specks under 2 cells are not worth drawing (still counted above).
        out.uncovered_polygons = polygons(&area, &frame, 2);
    }
    out
}

fn area_shape(g: &Grid) -> Grid {
    Grid { x0: g.x0, y0: g.y0, cell: g.cell, nx: g.nx, ny: g.ny, bits: Vec::new() }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A track along local metres from the frame origin at (31.758, -102.046).
    pub(crate) fn track(points: &[(f64, f64)]) -> Vec<Fix> {
        let frame = Frame::new(31.758, -102.046);
        points
            .iter()
            .enumerate()
            .map(|(i, &(x, y))| {
                let (lat, lon) = frame.to_geo(x, y);
                Fix { lat, lon, t_ms: i as i64 * 5_000 }
            })
            .collect()
    }

    /// A densely sampled straight walk (5 m steps) from a to b.
    pub(crate) fn walk(a: (f64, f64), b: (f64, f64)) -> Vec<Fix> {
        let n = ((((b.0 - a.0) as f64).hypot(b.1 - a.1)) / 5.0).ceil().max(1.0) as usize;
        track(&(0..=n).map(|i| (a.0 + (b.0 - a.0) * i as f64 / n as f64, a.1 + (b.1 - a.1) * i as f64 / n as f64)).collect::<Vec<_>>())
    }

    pub(crate) fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<[f64; 2]> {
        let frame = Frame::new(31.758, -102.046);
        [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
            .iter()
            .map(|&(x, y)| {
                let (lat, lon) = frame.to_geo(x, y);
                [lat, lon]
            })
            .collect()
    }

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn frame_round_trips() {
        let f = Frame::new(31.758, -102.046);
        let (lat, lon) = f.to_geo(1000.0, -500.0);
        let (x, y) = f.to_local(lat, lon);
        assert!(close(x, 1000.0, 1e-6) && close(y, -500.0, 1e-6));
    }

    #[test]
    fn a_straight_track_through_a_rectangle_covers_its_share() {
        // 400 m x 200 m target; a 30 m swath straight across its 200 m height
        // covers 30 x 200 = 6000 of 80000 m² = 7.5 %.
        let target = rect(0.0, 0.0, 400.0, 200.0);
        let tracks = vec![walk((200.0, -50.0), (200.0, 250.0))];
        let c = compute(&tracks, Some(&target), 30.0);
        let pct = c.percent.unwrap();
        assert!(close(pct, 7.5, 0.6), "{pct}");
        assert!(close(c.target_m2.unwrap(), 80_000.0, 1_500.0), "{:?}", c.target_m2);
        assert!(close(c.uncovered_m2.unwrap() + c.covered_in_target_m2.unwrap(), c.target_m2.unwrap(), 1e-6));
        // The swath outside the target counts towards the swept area only.
        assert!(close(c.swath_m2, 30.0 * 300.0 + std::f64::consts::PI * 225.0, 600.0), "{}", c.swath_m2);
        // The track splits the target into two uncovered parts and one swath.
        assert_eq!(c.uncovered_polygons.len(), 2, "{:?}", c.uncovered_polygons.len());
        assert_eq!(c.swath_polygons.len(), 1);
    }

    #[test]
    fn overlapping_tracks_are_not_counted_twice() {
        let target = rect(0.0, 0.0, 400.0, 200.0);
        let one = compute(&[walk((200.0, -50.0), (200.0, 250.0))], Some(&target), 30.0);
        let twice = compute(&[walk((200.0, -50.0), (200.0, 250.0)), walk((200.0, 250.0), (200.0, -50.0))], Some(&target), 30.0);
        assert_eq!(one.percent, twice.percent);
        assert_eq!(one.swath_m2, twice.swath_m2);
        // Half-overlapping (15 m apart): 45 m wide instead of 60.
        let half = compute(&[walk((200.0, -50.0), (200.0, 250.0)), walk((215.0, -50.0), (215.0, 250.0))], Some(&target), 30.0);
        assert!(close(half.percent.unwrap(), 45.0 * 200.0 / 80_000.0 * 100.0, 0.7), "{:?}", half.percent);
    }

    #[test]
    fn lawnmower_lanes_one_swath_apart_cover_everything() {
        let target = rect(0.0, 0.0, 300.0, 120.0);
        let mut tracks = Vec::new();
        let mut x = 15.0;
        while x < 300.0 {
            tracks.push(walk((x, -20.0), (x, 140.0)));
            x += 28.0;
        }
        let c = compute(&tracks, Some(&target), 30.0);
        assert!(c.percent.unwrap() > 99.0, "{:?}", c.percent);
        assert!(c.uncovered_polygons.is_empty(), "{}", c.uncovered_polygons.len());
    }

    #[test]
    fn a_missed_spot_inside_the_searched_area_is_reported_as_a_hole() {
        // A square loop of 60 m sides around the centre of a 100 m target
        // leaves the middle uncovered: the uncovered area is the outer frame
        // plus the hole in the middle.
        let target = rect(0.0, 0.0, 100.0, 100.0);
        let lap = track(&[(20.0, 20.0), (50.0, 20.0), (80.0, 20.0), (80.0, 50.0), (80.0, 80.0), (50.0, 80.0), (20.0, 80.0), (20.0, 50.0), (20.0, 20.0)]);
        let c = compute(&[lap], Some(&target), 20.0);
        // Middle hole: 60-20 = 40 m square = 1600 m².
        assert!(c.uncovered_polygons.len() >= 2, "{}", c.uncovered_polygons.len());
        // The covered loop is one ring with a bridged hole.
        assert_eq!(c.swath_polygons.len(), 1);
        // Band 10..90 m minus the 30..70 m hole: (6400 - 1600) / 10000.
        let p = c.percent.unwrap();
        assert!((p - 48.0).abs() < 1.5, "{p}");
    }

    #[test]
    fn a_gap_in_the_fixes_is_not_coverage() {
        let target = rect(0.0, 0.0, 400.0, 200.0);
        // Two fixes 300 m apart: a GPS dropout, not a walk. Only the two
        // discs around the fixes count (one half outside the target):
        // 1.5 x pi x 15² = 1060 m² of 80000.
        let c = compute(&[track(&[(0.0, 100.0), (300.0, 100.0)])], Some(&target), 30.0);
        assert!((c.percent.unwrap() - 1.33).abs() < 0.2, "{:?}", c.percent);
    }

    #[test]
    fn no_tracks_leaves_the_whole_target_uncovered() {
        let target = rect(0.0, 0.0, 200.0, 100.0);
        let c = compute(&[], Some(&target), 30.0);
        assert_eq!(c.percent, Some(0.0));
        assert_eq!(c.uncovered_polygons.len(), 1);
        assert!(c.swath_polygons.is_empty());
        assert!(compute(&[], None, 30.0).percent.is_none());
    }

    #[test]
    fn grids_stay_bounded_for_a_huge_area() {
        let target = rect(0.0, 0.0, 20_000.0, 20_000.0);
        let c = compute(&[walk((0.0, 0.0), (20_000.0, 20_000.0))], Some(&target), 30.0);
        assert!(c.cells <= 2 * MAX_CELLS, "{}", c.cells);
        assert!(c.swath_polygons.iter().all(|r| r.len() <= MAX_RING_POINTS));
    }

    #[test]
    fn tracing_bridges_holes_and_keeps_diagonals_apart() {
        // A 5x5 ring of cells with a 1-cell hole: one ring, area 24.
        let set = |r: i64, c: i64| (0..5).contains(&r) && (0..5).contains(&c) && !(r == 2 && c == 2);
        let rings = trace(5, 5, set);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].1, 24);
        // Two diagonal cells are two components.
        let diag = |r: i64, c: i64| (r == 0 && c == 0) || (r == 1 && c == 1);
        let rings = trace(2, 2, diag);
        assert_eq!(rings.len(), 2);
        assert!(rings.iter().all(|r| r.1 == 1));
    }

    /// CPU budget for a large search: 10 searchers walking 2 h each (one fix
    /// per 5 m, 2,160 fixes each) over a 1.2 km x 0.6 km landing area. Run
    /// with `--ignored --nocapture` (and `--release` for Pi-like timings).
    #[test]
    #[ignore]
    fn large_search_cost() {
        let target = rect(0.0, 0.0, 1200.0, 600.0);
        let tracks: Vec<Vec<Fix>> = (0..10)
            .map(|k| {
                let x = 30.0 + k as f64 * 115.0;
                let mut pts = Vec::new();
                for leg in 0..6 {
                    let (y0, y1) = if leg % 2 == 0 { (-50.0, 650.0) } else { (650.0, -50.0) };
                    pts.extend(walk((x + leg as f64 * 20.0, y0), (x + leg as f64 * 20.0, y1)));
                }
                pts.iter().enumerate().map(|(i, f)| Fix { t_ms: i as i64 * 3_300, ..*f }).collect()
            })
            .collect();
        let started = std::time::Instant::now();
        let c = compute(&tracks, Some(&target), 30.0);
        println!(
            "large_search_cost: {} fixes, {} cells, {:.1}% covered, {} swath + {} uncovered rings, {:?}",
            tracks.iter().map(Vec::len).sum::<usize>(),
            c.cells,
            c.percent.unwrap(),
            c.swath_polygons.len(),
            c.uncovered_polygons.len(),
            started.elapsed()
        );
    }
}
