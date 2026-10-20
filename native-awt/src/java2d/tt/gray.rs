// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! FreeType's smooth (anti-aliased) rasteriser, `ftgrays.c`: coverage by exact
//! area with 8-bit sub-pixel coordinates, accumulated per cell.

use std::collections::BTreeMap;

use super::interp::V;

const PIXEL_BITS: i64 = 8;
const ONE_PIXEL: i64 = 1 << PIXEL_BITS;

#[derive(Clone, Debug, Default)]
pub struct Bitmap {
    pub left: i32,
    pub top: i32,
    pub width: usize,
    pub rows: usize,
    /// Row 0 is the top row; one byte of coverage per pixel.
    pub data: Vec<u8>,
}

struct Ras {
    x: i64,
    y: i64,
    cover: i64,
    area: i64,
    ex: i64,
    ey: i64,
    invalid: bool,
    max_ex: i64,
    max_ey: i64,
    cells: BTreeMap<(i64, i64), (i64, i64)>,
}

fn trunc(x: i64) -> i64 {
    x >> PIXEL_BITS
}
fn upscale(x: i64) -> i64 {
    x * (ONE_PIXEL >> 6)
}

impl Ras {
    fn record_cell(&mut self) {
        if !self.invalid && (self.area != 0 || self.cover != 0) {
            let e = self.cells.entry((self.ey, self.ex)).or_insert((0, 0));
            e.0 += self.cover;
            e.1 += self.area;
        }
    }

    fn set_cell(&mut self, ex: i64, ey: i64) {
        // Cells to the left of the box are parked at x = -1.
        let ex = if ex < 0 { -1 } else { ex };
        if ex != self.ex || ey != self.ey {
            self.record_cell();
            self.area = 0;
            self.cover = 0;
            self.ex = ex;
            self.ey = ey;
        }
        self.invalid = ey < 0 || ey >= self.max_ey || ex >= self.max_ex;
    }

    fn start_cell(&mut self, ex: i64, ey: i64) {
        let ex = if ex < -1 { -1 } else { ex };
        self.area = 0;
        self.cover = 0;
        self.ex = ex;
        self.ey = ey;
        self.invalid = ey < 0 || ey >= self.max_ey || ex >= self.max_ex;
    }

    fn move_to(&mut self, tx: i64, ty: i64) {
        self.record_cell();
        let (x, y) = (upscale(tx), upscale(ty));
        self.start_cell(trunc(x), trunc(y));
        self.x = x;
        self.y = y;
    }

    fn render_line(&mut self, to_x: i64, to_y: i64) {
        let mut ey1 = trunc(self.y);
        let ey2 = trunc(to_y);
        if (ey1 >= self.max_ey && ey2 >= self.max_ey) || (ey1 < 0 && ey2 < 0) {
            self.x = to_x;
            self.y = to_y;
            return;
        }
        let mut ex1 = trunc(self.x);
        let ex2 = trunc(to_x);
        let mut fx1 = self.x - (ex1 << PIXEL_BITS);
        let mut fy1 = self.y - (ey1 << PIXEL_BITS);
        let dx = to_x - self.x;
        let dy = to_y - self.y;
        let (mut fx2, mut fy2);
        if ex1 == ex2 && ey1 == ey2 {
            // inside one cell
        } else if dy == 0 {
            ex1 = ex2;
            self.set_cell(ex1, ey1);
        } else if dx == 0 {
            if dy > 0 {
                loop {
                    fy2 = ONE_PIXEL;
                    self.cover += fy2 - fy1;
                    self.area += (fy2 - fy1) * fx1 * 2;
                    fy1 = 0;
                    ey1 += 1;
                    self.set_cell(ex1, ey1);
                    if ey1 == ey2 {
                        break;
                    }
                }
            } else {
                loop {
                    fy2 = 0;
                    self.cover += fy2 - fy1;
                    self.area += (fy2 - fy1) * fx1 * 2;
                    fy1 = ONE_PIXEL;
                    ey1 -= 1;
                    self.set_cell(ex1, ey1);
                    if ey1 == ey2 {
                        break;
                    }
                }
            }
        } else {
            let mut prod = dx * fy1 - dy * fx1;
            loop {
                if prod <= 0 && prod - dx * ONE_PIXEL > 0 {
                    // left
                    fx2 = 0;
                    fy2 = udiv(-prod, -dx);
                    prod -= dy * ONE_PIXEL;
                    self.cover += fy2 - fy1;
                    self.area += (fy2 - fy1) * (fx1 + fx2);
                    fx1 = ONE_PIXEL;
                    fy1 = fy2;
                    ex1 -= 1;
                } else if prod - dx * ONE_PIXEL <= 0 && prod - dx * ONE_PIXEL + dy * ONE_PIXEL > 0 {
                    // up
                    prod -= dx * ONE_PIXEL;
                    fx2 = udiv(-prod, dy);
                    fy2 = ONE_PIXEL;
                    self.cover += fy2 - fy1;
                    self.area += (fy2 - fy1) * (fx1 + fx2);
                    fx1 = fx2;
                    fy1 = 0;
                    ey1 += 1;
                } else if prod - dx * ONE_PIXEL + dy * ONE_PIXEL <= 0 && prod + dy * ONE_PIXEL >= 0
                {
                    // right
                    prod += dy * ONE_PIXEL;
                    fx2 = ONE_PIXEL;
                    fy2 = udiv(prod, dx);
                    self.cover += fy2 - fy1;
                    self.area += (fy2 - fy1) * (fx1 + fx2);
                    fx1 = 0;
                    fy1 = fy2;
                    ex1 += 1;
                } else {
                    // down
                    fx2 = udiv(prod, -dy);
                    fy2 = 0;
                    prod += dx * ONE_PIXEL;
                    self.cover += fy2 - fy1;
                    self.area += (fy2 - fy1) * (fx1 + fx2);
                    fx1 = fx2;
                    fy1 = ONE_PIXEL;
                    ey1 -= 1;
                }
                self.set_cell(ex1, ey1);
                if ex1 == ex2 && ey1 == ey2 {
                    break;
                }
            }
        }
        fx2 = to_x - (ex2 << PIXEL_BITS);
        fy2 = to_y - (ey2 << PIXEL_BITS);
        self.cover += fy2 - fy1;
        self.area += (fy2 - fy1) * (fx1 + fx2);
        self.x = to_x;
        self.y = to_y;
    }

    /// 2.14 renders a conic by forward differencing in 32.32 fixed point:
    /// `2^k` segments, `k` the number of 4-fold flatness reductions needed.
    fn render_conic(&mut self, cx: i64, cy: i64, tx: i64, ty: i64) {
        let (c_x, c_y) = (upscale(cx), upscale(cy));
        let (t_x, t_y) = (upscale(tx), upscale(ty));
        let (sx, sy) = (self.x, self.y);
        if (trunc(t_y) >= self.max_ey && trunc(c_y) >= self.max_ey && trunc(sy) >= self.max_ey)
            || (trunc(t_y) < 0 && trunc(c_y) < 0 && trunc(sy) < 0)
        {
            self.x = t_x;
            self.y = t_y;
            return;
        }
        let dx1 = c_x - sx;
        let dy1 = c_y - sy;
        let ddx = (t_x - c_x) - dx1;
        let ddy = (t_y - c_y) - dy1;
        let mut m = ddx.abs().max(ddy.abs());
        if m <= ONE_PIXEL / 4 {
            self.render_line(t_x, t_y);
            return;
        }
        let mut k = 0u32;
        loop {
            m >>= 2;
            k += 1;
            if m <= ONE_PIXEL / 4 {
                break;
            }
        }
        let n = 1u32 << k;
        let sh_dd = 32 - 2 * k as i32;
        let sh_d = 33 - k as i32;
        let mut px = sx << 32;
        let mut py = sy << 32;
        let mut d1x = (dx1 << sh_d) + (ddx << sh_dd);
        let mut d1y = (dy1 << sh_d) + (ddy << sh_dd);
        let d2x = (ddx << sh_dd) * 2;
        let d2y = (ddy << sh_dd) * 2;
        for _ in 0..n {
            px += d1x;
            py += d1y;
            d1x += d2x;
            d1y += d2y;
            self.render_line(px >> 32, py >> 32);
        }
    }
}

/// `FT_UDIV`: division by a 32-bit reciprocal, as the 2.14 line renderer does
/// (`a * (0xFFFFFFFF / b) >> 32`), so quotients can come out one short of exact.
fn udiv(a: i64, b: i64) -> i64 {
    let r = 0xFFFF_FFFFu64 / (b as u64);
    (((a as u64).wrapping_mul(r)) >> 32) as i64
}

impl Bitmap {
    /// The bytes FreeType would hold: coverage bytes, or for `mono` one bit per
    /// pixel (MSB first, rows padded to bytes) from a 0/255 byte-per-pixel map.
    pub fn packed(&self, mono: bool) -> Vec<u8> {
        if !mono {
            return self.data.clone();
        }
        let bpr = (self.width + 7) / 8;
        let mut out = vec![0u8; bpr * self.rows];
        for r in 0..self.rows {
            for c in 0..self.width {
                if self.data[r * self.width + c] != 0 {
                    out[r * bpr + c / 8] |= 0x80 >> (c % 8);
                }
            }
        }
        out
    }
}

/// `FT_Render_Glyph(.., FT_RENDER_MODE_NORMAL)` for an outline of on/off-curve
/// (quadratic) points in 26.6, non-zero winding.
pub fn render(pts: &[V], tags: &[u8], contours: &[u16]) -> Bitmap {
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
    let cx0 = xmin & !63;
    let cy0 = ymin & !63;
    let cx1 = (xmax + 63) & !63;
    let cy1 = (ymax + 63) & !63;
    let width = ((cx1 - cx0) >> 6) as usize;
    let rows = ((cy1 - cy0) >> 6) as usize;
    let mut bm = Bitmap {
        left: (cx0 >> 6) as i32,
        top: (cy1 >> 6) as i32,
        width,
        rows,
        data: vec![0; width * rows],
    };
    if width == 0 || rows == 0 {
        return bm;
    }
    let mut ras = Ras {
        x: 0,
        y: 0,
        cover: 0,
        area: 0,
        ex: i64::MAX,
        ey: i64::MAX,
        invalid: true,
        max_ex: width as i64,
        max_ey: rows as i64,
        cells: BTreeMap::new(),
    };
    let tp: Vec<V> = pts
        .iter()
        .map(|p| V {
            x: p.x - cx0,
            y: p.y - cy0,
        })
        .collect();
    let mut first = 0usize;
    for &end in contours {
        let last = end as usize;
        decompose_contour(&mut ras, &tp[first..=last], &tags[first..=last]);
        first = last + 1;
    }
    ras.record_cell();
    // Sweep.
    let mut by_row: BTreeMap<i64, Vec<(i64, i64, i64)>> = BTreeMap::new();
    for (&(y, x), &(cover, area)) in &ras.cells {
        by_row.entry(y).or_default().push((x, cover, area));
    }
    for (y, cells) in by_row {
        let mut cover = 0i64;
        let mut x = 0i64;
        let row = (rows as i64 - 1 - y) as usize;
        let mut hline = |data: &mut Vec<u8>, x0: i64, area: i64, count: i64| {
            let mut coverage = area >> (PIXEL_BITS * 2 + 1 - 8);
            if coverage < 0 {
                coverage = !coverage;
            }
            if coverage >= 256 {
                coverage = 255;
            }
            if coverage != 0 && count > 0 {
                for i in 0..count {
                    let px = x0 + i;
                    if px >= 0 && (px as usize) < width {
                        data[row * width + px as usize] = coverage as u8;
                    }
                }
            }
        };
        for &(cx, ccover, carea) in &cells {
            if cx > x && cover != 0 {
                hline(&mut bm.data, x, cover * (ONE_PIXEL * 2), cx - x);
            }
            cover += ccover;
            let area = cover * (ONE_PIXEL * 2) - carea;
            if area != 0 && cx >= 0 {
                hline(&mut bm.data, cx, area, 1);
            }
            x = cx + 1;
        }
        if cover != 0 {
            hline(&mut bm.data, x, cover * (ONE_PIXEL * 2), width as i64 - x);
        }
    }
    bm
}

fn decompose_contour(ras: &mut Ras, pts: &[V], tags: &[u8]) {
    let n = pts.len();
    if n == 0 {
        return;
    }
    let on = |i: usize| tags[i] & 1 != 0;
    let last = n - 1;
    let mut v_start = pts[0];
    let mut limit = last;
    let mut idx: isize = 0;
    if !on(0) {
        if on(last) {
            v_start = pts[last];
            limit -= 1;
        } else {
            v_start = V {
                x: (pts[0].x + pts[last].x) / 2,
                y: (pts[0].y + pts[last].y) / 2,
            };
        }
        idx = -1;
    }
    ras.move_to(v_start.x, v_start.y);
    while (idx as i64) < limit as i64 {
        idx += 1;
        let i = idx as usize;
        if on(i) {
            ras.render_line(upscale(pts[i].x), upscale(pts[i].y));
            continue;
        }
        let mut v_control = pts[i];
        loop {
            if (idx as i64) < limit as i64 {
                idx += 1;
                let j = idx as usize;
                let vec = pts[j];
                if on(j) {
                    ras.render_conic(v_control.x, v_control.y, vec.x, vec.y);
                    break;
                }
                let v_middle = V {
                    x: (v_control.x + vec.x) / 2,
                    y: (v_control.y + vec.y) / 2,
                };
                ras.render_conic(v_control.x, v_control.y, v_middle.x, v_middle.y);
                v_control = vec;
                continue;
            }
            ras.render_conic(v_control.x, v_control.y, v_start.x, v_start.y);
            // closed by the conic ending at the start
            return;
        }
    }
    ras.render_line(upscale(v_start.x), upscale(v_start.y));
}

/// Harmony sub-pixel geometry (`lcd_geometry`): the three sub-pixel centres, 26.6.
const LCD_SUB: [i64; 3] = [-21, 0, 21];

/// `FT_Render_Glyph` with `FT_RENDER_MODE_LCD` (`vertical` = false) or
/// `FT_RENDER_MODE_LCD_V`: with no LCD filter built in, FreeType 2.14 renders the
/// outline three times, shifted by the sub-pixel offsets, into the red, green and
/// blue channels. The bitmap box is the control box padded by the geometry.
///
/// Horizontal: `width` is three bytes per pixel (R, G, B). Vertical: `rows` is
/// three per pixel row (the top sub-pixel first), `width` one byte per pixel.
pub fn render_lcd(pts: &[V], tags: &[u8], contours: &[u16], vertical: bool) -> Bitmap {
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
    let pad = LCD_SUB[2] - LCD_SUB[0];
    let _ = pad;
    if vertical {
        ymin += LCD_SUB[0];
        ymax += LCD_SUB[2];
    } else {
        xmin += LCD_SUB[0];
        xmax += LCD_SUB[2];
    }
    let cx0 = xmin & !63;
    let cy0 = ymin & !63;
    let cx1 = (xmax + 63) & !63;
    let cy1 = (ymax + 63) & !63;
    let w = ((cx1 - cx0) >> 6) as usize;
    let h = ((cy1 - cy0) >> 6) as usize;
    let left = (cx0 >> 6) as i32;
    let top = (cy1 >> 6) as i32;
    let mut out = if vertical {
        Bitmap {
            left,
            top,
            width: w,
            rows: h * 3,
            data: vec![0; w * h * 3],
        }
    } else {
        Bitmap {
            left,
            top,
            width: w * 3,
            rows: h,
            data: vec![0; w * 3 * h],
        }
    };
    for ch in 0..3usize {
        let (dx, dy) = if vertical {
            (0, LCD_SUB[ch])
        } else {
            (-LCD_SUB[ch], 0)
        };
        let shifted: Vec<V> = pts
            .iter()
            .map(|p| V {
                x: p.x + dx,
                y: p.y + dy,
            })
            .collect();
        let b = render(&shifted, tags, contours);
        if b.width == 0 || b.rows == 0 {
            continue;
        }
        let ox = (b.left - left) as usize;
        let oy = (top - b.top) as usize;
        for r in 0..b.rows {
            for c in 0..b.width {
                let v = b.data[r * b.width + c];
                if vertical {
                    out.data[((oy + r) * 3 + ch) * w + ox + c] = v;
                } else {
                    out.data[(oy + r) * w * 3 + (ox + c) * 3 + ch] = v;
                }
            }
        }
    }
    out
}
