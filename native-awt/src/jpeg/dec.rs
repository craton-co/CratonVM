// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JPEG decompressor reproducing IJG libjpeg 6b: marker parsing (`jdmarker`),
//! Huffman decoding (`jdhuff`, `jdphuff`), the islow inverse DCT (`jidctint`),
//! fancy upsampling (`jdsample`) and YCbCr conversion (`jdcolor`).
//!
//! The decoder is whole-image: every scan is entropy-decoded into coefficient
//! arrays, then the image is reconstructed. libjpeg's streaming context-row
//! machinery is invisible in its output, so the result is the same bytes.

use super::{HuffSpec, NATURAL_ORDER};

/// Where JPEG bytes come from.
pub trait ByteSource {
    /// The next byte, or `None` at the end of the stream.
    fn next_byte(&mut self) -> Option<u8>;
}

/// A byte slice source (tests and in-memory decoding).
pub struct SliceSource<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl ByteSource for SliceSource<'_> {
    fn next_byte(&mut self) -> Option<u8> {
        let b = self.data.get(self.pos).copied();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }
}

/// A decoding failure, with libjpeg's message text where it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JpegError {
    /// `JERR_*` with its formatted message.
    Msg(String),
    /// Ran out of data before the first scan was complete.
    Truncated,
}

fn err<T>(m: impl Into<String>) -> Result<T, JpegError> {
    Err(JpegError::Msg(m.into()))
}

/// One image component's static description.
#[derive(Clone, Debug)]
pub struct CompInfo {
    pub id: u8,
    pub h: usize,
    pub v: usize,
    pub tq: usize,
}

/// A scan's component selection.
#[derive(Clone, Debug)]
struct ScanComp {
    ci: usize,
    td: usize,
    ta: usize,
}

/// What `read_header` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderOutcome {
    /// Reached SOS of an image.
    Image,
    /// Reached EOI with no image (tables-only stream).
    TablesOnly,
}

/// Decoded Huffman table.
#[derive(Clone)]
struct DHuff {
    maxcode: [i32; 18],
    valoffset: [i32; 17],
    vals: Vec<u8>,
}

fn make_dhuff(h: &HuffSpec) -> DHuff {
    let mut huffsize = Vec::new();
    for l in 1..=16usize {
        for _ in 0..h.bits[l] {
            huffsize.push(l as u8);
        }
    }
    let mut code = 0i32;
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
    let mut maxcode = [-1i32; 18];
    let mut valoffset = [0i32; 17];
    let mut p = 0usize;
    for l in 1..=16usize {
        if h.bits[l] != 0 {
            valoffset[l] = p as i32 - huffcode[p];
            p += h.bits[l] as usize;
            maxcode[l] = huffcode[p - 1];
        } else {
            maxcode[l] = -1;
        }
    }
    maxcode[17] = 0xFFFFF;
    DHuff {
        maxcode,
        valoffset,
        vals: h.vals.clone(),
    }
}

struct Comp {
    info: CompInfo,
    width_in_blocks: usize,
    height_in_blocks: usize,
    downsampled_width: usize,
    downsampled_height: usize,
    /// `width_in_blocks * height_in_blocks` blocks, natural order.
    coefs: Vec<[i16; 64]>,
    /// Quantization table latched at the component's first scan.
    qt: Option<[u16; 64]>,
}

/// The decoder: construct, `read_header`, `decode_scans`, `reconstruct`.
pub struct Decoder {
    pub width: usize,
    pub height: usize,
    pub progressive: bool,
    pub saw_jfif: bool,
    pub saw_adobe: bool,
    pub adobe_transform: u8,
    pub restart_interval: usize,
    pub icc: Option<Vec<u8>>,
    pub precision: u8,
    comps: Vec<Comp>,
    qtables: [Option<[u16; 64]>; 4],
    dc_tabs: [Option<DHuff>; 4],
    ac_tabs: [Option<DHuff>; 4],
    scan: Vec<ScanComp>,
    ss: usize,
    se: usize,
    ah: u32,
    al: u32,
    have_frame: bool,
    icc_chunks: Vec<(u8, Vec<u8>)>,
    icc_total: u8,
    max_h: usize,
    max_v: usize,
    /// A marker byte read ahead of the entropy-coded data.
    unread_marker: Option<u8>,
    pub warnings: Vec<String>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            width: 0,
            height: 0,
            progressive: false,
            saw_jfif: false,
            saw_adobe: false,
            adobe_transform: 0,
            restart_interval: 0,
            icc: None,
            precision: 8,
            comps: Vec::new(),
            qtables: [None, None, None, None],
            dc_tabs: [None, None, None, None],
            ac_tabs: [None, None, None, None],
            scan: Vec::new(),
            ss: 0,
            se: 63,
            ah: 0,
            al: 0,
            have_frame: false,
            icc_chunks: Vec::new(),
            icc_total: 0,
            max_h: 1,
            max_v: 1,
            unread_marker: None,
            warnings: Vec::new(),
        }
    }

    /// Install abbreviated-stream tables (`JPEGImageReadParam`).
    pub fn set_tables(
        &mut self,
        q: [Option<[u16; 64]>; 4],
        dc: [Option<HuffSpec>; 4],
        ac: [Option<HuffSpec>; 4],
    ) {
        for i in 0..4 {
            if q[i].is_some() {
                self.qtables[i] = q[i];
            }
            if let Some(h) = &dc[i] {
                self.dc_tabs[i] = Some(make_dhuff(h));
            }
            if let Some(h) = &ac[i] {
                self.ac_tabs[i] = Some(make_dhuff(h));
            }
        }
    }

    pub fn num_components(&self) -> usize {
        self.comps.len()
    }

    pub fn comp_info(&self) -> Vec<CompInfo> {
        self.comps.iter().map(|c| c.info.clone()).collect()
    }

    /// `jpeg_color_space` per `default_decompress_parms`, as the JDK's
    /// `JPEG.JCS_*` code (1 gray, 2 RGB, 3 YCbCr, 4 CMYK, 5 YCC/YCCK, 0 unknown).
    pub fn jpeg_color_space(&self) -> i32 {
        match self.comps.len() {
            1 => 1,
            3 => {
                if self.saw_jfif {
                    3
                } else if self.saw_adobe {
                    match self.adobe_transform {
                        0 => 2,
                        _ => 3,
                    }
                } else {
                    let (a, b, c) = (
                        self.comps[0].info.id,
                        self.comps[1].info.id,
                        self.comps[2].info.id,
                    );
                    if a == 82 && b == 71 && c == 66 {
                        2
                    } else {
                        3
                    }
                }
            }
            4 => {
                if self.saw_adobe {
                    match self.adobe_transform {
                        0 => 4,
                        _ => 11,
                    }
                } else {
                    4
                }
            }
            _ => 0,
        }
    }

    // ---- marker layer ------------------------------------------------------

    fn read_u8<S: ByteSource>(&mut self, s: &mut S) -> Result<u8, JpegError> {
        s.next_byte().ok_or(JpegError::Truncated)
    }

    fn read_u16<S: ByteSource>(&mut self, s: &mut S) -> Result<usize, JpegError> {
        let a = self.read_u8(s)? as usize;
        let b = self.read_u8(s)? as usize;
        Ok((a << 8) | b)
    }

    fn skip<S: ByteSource>(&mut self, s: &mut S, n: usize) -> Result<(), JpegError> {
        for _ in 0..n {
            self.read_u8(s)?;
        }
        Ok(())
    }

    /// `next_marker`: skip to the next marker byte (non-FF, non-00).
    fn next_marker<S: ByteSource>(&mut self, s: &mut S) -> Result<u8, JpegError> {
        if let Some(m) = self.unread_marker.take() {
            return Ok(m);
        }
        loop {
            let mut c = self.read_u8(s)?;
            while c != 0xff {
                c = self.read_u8(s)?;
            }
            loop {
                c = self.read_u8(s)?;
                if c != 0xff {
                    break;
                }
            }
            if c != 0 {
                return Ok(c);
            }
        }
    }

    /// Read markers up to the first SOS (`Image`) or EOI (`TablesOnly`).
    /// The stream must start at SOI.
    pub fn read_header<S: ByteSource>(&mut self, s: &mut S) -> Result<HeaderOutcome, JpegError> {
        let c1 = self.read_u8(s)?;
        let c2 = self.read_u8(s)?;
        if c1 != 0xff || c2 != 0xd8 {
            return err(format!(
                "Not a JPEG file: starts with 0x{:02x} 0x{:02x}",
                c1, c2
            ));
        }
        // New image: forget per-image state but keep tables.
        self.have_frame = false;
        self.comps.clear();
        self.saw_jfif = false;
        self.saw_adobe = false;
        self.icc = None;
        self.icc_chunks.clear();
        self.restart_interval = 0;
        self.unread_marker = None;
        self.progressive = false;
        self.read_markers(s)
    }

    fn read_markers<S: ByteSource>(&mut self, s: &mut S) -> Result<HeaderOutcome, JpegError> {
        loop {
            let m = self.next_marker(s)?;
            match m {
                0xd8 => return err("Invalid JPEG file structure: two SOI markers"),
                0xc0 | 0xc1 => self.get_sof(s, false)?,
                0xc2 => self.get_sof(s, true)?,
                0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => {
                    return err(format!("Unsupported JPEG process: SOF type 0x{:02x}", m));
                }
                0xda => {
                    if !self.have_frame {
                        return err("Invalid JPEG file structure: SOS before SOF");
                    }
                    self.get_sos(s)?;
                    return Ok(HeaderOutcome::Image);
                }
                0xd9 => return Ok(HeaderOutcome::TablesOnly),
                0xc4 => self.get_dht(s)?,
                0xdb => self.get_dqt(s)?,
                0xdd => {
                    let len = self.read_u16(s)?;
                    if len != 4 {
                        return err("Bogus marker length");
                    }
                    self.restart_interval = self.read_u16(s)?;
                }
                0xe0..=0xef => self.get_app(s, m)?,
                0xfe | 0xdc | 0xde | 0xdf | 0xc8 | 0xcc => {
                    let len = self.read_u16(s)?;
                    self.skip(s, len.saturating_sub(2))?;
                }
                0x01 | 0xd0..=0xd7 => {}
                _ => return err(format!("Unsupported marker type 0x{:02x}", m)),
            }
        }
    }

    fn get_sof<S: ByteSource>(&mut self, s: &mut S, progressive: bool) -> Result<(), JpegError> {
        if self.have_frame {
            return err("Invalid JPEG file structure: two SOF markers");
        }
        let len = self.read_u16(s)?;
        let prec = self.read_u8(s)?;
        let h = self.read_u16(s)?;
        let w = self.read_u16(s)?;
        let n = self.read_u8(s)? as usize;
        if len != 8 + n * 3 {
            return err("Bogus marker length");
        }
        if h == 0 || w == 0 || n == 0 {
            return err("Empty JPEG image (DNL not supported)");
        }
        if prec != 8 {
            return err(format!("Unsupported JPEG data precision {}", prec));
        }
        if n > 4 {
            return err(format!("Too many color components: {}, max 4", n));
        }
        self.precision = prec;
        self.width = w;
        self.height = h;
        self.progressive = progressive;
        let mut infos = Vec::new();
        for _ in 0..n {
            let id = self.read_u8(s)?;
            let hv = self.read_u8(s)?;
            let tq = self.read_u8(s)? as usize;
            let (ch, cv) = ((hv >> 4) as usize, (hv & 15) as usize);
            if ch == 0 || ch > 4 || cv == 0 || cv > 4 || tq > 3 {
                return err("Bogus sampling factors");
            }
            infos.push(CompInfo {
                id,
                h: ch,
                v: cv,
                tq,
            });
        }
        self.max_h = infos.iter().map(|c| c.h).max().unwrap_or(1);
        self.max_v = infos.iter().map(|c| c.v).max().unwrap_or(1);
        let (max_h, max_v) = (self.max_h, self.max_v);
        self.comps = infos
            .into_iter()
            .map(|info| {
                let wib = (w * info.h).div_ceil(max_h * 8);
                let hib = (h * info.v).div_ceil(max_v * 8);
                let dw = (w * info.h).div_ceil(max_h);
                let dh = (h * info.v).div_ceil(max_v);
                Comp {
                    info,
                    width_in_blocks: wib,
                    height_in_blocks: hib,
                    downsampled_width: dw,
                    downsampled_height: dh,
                    coefs: Vec::new(),
                    qt: None,
                }
            })
            .collect();
        self.have_frame = true;
        Ok(())
    }

    fn get_sos<S: ByteSource>(&mut self, s: &mut S) -> Result<(), JpegError> {
        let len = self.read_u16(s)?;
        let n = self.read_u8(s)? as usize;
        if len != n * 2 + 6 || n == 0 || n > 4 {
            return err("Bogus marker length");
        }
        let mut scan = Vec::new();
        for _ in 0..n {
            let cc = self.read_u8(s)?;
            let t = self.read_u8(s)?;
            let ci = self
                .comps
                .iter()
                .position(|c| c.info.id == cc)
                .ok_or_else(|| JpegError::Msg(format!("Invalid component ID {} in SOS", cc)))?;
            scan.push(ScanComp {
                ci,
                td: (t >> 4) as usize & 3,
                ta: (t & 15) as usize & 3,
            });
        }
        self.ss = self.read_u8(s)? as usize;
        self.se = self.read_u8(s)? as usize;
        let a = self.read_u8(s)?;
        self.ah = (a >> 4) as u32;
        self.al = (a & 15) as u32;
        self.scan = scan;
        for sc in &self.scan {
            let c = &mut self.comps[sc.ci];
            if c.qt.is_none() {
                c.qt = self.qtables[c.info.tq];
            }
        }
        Ok(())
    }

    fn get_dht<S: ByteSource>(&mut self, s: &mut S) -> Result<(), JpegError> {
        let mut len = self.read_u16(s)? as isize - 2;
        while len > 16 {
            let index = self.read_u8(s)?;
            let mut bits = [0u8; 17];
            let mut count = 0usize;
            for l in 1..=16 {
                bits[l] = self.read_u8(s)?;
                count += bits[l] as usize;
            }
            len -= 1 + 16;
            if count > 256 || count as isize > len {
                return err("Bogus Huffman table definition");
            }
            let mut vals = Vec::with_capacity(count);
            for _ in 0..count {
                vals.push(self.read_u8(s)?);
            }
            len -= count as isize;
            let t = make_dhuff(&HuffSpec { bits, vals });
            let ac = index & 0x10 != 0;
            let idx = (index & 15) as usize;
            if idx >= 4 {
                return err(format!("Bogus DHT index {}", idx));
            }
            if ac {
                self.ac_tabs[idx] = Some(t);
            } else {
                self.dc_tabs[idx] = Some(t);
            }
        }
        if len != 0 {
            return err("Bogus marker length");
        }
        Ok(())
    }

    fn get_dqt<S: ByteSource>(&mut self, s: &mut S) -> Result<(), JpegError> {
        let mut len = self.read_u16(s)? as isize - 2;
        while len > 0 {
            let n = self.read_u8(s)?;
            let prec = n >> 4;
            let idx = (n & 15) as usize;
            if idx >= 4 {
                return err(format!("Bogus DQT index {}", idx));
            }
            let mut t = [0u16; 64];
            for i in 0..64 {
                let v = if prec != 0 {
                    self.read_u16(s)? as u16
                } else {
                    self.read_u8(s)? as u16
                };
                t[NATURAL_ORDER[i]] = v;
            }
            len -= 1 + 64 * if prec != 0 { 2 } else { 1 };
            self.qtables[idx] = Some(t);
        }
        if len != 0 {
            return err("Bogus marker length");
        }
        Ok(())
    }

    fn get_app<S: ByteSource>(&mut self, s: &mut S, m: u8) -> Result<(), JpegError> {
        let len = self.read_u16(s)?;
        let mut data = Vec::with_capacity(len.saturating_sub(2));
        for _ in 0..len.saturating_sub(2) {
            data.push(self.read_u8(s)?);
        }
        if m == 0xe0 && data.len() >= 14 && &data[..5] == b"JFIF\0" {
            self.saw_jfif = true;
        } else if m == 0xee && data.len() >= 12 && &data[..5] == b"Adobe" {
            self.saw_adobe = true;
            self.adobe_transform = data[11];
        } else if m == 0xe2 && data.len() > 14 && &data[..12] == b"ICC_PROFILE\0" {
            self.icc_total = data[13];
            self.icc_chunks.push((data[12], data[14..].to_vec()));
            if self.icc_chunks.len() as u8 == self.icc_total && self.icc_total > 0 {
                let mut chunks = self.icc_chunks.clone();
                chunks.sort_by_key(|c| c.0);
                self.icc = Some(chunks.into_iter().flat_map(|c| c.1).collect());
            }
        }
        Ok(())
    }

    // ---- entropy decoding ------------------------------------------------

    /// Decode every remaining scan into coefficient arrays, stopping after EOI.
    pub fn decode_scans<S: ByteSource>(&mut self, s: &mut S) -> Result<(), JpegError> {
        for c in self.comps.iter_mut() {
            if c.coefs.is_empty() {
                c.coefs = vec![[0i16; 64]; c.width_in_blocks * c.height_in_blocks];
            }
        }
        loop {
            self.decode_one_scan(s)?;
            loop {
                match self.next_marker(s) {
                    Ok(0xda) => {
                        self.get_sos(s)?;
                        break;
                    }
                    Ok(0xd9) => return Ok(()),
                    Ok(0xc4) => self.get_dht(s)?,
                    Ok(0xdb) => self.get_dqt(s)?,
                    Ok(0xdd) => {
                        self.read_u16(s)?;
                        self.restart_interval = self.read_u16(s)?;
                    }
                    Ok(0xe0..=0xef) | Ok(0xfe) => {
                        let len = self.read_u16(s)?;
                        self.skip(s, len.saturating_sub(2))?;
                    }
                    Ok(0xd0..=0xd7) | Ok(0x01) => {}
                    Ok(m) => return err(format!("Unsupported marker type 0x{:02x}", m)),
                    Err(JpegError::Truncated) => {
                        self.warnings.push("Premature end of JPEG file".into());
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    fn decode_one_scan<S: ByteSource>(&mut self, s: &mut S) -> Result<(), JpegError> {
        let scan = self.scan.clone();
        let (max_h, max_v) = (self.max_h, self.max_v);
        let interleaved = scan.len() > 1;
        let (mcus_x, mcus_y) = if interleaved {
            (
                self.width.div_ceil(max_h * 8),
                self.height.div_ceil(max_v * 8),
            )
        } else {
            let c = &self.comps[scan[0].ci];
            (c.width_in_blocks, c.height_in_blocks)
        };
        let mut br = BitReader {
            bits: 0,
            nbits: 0,
            marker: None,
            eof: false,
        };
        let mut last_dc = [0i32; 4];
        let mut eobrun = 0u32;
        let mut restarts_to_go = self.restart_interval;
        let (ss, se, ah, al) = (self.ss, self.se, self.ah, self.al);
        let progressive = self.progressive;
        let mut tmp = [0i16; 64];
        for my in 0..mcus_y {
            for mx in 0..mcus_x {
                if self.restart_interval > 0 {
                    if restarts_to_go == 0 {
                        br.restart(s, &mut self.unread_marker);
                        last_dc = [0; 4];
                        eobrun = 0;
                        restarts_to_go = self.restart_interval;
                    }
                    restarts_to_go -= 1;
                }
                for (si, sc) in scan.iter().enumerate() {
                    let (bw, bh) = if interleaved {
                        (self.comps[sc.ci].info.h, self.comps[sc.ci].info.v)
                    } else {
                        (1, 1)
                    };
                    let (wib, hib) = (
                        self.comps[sc.ci].width_in_blocks,
                        self.comps[sc.ci].height_in_blocks,
                    );
                    for yi in 0..bh {
                        for xi in 0..bw {
                            let bx = mx * bw + xi;
                            let by = my * bh + yi;
                            let real = bx < wib && by < hib;
                            let dc_tab = self.dc_tabs[sc.td].as_ref();
                            let ac_tab = self.ac_tabs[sc.ta].as_ref();
                            let block: &mut [i16; 64] = if real {
                                &mut self.comps[sc.ci].coefs[by * wib + bx]
                            } else {
                                tmp = [0; 64];
                                &mut tmp
                            };
                            let un = &mut self.unread_marker;
                            if !progressive {
                                let (Some(dc), Some(ac)) = (dc_tab, ac_tab) else {
                                    return err("Huffman table 0x00 was not defined");
                                };
                                decode_block_seq(&mut br, s, un, block, &mut last_dc[si], dc, ac);
                            } else if ss == 0 {
                                if ah == 0 {
                                    let Some(dc) = dc_tab else {
                                        return err("Huffman table 0x00 was not defined");
                                    };
                                    let sym = br.decode(s, un, dc);
                                    let mut d = 0i32;
                                    if sym != 0 {
                                        let r = br.get_bits(s, un, sym as u32) as i32;
                                        d = huff_extend(r, sym as i32);
                                    }
                                    last_dc[si] += d;
                                    block[0] = (last_dc[si] << al) as i16;
                                } else if br.get_bits(s, un, 1) != 0 {
                                    block[0] |= (1 << al) as i16;
                                }
                            } else if ah == 0 {
                                let Some(ac) = ac_tab else {
                                    return err("Huffman table 0x00 was not defined");
                                };
                                decode_ac_first(&mut br, s, un, block, ac, ss, se, al, &mut eobrun);
                            } else {
                                let Some(ac) = ac_tab else {
                                    return err("Huffman table 0x00 was not defined");
                                };
                                decode_ac_refine(
                                    &mut br,
                                    s,
                                    un,
                                    block,
                                    ac,
                                    ss,
                                    se,
                                    al,
                                    &mut eobrun,
                                );
                            }
                        }
                    }
                }
            }
        }
        if let Some(m) = br.marker.take() {
            self.unread_marker = Some(m);
        }
        Ok(())
    }

    // ---- reconstruction ----------------------------------------------------

    fn reconstruct_planes(&self) -> Vec<Plane> {
        let mut planes = Vec::new();
        for c in &self.comps {
            let w = c.width_in_blocks * 8;
            let h = c.height_in_blocks * 8;
            let mut data = vec![0u8; w * h];
            let qt = c.qt.or(self.qtables[c.info.tq]).unwrap_or([1u16; 64]);
            for by in 0..c.height_in_blocks {
                for bx in 0..c.width_in_blocks {
                    let blk = &c.coefs[by * c.width_in_blocks + bx];
                    let mut out = [0u8; 64];
                    idct_islow(blk, &qt, &mut out);
                    for r in 0..8 {
                        let o = (by * 8 + r) * w + bx * 8;
                        data[o..o + 8].copy_from_slice(&out[r * 8..r * 8 + 8]);
                    }
                }
            }
            planes.push(Plane {
                w,
                data,
                dw: c.downsampled_width,
                dh: c.downsampled_height,
            });
        }
        planes
    }

    /// Reconstruct the image: rows of `width * ncomp` interleaved bytes.
    pub fn reconstruct(&self, convert: ColorConvert, fancy: bool) -> Vec<Vec<u8>> {
        let planes = self.reconstruct_planes();
        let (w, h) = (self.width, self.height);
        let n = self.comps.len();
        let mut full: Vec<Vec<u8>> = Vec::new();
        for (ci, c) in self.comps.iter().enumerate() {
            full.push(upsample(
                &planes[ci],
                c.info.h,
                c.info.v,
                self.max_h,
                self.max_v,
                w,
                h,
                fancy,
            ));
        }
        let t = ycc_rgb_tables();
        let mut rows = Vec::with_capacity(h);
        for y in 0..h {
            let mut row = vec![0u8; w * n];
            match (convert, n) {
                (ColorConvert::YccToRgb, 3) => {
                    for x in 0..w {
                        let yy = full[0][y * w + x] as i32;
                        let cb = full[1][y * w + x] as usize;
                        let cr = full[2][y * w + x] as usize;
                        row[x * 3] = clamp(yy + t.cr_r[cr]);
                        row[x * 3 + 1] = clamp(yy + ((t.cb_g[cb] + t.cr_g[cr]) >> 16));
                        row[x * 3 + 2] = clamp(yy + t.cb_b[cb]);
                    }
                }
                (ColorConvert::YcckToCmyk, 4) => {
                    for x in 0..w {
                        let yy = full[0][y * w + x] as i32;
                        let cb = full[1][y * w + x] as usize;
                        let cr = full[2][y * w + x] as usize;
                        row[x * 4] = clamp(255 - (yy + t.cr_r[cr]));
                        row[x * 4 + 1] = clamp(255 - (yy + ((t.cb_g[cb] + t.cr_g[cr]) >> 16)));
                        row[x * 4 + 2] = clamp(255 - (yy + t.cb_b[cb]));
                        row[x * 4 + 3] = full[3][y * w + x];
                    }
                }
                _ => {
                    for x in 0..w {
                        for c in 0..n {
                            row[x * n + c] = full[c][y * w + x];
                        }
                    }
                }
            }
            rows.push(row);
        }
        rows
    }
}

/// The output colour conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorConvert {
    None,
    YccToRgb,
    YcckToCmyk,
}

fn clamp(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

struct YccTables {
    cr_r: [i32; 256],
    cb_b: [i32; 256],
    cr_g: [i32; 256],
    cb_g: [i32; 256],
}

fn ycc_rgb_tables() -> YccTables {
    let fix = |x: f64| (x * 65536.0 + 0.5) as i32;
    let one_half = 1 << 15;
    let mut t = YccTables {
        cr_r: [0; 256],
        cb_b: [0; 256],
        cr_g: [0; 256],
        cb_g: [0; 256],
    };
    for i in 0..256 {
        let x = i as i32 - 128;
        t.cr_r[i] = (fix(1.40200) * x + one_half) >> 16;
        t.cb_b[i] = (fix(1.77200) * x + one_half) >> 16;
        t.cr_g[i] = -fix(0.71414) * x;
        t.cb_g[i] = -fix(0.34414) * x + one_half;
    }
    t
}

struct Plane {
    /// Padded plane width (a whole number of blocks).
    w: usize,
    data: Vec<u8>,
    /// Valid downsampled dimensions.
    dw: usize,
    dh: usize,
}

#[allow(clippy::too_many_arguments)]
fn upsample(
    p: &Plane,
    h: usize,
    v: usize,
    max_h: usize,
    max_v: usize,
    w: usize,
    ht: usize,
    fancy: bool,
) -> Vec<u8> {
    let mut out = vec![0u8; w * ht];
    let at = |x: usize, y: usize| -> i32 { p.data[y * p.w + x] as i32 };
    if h == max_h && v == max_v {
        for y in 0..ht {
            out[y * w..(y + 1) * w].copy_from_slice(&p.data[y * p.w..y * p.w + w]);
        }
    } else if h * 2 == max_h && v == max_v {
        let dw = p.dw;
        for y in 0..ht {
            let mut row = vec![0u8; dw * 2];
            if fancy && dw > 2 {
                let iv = at(0, y);
                row[0] = iv as u8;
                row[1] = ((iv * 3 + at(1, y) + 2) >> 2) as u8;
                for x in 1..dw - 1 {
                    let inv = at(x, y) * 3;
                    row[2 * x] = ((inv + at(x - 1, y) + 1) >> 2) as u8;
                    row[2 * x + 1] = ((inv + at(x + 1, y) + 2) >> 2) as u8;
                }
                let iv = at(dw - 1, y);
                row[2 * (dw - 1)] = ((iv * 3 + at(dw - 2, y) + 1) >> 2) as u8;
                row[2 * (dw - 1) + 1] = iv as u8;
            } else {
                for x in 0..dw {
                    row[2 * x] = at(x, y) as u8;
                    row[2 * x + 1] = at(x, y) as u8;
                }
            }
            out[y * w..(y + 1) * w].copy_from_slice(&row[..w]);
        }
    } else if h * 2 == max_h && v * 2 == max_v {
        let dw = p.dw;
        let dh = p.dh;
        for oy in 0..ht {
            let iy = (oy / 2).min(dh - 1);
            let mut row = vec![0u8; dw * 2];
            if fancy && dw > 2 {
                let near = iy;
                let far = if oy % 2 == 0 {
                    iy.saturating_sub(1)
                } else {
                    (iy + 1).min(dh - 1)
                };
                let col = |x: usize| at(x, near) * 3 + at(x, far);
                let mut this = col(0);
                let mut next = col(1);
                row[0] = ((this * 4 + 8) >> 4) as u8;
                row[1] = ((this * 3 + next + 7) >> 4) as u8;
                let mut last = this;
                this = next;
                for x in 1..dw - 1 {
                    next = col(x + 1);
                    row[2 * x] = ((this * 3 + last + 8) >> 4) as u8;
                    row[2 * x + 1] = ((this * 3 + next + 7) >> 4) as u8;
                    last = this;
                    this = next;
                }
                row[2 * (dw - 1)] = ((this * 3 + last + 8) >> 4) as u8;
                row[2 * (dw - 1) + 1] = ((this * 4 + 7) >> 4) as u8;
            } else {
                for x in 0..dw {
                    row[2 * x] = at(x, iy) as u8;
                    row[2 * x + 1] = at(x, iy) as u8;
                }
            }
            out[oy * w..(oy + 1) * w].copy_from_slice(&row[..w]);
        }
    } else {
        // Integral box replication (`int_upsample`).
        let hx = (max_h / h.max(1)).max(1);
        let vx = (max_v / v.max(1)).max(1);
        for oy in 0..ht {
            for ox in 0..w {
                let ix = (ox / hx).min(p.dw - 1);
                let iy = (oy / vx).min(p.dh - 1);
                out[oy * w + ox] = p.data[iy * p.w + ix];
            }
        }
    }
    out
}

// ---- bit reader ------------------------------------------------------------

struct BitReader {
    bits: u32,
    nbits: i32,
    /// A marker encountered inside entropy data.
    marker: Option<u8>,
    eof: bool,
}

impl BitReader {
    fn fill<S: ByteSource>(&mut self, s: &mut S, unread: &mut Option<u8>, need: i32) {
        while self.nbits < need {
            let mut byte = 0u32;
            if self.marker.is_none() && !self.eof {
                match s.next_byte() {
                    None => self.eof = true,
                    Some(0xff) => loop {
                        match s.next_byte() {
                            None => {
                                self.eof = true;
                                break;
                            }
                            Some(0xff) => continue,
                            Some(0) => {
                                byte = 0xff;
                                break;
                            }
                            Some(m) => {
                                self.marker = Some(m);
                                *unread = Some(m);
                                break;
                            }
                        }
                    },
                    Some(b) => byte = b as u32,
                }
            }
            // Past a marker (or EOF) the stream supplies zero bits.
            self.bits = (self.bits << 8) | byte;
            self.nbits += 8;
        }
    }

    fn get_bits<S: ByteSource>(&mut self, s: &mut S, unread: &mut Option<u8>, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        self.fill(s, unread, n as i32);
        self.nbits -= n as i32;
        (self.bits >> self.nbits) & ((1u32 << n) - 1)
    }

    fn decode<S: ByteSource>(&mut self, s: &mut S, unread: &mut Option<u8>, t: &DHuff) -> u8 {
        let mut l = 1usize;
        let mut code = self.get_bits(s, unread, 1) as i32;
        while code > t.maxcode[l] {
            code = (code << 1) | self.get_bits(s, unread, 1) as i32;
            l += 1;
            if l > 16 {
                return 0;
            }
        }
        let idx = (code + t.valoffset[l]) as usize;
        t.vals.get(idx).copied().unwrap_or(0)
    }

    /// Process a restart marker: drop buffered bits and consume `RSTn`.
    fn restart<S: ByteSource>(&mut self, s: &mut S, unread: &mut Option<u8>) {
        self.bits = 0;
        self.nbits = 0;
        while self.marker.is_none() && !self.eof {
            match s.next_byte() {
                None => self.eof = true,
                Some(0xff) => loop {
                    match s.next_byte() {
                        Some(0xff) => continue,
                        Some(0) => break,
                        Some(m) => {
                            self.marker = Some(m);
                            break;
                        }
                        None => {
                            self.eof = true;
                            break;
                        }
                    }
                },
                Some(_) => {}
            }
        }
        if let Some(m) = self.marker {
            if (0xd0..=0xd7).contains(&m) {
                self.marker = None;
                *unread = None;
            }
        }
    }
}

fn huff_extend(x: i32, s: i32) -> i32 {
    if x < (1 << (s - 1)) {
        x + ((-1) << s) + 1
    } else {
        x
    }
}

fn decode_block_seq<S: ByteSource>(
    br: &mut BitReader,
    s: &mut S,
    unread: &mut Option<u8>,
    block: &mut [i16; 64],
    last_dc: &mut i32,
    dc: &DHuff,
    ac: &DHuff,
) {
    let sym = br.decode(s, unread, dc);
    let mut diff = 0;
    if sym != 0 {
        let r = br.get_bits(s, unread, sym as u32) as i32;
        diff = huff_extend(r, sym as i32);
    }
    *last_dc += diff;
    block[0] = *last_dc as i16;
    let mut k = 1;
    while k < 64 {
        let sym = br.decode(s, unread, ac);
        let r = (sym >> 4) as usize;
        let sz = (sym & 15) as u32;
        if sz != 0 {
            k += r;
            let bits = br.get_bits(s, unread, sz) as i32;
            if k < 64 {
                block[NATURAL_ORDER[k]] = huff_extend(bits, sz as i32) as i16;
            }
        } else {
            if r != 15 {
                break;
            }
            k += 15;
        }
        k += 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_ac_first<S: ByteSource>(
    br: &mut BitReader,
    s: &mut S,
    unread: &mut Option<u8>,
    block: &mut [i16; 64],
    ac: &DHuff,
    ss: usize,
    se: usize,
    al: u32,
    eobrun: &mut u32,
) {
    if *eobrun > 0 {
        *eobrun -= 1;
        return;
    }
    let mut k = ss;
    while k <= se {
        let sym = br.decode(s, unread, ac);
        let r = (sym >> 4) as u32;
        let sz = (sym & 15) as u32;
        if sz != 0 {
            k += r as usize;
            let bits = br.get_bits(s, unread, sz) as i32;
            if k < 64 {
                block[NATURAL_ORDER[k]] = (huff_extend(bits, sz as i32) << al) as i16;
            }
        } else if r == 15 {
            k += 15;
        } else {
            *eobrun = 1 << r;
            if r != 0 {
                *eobrun += br.get_bits(s, unread, r);
            }
            *eobrun -= 1;
            break;
        }
        k += 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_ac_refine<S: ByteSource>(
    br: &mut BitReader,
    s: &mut S,
    unread: &mut Option<u8>,
    block: &mut [i16; 64],
    ac: &DHuff,
    ss: usize,
    se: usize,
    al: u32,
    eobrun: &mut u32,
) {
    let p1: i32 = 1 << al;
    let m1: i32 = (-1i32) << al;
    let mut k = ss;
    if *eobrun == 0 {
        while k <= se {
            let sym = br.decode(s, unread, ac);
            let mut r = (sym >> 4) as i32;
            let sz = (sym & 15) as u32;
            let mut value = 0i32;
            if sz != 0 {
                value = if br.get_bits(s, unread, 1) != 0 {
                    p1
                } else {
                    m1
                };
            } else if r != 15 {
                *eobrun = 1 << r;
                if r != 0 {
                    *eobrun += br.get_bits(s, unread, r as u32);
                }
                break;
            }
            loop {
                let pos = NATURAL_ORDER[k];
                let cur = block[pos] as i32;
                if cur != 0 {
                    if br.get_bits(s, unread, 1) != 0 && (cur & p1) == 0 {
                        block[pos] = if cur >= 0 {
                            (cur + p1) as i16
                        } else {
                            (cur + m1) as i16
                        };
                    }
                } else {
                    r -= 1;
                    if r < 0 {
                        break;
                    }
                }
                k += 1;
                if k > se {
                    break;
                }
            }
            if value != 0 && k <= se {
                block[NATURAL_ORDER[k]] = value as i16;
            }
            k += 1;
        }
    }
    if *eobrun > 0 {
        while k <= se {
            let pos = NATURAL_ORDER[k];
            let cur = block[pos] as i32;
            if cur != 0 && br.get_bits(s, unread, 1) != 0 && (cur & p1) == 0 {
                block[pos] = if cur >= 0 {
                    (cur + p1) as i16
                } else {
                    (cur + m1) as i16
                };
            }
            k += 1;
        }
        *eobrun -= 1;
    }
}

// ---- inverse DCT ---------------------------------------------------------

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

/// `range_limit[x & RANGE_MASK]` of the post-IDCT table.
#[inline]
fn range_limit(v: i32) -> u8 {
    let u = (v & 1023) as u32;
    if u < 128 {
        (u + 128) as u8
    } else if u < 512 {
        255
    } else if u < 896 {
        0
    } else {
        (u - 896) as u8
    }
}

/// The odd part shared by both passes. Returns `(tmp0, tmp1, tmp2, tmp3)`.
#[inline]
fn idct_odd(t0: i32, t1: i32, t2: i32, t3: i32) -> (i32, i32, i32, i32) {
    let mut tmp0 = t0;
    let mut tmp1 = t1;
    let mut tmp2 = t2;
    let mut tmp3 = t3;
    let mut z1 = tmp0 + tmp3;
    let mut z2 = tmp1 + tmp2;
    let mut z3 = tmp0 + tmp2;
    let mut z4 = tmp1 + tmp3;
    let z5 = (z3 + z4) * FIX_1_175875602;
    tmp0 *= FIX_0_298631336;
    tmp1 *= FIX_2_053119869;
    tmp2 *= FIX_3_072711026;
    tmp3 *= FIX_1_501321110;
    z1 *= -FIX_0_899976223;
    z2 *= -FIX_2_562915447;
    z3 *= -FIX_1_961570560;
    z4 *= -FIX_0_390180644;
    z3 += z5;
    z4 += z5;
    (
        tmp0 + z1 + z3,
        tmp1 + z2 + z4,
        tmp2 + z2 + z3,
        tmp3 + z1 + z4,
    )
}

/// `jpeg_idct_islow`: dequantize and inverse-transform one block.
pub fn idct_islow(coef: &[i16; 64], q: &[u16; 64], out: &mut [u8; 64]) {
    let mut ws = [0i32; 64];
    for c in 0..8 {
        let dq = |r: usize| coef[r * 8 + c] as i32 * q[r * 8 + c] as i32;
        if (1..8).all(|r| coef[r * 8 + c] == 0) {
            let dc = dq(0) << PASS1_BITS;
            for r in 0..8 {
                ws[r * 8 + c] = dc;
            }
            continue;
        }
        let z2 = dq(2);
        let z3 = dq(6);
        let z1 = (z2 + z3) * FIX_0_541196100;
        let tmp2 = z1 + z3 * (-FIX_1_847759065);
        let tmp3 = z1 + z2 * FIX_0_765366865;
        let z2 = dq(0);
        let z3 = dq(4);
        let tmp0 = (z2 + z3) << CONST_BITS;
        let tmp1 = (z2 - z3) << CONST_BITS;
        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;
        let (o0, o1, o2, o3) = idct_odd(dq(7), dq(5), dq(3), dq(1));
        let s = CONST_BITS - PASS1_BITS;
        ws[c] = descale(tmp10 + o3, s);
        ws[56 + c] = descale(tmp10 - o3, s);
        ws[8 + c] = descale(tmp11 + o2, s);
        ws[48 + c] = descale(tmp11 - o2, s);
        ws[16 + c] = descale(tmp12 + o1, s);
        ws[40 + c] = descale(tmp12 - o1, s);
        ws[24 + c] = descale(tmp13 + o0, s);
        ws[32 + c] = descale(tmp13 - o0, s);
    }
    for r in 0..8 {
        let w = &ws[r * 8..r * 8 + 8];
        let o = &mut out[r * 8..r * 8 + 8];
        if w[1..8].iter().all(|&v| v == 0) {
            o.fill(range_limit(descale(w[0], PASS1_BITS + 3)));
            continue;
        }
        let z2 = w[2];
        let z3 = w[6];
        let z1 = (z2 + z3) * FIX_0_541196100;
        let tmp2 = z1 + z3 * (-FIX_1_847759065);
        let tmp3 = z1 + z2 * FIX_0_765366865;
        let tmp0 = (w[0] + w[4]) << CONST_BITS;
        let tmp1 = (w[0] - w[4]) << CONST_BITS;
        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;
        let (o0, o1, o2, o3) = idct_odd(w[7], w[5], w[3], w[1]);
        let s = CONST_BITS + PASS1_BITS + 3;
        o[0] = range_limit(descale(tmp10 + o3, s));
        o[7] = range_limit(descale(tmp10 - o3, s));
        o[1] = range_limit(descale(tmp11 + o2, s));
        o[6] = range_limit(descale(tmp11 - o2, s));
        o[2] = range_limit(descale(tmp12 + o1, s));
        o[5] = range_limit(descale(tmp12 - o1, s));
        o[3] = range_limit(descale(tmp13 + o0, s));
        o[4] = range_limit(descale(tmp13 - o0, s));
    }
}
