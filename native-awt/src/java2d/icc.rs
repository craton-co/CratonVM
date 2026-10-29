// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A small ICC evaluator for the profiles `moxcms` does not take: the JDK's
//! abstract `CIEXYZ.pf` and `PYCC.pf` (a `3CLR` space with an `mAB` table), and
//! any other device/PCS table profile. It reads the tags a transform needs
//! (`curv` / `para` curves, `mft1` / `mft2` / `mAB` / `mBA` tables, colorant
//! matrices) and runs `device -> PCS XYZ(D50) -> device` in `f32`, the way
//! lcms's float pipeline does without black-point compensation.
//!
//! All samples are normalised to `[0, 1]`: a device value is the formatter's
//! sample over its maximum, a PCS value is the encoded one (so XYZ `1.0` is
//! `1 + 32767/32768`).

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn s15(b: &[u8], at: usize) -> Option<f32> {
    Some(be32(b, at)? as i32 as f32 / 65536.0)
}

const D50: [f32; 3] = [0.9642, 1.0, 0.8249];
/// PCS XYZ encoding: 1.0 normalised is `1 + 32767/32768`.
const XYZ_SCALE: f32 = 1.0 + 32767.0 / 32768.0;

#[derive(Clone, Debug)]
enum Curve {
    Identity,
    Gamma(f32),
    Table(Vec<f32>),
    Para(u16, [f32; 7]),
}

impl Curve {
    fn eval(&self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Curve::Identity => x,
            Curve::Gamma(g) => x.powf(*g),
            Curve::Table(t) => {
                let n = t.len();
                if n == 1 {
                    return t[0];
                }
                let p = x * (n - 1) as f32;
                let i = (p.floor() as usize).min(n - 2);
                let f = p - i as f32;
                t[i] * (1.0 - f) + t[i + 1] * f
            }
            Curve::Para(kind, p) => {
                let (g, a, b, c, d, e, f) = (p[0], p[1], p[2], p[3], p[4], p[5], p[6]);
                let y = match kind {
                    0 => x.powf(g),
                    1 => {
                        if x >= -b / a {
                            (a * x + b).max(0.0).powf(g)
                        } else {
                            0.0
                        }
                    }
                    2 => {
                        if x >= -b / a {
                            (a * x + b).max(0.0).powf(g) + c
                        } else {
                            c
                        }
                    }
                    3 => {
                        if x >= d {
                            (a * x + b).max(0.0).powf(g)
                        } else {
                            c * x
                        }
                    }
                    _ => {
                        if x >= d {
                            (a * x + b).max(0.0).powf(g) + e
                        } else {
                            c * x + f
                        }
                    }
                };
                y.clamp(0.0, 1.0)
            }
        }
    }

    /// The curve's inverse by bisection (curves are monotonic increasing).
    fn inverse(&self, y: f32) -> f32 {
        if let Curve::Identity = self {
            return y.clamp(0.0, 1.0);
        }
        let y = y.clamp(0.0, 1.0);
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        for _ in 0..24 {
            let mid = 0.5 * (lo + hi);
            if self.eval(mid) < y {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    }
}

/// A curve at `at` in `b` and the bytes it used (padded to 4 when `padded`).
fn parse_curve(b: &[u8], at: usize, padded: bool) -> Option<(Curve, usize)> {
    let sig = b.get(at..at + 4)?;
    let (curve, used) = match sig {
        b"curv" => {
            let n = be32(b, at + 8)? as usize;
            let used = 12 + 2 * n;
            let c = match n {
                0 => Curve::Identity,
                1 => Curve::Gamma(be16(b, at + 12)? as f32 / 256.0),
                _ => Curve::Table(
                    (0..n)
                        .map(|i| be16(b, at + 12 + 2 * i).map(|v| v as f32 / 65535.0))
                        .collect::<Option<Vec<_>>>()?,
                ),
            };
            (c, used)
        }
        b"para" => {
            let kind = be16(b, at + 8)?;
            let n = match kind {
                0 => 1,
                1 => 3,
                2 => 4,
                3 => 5,
                4 => 7,
                _ => return None,
            };
            let mut p = [0f32; 7];
            for (i, slot) in p.iter_mut().enumerate().take(n) {
                *slot = s15(b, at + 12 + 4 * i)?;
            }
            (Curve::Para(kind, p), 12 + 4 * n)
        }
        _ => return None,
    };
    let used = if padded { (used + 3) & !3 } else { used };
    Some((curve, used))
}

fn parse_curve_set(b: &[u8], mut at: usize, n: usize) -> Option<Vec<Curve>> {
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let (c, used) = parse_curve(b, at, true)?;
        v.push(c);
        at += used;
    }
    Some(v)
}

#[derive(Clone, Debug)]
struct Clut {
    grid: Vec<usize>,
    out_ch: usize,
    data: Vec<f32>,
}

impl Clut {
    fn at(&self, idx: &[usize], o: usize) -> f32 {
        let mut off = 0usize;
        for (i, &g) in idx.iter().zip(&self.grid) {
            off = off * g + (*i).min(g - 1);
        }
        self.data[off * self.out_ch + o]
    }

    fn eval(&self, input: &[f32], out: &mut [f32]) {
        let n = self.grid.len();
        let mut base = [0usize; 8];
        let mut frac = [0f32; 8];
        for k in 0..n {
            let g = self.grid[k];
            let p = input[k].clamp(0.0, 1.0) * (g - 1) as f32;
            let i = (p.floor() as usize).min(g.saturating_sub(2).max(0));
            base[k] = i;
            frac[k] = if g > 1 { p - i as f32 } else { 0.0 };
        }
        for (o, slot) in out.iter_mut().enumerate().take(self.out_ch) {
            *slot = if n == 3 {
                self.tetra(&base, &frac, o)
            } else {
                self.multilinear(n, &base, &frac, o)
            };
        }
    }

    fn tetra(&self, base: &[usize; 8], frac: &[f32; 8], o: usize) -> f32 {
        let (x, y, z) = (frac[0], frac[1], frac[2]);
        let g = |dx: usize, dy: usize, dz: usize| {
            self.at(&[base[0] + dx, base[1] + dy, base[2] + dz], o)
        };
        let c000 = g(0, 0, 0);
        let c111 = g(1, 1, 1);
        let (c1, c2, c3);
        if x >= y && y >= z {
            c1 = g(1, 0, 0) - c000;
            c2 = g(1, 1, 0) - g(1, 0, 0);
            c3 = c111 - g(1, 1, 0);
        } else if x >= z && z >= y {
            c1 = g(1, 0, 0) - c000;
            c2 = c111 - g(1, 0, 1);
            c3 = g(1, 0, 1) - g(1, 0, 0);
        } else if z >= x && x >= y {
            c1 = g(1, 0, 1) - g(0, 0, 1);
            c2 = c111 - g(1, 0, 1);
            c3 = g(0, 0, 1) - c000;
        } else if y >= x && x >= z {
            c1 = g(1, 1, 0) - g(0, 1, 0);
            c2 = g(0, 1, 0) - c000;
            c3 = c111 - g(1, 1, 0);
        } else if y >= z && z >= x {
            c1 = c111 - g(0, 1, 1);
            c2 = g(0, 1, 0) - c000;
            c3 = g(0, 1, 1) - g(0, 1, 0);
        } else {
            c1 = c111 - g(0, 1, 1);
            c2 = g(0, 1, 1) - g(0, 0, 1);
            c3 = g(0, 0, 1) - c000;
        }
        c000 + c1 * x + c2 * y + c3 * z
    }

    fn multilinear(&self, n: usize, base: &[usize; 8], frac: &[f32; 8], o: usize) -> f32 {
        let mut acc = 0f32;
        for corner in 0..(1usize << n) {
            let mut w = 1f32;
            let mut idx = [0usize; 8];
            for k in 0..n {
                let up = (corner >> k) & 1;
                w *= if up == 1 { frac[k] } else { 1.0 - frac[k] };
                idx[k] = base[k] + up;
            }
            if w != 0.0 {
                acc += w * self.at(&idx[..n], o);
            }
        }
        acc
    }
}

fn parse_clut(b: &[u8], at: usize, in_ch: usize, out_ch: usize) -> Option<Clut> {
    let grid: Vec<usize> = (0..in_ch).map(|i| *b.get(at + i).unwrap_or(&0) as usize).collect();
    if grid.iter().any(|&g| g == 0) {
        return None;
    }
    let prec = *b.get(at + 16)? as usize;
    let count: usize = grid.iter().product::<usize>().checked_mul(out_ch)?;
    let start = at + 20;
    let data: Vec<f32> = match prec {
        1 => b.get(start..start + count)?.iter().map(|&v| v as f32 / 255.0).collect(),
        2 => (0..count)
            .map(|i| be16(b, start + 2 * i).map(|v| v as f32 / 65535.0))
            .collect::<Option<Vec<_>>>()?,
        _ => return None,
    };
    Some(Clut { grid, out_ch, data })
}

#[derive(Clone, Debug)]
enum LutKind {
    /// `mft1` / `mft2`: matrix (3 channels), input curves, CLUT, output curves.
    Legacy {
        matrix: Option<[f32; 9]>,
        input: Vec<Curve>,
        clut: Option<Clut>,
        output: Vec<Curve>,
        /// `mft2` Lab encoding differs from the v4 one.
        v2_lab: bool,
    },
    /// `mAB ` (device to PCS) or `mBA ` (PCS to device).
    Mab {
        a2b: bool,
        a: Vec<Curve>,
        clut: Option<Clut>,
        m: Vec<Curve>,
        matrix: Option<[f32; 12]>,
        b: Vec<Curve>,
    },
}

#[derive(Clone, Debug)]
struct Lut {
    in_ch: usize,
    out_ch: usize,
    kind: LutKind,
}

fn parse_lut(t: &[u8]) -> Option<Lut> {
    match t.get(..4)? {
        b"mft1" | b"mft2" => {
            let wide = &t[..4] == b"mft2";
            let (i, o, g) = (t[8] as usize, t[9] as usize, t[10] as usize);
            let mut m = [0f32; 9];
            for (k, slot) in m.iter_mut().enumerate() {
                *slot = s15(t, 12 + 4 * k)?;
            }
            let identity = m.iter().enumerate().all(|(k, v)| {
                (v - if k % 4 == 0 { 1.0 } else { 0.0 }).abs() < 1.0 / 32768.0
            });
            let (n_in, n_out, mut at) = if wide {
                (be16(t, 48)? as usize, be16(t, 50)? as usize, 52)
            } else {
                (256, 256, 48)
            };
            let w = if wide { 2 } else { 1 };
            let read = |b: &[u8], at: usize| -> Option<f32> {
                if wide {
                    be16(b, at).map(|v| v as f32 / 65535.0)
                } else {
                    b.get(at).map(|&v| v as f32 / 255.0)
                }
            };
            let mut input = Vec::new();
            for _ in 0..i {
                let tab = (0..n_in).map(|k| read(t, at + w * k)).collect::<Option<Vec<_>>>()?;
                at += w * n_in;
                input.push(if n_in <= 1 { Curve::Identity } else { Curve::Table(tab) });
            }
            // A grid of 0 is a table-only profile: no CLUT, the curves meet directly.
            let clut = if g == 0 {
                None
            } else {
                let count = g.checked_pow(i as u32)?.checked_mul(o)?;
                let data = (0..count).map(|k| read(t, at + w * k)).collect::<Option<Vec<_>>>()?;
                at += w * count;
                Some(Clut {
                    grid: vec![g; i],
                    out_ch: o,
                    data,
                })
            };
            let mut output = Vec::new();
            for _ in 0..o {
                let tab = (0..n_out).map(|k| read(t, at + w * k)).collect::<Option<Vec<_>>>()?;
                at += w * n_out;
                output.push(if n_out <= 1 { Curve::Identity } else { Curve::Table(tab) });
            }
            Some(Lut {
                in_ch: i,
                out_ch: o,
                kind: LutKind::Legacy {
                    matrix: if identity || i != 3 { None } else { Some(m) },
                    input,
                    clut,
                    output,
                    v2_lab: wide,
                },
            })
        }
        b"mAB " | b"mBA " => {
            let a2b = &t[..4] == b"mAB ";
            let (i, o) = (t[8] as usize, t[9] as usize);
            let off = |k: usize| be32(t, 12 + 4 * k).map(|v| v as usize);
            let (ob, om, omc, oc, oa) = (off(0)?, off(1)?, off(2)?, off(3)?, off(4)?);
            // Channels at each stage: A2B: A(i) CLUT(i->o) M(o) matrix(o) B(o);
            // B2A: B(i) matrix(i) M(i) CLUT(i->o) A(o).
            let (a_n, mid_n, b_n) = if a2b { (i, o, o) } else { (o, i, i) };
            let a = if oa != 0 { parse_curve_set(t, oa, a_n)? } else { Vec::new() };
            let m = if omc != 0 { parse_curve_set(t, omc, mid_n)? } else { Vec::new() };
            let b = if ob != 0 { parse_curve_set(t, ob, b_n)? } else { Vec::new() };
            let matrix = if om != 0 {
                let mut mx = [0f32; 12];
                for (k, slot) in mx.iter_mut().enumerate() {
                    *slot = s15(t, om + 4 * k)?;
                }
                Some(mx)
            } else {
                None
            };
            let clut = if oc != 0 { Some(parse_clut(t, oc, i, o)?) } else { None };
            Some(Lut {
                in_ch: i,
                out_ch: o,
                kind: LutKind::Mab {
                    a2b,
                    a,
                    clut,
                    m,
                    matrix,
                    b,
                },
            })
        }
        _ => None,
    }
}

fn apply_curves(curves: &[Curve], v: &mut [f32]) {
    for (c, x) in curves.iter().zip(v.iter_mut()) {
        *x = c.eval(*x);
    }
}

impl Lut {
    fn run(&self, input: &[f32], out: &mut [f32]) {
        match &self.kind {
            LutKind::Legacy {
                matrix,
                input: ic,
                clut,
                output,
                ..
            } => {
                let mut v = [0f32; 8];
                v[..input.len()].copy_from_slice(input);
                if let Some(m) = matrix {
                    let (x, y, z) = (v[0], v[1], v[2]);
                    v[0] = m[0] * x + m[1] * y + m[2] * z;
                    v[1] = m[3] * x + m[4] * y + m[5] * z;
                    v[2] = m[6] * x + m[7] * y + m[8] * z;
                }
                apply_curves(ic, &mut v[..self.in_ch]);
                let mut o = [0f32; 8];
                match clut {
                    Some(c) => c.eval(&v[..self.in_ch], &mut o[..self.out_ch]),
                    None => o[..self.in_ch].copy_from_slice(&v[..self.in_ch]),
                }
                apply_curves(output, &mut o[..self.out_ch]);
                out[..self.out_ch].copy_from_slice(&o[..self.out_ch]);
            }
            LutKind::Mab {
                a2b,
                a,
                clut,
                m,
                matrix,
                b,
            } => {
                let mut v = [0f32; 8];
                v[..input.len()].copy_from_slice(input);
                let mut n = self.in_ch;
                let mat = |v: &mut [f32; 8], mx: &[f32; 12]| {
                    let (x, y, z) = (v[0], v[1], v[2]);
                    v[0] = (mx[0] * x + mx[1] * y + mx[2] * z + mx[9]).clamp(0.0, 1.0);
                    v[1] = (mx[3] * x + mx[4] * y + mx[5] * z + mx[10]).clamp(0.0, 1.0);
                    v[2] = (mx[6] * x + mx[7] * y + mx[8] * z + mx[11]).clamp(0.0, 1.0);
                };
                if *a2b {
                    apply_curves(a, &mut v[..n]);
                    if let Some(c) = clut {
                        let mut o = [0f32; 8];
                        c.eval(&v[..n], &mut o[..self.out_ch]);
                        v = o;
                        n = self.out_ch;
                    }
                    apply_curves(m, &mut v[..n]);
                    if let Some(mx) = matrix {
                        mat(&mut v, mx);
                    }
                    apply_curves(b, &mut v[..n]);
                } else {
                    apply_curves(b, &mut v[..n]);
                    if let Some(mx) = matrix {
                        mat(&mut v, mx);
                    }
                    apply_curves(m, &mut v[..n]);
                    if let Some(c) = clut {
                        let mut o = [0f32; 8];
                        c.eval(&v[..n], &mut o[..self.out_ch]);
                        v = o;
                        n = self.out_ch;
                    }
                    apply_curves(a, &mut v[..n]);
                }
                out[..self.out_ch].copy_from_slice(&v[..self.out_ch]);
            }
        }
    }

    fn v2_lab(&self) -> bool {
        matches!(self.kind, LutKind::Legacy { v2_lab: true, .. })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pcs {
    Xyz,
    Lab,
}

#[derive(Clone, Debug)]
enum Shaper {
    /// RGB colorants (columns r, g, b) and curves.
    Matrix {
        cols: [[f32; 3]; 3],
        trc: [Curve; 3],
    },
    Gray(Curve),
    None,
}

/// A parsed profile: what a transform needs from it.
#[derive(Clone, Debug)]
pub struct Profile {
    channels: usize,
    pcs: Pcs,
    a2b: [Option<Lut>; 3],
    b2a: [Option<Lut>; 3],
    shaper: Shaper,
}

fn space_channels(sig: &[u8]) -> Option<usize> {
    Some(match sig {
        b"GRAY" => 1,
        b"RGB " | b"XYZ " | b"Lab " | b"Luv " | b"YCbr" | b"Yxy " | b"HSV " | b"HLS " | b"CMY "
        | b"3CLR" => 3,
        b"CMYK" | b"4CLR" => 4,
        b"5CLR" => 5,
        b"6CLR" => 6,
        _ => return None,
    })
}

fn tag<'a>(p: &'a [u8], sig: &[u8; 4]) -> Option<&'a [u8]> {
    let n = be32(p, 128)? as usize;
    for i in 0..n.min(4096) {
        let at = 132 + 12 * i;
        if p.get(at..at + 4)? == sig {
            let (off, size) = (be32(p, at + 4)? as usize, be32(p, at + 8)? as usize);
            return p.get(off..off.checked_add(size)?);
        }
    }
    None
}

impl Profile {
    pub fn parse(p: &[u8]) -> Option<Profile> {
        if p.len() < 132 || p.get(36..40)? != b"acsp" {
            return None;
        }
        let channels = space_channels(p.get(16..20)?)?;
        let pcs = match p.get(20..24)? {
            b"XYZ " => Pcs::Xyz,
            b"Lab " => Pcs::Lab,
            _ => return None,
        };
        let lut = |sig: &[u8; 4]| tag(p, sig).and_then(parse_lut);
        let a2b = [lut(b"A2B0"), lut(b"A2B1"), lut(b"A2B2")];
        let b2a = [lut(b"B2A0"), lut(b"B2A1"), lut(b"B2A2")];
        let shaper = if a2b.iter().any(Option::is_some) {
            Shaper::None
        } else if channels == 3 {
            let xyz = |sig: &[u8; 4]| -> Option<[f32; 3]> {
                let t = tag(p, sig)?;
                Some([s15(t, 8)?, s15(t, 12)?, s15(t, 16)?])
            };
            let curve = |sig: &[u8; 4]| tag(p, sig).and_then(|t| parse_curve(t, 0, false)).map(|c| c.0);
            match (
                xyz(b"rXYZ"),
                xyz(b"gXYZ"),
                xyz(b"bXYZ"),
                curve(b"rTRC"),
                curve(b"gTRC"),
                curve(b"bTRC"),
            ) {
                (Some(r), Some(g), Some(b), Some(rc), Some(gc), Some(bc)) => Shaper::Matrix {
                    cols: [r, g, b],
                    trc: [rc, gc, bc],
                },
                _ => Shaper::None,
            }
        } else if channels == 1 {
            match tag(p, b"kTRC").and_then(|t| parse_curve(t, 0, false)) {
                Some((c, _)) => Shaper::Gray(c),
                None => Shaper::None,
            }
        } else {
            Shaper::None
        };
        if matches!(shaper, Shaper::None) && a2b.iter().all(Option::is_none) {
            return None;
        }
        Some(Profile {
            channels,
            pcs,
            a2b,
            b2a,
            shaper,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    fn pick<'a>(&'a self, tables: &'a [Option<Lut>; 3], intent: i32) -> Option<&'a Lut> {
        let order: [usize; 3] = match intent {
            1 | 3 => [1, 0, 2],
            2 => [2, 0, 1],
            _ => [0, 1, 2],
        };
        order.iter().find_map(|&i| tables[i].as_ref())
    }

    /// Device values (normalised) to PCS XYZ (D50, unencoded).
    fn to_xyz(&self, dev: &[f32], intent: i32) -> [f32; 3] {
        if let Some(lut) = self.pick(&self.a2b, intent) {
            let mut o = [0f32; 8];
            lut.run(dev, &mut o);
            return decode_pcs([o[0], o[1], o[2]], self.pcs, lut.v2_lab());
        }
        match &self.shaper {
            Shaper::Matrix { cols, trc } => {
                let l = [trc[0].eval(dev[0]), trc[1].eval(dev[1]), trc[2].eval(dev[2])];
                [
                    cols[0][0] * l[0] + cols[1][0] * l[1] + cols[2][0] * l[2],
                    cols[0][1] * l[0] + cols[1][1] * l[1] + cols[2][1] * l[2],
                    cols[0][2] * l[0] + cols[1][2] * l[1] + cols[2][2] * l[2],
                ]
            }
            Shaper::Gray(c) => {
                let y = c.eval(dev[0]);
                [D50[0] * y, y, D50[2] * y]
            }
            Shaper::None => [0.0; 3],
        }
    }

    /// PCS XYZ to device values (normalised).
    fn from_xyz(&self, xyz: [f32; 3], out: &mut [f32], intent: i32) {
        if let Some(lut) = self.pick(&self.b2a, intent) {
            let enc = encode_pcs(xyz, self.pcs, lut.v2_lab());
            let mut o = [0f32; 8];
            lut.run(&enc, &mut o);
            out[..self.channels].copy_from_slice(&o[..self.channels]);
            return;
        }
        match &self.shaper {
            Shaper::Matrix { cols, trc } => {
                let inv = invert3(cols);
                let lin = [
                    inv[0][0] * xyz[0] + inv[0][1] * xyz[1] + inv[0][2] * xyz[2],
                    inv[1][0] * xyz[0] + inv[1][1] * xyz[1] + inv[1][2] * xyz[2],
                    inv[2][0] * xyz[0] + inv[2][1] * xyz[1] + inv[2][2] * xyz[2],
                ];
                for k in 0..3 {
                    out[k] = trc[k].inverse(lin[k]);
                }
            }
            Shaper::Gray(c) => out[0] = c.inverse(xyz[1]),
            Shaper::None => out[..self.channels].fill(0.0),
        }
    }
}

fn invert3(c: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
    // `c` holds the columns; build the row-major matrix M[row][col] = c[col][row].
    let m = |r: usize, k: usize| c[k][r] as f64;
    let det = m(0, 0) * (m(1, 1) * m(2, 2) - m(1, 2) * m(2, 1))
        - m(0, 1) * (m(1, 0) * m(2, 2) - m(1, 2) * m(2, 0))
        + m(0, 2) * (m(1, 0) * m(2, 1) - m(1, 1) * m(2, 0));
    if det.abs() < 1e-12 {
        return [[0.0; 3]; 3];
    }
    let d = 1.0 / det;
    let f = |v: f64| (v * d) as f32;
    [
        [
            f(m(1, 1) * m(2, 2) - m(1, 2) * m(2, 1)),
            f(m(0, 2) * m(2, 1) - m(0, 1) * m(2, 2)),
            f(m(0, 1) * m(1, 2) - m(0, 2) * m(1, 1)),
        ],
        [
            f(m(1, 2) * m(2, 0) - m(1, 0) * m(2, 2)),
            f(m(0, 0) * m(2, 2) - m(0, 2) * m(2, 0)),
            f(m(0, 2) * m(1, 0) - m(0, 0) * m(1, 2)),
        ],
        [
            f(m(1, 0) * m(2, 1) - m(1, 1) * m(2, 0)),
            f(m(0, 1) * m(2, 0) - m(0, 0) * m(2, 1)),
            f(m(0, 0) * m(1, 1) - m(0, 1) * m(1, 0)),
        ],
    ]
}

fn lab_to_xyz(l: f32, a: f32, b: f32) -> [f32; 3] {
    let fy = (l + 16.0) / 116.0;
    let (fx, fz) = (fy + a / 500.0, fy - b / 200.0);
    let finv = |t: f32| {
        if t > 6.0 / 29.0 {
            t * t * t
        } else {
            3.0 * (6.0f32 / 29.0).powi(2) * (t - 4.0 / 29.0)
        }
    };
    [D50[0] * finv(fx), D50[1] * finv(fy), D50[2] * finv(fz)]
}

fn xyz_to_lab(xyz: [f32; 3]) -> [f32; 3] {
    let f = |t: f32| {
        if t > (6.0f32 / 29.0).powi(3) {
            t.cbrt()
        } else {
            t / (3.0 * (6.0f32 / 29.0).powi(2)) + 4.0 / 29.0
        }
    };
    let (fx, fy, fz) = (f(xyz[0] / D50[0]), f(xyz[1] / D50[1]), f(xyz[2] / D50[2]));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

fn decode_pcs(e: [f32; 3], pcs: Pcs, v2: bool) -> [f32; 3] {
    match pcs {
        Pcs::Xyz => [e[0] * XYZ_SCALE, e[1] * XYZ_SCALE, e[2] * XYZ_SCALE],
        Pcs::Lab => {
            if v2 {
                lab_to_xyz(
                    e[0] * 65535.0 / 65280.0 * 100.0,
                    e[1] * 65535.0 / 256.0 - 128.0,
                    e[2] * 65535.0 / 256.0 - 128.0,
                )
            } else {
                lab_to_xyz(e[0] * 100.0, e[1] * 255.0 - 128.0, e[2] * 255.0 - 128.0)
            }
        }
    }
}

fn encode_pcs(xyz: [f32; 3], pcs: Pcs, v2: bool) -> [f32; 3] {
    match pcs {
        Pcs::Xyz => [xyz[0] / XYZ_SCALE, xyz[1] / XYZ_SCALE, xyz[2] / XYZ_SCALE],
        Pcs::Lab => {
            let l = xyz_to_lab(xyz);
            if v2 {
                [
                    l[0] / 100.0 * 65280.0 / 65535.0,
                    (l[1] + 128.0) * 256.0 / 65535.0,
                    (l[2] + 128.0) * 256.0 / 65535.0,
                ]
            } else {
                [l[0] / 100.0, (l[1] + 128.0) / 255.0, (l[2] + 128.0) / 255.0]
            }
        }
    }
}

/// `src -> PCS -> dst` for one adjacent profile pair.
#[derive(Debug)]
pub struct IccStage {
    src: Profile,
    dst: Profile,
    intent: i32,
}

impl IccStage {
    pub fn new(src: Profile, dst: Profile, intent: i32) -> Self {
        Self { src, dst, intent }
    }

    pub fn in_channels(&self) -> usize {
        self.src.channels
    }

    pub fn out_channels(&self) -> usize {
        self.dst.channels
    }

    /// Transform `src` (pixel-interleaved device samples in `[0, 1]`) into `dst`.
    pub fn transform(&self, src: &[f32], dst: &mut [f32]) {
        let (ic, oc) = (self.src.channels, self.dst.channels);
        for (i, o) in src.chunks_exact(ic).zip(dst.chunks_exact_mut(oc)) {
            let xyz = self.src.to_xyz(i, self.intent);
            let mut out = [0f32; 8];
            self.dst.from_xyz(xyz, &mut out, self.intent);
            for (slot, v) in o.iter_mut().zip(out.iter()) {
                *slot = v.clamp(0.0, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JDK's own profiles (`jimage extract --include 'regex:.*cmm/profiles.*'`
    /// into `$CRATONVM_ICC_PROFILE_DIR`); the test is skipped when they are absent.
    fn pf(name: &str) -> Option<Profile> {
        let dir = std::env::var_os("CRATONVM_ICC_PROFILE_DIR")?;
        let bytes = std::fs::read(std::path::Path::new(&dir).join(name)).ok()?;
        Some(Profile::parse(&bytes).unwrap_or_else(|| panic!("{name} does not parse")))
    }

    fn roundtrip(other: &str, tol: f32) {
        let (Some(srgb), Some(o)) = (pf("sRGB.pf"), pf(other)) else {
            return;
        };
        let fwd = IccStage::new(srgb.clone(), o.clone(), 0);
        let back = IccStage::new(o, srgb, 0);
        for px in [
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.5, 0.5, 0.5],
            [0.9, 0.2, 0.1],
            [0.1, 0.6, 0.3],
            [0.2, 0.3, 0.8],
        ] {
            let mut mid = vec![0f32; fwd.out_channels()];
            fwd.transform(&px, &mut mid);
            let mut out = [0f32; 3];
            back.transform(&mid, &mut out);
            for k in 0..3 {
                assert!(
                    (out[k] - px[k]).abs() <= tol,
                    "{other}: {px:?} -> {mid:?} -> {out:?}"
                );
            }
        }
    }

    #[test]
    fn srgb_through_ciexyz_comes_back() {
        roundtrip("CIEXYZ.pf", 0.02);
    }

    #[test]
    fn srgb_through_pycc_comes_back() {
        // PYCC.pf's own A2B and B2A tables disagree by ~9% at white and ~4% at black.
        roundtrip("PYCC.pf", 0.1);
    }

    #[test]
    fn srgb_white_is_d50_in_xyz() {
        let Some(srgb) = pf("sRGB.pf") else { return };
        let xyz = srgb.to_xyz(&[1.0, 1.0, 1.0], 0);
        for k in 0..3 {
            assert!((xyz[k] - D50[k]).abs() < 0.01, "{xyz:?}");
        }
    }
}
