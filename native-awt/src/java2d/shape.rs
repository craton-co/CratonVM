// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `sun.java2d.pipe.ShapeSpanIterator`: scan conversion of a path into pixel
//! spans, with the JDK's rules:
//!
//! * a pixel is inside when its centre `(x + 0.5, y + 0.5)` is inside the
//!   path (a crossing at `xc` starts a span at `ceil(xc - 0.5)`);
//! * with `adjust` ("stroke normalisation", the default stroke hint) every
//!   path point is moved to `floor(v + 0.25) + 0.25`, curve control points
//!   follow by the average of the adjustments of their end points;
//! * curves are flattened by recursive subdivision.
//!
//! Pure Rust; the VM glue lives in the parent module.

const SEG_MOVETO: i32 = 0;
const SEG_LINETO: i32 = 1;
const SEG_QUADTO: i32 = 2;
const SEG_CUBICTO: i32 = 3;
const SEG_CLOSE: i32 = 4;

/// Squared flatness bound of curve subdivision (pixels squared).
const MAX_FLAT_SQ: f64 = 1.0;
/// Subdivision depth cap.
const MAX_LEVEL: u32 = 10;

#[derive(Clone, Copy, Debug)]
struct Edge {
    x0: f64,
    y0: f64,
    slope: f64,
    /// First and one-past-last scanline whose centre the edge crosses.
    first: i32,
    last: i32,
    dir: i32,
}

/// The iterator's state.
pub struct ShapeIter {
    adjust: bool,
    even_odd: bool,
    lox: i32,
    loy: i32,
    hix: i32,
    hiy: i32,
    cx: f32,
    cy: f32,
    mx: f32,
    my: f32,
    adjx: f32,
    adjy: f32,
    have_cur: bool,
    path_lox: f32,
    path_loy: f32,
    path_hix: f32,
    path_hiy: f32,
    edges: Vec<Edge>,
    // iteration
    prepared: bool,
    order: Vec<usize>,
    next_edge: usize,
    active: Vec<usize>,
    y: i32,
    pending: Vec<(i32, i32)>,
    pending_pos: usize,
}

impl ShapeIter {
    /// A new iterator; `adjust` is the stroke-normalisation flag.
    pub fn new(adjust: bool) -> Self {
        ShapeIter {
            adjust,
            even_odd: false,
            lox: i32::MIN,
            loy: i32::MIN,
            hix: i32::MAX,
            hiy: i32::MAX,
            cx: 0.0,
            cy: 0.0,
            mx: 0.0,
            my: 0.0,
            adjx: 0.0,
            adjy: 0.0,
            have_cur: false,
            path_lox: f32::MAX,
            path_loy: f32::MAX,
            path_hix: f32::MIN,
            path_hiy: f32::MIN,
            edges: Vec::new(),
            prepared: false,
            order: Vec::new(),
            next_edge: 0,
            active: Vec::new(),
            y: 0,
            pending: Vec::new(),
            pending_pos: 0,
        }
    }

    pub fn set_normalize(&mut self, adjust: bool) {
        self.adjust = adjust;
    }

    /// 0 = even-odd, 1 = non-zero (`PathIterator.WIND_*`).
    pub fn set_rule(&mut self, rule: i32) {
        self.even_odd = rule == 0;
    }

    pub fn set_output_area(&mut self, lox: i32, loy: i32, hix: i32, hiy: i32) {
        self.lox = lox;
        self.loy = loy;
        self.hix = hix;
        self.hiy = hiy;
    }

    pub fn intersect_clip_box(&mut self, lox: i32, loy: i32, hix: i32, hiy: i32) {
        self.lox = self.lox.max(lox);
        self.loy = self.loy.max(loy);
        self.hix = self.hix.min(hix);
        self.hiy = self.hiy.min(hiy);
    }

    fn adjusted(&self, x: f32, y: f32) -> (f32, f32) {
        if self.adjust {
            ((x + 0.25).floor() + 0.25, (y + 0.25).floor() + 0.25)
        } else {
            (x, y)
        }
    }

    fn box_point(&mut self, x: f32, y: f32) {
        if x < self.path_lox {
            self.path_lox = x;
        }
        if x > self.path_hix {
            self.path_hix = x;
        }
        if y < self.path_loy {
            self.path_loy = y;
        }
        if y > self.path_hiy {
            self.path_hiy = y;
        }
    }

    pub fn get_path_box(&self) -> [i32; 4] {
        if self.path_lox > self.path_hix {
            return [0, 0, 0, 0];
        }
        [
            self.path_lox.floor() as i32,
            self.path_loy.floor() as i32,
            self.path_hix.ceil() as i32,
            self.path_hiy.ceil() as i32,
        ]
    }

    fn add_edge(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.prepared = false;
        let (mut ax, mut ay, mut bx, mut by) = (x0 as f64, y0 as f64, x1 as f64, y1 as f64);
        let mut dir = 1;
        if ay > by {
            std::mem::swap(&mut ax, &mut bx);
            std::mem::swap(&mut ay, &mut by);
            dir = -1;
        }
        let first = (ay - 0.5).ceil() as i64;
        let last = (by - 0.5).ceil() as i64;
        if first >= last {
            return;
        }
        let first = first.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        let last = last.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        self.edges.push(Edge {
            x0: ax,
            y0: ay,
            slope: (bx - ax) / (by - ay),
            first,
            last,
            dir,
        });
    }

    pub fn move_to(&mut self, x: f32, y: f32) {
        if self.have_cur {
            self.close_subpath();
        }
        let (nx, ny) = self.adjusted(x, y);
        self.adjx = nx - x;
        self.adjy = ny - y;
        self.cx = nx;
        self.cy = ny;
        self.mx = nx;
        self.my = ny;
        self.have_cur = true;
        self.box_point(nx, ny);
    }

    fn close_subpath(&mut self) {
        if self.have_cur && (self.cx != self.mx || self.cy != self.my) {
            let (cx, cy, mx, my) = (self.cx, self.cy, self.mx, self.my);
            self.add_edge(cx, cy, mx, my);
        }
        self.cx = self.mx;
        self.cy = self.my;
    }

    pub fn line_to(&mut self, x: f32, y: f32) {
        if !self.have_cur {
            self.move_to(x, y);
            return;
        }
        let (nx, ny) = self.adjusted(x, y);
        self.adjx = nx - x;
        self.adjy = ny - y;
        let (cx, cy) = (self.cx, self.cy);
        self.add_edge(cx, cy, nx, ny);
        self.cx = nx;
        self.cy = ny;
        self.box_point(nx, ny);
    }

    pub fn quad_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) {
        if !self.have_cur {
            self.move_to(x2, y2);
            return;
        }
        let (oax, oay) = (self.adjx, self.adjy);
        let (nx, ny) = self.adjusted(x2, y2);
        let (nax, nay) = (nx - x2, ny - y2);
        // The control point moves by the mean of the end-point adjustments.
        let (cx1, cy1) = (x1 + (oax + nax) / 2.0, y1 + (oay + nay) / 2.0);
        self.adjx = nax;
        self.adjy = nay;
        let (x0, y0) = (self.cx, self.cy);
        self.subdivide_quad(
            0, x0 as f64, y0 as f64, cx1 as f64, cy1 as f64, nx as f64, ny as f64,
        );
        self.cx = nx;
        self.cy = ny;
    }

    pub fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
        if !self.have_cur {
            self.move_to(x3, y3);
            return;
        }
        let (oax, oay) = (self.adjx, self.adjy);
        let (nx, ny) = self.adjusted(x3, y3);
        let (nax, nay) = (nx - x3, ny - y3);
        let (c1x, c1y) = (x1 + oax, y1 + oay);
        let (c2x, c2y) = (x2 + nax, y2 + nay);
        self.adjx = nax;
        self.adjy = nay;
        let (x0, y0) = (self.cx, self.cy);
        self.subdivide_cubic(
            0,
            [
                x0 as f64, y0 as f64, c1x as f64, c1y as f64, c2x as f64, c2y as f64, nx as f64,
                ny as f64,
            ],
        );
        self.cx = nx;
        self.cy = ny;
    }

    fn seg_to(&mut self, x: f64, y: f64) {
        let (x0, y0) = (self.cx, self.cy);
        self.add_edge(x0, y0, x as f32, y as f32);
        self.cx = x as f32;
        self.cy = y as f32;
        self.box_point(x as f32, y as f32);
    }

    fn subdivide_quad(&mut self, level: u32, x0: f64, y0: f64, x1: f64, y1: f64, x2: f64, y2: f64) {
        if level < MAX_LEVEL && pt_seg_dist_sq(x0, y0, x2, y2, x1, y1) > MAX_FLAT_SQ {
            let (cx, cy) = ((x0 + 2.0 * x1 + x2) / 4.0, (y0 + 2.0 * y1 + y2) / 4.0);
            let (ax, ay) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
            let (bx, by) = ((x1 + x2) / 2.0, (y1 + y2) / 2.0);
            self.subdivide_quad(level + 1, x0, y0, ax, ay, cx, cy);
            self.subdivide_quad(level + 1, cx, cy, bx, by, x2, y2);
        } else {
            self.seg_to(x2, y2);
        }
    }

    fn subdivide_cubic(&mut self, level: u32, c: [f64; 8]) {
        let flat = pt_seg_dist_sq(c[0], c[1], c[6], c[7], c[2], c[3]) <= MAX_FLAT_SQ
            && pt_seg_dist_sq(c[0], c[1], c[6], c[7], c[4], c[5]) <= MAX_FLAT_SQ;
        if level >= MAX_LEVEL || flat {
            self.seg_to(c[6], c[7]);
            return;
        }
        let (x0, y0, x1, y1, x2, y2, x3, y3) = (c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]);
        let (ab_x, ab_y) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let (bc_x, bc_y) = ((x1 + x2) / 2.0, (y1 + y2) / 2.0);
        let (cd_x, cd_y) = ((x2 + x3) / 2.0, (y2 + y3) / 2.0);
        let (abc_x, abc_y) = ((ab_x + bc_x) / 2.0, (ab_y + bc_y) / 2.0);
        let (bcd_x, bcd_y) = ((bc_x + cd_x) / 2.0, (bc_y + cd_y) / 2.0);
        let (m_x, m_y) = ((abc_x + bcd_x) / 2.0, (abc_y + bcd_y) / 2.0);
        self.subdivide_cubic(level + 1, [x0, y0, ab_x, ab_y, abc_x, abc_y, m_x, m_y]);
        self.subdivide_cubic(level + 1, [m_x, m_y, bcd_x, bcd_y, cd_x, cd_y, x3, y3]);
    }

    pub fn close_path(&mut self) {
        self.close_subpath();
    }

    pub fn path_done(&mut self) {
        if self.have_cur {
            self.close_subpath();
        }
        self.have_cur = false;
    }

    /// `addSegment(type, coords)` with `PathIterator` segment types.
    pub fn add_segment(&mut self, seg: i32, c: &[f32]) {
        match seg {
            SEG_MOVETO => self.move_to(c[0], c[1]),
            SEG_LINETO => self.line_to(c[0], c[1]),
            SEG_QUADTO => self.quad_to(c[0], c[1], c[2], c[3]),
            SEG_CUBICTO => self.curve_to(c[0], c[1], c[2], c[3], c[4], c[5]),
            SEG_CLOSE => self.close_path(),
            _ => {}
        }
    }

    /// `appendPoly`: a closed polygon of integer vertices.
    pub fn append_poly(&mut self, xs: &[i32], ys: &[i32], xoff: i32, yoff: i32) {
        let n = xs.len().min(ys.len());
        if n == 0 {
            return;
        }
        let pt = |i: usize| ((xs[i] + xoff) as f32, (ys[i] + yoff) as f32);
        let (x, y) = pt(0);
        self.move_to(x, y);
        for i in 1..n {
            let (x, y) = pt(i);
            self.line_to(x, y);
        }
        self.close_path();
        self.have_cur = false;
    }

    fn prepare(&mut self) {
        self.order = (0..self.edges.len()).collect();
        let edges = &self.edges;
        self.order.sort_by_key(|&i| edges[i].first);
        self.next_edge = 0;
        self.active.clear();
        self.pending.clear();
        self.pending_pos = 0;
        self.y = match self.order.first() {
            Some(&i) => self.edges[i].first.max(self.loy),
            None => self.hiy,
        };
        self.prepared = true;
    }

    /// Fill `pending` with the spans of the current scanline; advance `y`.
    fn fill_row(&mut self) -> bool {
        loop {
            if self.y >= self.hiy {
                return false;
            }
            while self.next_edge < self.order.len()
                && self.edges[self.order[self.next_edge]].first <= self.y
            {
                self.active.push(self.order[self.next_edge]);
                self.next_edge += 1;
            }
            let y = self.y;
            let edges = &self.edges;
            self.active.retain(|&i| edges[i].last > y);
            if self.active.is_empty() {
                match self.order.get(self.next_edge) {
                    Some(&i) => {
                        self.y = self.edges[i].first.max(self.loy);
                        if self.y >= self.hiy {
                            return false;
                        }
                        continue;
                    }
                    None => return false,
                }
            }
            let yc = y as f64 + 0.5;
            let mut xs: Vec<(i64, i32)> = self
                .active
                .iter()
                .map(|&i| {
                    let e = &self.edges[i];
                    let xc = e.x0 + (yc - e.y0) * e.slope;
                    ((xc - 0.5).ceil() as i64, e.dir)
                })
                .collect();
            xs.sort_by_key(|p| p.0);
            self.pending.clear();
            self.pending_pos = 0;
            let (lox, hix) = (self.lox as i64, self.hix as i64);
            let mut wind = 0i32;
            let mut start = 0i64;
            for &(x, d) in &xs {
                let was_in = if self.even_odd {
                    wind & 1 != 0
                } else {
                    wind != 0
                };
                wind += d;
                let is_in = if self.even_odd {
                    wind & 1 != 0
                } else {
                    wind != 0
                };
                if !was_in && is_in {
                    start = x;
                } else if was_in && !is_in {
                    let (a, b) = (start.max(lox), x.min(hix));
                    if a < b {
                        // Merge with the previous span when they touch.
                        match self.pending.last_mut() {
                            Some(last) if last.1 as i64 == a => last.1 = b as i32,
                            _ => self.pending.push((a as i32, b as i32)),
                        }
                    }
                }
            }
            self.y += 1;
            if !self.pending.is_empty() {
                return true;
            }
        }
    }

    /// The next span `[x0, y0, x1, y1)` (one scanline tall) or `None`.
    pub fn next_span(&mut self) -> Option<[i32; 4]> {
        if !self.prepared {
            self.prepare();
        }
        loop {
            if self.pending_pos < self.pending.len() {
                let (a, b) = self.pending[self.pending_pos];
                self.pending_pos += 1;
                let row = self.y - 1;
                return Some([a, row, b, row + 1]);
            }
            if !self.fill_row() {
                return None;
            }
        }
    }

    /// `skipDownTo(y)`: discard every span above scanline `y`. Edges are
    /// activated lazily by `fill_row`, so jumping forward is safe.
    pub fn skip_down_to(&mut self, y: i32) {
        if !self.prepared {
            self.prepare();
        }
        self.pending.clear();
        self.pending_pos = 0;
        if self.y < y {
            self.y = y;
        }
    }
}

/// Squared distance from point `(px, py)` to the segment `(x1,y1)-(x2,y2)`.
fn pt_seg_dist_sq(x1: f64, y1: f64, x2: f64, y2: f64, px: f64, py: f64) -> f64 {
    let (dx, dy) = (x2 - x1, y2 - y1);
    let (mut pxr, mut pyr) = (px - x1, py - y1);
    let mut dot = pxr * dx + pyr * dy;
    let proj_sq = if dot <= 0.0 {
        0.0
    } else {
        pxr = dx - pxr;
        pyr = dy - pyr;
        dot = pxr * dx + pyr * dy;
        if dot <= 0.0 {
            0.0
        } else {
            dot * dot / (dx * dx + dy * dy)
        }
    };
    let len_sq = pxr * pxr + pyr * pyr;
    let d = len_sq - proj_sq;
    if d < 0.0 {
        0.0
    } else {
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(mut it: ShapeIter) -> Vec<[i32; 4]> {
        let mut v = Vec::new();
        while let Some(s) = it.next_span() {
            v.push(s);
        }
        v
    }

    #[test]
    fn integer_rectangle() {
        let mut it = ShapeIter::new(false);
        it.set_rule(1);
        it.move_to(2.0, 1.0);
        it.line_to(6.0, 1.0);
        it.line_to(6.0, 3.0);
        it.line_to(2.0, 3.0);
        it.close_path();
        it.path_done();
        let v = collect(it);
        assert_eq!(v, vec![[2, 1, 6, 2], [2, 2, 6, 3]]);
    }

    #[test]
    fn clip_box_limits_spans() {
        let mut it = ShapeIter::new(false);
        it.set_rule(1);
        it.set_output_area(3, 2, 5, 3);
        it.append_poly(&[2, 6, 6, 2], &[1, 1, 3, 3], 0, 0);
        let v = collect(it);
        assert_eq!(v, vec![[3, 2, 5, 3]]);
    }

    #[test]
    fn even_odd_hole() {
        let mut it = ShapeIter::new(false);
        it.set_rule(0);
        for (x0, y0, x1, y1) in [(0.0f32, 0.0f32, 6.0f32, 6.0f32), (2.0, 2.0, 4.0, 4.0)] {
            it.move_to(x0, y0);
            it.line_to(x1, y0);
            it.line_to(x1, y1);
            it.line_to(x0, y1);
            it.close_path();
        }
        it.path_done();
        let v = collect(it);
        assert!(v.contains(&[0, 2, 2, 3]) && v.contains(&[4, 2, 6, 3]));
        assert!(!v.iter().any(|s| s[1] == 2 && s[0] == 2));
    }
}
