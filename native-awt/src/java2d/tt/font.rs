// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The TrueType tables the hinter needs, parsed straight from the font file:
//! `head`, `maxp`, `hhea`, `hmtx`, `loca`, `glyf`, `cvt `, `fpgm`, `prep`,
//! `OS/2` and the optional vertical header.

use std::sync::Arc;

pub fn be16(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(o)?, *d.get(o + 1)?]))
}
pub fn be32(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *d.get(o)?,
        *d.get(o + 1)?,
        *d.get(o + 2)?,
        *d.get(o + 3)?,
    ]))
}
fn bei16(d: &[u8], o: usize) -> Option<i16> {
    be16(d, o).map(|v| v as i16)
}

#[derive(Clone, Copy, Default)]
struct Table {
    off: usize,
    len: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Maxp {
    pub num_glyphs: u16,
    pub max_twilight_points: u16,
    pub max_storage: u16,
    pub max_function_defs: u16,
    pub max_instruction_defs: u16,
    pub max_stack_elements: u16,
    pub max_component_depth: u16,
}

pub struct TtFont {
    pub data: Arc<Vec<u8>>,
    pub upem: u16,
    pub head_flags: u16,
    loca_long: bool,
    pub maxp: Maxp,
    pub num_hmetrics: u16,
    pub hhea_ascender: i16,
    pub hhea_descender: i16,
    pub os2_version: u16,
    pub os2_typo_ascender: i16,
    pub os2_typo_descender: i16,
    glyf: Table,
    loca: Table,
    hmtx: Table,
    vhea: Option<Table>,
    vmtx: Table,
    num_vmetrics: u16,
    pub cvt: Vec<i16>,
    pub fpgm: Vec<u8>,
    pub prep: Vec<u8>,
}

/// A parsed glyph record, before scaling.
pub struct RawGlyph {
    pub n_contours: i16,
    pub bbox: [i16; 4],
    pub body: RawBody,
}

pub enum RawBody {
    Empty,
    Simple {
        ends: Vec<u16>,
        ins: Vec<u8>,
        /// `(x, y, on_curve)` in font units.
        pts: Vec<(i32, i32, bool)>,
    },
    Composite {
        comps: Vec<Component>,
        ins: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
pub struct Component {
    pub flags: u16,
    pub gid: u16,
    pub arg1: i32,
    pub arg2: i32,
    /// 16.16 transform, FreeType's `xx, xy, yx, yy`.
    pub xx: i64,
    pub xy: i64,
    pub yx: i64,
    pub yy: i64,
}

pub const ARGS_ARE_WORDS: u16 = 0x0001;
pub const ARGS_ARE_XY_VALUES: u16 = 0x0002;
pub const ROUND_XY_TO_GRID: u16 = 0x0004;
pub const WE_HAVE_A_SCALE: u16 = 0x0008;
pub const MORE_COMPONENTS: u16 = 0x0020;
pub const WE_HAVE_AN_XY_SCALE: u16 = 0x0040;
pub const WE_HAVE_A_2X2: u16 = 0x0080;
pub const WE_HAVE_INSTR: u16 = 0x0100;
pub const USE_MY_METRICS: u16 = 0x0200;
pub const SCALED_COMPONENT_OFFSET: u16 = 0x0800;
pub const UNSCALED_COMPONENT_OFFSET: u16 = 0x1000;

impl TtFont {
    /// Parse the font at `index` of a file (TTC aware). `None` for anything that is
    /// not a TrueType-outline font with the tables the hinter needs.
    pub fn parse(data: Arc<Vec<u8>>, index: u32) -> Option<TtFont> {
        let d: &[u8] = &data;
        let mut base = 0usize;
        if d.get(0..4)? == b"ttcf" {
            base = be32(d, 12 + 4 * index as usize)? as usize;
        }
        let tag = be32(d, base)?;
        if tag != 0x0001_0000 && tag != 0x7472_7565 {
            return None;
        }
        let n = be16(d, base + 4)? as usize;
        let find = |name: &[u8; 4]| -> Option<Table> {
            for i in 0..n {
                let r = base + 12 + 16 * i;
                if d.get(r..r + 4)? == name {
                    return Some(Table {
                        off: be32(d, r + 8)? as usize,
                        len: be32(d, r + 12)? as usize,
                    });
                }
            }
            None
        };
        let head = find(b"head")?;
        let maxp_t = find(b"maxp")?;
        let hhea = find(b"hhea")?;
        let glyf = find(b"glyf")?;
        let loca = find(b"loca")?;
        let hmtx = find(b"hmtx")?;
        let upem = be16(d, head.off + 18)?;
        let head_flags = be16(d, head.off + 16)?;
        let loca_long = bei16(d, head.off + 50)? != 0;
        let maxp = Maxp {
            num_glyphs: be16(d, maxp_t.off + 4)?,
            max_twilight_points: be16(d, maxp_t.off + 16).unwrap_or(0),
            max_storage: be16(d, maxp_t.off + 18).unwrap_or(0),
            max_function_defs: be16(d, maxp_t.off + 20).unwrap_or(0),
            max_instruction_defs: be16(d, maxp_t.off + 22).unwrap_or(0),
            max_stack_elements: be16(d, maxp_t.off + 24).unwrap_or(0),
            max_component_depth: be16(d, maxp_t.off + 30).unwrap_or(0),
        };
        let slice = |t: Option<Table>| -> Vec<u8> {
            t.and_then(|t| d.get(t.off..t.off + t.len))
                .map(|s| s.to_vec())
                .unwrap_or_default()
        };
        let cvt_bytes = slice(find(b"cvt "));
        let cvt = cvt_bytes
            .chunks_exact(2)
            .map(|c| i16::from_be_bytes([c[0], c[1]]))
            .collect();
        let (os2_version, ta, td) = match find(b"OS/2") {
            Some(t) => (
                be16(d, t.off).unwrap_or(0xFFFF),
                bei16(d, t.off + 68).unwrap_or(0),
                bei16(d, t.off + 70).unwrap_or(0),
            ),
            None => (0xFFFF, 0, 0),
        };
        let vhea = find(b"vhea");
        let num_vmetrics = vhea.and_then(|t| be16(d, t.off + 34)).unwrap_or(0);
        Some(TtFont {
            upem,
            head_flags,
            loca_long,
            maxp,
            num_hmetrics: be16(d, hhea.off + 34)?,
            hhea_ascender: bei16(d, hhea.off + 4)?,
            hhea_descender: bei16(d, hhea.off + 6)?,
            os2_version,
            os2_typo_ascender: ta,
            os2_typo_descender: td,
            glyf,
            loca,
            hmtx,
            vhea,
            vmtx: find(b"vmtx").unwrap_or_default(),
            num_vmetrics,
            cvt,
            fpgm: slice(find(b"fpgm")),
            prep: slice(find(b"prep")),
            data,
        })
    }

    pub fn num_glyphs(&self) -> u16 {
        self.maxp.num_glyphs
    }

    /// `(advance, left side bearing)` of the horizontal metrics.
    pub fn hmetrics(&self, gid: u16) -> (i32, i32) {
        let d: &[u8] = &self.data;
        let nh = self.num_hmetrics as usize;
        if nh == 0 {
            return (0, 0);
        }
        let g = gid as usize;
        if g < nh {
            let o = self.hmtx.off + 4 * g;
            (
                be16(d, o).unwrap_or(0) as i32,
                bei16(d, o + 2).unwrap_or(0) as i32,
            )
        } else {
            let adv = be16(d, self.hmtx.off + 4 * (nh - 1)).unwrap_or(0) as i32;
            let o = self.hmtx.off + 4 * nh + 2 * (g - nh);
            (adv, bei16(d, o).unwrap_or(0) as i32)
        }
    }

    /// `TT_Get_VMetrics`: `(top side bearing, advance height)`.
    pub fn vmetrics(&self, gid: u16, y_max: i32) -> (i32, i32) {
        let d: &[u8] = &self.data;
        if self.vhea.is_some() && self.num_vmetrics > 0 {
            let nv = self.num_vmetrics as usize;
            let g = gid as usize;
            if g < nv {
                let o = self.vmtx.off + 4 * g;
                return (
                    bei16(d, o + 2).unwrap_or(0) as i32,
                    be16(d, o).unwrap_or(0) as i32,
                );
            }
            let adv = be16(d, self.vmtx.off + 4 * (nv - 1)).unwrap_or(0) as i32;
            let o = self.vmtx.off + 4 * nv + 2 * (g - nv);
            return (bei16(d, o).unwrap_or(0) as i32, adv);
        }
        if self.os2_version != 0xFFFF {
            (
                self.os2_typo_ascender as i32 - y_max,
                (self.os2_typo_ascender as i32 - self.os2_typo_descender as i32).abs(),
            )
        } else {
            (
                self.hhea_ascender as i32 - y_max,
                (self.hhea_ascender as i32 - self.hhea_descender as i32).abs(),
            )
        }
    }

    fn glyph_range(&self, gid: u16) -> Option<(usize, usize)> {
        let d: &[u8] = &self.data;
        let g = gid as usize;
        if g >= self.maxp.num_glyphs as usize {
            return None;
        }
        let (a, b) = if self.loca_long {
            (
                be32(d, self.loca.off + 4 * g)? as usize,
                be32(d, self.loca.off + 4 * (g + 1))
                    .map(|v| v as usize)
                    .unwrap_or(self.glyf.len),
            )
        } else {
            (
                be16(d, self.loca.off + 2 * g)? as usize * 2,
                be16(d, self.loca.off + 2 * (g + 1))
                    .map(|v| v as usize * 2)
                    .unwrap_or(self.glyf.len),
            )
        };
        let b = b.min(self.glyf.len);
        if b <= a {
            return Some((a, a));
        }
        Some((self.glyf.off + a, self.glyf.off + b))
    }

    /// Parse the glyph record; `None` when the data is malformed.
    pub fn raw_glyph(&self, gid: u16) -> Option<RawGlyph> {
        let d: &[u8] = &self.data;
        let (s, e) = self.glyph_range(gid)?;
        if e <= s {
            return Some(RawGlyph {
                n_contours: 0,
                bbox: [0; 4],
                body: RawBody::Empty,
            });
        }
        let g = d.get(s..e)?;
        let nc = bei16(g, 0)?;
        let bbox = [bei16(g, 2)?, bei16(g, 4)?, bei16(g, 6)?, bei16(g, 8)?];
        let mut p = 10usize;
        if nc >= 0 {
            let nc_u = nc as usize;
            let mut ends = Vec::with_capacity(nc_u);
            for i in 0..nc_u {
                ends.push(be16(g, p + 2 * i)?);
            }
            p += 2 * nc_u;
            let ilen = be16(g, p)? as usize;
            p += 2;
            let ins = g.get(p..p + ilen)?.to_vec();
            p += ilen;
            let n_points = ends.last().map(|&v| v as usize + 1).unwrap_or(0);
            let mut flags = Vec::with_capacity(n_points);
            while flags.len() < n_points {
                let f = *g.get(p)?;
                p += 1;
                flags.push(f);
                if f & 8 != 0 {
                    let r = *g.get(p)? as usize;
                    p += 1;
                    for _ in 0..r {
                        if flags.len() >= n_points {
                            break;
                        }
                        flags.push(f);
                    }
                }
            }
            let mut xs = Vec::with_capacity(n_points);
            let mut x = 0i32;
            for &f in &flags {
                if f & 2 != 0 {
                    let dx = *g.get(p)? as i32;
                    p += 1;
                    x += if f & 16 != 0 { dx } else { -dx };
                } else if f & 16 == 0 {
                    x += bei16(g, p)? as i32;
                    p += 2;
                }
                xs.push(x);
            }
            let mut pts = Vec::with_capacity(n_points);
            let mut y = 0i32;
            for (i, &f) in flags.iter().enumerate() {
                if f & 4 != 0 {
                    let dy = *g.get(p)? as i32;
                    p += 1;
                    y += if f & 32 != 0 { dy } else { -dy };
                } else if f & 32 == 0 {
                    y += bei16(g, p)? as i32;
                    p += 2;
                }
                pts.push((xs[i], y, f & 1 != 0));
            }
            Some(RawGlyph {
                n_contours: nc,
                bbox,
                body: RawBody::Simple { ends, ins, pts },
            })
        } else {
            let mut comps = Vec::new();
            let mut flags;
            loop {
                flags = be16(g, p)?;
                let gid = be16(g, p + 2)?;
                p += 4;
                let (a1, a2);
                if flags & ARGS_ARE_WORDS != 0 {
                    if flags & ARGS_ARE_XY_VALUES != 0 {
                        a1 = bei16(g, p)? as i32;
                        a2 = bei16(g, p + 2)? as i32;
                    } else {
                        a1 = be16(g, p)? as i32;
                        a2 = be16(g, p + 2)? as i32;
                    }
                    p += 4;
                } else {
                    if flags & ARGS_ARE_XY_VALUES != 0 {
                        a1 = *g.get(p)? as i8 as i32;
                        a2 = *g.get(p + 1)? as i8 as i32;
                    } else {
                        a1 = *g.get(p)? as i32;
                        a2 = *g.get(p + 1)? as i32;
                    }
                    p += 2;
                }
                let (mut xx, mut xy, mut yx, mut yy) = (0x10000i64, 0i64, 0i64, 0x10000i64);
                if flags & WE_HAVE_A_SCALE != 0 {
                    xx = bei16(g, p)? as i64 * 4;
                    yy = xx;
                    p += 2;
                } else if flags & WE_HAVE_AN_XY_SCALE != 0 {
                    xx = bei16(g, p)? as i64 * 4;
                    yy = bei16(g, p + 2)? as i64 * 4;
                    p += 4;
                } else if flags & WE_HAVE_A_2X2 != 0 {
                    xx = bei16(g, p)? as i64 * 4;
                    yx = bei16(g, p + 2)? as i64 * 4;
                    xy = bei16(g, p + 4)? as i64 * 4;
                    yy = bei16(g, p + 6)? as i64 * 4;
                    p += 8;
                }
                comps.push(Component {
                    flags,
                    gid,
                    arg1: a1,
                    arg2: a2,
                    xx,
                    xy,
                    yx,
                    yy,
                });
                if flags & MORE_COMPONENTS == 0 {
                    break;
                }
            }
            let mut ins = Vec::new();
            if flags & WE_HAVE_INSTR != 0 {
                let il = be16(g, p)? as usize;
                p += 2;
                ins = g.get(p..p + il)?.to_vec();
            }
            Some(RawGlyph {
                n_contours: nc,
                bbox,
                body: RawBody::Composite { comps, ins },
            })
        }
    }
}
