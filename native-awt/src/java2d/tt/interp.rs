// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The TrueType bytecode interpreter, transcribed from FreeType's
//! `ttinterp.c` (the 2.14 sources the JDK bundles) for the mode the JDK uses:
//! `FT_LOAD_TARGET_MONO`, i.e. the v40 interpreter with backward compatibility
//! and the lean-subpixel hacks off, which is the classic full-hinting behaviour.
//! Every rounding, projection and fix-up follows the C code so hinted outlines
//! come out point for point the same.

use super::fixed::*;
use super::font::TtFont;

pub const TAG_ON: u8 = 1;
pub const TOUCH_X: u8 = 8;
pub const TOUCH_Y: u8 = 16;
pub const TOUCH_BOTH: u8 = 24;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct V {
    pub x: i64,
    pub y: i64,
}

#[derive(Clone, Default, Debug)]
pub struct Zone {
    pub org: Vec<V>,
    pub cur: Vec<V>,
    pub orus: Vec<V>,
    pub tags: Vec<u8>,
    /// End point index of each contour, relative to the zone.
    pub contours: Vec<u16>,
}

impl Zone {
    pub fn n_points(&self) -> usize {
        self.cur.len()
    }
    pub fn with_points(n: usize) -> Zone {
        Zone {
            org: vec![V::default(); n],
            cur: vec![V::default(); n],
            orus: vec![V::default(); n],
            tags: vec![0; n],
            contours: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Gs {
    pub rp0: u16,
    pub rp1: u16,
    pub rp2: u16,
    pub dual: V,
    pub proj: V,
    pub free: V,
    pub loop_: i64,
    pub min_dist: i64,
    pub round_state: i32,
    pub auto_flip: bool,
    pub cvt_cutin: i64,
    pub swc_cutin: i64,
    pub sw_value: i64,
    pub delta_base: u16,
    pub delta_shift: u16,
    pub instruct_control: u8,
    pub scan_control: bool,
    pub scan_type: i32,
    pub gep0: u16,
    pub gep1: u16,
    pub gep2: u16,
}

impl Default for Gs {
    fn default() -> Gs {
        Gs {
            rp0: 0,
            rp1: 0,
            rp2: 0,
            dual: V { x: 0x4000, y: 0 },
            proj: V { x: 0x4000, y: 0 },
            free: V { x: 0x4000, y: 0 },
            loop_: 1,
            min_dist: 64,
            round_state: 1,
            auto_flip: true,
            cvt_cutin: 68,
            swc_cutin: 0,
            sw_value: 0,
            delta_base: 9,
            delta_shift: 3,
            instruct_control: 0,
            scan_control: false,
            scan_type: 0,
            gep0: 1,
            gep1: 1,
            gep2: 1,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Def {
    pub range: usize,
    pub opc: u32,
    pub start: i64,
    pub end: i64,
    pub active: bool,
}

#[derive(Clone, Debug)]
struct CallRec {
    caller_range: usize,
    caller_ip: i64,
    cur_count: i64,
    def: Def,
}

/// Code ranges: 1 = `fpgm`, 2 = `prep`, 3 = glyph program.
pub const RANGE_FONT: usize = 1;
pub const RANGE_CVT: usize = 2;
pub const RANGE_GLYPH: usize = 3;

const ROUND_HALF: i32 = 0;
const ROUND_GRID: i32 = 1;
const ROUND_DOUBLE: i32 = 2;
const ROUND_DOWN: i32 = 3;
const ROUND_UP: i32 = 4;
const ROUND_OFF: i32 = 5;
const ROUND_SUPER: i32 = 6;
const ROUND_SUPER45: i32 = 7;

pub struct Exec {
    pub gs: Gs,
    pub zones: [Zone; 2],
    pub stack: Vec<i64>,
    pub top: usize,
    args: usize,
    new_top: usize,
    codes: [Vec<u8>; 4],
    cur_range: usize,
    ini_range: usize,
    ip: i64,
    opcode: u8,
    length: i64,
    step_ins: bool,
    err: bool,
    call_stack: Vec<CallRec>,
    pub fdefs: Vec<Def>,
    pub idefs: Vec<Def>,
    pub max_fdefs: usize,
    pub max_idefs: usize,
    max_func: u32,
    pub cvt: Vec<i64>,
    glyf_cvt: Option<Vec<i64>>,
    pub storage: Vec<i64>,
    glyf_storage: Option<Vec<i64>>,
    pub x_scale: i64,
    pub y_scale: i64,
    pub tt_scale: i64,
    pub ppem: i64,
    pub point_size: i64,
    period: i64,
    phase: i64,
    threshold: i64,
    /// 16.16 factors turning a distance along the projection vector into a move
    /// along the freedom vector (FreeType 2.14 precomputes them in `Compute_Funcs`).
    mv_x: i64,
    mv_y: i64,
    /// 0 general, 1 pure X, 2 pure Y.
    mv_mode: u8,
    iupx_called: bool,
    iupy_called: bool,
    is_composite: bool,
    neg_jump_counter: u64,
    loopcall_counter: u64,
    max_ins: u64,
    /// Print every executed instruction (debug aid, `TT_TRACE=1` in the dump test).
    pub trace: bool,
}

const MAX_RUNNABLE_OPCODES: u64 = 1_000_000;

fn pop_push(op: u8) -> (usize, usize) {
    match op {
        0x00..=0x05 => (0, 0),
        0x06..=0x09 => (2, 0),
        0x0A | 0x0B => (2, 0),
        0x0C | 0x0D => (0, 2),
        0x0E => (0, 0),
        0x0F => (5, 0),
        0x10..=0x17 => (1, 0),
        0x18 | 0x19 => (0, 0),
        0x1A => (1, 0),
        0x1B => (0, 0),
        0x1C..=0x1F => (1, 0),
        0x20 => (1, 2),
        0x21 => (1, 0),
        0x22 => (0, 0),
        0x23 => (2, 2),
        0x24 => (0, 1),
        0x25 => (1, 1),
        0x26 => (1, 0),
        0x27 => (2, 0),
        0x28 => (0, 0),
        0x29 => (1, 0),
        0x2A => (2, 0),
        0x2B => (1, 0),
        0x2C => (1, 0),
        0x2D => (0, 0),
        0x2E | 0x2F => (1, 0),
        0x30..=0x33 => (0, 0),
        0x34..=0x37 => (1, 0),
        0x38 => (1, 0),
        0x39 => (0, 0),
        0x3A | 0x3B => (2, 0),
        0x3C | 0x3D => (0, 0),
        0x3E | 0x3F => (2, 0),
        0x40 | 0x41 => (0, 0),
        0x42 => (2, 0),
        0x43 => (1, 1),
        0x44 => (2, 0),
        0x45 => (1, 1),
        0x46 | 0x47 => (1, 1),
        0x48 => (2, 0),
        0x49 | 0x4A => (2, 1),
        0x4B | 0x4C => (0, 1),
        0x4D | 0x4E => (0, 0),
        0x4F => (1, 0),
        0x50..=0x55 => (2, 1),
        0x56 | 0x57 => (1, 1),
        0x58 => (1, 0),
        0x59 => (0, 0),
        0x5A | 0x5B => (2, 1),
        0x5C => (1, 1),
        0x5D => (1, 0),
        0x5E | 0x5F => (1, 0),
        0x60..=0x63 => (2, 1),
        0x64..=0x67 => (1, 1),
        0x68..=0x6F => (1, 1),
        0x70 => (2, 0),
        0x71..=0x77 => (1, 0),
        0x78 | 0x79 => (2, 0),
        0x7A => (0, 0),
        0x7B => (0, 0),
        0x7C | 0x7D => (0, 0),
        0x7E | 0x7F => (1, 0),
        0x80 => (0, 0),
        0x81 | 0x82 => (2, 0),
        0x85 => (1, 0),
        0x86 | 0x87 => (2, 0),
        0x88 => (1, 1),
        0x89 => (1, 0),
        0x8A => (3, 3),
        0x8B | 0x8C => (2, 1),
        0x8D => (1, 0),
        0x8E => (2, 0),
        0xB0..=0xB7 => (0, (op - 0xB0) as usize + 1),
        0xB8..=0xBF => (0, (op - 0xB8) as usize + 1),
        0xC0..=0xDF => (1, 0),
        0xE0..=0xFF => (2, 0),
        _ => (0, 0),
    }
}

fn opcode_length(op: u8) -> i64 {
    match op {
        0x40 => -1,
        0x41 => -2,
        0xB0..=0xB7 => (op - 0xB0) as i64 + 2,
        0xB8..=0xBF => (op - 0xB8) as i64 * 2 + 3,
        _ => 1,
    }
}

impl Exec {
    pub fn new(stack_size: usize) -> Exec {
        Exec {
            gs: Gs::default(),
            zones: [Zone::default(), Zone::default()],
            stack: vec![0; stack_size],
            top: 0,
            args: 0,
            new_top: 0,
            codes: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            cur_range: 0,
            ini_range: 0,
            ip: 0,
            opcode: 0,
            length: 1,
            step_ins: true,
            err: false,
            call_stack: Vec::new(),
            fdefs: Vec::new(),
            idefs: Vec::new(),
            max_fdefs: 0,
            max_idefs: 0,
            max_func: 0,
            cvt: Vec::new(),
            glyf_cvt: None,
            storage: Vec::new(),
            glyf_storage: None,
            x_scale: 0,
            y_scale: 0,
            tt_scale: 0,
            ppem: 0,
            point_size: 0,
            period: 64,
            phase: 0,
            threshold: 0,
            mv_x: 0x10000,
            mv_y: 0,
            mv_mode: 1,
            iupx_called: false,
            iupy_called: false,
            is_composite: false,
            neg_jump_counter: 0,
            loopcall_counter: 0,
            max_ins: 0,
            trace: false,
        }
    }

    pub fn set_code(&mut self, range: usize, code: Vec<u8>) {
        self.codes[range] = code;
    }

    /// `TT_Run_Context` for `range`: starts at instruction 0.
    pub fn run(&mut self, range: usize) -> bool {
        self.cur_range = range;
        self.ini_range = range;
        self.ip = 0;
        self.top = 0;
        self.call_stack.clear();
        // `TT_Run_Context`: every program starts on the grid with axis-aligned vectors.
        self.gs.gep0 = 1;
        self.gs.gep1 = 1;
        self.gs.gep2 = 1;
        self.gs.proj = V { x: 0x4000, y: 0 };
        self.gs.free = V { x: 0x4000, y: 0 };
        self.gs.dual = V { x: 0x4000, y: 0 };
        self.gs.round_state = ROUND_GRID;
        self.gs.loop_ = 1;
        self.compute_funcs();
        self.update_round();
        self.glyf_cvt = None;
        self.glyf_storage = None;
        self.neg_jump_counter = 0;
        self.loopcall_counter = 0;
        self.max_ins = 0;
        self.iupx_called = false;
        self.iupy_called = false;
        let ok = self.run_ins();
        self.glyf_cvt = None;
        self.glyf_storage = None;
        ok
    }

    fn code_len(&self) -> i64 {
        self.codes[self.cur_range].len() as i64
    }
    fn cb(&self, i: i64) -> u8 {
        let c = &self.codes[self.cur_range];
        if i < 0 {
            0
        } else {
            *c.get(i as usize).unwrap_or(&0)
        }
    }

    // ----- rounding -------------------------------------------------------

    fn update_round(&mut self) {}

    fn round(&self, d: i64, comp: i64) -> i64 {
        match self.gs.round_state {
            ROUND_HALF => {
                if d >= 0 {
                    let v = pix_floor(d + comp) + 32;
                    if v < 0 {
                        32
                    } else {
                        v
                    }
                } else {
                    let v = -(pix_floor(comp - d) + 32);
                    if v > 0 {
                        -32
                    } else {
                        v
                    }
                }
            }
            ROUND_GRID => {
                if d >= 0 {
                    let v = pix_round(d + comp);
                    if v < 0 {
                        0
                    } else {
                        v
                    }
                } else {
                    let v = -pix_round(comp - d);
                    if v > 0 {
                        0
                    } else {
                        v
                    }
                }
            }
            ROUND_DOUBLE => {
                if d >= 0 {
                    let v = (d + comp + 16) & !31;
                    if v < 0 {
                        0
                    } else {
                        v
                    }
                } else {
                    let v = -((comp - d + 16) & !31);
                    if v > 0 {
                        0
                    } else {
                        v
                    }
                }
            }
            ROUND_DOWN => {
                if d >= 0 {
                    let v = pix_floor(d + comp);
                    if v < 0 {
                        0
                    } else {
                        v
                    }
                } else {
                    let v = -pix_floor(comp - d);
                    if v > 0 {
                        0
                    } else {
                        v
                    }
                }
            }
            ROUND_UP => {
                if d >= 0 {
                    let v = pix_ceil(d + comp);
                    if v < 0 {
                        0
                    } else {
                        v
                    }
                } else {
                    let v = -pix_ceil(comp - d);
                    if v > 0 {
                        0
                    } else {
                        v
                    }
                }
            }
            ROUND_OFF => self.round_none(d, comp),
            ROUND_SUPER => {
                if d >= 0 {
                    let mut v = (d + self.threshold - self.phase + comp) & -self.period;
                    v += self.phase;
                    if v < 0 {
                        v = self.phase;
                    }
                    v
                } else {
                    let mut v = -((self.threshold - self.phase + comp - d) & -self.period);
                    v -= self.phase;
                    if v > 0 {
                        v = -self.phase;
                    }
                    v
                }
            }
            ROUND_SUPER45 => {
                if d >= 0 {
                    let mut v =
                        ((d + self.threshold - self.phase + comp) / self.period) * self.period;
                    v += self.phase;
                    if v < 0 {
                        v = self.phase;
                    }
                    v
                } else {
                    let mut v =
                        -(((self.threshold - self.phase + comp - d) / self.period) * self.period);
                    v -= self.phase;
                    if v > 0 {
                        v = -self.phase;
                    }
                    v
                }
            }
            _ => self.round_none(d, comp),
        }
    }

    fn round_none(&self, d: i64, comp: i64) -> i64 {
        let _ = self;
        if d >= 0 {
            let v = d + comp;
            if v < 0 {
                0
            } else {
                v
            }
        } else {
            let v = d - comp;
            if v > 0 {
                0
            } else {
                v
            }
        }
    }

    fn set_super_round(&mut self, grid_period: i64, selector: i64) {
        let mut period = match selector & 0xC0 {
            0 => grid_period / 2,
            0x40 => grid_period,
            0x80 => grid_period * 2,
            _ => grid_period,
        };
        let mut phase = match selector & 0x30 {
            0 => 0,
            0x10 => period / 4,
            0x20 => period / 2,
            _ => period * 3 / 4,
        };
        let mut threshold = if selector & 0x0F == 0 {
            period - 1
        } else {
            ((selector & 0x0F) - 4) * period / 8
        };
        period >>= 8;
        phase >>= 8;
        threshold >>= 8;
        self.period = period;
        self.phase = phase;
        self.threshold = threshold;
    }

    // ----- projection ------------------------------------------------------

    fn project(&self, dx: i64, dy: i64) -> i64 {
        let p = self.gs.proj;
        if p.x == 0x4000 {
            dx
        } else if p.y == 0x4000 {
            dy
        } else {
            dot_fix14(dx, dy, p.x, p.y)
        }
    }

    fn dual_project(&self, dx: i64, dy: i64) -> i64 {
        let p = self.gs.dual;
        if p.x == 0x4000 {
            dx
        } else if p.y == 0x4000 {
            dy
        } else {
            dot_fix14(dx, dy, p.x, p.y)
        }
    }

    fn compute_funcs(&mut self) {
        let g = &self.gs;
        let r = g.proj.x * g.free.x + g.proj.y * g.free.y + 0x2000;
        let f = r >> 14;
        if r > 0x0FFF_7FFF {
            self.mv_x = g.free.x * 4;
            self.mv_y = g.free.y * 4;
            self.mv_mode = if g.free.x == 0x4000 {
                1
            } else if g.free.y == 0x4000 {
                2
            } else {
                0
            };
        } else if f.abs() <= 0x3FF {
            self.mv_x = 0;
            self.mv_y = 0;
            self.mv_mode = 0;
        } else {
            self.mv_x = (g.free.x << 16) / f;
            self.mv_y = (g.free.y << 16) / f;
            self.mv_mode = 0;
        }
    }

    fn zone_n(&self, sel: u16) -> usize {
        self.zones[sel as usize].cur.len()
    }

    /// `func_move` on zone `sel`, point `p`.
    fn do_move(&mut self, sel: u16, p: usize, distance: i64) {
        if self.trace && self.ini_range == RANGE_GLYPH {
            let c = self.zones[sel as usize].cur[p];
            eprintln!("M {} {} {} {}", p, distance, c.x, c.y);
        }
        let (mx, my, mode) = (self.mv_x, self.mv_y, self.mv_mode);
        let z = &mut self.zones[sel as usize];
        match mode {
            1 => {
                z.cur[p].x += distance;
                z.tags[p] |= TOUCH_X;
            }
            2 => {
                z.cur[p].y += distance;
                z.tags[p] |= TOUCH_Y;
            }
            _ => {
                if mx != 0 {
                    z.cur[p].x += mul_fix(distance, mx);
                    z.tags[p] |= TOUCH_X;
                }
                if my != 0 {
                    z.cur[p].y += mul_fix(distance, my);
                    z.tags[p] |= TOUCH_Y;
                }
            }
        }
    }

    fn do_move_orig(&mut self, sel: u16, p: usize, distance: i64) {
        let (mx, my, mode) = (self.mv_x, self.mv_y, self.mv_mode);
        let z = &mut self.zones[sel as usize];
        match mode {
            1 => z.org[p].x += distance,
            2 => z.org[p].y += distance,
            _ => {
                if mx != 0 {
                    z.org[p].x += mul_fix(distance, mx);
                }
                if my != 0 {
                    z.org[p].y += mul_fix(distance, my);
                }
            }
        }
    }

    fn move_zp2_point(&mut self, p: usize, dx: i64, dy: i64, touch: bool) {
        let free = self.gs.free;
        let sel = self.gs.gep2 as usize;
        let z = &mut self.zones[sel];
        if free.x != 0 {
            z.cur[p].x += dx;
            if touch {
                z.tags[p] |= TOUCH_X;
            }
        }
        if free.y != 0 {
            z.cur[p].y += dy;
            if touch {
                z.tags[p] |= TOUCH_Y;
            }
        }
    }

    fn fail(&mut self) {
        self.err = true;
    }

    // ----- the main loop ---------------------------------------------------

    fn run_ins(&mut self) -> bool {
        let mut ins_counter: u64 = 0;
        // Empty programs and programs that fall off the end finish cleanly.
        if self.ip >= self.code_len() {
            return true;
        }
        loop {
            self.opcode = self.cb(self.ip);
            self.length = opcode_length(self.opcode);
            if self.length < 0 {
                if self.ip + 1 >= self.code_len() {
                    return false;
                }
                self.length = 2 - self.length * self.cb(self.ip + 1) as i64;
            }
            if self.ip + self.length > self.code_len() {
                return false;
            }
            let (pop, push) = pop_push(self.opcode);
            if self.top < pop {
                for i in 0..pop {
                    if i < self.stack.len() {
                        self.stack[i] = 0;
                    }
                }
                self.args = 0;
            } else {
                self.args = self.top - pop;
            }
            self.new_top = self.args + push;
            if self.new_top > self.stack.len() {
                return false;
            }
            self.step_ins = true;
            self.err = false;
            if self.trace && self.ini_range == RANGE_GLYPH {
                let st: Vec<i64> = self.stack[..self.top]
                    .iter()
                    .rev()
                    .take(6)
                    .cloned()
                    .collect();
                eprintln!("ip {} op {:02x} gs[rp {} {} {} proj {:?} free {:?} gep {} {} {} rs {}] stack-top {:?}", self.ip, self.opcode, self.gs.rp0, self.gs.rp1, self.gs.rp2, self.gs.proj, self.gs.free, self.gs.gep0, self.gs.gep1, self.gs.gep2, self.gs.round_state, st);
            }
            self.exec_one();
            if self.err {
                // Undefined opcode: look for an `IDEF`.
                if self.opcode_is_unknown() {
                    let op = self.opcode as u32;
                    if let Some(def) = self.idefs.iter().find(|d| d.active && d.opc == op).cloned()
                    {
                        if self.call_stack.len() >= 32 {
                            return false;
                        }
                        self.call_stack.push(CallRec {
                            caller_range: self.cur_range,
                            caller_ip: self.ip + 1,
                            cur_count: 1,
                            def: def.clone(),
                        });
                        if !self.goto_range(def.range, def.start) {
                            return false;
                        }
                        self.step_ins = false;
                        self.err = false;
                        if self.ip >= self.code_len() {
                            if !self.call_stack.is_empty() {
                                return false;
                            }
                            return true;
                        }
                        continue;
                    }
                }
                return false;
            }
            self.top = self.new_top;
            if self.step_ins {
                self.ip += self.length;
            }
            ins_counter += 1;
            if ins_counter > MAX_RUNNABLE_OPCODES {
                return false;
            }
            if self.ip >= self.code_len() {
                if !self.call_stack.is_empty() {
                    return false;
                }
                return true;
            }
        }
    }

    fn opcode_is_unknown(&self) -> bool {
        matches!(
            self.opcode,
            0x28 | 0x7B | 0x83 | 0x84 | 0x8F | 0x90 | 0x91 | 0x92 | 0x93..=0xAF
        ) || (self.opcode >= 0x8F && self.opcode <= 0xAF)
    }

    fn goto_range(&mut self, range: usize, ip: i64) -> bool {
        if range == 0 || range > 3 {
            return false;
        }
        if ip > self.codes[range].len() as i64 {
            return false;
        }
        self.cur_range = range;
        self.ip = ip;
        true
    }

    fn skip_code(&mut self) -> bool {
        self.ip += self.length;
        if self.ip < self.code_len() {
            self.opcode = self.cb(self.ip);
            self.length = opcode_length(self.opcode);
            if self.length < 0 {
                if self.ip + 1 >= self.code_len() {
                    return false;
                }
                self.length = 2 - self.length * self.cb(self.ip + 1) as i64;
            }
            if self.ip + self.length <= self.code_len() {
                return true;
            }
        }
        false
    }

    fn get_short_ins(&mut self) -> i64 {
        let v = ((self.cb(self.ip) as i16) << 8) | self.cb(self.ip + 1) as i16;
        self.ip += 2;
        v as i64
    }

    #[inline]
    fn a(&self, i: usize) -> i64 {
        self.stack[self.args + i]
    }
    #[inline]
    fn set_a(&mut self, i: usize, v: i64) {
        let k = self.args + i;
        self.stack[k] = v;
    }

    fn read_cvt(&self, idx: usize) -> i64 {
        match &self.glyf_cvt {
            Some(c) => c[idx],
            None => self.cvt[idx],
        }
    }

    fn cvt_mut(&mut self) -> &mut Vec<i64> {
        if self.ini_range == RANGE_GLYPH && self.glyf_cvt.is_none() {
            self.glyf_cvt = Some(self.cvt.clone());
        }
        match &mut self.glyf_cvt {
            Some(c) => c,
            None => &mut self.cvt,
        }
    }

    fn storage_mut(&mut self) -> &mut Vec<i64> {
        if self.ini_range == RANGE_GLYPH && self.glyf_storage.is_none() {
            self.glyf_storage = Some(self.storage.clone());
        }
        match &mut self.glyf_storage {
            Some(c) => c,
            None => &mut self.storage,
        }
    }

    fn normalize(&self, vx: i64, vy: i64) -> Option<V> {
        if vx == 0 && vy == 0 {
            return None;
        }
        let (x, y) = norm_len(vx, vy);
        Some(V { x: x / 4, y: y / 4 })
    }

    fn sxvtl(&self, idx1: u16, idx2: u16, opc: u8) -> Option<V> {
        let (z1, z2) = (
            &self.zones[self.gs.gep1 as usize],
            &self.zones[self.gs.gep2 as usize],
        );
        if idx1 as usize >= z2.cur.len() || idx2 as usize >= z1.cur.len() {
            return None;
        }
        let p1 = z1.cur[idx2 as usize];
        let p2 = z2.cur[idx1 as usize];
        let mut a = p1.x - p2.x;
        let mut b = p1.y - p2.y;
        let mut opc = opc;
        if a == 0 && b == 0 {
            a = 0x4000;
            opc = 0;
        }
        if opc & 1 != 0 {
            let c = b;
            b = a;
            a = -c;
        }
        // Normalize of a zero vector cannot happen here.
        self.normalize(a, b)
    }

    fn exec_one(&mut self) {
        let op = self.opcode;
        match op {
            0x00..=0x05 => {
                let a = ((op & 1) as i64) << 14;
                let b = a ^ 0x4000;
                if op < 4 {
                    self.gs.proj = V { x: a, y: b };
                    self.gs.dual = V { x: a, y: b };
                }
                if op & 2 == 0 {
                    self.gs.free = V { x: a, y: b };
                }
                self.compute_funcs();
            }
            0x06 | 0x07 => {
                let (a1, a0) = (self.a(1) as u16, self.a(0) as u16);
                match self.sxvtl(a1, a0, op) {
                    Some(v) => {
                        self.gs.proj = v;
                        self.gs.dual = v;
                        self.compute_funcs();
                    }
                    None => {}
                }
            }
            0x08 | 0x09 => {
                let (a1, a0) = (self.a(1) as u16, self.a(0) as u16);
                if let Some(v) = self.sxvtl(a1, a0, op) {
                    self.gs.free = v;
                    self.compute_funcs();
                }
            }
            0x0A => {
                let y = self.a(1) as i16 as i64;
                let x = self.a(0) as i16 as i64;
                if let Some(v) = self.normalize(x, y) {
                    self.gs.proj = v;
                    self.gs.dual = v;
                }
                self.compute_funcs();
            }
            0x0B => {
                let y = self.a(1) as i16 as i64;
                let x = self.a(0) as i16 as i64;
                if let Some(v) = self.normalize(x, y) {
                    self.gs.free = v;
                }
                self.compute_funcs();
            }
            0x0C => {
                let p = self.gs.proj;
                self.set_a(0, p.x);
                self.set_a(1, p.y);
            }
            0x0D => {
                let p = self.gs.free;
                self.set_a(0, p.x);
                self.set_a(1, p.y);
            }
            0x0E => {
                self.gs.free = self.gs.proj;
                self.compute_funcs();
            }
            0x0F => self.ins_isect(),
            0x10 => self.gs.rp0 = self.a(0) as u16,
            0x11 => self.gs.rp1 = self.a(0) as u16,
            0x12 => self.gs.rp2 = self.a(0) as u16,
            0x13 => match self.a(0) {
                0 | 1 => self.gs.gep0 = self.a(0) as u16,
                _ => self.fail(),
            },
            0x14 => match self.a(0) {
                0 | 1 => self.gs.gep1 = self.a(0) as u16,
                _ => self.fail(),
            },
            0x15 => match self.a(0) {
                0 | 1 => self.gs.gep2 = self.a(0) as u16,
                _ => self.fail(),
            },
            0x16 => match self.a(0) {
                0 | 1 => {
                    let v = self.a(0) as u16;
                    self.gs.gep0 = v;
                    self.gs.gep1 = v;
                    self.gs.gep2 = v;
                }
                _ => self.fail(),
            },
            0x17 => {
                if self.a(0) < 0 {
                    self.fail();
                } else {
                    self.gs.loop_ = self.a(0).min(0xFFFF);
                }
            }
            0x18 => self.gs.round_state = ROUND_GRID,
            0x19 => self.gs.round_state = ROUND_HALF,
            0x1A => self.gs.min_dist = self.a(0),
            0x1B => {
                // ELSE
                let mut n_ifs = 1;
                loop {
                    if !self.skip_code() {
                        self.fail();
                        return;
                    }
                    match self.opcode {
                        0x58 => n_ifs += 1,
                        0x59 => n_ifs -= 1,
                        _ => {}
                    }
                    if n_ifs == 0 {
                        break;
                    }
                }
            }
            0x1C => self.ins_jmpr(),
            0x1D => self.gs.cvt_cutin = self.a(0),
            0x1E => self.gs.swc_cutin = self.a(0),
            0x1F => self.gs.sw_value = mul_fix(self.a(0), self.tt_scale),
            0x20 => {
                let v = self.a(0);
                self.set_a(1, v);
            }
            0x21 => {}
            0x22 => self.new_top = 0,
            0x23 => {
                let (x, y) = (self.a(0), self.a(1));
                self.set_a(0, y);
                self.set_a(1, x);
            }
            0x24 => self.set_a(0, self.top as i64),
            0x25 => {
                let l = self.a(0);
                if l <= 0 || l > self.args as i64 {
                    self.fail();
                    self.set_a(0, 0);
                } else {
                    let v = self.stack[self.args - l as usize];
                    self.set_a(0, v);
                }
            }
            0x26 => {
                let l = self.a(0);
                if l <= 0 || l > self.args as i64 {
                    self.fail();
                } else {
                    let l = l as usize;
                    let k = self.stack[self.args - l];
                    let s = self.args - l;
                    for i in 0..l - 1 {
                        self.stack[s + i] = self.stack[s + i + 1];
                    }
                    self.stack[self.args - 1] = k;
                }
            }
            0x27 => {
                let (p1, p2) = (self.a(0) as usize, self.a(1) as usize);
                let (s1, s0) = (self.gs.gep1 as usize, self.gs.gep0 as usize);
                if p1 >= self.zones[s1].cur.len() || p2 >= self.zones[s0].cur.len() {
                    self.fail();
                    return;
                }
                let c0 = self.zones[s0].cur[p2];
                let c1 = self.zones[s1].cur[p1];
                let distance = self.project(c0.x - c1.x, c0.y - c1.y) / 2;
                self.do_move(s1 as u16, p1, distance);
                self.do_move(s0 as u16, p2, -distance);
            }
            0x28 => self.fail(),
            0x29 => {
                let point = self.a(0) as usize;
                let s0 = self.gs.gep0 as usize;
                if point >= self.zones[s0].cur.len() {
                    self.fail();
                    return;
                }
                let mut mask = 0xFFu8;
                if self.gs.free.x != 0 {
                    mask &= !TOUCH_X;
                }
                if self.gs.free.y != 0 {
                    mask &= !TOUCH_Y;
                }
                self.zones[s0].tags[point] &= mask;
            }
            0x2A => self.ins_call(true),
            0x2B => self.ins_call(false),
            0x2C => self.ins_def(true),
            0x2D => {
                // ENDF
                let Some(mut rec) = self.call_stack.pop() else {
                    self.fail();
                    return;
                };
                rec.cur_count -= 1;
                self.step_ins = false;
                if rec.cur_count > 0 {
                    let start = rec.def.start;
                    self.call_stack.push(rec);
                    self.ip = start;
                } else if !self.goto_range(rec.caller_range, rec.caller_ip) {
                    self.fail();
                }
            }
            0x2E | 0x2F => self.ins_mdap(),
            0x30 | 0x31 => self.ins_iup(),
            0x32 | 0x33 => self.ins_shp(),
            0x34 | 0x35 => self.ins_shc(),
            0x36 | 0x37 => self.ins_shz(),
            0x38 => self.ins_shpix(),
            0x39 => self.ins_ip(),
            0x3A | 0x3B => self.ins_msirp(),
            0x3C => self.ins_alignrp(),
            0x3D => self.gs.round_state = ROUND_DOUBLE,
            0x3E | 0x3F => self.ins_miap(),
            0x40 => {
                let l = self.cb(self.ip + 1) as usize;
                if l > self.stack.len() + 1 - self.top.min(self.stack.len() + 1) {
                    self.fail();
                    return;
                }
                if self.args + l > self.stack.len() {
                    self.fail();
                    return;
                }
                for k in 1..=l {
                    let v = self.cb(self.ip + k as i64 + 1) as i64;
                    self.set_a(k - 1, v);
                }
                self.new_top += l;
            }
            0x41 => {
                let l = self.cb(self.ip + 1) as usize;
                if self.args + l > self.stack.len() {
                    self.fail();
                    return;
                }
                self.ip += 2;
                for k in 0..l {
                    let v = self.get_short_ins();
                    self.set_a(k, v);
                }
                self.step_ins = false;
                self.new_top += l;
            }
            0x42 => {
                let i = self.a(0);
                if i < 0 || i as usize >= self.storage.len() {
                    self.fail_soft();
                } else {
                    let v = self.a(1);
                    self.storage_mut()[i as usize] = v;
                }
            }
            0x43 => {
                let i = self.a(0);
                if i < 0 || i as usize >= self.storage.len() {
                    self.set_a(0, 0);
                } else {
                    let v = match &self.glyf_storage {
                        Some(s) => s[i as usize],
                        None => self.storage[i as usize],
                    };
                    self.set_a(0, v);
                }
            }
            0x44 => {
                let i = self.a(0);
                if i < 0 || i as usize >= self.cvt.len() {
                    self.fail_soft();
                } else {
                    let v = self.a(1);
                    self.cvt_mut()[i as usize] = v;
                }
            }
            0x45 => {
                let i = self.a(0);
                if i < 0 || i as usize >= self.cvt.len() {
                    self.set_a(0, 0);
                } else {
                    let v = self.read_cvt(i as usize);
                    self.set_a(0, v);
                }
            }
            0x46 | 0x47 => {
                let l = self.a(0);
                let s2 = self.gs.gep2 as usize;
                let r = if l < 0 || l as usize >= self.zones[s2].cur.len() {
                    0
                } else if op & 1 != 0 {
                    let o = self.zones[s2].org[l as usize];
                    self.dual_project(o.x, o.y)
                } else {
                    let c = self.zones[s2].cur[l as usize];
                    self.project(c.x, c.y)
                };
                self.set_a(0, r);
            }
            0x48 => {
                let l = self.a(0);
                let s2 = self.gs.gep2;
                if l < 0 || l as usize >= self.zone_n(s2) {
                    self.fail();
                    return;
                }
                let l = l as usize;
                let c = self.zones[s2 as usize].cur[l];
                let k = self.project(c.x, c.y);
                let d = self.a(1) - k;
                self.do_move(s2, l, d);
                if self.gs.gep2 == 0 {
                    let cc = self.zones[0].cur[l];
                    self.zones[0].org[l] = cc;
                }
            }
            0x49 | 0x4A => self.ins_md(),
            0x4B => self.set_a(0, self.ppem),
            0x4C => self.set_a(0, self.point_size),
            0x4D => self.gs.auto_flip = true,
            0x4E => self.gs.auto_flip = false,
            0x4F => self.fail(),
            0x50 => self.set_a(0, (self.a(0) < self.a(1)) as i64),
            0x51 => self.set_a(0, (self.a(0) <= self.a(1)) as i64),
            0x52 => self.set_a(0, (self.a(0) > self.a(1)) as i64),
            0x53 => self.set_a(0, (self.a(0) >= self.a(1)) as i64),
            0x54 => self.set_a(0, (self.a(0) == self.a(1)) as i64),
            0x55 => self.set_a(0, (self.a(0) != self.a(1)) as i64),
            0x56 => {
                let r = self.round(self.a(0), 0);
                self.set_a(0, ((r & 127) == 64) as i64);
            }
            0x57 => {
                let r = self.round(self.a(0), 0);
                self.set_a(0, ((r & 127) == 0) as i64);
            }
            0x58 => {
                // IF
                if self.a(0) != 0 {
                    return;
                }
                let mut n_ifs = 1;
                loop {
                    if !self.skip_code() {
                        self.fail();
                        return;
                    }
                    let out;
                    match self.opcode {
                        0x58 => {
                            n_ifs += 1;
                            out = false;
                        }
                        0x1B => out = n_ifs == 1,
                        0x59 => {
                            n_ifs -= 1;
                            out = n_ifs == 0;
                        }
                        _ => out = false,
                    }
                    if out {
                        break;
                    }
                }
            }
            0x59 => {}
            0x5A => self.set_a(0, (self.a(0) != 0 && self.a(1) != 0) as i64),
            0x5B => self.set_a(0, (self.a(0) != 0 || self.a(1) != 0) as i64),
            0x5C => self.set_a(0, (self.a(0) == 0) as i64),
            0x5D | 0x71 | 0x72 => self.ins_deltap(),
            0x5E => self.gs.delta_base = self.a(0) as u16,
            0x5F => {
                if (self.a(0) as u64) > 6 {
                    self.fail();
                } else {
                    self.gs.delta_shift = self.a(0) as u16;
                }
            }
            0x60 => self.set_a(0, self.a(0).wrapping_add(self.a(1))),
            0x61 => self.set_a(0, self.a(0).wrapping_sub(self.a(1))),
            0x62 => {
                if self.a(1) == 0 {
                    self.fail();
                } else {
                    self.set_a(0, mul_div_no_round(self.a(0), 64, self.a(1)));
                }
            }
            0x63 => self.set_a(0, mul_div(self.a(0), self.a(1), 64)),
            0x64 => self.set_a(0, self.a(0).abs()),
            0x65 => self.set_a(0, -self.a(0)),
            0x66 => self.set_a(0, pix_floor(self.a(0))),
            0x67 => self.set_a(0, pix_ceil(self.a(0))),
            0x68..=0x6B => {
                let r = self.round(self.a(0), 0);
                self.set_a(0, r);
            }
            0x6C..=0x6F => {
                let r = self.round_none(self.a(0), 0);
                self.set_a(0, r);
            }
            0x70 => {
                let i = self.a(0);
                if i < 0 || i as usize >= self.cvt.len() {
                    self.fail_soft();
                } else {
                    let v = mul_fix(self.a(1), self.tt_scale);
                    self.cvt_mut()[i as usize] = v;
                }
            }
            0x73..=0x75 => self.ins_deltac(),
            0x76 => {
                self.set_super_round(0x4000, self.a(0));
                self.gs.round_state = ROUND_SUPER;
            }
            0x77 => {
                self.set_super_round(0x2D41, self.a(0));
                self.gs.round_state = ROUND_SUPER45;
            }
            0x78 => {
                if self.a(1) != 0 {
                    self.ins_jmpr();
                }
            }
            0x79 => {
                if self.a(1) == 0 {
                    self.ins_jmpr();
                }
            }
            0x7A => self.gs.round_state = ROUND_OFF,
            0x7C => self.gs.round_state = ROUND_UP,
            0x7D => self.gs.round_state = ROUND_DOWN,
            0x7E | 0x7F => {}
            0x80 => self.ins_flippt(),
            0x81 | 0x82 => {
                let (k, l) = (self.a(1), self.a(0));
                let n = self.zones[1].cur.len() as i64;
                if k < 0 || k >= n || l < 0 || l >= n {
                    self.fail();
                    return;
                }
                for i in l..=k {
                    if op == 0x81 {
                        self.zones[1].tags[i as usize] |= TAG_ON;
                    } else {
                        self.zones[1].tags[i as usize] &= !TAG_ON;
                    }
                }
            }
            0x85 => self.ins_scanctrl(),
            0x86 | 0x87 => self.ins_sdpvtl(),
            0x88 => {
                let a0 = self.a(0);
                let mut k = 0i64;
                if a0 & 1 != 0 {
                    k = 40;
                }
                self.set_a(0, k);
            }
            0x89 => self.ins_def(false),
            0x8A => {
                let (a, b, c) = (self.a(2), self.a(1), self.a(0));
                self.set_a(2, c);
                self.set_a(1, a);
                self.set_a(0, b);
            }
            0x8B => self.set_a(0, self.a(0).max(self.a(1))),
            0x8C => self.set_a(0, self.a(0).min(self.a(1))),
            0x8D => {
                if self.a(0) >= 0 {
                    self.gs.scan_type = (self.a(0) & 0xFFFF) as i32;
                }
            }
            0x8E => self.ins_instctrl(),
            0xB0..=0xB7 => {
                let l = (op - 0xB0) as usize + 1;
                if self.args + l > self.stack.len() {
                    self.fail();
                    return;
                }
                for k in 1..=l {
                    let v = self.cb(self.ip + k as i64) as i64;
                    self.set_a(k - 1, v);
                }
            }
            0xB8..=0xBF => {
                let l = (op - 0xB8) as usize + 1;
                if self.args + l > self.stack.len() {
                    self.fail();
                    return;
                }
                self.ip += 1;
                for k in 0..l {
                    let v = self.get_short_ins();
                    self.set_a(k, v);
                }
                self.step_ins = false;
            }
            0xC0..=0xDF => self.ins_mdrp(),
            0xE0..=0xFF => self.ins_mirp(),
            _ => self.fail(),
        }
    }

    /// Errors FreeType ignores in non-pedantic mode (bad indices and the like).
    fn fail_soft(&mut self) {}

    // ----- control flow -----------------------------------------------------

    fn ins_jmpr(&mut self) {
        let a0 = self.a(0);
        if a0 == 0 && self.args == 0 {
            self.fail();
            return;
        }
        self.ip += a0;
        let bad = self.ip < 0
            || (!self.call_stack.is_empty()
                && self.ip
                    > self
                        .call_stack
                        .last()
                        .map(|c| c.def.end)
                        .unwrap_or(i64::MAX));
        if bad {
            self.fail();
            return;
        }
        self.step_ins = false;
        if a0 < 0 {
            self.neg_jump_counter += 1;
            if self.neg_jump_counter > 1_000_000 {
                self.fail();
            }
        }
    }

    fn find_fdef(&self, f: i64) -> Option<Def> {
        if f < 0 {
            return None;
        }
        self.fdefs.iter().find(|d| d.opc as i64 == f).cloned()
    }

    fn ins_call(&mut self, looped: bool) {
        let (count, f) = if looped {
            (self.a(0), self.a(1))
        } else {
            (1, self.a(0))
        };
        let Some(def) = self.find_fdef(f) else {
            self.fail();
            return;
        };
        if !def.active {
            self.fail();
            return;
        }
        if looped && count <= 0 {
            return;
        }
        if self.call_stack.len() >= 32 {
            self.fail();
            return;
        }
        self.call_stack.push(CallRec {
            caller_range: self.cur_range,
            caller_ip: self.ip + 1,
            cur_count: count,
            def: def.clone(),
        });
        if !self.goto_range(def.range, def.start) {
            self.fail();
            return;
        }
        self.step_ins = false;
        if looped {
            self.loopcall_counter += count as u64;
            if self.loopcall_counter > 10_000_000 {
                self.fail();
            }
        }
    }

    fn ins_def(&mut self, is_fdef: bool) {
        if self.ini_range == RANGE_GLYPH {
            self.fail();
            return;
        }
        let n = self.a(0);
        let (limit, table_len) = if is_fdef {
            (self.max_fdefs, self.fdefs.len())
        } else {
            (self.max_idefs, self.idefs.len())
        };
        let pos = if is_fdef {
            self.fdefs.iter().position(|d| d.opc as i64 == n)
        } else {
            self.idefs.iter().position(|d| d.opc as i64 == (n & 0xFF))
        };
        let pos = match pos {
            Some(p) => p,
            None => {
                if table_len >= limit.max(1) && table_len >= limit {
                    self.fail();
                    return;
                }
                if is_fdef {
                    self.fdefs.push(Def::default());
                    self.fdefs.len() - 1
                } else {
                    self.idefs.push(Def::default());
                    self.idefs.len() - 1
                }
            }
        };
        if is_fdef && n > 0xFFFF {
            self.fail();
            return;
        }
        if !is_fdef && n > 0xFF {
            self.fail();
            return;
        }
        let start = self.ip + 1;
        let range = self.cur_range;
        let opc = n as u32;
        if is_fdef && opc > self.max_func {
            self.max_func = opc;
        }
        let mut end = 0;
        loop {
            if !self.skip_code() {
                self.fail();
                return;
            }
            match self.opcode {
                0x89 | 0x2C => {
                    self.fail();
                    return;
                }
                0x2D => {
                    end = self.ip;
                    break;
                }
                _ => {}
            }
        }
        let d = Def {
            range,
            opc,
            start,
            end,
            active: true,
        };
        if is_fdef {
            self.fdefs[pos] = d;
        } else {
            self.idefs[pos] = d;
        }
    }

    // ----- point instructions ----------------------------------------------

    fn ins_mdap(&mut self) {
        let point = self.a(0) as usize;
        let s0 = self.gs.gep0;
        if point >= self.zone_n(s0) {
            return;
        }
        let distance = if self.opcode & 1 != 0 {
            let c = self.zones[s0 as usize].cur[point];
            let cur_dist = self.project(c.x, c.y);
            self.round(cur_dist, 0) - cur_dist
        } else {
            0
        };
        self.do_move(s0, point, distance);
        self.gs.rp0 = point as u16;
        self.gs.rp1 = point as u16;
    }

    fn ins_miap(&mut self) {
        let cvt_entry = self.a(1);
        let point = self.a(0);
        let s0 = self.gs.gep0;
        if point < 0
            || point as usize >= self.zone_n(s0)
            || cvt_entry < 0
            || cvt_entry as usize >= self.cvt.len()
        {
            self.gs.rp0 = point as u16;
            self.gs.rp1 = point as u16;
            return;
        }
        let point = point as usize;
        let mut distance = self.read_cvt(cvt_entry as usize);
        if self.gs.gep0 == 0 {
            let f = self.gs.free;
            let nx = mul_fix14(distance, f.x);
            let ny = mul_fix14(distance, f.y);
            let z = &mut self.zones[0];
            z.org[point] = V { x: nx, y: ny };
            z.cur[point] = z.org[point];
        }
        let c = self.zones[s0 as usize].cur[point];
        let org_dist = self.project(c.x, c.y);
        if self.opcode & 1 != 0 {
            let delta = (distance - org_dist).abs();
            if delta > self.gs.cvt_cutin {
                distance = org_dist;
            }
            distance = self.round(distance, 0);
        }
        self.do_move(s0, point, distance - org_dist);
        self.gs.rp0 = point as u16;
        self.gs.rp1 = point as u16;
    }

    fn ins_mdrp(&mut self) {
        let point = self.a(0);
        let (s0, s1) = (self.gs.gep0, self.gs.gep1);
        let rp0 = self.gs.rp0 as usize;
        let ok = point >= 0 && (point as usize) < self.zone_n(s1) && rp0 < self.zone_n(s0);
        if ok {
            let point = point as usize;
            let mut org_dist;
            if self.gs.gep0 == 0 || self.gs.gep1 == 0 {
                let v1 = self.zones[s1 as usize].org[point];
                let v2 = self.zones[s0 as usize].org[rp0];
                org_dist = self.dual_project(v1.x - v2.x, v1.y - v2.y);
            } else {
                let v1 = self.zones[s1 as usize].orus[point];
                let v2 = self.zones[s0 as usize].orus[rp0];
                if self.x_scale == self.y_scale {
                    org_dist = self.dual_project(v1.x - v2.x, v1.y - v2.y);
                    org_dist = mul_fix(org_dist, self.x_scale);
                } else {
                    let vx = mul_fix(v1.x - v2.x, self.x_scale);
                    let vy = mul_fix(v1.y - v2.y, self.y_scale);
                    org_dist = self.dual_project(vx, vy);
                }
            }
            if self.gs.swc_cutin > 0
                && org_dist < self.gs.sw_value + self.gs.swc_cutin
                && org_dist > self.gs.sw_value - self.gs.swc_cutin
            {
                org_dist = if org_dist >= 0 {
                    self.gs.sw_value
                } else {
                    -self.gs.sw_value
                };
            }
            let mut distance = if self.opcode & 4 != 0 {
                self.round(org_dist, 0)
            } else {
                self.round_none(org_dist, 0)
            };
            if self.opcode & 8 != 0 {
                let md = self.gs.min_dist;
                if org_dist >= 0 {
                    if distance < md {
                        distance = md;
                    }
                } else if distance > -md {
                    distance = -md;
                }
            }
            let c1 = self.zones[s1 as usize].cur[point];
            let c0 = self.zones[s0 as usize].cur[rp0];
            let od = self.project(c1.x - c0.x, c1.y - c0.y);
            self.do_move(s1, point, distance - od);
        }
        self.gs.rp1 = self.gs.rp0;
        self.gs.rp2 = point as u16;
        if self.opcode & 16 != 0 {
            self.gs.rp0 = point as u16;
        }
    }

    fn ins_mirp(&mut self) {
        let point = self.a(0);
        let cvt_entry = self.a(1).wrapping_add(1);
        let (s0, s1) = (self.gs.gep0, self.gs.gep1);
        let rp0 = self.gs.rp0 as usize;
        let ok = point >= 0
            && (point as usize) < self.zone_n(s1)
            && cvt_entry >= 0
            && (cvt_entry as usize) < self.cvt.len() + 1
            && rp0 < self.zone_n(s0);
        if ok {
            let point = point as usize;
            let mut cvt_dist = if cvt_entry == 0 {
                0
            } else {
                self.read_cvt(cvt_entry as usize - 1)
            };
            let delta = (cvt_dist - self.gs.sw_value).abs();
            if delta < self.gs.swc_cutin {
                cvt_dist = if cvt_dist >= 0 {
                    self.gs.sw_value
                } else {
                    -self.gs.sw_value
                };
            }
            if self.gs.gep1 == 0 {
                let f = self.gs.free;
                let o0 = self.zones[s0 as usize].org[rp0];
                let nx = o0.x + mul_fix14(cvt_dist, f.x);
                let ny = o0.y + mul_fix14(cvt_dist, f.y);
                let z = &mut self.zones[s1 as usize];
                z.org[point] = V { x: nx, y: ny };
                z.cur[point] = z.org[point];
            }
            let o1 = self.zones[s1 as usize].org[point];
            let o0 = self.zones[s0 as usize].org[rp0];
            let org_dist = self.dual_project(o1.x - o0.x, o1.y - o0.y);
            let c1 = self.zones[s1 as usize].cur[point];
            let c0 = self.zones[s0 as usize].cur[rp0];
            let cur_dist = self.project(c1.x - c0.x, c1.y - c0.y);
            if self.gs.auto_flip && (org_dist ^ cvt_dist) < 0 {
                cvt_dist = -cvt_dist;
            }
            let mut distance;
            if self.opcode & 4 != 0 {
                if self.gs.gep0 == self.gs.gep1 {
                    let delta = (cvt_dist - org_dist).abs();
                    if delta > self.gs.cvt_cutin {
                        cvt_dist = org_dist;
                    }
                }
                distance = self.round(cvt_dist, 0);
            } else {
                distance = self.round_none(cvt_dist, 0);
            }
            if self.opcode & 8 != 0 {
                let md = self.gs.min_dist;
                if org_dist >= 0 {
                    if distance < md {
                        distance = md;
                    }
                } else if distance > -md {
                    distance = -md;
                }
            }
            self.do_move(s1, point, distance - cur_dist);
        }
        self.gs.rp1 = self.gs.rp0;
        if self.opcode & 16 != 0 {
            self.gs.rp0 = point as u16;
        }
        self.gs.rp2 = point as u16;
    }

    fn ins_msirp(&mut self) {
        let point = self.a(0);
        let (s0, s1) = (self.gs.gep0, self.gs.gep1);
        let rp0 = self.gs.rp0 as usize;
        if point < 0 || point as usize >= self.zone_n(s1) || rp0 >= self.zone_n(s0) {
            return;
        }
        let point = point as usize;
        if self.gs.gep1 == 0 {
            let o = self.zones[s0 as usize].org[rp0];
            self.zones[s1 as usize].org[point] = o;
            let a1 = self.a(1);
            self.do_move_orig(s1, point, a1);
            let oo = self.zones[s1 as usize].org[point];
            self.zones[s1 as usize].cur[point] = oo;
        }
        let c1 = self.zones[s1 as usize].cur[point];
        let c0 = self.zones[s0 as usize].cur[rp0];
        let distance = self.project(c1.x - c0.x, c1.y - c0.y);
        let a1 = self.a(1);
        self.do_move(s1, point, a1 - distance);
        self.gs.rp1 = self.gs.rp0;
        self.gs.rp2 = point as u16;
        if self.opcode & 1 != 0 {
            self.gs.rp0 = point as u16;
        }
    }

    fn ins_alignrp(&mut self) {
        let (s0, s1) = (self.gs.gep0, self.gs.gep1);
        let rp0 = self.gs.rp0 as usize;
        if (self.top as i64) < self.gs.loop_ || rp0 >= self.zone_n(s0) {
            self.gs.loop_ = 1;
            self.new_top = self.args;
            return;
        }
        while self.gs.loop_ > 0 {
            self.args -= 1;
            let point = self.stack[self.args] as u16 as usize;
            if point < self.zone_n(s1) {
                let c1 = self.zones[s1 as usize].cur[point];
                let c0 = self.zones[s0 as usize].cur[rp0];
                let distance = self.project(c1.x - c0.x, c1.y - c0.y);
                self.do_move(s1, point, -distance);
            }
            self.gs.loop_ -= 1;
        }
        self.gs.loop_ = 1;
        self.new_top = self.args;
    }

    fn ins_ip(&mut self) {
        let loop_ok = (self.top as i64) >= self.gs.loop_;
        if loop_ok {
            self.ip_inner();
        }
        self.gs.loop_ = 1;
        self.new_top = self.args;
    }

    fn ip_inner(&mut self) {
        let (s0, s1, s2) = (self.gs.gep0, self.gs.gep1, self.gs.gep2);
        let twilight = self.gs.gep0 == 0 || self.gs.gep1 == 0 || self.gs.gep2 == 0;
        let rp1 = self.gs.rp1 as usize;
        let rp2 = self.gs.rp2 as usize;
        if rp1 >= self.zone_n(s0) {
            return;
        }
        let orus_base = if twilight {
            self.zones[s0 as usize].org[rp1]
        } else {
            self.zones[s0 as usize].orus[rp1]
        };
        let cur_base = self.zones[s0 as usize].cur[rp1];
        let (old_range, cur_range);
        if rp2 >= self.zone_n(s1) {
            old_range = 0;
            cur_range = 0;
        } else {
            old_range = if twilight {
                let o = self.zones[s1 as usize].org[rp2];
                self.dual_project(o.x - orus_base.x, o.y - orus_base.y)
            } else if self.x_scale == self.y_scale {
                let o = self.zones[s1 as usize].orus[rp2];
                self.dual_project(o.x - orus_base.x, o.y - orus_base.y)
            } else {
                let o = self.zones[s1 as usize].orus[rp2];
                let vx = mul_fix(o.x - orus_base.x, self.x_scale);
                let vy = mul_fix(o.y - orus_base.y, self.y_scale);
                self.dual_project(vx, vy)
            };
            let c = self.zones[s1 as usize].cur[rp2];
            cur_range = self.project(c.x - cur_base.x, c.y - cur_base.y);
        }
        while self.gs.loop_ > 0 {
            self.args -= 1;
            let point = self.stack[self.args] as u16 as usize;
            self.gs.loop_ -= 1;
            if point >= self.zone_n(s2) {
                continue;
            }
            let org_dist = if twilight {
                let o = self.zones[s2 as usize].org[point];
                self.dual_project(o.x - orus_base.x, o.y - orus_base.y)
            } else if self.x_scale == self.y_scale {
                let o = self.zones[s2 as usize].orus[point];
                self.dual_project(o.x - orus_base.x, o.y - orus_base.y)
            } else {
                let o = self.zones[s2 as usize].orus[point];
                let vx = mul_fix(o.x - orus_base.x, self.x_scale);
                let vy = mul_fix(o.y - orus_base.y, self.y_scale);
                self.dual_project(vx, vy)
            };
            let c = self.zones[s2 as usize].cur[point];
            let cur_dist = self.project(c.x - cur_base.x, c.y - cur_base.y);
            let new_dist = if org_dist != 0 {
                if old_range != 0 {
                    mul_div(org_dist, cur_range, old_range)
                } else {
                    org_dist
                }
            } else {
                0
            };
            self.do_move(s2, point, new_dist - cur_dist);
        }
    }

    fn compute_point_displacement(&self) -> Option<(i64, i64, u16, usize)> {
        let (zsel, p) = if self.opcode & 1 != 0 {
            (self.gs.gep0, self.gs.rp1 as usize)
        } else {
            (self.gs.gep1, self.gs.rp2 as usize)
        };
        if p >= self.zone_n(zsel) {
            return None;
        }
        let z = &self.zones[zsel as usize];
        let d = self.project(z.cur[p].x - z.org[p].x, z.cur[p].y - z.org[p].y);
        let x = mul_fix(d, self.mv_x);
        let y = mul_fix(d, self.mv_y);
        Some((x, y, zsel, p))
    }

    fn ins_shp(&mut self) {
        if (self.top as i64) < self.gs.loop_ {
            self.gs.loop_ = 1;
            self.new_top = self.args;
            return;
        }
        if let Some((dx, dy, _zp, _refp)) = self.compute_point_displacement() {
            while self.gs.loop_ > 0 {
                self.args -= 1;
                let point = self.stack[self.args] as u16 as usize;
                if point < self.zone_n(self.gs.gep2) {
                    self.move_zp2_point(point, dx, dy, true);
                }
                self.gs.loop_ -= 1;
            }
        }
        self.gs.loop_ = 1;
        self.new_top = self.args;
    }

    fn ins_shc(&mut self) {
        let contour = self.a(0);
        let s2 = self.gs.gep2;
        let bounds = if s2 == 0 {
            1
        } else {
            self.zones[s2 as usize].contours.len() as i64
        };
        if contour < 0 || contour >= bounds {
            return;
        }
        let Some((dx, dy, zp, refp)) = self.compute_point_displacement() else {
            return;
        };
        let contour = contour as usize;
        let start = if contour == 0 {
            0
        } else {
            self.zones[s2 as usize].contours[contour - 1] as usize + 1
        };
        let limit = if s2 == 0 {
            self.zone_n(s2)
        } else {
            self.zones[s2 as usize].contours[contour] as usize + 1
        };
        for i in start..limit {
            if !(zp == s2 && refp == i) {
                self.move_zp2_point(i, dx, dy, true);
            }
        }
    }

    fn ins_shz(&mut self) {
        if self.a(0) < 0 || self.a(0) >= 2 {
            return;
        }
        let Some((dx, dy, zp, refp)) = self.compute_point_displacement() else {
            return;
        };
        let s2 = self.gs.gep2;
        let limit = if s2 == 0 {
            self.zone_n(s2)
        } else if s2 == 1 && !self.zones[1].contours.is_empty() {
            *self.zones[1].contours.last().unwrap() as usize + 1
        } else {
            0
        };
        for i in 0..limit {
            if !(zp == s2 && refp == i) {
                self.move_zp2_point(i, dx, dy, false);
            }
        }
    }

    fn ins_shpix(&mut self) {
        let f = self.gs.free;
        let dx = mul_fix14(self.a(0), f.x);
        let dy = mul_fix14(self.a(0), f.y);
        if (self.top as i64) < self.gs.loop_ + 1 {
            self.gs.loop_ = 1;
            self.new_top = self.args;
            return;
        }
        while self.gs.loop_ > 0 {
            self.args -= 1;
            let point = self.stack[self.args] as u16 as usize;
            if point < self.zone_n(self.gs.gep2) {
                self.move_zp2_point(point, dx, dy, true);
            }
            self.gs.loop_ -= 1;
        }
        self.gs.loop_ = 1;
        self.new_top = self.args;
    }

    fn ins_isect(&mut self) {
        let point = self.a(0) as usize;
        let a0 = self.a(1) as usize;
        let a1 = self.a(2) as usize;
        let b0 = self.a(3) as usize;
        let b1 = self.a(4) as usize;
        let (s0, s1, s2) = (self.gs.gep0, self.gs.gep1, self.gs.gep2);
        if b0 >= self.zone_n(s0)
            || b1 >= self.zone_n(s0)
            || a0 >= self.zone_n(s1)
            || a1 >= self.zone_n(s1)
            || point >= self.zone_n(s2)
        {
            return;
        }
        let z0 = &self.zones[s0 as usize];
        let z1 = &self.zones[s1 as usize];
        let dbx = z0.cur[b1].x - z0.cur[b0].x;
        let dby = z0.cur[b1].y - z0.cur[b0].y;
        let dax = z1.cur[a1].x - z1.cur[a0].x;
        let day = z1.cur[a1].y - z1.cur[a0].y;
        let dx = z0.cur[b0].x - z1.cur[a0].x;
        let dy = z0.cur[b0].y - z1.cur[a0].y;
        let discriminant = mul_div(dax, -dby, 0x40) + mul_div(day, dbx, 0x40);
        let dotproduct = mul_div(dax, dbx, 0x40) + mul_div(day, dby, 0x40);
        let (nx, ny);
        if 19 * discriminant.abs() > dotproduct.abs() {
            let val = mul_div(dx, -dby, 0x40) + mul_div(dy, dbx, 0x40);
            let rx = mul_div(val, dax, discriminant);
            let ry = mul_div(val, day, discriminant);
            nx = z1.cur[a0].x + rx;
            ny = z1.cur[a0].y + ry;
        } else {
            nx = (z1.cur[a0].x + z1.cur[a1].x + z0.cur[b0].x + z0.cur[b1].x) / 4;
            ny = (z1.cur[a0].y + z1.cur[a1].y + z0.cur[b0].y + z0.cur[b1].y) / 4;
        }
        let z2 = &mut self.zones[s2 as usize];
        z2.tags[point] |= TOUCH_BOTH;
        z2.cur[point] = V { x: nx, y: ny };
    }

    fn ins_md(&mut self) {
        let k = self.a(1);
        let l = self.a(0);
        let (s0, s1) = (self.gs.gep0, self.gs.gep1);
        let d = if l < 0 || l as usize >= self.zone_n(s0) || k < 0 || k as usize >= self.zone_n(s1)
        {
            0
        } else {
            let (l, k) = (l as usize, k as usize);
            if self.opcode & 1 != 0 {
                let c0 = self.zones[s0 as usize].cur[l];
                let c1 = self.zones[s1 as usize].cur[k];
                self.project(c0.x - c1.x, c0.y - c1.y)
            } else if self.gs.gep0 == 0 || self.gs.gep1 == 0 {
                let v1 = self.zones[s0 as usize].org[l];
                let v2 = self.zones[s1 as usize].org[k];
                self.dual_project(v1.x - v2.x, v1.y - v2.y)
            } else {
                let v1 = self.zones[s0 as usize].orus[l];
                let v2 = self.zones[s1 as usize].orus[k];
                if self.x_scale == self.y_scale {
                    let d = self.dual_project(v1.x - v2.x, v1.y - v2.y);
                    mul_fix(d, self.x_scale)
                } else {
                    let vx = mul_fix(v1.x - v2.x, self.x_scale);
                    let vy = mul_fix(v1.y - v2.y, self.y_scale);
                    self.dual_project(vx, vy)
                }
            }
        };
        self.set_a(0, d);
    }

    fn ins_flippt(&mut self) {
        if (self.top as i64) < self.gs.loop_ {
            self.gs.loop_ = 1;
            self.new_top = self.args;
            return;
        }
        while self.gs.loop_ > 0 {
            self.args -= 1;
            let point = self.stack[self.args] as u16 as usize;
            if point < self.zones[1].cur.len() {
                self.zones[1].tags[point] ^= TAG_ON;
            }
            self.gs.loop_ -= 1;
        }
        self.gs.loop_ = 1;
        self.new_top = self.args;
    }

    fn ins_deltap(&mut self) {
        let nump = self.a(0) as u64;
        let p = self.ppem;
        let s0 = self.gs.gep0;
        let mut k = 1u64;
        while k <= nump {
            if self.args < 2 {
                self.args = 0;
                break;
            }
            self.args -= 2;
            let a = self.stack[self.args + 1] as u16 as usize;
            let b = self.stack[self.args];
            if a < self.zone_n(s0) {
                let mut c = ((b as u64) & 0xF0) >> 4;
                match self.opcode {
                    0x5D => {}
                    0x71 => c += 16,
                    _ => c += 32,
                }
                c += self.gs.delta_base as u64;
                if p as u64 == c {
                    let mut bb = ((b as u64) & 0xF) as i64 - 8;
                    if bb >= 0 {
                        bb += 1;
                    }
                    bb *= 1i64 << (6 - self.gs.delta_shift);
                    self.do_move(s0, a, bb);
                }
            }
            k += 1;
        }
        self.new_top = self.args;
    }

    fn ins_deltac(&mut self) {
        let nump = self.a(0) as u64;
        let p = self.ppem;
        let mut k = 1u64;
        while k <= nump {
            if self.args < 2 {
                self.args = 0;
                break;
            }
            self.args -= 2;
            let a = self.stack[self.args + 1];
            let b = self.stack[self.args];
            if a >= 0 && (a as usize) < self.cvt.len() {
                let mut c = ((b as u64) & 0xF0) >> 4;
                match self.opcode {
                    0x73 => {}
                    0x74 => c += 16,
                    _ => c += 32,
                }
                c += self.gs.delta_base as u64;
                if p as u64 == c {
                    let mut bb = ((b as u64) & 0xF) as i64 - 8;
                    if bb >= 0 {
                        bb += 1;
                    }
                    bb *= 1i64 << (6 - self.gs.delta_shift);
                    let cv = self.cvt_mut();
                    cv[a as usize] += bb;
                }
            }
            k += 1;
        }
        self.new_top = self.args;
    }

    fn ins_scanctrl(&mut self) {
        let a = self.a(0) & 0xFF;
        if a == 0xFF {
            self.gs.scan_control = true;
            return;
        } else if a == 0 {
            self.gs.scan_control = false;
            return;
        }
        let v = self.a(0);
        if v & 0x100 != 0 && self.ppem <= a {
            self.gs.scan_control = true;
        }
        if v & 0x800 != 0 && self.ppem > a {
            self.gs.scan_control = false;
        }
    }

    fn ins_sdpvtl(&mut self) {
        let p1 = self.a(1) as u16 as usize;
        let p2 = self.a(0) as u16 as usize;
        let (s1, s2) = (self.gs.gep1, self.gs.gep2);
        if p2 >= self.zone_n(s1) || p1 >= self.zone_n(s2) {
            return;
        }
        let mut opc = self.opcode;
        let v1 = self.zones[s1 as usize].org[p2];
        let v2 = self.zones[s2 as usize].org[p1];
        let mut a = v1.x - v2.x;
        let mut b = v1.y - v2.y;
        if a == 0 && b == 0 {
            a = 0x4000;
            opc = 0;
        }
        if opc & 1 != 0 {
            let c = b;
            b = a;
            a = -c;
        }
        if let Some(v) = self.normalize(a, b) {
            self.gs.dual = v;
        }
        let mut opc = self.opcode;
        let v1 = self.zones[s1 as usize].cur[p2];
        let v2 = self.zones[s2 as usize].cur[p1];
        let mut a = v1.x - v2.x;
        let mut b = v1.y - v2.y;
        if a == 0 && b == 0 {
            a = 0x4000;
            opc = 0;
        }
        if opc & 1 != 0 {
            let c = b;
            b = a;
            a = -c;
        }
        if let Some(v) = self.normalize(a, b) {
            self.gs.proj = v;
        }
        self.compute_funcs();
    }

    fn ins_instctrl(&mut self) {
        let k = self.a(1);
        let l = self.a(0);
        if !(1..=3).contains(&k) {
            self.fail();
            return;
        }
        let kf = 1i64 << (k - 1);
        if l != 0 && l != kf {
            self.fail();
            return;
        }
        if self.ini_range == RANGE_CVT {
            self.gs.instruct_control &= !(kf as u8);
            self.gs.instruct_control |= l as u8;
        }
    }

    fn ins_iup(&mut self) {
        if self.zones[1].contours.is_empty() {
            return;
        }
        let xaxis = self.opcode & 1 != 0;
        let mask = if xaxis { TOUCH_X } else { TOUCH_Y };
        let z = &mut self.zones[1];
        let n = z.cur.len();
        let mut contour = 0usize;
        let mut point = 0usize;
        loop {
            let mut end_point = z.contours[contour] as usize;
            let first_point = point;
            if end_point >= n {
                end_point = n - 1;
            }
            while point <= end_point && z.tags[point] & mask == 0 {
                point += 1;
            }
            if point <= end_point {
                let first_touched = point;
                let mut cur_touched = point;
                point += 1;
                while point <= end_point {
                    if z.tags[point] & mask != 0 {
                        iup_interpolate(
                            z,
                            xaxis,
                            cur_touched + 1,
                            point as i64 - 1,
                            cur_touched,
                            point,
                        );
                        cur_touched = point;
                    }
                    point += 1;
                }
                if cur_touched == first_touched {
                    iup_shift(z, xaxis, first_point, end_point, cur_touched);
                } else {
                    iup_interpolate(
                        z,
                        xaxis,
                        cur_touched + 1,
                        end_point as i64,
                        cur_touched,
                        first_touched,
                    );
                    if first_touched > 0 {
                        iup_interpolate(
                            z,
                            xaxis,
                            first_point,
                            first_touched as i64 - 1,
                            cur_touched,
                            first_touched,
                        );
                    }
                }
            }
            contour += 1;
            if contour >= z.contours.len() {
                break;
            }
        }
    }
}

fn coord(v: &V, xaxis: bool) -> i64 {
    if xaxis {
        v.x
    } else {
        v.y
    }
}
fn set_coord(v: &mut V, xaxis: bool, c: i64) {
    if xaxis {
        v.x = c;
    } else {
        v.y = c;
    }
}

fn iup_shift(z: &mut Zone, xaxis: bool, p1: usize, p2: usize, p: usize) {
    let dx = coord(&z.cur[p], xaxis) - coord(&z.org[p], xaxis);
    if dx != 0 {
        for i in p1..p {
            let c = coord(&z.cur[i], xaxis) + dx;
            set_coord(&mut z.cur[i], xaxis, c);
        }
        for i in p + 1..=p2 {
            let c = coord(&z.cur[i], xaxis) + dx;
            set_coord(&mut z.cur[i], xaxis, c);
        }
    }
}

fn iup_interpolate(z: &mut Zone, xaxis: bool, p1: usize, p2: i64, ref1: usize, ref2: usize) {
    if (p1 as i64) > p2 {
        return;
    }
    let p2 = p2 as usize;
    let n = z.cur.len();
    if ref1 >= n || ref2 >= n {
        return;
    }
    let (mut ref1, mut ref2) = (ref1, ref2);
    let mut orus1 = coord(&z.orus[ref1], xaxis);
    let mut orus2 = coord(&z.orus[ref2], xaxis);
    if orus1 > orus2 {
        std::mem::swap(&mut orus1, &mut orus2);
        std::mem::swap(&mut ref1, &mut ref2);
    }
    let org1 = coord(&z.org[ref1], xaxis);
    let org2 = coord(&z.org[ref2], xaxis);
    let cur1 = coord(&z.cur[ref1], xaxis);
    let cur2 = coord(&z.cur[ref2], xaxis);
    let delta1 = cur1 - org1;
    let delta2 = cur2 - org2;
    if cur1 == cur2 || orus1 == orus2 {
        for i in p1..=p2 {
            let mut x = coord(&z.org[i], xaxis);
            if x <= org1 {
                x += delta1;
            } else if x >= org2 {
                x += delta2;
            } else {
                x = cur1;
            }
            set_coord(&mut z.cur[i], xaxis, x);
        }
    } else {
        let mut scale = 0i64;
        let mut scale_valid = false;
        for i in p1..=p2 {
            let mut x = coord(&z.org[i], xaxis);
            if x <= org1 {
                x += delta1;
            } else if x >= org2 {
                x += delta2;
            } else {
                if !scale_valid {
                    scale_valid = true;
                    scale = div_fix(cur2 - cur1, orus2 - orus1);
                }
                x = cur1 + mul_fix(coord(&z.orus[i], xaxis) - orus1, scale);
            }
            set_coord(&mut z.cur[i], xaxis, x);
        }
    }
}

// Silence the unused import when the module is built without tests.
#[allow(dead_code)]
fn _font_marker(_: &TtFont) {}
