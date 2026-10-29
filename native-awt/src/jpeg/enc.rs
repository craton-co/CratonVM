// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Baseline JPEG compressor reproducing IJG libjpeg 6b (what the JDK links in
//! `libjavajpeg`): `jccolor`, `jcsample`, `jcprepct`, `jcdctmgr` (islow),
//! `jchuff` and `jcmarker`, byte for byte.

use super::{HuffSpec, NATURAL_ORDER};

/// One component of the frame.
#[derive(Clone, Debug)]
pub struct CompSpec {
    pub id: u8,
    pub h: usize,
    pub v: usize,
    /// Quantization table index (0..3).
    pub tq: usize,
    /// DC and AC Huffman table indices (0..3).
    pub td: usize,
    pub ta: usize,
}

/// Everything the compressor needs.
pub struct EncParams {
    pub width: usize,
    pub height: usize,
    /// 8-bit samples per pixel of the INPUT rows (1 or 3).
    pub in_comps: usize,
    /// Convert RGB input to YCbCr (otherwise components are passed through).
    pub rgb_to_ycc: bool,
    pub comps: Vec<CompSpec>,
    /// Quantization tables in natural (row-major) order; `None` = unused.
    pub qtables: [Option<[u16; 64]>; 4],
    pub dc_tables: [Option<HuffSpec>; 4],
    pub ac_tables: [Option<HuffSpec>; 4],
    pub write_dqt: bool,
    pub write_dht: bool,
    pub restart_interval: usize,
    /// Two-pass optimal Huffman tables (`cinfo->optimize_coding`): the
    /// supplied DC / AC tables are replaced by `jpeg_gen_optimal_table`'s.
    pub optimize: bool,
}

fn fix(x: f64) -> i32 {
    (x * 65536.0 + 0.5) as i32
}

struct YccTab(Vec<i32>);

fn ycc_tab() -> YccTab {
    const ONE_HALF: i32 = 1 << 15;
    const CBCR_OFFSET: i32 = 128 << 16;
    let mut t = vec![0i32; 8 * 256];
    for i in 0..256i32 {
        let iu = i as usize;
        t[iu] = fix(0.29900) * i;
        t[iu + 256] = fix(0.58700) * i;
        t[iu + 512] = fix(0.11400) * i + ONE_HALF;
        t[iu + 768] = -fix(0.16874) * i;
        t[iu + 1024] = -fix(0.33126) * i;
        t[iu + 1280] = fix(0.50000) * i + CBCR_OFFSET + ONE_HALF - 1;
        t[iu + 1536] = -fix(0.41869) * i;
        t[iu + 1792] = -fix(0.08131) * i;
    }
    YccTab(t)
}

const CONST_BITS: i32 = 13;
const PASS1_BITS: i32 = 2;
const FIX_0_298631336: i32 = 2446;
const FIX_0_390180644: i32 = 3196;
const FIX_0_541196100: i32 = 4433;
const FIX_0_765366865: i32 = 6270;
const FIX_0_899976223: i32 = 7373;
const FIX_1_175875602: i32 = 9633;
const FIX_1_501321110: i32 = 12299;
const FIX_1_847759065: i32 = 15137;
const FIX_1_961570560: i32 = 16069;
const FIX_2_053119869: i32 = 16819;
const FIX_2_562915447: i32 = 20995;
const FIX_3_072711026: i32 = 25172;

#[inline]
fn descale(x: i32, n: i32) -> i32 {
    (x + (1 << (n - 1))) >> n
}

/// `jpeg_fdct_islow`: in-place 8x8 forward DCT of level-shifted samples.
pub fn fdct_islow(d: &mut [i32; 64]) {
    for r in 0..8 {
        let p = r * 8;
        let tmp0 = d[p] + d[p + 7];
        let tmp7 = d[p] - d[p + 7];
        let tmp1 = d[p + 1] + d[p + 6];
        let tmp6 = d[p + 1] - d[p + 6];
        let tmp2 = d[p + 2] + d[p + 5];
        let tmp5 = d[p + 2] - d[p + 5];
        let tmp3 = d[p + 3] + d[p + 4];
        let tmp4 = d[p + 3] - d[p + 4];
        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;
        d[p] = (tmp10 + tmp11) << PASS1_BITS;
        d[p + 4] = (tmp10 - tmp11) << PASS1_BITS;
        let z1 = (tmp12 + tmp13) * FIX_0_541196100;
        d[p + 2] = descale(z1 + tmp13 * FIX_0_765366865, CONST_BITS - PASS1_BITS);
        d[p + 6] = descale(z1 + tmp12 * (-FIX_1_847759065), CONST_BITS - PASS1_BITS);
        let mut z1 = tmp4 + tmp7;
        let mut z2 = tmp5 + tmp6;
        let mut z3 = tmp4 + tmp6;
        let mut z4 = tmp5 + tmp7;
        let z5 = (z3 + z4) * FIX_1_175875602;
        let tmp4 = tmp4 * FIX_0_298631336;
        let tmp5 = tmp5 * FIX_2_053119869;
        let tmp6 = tmp6 * FIX_3_072711026;
        let tmp7 = tmp7 * FIX_1_501321110;
        z1 *= -FIX_0_899976223;
        z2 *= -FIX_2_562915447;
        z3 *= -FIX_1_961570560;
        z4 *= -FIX_0_390180644;
        z3 += z5;
        z4 += z5;
        d[p + 7] = descale(tmp4 + z1 + z3, CONST_BITS - PASS1_BITS);
        d[p + 5] = descale(tmp5 + z2 + z4, CONST_BITS - PASS1_BITS);
        d[p + 3] = descale(tmp6 + z2 + z3, CONST_BITS - PASS1_BITS);
        d[p + 1] = descale(tmp7 + z1 + z4, CONST_BITS - PASS1_BITS);
    }
    for c in 0..8 {
        let tmp0 = d[c] + d[56 + c];
        let tmp7 = d[c] - d[56 + c];
        let tmp1 = d[8 + c] + d[48 + c];
        let tmp6 = d[8 + c] - d[48 + c];
        let tmp2 = d[16 + c] + d[40 + c];
        let tmp5 = d[16 + c] - d[40 + c];
        let tmp3 = d[24 + c] + d[32 + c];
        let tmp4 = d[24 + c] - d[32 + c];
        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;
        d[c] = descale(tmp10 + tmp11, PASS1_BITS);
        d[32 + c] = descale(tmp10 - tmp11, PASS1_BITS);
        let z1 = (tmp12 + tmp13) * FIX_0_541196100;
        d[16 + c] = descale(z1 + tmp13 * FIX_0_765366865, CONST_BITS + PASS1_BITS);
        d[48 + c] = descale(z1 + tmp12 * (-FIX_1_847759065), CONST_BITS + PASS1_BITS);
        let mut z1 = tmp4 + tmp7;
        let mut z2 = tmp5 + tmp6;
        let mut z3 = tmp4 + tmp6;
        let mut z4 = tmp5 + tmp7;
        let z5 = (z3 + z4) * FIX_1_175875602;
        let tmp4 = tmp4 * FIX_0_298631336;
        let tmp5 = tmp5 * FIX_2_053119869;
        let tmp6 = tmp6 * FIX_3_072711026;
        let tmp7 = tmp7 * FIX_1_501321110;
        z1 *= -FIX_0_899976223;
        z2 *= -FIX_2_562915447;
        z3 *= -FIX_1_961570560;
        z4 *= -FIX_0_390180644;
        z3 += z5;
        z4 += z5;
        d[56 + c] = descale(tmp4 + z1 + z3, CONST_BITS + PASS1_BITS);
        d[40 + c] = descale(tmp5 + z2 + z4, CONST_BITS + PASS1_BITS);
        d[24 + c] = descale(tmp6 + z2 + z3, CONST_BITS + PASS1_BITS);
        d[8 + c] = descale(tmp7 + z1 + z4, CONST_BITS + PASS1_BITS);
    }
}

/// Quantize like `jcdctmgr.c` (`divisors = quantval << 3`).
fn quantize(d: &[i32; 64], q: &[u16; 64]) -> [i32; 64] {
    let mut out = [0i32; 64];
    for i in 0..64 {
        let qval = (q[i] as i32) << 3;
        let mut temp = d[i];
        if temp < 0 {
            temp = -temp;
            temp += qval >> 1;
            temp = if temp >= qval { temp / qval } else { 0 };
            temp = -temp;
        } else {
            temp += qval >> 1;
            temp = if temp >= qval { temp / qval } else { 0 };
        }
        out[i] = temp;
    }
    out
}

/// Derived Huffman encoding table (`jpeg_make_c_derived_tbl`).
struct Derived {
    code: [u32; 256],
    size: [u8; 256],
}

fn derive(h: &HuffSpec) -> Derived {
    let mut huffsize = Vec::new();
    for l in 1..=16usize {
        for _ in 0..h.bits[l] {
            huffsize.push(l as u8);
        }
    }
    let mut code = 0u32;
    let mut huffcode = Vec::with_capacity(huffsize.len());
    let mut si = huffsize.first().copied().unwrap_or(0);
    let mut p = 0;
    while p < huffsize.len() {
        while p < huffsize.len() && huffsize[p] == si {
            huffcode.push(code);
            code += 1;
            p += 1;
        }
        code <<= 1;
        si += 1;
    }
    let mut d = Derived {
        code: [0; 256],
        size: [0; 256],
    };
    for (i, &v) in h.vals.iter().enumerate().take(huffsize.len()) {
        d.code[v as usize] = huffcode[i];
        d.size[v as usize] = huffsize[i];
    }
    d
}

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    nbits: i32,
}

impl BitWriter {
    fn emit(&mut self, code: u32, size: i32) {
        if size == 0 {
            return;
        }
        let mut put_buffer = code & ((1u32 << size) - 1);
        let mut put_bits = self.nbits + size;
        put_buffer <<= 24 - put_bits;
        put_buffer |= self.acc;
        while put_bits >= 8 {
            let c = ((put_buffer >> 16) & 0xff) as u8;
            self.out.push(c);
            if c == 0xff {
                self.out.push(0);
            }
            put_buffer <<= 8;
            put_bits -= 8;
        }
        self.acc = put_buffer;
        self.nbits = put_bits;
    }

    fn flush(&mut self) {
        self.emit(0x7f, 7);
        self.acc = 0;
        self.nbits = 0;
    }
}

fn marker(out: &mut Vec<u8>, m: u8) {
    out.push(0xff);
    out.push(m);
}

fn put2(out: &mut Vec<u8>, v: usize) {
    out.push((v >> 8) as u8);
    out.push(v as u8);
}

/// The compressor.
pub struct Encoder {
    p: EncParams,
    max_h: usize,
    max_v: usize,
}

struct Plane {
    w: usize,
    data: Vec<u8>,
}

impl Encoder {
    pub fn new(p: EncParams) -> Self {
        let max_h = p.comps.iter().map(|c| c.h).max().unwrap_or(1);
        let max_v = p.comps.iter().map(|c| c.v).max().unwrap_or(1);
        Encoder { p, max_h, max_v }
    }

    /// `SOI`.
    pub fn soi(&self) -> Vec<u8> {
        vec![0xff, 0xd8]
    }

    /// DQT segments (if enabled) and SOF.
    pub fn frame_header(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut prec_any = false;
        if self.p.write_dqt {
            let mut sent = [false; 4];
            for c in &self.p.comps {
                if sent[c.tq] {
                    continue;
                }
                sent[c.tq] = true;
                if let Some(q) = &self.p.qtables[c.tq] {
                    let prec = q.iter().any(|&v| v > 255);
                    prec_any |= prec;
                    marker(&mut out, 0xdb);
                    put2(&mut out, if prec { 64 * 2 + 1 + 2 } else { 64 + 1 + 2 });
                    out.push((c.tq as u8) + if prec { 16 } else { 0 });
                    for i in 0..64 {
                        let v = q[NATURAL_ORDER[i]];
                        if prec {
                            out.push((v >> 8) as u8);
                        }
                        out.push(v as u8);
                    }
                }
            }
        } else {
            for c in &self.p.comps {
                if let Some(q) = &self.p.qtables[c.tq] {
                    prec_any |= q.iter().any(|&v| v > 255);
                }
            }
        }
        let mut baseline = true;
        for c in &self.p.comps {
            if c.td > 1 || c.ta > 1 {
                baseline = false;
            }
        }
        if prec_any {
            baseline = false;
        }
        marker(&mut out, if baseline { 0xc0 } else { 0xc1 });
        put2(&mut out, 3 * self.p.comps.len() + 2 + 5 + 1);
        out.push(8);
        put2(&mut out, self.p.height);
        put2(&mut out, self.p.width);
        out.push(self.p.comps.len() as u8);
        for c in &self.p.comps {
            out.push(c.id);
            out.push(((c.h << 4) | c.v) as u8);
            out.push(c.tq as u8);
        }
        out
    }

    /// DHT segments (if enabled), DRI and SOS, with the parameters' tables.
    pub fn scan_header(&self) -> Vec<u8> {
        self.scan_header_for(&self.p.dc_tables, &self.p.ac_tables)
    }

    fn scan_header_for(
        &self,
        dc_tables: &[Option<HuffSpec>; 4],
        ac_tables: &[Option<HuffSpec>; 4],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        if self.p.write_dht {
            let mut sent_dc = [false; 4];
            let mut sent_ac = [false; 4];
            for c in &self.p.comps {
                for (ac, idx) in [(false, c.td), (true, c.ta)] {
                    let sent = if ac { &mut sent_ac } else { &mut sent_dc };
                    if sent[idx] {
                        continue;
                    }
                    sent[idx] = true;
                    let spec = if ac { &ac_tables[idx] } else { &dc_tables[idx] };
                    if let Some(h) = spec {
                        let n: usize = h.vals.len();
                        marker(&mut out, 0xc4);
                        put2(&mut out, 2 + 1 + 16 + n);
                        out.push((idx as u8) + if ac { 0x10 } else { 0 });
                        for l in 1..=16 {
                            out.push(h.bits[l]);
                        }
                        out.extend_from_slice(&h.vals);
                    }
                }
            }
        }
        if self.p.restart_interval > 0 {
            marker(&mut out, 0xdd);
            put2(&mut out, 4);
            put2(&mut out, self.p.restart_interval);
        }
        marker(&mut out, 0xda);
        put2(&mut out, 2 * self.p.comps.len() + 2 + 1 + 3);
        out.push(self.p.comps.len() as u8);
        for c in &self.p.comps {
            out.push(c.id);
            out.push(((c.td << 4) | c.ta) as u8);
        }
        out.push(0);
        out.push(63);
        out.push(0);
        out
    }

    /// `EOI`.
    pub fn eoi(&self) -> Vec<u8> {
        vec![0xff, 0xd9]
    }

    /// Compress the whole image: `rows[y]` holds `width * in_comps` bytes.
    /// Returns the entropy-coded scan data (restart markers included).
    pub fn encode_scan(&self, rows: &[Vec<u8>]) -> Vec<u8> {
        self.encode(rows).1
    }

    /// Compress the whole image; returns `(scan header, entropy-coded data)`.
    /// The scan header carries the tables the data was coded with (optimal
    /// ones when `optimize` is set).
    pub fn encode(&self, rows: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>) {
        let p = &self.p;
        let (w, h) = (p.width, p.height);
        let nc = p.comps.len();
        // Colour-convert into full-resolution planes, padded to a whole row group.
        let padded_h = h.div_ceil(self.max_v) * self.max_v;
        let mut full: Vec<Plane> = (0..nc)
            .map(|_| Plane {
                w,
                data: vec![0; w * padded_h],
            })
            .collect();
        let tab = ycc_tab().0;
        for y in 0..padded_h {
            let src = &rows[y.min(h - 1)];
            for x in 0..w {
                if p.rgb_to_ycc && nc == 3 {
                    let r = src[x * 3] as usize;
                    let g = src[x * 3 + 1] as usize;
                    let b = src[x * 3 + 2] as usize;
                    full[0].data[y * w + x] = ((tab[r] + tab[g + 256] + tab[b + 512]) >> 16) as u8;
                    full[1].data[y * w + x] =
                        ((tab[r + 768] + tab[g + 1024] + tab[b + 1280]) >> 16) as u8;
                    full[2].data[y * w + x] =
                        ((tab[r + 1280] + tab[g + 1536] + tab[b + 1792]) >> 16) as u8;
                } else {
                    for c in 0..nc {
                        full[c].data[y * w + x] = src[x * p.in_comps + c.min(p.in_comps - 1)];
                    }
                }
            }
        }
        // Per-component geometry and downsampling.
        let mcu_w = self.max_h * 8;
        let mcu_h = self.max_v * 8;
        let mcus_x = w.div_ceil(mcu_w);
        let mcus_y = h.div_ceil(mcu_h);
        let mut planes: Vec<Plane> = Vec::new();
        let mut wib = Vec::new();
        let mut hib = Vec::new();
        for (ci, c) in p.comps.iter().enumerate() {
            let width_in_blocks = (w * c.h).div_ceil(self.max_h * 8);
            let height_in_blocks = (h * c.v).div_ceil(self.max_v * 8);
            wib.push(width_in_blocks);
            hib.push(height_in_blocks);
            let out_cols = width_in_blocks * 8;
            let rows_valid = (padded_h / self.max_v) * c.v;
            let rows_total = (mcus_y * c.v * 8).max(rows_valid);
            let he = self.max_h / c.h;
            let ve = self.max_v / c.v;
            let src = &full[ci];
            let mut out = vec![0u8; out_cols * rows_total];
            let at = |x: usize, y: usize| -> u32 {
                // Right-edge replication of the full-resolution row.
                src.data[y * w + x.min(w - 1)] as u32
            };
            for orow in 0..rows_valid {
                for ocol in 0..out_cols {
                    let v = if he == 1 && ve == 1 {
                        at(ocol, orow) as u8
                    } else if he == 2 && ve == 1 {
                        let bias = (ocol & 1) as u32;
                        ((at(ocol * 2, orow) + at(ocol * 2 + 1, orow) + bias) >> 1) as u8
                    } else if he == 2 && ve == 2 {
                        let bias = if ocol & 1 == 0 { 1 } else { 2 };
                        let (r0, r1) = (orow * 2, orow * 2 + 1);
                        ((at(ocol * 2, r0)
                            + at(ocol * 2 + 1, r0)
                            + at(ocol * 2, r1)
                            + at(ocol * 2 + 1, r1)
                            + bias)
                            >> 2) as u8
                    } else {
                        let numpix = (he * ve) as u32;
                        let mut sum = 0u32;
                        for dv in 0..ve {
                            for dh in 0..he {
                                sum += at(ocol * he + dh, orow * ve + dv);
                            }
                        }
                        ((sum + numpix / 2) / numpix) as u8
                    };
                    out[orow * out_cols + ocol] = v;
                }
            }
            // Bottom replication of the last valid downsampled row.
            for orow in rows_valid..rows_total {
                let (a, b) = out.split_at_mut(orow * out_cols);
                b[..out_cols]
                    .copy_from_slice(&a[(rows_valid - 1) * out_cols..rows_valid * out_cols]);
            }
            planes.push(Plane {
                w: out_cols,
                data: out,
            });
        }
        let interleaved = nc > 1;
        let (mcu_cols, mcu_rows) = if interleaved {
            (mcus_x, mcus_y)
        } else {
            (wib[0], hib[0])
        };
        // The MCU walk, shared by the statistics pass and the coding pass.
        let walk = |on_block: &mut dyn FnMut(usize, &[i32; 64]), on_restart: &mut dyn FnMut(u8)| {
            let mut restarts_to_go = p.restart_interval;
            let mut next_restart = 0u8;
            let zero_block = [0i32; 64];
            for my in 0..mcu_rows {
                for mx in 0..mcu_cols {
                    if p.restart_interval > 0 && restarts_to_go == 0 {
                        on_restart(next_restart);
                        next_restart = (next_restart + 1) & 7;
                        restarts_to_go = p.restart_interval;
                    }
                    for (ci, c) in p.comps.iter().enumerate() {
                        let (bw_n, bh_n) = if interleaved { (c.h, c.v) } else { (1, 1) };
                        let last_col_width = {
                            let m = wib[ci] % bw_n;
                            if m == 0 {
                                bw_n
                            } else {
                                m
                            }
                        };
                        let last_row_height = {
                            let m = hib[ci] % bh_n;
                            if m == 0 {
                                bh_n
                            } else {
                                m
                            }
                        };
                        let last_mcu_col = mcu_cols - 1;
                        let last_mcu_row = mcu_rows - 1;
                        let blockcnt = if mx < last_mcu_col {
                            bw_n
                        } else {
                            last_col_width
                        };
                        let mut prev_dc_in_mcu: Option<i32> = None;
                        for yi in 0..bh_n {
                            let mut row_blocks: Vec<[i32; 64]> = Vec::new();
                            if my < last_mcu_row || yi < last_row_height {
                                for xi in 0..bw_n {
                                    if xi < blockcnt {
                                        let bx = mx * bw_n + xi;
                                        let by = my * bh_n + yi;
                                        let q =
                                            p.qtables[c.tq].as_ref().expect("quantization table");
                                        let mut blk = [0i32; 64];
                                        let pl = &planes[ci];
                                        for r in 0..8 {
                                            for cc in 0..8 {
                                                blk[r * 8 + cc] = pl.data
                                                    [(by * 8 + r) * pl.w + bx * 8 + cc]
                                                    as i32
                                                    - 128;
                                            }
                                        }
                                        fdct_islow(&mut blk);
                                        row_blocks.push(quantize(&blk, q));
                                    } else {
                                        let mut dummy = zero_block;
                                        dummy[0] = row_blocks.last().map(|b| b[0]).unwrap_or(0);
                                        row_blocks.push(dummy);
                                    }
                                }
                            } else {
                                for _ in 0..bw_n {
                                    let mut dummy = zero_block;
                                    dummy[0] = match row_blocks.last() {
                                        Some(b) => b[0],
                                        None => prev_dc_in_mcu.unwrap_or(0),
                                    };
                                    row_blocks.push(dummy);
                                }
                            }
                            prev_dc_in_mcu = row_blocks.last().map(|b| b[0]);
                            for blk in &row_blocks {
                                on_block(ci, blk);
                            }
                        }
                    }
                    if p.restart_interval > 0 {
                        restarts_to_go -= 1;
                    }
                }
            }
        };
        // Tables: the supplied ones, or the optimal ones from a statistics pass.
        let (dc_specs, ac_specs) = if p.optimize {
            let dc_freq = std::cell::RefCell::new(vec![[0u32; 257]; 4]);
            let ac_freq = std::cell::RefCell::new(vec![[0u32; 257]; 4]);
            let last = std::cell::RefCell::new(vec![0i32; nc]);
            let comps = &p.comps;
            walk(
                &mut |ci, blk| {
                    gather_block(
                        blk,
                        &mut last.borrow_mut()[ci],
                        &mut dc_freq.borrow_mut()[comps[ci].td],
                        &mut ac_freq.borrow_mut()[comps[ci].ta],
                    );
                },
                &mut |_| {
                    for v in last.borrow_mut().iter_mut() {
                        *v = 0;
                    }
                },
            );
            let (dc_freq, ac_freq) = (dc_freq.into_inner(), ac_freq.into_inner());
            let mut dcs: [Option<HuffSpec>; 4] = [None, None, None, None];
            let mut acs: [Option<HuffSpec>; 4] = [None, None, None, None];
            for c in &p.comps {
                if dcs[c.td].is_none() {
                    dcs[c.td] = Some(gen_optimal(&dc_freq[c.td]));
                }
                if acs[c.ta].is_none() {
                    acs[c.ta] = Some(gen_optimal(&ac_freq[c.ta]));
                }
            }
            (dcs, acs)
        } else {
            (p.dc_tables.clone(), p.ac_tables.clone())
        };
        let dc: Vec<Option<Derived>> = (0..4).map(|i| dc_specs[i].as_ref().map(derive)).collect();
        let ac: Vec<Option<Derived>> = (0..4).map(|i| ac_specs[i].as_ref().map(derive)).collect();
        let bw = std::cell::RefCell::new(BitWriter {
            out: Vec::new(),
            acc: 0,
            nbits: 0,
        });
        let last_dc = std::cell::RefCell::new(vec![0i32; nc]);
        walk(
            &mut |ci, blk| {
                let c = &p.comps[ci];
                encode_block(
                    &mut bw.borrow_mut(),
                    blk,
                    &mut last_dc.borrow_mut()[ci],
                    dc[c.td].as_ref().expect("DC table"),
                    ac[c.ta].as_ref().expect("AC table"),
                );
            },
            &mut |num| {
                let mut b = bw.borrow_mut();
                b.flush();
                b.out.push(0xff);
                b.out.push(0xd0 + num);
                for v in last_dc.borrow_mut().iter_mut() {
                    *v = 0;
                }
            },
        );
        let mut bw = bw.into_inner();
        bw.flush();
        (self.scan_header_for(&dc_specs, &ac_specs), bw.out)
    }
}

/// Count the DC / AC symbols of one block (`htest_one_block`).
fn gather_block(
    block: &[i32; 64],
    last_dc: &mut i32,
    dc_freq: &mut [u32; 257],
    ac_freq: &mut [u32; 257],
) {
    let mut temp = block[0] - *last_dc;
    *last_dc = block[0];
    if temp < 0 {
        temp = -temp;
    }
    let mut nbits = 0usize;
    while temp != 0 {
        nbits += 1;
        temp >>= 1;
    }
    dc_freq[nbits] += 1;
    let mut r = 0usize;
    for k in 1..64 {
        let mut t = block[NATURAL_ORDER[k]];
        if t == 0 {
            r += 1;
        } else {
            while r > 15 {
                ac_freq[0xf0] += 1;
                r -= 16;
            }
            if t < 0 {
                t = -t;
            }
            let mut nb = 1usize;
            loop {
                t >>= 1;
                if t == 0 {
                    break;
                }
                nb += 1;
            }
            ac_freq[(r << 4) + nb] += 1;
            r = 0;
        }
    }
    if r > 0 {
        ac_freq[0] += 1;
    }
}

/// `jpeg_gen_optimal_table`: the optimal length-limited Huffman table for the
/// symbol frequencies (symbol 256 is the pseudo-symbol that keeps the all-ones
/// code unused).
fn gen_optimal(freq_in: &[u32; 257]) -> HuffSpec {
    let mut freq = [0i64; 257];
    for i in 0..257 {
        freq[i] = freq_in[i] as i64;
    }
    freq[256] = 1;
    let mut codesize = [0usize; 257];
    let mut others = [-1i32; 257];
    loop {
        let mut c1: i32 = -1;
        let mut v = 1_000_000_000i64;
        for i in 0..=256usize {
            if freq[i] != 0 && freq[i] <= v {
                v = freq[i];
                c1 = i as i32;
            }
        }
        let mut c2: i32 = -1;
        v = 1_000_000_000;
        for i in 0..=256usize {
            if freq[i] != 0 && freq[i] <= v && i as i32 != c1 {
                v = freq[i];
                c2 = i as i32;
            }
        }
        if c2 < 0 {
            break;
        }
        freq[c1 as usize] += freq[c2 as usize];
        freq[c2 as usize] = 0;
        let mut c = c1 as usize;
        codesize[c] += 1;
        while others[c] >= 0 {
            c = others[c] as usize;
            codesize[c] += 1;
        }
        others[c] = c2;
        let mut c = c2 as usize;
        codesize[c] += 1;
        while others[c] >= 0 {
            c = others[c] as usize;
            codesize[c] += 1;
        }
    }
    let mut bits = [0i32; 33];
    for i in 0..=256usize {
        if codesize[i] != 0 {
            bits[codesize[i].min(32)] += 1;
        }
    }
    let mut i = 32usize;
    while i > 16 {
        while bits[i] > 0 {
            let mut j = i - 2;
            while bits[j] == 0 {
                j -= 1;
            }
            bits[i] -= 2;
            bits[i - 1] += 1;
            bits[j + 1] += 2;
            bits[j] -= 1;
        }
        i -= 1;
    }
    let mut i = 16usize;
    while bits[i] == 0 {
        i -= 1;
    }
    bits[i] -= 1;
    let mut spec = HuffSpec {
        bits: [0; 17],
        vals: Vec::new(),
    };
    for l in 1..=16 {
        spec.bits[l] = bits[l] as u8;
    }
    for len in 1..=32usize {
        for j in 0..=255usize {
            if codesize[j] == len {
                spec.vals.push(j as u8);
            }
        }
    }
    spec
}

fn encode_block(
    bw: &mut BitWriter,
    block: &[i32; 64],
    last_dc: &mut i32,
    dc: &Derived,
    ac: &Derived,
) {
    let mut temp = block[0] - *last_dc;
    let mut temp2 = temp;
    if temp < 0 {
        temp = -temp;
        temp2 -= 1;
    }
    *last_dc = block[0];
    let mut nbits = 0;
    while temp != 0 {
        nbits += 1;
        temp >>= 1;
    }
    bw.emit(dc.code[nbits as usize], dc.size[nbits as usize] as i32);
    if nbits != 0 {
        bw.emit(temp2 as u32, nbits);
    }
    let mut r = 0;
    for k in 1..64 {
        let mut t = block[NATURAL_ORDER[k]];
        if t == 0 {
            r += 1;
        } else {
            while r > 15 {
                bw.emit(ac.code[0xf0], ac.size[0xf0] as i32);
                r -= 16;
            }
            let mut t2 = t;
            if t < 0 {
                t = -t;
                t2 -= 1;
            }
            let mut nb = 1;
            loop {
                t >>= 1;
                if t == 0 {
                    break;
                }
                nb += 1;
            }
            let i = ((r << 4) + nb) as usize;
            bw.emit(ac.code[i], ac.size[i] as i32);
            bw.emit(t2 as u32, nb);
            r = 0;
        }
    }
    if r > 0 {
        bw.emit(ac.code[0], ac.size[0] as i32);
    }
}
