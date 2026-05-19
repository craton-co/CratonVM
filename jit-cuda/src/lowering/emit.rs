//! Java bytecode → PTX body emitter.
//!
//! The kernel-body emitter is built around three abstractions:
//!
//! * [`RegPool`] hands out fresh PTX virtual register names. It also
//!   tracks the maximum index used per `RegKind` so the kernel can
//!   declare them up front.
//! * [`OpStack`] is a simulated JVM operand stack of typed register
//!   names. Each opcode pops some operands, computes (emitting PTX
//!   instructions to a string), and pushes the result.
//! * [`Locals`] maps each JVM local-variable slot to its current
//!   typed-register binding. `*store` writes, `*load` reads.
//!
//! The whole emitter never panics on input bytecode it understands; if
//! it encounters something outside the canonical-pattern subset it
//! returns [`LoweringError::UnsupportedNode`] with a precise message.

use crate::analyzer::ParamKind;
use crate::emitter::{LoweringError, RegKind};
use crate::lowering::loop_recog::{instr_size, CountedLoop};
use crate::signature::KernelSignature;
use std::fmt::Write;

/// One slot of typed JVM state.
#[derive(Clone, Debug)]
pub(crate) struct Reg {
    pub kind: RegKind,
    pub name: String,
    /// Whether this register holds a 64-bit (category-2) JVM value
    /// (`long`/`double`). The simulated stack uses this to size pops.
    pub wide: bool,
}

/// Allocator for fresh PTX register names. Each `RegKind` gets its
/// own counter; the largest index emitted becomes the kernel's
/// `.reg .<kind> %<prefix><N>` declaration count.
#[derive(Default)]
pub(crate) struct RegPool {
    pub u32_count: u32,
    pub u64_count: u32,
    pub s32_count: u32,
    pub s64_count: u32,
    pub f32_count: u32,
    pub f64_count: u32,
    pub pred_count: u32,
}

impl RegPool {
    pub fn fresh(&mut self, kind: RegKind) -> String {
        let (count, prefix) = match kind {
            RegKind::U32 => (&mut self.u32_count, "ru"),
            RegKind::U64 => (&mut self.u64_count, "rd"),
            RegKind::S32 => (&mut self.s32_count, "r"),
            RegKind::S64 => (&mut self.s64_count, "rl"),
            RegKind::F32 => (&mut self.f32_count, "f"),
            RegKind::F64 => (&mut self.f64_count, "fd"),
            RegKind::Pred => (&mut self.pred_count, "p"),
        };
        let n = *count;
        *count += 1;
        format!("%{prefix}{n}")
    }

    pub fn fresh_reg(&mut self, kind: RegKind) -> Reg {
        let wide = matches!(kind, RegKind::U64 | RegKind::S64 | RegKind::F64);
        Reg {
            kind,
            name: self.fresh(kind),
            wide,
        }
    }
}

/// Simulated JVM operand stack of typed registers.
#[derive(Default)]
pub(crate) struct OpStack(Vec<Reg>);

impl OpStack {
    pub fn push(&mut self, r: Reg) {
        self.0.push(r);
    }

    pub fn pop(&mut self) -> Result<Reg, LoweringError> {
        self.0
            .pop()
            .ok_or_else(|| LoweringError::Internal("operand stack underflow".into()))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Per-local-slot register binding. JVM has up to `max_locals` slots,
/// each addressed by index. Long/double take 2 slots but the emitter
/// only stores into the low slot — the simulator ignores slot+1.
#[derive(Default)]
pub(crate) struct Locals(Vec<Option<Reg>>);

impl Locals {
    pub fn ensure(&mut self, idx: usize) {
        if self.0.len() <= idx {
            self.0.resize(idx + 1, None);
        }
    }

    pub fn get(&self, idx: usize) -> Option<&Reg> {
        self.0.get(idx).and_then(|s| s.as_ref())
    }

    pub fn set(&mut self, idx: usize, r: Reg) {
        self.ensure(idx);
        self.0[idx] = Some(r);
    }
}

/// Identifies an array-reference's backing kernel parameter — either a
/// regular method parameter (`pN_ptr` / `pN_len`) or a `this.<field>`
/// access (`pthis_<i>_ptr` / `pthis_<i>_len`).
///
/// Phase 9 #2 — non-static methods access primitive-array fields via
/// the `aload_0; getfield <cp_index>` pattern. Each unique cp_index in
/// the method body becomes one extra kernel parameter pair; the index
/// here is the zero-based position of that cp_index in the dedup'd
/// `this_field_cps` list (see [`Emitter::this_field_cps_unique`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum ArrayKey {
    /// `pN_ptr` / `pN_len` — a declared method parameter.
    Param(usize),
    /// `pthis_N_ptr` / `pthis_N_len` — a unique `this.<field>` access.
    ThisField(usize),
}

impl ArrayKey {
    /// PTX-side name of the matching `_len` parameter.
    pub fn len_param_name(self) -> String {
        match self {
            ArrayKey::Param(i) => format!("p{i}_len"),
            ArrayKey::ThisField(i) => format!("pthis_{i}_len"),
        }
    }
}

/// All state the emitter needs while walking a method.
pub(crate) struct Emitter<'a> {
    pub bytes: &'a [u8],
    pub body: String,
    pub regs: RegPool,
    pub stack: OpStack,
    pub locals: Locals,
    pub sig: &'a KernelSignature,
    /// For each parameter index: the PTX register name we loaded the
    /// array pointer into (empty string for scalar params). Populated
    /// by `bind_param_locals`; used by `array_param_of` to map an
    /// `aload`'d array reference back to its parameter index.
    pub param_ptr_reg: Vec<String>,
    /// Phase 9 #2 — whether the lowered method is non-static. When
    /// true, slot 0 holds `this` (the [`Self::this_sentinel_name`]
    /// register), and `aload_0; getfield <cp>` chains resolve through
    /// [`Self::this_field_ptr_reg`].
    pub is_static: bool,
    /// Phase 9 #2 — name of the sentinel U64 register bound to slot 0
    /// for non-static methods. Never written by the kernel — only
    /// inspected to detect the `aload_0; getfield` shape. Empty
    /// string for static methods.
    pub this_sentinel_name: String,
    /// Phase 9 #2 — dedup'd `this_field_cps`, in the body-encounter
    /// order recorded by the analyzer. `this_field_cps_unique[i]` is
    /// the cp_index whose ptr lives in `pthis_<i>_ptr`.
    pub this_field_cps_unique: Vec<u16>,
    /// Phase 9 #2 — for each unique cp_index, the U64 register we
    /// pre-loaded `pthis_<i>_ptr` into. Key: cp_index; value: (the
    /// dedup'd index, the register). `getfield <cp>` pushes the
    /// matching register's clone onto the operand stack; from then on
    /// it behaves identically to an array-parameter reference.
    pub this_field_ptr_reg: std::collections::HashMap<u16, (usize, Reg)>,
    /// Local slot of the loop induction variable (if we are in a loop).
    pub iv_slot: Option<u16>,
    /// Loop-bound register; populated once we emit the prologue. Used
    /// only for the early-out at kernel entry.
    pub bound_reg: Option<Reg>,
    /// Register holding `tid` (computed once in the prologue).
    pub tid_reg: Option<Reg>,
    /// Label used to signal an out-of-bounds bounds-check failure.
    /// Emitted once at kernel epilogue; jump from each `*aload`/`*astore`.
    pub bounds_fail_label: String,
    /// Set to `true` the first time we emit a bounds check; controls
    /// whether the failure block needs to be emitted at the end.
    pub used_bounds_label: bool,
    /// Set to `true` when a `goto` to the loop header is encountered;
    /// caller should stop emitting at that point.
    pub hit_back_branch: bool,
    /// Local slot used for the kernel result return (scalar return only).
    /// Populated by the post-loop walker when it sees the matching `*return`.
    pub ret_value_reg: Option<Reg>,
}

impl<'a> Emitter<'a> {
    pub fn new(bytes: &'a [u8], sig: &'a KernelSignature, is_static: bool) -> Self {
        // Phase 9 #2 — dedup the analyzer's body-encounter order list.
        // Each unique cp_index becomes one kernel parameter pair; we
        // preserve first-encounter order so the kernel arg ordering
        // is stable and matches the marshaller's iteration.
        let mut this_field_cps_unique: Vec<u16> = Vec::new();
        for &cp in &sig.this_field_cps {
            if !this_field_cps_unique.contains(&cp) {
                this_field_cps_unique.push(cp);
            }
        }
        Self {
            bytes,
            body: String::new(),
            regs: RegPool::default(),
            stack: OpStack::default(),
            locals: Locals::default(),
            sig,
            param_ptr_reg: vec![String::new(); sig.param_kinds.len()],
            is_static,
            this_sentinel_name: String::new(),
            this_field_cps_unique,
            this_field_ptr_reg: std::collections::HashMap::new(),
            iv_slot: None,
            bound_reg: None,
            tid_reg: None,
            bounds_fail_label: "L_bounds_fail".into(),
            used_bounds_label: false,
            hit_back_branch: false,
            ret_value_reg: None,
        }
    }

    /// Initialize local slot bindings from method parameters: slot 0 is
    /// param 0, slot 1 is param 1 (or slot 2 for a long/double param 0),
    /// etc. Each array parameter binds the slot to its `pN_ptr` U64 reg;
    /// each scalar parameter binds to a freshly-loaded value reg.
    ///
    /// Phase 9 #2 — for non-static methods, slot 0 is bound to a
    /// sentinel U64 register that stands in for `this`. The sentinel
    /// is never written by the kernel; subsequent `aload_0; getfield`
    /// sequences resolve the sentinel back to the corresponding
    /// `pthis_<i>_ptr` register pre-loaded here.
    pub fn bind_param_locals(&mut self) -> Result<(), LoweringError> {
        let mut slot = 0usize;
        // Phase 9 #2 — slot 0 = `this` for non-static methods. Bind a
        // sentinel U64 reg; `aload_0; getfield` recognises this name
        // and rewrites the stack top to the matching this-field ptr.
        if !self.is_static {
            let sentinel = self.regs.fresh_reg(RegKind::U64);
            // No `ld.param` is emitted — the sentinel is purely a
            // type-system marker on the simulated operand stack.
            // The marshaller does not pass `this` as a kernel arg;
            // only the named fields are marshalled.
            self.this_sentinel_name = sentinel.name.clone();
            self.locals.set(slot, sentinel);
            slot += 1;
            // Pre-load each unique this-field array pointer into a
            // fresh U64 register. `getfield <cp>` replaces the
            // sentinel on the stack with a clone of this register;
            // from that point on the array reference behaves
            // identically to a method-parameter array reference.
            for (i, &cp) in self.this_field_cps_unique.clone().iter().enumerate() {
                let r = self.regs.fresh_reg(RegKind::U64);
                writeln!(self.body, "    ld.param.u64 {}, [pthis_{i}_ptr];", r.name).unwrap();
                self.this_field_ptr_reg.insert(cp, (i, r));
            }
        }
        for (i, k) in self.sig.param_kinds.iter().enumerate() {
            match k {
                ParamKind::Void => {}
                ParamKind::I32 => {
                    let r = self.regs.fresh_reg(RegKind::S32);
                    writeln!(self.body, "    ld.param.s32 {}, [p{i}];", r.name).unwrap();
                    self.locals.set(slot, r);
                    slot += 1;
                }
                ParamKind::I64 => {
                    let r = self.regs.fresh_reg(RegKind::S64);
                    writeln!(self.body, "    ld.param.s64 {}, [p{i}];", r.name).unwrap();
                    self.locals.set(slot, r);
                    slot += 2;
                }
                ParamKind::F32 => {
                    let r = self.regs.fresh_reg(RegKind::F32);
                    writeln!(self.body, "    ld.param.f32 {}, [p{i}];", r.name).unwrap();
                    self.locals.set(slot, r);
                    slot += 1;
                }
                ParamKind::F64 => {
                    let r = self.regs.fresh_reg(RegKind::F64);
                    writeln!(self.body, "    ld.param.f64 {}, [p{i}];", r.name).unwrap();
                    self.locals.set(slot, r);
                    slot += 2;
                }
                ParamKind::I32Array
                | ParamKind::I64Array
                | ParamKind::F32Array
                | ParamKind::F64Array
                | ParamKind::I16Array
                | ParamKind::I8Array => {
                    let r = self.regs.fresh_reg(RegKind::U64);
                    writeln!(self.body, "    ld.param.u64 {}, [p{i}_ptr];", r.name).unwrap();
                    self.param_ptr_reg[i] = r.name.clone();
                    self.locals.set(slot, r);
                    slot += 1;
                }
            }
        }
        Ok(())
    }

    /// Emit the `tid = ctaid.x * ntid.x + tid.x` computation. Result is
    /// stored in `self.tid_reg`.
    pub fn emit_tid(&mut self) {
        let ctaid = self.regs.fresh_reg(RegKind::U32);
        let ntid = self.regs.fresh_reg(RegKind::U32);
        let tidx = self.regs.fresh_reg(RegKind::U32);
        let tid_u32 = self.regs.fresh_reg(RegKind::U32);
        let tid_s32 = self.regs.fresh_reg(RegKind::S32);
        writeln!(self.body, "    mov.u32 {}, %ctaid.x;", ctaid.name).unwrap();
        writeln!(self.body, "    mov.u32 {}, %ntid.x;", ntid.name).unwrap();
        writeln!(self.body, "    mov.u32 {}, %tid.x;", tidx.name).unwrap();
        writeln!(
            self.body,
            "    mad.lo.u32 {}, {}, {}, {};",
            tid_u32.name, ctaid.name, ntid.name, tidx.name
        )
        .unwrap();
        // Reinterpret as s32 for index arithmetic.
        writeln!(self.body, "    mov.b32 {}, {};", tid_s32.name, tid_u32.name).unwrap();
        self.tid_reg = Some(tid_s32);
    }

    /// Emit `if (tid >= bound) ret;` using a fresh predicate register.
    pub fn emit_loop_guard(&mut self, bound: &Reg) {
        let tid = self.tid_reg.clone().expect("emit_tid was called");
        let p = self.regs.fresh_reg(RegKind::Pred);
        writeln!(
            self.body,
            "    setp.ge.s32 {}, {}, {};",
            p.name, tid.name, bound.name
        )
        .unwrap();
        writeln!(self.body, "    @{} bra L_done;", p.name).unwrap();
    }

    /// Append the bounds-check failure block and the kernel epilogue
    /// label. The kernel always ends with an unconditional `ret;`.
    pub fn finalize_epilogue(&mut self) {
        // Common "done" label — used by the loop guard and the return
        // paths. We just fall through to ret.
        writeln!(self.body, "L_done:").unwrap();
        writeln!(self.body, "    ret;").unwrap();

        // Bounds-fail block: write 1 to *failure_flag and ret.
        if self.used_bounds_label {
            let flag_ptr = self.regs.fresh_reg(RegKind::U64);
            let one = self.regs.fresh_reg(RegKind::U32);
            writeln!(self.body, "{}:", self.bounds_fail_label).unwrap();
            writeln!(
                self.body,
                "    ld.param.u64 {}, [failure_flag];",
                flag_ptr.name
            )
            .unwrap();
            writeln!(self.body, "    mov.u32 {}, 1;", one.name).unwrap();
            writeln!(
                self.body,
                "    st.global.u32 [{}], {};",
                flag_ptr.name, one.name
            )
            .unwrap();
            writeln!(self.body, "    ret;").unwrap();
        }
    }

    /// Emit a bounds check: if `index >= len` jump to failure label.
    /// `len_param` is the kernel-parameter name (e.g., `p0_len`).
    fn emit_bounds_check(&mut self, index: &Reg, len_param: &str) {
        self.used_bounds_label = true;
        let len = self.regs.fresh_reg(RegKind::S32);
        let p_neg = self.regs.fresh_reg(RegKind::Pred);
        let p_ge = self.regs.fresh_reg(RegKind::Pred);
        writeln!(
            self.body,
            "    ld.param.s32 {}, [{}];",
            len.name, len_param
        )
        .unwrap();
        // index < 0 also fails — Java semantics.
        let zero = self.regs.fresh_reg(RegKind::S32);
        writeln!(self.body, "    mov.s32 {}, 0;", zero.name).unwrap();
        writeln!(
            self.body,
            "    setp.lt.s32 {}, {}, {};",
            p_neg.name, index.name, zero.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    @{} bra {};",
            p_neg.name, self.bounds_fail_label
        )
        .unwrap();
        writeln!(
            self.body,
            "    setp.ge.s32 {}, {}, {};",
            p_ge.name, index.name, len.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    @{} bra {};",
            p_ge.name, self.bounds_fail_label
        )
        .unwrap();
    }

    /// Walk a region of bytecode from `start` to `end` (exclusive),
    /// emitting PTX for each opcode. The walker returns early if it
    /// hits a `goto` to the loop header (back-branch).
    pub fn walk(
        &mut self,
        start: usize,
        end: usize,
        loop_info: Option<&CountedLoop>,
    ) -> Result<(), LoweringError> {
        let mut pc = start;
        while pc < end {
            if self.hit_back_branch {
                return Ok(());
            }
            let op = self.bytes[pc];
            let size = instr_size(self.bytes, pc)?;
            self.emit_op(op, pc, loop_info)?;
            pc += size;
        }
        Ok(())
    }

    fn emit_op(
        &mut self,
        op: u8,
        pc: usize,
        loop_info: Option<&CountedLoop>,
    ) -> Result<(), LoweringError> {
        match op {
            // ── constants ────────────────────────────────────────────
            0x01 => {
                // aconst_null — push a u64 zero. We never expect to use
                // it, but the analyzer permits the opcode.
                let r = self.regs.fresh_reg(RegKind::U64);
                writeln!(self.body, "    mov.u64 {}, 0;", r.name).unwrap();
                self.stack.push(r);
            }
            0x02..=0x08 => {
                let v = op as i32 - 0x03; // iconst_m1..iconst_5
                let r = self.regs.fresh_reg(RegKind::S32);
                writeln!(self.body, "    mov.s32 {}, {};", r.name, v).unwrap();
                self.stack.push(r);
            }
            0x09 | 0x0A => {
                let v: i64 = (op - 0x09) as i64;
                let r = self.regs.fresh_reg(RegKind::S64);
                writeln!(self.body, "    mov.s64 {}, {};", r.name, v).unwrap();
                self.stack.push(r);
            }
            0x0B..=0x0D => {
                let v: f32 = (op - 0x0B) as f32;
                let r = self.regs.fresh_reg(RegKind::F32);
                writeln!(self.body, "    mov.f32 {}, 0f{:08X};", r.name, v.to_bits()).unwrap();
                self.stack.push(r);
            }
            0x0E | 0x0F => {
                let v: f64 = (op - 0x0E) as f64;
                let r = self.regs.fresh_reg(RegKind::F64);
                writeln!(self.body, "    mov.f64 {}, 0d{:016X};", r.name, v.to_bits()).unwrap();
                self.stack.push(r);
            }
            0x10 => {
                // bipush
                let v = self.bytes[pc + 1] as i8 as i32;
                let r = self.regs.fresh_reg(RegKind::S32);
                writeln!(self.body, "    mov.s32 {}, {};", r.name, v).unwrap();
                self.stack.push(r);
            }
            0x11 => {
                // sipush
                let v = i16::from_be_bytes([self.bytes[pc + 1], self.bytes[pc + 2]]) as i32;
                let r = self.regs.fresh_reg(RegKind::S32);
                writeln!(self.body, "    mov.s32 {}, {};", r.name, v).unwrap();
                self.stack.push(r);
            }
            // ── locals: load ────────────────────────────────────────
            0x15 => self.iload(self.bytes[pc + 1] as u16)?,
            0x1A..=0x1D => self.iload((op - 0x1A) as u16)?,
            0x16 => self.lload(self.bytes[pc + 1] as u16)?,
            0x1E..=0x21 => self.lload((op - 0x1E) as u16)?,
            0x17 => self.fload(self.bytes[pc + 1] as u16)?,
            0x22..=0x25 => self.fload((op - 0x22) as u16)?,
            0x18 => self.dload(self.bytes[pc + 1] as u16)?,
            0x26..=0x29 => self.dload((op - 0x26) as u16)?,
            0x19 => self.aload(self.bytes[pc + 1] as u16)?,
            0x2A..=0x2D => self.aload((op - 0x2A) as u16)?,
            // ── locals: store ───────────────────────────────────────
            0x36 => self.istore(self.bytes[pc + 1] as u16)?,
            0x3B..=0x3E => self.istore((op - 0x3B) as u16)?,
            0x37 => self.lstore(self.bytes[pc + 1] as u16)?,
            0x3F..=0x42 => self.lstore((op - 0x3F) as u16)?,
            0x38 => self.fstore(self.bytes[pc + 1] as u16)?,
            0x43..=0x46 => self.fstore((op - 0x43) as u16)?,
            0x39 => self.dstore(self.bytes[pc + 1] as u16)?,
            0x47..=0x4A => self.dstore((op - 0x47) as u16)?,
            0x3A => self.astore(self.bytes[pc + 1] as u16)?,
            0x4B..=0x4E => self.astore((op - 0x4B) as u16)?,
            // ── array load ──────────────────────────────────────────
            0x2E => self.array_load(RegKind::S32, ".s32", 4)?, // iaload
            0x2F => self.array_load(RegKind::S64, ".s64", 8)?, // laload
            0x30 => self.array_load(RegKind::F32, ".f32", 4)?, // faload
            0x31 => self.array_load(RegKind::F64, ".f64", 8)?, // daload
            0x33 => self.array_load_byte()?,                    // baload
            0x34 => self.array_load_char()?,                    // caload
            0x35 => self.array_load_short()?,                   // saload
            // ── array store ─────────────────────────────────────────
            0x4F => self.array_store(RegKind::S32, ".s32", 4)?, // iastore
            0x50 => self.array_store(RegKind::S64, ".s64", 8)?, // lastore
            0x51 => self.array_store(RegKind::F32, ".f32", 4)?, // fastore
            0x52 => self.array_store(RegKind::F64, ".f64", 8)?, // dastore
            0x54 => self.array_store_byte()?,                    // bastore
            0x55 => self.array_store_char_or_short(2)?,          // castore
            0x56 => self.array_store_char_or_short(2)?,          // sastore
            // ── stack ops ───────────────────────────────────────────
            0x57 => {
                self.stack.pop()?;
            }
            0x58 => {
                // pop2 — pop two cat-1, or one cat-2
                let top = self.stack.pop()?;
                if !top.wide {
                    self.stack.pop()?;
                }
            }
            0x59 => {
                let r = self.stack.pop()?;
                self.stack.push(r.clone());
                self.stack.push(r);
            }
            0x5A => {
                // dup_x1
                let a = self.stack.pop()?;
                let b = self.stack.pop()?;
                self.stack.push(a.clone());
                self.stack.push(b);
                self.stack.push(a);
            }
            0x5B => {
                // dup_x2
                let a = self.stack.pop()?;
                let b = self.stack.pop()?;
                if b.wide {
                    self.stack.push(a.clone());
                    self.stack.push(b);
                    self.stack.push(a);
                } else {
                    let c = self.stack.pop()?;
                    self.stack.push(a.clone());
                    self.stack.push(c);
                    self.stack.push(b);
                    self.stack.push(a);
                }
            }
            0x5C => {
                // dup2 — for cat-1 it's "top two"; for cat-2 it's the
                // single wide value.
                let a = self.stack.pop()?;
                if a.wide {
                    self.stack.push(a.clone());
                    self.stack.push(a);
                } else {
                    let b = self.stack.pop()?;
                    self.stack.push(b.clone());
                    self.stack.push(a.clone());
                    self.stack.push(b);
                    self.stack.push(a);
                }
            }
            0x5D | 0x5E => {
                return Err(LoweringError::UnsupportedNode(format!(
                    "dup2_x{} on the JVM stack is not lowered (uncommon for element-wise loops)",
                    if op == 0x5D { 1 } else { 2 }
                )));
            }
            0x5F => {
                let a = self.stack.pop()?;
                let b = self.stack.pop()?;
                self.stack.push(a);
                self.stack.push(b);
            }
            // ── arithmetic ──────────────────────────────────────────
            0x60 => self.binop_i32("add.s32")?,
            0x61 => self.binop_i64("add.s64")?,
            0x62 => self.binop_f32("add.f32")?,
            0x63 => self.binop_f64("add.f64")?,
            0x64 => self.binop_i32("sub.s32")?,
            0x65 => self.binop_i64("sub.s64")?,
            0x66 => self.binop_f32("sub.f32")?,
            0x67 => self.binop_f64("sub.f64")?,
            0x68 => self.binop_i32("mul.lo.s32")?,
            0x69 => self.binop_i64("mul.lo.s64")?,
            0x6A => self.binop_f32("mul.f32")?,
            0x6B => self.binop_f64("mul.f64")?,
            0x6C => self.binop_i32("div.s32")?,
            0x6D => self.binop_i64("div.s64")?,
            0x6E => self.binop_f32("div.f32")?,
            0x6F => self.binop_f64("div.f64")?,
            0x70 => self.binop_i32("rem.s32")?,
            0x71 => self.binop_i64("rem.s64")?,
            0x72 => self.binop_f32("rem.f32")?,
            0x73 => self.binop_f64("rem.f64")?,
            0x74 => self.unop_i32("neg.s32")?,
            0x75 => self.unop_i64("neg.s64")?,
            0x76 => self.unop_f32("neg.f32")?,
            0x77 => self.unop_f64("neg.f64")?,
            // Shifts: PTX shl/shr take a 32-bit shift count.
            0x78 => self.shift_i32("shl.b32")?,
            0x79 => self.shift_i64("shl.b64")?,
            0x7A => self.shift_i32("shr.s32")?,
            0x7B => self.shift_i64("shr.s64")?,
            0x7C => self.shift_i32("shr.u32")?,
            0x7D => self.shift_i64("shr.u64")?,
            0x7E => self.binop_i32("and.b32")?,
            0x7F => self.binop_i64("and.b64")?,
            0x80 => self.binop_i32("or.b32")?,
            0x81 => self.binop_i64("or.b64")?,
            0x82 => self.binop_i32("xor.b32")?,
            0x83 => self.binop_i64("xor.b64")?,
            // ── iinc ────────────────────────────────────────────────
            0x84 => {
                let idx = self.bytes[pc + 1] as u16;
                let delta = self.bytes[pc + 2] as i8 as i32;
                self.iinc(idx, delta)?;
            }
            // ── conversions ─────────────────────────────────────────
            0x85 => self.conv("cvt.s64.s32", RegKind::S32, RegKind::S64)?, // i2l
            0x86 => self.conv("cvt.rn.f32.s32", RegKind::S32, RegKind::F32)?, // i2f
            0x87 => self.conv("cvt.rn.f64.s32", RegKind::S32, RegKind::F64)?, // i2d
            0x88 => self.conv("cvt.s32.s64", RegKind::S64, RegKind::S32)?, // l2i
            0x89 => self.conv("cvt.rn.f32.s64", RegKind::S64, RegKind::F32)?, // l2f
            0x8A => self.conv("cvt.rn.f64.s64", RegKind::S64, RegKind::F64)?, // l2d
            0x8B => self.conv("cvt.rzi.s32.f32", RegKind::F32, RegKind::S32)?, // f2i
            0x8C => self.conv("cvt.rzi.s64.f32", RegKind::F32, RegKind::S64)?, // f2l
            0x8D => self.conv("cvt.f64.f32", RegKind::F32, RegKind::F64)?,   // f2d
            0x8E => self.conv("cvt.rzi.s32.f64", RegKind::F64, RegKind::S32)?, // d2i
            0x8F => self.conv("cvt.rzi.s64.f64", RegKind::F64, RegKind::S64)?, // d2l
            0x90 => self.conv("cvt.rn.f32.f64", RegKind::F64, RegKind::F32)?, // d2f
            0x91 => self.conv_truncate_i32(8)?,                                // i2b
            0x92 => self.conv_truncate_i32(16)?,                               // i2c (unsigned 16)
            0x93 => self.conv_truncate_i32(16)?,                               // i2s
            // ── compares (push int -1/0/1) ──────────────────────────
            0x94 => self.cmp_long_or_float(RegKind::S64, false)?,
            0x95 => self.cmp_long_or_float(RegKind::F32, true)?, // fcmpl: NaN → -1
            0x96 => self.cmp_long_or_float(RegKind::F32, true)?, // fcmpg: NaN → +1 — we treat the same; semantics OK for the analyzer subset
            0x97 => self.cmp_long_or_float(RegKind::F64, true)?,
            0x98 => self.cmp_long_or_float(RegKind::F64, true)?,
            // ── branches ────────────────────────────────────────────
            0x99..=0xA4 => {
                // if* / if_icmp* — we accept these only at the loop
                // entry guard (which `walk_body` handles by skipping)
                // or as a no-op inside straight-line code. For the
                // counted-loop case the body walker never hits the
                // exit-if because we bracket the walk to skip it. So
                // an `if*` reaching `emit_op` means the loop pattern
                // didn't match; reject.
                return Err(LoweringError::UnsupportedNode(format!(
                    "if-branch opcode 0x{op:02x} at pc={pc} outside canonical-loop guard \
                     position (non-canonical control flow)"
                )));
            }
            0xA7 | 0xC8 => {
                // goto / goto_w — if it targets the loop header, it's
                // the back branch; mark and stop. Anything else is
                // non-canonical.
                let target = if op == 0xA7 {
                    let off =
                        i16::from_be_bytes([self.bytes[pc + 1], self.bytes[pc + 2]]) as i32;
                    (pc as i32 + off) as usize
                } else {
                    let off = i32::from_be_bytes([
                        self.bytes[pc + 1],
                        self.bytes[pc + 2],
                        self.bytes[pc + 3],
                        self.bytes[pc + 4],
                    ]);
                    (pc as i32 + off) as usize
                };
                if let Some(li) = loop_info {
                    if target == li.header_pc {
                        self.hit_back_branch = true;
                        return Ok(());
                    }
                }
                return Err(LoweringError::UnsupportedNode(format!(
                    "unconditional branch at pc={pc} → {target} is not a back-branch to \
                     the loop header"
                )));
            }
            0xC6 | 0xC7 => {
                return Err(LoweringError::UnsupportedNode(
                    "ifnull/ifnonnull are not supported in element-wise kernels".into(),
                ));
            }
            // ── returns ─────────────────────────────────────────────
            0xAC => self.scalar_return(RegKind::S32, ".s32")?, // ireturn
            0xAD => self.scalar_return(RegKind::S64, ".s64")?, // lreturn
            0xAE => self.scalar_return(RegKind::F32, ".f32")?, // freturn
            0xAF => self.scalar_return(RegKind::F64, ".f64")?, // dreturn
            0xB1 => {
                writeln!(self.body, "    bra L_done;").unwrap();
            }
            // ── getfield (Phase 9 #2 — receiver-access pattern) ─────
            // Only the `aload_0; getfield <cp>` shape reaches this
            // arm: the analyzer rejects other `getfield` shapes via
            // `Reason::FieldAccess`, and the pre-`getfield` opcode is
            // always `aload_0`. We pop the sentinel pushed by
            // `aload_0` and replace it with the pre-loaded
            // `pthis_<i>_ptr` register matching this cp_index.
            0xB4 => {
                let cp_index =
                    u16::from_be_bytes([self.bytes[pc + 1], self.bytes[pc + 2]]);
                let top = self.stack.pop()?;
                if top.name != self.this_sentinel_name {
                    return Err(LoweringError::UnsupportedNode(format!(
                        "getfield #{cp_index} at pc={pc} on an operand stack \
                         value that is not the `this` sentinel — only \
                         `aload_0; getfield <primitive-array>` is supported"
                    )));
                }
                let (_, reg) = self
                    .this_field_ptr_reg
                    .get(&cp_index)
                    .cloned()
                    .ok_or_else(|| {
                        LoweringError::Internal(format!(
                            "getfield #{cp_index} at pc={pc}: cp_index was not \
                             pre-bound by `bind_param_locals` — analyzer/\
                             emitter signature mismatch"
                        ))
                    })?;
                self.stack.push(reg);
            }
            // ── arraylength ─────────────────────────────────────────
            0xBE => self.arraylength()?,
            // ── wide ────────────────────────────────────────────────
            0xC4 => {
                let sub = self.bytes[pc + 1];
                let idx = u16::from_be_bytes([self.bytes[pc + 2], self.bytes[pc + 3]]);
                match sub {
                    0x15 => self.iload(idx)?,
                    0x16 => self.lload(idx)?,
                    0x17 => self.fload(idx)?,
                    0x18 => self.dload(idx)?,
                    0x19 => self.aload(idx)?,
                    0x36 => self.istore(idx)?,
                    0x37 => self.lstore(idx)?,
                    0x38 => self.fstore(idx)?,
                    0x39 => self.dstore(idx)?,
                    0x3A => self.astore(idx)?,
                    0x84 => {
                        let delta = i16::from_be_bytes([self.bytes[pc + 4], self.bytes[pc + 5]])
                            as i32;
                        self.iinc(idx, delta)?;
                    }
                    _ => {
                        return Err(LoweringError::UnsupportedNode(format!(
                            "wide prefix on unsupported sub-opcode 0x{sub:02x}"
                        )));
                    }
                }
            }
            // ── nop ─────────────────────────────────────────────────
            0x00 => {}
            // ── reject everything else ──────────────────────────────
            _ => {
                return Err(LoweringError::UnsupportedNode(format!(
                    "opcode 0x{op:02x} not implemented in lowering",
                )));
            }
        }
        Ok(())
    }

    // ─────────────────── local-variable helpers ─────────────────────

    fn iload(&mut self, slot: u16) -> Result<(), LoweringError> {
        // If this slot is the induction variable, substitute tid.
        if Some(slot) == self.iv_slot {
            let tid = self
                .tid_reg
                .clone()
                .ok_or_else(|| LoweringError::Internal("tid not initialised".into()))?;
            self.stack.push(tid);
            return Ok(());
        }
        let r = self
            .locals
            .get(slot as usize)
            .cloned()
            .ok_or_else(|| {
                LoweringError::UnsupportedNode(format!(
                    "iload from uninitialised local {slot}"
                ))
            })?;
        self.stack.push(r);
        Ok(())
    }

    fn lload(&mut self, slot: u16) -> Result<(), LoweringError> {
        let r = self
            .locals
            .get(slot as usize)
            .cloned()
            .ok_or_else(|| {
                LoweringError::UnsupportedNode(format!(
                    "lload from uninitialised local {slot}"
                ))
            })?;
        self.stack.push(r);
        Ok(())
    }

    fn fload(&mut self, slot: u16) -> Result<(), LoweringError> {
        self.lload(slot)
    }

    fn dload(&mut self, slot: u16) -> Result<(), LoweringError> {
        self.lload(slot)
    }

    fn aload(&mut self, slot: u16) -> Result<(), LoweringError> {
        let r = self
            .locals
            .get(slot as usize)
            .cloned()
            .ok_or_else(|| {
                LoweringError::UnsupportedNode(format!(
                    "aload from uninitialised local {slot} \
                     (non-parameter object references are not supported)"
                ))
            })?;
        self.stack.push(r);
        Ok(())
    }

    fn istore(&mut self, slot: u16) -> Result<(), LoweringError> {
        let r = self.stack.pop()?;
        self.locals.set(slot as usize, r);
        Ok(())
    }

    fn lstore(&mut self, slot: u16) -> Result<(), LoweringError> {
        let r = self.stack.pop()?;
        self.locals.set(slot as usize, r);
        Ok(())
    }

    fn fstore(&mut self, slot: u16) -> Result<(), LoweringError> {
        let r = self.stack.pop()?;
        self.locals.set(slot as usize, r);
        Ok(())
    }

    fn dstore(&mut self, slot: u16) -> Result<(), LoweringError> {
        let r = self.stack.pop()?;
        self.locals.set(slot as usize, r);
        Ok(())
    }

    fn astore(&mut self, slot: u16) -> Result<(), LoweringError> {
        let r = self.stack.pop()?;
        self.locals.set(slot as usize, r);
        Ok(())
    }

    fn iinc(&mut self, slot: u16, delta: i32) -> Result<(), LoweringError> {
        if Some(slot) == self.iv_slot {
            // Induction variable iinc is implicit in the parallel-for
            // dispatch — skip.
            return Ok(());
        }
        let cur = self
            .locals
            .get(slot as usize)
            .cloned()
            .ok_or_else(|| {
                LoweringError::UnsupportedNode(format!(
                    "iinc on uninitialised local {slot}"
                ))
            })?;
        let next = self.regs.fresh_reg(RegKind::S32);
        writeln!(
            self.body,
            "    add.s32 {}, {}, {};",
            next.name, cur.name, delta
        )
        .unwrap();
        self.locals.set(slot as usize, next);
        Ok(())
    }

    // ─────────────────── arithmetic helpers ─────────────────────────

    fn binop_i32(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::S32);
        writeln!(self.body, "    {} {}, {}, {};", mnemonic, r.name, a.name, b.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn binop_i64(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::S64);
        writeln!(self.body, "    {} {}, {}, {};", mnemonic, r.name, a.name, b.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn binop_f32(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::F32);
        // div/rem on f32 need rounding mode in PTX; rn.f32 family.
        let m = match mnemonic {
            "div.f32" => "div.rn.f32",
            "rem.f32" => "rem.f32", // PTX has no direct rem; closest is fmod through cvts. Emit literal — analyzer rarely permits frem in practice.
            other => other,
        };
        writeln!(self.body, "    {} {}, {}, {};", m, r.name, a.name, b.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn binop_f64(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::F64);
        let m = match mnemonic {
            "div.f64" => "div.rn.f64",
            other => other,
        };
        writeln!(self.body, "    {} {}, {}, {};", m, r.name, a.name, b.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn unop_i32(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::S32);
        writeln!(self.body, "    {} {}, {};", mnemonic, r.name, a.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn unop_i64(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::S64);
        writeln!(self.body, "    {} {}, {};", mnemonic, r.name, a.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn unop_f32(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::F32);
        writeln!(self.body, "    {} {}, {};", mnemonic, r.name, a.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn unop_f64(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::F64);
        writeln!(self.body, "    {} {}, {};", mnemonic, r.name, a.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn shift_i32(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        let b = self.stack.pop()?; // shift count (s32)
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::S32);
        // PTX shifts want a u32 shift amount — `setp.b32` reinterpret
        // is implicit when the source register is already 32-bit. We
        // just emit directly; PTX accepts either signedness for the
        // count operand.
        writeln!(self.body, "    {} {}, {}, {};", mnemonic, r.name, a.name, b.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn shift_i64(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        // JVM long-shift count is an int — top of stack is s32.
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::S64);
        writeln!(self.body, "    {} {}, {}, {};", mnemonic, r.name, a.name, b.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    // ─────────────────── conversions ────────────────────────────────

    fn conv(
        &mut self,
        mnemonic: &str,
        _from: RegKind,
        to: RegKind,
    ) -> Result<(), LoweringError> {
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(to);
        writeln!(self.body, "    {} {}, {};", mnemonic, r.name, a.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn conv_truncate_i32(&mut self, bits: u32) -> Result<(), LoweringError> {
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::S32);
        // Sign-extend an N-bit value back to s32 by emitting:
        //   shl  tmp, a, (32 - N)
        //   shr  r,   tmp, (32 - N)
        let amt = 32 - bits;
        let tmp = self.regs.fresh_reg(RegKind::S32);
        writeln!(self.body, "    shl.b32 {}, {}, {};", tmp.name, a.name, amt).unwrap();
        writeln!(self.body, "    shr.s32 {}, {}, {};", r.name, tmp.name, amt).unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn cmp_long_or_float(
        &mut self,
        _kind: RegKind,
        _is_float: bool,
    ) -> Result<(), LoweringError> {
        // JVM `*cmp*` pushes -1/0/+1 on the int stack. We never expect
        // these inside the canonical-loop subset (they appear with the
        // following if*); reject so the caller falls back.
        Err(LoweringError::UnsupportedNode(
            "*cmp* opcodes are not supported in the element-wise lowering".into(),
        ))
    }

    // ─────────────────── array ops ──────────────────────────────────

    /// Find the array param backing the top-of-stack array reference.
    /// Returns (key, kind). The reference itself must have been
    /// produced by either:
    ///
    /// - `aload` of an array-parameter local — returns
    ///   `ArrayKey::Param(idx)`; the kind is the analyzer's
    ///   `ParamKind` for that slot.
    /// - Phase 9 #2: `aload_0; getfield <cp>` — returns
    ///   `ArrayKey::ThisField(idx)` where `idx` is the dedup'd
    ///   position of `cp` in `this_field_cps_unique`. The kind is
    ///   `ParamKind::I32Array` today (the analyzer's
    ///   `NonStaticReceiverMisuse` path only admits int[] fields in
    ///   the first cut; widening to other primitive arrays is a
    ///   follow-up that needs analyzer + marshaller coordination).
    fn array_param_of(&self, array_reg: &Reg) -> Result<(ArrayKey, ParamKind), LoweringError> {
        // Each array-parameter binding recorded its U64 register name
        // in `param_ptr_reg[i]`. The reference flows through `aload`
        // unchanged (we just clone the local's Reg), so the runtime
        // register name on the simulated stack equals that initial
        // binding.
        for (i, k) in self.sig.param_kinds.iter().enumerate() {
            if !k.is_array() {
                continue;
            }
            if !self.param_ptr_reg[i].is_empty() && self.param_ptr_reg[i] == array_reg.name {
                return Ok((ArrayKey::Param(i), *k));
            }
        }
        // Phase 9 #2 — `getfield` of a `this.<field>` primitive array.
        // The `this_field_ptr_reg` map indexes by cp_index; iterate
        // the entries and match by register name. The entry's
        // `(this_field_index, _)` tuple gives us the `pthis_<i>_*`
        // parameter index directly.
        for (_cp, (idx, reg)) in &self.this_field_ptr_reg {
            if reg.name == array_reg.name {
                return Ok((ArrayKey::ThisField(*idx), ParamKind::I32Array));
            }
        }
        Err(LoweringError::UnsupportedNode(
            "array reference does not flow from a method parameter (\
             non-parameter array references are not supported)"
                .into(),
        ))
    }

    fn array_load(
        &mut self,
        elem_kind: RegKind,
        suffix: &str,
        elem_size: usize,
    ) -> Result<(), LoweringError> {
        let index = self.stack.pop()?;
        let array_ref = self.stack.pop()?;
        let (array_key, _kind) = self.array_param_of(&array_ref)?;
        let len_param = array_key.len_param_name();
        self.emit_bounds_check(&index, &len_param);
        let offset = self.regs.fresh_reg(RegKind::U64);
        let byte_idx = self.regs.fresh_reg(RegKind::U64);
        let addr = self.regs.fresh_reg(RegKind::U64);
        let result = self.regs.fresh_reg(elem_kind);
        writeln!(
            self.body,
            "    cvt.s64.s32 {}, {};",
            byte_idx.name, index.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    mul.lo.s64 {}, {}, {};",
            offset.name, byte_idx.name, elem_size
        )
        .unwrap();
        writeln!(
            self.body,
            "    add.u64 {}, {}, {};",
            addr.name, array_ref.name, offset.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    ld.global{} {}, [{}];",
            suffix, result.name, addr.name
        )
        .unwrap();
        self.stack.push(result);
        Ok(())
    }

    fn array_load_byte(&mut self) -> Result<(), LoweringError> {
        let index = self.stack.pop()?;
        let array_ref = self.stack.pop()?;
        let (array_key, _kind) = self.array_param_of(&array_ref)?;
        let len_param = array_key.len_param_name();
        self.emit_bounds_check(&index, &len_param);
        let byte_idx = self.regs.fresh_reg(RegKind::U64);
        let addr = self.regs.fresh_reg(RegKind::U64);
        let raw = self.regs.fresh_reg(RegKind::S32);
        let result = self.regs.fresh_reg(RegKind::S32);
        writeln!(
            self.body,
            "    cvt.s64.s32 {}, {};",
            byte_idx.name, index.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    add.u64 {}, {}, {};",
            addr.name, array_ref.name, byte_idx.name
        )
        .unwrap();
        // Load signed 8-bit, widen to 32-bit (sign-extended) — Java baload semantics.
        writeln!(
            self.body,
            "    ld.global.s8 {}, [{}];",
            raw.name, addr.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    cvt.s32.s8 {}, {};",
            result.name, raw.name
        )
        .unwrap();
        self.stack.push(result);
        Ok(())
    }

    fn array_load_char(&mut self) -> Result<(), LoweringError> {
        self.array_load_16(true)
    }

    fn array_load_short(&mut self) -> Result<(), LoweringError> {
        self.array_load_16(false)
    }

    fn array_load_16(&mut self, is_char: bool) -> Result<(), LoweringError> {
        let index = self.stack.pop()?;
        let array_ref = self.stack.pop()?;
        let (array_key, _kind) = self.array_param_of(&array_ref)?;
        let len_param = array_key.len_param_name();
        self.emit_bounds_check(&index, &len_param);
        let byte_idx = self.regs.fresh_reg(RegKind::U64);
        let offset = self.regs.fresh_reg(RegKind::U64);
        let addr = self.regs.fresh_reg(RegKind::U64);
        let raw = self.regs.fresh_reg(RegKind::S32);
        let result = self.regs.fresh_reg(RegKind::S32);
        writeln!(
            self.body,
            "    cvt.s64.s32 {}, {};",
            byte_idx.name, index.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    mul.lo.s64 {}, {}, 2;",
            offset.name, byte_idx.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    add.u64 {}, {}, {};",
            addr.name, array_ref.name, offset.name
        )
        .unwrap();
        let load_suffix = if is_char { "u16" } else { "s16" };
        let cvt = if is_char { "cvt.u32.u16" } else { "cvt.s32.s16" };
        writeln!(
            self.body,
            "    ld.global.{} {}, [{}];",
            load_suffix, raw.name, addr.name
        )
        .unwrap();
        writeln!(self.body, "    {} {}, {};", cvt, result.name, raw.name).unwrap();
        self.stack.push(result);
        Ok(())
    }

    fn array_store(
        &mut self,
        _elem_kind: RegKind,
        suffix: &str,
        elem_size: usize,
    ) -> Result<(), LoweringError> {
        let value = self.stack.pop()?;
        let index = self.stack.pop()?;
        let array_ref = self.stack.pop()?;
        let (array_key, _kind) = self.array_param_of(&array_ref)?;
        let len_param = array_key.len_param_name();
        self.emit_bounds_check(&index, &len_param);
        let offset = self.regs.fresh_reg(RegKind::U64);
        let byte_idx = self.regs.fresh_reg(RegKind::U64);
        let addr = self.regs.fresh_reg(RegKind::U64);
        writeln!(
            self.body,
            "    cvt.s64.s32 {}, {};",
            byte_idx.name, index.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    mul.lo.s64 {}, {}, {};",
            offset.name, byte_idx.name, elem_size
        )
        .unwrap();
        writeln!(
            self.body,
            "    add.u64 {}, {}, {};",
            addr.name, array_ref.name, offset.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    st.global{} [{}], {};",
            suffix, addr.name, value.name
        )
        .unwrap();
        Ok(())
    }

    fn array_store_byte(&mut self) -> Result<(), LoweringError> {
        let value = self.stack.pop()?;
        let index = self.stack.pop()?;
        let array_ref = self.stack.pop()?;
        let (array_key, _kind) = self.array_param_of(&array_ref)?;
        let len_param = array_key.len_param_name();
        self.emit_bounds_check(&index, &len_param);
        let byte_idx = self.regs.fresh_reg(RegKind::U64);
        let addr = self.regs.fresh_reg(RegKind::U64);
        writeln!(
            self.body,
            "    cvt.s64.s32 {}, {};",
            byte_idx.name, index.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    add.u64 {}, {}, {};",
            addr.name, array_ref.name, byte_idx.name
        )
        .unwrap();
        // Truncate value to s8 implicitly via st.global.s8 (PTX OK).
        writeln!(
            self.body,
            "    st.global.s8 [{}], {};",
            addr.name, value.name
        )
        .unwrap();
        Ok(())
    }

    fn array_store_char_or_short(&mut self, elem_size: usize) -> Result<(), LoweringError> {
        let value = self.stack.pop()?;
        let index = self.stack.pop()?;
        let array_ref = self.stack.pop()?;
        let (array_key, _kind) = self.array_param_of(&array_ref)?;
        let len_param = array_key.len_param_name();
        self.emit_bounds_check(&index, &len_param);
        let byte_idx = self.regs.fresh_reg(RegKind::U64);
        let offset = self.regs.fresh_reg(RegKind::U64);
        let addr = self.regs.fresh_reg(RegKind::U64);
        writeln!(
            self.body,
            "    cvt.s64.s32 {}, {};",
            byte_idx.name, index.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    mul.lo.s64 {}, {}, {};",
            offset.name, byte_idx.name, elem_size
        )
        .unwrap();
        writeln!(
            self.body,
            "    add.u64 {}, {}, {};",
            addr.name, array_ref.name, offset.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    st.global.s16 [{}], {};",
            addr.name, value.name
        )
        .unwrap();
        Ok(())
    }

    fn arraylength(&mut self) -> Result<(), LoweringError> {
        let array_ref = self.stack.pop()?;
        let (array_key, _kind) = self.array_param_of(&array_ref)?;
        let len_param = array_key.len_param_name();
        let r = self.regs.fresh_reg(RegKind::S32);
        writeln!(
            self.body,
            "    ld.param.s32 {}, [{len_param}];",
            r.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn scalar_return(&mut self, _kind: RegKind, suffix: &str) -> Result<(), LoweringError> {
        let value = self.stack.pop()?;
        // Write the value through ret_ptr. Each thread writes the same
        // location — for a single-thread scalar return that's the
        // intent; for a per-thread "result" it overwrites. Marshalling
        // pre-allocates a one-element output buffer.
        let ret_ptr = self.regs.fresh_reg(RegKind::U64);
        writeln!(
            self.body,
            "    ld.param.u64 {}, [ret_ptr];",
            ret_ptr.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    st.global{} [{}], {};",
            suffix, ret_ptr.name, value.name
        )
        .unwrap();
        self.ret_value_reg = Some(value);
        writeln!(self.body, "    bra L_done;").unwrap();
        Ok(())
    }
}

/// Find the loop bound for the canonical pattern by inspecting the
/// instruction immediately before the exit-if. The bound is either:
///
/// * `iload N` of a local that was set from `arraylength` of an array
///   parameter → returns the array parameter's `_len` name.
/// * `iload N` of a constant local → unsupported (we'd need const
///   propagation).
/// * `iconst_*` / `bipush` / `sipush` literal → returns an inline literal.
///
/// On unsupported shapes, returns Err.
pub(crate) fn locate_bound(
    bytes: &[u8],
    loop_info: &CountedLoop,
    sig: &KernelSignature,
) -> Result<BoundSource, LoweringError> {
    // The exit-if's two operands sit on the stack just before it.
    // We walk back from exit_if_pc looking at the two preceding
    // instructions; they must form an `iload iv` followed by either
    // `iload bound_slot` or a const-push.
    let mut prev_ops = Vec::new();
    let mut pc = loop_info.header_pc;
    while pc < loop_info.exit_if_pc {
        prev_ops.push(pc);
        pc += instr_size(bytes, pc)?;
    }
    if prev_ops.len() < 2 {
        return Err(LoweringError::UnsupportedNode(
            "loop header does not have two stack-producing instructions before exit-if".into(),
        ));
    }
    let bound_pc = prev_ops[prev_ops.len() - 1];
    let bound_op = bytes[bound_pc];
    let bound_local = match bound_op {
        0x1A..=0x1D => Some((bound_op - 0x1A) as u16),
        0x15 => Some(bytes[bound_pc + 1] as u16),
        0xC4 if bytes[bound_pc + 1] == 0x15 => Some(u16::from_be_bytes([
            bytes[bound_pc + 2],
            bytes[bound_pc + 3],
        ])),
        _ => None,
    };
    if let Some(slot) = bound_local {
        // The bound local must have been initialised pre-loop. Walk
        // the pre-loop and find the most recent store to `slot`.
        if let Some(src) = pre_loop_bound_source(bytes, loop_info.header_pc, slot, sig)? {
            return Ok(src);
        }
        return Err(LoweringError::UnsupportedNode(format!(
            "loop bound is local {slot}, but its definition is not an \
             arraylength of a method parameter — bound is unknown"
        )));
    }
    // Literal bound — bipush / sipush / iconst_*
    let lit = match bound_op {
        0x02..=0x08 => Some(bound_op as i32 - 0x03),
        0x10 => Some(bytes[bound_pc + 1] as i8 as i32),
        0x11 => Some(i16::from_be_bytes([bytes[bound_pc + 1], bytes[bound_pc + 2]]) as i32),
        _ => None,
    };
    if let Some(v) = lit {
        return Ok(BoundSource::Literal(v));
    }
    Err(LoweringError::UnsupportedNode(format!(
        "loop bound at pc={bound_pc} (op 0x{bound_op:02x}) is not a recognized \
         literal or arraylength-of-parameter form"
    )))
}

/// Source of a value sitting on the operand stack: either an `aload`
/// of a parameter local, or (Phase 9 #2) the result of an
/// `aload_0; getfield <cp>` chain accessing a `this.<field>`
/// primitive array.
#[derive(Clone, Copy, Debug)]
enum ArraySrc {
    /// `aload <slot>` of a parameter local. The marshaller-side
    /// length lives at `pN_len` where N is computed from the slot.
    ParamLocal(u16),
    /// `aload_0; getfield <cp_index>`. The marshaller-side length
    /// lives at `pthis_<i>_len` where `i` is the dedup'd position of
    /// `cp_index` in `KernelSignature::this_field_cps`.
    ThisField(u16),
}

/// Try to identify the source of a bound-local: scan the pre-loop
/// bytecode for either:
///
///   - `aload K; arraylength; istore slot`  (static or non-static
///     methods accessing an array parameter), or
///   - `aload_0; getfield <cp>; arraylength; istore slot`  (Phase 9
///     #2: non-static method's bound source is `this.<field>.length`).
///
/// Returns the matching `BoundSource` on success.
fn pre_loop_bound_source(
    bytes: &[u8],
    header_pc: usize,
    slot: u16,
    sig: &KernelSignature,
) -> Result<Option<BoundSource>, LoweringError> {
    let mut pc = 0usize;
    // The most recent `aload <K>` we observed — None unless the
    // immediately previous opcode was an aload.
    let mut last_aload_local: Option<u16> = None;
    // Whether the most recent stack-producing opcode is `aload_0`
    // (the receiver). Tracked separately from `last_aload_local`
    // because a `getfield` consuming the receiver transforms the
    // stack-top into a `this.<field>` value with different bounds
    // semantics.
    let mut last_aload_was_zero = false;
    // After `getfield <cp>` immediately following `aload_0`, the
    // stack top is the named field — record its cp_index so a
    // subsequent `arraylength` knows it came from a this-field.
    let mut last_getfield_this_cp: Option<u16> = None;
    // After `arraylength`, the s32 length value is on top of the
    // stack; remember its origin so the matching `istore slot`
    // can resolve to either `ParamLocal` or `ThisField`.
    let mut last_arraylength_src: Option<ArraySrc> = None;
    while pc < header_pc {
        let op = bytes[pc];
        let size = instr_size(bytes, pc)?;
        match op {
            // ── aload variants ───────────────────────────────────────
            0x2A => {
                // aload_0 — the receiver in non-static methods.
                last_aload_local = Some(0);
                last_aload_was_zero = true;
                last_getfield_this_cp = None;
                pc += size;
                continue;
            }
            0x2B..=0x2D => {
                last_aload_local = Some((op - 0x2A) as u16);
                last_aload_was_zero = false;
                last_getfield_this_cp = None;
                pc += size;
                continue;
            }
            0x19 => {
                let k = bytes[pc + 1] as u16;
                last_aload_local = Some(k);
                last_aload_was_zero = k == 0;
                last_getfield_this_cp = None;
                pc += size;
                continue;
            }
            0xC4 if bytes[pc + 1] == 0x19 => {
                let k = u16::from_be_bytes([bytes[pc + 2], bytes[pc + 3]]);
                last_aload_local = Some(k);
                last_aload_was_zero = k == 0;
                last_getfield_this_cp = None;
                pc += size;
                continue;
            }
            // ── getfield (Phase 9 #2) ────────────────────────────────
            // Only the `aload_0; getfield` shape contributes to a
            // bound-source. Any other getfield (post-non-zero aload,
            // post-arithmetic) would have already failed the
            // analyzer's `Reason::NonStaticReceiverMisuse` /
            // `Reason::FieldAccess` checks.
            0xB4 => {
                if last_aload_was_zero {
                    let cp = u16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]);
                    last_getfield_this_cp = Some(cp);
                } else {
                    last_getfield_this_cp = None;
                }
                last_aload_local = None;
                last_aload_was_zero = false;
                pc += size;
                continue;
            }
            // ── arraylength ──────────────────────────────────────────
            0xBE => {
                if let Some(cp) = last_getfield_this_cp {
                    last_arraylength_src = Some(ArraySrc::ThisField(cp));
                } else if let Some(local) = last_aload_local {
                    last_arraylength_src = Some(ArraySrc::ParamLocal(local));
                } else {
                    last_arraylength_src = None;
                }
                last_aload_local = None;
                last_aload_was_zero = false;
                last_getfield_this_cp = None;
                pc += size;
                continue;
            }
            // ── istore <slot> variants ───────────────────────────────
            0x36 => {
                let target_slot = bytes[pc + 1] as u16;
                if target_slot == slot {
                    if let Some(src) = last_arraylength_src {
                        return Ok(Some(resolve_bound_src(sig, src)?));
                    }
                }
                last_arraylength_src = None;
            }
            0x3B..=0x3E => {
                let target_slot = (op - 0x3B) as u16;
                if target_slot == slot {
                    if let Some(src) = last_arraylength_src {
                        return Ok(Some(resolve_bound_src(sig, src)?));
                    }
                }
                last_arraylength_src = None;
            }
            0xC4 if bytes[pc + 1] == 0x36 => {
                let target_slot = u16::from_be_bytes([bytes[pc + 2], bytes[pc + 3]]);
                if target_slot == slot {
                    if let Some(src) = last_arraylength_src {
                        return Ok(Some(resolve_bound_src(sig, src)?));
                    }
                }
                last_arraylength_src = None;
            }
            _ => {}
        }
        // Every other instruction clears the aload/getfield/arraylength
        // tracking state — only contiguous sequences are recognised.
        last_aload_local = None;
        last_aload_was_zero = false;
        last_getfield_this_cp = None;
        pc += size;
    }
    Ok(None)
}

/// Resolve an [`ArraySrc`] to its [`BoundSource`] flavour using the
/// kernel signature: a parameter slot maps to `ParamLen(i)`; a
/// this-field cp_index maps to `ThisFieldLen(i)` where `i` is the
/// dedup'd position of the cp_index in
/// [`KernelSignature::this_field_cps`].
fn resolve_bound_src(sig: &KernelSignature, src: ArraySrc) -> Result<BoundSource, LoweringError> {
    match src {
        ArraySrc::ParamLocal(slot) => param_len_for_local(sig, slot),
        ArraySrc::ThisField(cp) => {
            // Dedup the analyzer's body-encounter list in the same
            // order `Emitter::new` does (first-encounter order
            // preserved). The marshaller mirrors this dedup so the
            // index here is stable between emitter and runtime.
            let mut idx: Option<usize> = None;
            let mut seen: Vec<u16> = Vec::with_capacity(sig.this_field_cps.len());
            for &c in &sig.this_field_cps {
                if !seen.contains(&c) {
                    if c == cp {
                        idx = Some(seen.len());
                        break;
                    }
                    seen.push(c);
                }
            }
            let idx = idx.ok_or_else(|| {
                LoweringError::Internal(format!(
                    "this-field bound source cp_index #{cp} not in \
                     KernelSignature::this_field_cps — analyzer/emitter mismatch"
                ))
            })?;
            Ok(BoundSource::ThisFieldLen(idx))
        }
    }
}

fn param_len_for_local(sig: &KernelSignature, slot: u16) -> Result<BoundSource, LoweringError> {
    let mut s = 0u16;
    for (i, k) in sig.param_kinds.iter().enumerate() {
        if s == slot {
            if !k.is_array() {
                return Err(LoweringError::UnsupportedNode(
                    "arraylength of a non-array parameter".into(),
                ));
            }
            return Ok(BoundSource::ParamLen(i));
        }
        s += match k {
            ParamKind::I64 | ParamKind::F64 => 2,
            ParamKind::Void => 0,
            _ => 1,
        };
    }
    Err(LoweringError::UnsupportedNode(format!(
        "arraylength source local {slot} is not a parameter"
    )))
}

/// Where the loop bound comes from.
#[derive(Clone, Debug)]
pub(crate) enum BoundSource {
    /// `pN_len` for parameter index N.
    ParamLen(usize),
    /// Phase 9 #2 — `pthis_<i>_len` for a this-field array access.
    /// The index is the dedup'd position of the field's cp_index in
    /// [`KernelSignature::this_field_cps`].
    ThisFieldLen(usize),
    /// A literal compile-time constant.
    Literal(i32),
}
