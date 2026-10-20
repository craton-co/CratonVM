// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! FreeType's monochrome scan converter, `ftraster.c` (the 2.14 sources the JDK
//! bundles). Edges are chopped into monotone profiles sampled on pixel-centre
//! scanlines; spans run between rising ("left") and falling ("right") profiles
//! and a drop-out pass (vertical, then horizontal) lights pixels thin features
//! would otherwise lose.

use super::gray::Bitmap;
use super::interp::V;

const FLOW_UP: u16 = 0x08;
const OVERSHOOT_TOP: u16 = 0x10;
const OVERSHOOT_BOTTOM: u16 = 0x20;
const DROPOUT_PENDING: u16 = 0x40;

#[derive(Clone, Debug)]
struct Profile {
    flags: u16,
    /// First scanline (the bottom one once finalised).
    start: i64,
    /// Samples in traversal order (top to bottom for falling profiles).
    xs: Vec<i64>,
    /// Next profile of the contour (the last one links back to the first).
    next: Option<usize>,
}

struct Ras {
    bits: i64,
    precision: i64,
    half: i64,
    mask: i64,
    step: i64,
    scale: i64,
    min_y: i64,
    max_y: i64,
    /// 0 unknown, 1 ascending, 2 descending.
    state: u8,
    last_x: i64,
    last_y: i64,
    drop_out: u16,
    profs: Vec<Profile>,
    cur: Option<usize>,
    g_profile: Option<usize>,
}

impl Ras {
    fn floor(&self, x: i64) -> i64 {
        x & self.mask
    }
    fn ceiling(&self, x: i64) -> i64 {
        (x + self.precision - 1) & self.mask
    }
    fn trunc(&self, x: i64) -> i64 {
        x >> self.bits
    }
    fn scaled(&self, x: i64) -> i64 {
        x * self.scale - self.half
    }

    fn new_profile(&mut self, state: u8) {
        let c = match self.cur {
            Some(c) if self.profs[c].xs.is_empty() => c,
            _ => {
                self.profs.push(Profile {
                    flags: 0,
                    start: 0,
                    xs: Vec::new(),
                    next: None,
                });
                self.profs.len() - 1
            }
        };
        self.cur = Some(c);
        let mut flags = self.drop_out;
        let last_y = self.last_y;
        let start_line;
        if state == 2 {
            if last_y & (self.precision - 1) >= self.half {
                flags |= OVERSHOOT_TOP;
            }
            start_line = self.floor(last_y);
        } else {
            flags |= FLOW_UP;
            if self.ceiling(last_y) - last_y >= self.half {
                flags |= OVERSHOOT_BOTTOM;
            }
            start_line = self.ceiling(last_y);
        }
        let start_line = start_line.min(self.max_y).max(self.min_y);
        let p = &mut self.profs[c];
        p.flags = flags;
        p.start = start_line >> self.bits;
        p.xs.clear();
        p.next = None;
        if start_line == last_y {
            p.xs.push(self.last_x);
        }
        self.state = state;
    }

    /// `End_Profile`.
    fn end_profile(&mut self) {
        let Some(c) = self.cur else {
            return;
        };
        let h = self.profs[c].xs.len() as i64;
        if h == 0 {
            return;
        }
        let last_y = self.last_y;
        let (precision, half) = (self.precision, self.half);
        let ceil_last = self.ceiling(last_y);
        let p = &mut self.profs[c];
        if p.flags & FLOW_UP != 0 {
            if last_y & (precision - 1) >= half {
                p.flags |= OVERSHOOT_TOP;
            }
        } else {
            if ceil_last - last_y >= half {
                p.flags |= OVERSHOOT_BOTTOM;
            }
            p.start = p.start + 1 - h;
        }
        if self.g_profile.is_none() {
            self.g_profile = Some(c);
        }
        self.profs[c].next = self.g_profile;
        self.cur = None;
    }

    fn line_up(&mut self, x1: i64, y1: i64, x2: i64, y2: i64, miny: i64, maxy: i64) {
        if y2 < miny || y1 > maxy {
            return;
        }
        let e2 = if y2 > maxy { maxy } else { self.floor(y2) };
        let mut e = if y1 < miny {
            miny
        } else {
            self.floor(y1) + self.precision
        };
        if e > e2 {
            return;
        }
        let dx = x2 - x1;
        let dy = y2 - y1;
        let c = self.cur.expect("current profile");
        while e <= e2 {
            let x = if dx == 0 {
                x1
            } else {
                x1 + ((e - y1) as i128 * dx as i128 / dy as i128) as i64
            };
            self.profs[c].xs.push(x);
            e += self.precision;
        }
    }

    fn line_to(&mut self, x: i64, y: i64) {
        if y != self.last_y {
            let new_state = if self.last_y < y { 1 } else { 2 };
            if self.state != new_state {
                if self.state != 0 {
                    self.end_profile();
                }
                self.new_profile(new_state);
            }
            if new_state == 1 {
                self.line_up(self.last_x, self.last_y, x, y, self.min_y, self.max_y);
            } else {
                self.line_up(self.last_x, -self.last_y, x, -y, -self.max_y, -self.min_y);
            }
        }
        self.last_x = x;
        self.last_y = y;
    }

    fn split_conic(a: &mut [(i64, i64)], b: usize) {
        a[b + 4] = a[b + 2];
        let s0 = (a[b + 2].0 + a[b + 1].0, a[b + 2].1 + a[b + 1].1);
        let s1 = (a[b].0 + a[b + 1].0, a[b].1 + a[b + 1].1);
        a[b + 3] = (s0.0 >> 1, s0.1 >> 1);
        a[b + 2] = ((s0.0 + s1.0) >> 2, (s0.1 + s1.1) >> 2);
        a[b + 1] = (s1.0 >> 1, s1.1 >> 1);
    }

    /// `Bezier_Up` for a conic arc whose top is `arcs[a..a+3]`.
    fn bezier_up(&mut self, arcs: &mut [(i64, i64)], a: usize, miny: i64, maxy: i64) {
        let y1 = arcs[a + 2].1;
        let y2 = arcs[a].1;
        if y2 < miny || y1 > maxy {
            return;
        }
        let e2 = if y2 > maxy { maxy } else { self.floor(y2) };
        let mut e = if y1 < miny {
            miny
        } else {
            self.floor(y1) + self.precision
        };
        if e > e2 {
            return;
        }
        let c = self.cur.expect("current profile");
        let mut arc = a;
        loop {
            let y2 = arcs[arc].1;
            if e < y2 {
                let dy = y2 - arcs[arc + 2].1;
                let dx = arcs[arc].0 - arcs[arc + 2].0;
                if dy.max(dx.abs()) > self.step {
                    Self::split_conic(arcs, arc);
                    arc += 2;
                } else {
                    let x = arcs[arc].0 - ((y2 - e) as i128 * dx as i128 / dy as i128) as i64;
                    self.profs[c].xs.push(x);
                    e += self.precision;
                    arc = arc.wrapping_sub(2);
                }
            } else {
                if e == y2 {
                    let x = arcs[arc].0;
                    self.profs[c].xs.push(x);
                    e += self.precision;
                }
                arc = arc.wrapping_sub(2);
            }
            if e > e2 {
                break;
            }
        }
    }

    fn conic_to(&mut self, cx: i64, cy: i64, x: i64, y: i64) {
        let mut arcs: Vec<(i64, i64)> = vec![(0, 0); 130];
        arcs[2] = (self.last_x, self.last_y);
        arcs[1] = (cx, cy);
        arcs[0] = (x, y);
        let mut arc: usize = 0;
        loop {
            let a = arc;
            let (sy, cy_, ey, ex) = (arcs[a + 2].1, arcs[a + 1].1, arcs[a].1, arcs[a].0);
            let (lo, hi) = if sy > ey { (ey, sy) } else { (sy, ey) };
            if cy_ < self.floor(lo) || cy_ > self.ceiling(hi) {
                Self::split_conic(&mut arcs, a);
                arc += 2;
                continue;
            }
            if sy != ey {
                let new_state = if sy < ey { 1 } else { 2 };
                if self.state != new_state {
                    if self.state != 0 {
                        self.end_profile();
                    }
                    self.new_profile(new_state);
                }
                let (miny, maxy) = (self.min_y, self.max_y);
                if new_state == 1 {
                    self.bezier_up(&mut arcs, a, miny, maxy);
                } else {
                    arcs[a].1 = -arcs[a].1;
                    arcs[a + 1].1 = -arcs[a + 1].1;
                    arcs[a + 2].1 = -arcs[a + 2].1;
                    self.bezier_up(&mut arcs, a, -maxy, -miny);
                    arcs[a].1 = -arcs[a].1;
                }
            }
            self.last_x = ex;
            self.last_y = ey;
            if arc == 0 {
                break;
            }
            arc -= 2;
        }
    }
}

/// `FT_Render_Glyph(.., FT_RENDER_MODE_MONO)` of a TrueType outline. `high` is
/// `FT_OUTLINE_HIGH_PRECISION` (ppem < 24); `marks` are the `(point, scan_type)`
/// pairs the glyph programs left on the outline (`FT_CURVE_TAG_HAS_SCANMODE`).
/// One byte per pixel (0 or 255), row 0 on top.
pub fn render(
    pts: &[V],
    tags: &[u8],
    contours: &[u16],
    high: bool,
    marks: &[(usize, u8)],
) -> Bitmap {
    if pts.is_empty() {
        return Bitmap::default();
    }
    let (mut xmin, mut ymin, mut xmax, mut ymax) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
    for p in pts {
        xmin = xmin.min(p.x);
        ymin = ymin.min(p.y);
        xmax = xmax.max(p.x);
        ymax = ymax.max(p.y);
    }
    // `ft_glyphslot_preset_bitmap`, monochrome: the box of pixel centres inside the
    // control box; an empty extent grows toward the larger fractional part.
    let mut l = (xmin + 31) >> 6;
    let mut r = (xmax + 32) >> 6;
    let mut b = (ymin + 31) >> 6;
    let mut t = (ymax + 32) >> 6;
    if l == r {
        let s = (((xmin & 63) + 31) & 63) + (((xmax & 63) + 32) & 63) - 31;
        if s > 31 {
            r = l + 1;
        } else {
            l -= 1;
        }
    }
    if b == t {
        let s = (((ymin & 63) + 31) & 63) + (((ymax & 63) + 32) & 63) - 31;
        if s > 31 {
            t = b + 1;
        } else {
            b -= 1;
        }
    }
    let width = (r - l) as usize;
    let rows = (t - b) as usize;
    let mut bm = Bitmap {
        left: l as i32,
        top: t as i32,
        width,
        rows,
        data: vec![0; width * rows],
    };
    if width == 0 || rows == 0 {
        return bm;
    }
    let high = high && (width as i64 - 1) + (rows as i64 - 1) <= 255;
    let (cx0, cy0) = (l << 6, b << 6);
    let tp: Vec<V> = pts
        .iter()
        .map(|p| V {
            x: p.x - cx0,
            y: p.y - cy0,
        })
        .collect();
    let mut tg: Vec<u8> = tags.iter().map(|t| t & 1).collect();
    for &(i, st) in marks {
        if i < tg.len() {
            tg[i] |= (st << 5) | 4;
        }
    }
    let mut drop_out = 1u16;
    for flipped in [false, true] {
        let nlines = if flipped { width } else { rows } as i64;
        let ras = build(&tp, &tg, contours, high, flipped, nlines, drop_out);
        drop_out = ras.drop_out;
        sweep(&ras, &mut bm, flipped);
    }
    bm
}

fn build(
    tp: &[V],
    tags: &[u8],
    contours: &[u16],
    high: bool,
    flipped: bool,
    nlines: i64,
    drop_out: u16,
) -> Ras {
    let (bits, step, scale) = if high { (12, 256, 64) } else { (6, 32, 1) };
    let precision = 1i64 << bits;
    let mut ras = Ras {
        bits,
        precision,
        half: precision / 2,
        mask: -precision,
        step,
        scale,
        min_y: 0,
        max_y: (nlines - 1) * precision,
        state: 0,
        last_x: 0,
        last_y: 0,
        // Default with no IGNORE/SMART flag and no INCLUDE_STUBS: simple drop-outs, stubs excluded.
        drop_out,
        profs: Vec::new(),
        cur: None,
        g_profile: None,
    };
    let mut first = 0usize;
    for &end in contours {
        let last = end as usize;
        ras.state = 0;
        ras.g_profile = None;
        if tags[first] & 4 != 0 {
            ras.drop_out = (tags[first] >> 5) as u16;
        }
        decompose(&mut ras, &tp[first..=last], &tags[first..=last], flipped);
        // Do the extreme arcs of the contour join?
        let on_line = ras.last_y & (ras.precision - 1) == 0
            && ras.last_y >= ras.min_y
            && ras.last_y <= ras.max_y;
        if on_line {
            if let (Some(g), Some(c)) = (ras.g_profile, ras.cur) {
                if (ras.profs[g].flags ^ ras.profs[c].flags) & FLOW_UP == 0 {
                    ras.profs[c].xs.pop();
                }
            }
        }
        ras.end_profile();
        first = last + 1;
    }
    // Chain the profiles of each contour: `next` of one points at the following one.
    let n = ras.profs.len();
    for i in 0..n.saturating_sub(1) {
        if !ras.profs[i + 1].xs.is_empty() && ras.profs[i + 1].next == ras.profs[i].next {
            ras.profs[i].next = Some(i + 1);
        }
    }
    ras
}

fn decompose(ras: &mut Ras, pts: &[V], tags: &[u8], flipped: bool) {
    let n = pts.len();
    if n == 0 {
        return;
    }
    let on = |i: usize| tags[i] & 1 != 0;
    let sc = |ras: &Ras, p: V| {
        let (x, y) = (ras.scaled(p.x), ras.scaled(p.y));
        if flipped {
            (y, x)
        } else {
            (x, y)
        }
    };
    let last = n - 1;
    let mut v_start = sc(ras, pts[0]);
    let v_last = sc(ras, pts[last]);
    let mut limit = last;
    let mut idx: isize = 0;
    if !on(0) {
        if on(last) {
            v_start = v_last;
            limit -= 1;
        } else {
            v_start = ((v_start.0 + v_last.0) / 2, (v_start.1 + v_last.1) / 2);
        }
        idx = -1;
    }
    ras.last_x = v_start.0;
    ras.last_y = v_start.1;
    while (idx as i64) < limit as i64 {
        idx += 1;
        let i = idx as usize;
        if on(i) {
            let (x, y) = sc(ras, pts[i]);
            ras.line_to(x, y);
            continue;
        }
        let mut v_control = sc(ras, pts[i]);
        loop {
            if (idx as i64) < limit as i64 {
                idx += 1;
                let j = idx as usize;
                let (x, y) = sc(ras, pts[j]);
                if on(j) {
                    ras.conic_to(v_control.0, v_control.1, x, y);
                    break;
                }
                let v_middle = ((v_control.0 + x) / 2, (v_control.1 + y) / 2);
                ras.conic_to(v_control.0, v_control.1, v_middle.0, v_middle.1);
                v_control = (x, y);
                continue;
            }
            ras.conic_to(v_control.0, v_control.1, v_start.0, v_start.1);
            return;
        }
    }
    ras.line_to(v_start.0, v_start.1);
}

struct Target<'a> {
    width: usize,
    rows: usize,
    nlines: i64,
    npos: i64,
    flipped: bool,
    data: &'a mut Vec<u8>,
}

impl Target<'_> {
    fn index(&self, line: i64, pos: i64) -> Option<usize> {
        if line < 0 || line >= self.nlines || pos < 0 || pos >= self.npos {
            return None;
        }
        let (col, row_from_bottom) = if self.flipped {
            (line, pos)
        } else {
            (pos, line)
        };
        let row = self.rows as i64 - 1 - row_from_bottom;
        Some(row as usize * self.width + col as usize)
    }

    fn put(&mut self, line: i64, pos: i64) {
        if let Some(i) = self.index(line, pos) {
            self.data[i] = 255;
        }
    }

    fn is_set(&self, line: i64, pos: i64) -> bool {
        self.index(line, pos)
            .map(|i| self.data[i] != 0)
            .unwrap_or(false)
    }
}

/// A profile while it is being swept.
struct Live {
    flags: u16,
    start: i64,
    xs: Vec<i64>,
    next: Option<usize>,
    up: bool,
    /// Index of the sample for the current scanline, and samples left including it.
    offset: i64,
    height: i64,
    x: i64,
}

fn sweep(ras: &Ras, bm: &mut Bitmap, flipped: bool) {
    let (nlines, npos) = if flipped {
        (bm.width as i64, bm.rows as i64)
    } else {
        (bm.rows as i64, bm.width as i64)
    };
    let mut t = Target {
        width: bm.width,
        rows: bm.rows,
        nlines,
        npos,
        flipped,
        data: &mut bm.data,
    };
    let mut live: Vec<Live> = Vec::new();
    // Map from `ras.profs` index to `live` index (empty profiles are skipped).
    let mut map: Vec<Option<usize>> = vec![None; ras.profs.len()];
    for (i, p) in ras.profs.iter().enumerate() {
        if p.xs.is_empty() {
            continue;
        }
        let up = p.flags & FLOW_UP != 0;
        let h = p.xs.len() as i64;
        let mut xs = p.xs.clone();
        let start = p.start;
        let offset;
        if up {
            offset = 0;
        } else {
            offset = h - 1;
            // `start` is the bottom line; samples are read bottom to top.
        }
        let x = xs[offset as usize];
        let _ = &mut xs;
        map[i] = Some(live.len());
        live.push(Live {
            flags: p.flags,
            start,
            xs,
            next: p.next,
            up,
            offset,
            height: h,
            x,
        });
    }
    for l in live.iter_mut() {
        l.next = l.next.and_then(|n| map.get(n).copied().flatten());
    }
    if live.is_empty() {
        return;
    }
    let y_lo = live.iter().map(|l| l.start).min().unwrap();
    let y_hi = live.iter().map(|l| l.start + l.height - 1).max().unwrap();
    let prec = ras.precision;
    let mut waiting: Vec<usize> = (0..live.len()).collect();
    let mut left: Vec<usize> = Vec::new();
    let mut right: Vec<usize> = Vec::new();
    for y in y_lo..=y_hi {
        // Activate the profiles starting on this scanline.
        let mut still: Vec<usize> = Vec::new();
        for &pi in &waiting {
            if live[pi].start == y {
                let list = if live[pi].up { &mut left } else { &mut right };
                let nx = live[pi].x;
                let pos = list
                    .iter()
                    .position(|&q| live[q].x >= nx)
                    .unwrap_or(list.len());
                list.insert(pos, pi);
            } else {
                still.push(pi);
            }
        }
        waiting = still;
        // Spans and drop-out marks.
        let mut marked = 0;
        for k in 0..left.len().min(right.len()) {
            let (li, ri) = (left[k], right[k]);
            let (mut x1, mut x2) = (live[li].x, live[ri].x);
            if x1 > x2 {
                std::mem::swap(&mut x1, &mut x2);
            }
            let e1 = ras.ceiling(x1);
            let e2 = ras.floor(x2);
            if e1 <= e2 {
                if !flipped {
                    let a = ras.trunc(e1);
                    let b = ras.trunc(e2);
                    if b >= 0 && a <= npos - 1 {
                        for px in a.max(0)..=b.min(npos - 1) {
                            t.put(y, px);
                        }
                    }
                } else {
                    if x1 == e1 {
                        let px = ras.trunc(x1);
                        t.put(y, px);
                    }
                    if x2 == e2 {
                        let px = ras.trunc(x2);
                        t.put(y, px);
                    }
                }
                continue;
            }
            // A drop-out: no pixel centre between the edges.
            let flags = live[li].flags;
            if flags & 2 != 0 {
                continue;
            }
            if flags & 1 != 0 {
                let l = &live[li];
                let wide = x2 - x1 >= ras.half;
                if l.height == 1 {
                    if l.next == Some(ri) {
                        if flags & OVERSHOOT_TOP == 0 {
                            continue;
                        }
                        if !wide {
                            continue;
                        }
                    }
                }
                if l.offset == 0 && live[ri].next == Some(li) {
                    if flags & OVERSHOOT_BOTTOM == 0 {
                        continue;
                    }
                    if !wide {
                        continue;
                    }
                }
            }
            let (pa, pb);
            if flags & 4 != 0 {
                let adj = (63 * prec) / 64;
                let pxl = ((x1 + x2 + adj) >> 1) & ras.mask;
                pa = pxl;
                pb = if x1 > pxl { pxl + prec } else { pxl - prec };
            } else {
                pa = e2;
                pb = e1;
            }
            live[li].x = pa;
            live[ri].x = pb;
            live[li].flags |= DROPOUT_PENDING;
            marked += 1;
        }
        if marked > 0 {
            for k in 0..left.len().min(right.len()) {
                let (li, ri) = (left[k], right[k]);
                if live[li].flags & DROPOUT_PENDING == 0 {
                    continue;
                }
                let pa = ras.trunc(live[li].x);
                let pb = ras.trunc(live[ri].x);
                let a_ok = pa >= 0 && pa <= npos - 1;
                let b_ok = pb >= 0 && pb <= npos - 1;
                if a_ok {
                    if !b_ok || !t.is_set(y, pb) {
                        t.put(y, pa);
                    }
                } else if b_ok {
                    t.put(y, pb);
                }
                live[li].flags &= !DROPOUT_PENDING;
            }
        }
        // Advance every active profile to the next scanline and re-sort.
        for list in [&mut left, &mut right] {
            let mut keep: Vec<usize> = Vec::new();
            for &pi in list.iter() {
                let l = &mut live[pi];
                l.height -= 1;
                if l.height == 0 {
                    continue;
                }
                if l.up {
                    l.offset += 1;
                } else {
                    l.offset -= 1;
                }
                l.x = l.xs[l.offset as usize];
                keep.push(pi);
            }
            // Stable insertion sort by x.
            for i in 1..keep.len() {
                let mut j = i;
                while j > 0 && live[keep[j - 1]].x > live[keep[j]].x {
                    keep.swap(j - 1, j);
                    j -= 1;
                }
            }
            *list = keep;
        }
    }
}
