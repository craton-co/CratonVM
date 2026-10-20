// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `FT_Outline_Embolden` (`ftoutln.c`, FreeType 2.14): the synthetic bold the JDK
//! applies to a glyph whose font has no bold face.

use super::fixed::{mul_div, mul_fix, norm_len_l};
use super::interp::V;

#[derive(PartialEq, Clone, Copy)]
enum Orientation {
    TrueType,
    PostScript,
    None,
}

fn msb(v: u32) -> i32 {
    31 - v.leading_zeros() as i32
}

/// `FT_Outline_Get_Orientation`.
fn orientation(pts: &[V], contours: &[u16]) -> Orientation {
    if pts.is_empty() {
        return Orientation::TrueType;
    }
    let (mut xmin, mut ymin, mut xmax, mut ymax) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
    for p in pts {
        xmin = xmin.min(p.x);
        ymin = ymin.min(p.y);
        xmax = xmax.max(p.x);
        ymax = ymax.max(p.y);
    }
    if xmax == xmin || ymax == ymin {
        return Orientation::None;
    }
    let xshift = (msb((xmax.abs() | xmin.abs()) as u32) - 14).max(0);
    let yshift = (msb((ymax - ymin) as u32) - 14).max(0);
    let mut area: i64 = 0;
    let mut first = 0usize;
    for &end in contours {
        let last = end as usize;
        let mut prev = (pts[last].x >> xshift, pts[last].y >> yshift);
        for p in &pts[first..=last] {
            let cur = (p.x >> xshift, p.y >> yshift);
            area += (cur.0 - prev.0) * (cur.1 + prev.1);
            prev = cur;
        }
        first = last + 1;
    }
    // Clockwise outlines (positive `area` for y up) are the TrueType orientation.
    if area < 0 {
        Orientation::PostScript
    } else if area > 0 {
        Orientation::TrueType
    } else {
        Orientation::None
    }
}

/// `FT_Outline_Embolden(outline, strength)`.
pub fn embolden(pts: &mut [V], contours: &[u16], strength: i64) {
    let xstrength = strength / 2;
    let ystrength = strength / 2;
    if xstrength == 0 && ystrength == 0 {
        return;
    }
    let orient = orientation(pts, contours);
    if orient == Orientation::None {
        return;
    }
    let mut first = 0usize;
    for &end in contours {
        let last = end as usize;
        let mut l_in: i64 = 0;
        let (mut inv, mut anchor) = ((0i64, 0i64), (0i64, 0i64));
        let mut l_anchor: i64 = 0;
        let mut i = last;
        let mut j = first;
        let mut k: i64 = -1;
        while j != i && i as i64 != k {
            let out;
            let l_out;
            if j as i64 != k {
                let (dx, dy) = (pts[j].x - pts[i].x, pts[j].y - pts[i].y);
                let (nx, ny, l) = norm_len_l(dx, dy);
                out = (nx, ny);
                l_out = l as i64;
                if l_out == 0 {
                    j = if j < last { j + 1 } else { first };
                    continue;
                }
            } else {
                out = anchor;
                l_out = l_anchor;
            }
            if l_in != 0 {
                if k < 0 {
                    k = i as i64;
                    anchor = inv;
                    l_anchor = l_in;
                }
                let mut d = mul_fix(inv.0, out.0) + mul_fix(inv.1, out.1);
                let mut shift = (0i64, 0i64);
                // shift only if the turn is less than ~160 degrees
                if d > -0xF000 {
                    d += 0x10000;
                    shift.0 = inv.1 + out.1;
                    shift.1 = inv.0 + out.0;
                    if orient == Orientation::TrueType {
                        shift.0 = -shift.0;
                    } else {
                        shift.1 = -shift.1;
                    }
                    let mut q = mul_fix(out.0, inv.1) - mul_fix(out.1, inv.0);
                    if orient == Orientation::TrueType {
                        q = -q;
                    }
                    let l = l_in.min(l_out);
                    shift.0 = if mul_fix(xstrength, q) <= mul_fix(l, d) {
                        mul_div(shift.0, xstrength, d)
                    } else {
                        mul_div(shift.0, l, q)
                    };
                    shift.1 = if mul_fix(ystrength, q) <= mul_fix(l, d) {
                        mul_div(shift.1, ystrength, d)
                    } else {
                        mul_div(shift.1, l, q)
                    };
                }
                while i != j {
                    pts[i].x += xstrength + shift.0;
                    pts[i].y += ystrength + shift.1;
                    i = if i < last { i + 1 } else { first };
                }
            } else {
                i = j;
            }
            inv = out;
            l_in = l_out;
            j = if j < last { j + 1 } else { first };
        }
        first = last + 1;
    }
}
