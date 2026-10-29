// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A port of the parts of FreeType (2.14, as bundled by the JDK) that decide
//! what Java2D text looks like: the TrueType glyph loader with its bytecode
//! hinting (`ttgload.c`, `ttinterp.c`, `ttobjs.c`) and the rasterisers
//! (`ftgrays.c`, `ftraster.c`). HotSpot loads every glyph with
//! `FT_LOAD_TARGET_MONO` hinting, so that is the mode implemented here.

pub mod embolden;
pub mod fixed;
pub mod font;
pub mod gray;
pub mod interp;
pub mod mono;

#[cfg(test)]
mod tests;

use font::*;
use interp::*;

use fixed::*;

/// A loaded glyph outline in 26.6 pixel coordinates.
#[derive(Clone, Debug, Default)]
pub struct GlyphOutline {
    pub pts: Vec<V>,
    /// Bit 0 set for on-curve points.
    pub tags: Vec<u8>,
    /// End point index of each contour.
    pub contours: Vec<u16>,
    /// Horizontal advance in 26.6, rounded to whole pixels when hinted.
    pub advance: i64,
    /// `(point, scan type)` marks the glyph programs left on the outline
    /// (`FT_CURVE_TAG_HAS_SCANMODE`): the monochrome rasteriser's drop-out mode.
    pub marks: Vec<(usize, u8)>,
}

/// Per-size state: scaled `cvt`, the `fpgm`/`prep` results and the twilight zone.
pub struct TtSize {
    pub x_scale: i64,
    pub y_scale: i64,
    /// The exact `FT_Set_Char_Size` scale: unhinted loads use it, hinted ones the
    /// ppem-rounded `x_scale` (FreeType's `hinted_metrics`).
    exact_scale: i64,
    hinted_scale: i64,
    pub ppem: i64,
    exec: Exec,
    gs: Gs,
    hinting_disabled: bool,
    use_default_gs: bool,
}

struct Loader {
    pts: Vec<V>,
    tags: Vec<u8>,
    contours: Vec<u16>,
    pp: [V; 4],
    hinted: bool,
    marks: Vec<(usize, u8)>,
}

impl TtSize {
    /// `FT_Set_Char_Size(face, 0, size26_6, 72, 72)` plus `tt_size_ready_bytecode`.
    pub fn new(font: &TtFont, size26_6: i64) -> TtSize {
        let upem = font.upem as i64;
        let x_ppem = (size26_6 + 32) >> 6;
        let mut x_scale = div_fix(size26_6, upem);
        let mut y_scale = x_scale;
        let exact_scale = x_scale;
        if font.head_flags & 8 != 0 {
            x_scale = div_fix(x_ppem << 6, upem);
            y_scale = x_scale;
        }
        let stack = font.maxp.max_stack_elements as usize + 32;
        let mut exec = Exec::new(stack);
        exec.x_scale = x_scale;
        exec.y_scale = y_scale;
        exec.tt_scale = x_scale;
        exec.ppem = x_ppem;
        exec.point_size = mul_div(x_ppem, 64 * 72, 72);
        exec.max_fdefs = (font.maxp.max_function_defs as usize).max(64);
        exec.max_idefs = font.maxp.max_instruction_defs as usize;
        exec.storage = vec![0; font.maxp.max_storage as usize];
        exec.cvt = font
            .cvt
            .iter()
            .map(|&c| mul_fix(c as i64, x_scale))
            .collect();
        let n_tw = font.maxp.max_twilight_points as usize + 4;
        exec.zones[0] = Zone::with_points(n_tw);
        exec.set_code(RANGE_FONT, font.fpgm.clone());
        exec.set_code(RANGE_CVT, font.prep.clone());
        exec.gs = Gs::default();
        if !font.fpgm.is_empty() {
            exec.run(RANGE_FONT);
        }
        exec.gs = Gs::default();
        if !font.prep.is_empty() {
            exec.run(RANGE_CVT);
        }
        // The CVT program may not change these (an undocumented MS rasteriser rule).
        exec.gs.dual = V { x: 0x4000, y: 0 };
        exec.gs.proj = V { x: 0x4000, y: 0 };
        exec.gs.free = V { x: 0x4000, y: 0 };
        exec.gs.rp0 = 0;
        exec.gs.rp1 = 0;
        exec.gs.rp2 = 0;
        exec.gs.gep0 = 1;
        exec.gs.gep1 = 1;
        exec.gs.gep2 = 1;
        exec.gs.loop_ = 1;
        let gs = exec.gs.clone();
        let hinting_disabled = gs.instruct_control & 1 != 0;
        let use_default_gs = gs.instruct_control & 2 != 0;
        TtSize {
            x_scale,
            y_scale,
            exact_scale,
            hinted_scale: x_scale,
            ppem: x_ppem,
            exec,
            gs,
            hinting_disabled,
            use_default_gs,
        }
    }

    /// `FT_Load_Glyph` with `FT_LOAD_NO_BITMAP` (+ `FT_LOAD_TARGET_MONO` when `hinted`,
    /// `FT_LOAD_NO_HINTING` otherwise).
    pub fn load(&mut self, font: &TtFont, gid: u16, hinted: bool) -> Option<GlyphOutline> {
        let hinted = hinted && !self.hinting_disabled;
        let sc = if hinted {
            self.hinted_scale
        } else {
            self.exact_scale
        };
        self.x_scale = sc;
        self.y_scale = sc;
        let mut ld = Loader {
            pts: Vec::new(),
            tags: Vec::new(),
            contours: Vec::new(),
            pp: [V::default(); 4],
            hinted,
            marks: Vec::new(),
        };
        self.load_rec(font, gid, 0, &mut ld)?;
        let pp1 = ld.pp[0];
        if pp1.x != 0 {
            for p in &mut ld.pts {
                p.x -= pp1.x;
            }
        }
        let mut advance = ld.pp[1].x - ld.pp[0].x;
        if hinted {
            advance = pix_round(advance);
        }
        for t in &mut ld.tags {
            *t &= TAG_ON;
        }
        Some(GlyphOutline {
            pts: ld.pts,
            tags: ld.tags,
            contours: ld.contours,
            advance,
            marks: ld.marks,
        })
    }

    fn scale_pp(&self, pp: &mut [V; 4]) {
        pp[0].x = mul_fix(pp[0].x, self.x_scale);
        pp[1].x = mul_fix(pp[1].x, self.x_scale);
        pp[2].y = mul_fix(pp[2].y, self.y_scale);
        pp[3].y = mul_fix(pp[3].y, self.y_scale);
    }

    fn set_pp(font: &TtFont, gid: u16, bbox: &[i16; 4]) -> [V; 4] {
        let (adv, lsb) = font.hmetrics(gid);
        let (tsb, vadv) = font.vmetrics(gid, bbox[3] as i32);
        let pp1x = bbox[0] as i64 - lsb as i64;
        let pp3y = bbox[3] as i64 + tsb as i64;
        [
            V { x: pp1x, y: 0 },
            V {
                x: pp1x + adv as i64,
                y: 0,
            },
            V { x: 0, y: pp3y },
            V {
                x: 0,
                y: pp3y - vadv as i64,
            },
        ]
    }

    fn load_rec(&mut self, font: &TtFont, gid: u16, depth: u32, ld: &mut Loader) -> Option<()> {
        if depth > font.maxp.max_component_depth.max(8) as u32 {
            return None;
        }
        let raw = font.raw_glyph(gid)?;
        match raw.body {
            RawBody::Empty => {
                let mut pp = Self::set_pp(font, gid, &[0; 4]);
                self.scale_pp(&mut pp);
                ld.pp = pp;
                Some(())
            }
            RawBody::Simple { ends, ins, pts } => {
                let mut pp = Self::set_pp(font, gid, &raw.bbox);
                let n = pts.len();
                let mut orus: Vec<V> = pts
                    .iter()
                    .map(|&(x, y, _)| V {
                        x: x as i64,
                        y: y as i64,
                    })
                    .collect();
                orus.extend_from_slice(&pp);
                let mut cur: Vec<V> = orus
                    .iter()
                    .map(|p| V {
                        x: mul_fix(p.x, self.x_scale),
                        y: mul_fix(p.y, self.y_scale),
                    })
                    .collect();
                let mut tags: Vec<u8> = pts.iter().map(|&(_, _, on)| on as u8).collect();
                tags.extend_from_slice(&[0; 4]);
                let mut zone = Zone {
                    org: cur.clone(),
                    cur: Vec::new(),
                    orus,
                    tags,
                    contours: ends.clone(),
                };
                if ld.hinted {
                    zone.cur = std::mem::take(&mut cur);
                    self.hint_glyph(&mut zone, &ins, false);
                    cur = std::mem::take(&mut zone.cur);
                }
                pp = [cur[n], cur[n + 1], cur[n + 2], cur[n + 3]];
                let base = ld.pts.len() as u16;
                ld.pts.extend_from_slice(&cur[..n]);
                ld.tags.extend_from_slice(&zone.tags[..n]);
                ld.contours.extend(ends.iter().map(|&e| e + base));
                ld.pp = pp;
                Some(())
            }
            RawBody::Composite { comps, ins } => {
                let mut pp = Self::set_pp(font, gid, &raw.bbox);
                self.scale_pp(&mut pp);
                ld.pp = pp;
                let start_point = ld.pts.len();
                let start_contour = ld.contours.len();
                for c in &comps {
                    let pp_before = ld.pp;
                    let num_base = ld.pts.len();
                    self.load_rec(font, c.gid, depth + 1, ld)?;
                    if c.flags & USE_MY_METRICS == 0 {
                        ld.pp = pp_before;
                    }
                    let num_points = ld.pts.len();
                    self.process_component(ld, c, start_point, num_base, num_points)?;
                }
                if ld.hinted && !ins.is_empty() {
                    let n = ld.pts.len() - start_point;
                    let mut cur: Vec<V> = ld.pts[start_point..].to_vec();
                    cur.extend_from_slice(&ld.pp);
                    let mut tags: Vec<u8> = ld.tags[start_point..]
                        .iter()
                        .map(|t| t & !TOUCH_BOTH)
                        .collect();
                    tags.extend_from_slice(&[0; 4]);
                    let mut zone = Zone {
                        org: cur.clone(),
                        cur,
                        orus: Vec::new(),
                        tags,
                        contours: ld.contours[start_contour..]
                            .iter()
                            .map(|&e| e - start_point as u16)
                            .collect(),
                    };
                    if let Some(st) = self.hint_glyph(&mut zone, &ins, true) {
                        ld.marks.push((start_point, st));
                    }
                    ld.pts.truncate(start_point);
                    ld.pts.extend_from_slice(&zone.cur[..n]);
                    ld.pp = [
                        zone.cur[n],
                        zone.cur[n + 1],
                        zone.cur[n + 2],
                        zone.cur[n + 3],
                    ];
                    for (i, t) in zone.tags[..n].iter().enumerate() {
                        ld.tags[start_point + i] = *t;
                    }
                }
                Some(())
            }
        }
    }

    fn process_component(
        &self,
        ld: &mut Loader,
        c: &Component,
        start_point: usize,
        num_base: usize,
        num_points: usize,
    ) -> Option<()> {
        let have_scale = c.flags & (WE_HAVE_A_SCALE | WE_HAVE_AN_XY_SCALE | WE_HAVE_A_2X2) != 0;
        if have_scale {
            for p in &mut ld.pts[num_base..num_points] {
                let x = mul_fix(p.x, c.xx) + mul_fix(p.y, c.xy);
                let y = mul_fix(p.x, c.yx) + mul_fix(p.y, c.yy);
                p.x = x;
                p.y = y;
            }
        }
        let (x, y);
        if c.flags & ARGS_ARE_XY_VALUES == 0 {
            let k = c.arg1 as usize + start_point;
            let l = c.arg2 as usize + num_base;
            if k >= num_base || l >= num_points {
                return None;
            }
            x = ld.pts[k].x - ld.pts[l].x;
            y = ld.pts[k].y - ld.pts[l].y;
        } else {
            let (mut ax, mut ay) = (c.arg1 as i64, c.arg2 as i64);
            if ax == 0 && ay == 0 {
                return Some(());
            }
            if have_scale && c.flags & SCALED_COMPONENT_OFFSET != 0 {
                let mx = hypot_fix(c.xx, c.xy);
                let my = hypot_fix(c.yy, c.yx);
                ax = mul_fix(ax, mx);
                ay = mul_fix(ay, my);
            }
            ax = mul_fix(ax, self.x_scale);
            ay = mul_fix(ay, self.y_scale);
            if c.flags & ROUND_XY_TO_GRID != 0 && ld.hinted {
                // Mono target (no lean subpixel hinting): both offsets are rounded.
                ax = pix_round(ax);
                ay = pix_round(ay);
            }
            x = ax;
            y = ay;
        }
        if x != 0 || y != 0 {
            for p in &mut ld.pts[num_base..num_points] {
                p.x += x;
                p.y += y;
            }
        }
        Some(())
    }

    /// `TT_Hint_Glyph`: run the glyph program on `zone` (which ends in the four phantom points).
    fn hint_glyph(&mut self, zone: &mut Zone, ins: &[u8], is_composite: bool) -> Option<u8> {
        let n = zone.cur.len();
        if !ins.is_empty() {
            zone.org = zone.cur.clone();
        }
        if is_composite {
            zone.orus = zone.cur.clone();
        }
        // Round the phantom points: pp1.x, pp2.x, pp3.y, pp4.y.
        zone.cur[n - 4].x = pix_round(zone.cur[n - 4].x);
        zone.cur[n - 3].x = pix_round(zone.cur[n - 3].x);
        zone.cur[n - 2].y = pix_round(zone.cur[n - 2].y);
        zone.cur[n - 1].y = pix_round(zone.cur[n - 1].y);
        if !ins.is_empty() {
            self.exec.gs = if self.use_default_gs {
                Gs::default()
            } else {
                self.gs.clone()
            };
            if is_composite {
                self.exec.x_scale = 1 << 16;
                self.exec.y_scale = 1 << 16;
            } else {
                self.exec.x_scale = self.x_scale;
                self.exec.y_scale = self.y_scale;
            }
            self.exec.trace = std::env::var("TT_TRACE").is_ok();
            self.exec.set_code(RANGE_GLYPH, ins.to_vec());
            self.exec.zones[1] = std::mem::take(zone);
            self.exec.run(RANGE_GLYPH);
            *zone = std::mem::take(&mut self.exec.zones[1]);
            return Some((self.exec.gs.scan_type & 7) as u8);
        }
        None
    }
}

/// `FT_Hypot` on 16.16 values.
fn hypot_fix(x: i64, y: i64) -> i64 {
    (((x as f64) * (x as f64) + (y as f64) * (y as f64)).sqrt() + 0.5) as i64
}
