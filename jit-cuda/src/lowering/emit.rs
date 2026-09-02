// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use crate::analyzer::{resolve_math_intrinsic, MathIntrinsic, ParamKind};
use crate::emitter::{LoweringError, RegKind};
use crate::lowering::loop_recog::{instr_size, CountedLoop, NestedLoop};
use crate::signature::KernelSignature;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
    pub b16_count: u32,
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
            RegKind::B16 => (&mut self.b16_count, "rs"),
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

    /// Like [`fresh_reg`] but lets the caller force the JVM
    /// category-2 (wide) flag.
    ///
    /// AUDIT 2026-05-24 (C31): array references live in `RegKind::U64`
    /// PTX registers (64-bit device pointer width), but on the JVM
    /// operand stack they are category-1 — `pop2`, `dup2_x1`, etc. must
    /// treat them as a single slot, not two. The default `fresh_reg`
    /// infers wideness from `RegKind` alone (U64/S64/F64 → `wide=true`),
    /// which is correct for `long`/`double` JVM values but wrong for
    /// array refs. Use this constructor at array-ref binding sites
    /// (`bind_param_locals`) so `pop2` of two stacked array refs pops
    /// two slots instead of one.
    pub fn fresh_reg_with_wide(&mut self, kind: RegKind, wide: bool) -> Reg {
        Reg {
            kind,
            name: self.fresh(kind),
            wide,
        }
    }
}

/// Simulated JVM operand stack of typed registers.
#[derive(Clone, Default)]
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
#[derive(Clone, Default)]
pub(crate) struct Locals(Vec<Option<Reg>>);

/// JVM execution state at a basic-block entry.  A branch target owns one
/// canonical register for every live local and operand-stack value; every
/// predecessor copies its state into those registers before branching.  This
/// is the small, explicit phi representation used by [`Emitter::walk_cfg`].
#[derive(Clone)]
struct BlockState {
    stack: OpStack,
    locals: Locals,
}

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

/// A `cond ? then : else` diamond the emitter can turn into two
/// unconditional computations plus a `selp`.
///
/// Built by [`Emitter::plan_if_conversion`], which documents the shape and
/// why only this one is recognised.
#[derive(Debug)]
struct IfConversion {
    /// First instruction of the fall-through (predicate FALSE) arm.
    then_start: usize,
    /// PC of that arm's terminating `goto <join>`.
    then_goto_pc: usize,
    /// First instruction of the branch-taken (predicate TRUE) arm, which
    /// is also where the fall-through arm's block ends.
    else_start: usize,
    /// Where the two arms reconverge.
    join: usize,
}

/// Whether a speculated arm's emitted PTX may run unconditionally, and
/// what it costs to do so.
///
/// See [`Emitter::try_emit_if_converted`] for why this reads the PTX and
/// not the bytecode. `None` means "not speculatable at any price";
/// `Some(cost)` is a weighted instruction count the caller compares
/// against its budget.
///
/// # Why a weighted count and not a plain one
///
/// Both arms run on every lane, so converting trades a branch the warp
/// might never have diverged on for arithmetic it certainly executes.
/// Whether that is a win depends on what the arithmetic COSTS, not on how
/// many lines it takes: the ray tracer's
/// `disc > 0f ? (float) Math.sqrt(disc) : 1e9f` is one instruction per
/// arm, and one of them is `sqrt.rn.f32`, which `ptxas` expands into a
/// `MUFU.RSQ` and a Newton-Raphson chain. Speculating it on a warp whose
/// lanes all miss the sphere -- which is most of a frame's background --
/// buys a removed `BSSY`/`BSYNC` pair and pays for a square root nobody
/// wanted. Measured on this kernel, the plain-count version of this
/// screen made the compute half 56% SLOWER.
fn speculation_cost(text: &str) -> Option<u32> {
    /// What one instruction counts as. `1` for ordinary ALU work; the
    /// multi-instruction expansions are what a plain count gets wrong.
    fn weight(t: &str) -> u32 {
        const EXPENSIVE: [&str; 7] = [
            "sqrt.", "div.rn", "div.rz", "div.rm", "div.rp", "rcp.", "ex2.",
        ];
        if EXPENSIVE.iter().any(|m| t.contains(m)) {
            16
        } else {
            1
        }
    }
    let mut cost = 0u32;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.ends_with(':')
            || t.starts_with('@')
            || t.contains("bra ")
            || t.contains("ld.global")
            || t.contains("st.global")
            || t.contains("ld.shared")
            || t.contains("st.shared")
            || t.contains("atom.")
            || t.contains("red.")
            || t.contains("bar.")
            || t.contains("call")
            || t.contains("ret;")
        {
            return None;
        }
        cost = cost.saturating_add(weight(t));
    }
    Some(cost)
}

/// The budget `CRATONVM_GPU_IF_CONVERT=1` selects: the least-bad setting
/// the sweep found, which is a tie with the feature off rather than a win.
/// Everything cheaper and everything dearer measured worse.
const DEFAULT_IF_CONVERSION_BUDGET: u32 = 8;

/// The if-conversion budget this process runs with: the largest weighted
/// arm-pair cost [`speculation_cost`] may report and still convert. `0`
/// converts nothing.
///
/// **Off by default, and that is a measured decision rather than caution.**
/// The transform does what it was built to do -- on the ray tracer it takes
/// PTX branches from 51 to 21 and SASS branch machinery from 17.4% of the
/// kernel to 15.4% -- and it makes that kernel SLOWER, by 56% of its compute
/// half, in 8 of 8 interleaved rounds against a transfer-floor control. A
/// branch a warp does not diverge on is nearly free; `selp` makes every lane
/// compute both arms. Sweeping the budget found no value that wins: 8 is a
/// tie with off and every other setting is worse. See
/// `gpu/raytracer-vs-tornadovm-RESOLVED-20260821.md`'s residual pass.
///
/// It is kept, and kept reachable, because that is one kernel. A shape with
/// cheap arms and heavy divergence is exactly what it is for, and the flags
/// are how someone measures whether theirs is one.
///
/// `CRATONVM_GPU_IF_CONVERT=1` turns it on at [`DEFAULT_IF_CONVERSION_BUDGET`];
/// `CRATONVM_GPU_IF_CONVERT_MAX_OPS=<n>` turns it on at `n`.
fn if_conversion_budget_from_flags() -> u32 {
    static BUDGET: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        if let Some(n) = cratonvm_types::flags::runtime_var("CRATONVM_GPU_IF_CONVERT_MAX_OPS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
        {
            return n;
        }
        let on = cratonvm_types::flags::runtime_var("CRATONVM_GPU_IF_CONVERT")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(false);
        if on {
            DEFAULT_IF_CONVERSION_BUDGET
        } else {
            0
        }
    })
}

/// What the kernel's dispatch guard has proved about one register.
///
/// The guard every counted-loop kernel opens with — `if (tid >= bound)
/// return;` — is a range check the body then repeats at every array
/// access. For the overwhelmingly common shape, `arr[i]` where `i` is
/// the induction variable, the repeat is provably redundant: the guard
/// already established `0 <= tid < bound`, and if `bound` IS that
/// array's length there is nothing left to check.
///
/// The `0 <=` half is not free, and getting it wrong would be a heap
/// overwrite rather than a slow kernel. `tid` is
/// `ctaid.x * ntid.x + tid.x + tid_base`, computed in `u32` and
/// reinterpreted as `s32`; a launch covering `i32::MAX` elements creates
/// up to `block - 1` threads past `2^31`, whose `s32` index is NEGATIVE.
/// A signed `tid >= bound` test lets those through — today they are
/// caught by the per-access `index < 0` check and deopt to the CPU. So
/// the guard has to prove non-negativity itself before the per-access
/// check can be removed; see [`Emitter::emit_loop_guard`] for the two
/// forms it emits and which one each bound gets.
#[derive(Clone, Debug)]
pub(crate) struct IndexProof {
    /// The register proved to satisfy `0 <= index < bound`.
    pub index: String,
    /// The register holding that exclusive upper bound.
    pub bound: String,
    /// Which parameter's `.length` the bound IS, when it is one — the
    /// case where an access to that same parameter needs no precondition
    /// at all.
    pub bound_is_param_len: Option<usize>,
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
    /// For each parameter index: the cached `pN_len` kernel-parameter
    /// name. Built once in `bind_param_locals` (mirroring
    /// `param_ptr_reg`) so each array op can index it instead of
    /// re-running `format!("p{param_idx}_len")` on every access.
    pub param_len_name: Vec<String>,
    /// For each parameter index: the register its `pN_len` was loaded
    /// into, once, in the prologue.
    ///
    /// AUDIT 2026-09-02: `emit_bounds_check` used to emit its own
    /// `ld.param.s32` for the length on EVERY array access, plus a
    /// fresh `mov.s32 %r, 0` for the constant it compared against. Both
    /// are loop-invariant and neither depends on the access, so a kernel
    /// touching three arrays paid six redundant instructions per
    /// element. Loaded once here, in `bind_param_locals`, which
    /// dominates every access by construction.
    pub param_len_reg: Vec<Option<Reg>>,
    /// What the dispatch guard has proved about the induction register.
    ///
    /// See [`IndexProof`] and `emit_bounds_check` for how a proof
    /// retires an access's bounds check entirely.
    pub(crate) index_proofs: Vec<IndexProof>,
    /// Per-array preconditions discovered while walking the body, to be
    /// spliced in at [`Emitter::prologue_splice_at`].
    ///
    /// A bounds check is retired by proving `bound <= pN_len` once for
    /// the whole kernel rather than `index < pN_len` at every access.
    /// Which arrays need such a precondition is only known after the
    /// walk — and emitting one for every array parameter up front would
    /// deopt kernels that never index that array at the loop bound, which
    /// is a behaviour change, not an optimisation. So they accumulate
    /// here and are spliced back to a dominating position at the end.
    pub(crate) bounds_prologue: String,
    /// Byte offset in `body` immediately after the dispatch guard, where
    /// [`Emitter::bounds_prologue`] is spliced by `into_body`.
    ///
    /// `None` for a shape with no guard (straight-line kernels), where
    /// nothing is proved and nothing is spliced.
    pub(crate) prologue_splice_at: Option<usize>,
    /// `(param index, bound register name)` pairs that already have a
    /// precondition in `bounds_prologue`, so a kernel with twenty
    /// accesses to one array emits one.
    pub(crate) param_len_guarded: Vec<(usize, String)>,
    /// Set while `try_emit_if_converted` is speculatively emitting an
    /// arm whose text it may discard.
    ///
    /// A discarded arm must leave nothing behind — that is why the
    /// speculation already saves and restores `writes_param_mask`,
    /// `reads_param_mask` and `used_bounds_label`. Rather than add
    /// `bounds_prologue` and `param_len_guarded` to that list, this flag
    /// stops the one thing that writes them from running at all during
    /// speculation. That is also the right ANSWER and not merely the
    /// simpler bookkeeping: a speculated arm is code that may or may not
    /// have executed, and hoisting a precondition out of it would apply
    /// it to threads that were never going to take the branch.
    pub(crate) speculating: bool,
    /// Local slot of the loop induction variable (if we are in a loop).
    pub iv_slot: Option<u16>,
    /// 2-D nested-loop follow-up: local slot of the INNER loop's
    /// induction variable, when lowering a nested (2-D) counted loop
    /// (see [`NestedLoop`]). `None` for a straight-line or single-loop
    /// kernel. Checked by `iload`/`iinc` alongside `iv_slot` so both `i`
    /// and `j` substitute to their own computed index register.
    pub iv_slot_inner: Option<u16>,
    /// Loop-bound register; populated once we emit the prologue. Used
    /// only for the early-out at kernel entry.
    pub bound_reg: Option<Reg>,
    /// Register holding `tid` (computed once in the prologue). For a
    /// nested (2-D) loop this becomes the outer loop's recovered index
    /// `i` once `emit_nested_loop_guard_and_decompose` runs.
    pub tid_reg: Option<Reg>,
    /// 2-D nested-loop follow-up: register holding the inner loop's
    /// recovered index `j` (`tid % C`), populated by
    /// `emit_nested_loop_guard_and_decompose`. `None` outside a nested
    /// loop.
    pub tid_reg_inner: Option<Reg>,
    /// Label used to signal an out-of-bounds bounds-check failure.
    /// Emitted once at kernel epilogue; jump from each `*aload`/`*astore`.
    pub bounds_fail_label: String,
    /// Set to `true` the first time we emit a bounds check; controls
    /// whether the failure block needs to be emitted at the end.
    pub used_bounds_label: bool,
    /// Weighted-instruction budget for the branch-to-`selp` if-conversion;
    /// `0` converts nothing. Taken from the flags by [`Emitter::new`], and
    /// overridable per call so a test can exercise the transform without
    /// depending on the process-wide default -- which is `0`.
    pub if_convert_budget: u32,
    /// Set to `true` when a `goto` to the loop header is encountered;
    /// caller should stop emitting at that point.
    pub hit_back_branch: bool,
    /// Local slot used for the kernel result return (scalar return only).
    /// Populated by the post-loop walker when it sees the matching `*return`.
    pub ret_value_reg: Option<Reg>,
    /// Phase 10 #2 — bit-set of parameter indices the body writes to
    /// via `*astore`. Each `array_store*` arm in the opcode dispatch
    /// resolves the array reference back to its parameter via
    /// `array_param_of` and sets the matching bit. `lower_method`
    /// reads this after the walk and stamps it into the returned
    /// `PtxKernel` (and from there into `KernelSignature`) so the
    /// marshaller can skip the post-launch D→H copy for read-only
    /// array inputs — see `KernelSignature::writes_param_mask`.
    pub writes_param_mask: u64,
    /// Bit-set of parameter indices the body READS via an `*aload`.
    ///
    /// The mirror of `writes_param_mask`, and it exists for the chunked
    /// writeback. Committing a chunk into the Java array as its event
    /// fires puts results there BEFORE the bounds-failure flag has been
    /// read. That is harmless when the CPU re-run recomputes and rewrites
    /// those same elements, but NOT harmless if the kernel reads an array
    /// it also writes, because then a partial commit would change the
    /// re-run's own input. `writes & !reads` is therefore the set of
    /// arrays a chunked dispatch may safely stream out early.
    ///
    /// `arraylength` deliberately does not set a bit: reading a length
    /// does not depend on element contents.
    pub reads_param_mask: u64,
    /// AUDIT C31 follow-up (2026-07-11): the class's constant pool, so
    /// `ldc`/`ldc_w`/`ldc2_w` (opcodes 0x12/0x13/0x14) can resolve their
    /// operand to an `Integer`/`Float`/`Long`/`Double` value and push it
    /// as a PTX immediate — see `ldc`/`ldc2_w` below. `None` when the
    /// caller went through the CP-free `lowering::lower_method` entry
    /// point, in which case those three opcodes fail to lower with
    /// `LoweringError::UnsupportedNode` (they never reach `emit_op` in
    /// practice, because the CP-free analyzer entry points already
    /// reject any method containing them — see
    /// `analyzer::Reason::LoadConstant`).
    pub cp: Option<&'a ConstantPool>,
}

impl<'a> Emitter<'a> {
    pub fn new(bytes: &'a [u8], sig: &'a KernelSignature, cp: Option<&'a ConstantPool>) -> Self {
        Self {
            bytes,
            body: String::new(),
            regs: RegPool::default(),
            stack: OpStack::default(),
            locals: Locals::default(),
            sig,
            param_ptr_reg: vec![String::new(); sig.param_kinds.len()],
            param_len_name: (0..sig.param_kinds.len())
                .map(|i| format!("p{i}_len"))
                .collect(),
            param_len_reg: vec![None; sig.param_kinds.len()],
            index_proofs: Vec::new(),
            bounds_prologue: String::new(),
            prologue_splice_at: None,
            param_len_guarded: Vec::new(),
            speculating: false,
            iv_slot: None,
            iv_slot_inner: None,
            bound_reg: None,
            tid_reg: None,
            tid_reg_inner: None,
            bounds_fail_label: "L_bounds_fail".into(),
            used_bounds_label: false,
            if_convert_budget: if_conversion_budget_from_flags(),
            hit_back_branch: false,
            ret_value_reg: None,
            writes_param_mask: 0,
            reads_param_mask: 0,
            cp,
        }
    }

    /// Initialize local slot bindings from method parameters: slot 0 is
    /// param 0, slot 1 is param 1 (or slot 2 for a long/double param 0),
    /// etc. Each array parameter binds the slot to its `pN_ptr` U64 reg;
    /// each scalar parameter binds to a freshly-loaded value reg.
    pub fn bind_param_locals(&mut self) -> Result<(), LoweringError> {
        let mut slot = 0usize;
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
                    // AUDIT 2026-05-24 (C31): array references are JVM
                    // category-1 (one operand-stack slot each), but they
                    // live in a `RegKind::U64` PTX register because the
                    // pointer is 64-bit on device. The default
                    // `fresh_reg` would tag them `wide=true` from the
                    // U64 kind alone, which would break `pop2` of two
                    // stacked array refs (it would pop one slot instead
                    // of two). Force `wide=false` here.
                    let r = self.regs.fresh_reg_with_wide(RegKind::U64, false);
                    writeln!(self.body, "    ld.param.u64 {}, [p{i}_ptr];", r.name).unwrap();
                    self.param_ptr_reg[i] = r.name.clone();
                    self.locals.set(slot, r);
                    // AUDIT 2026-09-02: hoist the length load. Every
                    // bounds check used to emit its own
                    // `ld.param.s32 [pN_len]`; it is loop-invariant, so
                    // one load here — at a point that dominates every
                    // access by construction — serves the whole kernel.
                    // ptxas removes it again if the array is never
                    // indexed, which is the only case it costs anything.
                    let len = self.regs.fresh_reg(RegKind::S32);
                    writeln!(self.body, "    ld.param.s32 {}, [p{i}_len];", len.name).unwrap();
                    self.param_len_reg[i] = Some(len);
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
        // Add the launch's base index. Zero for a whole-array launch; the
        // chunk's first element for a chunked one, which is how several
        // launches can cover disjoint slices of one iteration space while
        // every thread still knows its global index. See the `tid_base`
        // parameter in `lowering::ptx_params`.
        let base = self.regs.fresh_reg(RegKind::S32);
        let tid_final = self.regs.fresh_reg(RegKind::S32);
        writeln!(self.body, "    ld.param.s32 {}, [tid_base];", base.name).unwrap();
        writeln!(
            self.body,
            "    add.s32 {}, {}, {};",
            tid_final.name, tid_s32.name, base.name
        )
        .unwrap();
        self.tid_reg = Some(tid_final);
    }

    /// Fold a counted loop's compile-time-constant start value `K`
    /// (`for (i = K; ...)`, `K >= 0` — see
    /// `crate::lowering::loop_recog`'s module doc comment for why a
    /// negative `K` is rejected before lowering ever reaches this
    /// point) into `self.tid_reg`, turning the raw physical thread id
    /// `emit_tid` computed into the SIMT element index `tid + K`.
    ///
    /// Call this exactly once, immediately after `emit_tid`, before
    /// anything else reads `self.tid_reg` — every downstream consumer
    /// (`emit_loop_guard`'s `tid >= bound` comparison and the `iload
    /// iv_slot` substitution in `iload` below) then transparently sees
    /// `tid + K` without needing to know `K` exists. This mirrors
    /// exactly how the Java loop itself works: the loop variable `i`
    /// starts at `K`, so the element each thread handles is `K` higher
    /// than its raw thread index.
    ///
    /// `K == 0` (the overwhelmingly common case, and the only case that
    /// existed before this feature) is a deliberate no-op: emitting
    /// `add.s32 %rN, tid, 0;` would be correct but wasteful, and — more
    /// importantly — would change the exact PTX text every existing
    /// start-at-zero fixture test asserts against. Skipping the `add`
    /// for `K == 0` keeps this change byte-for-byte invisible to every
    /// kernel that doesn't use it.
    ///
    /// # Why over-launching is safe for `K >= 0` but not `K < 0`
    ///
    /// The host (`vm/src/runtime/offload.rs`) sizes the CUDA grid off
    /// the largest array argument's length, i.e. at least `bound`
    /// threads. With `K >= 0` the Java loop's trip count is `bound - K
    /// <= bound`, so a `bound`-sized launch always has threads to
    /// spare — `emit_loop_guard`'s `tid + K >= bound` check simply
    /// retires them immediately, same as it already does for the
    /// `K == 0` case when the grid is over-sized. (This is exactly why
    /// `loop_recog::resolve_start_value` rejects `K < 0`: that would
    /// need *more* than `bound` threads.)
    ///
    /// # Why the output array's untouched `0..K` prefix is safe
    ///
    /// Elements `0..K` of any array the loop writes are never touched
    /// by any thread (every thread's index is `tid + K >= K`) — which
    /// matches Java exactly (the loop body never runs for `i < K`).
    /// That's only a safe *device-side* story because
    /// `vm/src/runtime/offload.rs::marshal_array_arg` uploads the
    /// **entire** array to the device before every kernel launch,
    /// unconditionally, regardless of whether the kernel writes it —
    /// there is no "allocate output uninitialised" fast path. So the
    /// device buffer's `0..K` prefix starts out byte-identical to the
    /// host array's, the kernel never writes it, and the post-launch
    /// D→H writeback (`gpu_marshal::download_obj_*`, also an
    /// unconditional full-length copy) writes those same untouched
    /// bytes straight back — a no-op from the host's point of view.
    pub fn apply_loop_start_offset(&mut self, start: i32) {
        if start == 0 {
            return;
        }
        let tid = self.tid_reg.clone().expect("emit_tid was called");
        let r = self.regs.fresh_reg(RegKind::S32);
        writeln!(
            self.body,
            "    add.s32 {}, {}, {};",
            r.name, tid.name, start
        )
        .unwrap();
        self.tid_reg = Some(r);
    }

    /// Emit the dispatch guard — `if (tid outside [0, bound)) return;` —
    /// and record what it proves.
    ///
    /// This `tid < bound` dispatch (one thread runs iff `0 <= tid <
    /// bound`) is the GPU analogue of the canonical Java loop
    /// `for (int i = 0; i < bound; i++)`. It is *only* correct for that
    /// exact shape: stride +1, strict-`<` exit, and a start value that is
    /// either `0` or has already been folded into `tid` by
    /// `apply_loop_start_offset` (so `tid` here may really mean
    /// `tid + K`). The loop recognizer
    /// ([`crate::lowering::loop_recog::detect_loop`]) has already rejected
    /// every other shape — `<=`/`!=`/`>`/`>=` exits, non-unit strides,
    /// negative or non-constant starts — before emission reaches here.
    /// The `debug_assert!`s below pin that contract: if a future change to
    /// the recognizer ever lets a non-canonical loop through, a debug
    /// build trips here instead of silently corrupting data.
    ///
    /// # Two forms, and why the guard has to prove `tid >= 0`
    ///
    /// AUDIT 2026-09-02: the guard used to be a single `setp.ge.s32`,
    /// which retires every thread whose index is too large and says
    /// nothing about one whose index is negative. That was fine while
    /// every access re-checked `index < 0` for itself. It stops being
    /// fine the moment [`Emitter::prove_index_within_param`] uses the
    /// guard to retire those checks — and the negative case is real, not
    /// theoretical: `tid` is `ctaid.x * ntid.x + tid.x + tid_base`
    /// computed in `u32` and reinterpreted as `s32`, so a launch covering
    /// close to `i32::MAX` elements creates up to `block - 1` threads
    /// past `2^31`, whose `s32` index is negative.
    ///
    /// * `bound_nonneg` — the bound is an array length, or a literal that
    ///   is not negative. One `setp.ge.u32` then decides both halves:
    ///   reinterpreted as `u32` a negative `tid` is at least `2^31`,
    ///   which exceeds any bound that fits in a non-negative `s32`. Same
    ///   instruction count as before, strictly more proved.
    /// * otherwise — the bound is an `int` PARAMETER, which may legally be
    ///   negative (`for (i = 0; i < n; i++)` with `n < 0` runs zero
    ///   times). An unsigned compare would read that as an enormous bound
    ///   and run the body, so this form keeps the signed comparison and
    ///   pays one extra compare-and-branch to reject a negative `tid`
    ///   explicitly.
    ///
    /// Retiring an out-of-range thread to `L_done` rather than to the
    /// bounds-failure block is correct in both forms: such a thread
    /// corresponds to no loop iteration at all, so there is nothing for
    /// the CPU to re-run.
    ///
    /// `bound_is_param_len` names the parameter whose `.length` the bound
    /// IS, when it is one — the case where an access to that same
    /// parameter needs no precondition either.
    pub fn emit_loop_guard(
        &mut self,
        bound: &Reg,
        loop_info: &CountedLoop,
        bound_is_param_len: Option<usize>,
        bound_nonneg: bool,
    ) {
        debug_assert_eq!(
            loop_info.exit_op, 0xA2,
            "emit_loop_guard: counted loop reached emission with a \
             non-`if_icmpge` exit (0x{:02x}) — loop_recog must reject it",
            loop_info.exit_op
        );
        debug_assert_eq!(
            loop_info.iv_stride, 1,
            "emit_loop_guard: counted loop reached emission with stride {} \
             — loop_recog must reject any non-unit stride",
            loop_info.iv_stride
        );
        let tid = self.tid_reg.clone().expect("emit_tid was called");
        if bound_nonneg {
            let p = self.regs.fresh_reg(RegKind::Pred);
            writeln!(
                self.body,
                "    setp.ge.u32 {}, {}, {};",
                p.name, tid.name, bound.name
            )
            .unwrap();
            writeln!(self.body, "    @{} bra L_done;", p.name).unwrap();
        } else {
            let neg = self.regs.fresh_reg(RegKind::Pred);
            writeln!(self.body, "    setp.lt.s32 {}, {}, 0;", neg.name, tid.name).unwrap();
            writeln!(self.body, "    @{} bra L_done;", neg.name).unwrap();
            let p = self.regs.fresh_reg(RegKind::Pred);
            writeln!(
                self.body,
                "    setp.ge.s32 {}, {}, {};",
                p.name, tid.name, bound.name
            )
            .unwrap();
            writeln!(self.body, "    @{} bra L_done;", p.name).unwrap();
        }
        self.index_proofs.push(IndexProof {
            index: tid.name.clone(),
            bound: bound.name.clone(),
            bound_is_param_len,
        });
        // Everything emitted from here on is dominated by the guard, so
        // this is where a per-array precondition discovered during the
        // walk gets spliced back to. See `Emitter::bounds_prologue`.
        self.prologue_splice_at = Some(self.body.len());
    }

    /// 2-D nested-loop follow-up: emit the nested-loop guard and index
    /// decomposition — `total = R * C; if (tid >= total) ret; i = tid /
    /// C; j = tid % C;`.
    ///
    /// This is the 2-D analogue of [`emit_loop_guard`] — see that
    /// function's doc comment for the general "one thread per
    /// iteration" dispatch model this extends. Here one thread handles
    /// one `(i, j)` pair from the flattened `R*C`-element iteration
    /// space instead of one `i` from a `bound`-element space.
    ///
    /// # Why the division is always safe (no zero-divisor guard needed)
    ///
    /// `div.s32`/`rem.s32` by `inner_bound` only execute for threads
    /// that already passed the `tid >= total` guard, i.e. `tid < R*C`.
    /// If `inner_bound (C) <= 0` then `total = R*C <= 0`, and a
    /// non-negative `tid` can never satisfy `tid < total` — every
    /// thread branches to `L_done` before reaching the division. So a
    /// thread only ever divides by a `C` it has already proven is
    /// `> 0`.
    ///
    /// Call this once, after both bound registers are materialised.
    /// Sets `self.iv_slot`/`self.tid_reg` (outer `i`) and
    /// `self.iv_slot_inner`/`self.tid_reg_inner` (inner `j`) so every
    /// subsequent `iload` of either induction variable substitutes
    /// transparently — the 2-D counterpart of how the single-loop path
    /// pairs `emit_loop_guard` with `self.iv_slot`/`self.tid_reg`.
    pub fn emit_nested_loop_guard_and_decompose(
        &mut self,
        outer_bound: &Reg,
        inner_bound: &Reg,
        nested: &NestedLoop,
    ) {
        debug_assert_eq!(
            nested.outer.exit_op, 0xA2,
            "emit_nested_loop_guard_and_decompose: outer loop reached emission with a \
             non-`if_icmpge` exit — loop_recog must reject it"
        );
        debug_assert_eq!(
            nested.outer.iv_stride, 1,
            "emit_nested_loop_guard_and_decompose: outer loop reached emission with a \
             non-unit stride — loop_recog must reject it"
        );
        debug_assert_eq!(
            nested.inner.exit_op, 0xA2,
            "emit_nested_loop_guard_and_decompose: inner loop reached emission with a \
             non-`if_icmpge` exit — loop_recog must reject it"
        );
        debug_assert_eq!(
            nested.inner.iv_stride, 1,
            "emit_nested_loop_guard_and_decompose: inner loop reached emission with a \
             non-unit stride — loop_recog must reject it"
        );
        let tid = self.tid_reg.clone().expect("emit_tid was called");
        let total = self.regs.fresh_reg(RegKind::S32);
        writeln!(
            self.body,
            "    mul.lo.s32 {}, {}, {};",
            total.name, outer_bound.name, inner_bound.name
        )
        .unwrap();
        let p = self.regs.fresh_reg(RegKind::Pred);
        writeln!(
            self.body,
            "    setp.ge.s32 {}, {}, {};",
            p.name, tid.name, total.name
        )
        .unwrap();
        writeln!(self.body, "    @{} bra L_done;", p.name).unwrap();
        let i = self.regs.fresh_reg(RegKind::S32);
        let j = self.regs.fresh_reg(RegKind::S32);
        writeln!(
            self.body,
            "    div.s32 {}, {}, {};",
            i.name, tid.name, inner_bound.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    rem.s32 {}, {}, {};",
            j.name, tid.name, inner_bound.name
        )
        .unwrap();
        self.iv_slot = Some(nested.outer.iv_slot);
        self.tid_reg = Some(i);
        self.iv_slot_inner = Some(nested.inner.iv_slot);
        self.tid_reg_inner = Some(j);
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

    /// Emit the Java bounds check for `index` against parameter
    /// `param_idx`'s length: on failure, jump to the shared failure
    /// block, which raises the flag the host reads to deopt.
    ///
    /// # One unsigned compare, not two signed ones
    ///
    /// Java's check is `index < 0 || index >= len`, and this used to
    /// emit it literally: materialise `0`, compare, branch, compare
    /// again, branch again — six instructions and two branches per
    /// access. A single unsigned compare decides both halves at once.
    /// Reinterpreted as `u32` a negative index is at least `2^31`, and
    /// an array length is at most `i32::MAX`, so `index >=u len` holds
    /// exactly when `index < 0 || index >= len` does. This is the same
    /// identity HotSpot's own bounds check has used for decades.
    ///
    /// It rests on `pN_len` being non-negative, which the marshaller
    /// guarantees: it is a JVM array length, read from the object
    /// header, and the JVM has no negative-length array.
    ///
    /// # …and often no compare at all
    ///
    /// Before emitting anything, ask whether the dispatch guard already
    /// settled it. See [`IndexProof`] for what the guard proves and
    /// [`Emitter::prove_index_within_param`] for the two ways a proof
    /// discharges an access.
    fn emit_bounds_check(&mut self, index: &Reg, param_idx: usize) {
        if self.prove_index_within_param(index, param_idx) {
            return;
        }
        self.used_bounds_label = true;
        let len = self.param_len_operand(param_idx);
        let p = self.regs.fresh_reg(RegKind::Pred);
        // `.u32` on registers declared `.s32`: PTX's relaxed
        // source-operand typing makes signed and unsigned integers of the
        // same width compatible, and the reinterpretation is the point.
        writeln!(
            self.body,
            "    setp.ge.u32 {}, {}, {};",
            p.name, index.name, len
        )
        .unwrap();
        writeln!(self.body, "    @{} bra {};", p.name, self.bounds_fail_label).unwrap();
    }

    /// The operand naming parameter `param_idx`'s length — the register
    /// `bind_param_locals` hoisted it into, or a freshly-loaded one for a
    /// caller that never ran `bind_param_locals` (only the unit tests
    /// that drive `walk` directly do that).
    fn param_len_operand(&mut self, param_idx: usize) -> String {
        if let Some(r) = &self.param_len_reg[param_idx] {
            return r.name.clone();
        }
        let r = self.regs.fresh_reg(RegKind::S32);
        writeln!(
            self.body,
            "    ld.param.s32 {}, [{}];",
            r.name, self.param_len_name[param_idx]
        )
        .unwrap();
        let name = r.name.clone();
        self.param_len_reg[param_idx] = Some(r);
        name
    }

    /// Whether the dispatch guard already proves `index` is in range for
    /// parameter `param_idx`, so the per-access check can be omitted.
    ///
    /// Two ways it can:
    ///
    /// 1. **The bound IS this array's length.** `for (i = 0; i < a.length;
    ///    i++) a[i] = …` — the guard retired every thread whose index
    ///    falls outside `[0, a.length)` before the body started. Nothing
    ///    to emit here, and nothing once per kernel either.
    /// 2. **The bound is no larger than this array's length.** The other
    ///    arrays in `out[i] = a[i] + b[i]`, whose lengths the guard says
    ///    nothing about. One precondition per array — `if (a.length <
    ///    bound) deopt;` — discharges every access to it, however many
    ///    there are. It is spliced back to a dominating position (see
    ///    [`Emitter::bounds_prologue`]) rather than left where it was
    ///    discovered.
    ///
    /// Case 2 replaces a per-access check with a weaker precondition, not
    /// a stronger one: it deopts exactly the launches on which some
    /// access WOULD have gone out of bounds, and Java's semantics for
    /// those is an `ArrayIndexOutOfBoundsException` anyway. It fires
    /// earlier than the failing access, which the deopt contract already
    /// permits — the host discards the device result and re-runs the
    /// whole method on the CPU, where the exception is raised at the
    /// right place with the right message.
    ///
    /// Deliberately NOT applied to the nested (2-D) shape. There the
    /// guard is `tid < R * C` with the product computed in `s32`, so a
    /// large `R` and `C` overflow it and a "proof" resting on it would
    /// admit indices past the end. Fixing that means widening the
    /// multiply, which changes the nested guard's shape and its
    /// measurements; until then the nested kernels keep the (now
    /// single-compare) check. `emit_nested_loop_guard_and_decompose`
    /// registers no proof, so this returns `false` there by construction.
    fn prove_index_within_param(&mut self, index: &Reg, param_idx: usize) -> bool {
        let Some(proof) = self
            .index_proofs
            .iter()
            .find(|p| p.index == index.name)
            .cloned()
        else {
            return false;
        };
        if proof.bound_is_param_len == Some(param_idx) {
            return true;
        }
        if self
            .param_len_guarded
            .iter()
            .any(|(i, b)| *i == param_idx && *b == proof.bound)
        {
            return true;
        }
        if !self.unconditional_since_guard() {
            // The access sits behind a branch, so it may not execute.
            // A precondition hoisted out of it would deopt launches on
            // which Java would have thrown nothing at all — a silent
            // performance cliff rather than a wrong answer, but a
            // behaviour change either way. Fall back to the per-access
            // check, which is still one unsigned compare rather than the
            // six this used to be.
            //
            // Case 1 above does NOT need this guard: it emits nothing.
            // It is a statement about the VALUE of the index, which the
            // dispatch guard established for every thread that reached
            // the body, and that stays true wherever inside the body the
            // access happens to sit.
            return false;
        }
        let Some(len) = self.param_len_reg[param_idx].clone() else {
            // No hoisted length means `bind_param_locals` never ran — a
            // unit test driving `walk` directly. Nothing to prove
            // against; fall back to the per-access check.
            return false;
        };
        // `bound <= pN_len` plus the guard's `0 <= index < bound` gives
        // `0 <= index < pN_len`, which is the check being retired.
        // Signed, not unsigned: a `ParamScalar` bound may legitimately be
        // negative (a loop that runs zero times), and the guard's own
        // form already accounts for that.
        self.used_bounds_label = true;
        let p = self.regs.fresh_reg(RegKind::Pred);
        writeln!(
            self.bounds_prologue,
            "    setp.lt.s32 {}, {}, {};",
            p.name, len.name, proof.bound
        )
        .unwrap();
        writeln!(
            self.bounds_prologue,
            "    @{} bra {};",
            p.name, self.bounds_fail_label
        )
        .unwrap();
        self.param_len_guarded.push((param_idx, proof.bound));
        true
    }

    /// Whether everything emitted since the dispatch guard is
    /// straight-line, so the current emission point is reached by every
    /// thread that passed the guard.
    ///
    /// Answered by looking for a branch in the body text rather than by
    /// tracking block structure, for the same reason
    /// `check_every_branch_has_its_label` and `speculation_cost` read the
    /// text: there is no IR to ask. It is sound in the direction that
    /// matters — any conditional control flow in this emitter goes
    /// through a `bra`, so no branch means no condition. A `bra` that is
    /// unconditional (the end-of-body jump to `L_done`) reads as
    /// conditional here and merely gives up an optimisation.
    ///
    /// `false` during speculation and for shapes with no guard at all.
    fn unconditional_since_guard(&self) -> bool {
        if self.speculating {
            return false;
        }
        match self.prologue_splice_at {
            Some(at) if at <= self.body.len() => !self.body[at..].contains("bra "),
            _ => false,
        }
    }

    /// Emit the address of element `index` of the array based at `base`,
    /// and return the register holding it.
    ///
    /// One instruction. `mad.wide.s32 %rd, %r, <size>, %rd_base`
    /// sign-extends the 32-bit index, multiplies it by the element size
    /// and adds the 64-bit base, which is a single `IMAD.WIDE` in SASS.
    ///
    /// AUDIT 2026-09-02: this used to be three — `cvt.s64.s32` to widen,
    /// `mul.lo.s64` to scale, `add.u64` to offset — plus two 64-bit
    /// temporaries live across them. ptxas contracts that pattern
    /// sometimes and is not obliged to, and certainly not across the
    /// bounds-check branches that used to sit in between. The register
    /// saving matters as much as the instruction count: 64-bit
    /// temporaries are what push an index-heavy kernel into spilling.
    fn emit_element_addr(&mut self, index: &Reg, base: &Reg, elem_size: usize) -> Reg {
        let addr = self.regs.fresh_reg(RegKind::U64);
        writeln!(
            self.body,
            "    mad.wide.s32 {}, {}, {}, {};",
            addr.name, index.name, elem_size, base.name
        )
        .unwrap();
        addr
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
            // The `Code` attribute may end mid-instruction (truncated or
            // malformed class file). `emit_op` reads operand bytes via
            // raw `self.bytes[pc + 1..]` indexing, so verify the whole
            // instruction fits before dispatching — otherwise a short
            // method panics with an out-of-bounds slice index.
            if pc + size > self.bytes.len() {
                return Err(LoweringError::UnsupportedNode(format!(
                    "instruction at pc={pc} (op 0x{op:02x}, size {size}) runs past \
                     the end of the bytecode ({} bytes) — truncated Code attribute",
                    self.bytes.len()
                )));
            }
            self.emit_op(op, pc, loop_info)?;
            pc += size;
        }
        Ok(())
    }

    /// Lower the control-flow graph inside one counted-loop body.
    ///
    /// The surrounding counted loop is still executed once per CUDA thread,
    /// so its canonical back-edge is deliberately outside `end`.  Within that
    /// boundary this is a real CFG walk, not a pattern matcher: it discovers
    /// basic blocks, emits PTX labels and predicate branches, and reconciles
    /// JVM locals/operand-stack values at every join.
    ///
    /// **Interior back-edges are lowered as real PTX loops.** A back-edge
    /// target is a block the walker reaches first through a forward edge, so
    /// by the time the back-edge is emitted the target's canonical registers
    /// already exist and its label is already in the body text: the edge is
    /// then exactly the same "copy the live state into the join's registers,
    /// then branch" it is for a forward edge, and those registers *are* the
    /// loop's phis. That is what lets one thread run `out[i] = sum_j ...`
    /// sequentially while the outer loop still gives one thread per `i`
    /// ([`crate::lowering::loop_recog::classify_outer_parallel_loop`]).
    ///
    /// Two things a back-edge needs that a forward edge does not, both
    /// enforced below: the target block must actually have been emitted
    /// (an unreachable block is skipped, and branching to a label that was
    /// never written is a `ptxas` error rather than a CPU fallback), and the
    /// state copy must be simultaneous — see [`Self::copy_matching_state`].
    pub fn walk_cfg(
        &mut self,
        start: usize,
        end: usize,
        loop_info: &CountedLoop,
    ) -> Result<(), LoweringError> {
        let starts = self.cfg_block_starts(start, end, loop_info)?;
        let starts: Vec<usize> = starts.into_iter().collect();
        let back_targets = self.back_edge_targets(start, end, loop_info)?;
        let mut emitted = BTreeSet::<usize>::new();
        let mut entries = BTreeMap::<usize, BlockState>::new();
        entries.insert(
            start,
            BlockState {
                stack: self.stack.clone(),
                locals: self.locals.clone(),
            },
        );

        // Blocks the if-converter consumed as the arms of a diamond.
        // They are emitted inline, unconditionally, ahead of the join --
        // so the outer walk must not emit them a second time.
        let mut if_converted = BTreeSet::<usize>::new();
        for (index, &block_start) in starts.iter().enumerate() {
            let block_end = starts.get(index + 1).copied().unwrap_or(end);
            if if_converted.contains(&block_start) {
                continue;
            }
            let Some(entry) = entries.get(&block_start).cloned() else {
                // Valid class files can retain unreachable bytecode.  It has
                // no incoming edge in the admitted subgraph, so emitting it
                // would manufacture an execution state the JVM never has.
                continue;
            };
            self.stack = entry.stack;
            self.locals = entry.locals;
            // The body's first block normally needs no label — control
            // falls into it from the loop guard.  It needs one as soon as
            // an interior back-edge targets it, which happens when the
            // whole outer body *is* the inner loop's header.
            if block_start != start || back_targets.contains(&block_start) {
                writeln!(self.body, "L_body_{block_start}:").unwrap();
            }
            emitted.insert(block_start);

            let mut pc = block_start;
            let mut terminated = false;
            while pc < block_end {
                let op = self.bytes[pc];
                let size = instr_size(self.bytes, pc)?;
                if pc + size > self.bytes.len() || pc + size > end {
                    return Err(LoweringError::UnsupportedNode(format!(
                        "instruction at pc={pc} crosses a control-flow block boundary"
                    )));
                }
                let next = pc + size;
                match op {
                    0x99..=0xA4 | 0xC6 | 0xC7 => {
                        let target = self.branch_target(pc, op)?;
                        Self::check_back_edge(pc, target, &emitted, &entries)?;
                        let predicate = self.emit_branch_predicate(op)?;
                        // A `cond ? a : b` whose two arms are short and
                        // pure becomes two unconditional computations and
                        // one `selp`, with no branch and therefore no warp
                        // reconvergence at all. See `plan_if_conversion`.
                        let mut converted = false;
                        if self.if_convert_budget > 0 {
                            if let Some(plan) = self
                                .plan_if_conversion(pc, next, target, &starts, index, start, end)
                            {
                                if self.try_emit_if_converted(
                                    &plan,
                                    &predicate,
                                    loop_info,
                                    &mut entries,
                                    end,
                                )? {
                                    emitted.insert(plan.then_start);
                                    emitted.insert(plan.else_start);
                                    if_converted.insert(plan.then_start);
                                    if_converted.insert(plan.else_start);
                                    converted = true;
                                }
                            }
                        }
                        if !converted {
                            let state = BlockState {
                                stack: self.stack.clone(),
                                locals: self.locals.clone(),
                            };
                            self.copy_state_to_edge(
                                &state,
                                target,
                                Some((&predicate.name, false)),
                                &mut entries,
                            )?;
                            self.copy_state_to_edge(
                                &state,
                                next,
                                Some((&predicate.name, true)),
                                &mut entries,
                            )?;
                            writeln!(self.body, "    @{} bra L_body_{target};", predicate.name)
                                .unwrap();
                        }
                        terminated = true;
                    }
                    0xA7 | 0xC8 => {
                        let target = self.branch_target(pc, op)?;
                        if target == loop_info.header_pc {
                            // The host-side loop guard already selected one
                            // iteration per thread.  Do not emit the JVM
                            // back-edge or this thread would repeat work.
                            self.hit_back_branch = true;
                        } else {
                            Self::check_back_edge(pc, target, &emitted, &entries)?;
                            let state = BlockState {
                                stack: self.stack.clone(),
                                locals: self.locals.clone(),
                            };
                            self.copy_state_to_edge(&state, target, None, &mut entries)?;
                            writeln!(self.body, "    bra L_body_{target};").unwrap();
                        }
                        terminated = true;
                    }
                    0xAC..=0xB1 => {
                        return Err(LoweringError::UnsupportedNode(format!(
                            "return at pc={pc} inside a GPU loop body is an early exit; \
                             per-iteration kernels only admit the post-loop return"
                        )));
                    }
                    _ => self.emit_op(op, pc, Some(loop_info))?,
                }
                pc = next;
                if terminated {
                    if pc != block_end {
                        return Err(LoweringError::UnsupportedNode(format!(
                            "control transfer at pc={} is not the last instruction in its basic block",
                            pc - size
                        )));
                    }
                    break;
                }
            }
            if !terminated && block_end < end {
                let state = BlockState {
                    stack: self.stack.clone(),
                    locals: self.locals.clone(),
                };
                self.copy_state_to_edge(&state, block_end, None, &mut entries)?;
            }
        }
        Ok(())
    }

    /// Recognise the one control-flow shape worth if-converting, and
    /// nothing else.
    ///
    /// # What this is for
    ///
    /// The GPU kernels in this tree are written branchlessly on purpose
    /// -- ternaries and short-circuit `&&`s rather than `if` statements
    /// -- and the lowerer turned them straight back into branches. On the
    /// ray tracer that came to 67 `BRA` plus 32 `BSSY`/`BSYNC`/`BMOV`
    /// triples, 18% of the kernel's SASS, spent reconverging warps around
    /// arms that are three instructions of float arithmetic. A `selp` has
    /// no reconvergence: every lane computes both sides and keeps one.
    ///
    /// # The shape
    ///
    /// Exactly what `javac` emits for `c ? a : b`:
    ///
    /// ```text
    ///     <cond>; if<cmp> ELSE     <- this block's terminator
    ///     <then expr>
    ///     goto JOIN
    ///   ELSE:
    ///     <else expr>
    ///   JOIN:                      <- fallen into, never branched to
    /// ```
    ///
    /// so: the fall-through block runs from `next` to exactly where the
    /// branch target begins, and ends in a `goto`; the target's block runs
    /// to that `goto`'s destination and falls through to it. Both are
    /// single basic blocks, which is what makes them speculatable at all.
    /// An arm containing its own branch is refused and lowered the old
    /// way, so a NESTED ternary converts its inner diamond and keeps the
    /// outer branch. That is a deliberate boundary rather than an
    /// oversight: every conversion is independently sound, and the
    /// innermost arms are the shortest ones.
    ///
    /// This judges the SHAPE only. Whether the arms may actually run
    /// unconditionally is decided by `try_emit_if_converted`, from the PTX
    /// they emit rather than from the bytecode they came from.
    fn plan_if_conversion(
        &self,
        branch_pc: usize,
        next: usize,
        target: usize,
        starts: &[usize],
        index: usize,
        region_start: usize,
        end: usize,
    ) -> Option<IfConversion> {
        // The branch must end this block and the fall-through must be the
        // next one. `cfg_block_starts` inserts both, so their absence
        // means some other edge already claimed the range.
        if starts.get(index + 1).copied() != Some(next) {
            return None;
        }
        let then_end = starts.get(index + 2).copied()?;
        // The taken arm must begin exactly where the fall-through arm's
        // block ends: a forward branch over the whole then-expression and
        // nothing more.
        if target != then_end {
            return None;
        }
        let else_end = starts.get(index + 3).copied().unwrap_or(end);
        let goto_pc = self.last_instruction_pc(next, then_end)?;
        let goto_op = *self.bytes.get(goto_pc)?;
        if goto_op != 0xA7 && goto_op != 0xC8 {
            return None;
        }
        let join = self.branch_target(goto_pc, goto_op).ok()?;
        if join != else_end || join <= target || join > end {
            return None;
        }
        // Neither arm may carry control flow of its own; the taken arm
        // must have no terminator at all, because it falls into the join.
        if self.has_control_flow(next, goto_pc) || self.has_control_flow(target, join) {
            return None;
        }
        // Neither arm may have a predecessor other than this branch.
        //
        // This is not a refinement, it is the thing that makes the
        // conversion legal at all. A short-circuit `a && b ? x : y`
        // compiles to TWO conditional branches to the SAME else-label,
        // and the second one's diamond passes every test above -- so
        // consuming the else-block would leave the FIRST branch's
        // `bra L_body_<else>` pointing at a label nothing emits. That is
        // PTX `ptxas` rejects, which the VM turns into a blacklisted
        // method and a silent CPU fallback: the kernel would still
        // produce the right answer, just never on the device, and a
        // correctness check would read BIT_IDENTICAL either way.
        if self.has_other_predecessor(region_start, end, branch_pc, next)
            || self.has_other_predecessor(region_start, end, branch_pc, target)
        {
            return None;
        }
        Some(IfConversion {
            then_start: next,
            then_goto_pc: goto_pc,
            else_start: target,
            join,
        })
    }

    /// PC of the last instruction starting in `[from, to)`, or `None` if
    /// the range does not decode into whole instructions.
    fn last_instruction_pc(&self, from: usize, to: usize) -> Option<usize> {
        let mut pc = from;
        let mut last = None;
        while pc < to {
            let size = instr_size(self.bytes, pc).ok()?;
            if size == 0 || pc + size > to {
                return None;
            }
            last = Some(pc);
            pc += size;
        }
        last
    }

    /// Whether any control transfer in `[from, to)` other than the one at
    /// `this_branch` targets `block`.
    ///
    /// Conservative by construction: an undecodable instruction or a
    /// switch (whose table this does not read) answers "yes", which
    /// refuses the conversion rather than risking a dangling label.
    fn has_other_predecessor(
        &self,
        from: usize,
        to: usize,
        this_branch: usize,
        block: usize,
    ) -> bool {
        let mut pc = from;
        while pc < to {
            let Ok(size) = instr_size(self.bytes, pc) else {
                return true;
            };
            if size == 0 || pc + size > to {
                return true;
            }
            let op = self.bytes[pc];
            match op {
                0xAA | 0xAB => return true,
                0x99..=0xA4 | 0xA7 | 0xC6..=0xC8 if pc != this_branch => {
                    match self.branch_target(pc, op) {
                        Ok(t) if t == block => return true,
                        Err(_) => return true,
                        _ => {}
                    }
                }
                _ => {}
            }
            pc += size;
        }
        false
    }

    /// Whether `[from, to)` contains any branch, switch or return.
    fn has_control_flow(&self, from: usize, to: usize) -> bool {
        let mut pc = from;
        while pc < to {
            let Ok(size) = instr_size(self.bytes, pc) else {
                return true;
            };
            if size == 0 || pc + size > to {
                return true;
            }
            match self.bytes[pc] {
                0x99..=0xA8 | 0xAA | 0xAB | 0xAC..=0xB1 | 0xC6..=0xC8 => return true,
                _ => {}
            }
            pc += size;
        }
        false
    }

    /// Emit both arms unconditionally and merge them with `selp`.
    ///
    /// Returns `Ok(false)` when the arms turn out not to be speculatable,
    /// having left the emitter exactly as it found it -- the caller then
    /// lowers the branch the ordinary way.
    ///
    /// # Why the screen is on the PTX and not on the bytecode
    ///
    /// Running both arms is sound only when neither can fault, store, or
    /// deopt. That is a property of what the arm LOWERS TO, and the
    /// opcode-to-lowering map is a 2000-line match with array bounds
    /// checks, divide-by-zero guards and an intrinsic table in it. An
    /// allow-list of opcodes would be a second copy of that knowledge,
    /// free to drift from the first; the text the arm actually emitted IS
    /// the knowledge. So: emit into a scratch buffer, and refuse anything
    /// holding a label, a branch, a predicated instruction, or a memory
    /// access. A bounds check lowers to `@%p bra L_bounds_fail`, so the
    /// branch screen already catches it; the memory screen is there for a
    /// future lowering that somehow does not branch.
    fn try_emit_if_converted(
        &mut self,
        plan: &IfConversion,
        predicate: &Reg,
        loop_info: &CountedLoop,
        entries: &mut BTreeMap<usize, BlockState>,
        end: usize,
    ) -> Result<bool, LoweringError> {
        let saved = BlockState {
            stack: self.stack.clone(),
            locals: self.locals.clone(),
        };
        // Emitter state a rejected speculation must not leave behind.
        // `emit_op` sets these as a side effect of lowering an opcode, and
        // a rejected arm's opcodes did not happen: a stray `writes` bit
        // would cost a pointless D2H writeback of a read-only array, and a
        // stray `reads` bit would silently disable the chunked overlap for
        // an array nothing reads.
        let saved_writes = self.writes_param_mask;
        let saved_reads = self.reads_param_mask;
        let saved_bounds_label = self.used_bounds_label;

        let real_body = std::mem::take(&mut self.body);
        // Nothing emitted from here to the restore below may escape into
        // the kernel prologue; see the `speculating` field.
        let was_speculating = self.speculating;
        self.speculating = true;
        let then_ok = self.walk_straight(plan.then_start, plan.then_goto_pc, loop_info);
        let then_text = std::mem::take(&mut self.body);
        let then_state = BlockState {
            stack: self.stack.clone(),
            locals: self.locals.clone(),
        };

        self.stack = saved.stack.clone();
        self.locals = saved.locals.clone();
        let else_ok = if then_ok.is_ok() {
            self.walk_straight(plan.else_start, plan.join, loop_info)
        } else {
            Ok(())
        };
        let else_text = std::mem::take(&mut self.body);
        let else_state = BlockState {
            stack: self.stack.clone(),
            locals: self.locals.clone(),
        };

        self.body = real_body;
        self.speculating = was_speculating;
        let budget = self.if_convert_budget;
        let cost = speculation_cost(&then_text)
            .zip(speculation_cost(&else_text))
            .map(|(a, b)| a.saturating_add(b));
        let usable = then_ok.is_ok()
            && else_ok.is_ok()
            && cost.is_some_and(|c| c <= budget)
            && then_state.stack.0.len() == else_state.stack.0.len();
        if !usable {
            self.stack = saved.stack;
            self.locals = saved.locals;
            self.writes_param_mask = saved_writes;
            self.reads_param_mask = saved_reads;
            self.used_bounds_label = saved_bounds_label;
            return Ok(false);
        }

        // Build the merge before committing any text, so a slot pair no
        // `selp` can express still leaves the emitter untouched.
        let Some((merged, selps)) = self.plan_merge(&then_state, &else_state, predicate) else {
            self.stack = saved.stack;
            self.locals = saved.locals;
            self.writes_param_mask = saved_writes;
            self.reads_param_mask = saved_reads;
            self.used_bounds_label = saved_bounds_label;
            return Ok(false);
        };

        self.body.push_str(&then_text);
        self.body.push_str(&else_text);
        for line in selps {
            self.body.push_str(&line);
        }
        self.stack = merged.stack;
        self.locals = merged.locals;
        if plan.join < end {
            let state = BlockState {
                stack: self.stack.clone(),
                locals: self.locals.clone(),
            };
            self.copy_state_to_edge(&state, plan.join, None, entries)?;
        }
        Ok(true)
    }

    /// Emit `[from, to)` as straight-line code. The caller has already
    /// established that the range holds no control flow.
    fn walk_straight(
        &mut self,
        from: usize,
        to: usize,
        loop_info: &CountedLoop,
    ) -> Result<(), LoweringError> {
        let mut pc = from;
        while pc < to {
            let op = self.bytes[pc];
            let size = instr_size(self.bytes, pc)?;
            if size == 0 || pc + size > to {
                return Err(LoweringError::UnsupportedNode(format!(
                    "instruction at pc={pc} runs past the speculated arm"
                )));
            }
            self.emit_op(op, pc, Some(loop_info))?;
            pc += size;
        }
        Ok(())
    }

    /// The `selp` merge of two arms' end states, or `None` when some slot
    /// pair cannot be expressed as one.
    ///
    /// `predicate` is TRUE on the branch-TAKEN path, which is the `else`
    /// arm -- so the operand order is `selp d, else, then, p`. Reversing
    /// it yields a kernel that computes both right values and keeps the
    /// wrong one, which no shape test would catch.
    fn plan_merge(
        &mut self,
        then_state: &BlockState,
        else_state: &BlockState,
        predicate: &Reg,
    ) -> Option<(BlockState, Vec<String>)> {
        let mut lines = Vec::new();
        let mut stack = Vec::with_capacity(then_state.stack.0.len());
        for (t, e) in then_state.stack.0.iter().zip(&else_state.stack.0) {
            stack.push(self.merge_slot(t, e, predicate, &mut lines)?);
        }
        let local_count = then_state.locals.0.len().max(else_state.locals.0.len());
        let mut locals = Vec::with_capacity(local_count);
        for i in 0..local_count {
            let t = then_state.locals.0.get(i).and_then(Option::as_ref);
            let e = else_state.locals.0.get(i).and_then(Option::as_ref);
            locals.push(match (t, e) {
                (Some(t), Some(e)) => Some(self.merge_slot(t, e, predicate, &mut lines)?),
                // Bound on one arm only: dead after the join for any
                // verifier-valid method, exactly as `copy_matching_state`
                // already assumes for the branching form.
                _ => None,
            });
        }
        Some((
            BlockState {
                stack: OpStack(stack),
                locals: Locals(locals),
            },
            lines,
        ))
    }

    /// One slot of [`Emitter::plan_merge`].
    fn merge_slot(
        &mut self,
        t: &Reg,
        e: &Reg,
        predicate: &Reg,
        lines: &mut Vec<String>,
    ) -> Option<Reg> {
        if t.name == e.name {
            return Some(t.clone());
        }
        if t.kind != e.kind || t.wide != e.wide {
            return None;
        }
        // A `U64` slot is an array reference, and the array lowering
        // identifies an array BY its parameter-pointer register -- a
        // merged one would be an array nothing can resolve, the same
        // reason `canonicalise_state` refuses to phi them. A predicate
        // and the raw-16-bit conversion temporary never legitimately
        // live across a join.
        let suffix = match t.kind {
            RegKind::U32 => "u32",
            RegKind::S32 => "s32",
            RegKind::S64 => "s64",
            RegKind::F32 => "f32",
            RegKind::F64 => "f64",
            RegKind::U64 | RegKind::Pred | RegKind::B16 => return None,
        };
        let dst = self.regs.fresh_reg_with_wide(t.kind, t.wide);
        lines.push(format!(
            "    selp.{suffix} {}, {}, {}, {};\n",
            dst.name, e.name, t.name, predicate.name
        ));
        Some(dst)
    }

    fn cfg_block_starts(
        &self,
        start: usize,
        end: usize,
        loop_info: &CountedLoop,
    ) -> Result<BTreeSet<usize>, LoweringError> {
        let mut starts = BTreeSet::from([start]);
        let mut pc = start;
        while pc < end {
            let op = self.bytes[pc];
            let size = instr_size(self.bytes, pc)?;
            let next = pc + size;
            if next > end || next > self.bytes.len() {
                return Err(LoweringError::UnsupportedNode(format!(
                    "instruction at pc={pc} crosses the loop-body boundary"
                )));
            }
            match op {
                0x99..=0xA4 | 0xC6 | 0xC7 => {
                    let target = self.branch_target(pc, op)?;
                    self.validate_cfg_target(pc, target, start, end, loop_info)?;
                    starts.insert(target);
                    if next < end {
                        starts.insert(next);
                    }
                }
                0xA7 | 0xC8 => {
                    let target = self.branch_target(pc, op)?;
                    if target != loop_info.header_pc {
                        self.validate_cfg_target(pc, target, start, end, loop_info)?;
                        starts.insert(target);
                    }
                }
                0xAC..=0xB1 => {
                    return Err(LoweringError::UnsupportedNode(format!(
                        "return at pc={pc} inside a GPU loop body is not supported"
                    )));
                }
                _ => {}
            }
            pc = next;
        }
        Ok(starts)
    }

    /// Every interior back-edge target inside `start..end`, i.e. the
    /// header of each sequential loop the CUDA thread will run itself.
    /// The outer loop's own canonical back-edge is excluded — `walk_cfg`
    /// is called with `end = back_branch_pc` so it is out of range
    /// anyway, and its target is the loop header the host guard replaced.
    fn back_edge_targets(
        &self,
        start: usize,
        end: usize,
        loop_info: &CountedLoop,
    ) -> Result<BTreeSet<usize>, LoweringError> {
        let mut targets = BTreeSet::new();
        let mut pc = start;
        while pc < end {
            let op = self.bytes[pc];
            let size = instr_size(self.bytes, pc)?;
            if matches!(op, 0x99..=0xA7 | 0xC6 | 0xC7 | 0xC8) {
                let target = self.branch_target(pc, op)?;
                if target <= pc && target != loop_info.header_pc {
                    targets.insert(target);
                }
            }
            pc += size;
        }
        Ok(targets)
    }

    /// Refuse a back-edge whose target block was never emitted.
    ///
    /// The walker visits blocks in ascending PC order and skips any with
    /// no incoming edge, so a back-edge target that is missing from
    /// `emitted`/`entries` is a block the admitted subgraph never
    /// entered forward — an irreducible or unreachable region. Emitting
    /// `bra L_body_<pc>` for it would produce PTX referencing a label
    /// that does not exist, which `ptxas` rejects at module load: a hard
    /// failure instead of the CPU fallback every other refusal here
    /// produces. Forward edges are unaffected (their target is always
    /// still ahead, and `copy_state_to_edge` creates its state).
    fn check_back_edge(
        source: usize,
        target: usize,
        emitted: &BTreeSet<usize>,
        entries: &BTreeMap<usize, BlockState>,
    ) -> Result<(), LoweringError> {
        if target > source {
            return Ok(());
        }
        if !emitted.contains(&target) || !entries.contains_key(&target) {
            return Err(LoweringError::UnsupportedNode(format!(
                "back-edge at pc={source} targets pc={target}, a block the walk never \
                 entered through a forward edge (irreducible or unreachable control flow)"
            )));
        }
        Ok(())
    }

    fn validate_cfg_target(
        &self,
        source: usize,
        target: usize,
        start: usize,
        end: usize,
        _loop_info: &CountedLoop,
    ) -> Result<(), LoweringError> {
        if !(start..end).contains(&target) {
            return Err(LoweringError::UnsupportedNode(format!(
                "branch at pc={source} leaves the loop body (target {target}); \
                 break/continue/early-exit control flow stays on the CPU"
            )));
        }
        if !self.is_instruction_boundary(start, end, target)? {
            return Err(LoweringError::UnsupportedNode(format!(
                "branch at pc={source} targets byte {target}, which is not a JVM instruction boundary"
            )));
        }
        // A backward interior branch is a sequential loop the thread runs
        // itself, which `walk_cfg` lowers. A branch to *itself* is not:
        // it is an unconditional infinite loop with an empty body, and no
        // iteration-space mapping makes that terminate.
        if target == source {
            return Err(LoweringError::UnsupportedNode(format!(
                "branch at pc={source} targets itself — an empty infinite loop"
            )));
        }
        Ok(())
    }

    fn is_instruction_boundary(
        &self,
        start: usize,
        end: usize,
        target: usize,
    ) -> Result<bool, LoweringError> {
        let mut pc = start;
        while pc < end {
            if pc == target {
                return Ok(true);
            }
            pc += instr_size(self.bytes, pc)?;
        }
        Ok(false)
    }

    fn branch_target(&self, pc: usize, op: u8) -> Result<usize, LoweringError> {
        let offset =
            match op {
                0xC8 => {
                    i32::from_be_bytes([
                        *self.bytes.get(pc + 1).ok_or_else(|| {
                            LoweringError::UnsupportedNode("truncated goto_w".into())
                        })?,
                        *self.bytes.get(pc + 2).ok_or_else(|| {
                            LoweringError::UnsupportedNode("truncated goto_w".into())
                        })?,
                        *self.bytes.get(pc + 3).ok_or_else(|| {
                            LoweringError::UnsupportedNode("truncated goto_w".into())
                        })?,
                        *self.bytes.get(pc + 4).ok_or_else(|| {
                            LoweringError::UnsupportedNode("truncated goto_w".into())
                        })?,
                    ]) as i64
                }
                _ => {
                    i16::from_be_bytes([
                        *self.bytes.get(pc + 1).ok_or_else(|| {
                            LoweringError::UnsupportedNode("truncated branch".into())
                        })?,
                        *self.bytes.get(pc + 2).ok_or_else(|| {
                            LoweringError::UnsupportedNode("truncated branch".into())
                        })?,
                    ]) as i64
                }
            };
        let target = pc as i64 + offset;
        if target < 0 || target as usize >= self.bytes.len() {
            return Err(LoweringError::UnsupportedNode(format!(
                "branch at pc={pc} has out-of-range target {target}"
            )));
        }
        Ok(target as usize)
    }

    fn emit_branch_predicate(&mut self, op: u8) -> Result<Reg, LoweringError> {
        let (mnemonic, lhs, rhs, kind) = match op {
            0x99..=0x9E => {
                let value = self.stack.pop()?;
                if value.kind != RegKind::S32 {
                    return Err(LoweringError::UnsupportedNode(format!(
                        "if opcode 0x{op:02x} requires an int value, got {:?}",
                        value.kind
                    )));
                }
                let cmp = match op {
                    0x99 => "eq",
                    0x9A => "ne",
                    0x9B => "lt",
                    0x9C => "ge",
                    0x9D => "gt",
                    _ => "le",
                };
                (cmp, value.name, "0".to_string(), RegKind::S32)
            }
            0x9F..=0xA4 => {
                let rhs = self.stack.pop()?;
                let lhs = self.stack.pop()?;
                if lhs.kind != RegKind::S32 || rhs.kind != RegKind::S32 {
                    return Err(LoweringError::UnsupportedNode(format!(
                        "if_icmp opcode 0x{op:02x} requires two int values"
                    )));
                }
                let cmp = match op {
                    0x9F => "eq",
                    0xA0 => "ne",
                    0xA1 => "lt",
                    0xA2 => "ge",
                    0xA3 => "gt",
                    _ => "le",
                };
                (cmp, lhs.name, rhs.name, RegKind::S32)
            }
            0xC6 | 0xC7 => {
                let value = self.stack.pop()?;
                if value.kind != RegKind::U64 {
                    return Err(LoweringError::UnsupportedNode(format!(
                        "ifnull/ifnonnull at a non-reference value ({:?})",
                        value.kind
                    )));
                }
                (
                    if op == 0xC6 { "eq" } else { "ne" },
                    value.name,
                    "0".to_string(),
                    RegKind::U64,
                )
            }
            _ => unreachable!("only conditional branches call emit_branch_predicate"),
        };
        let pred = self.regs.fresh_reg(RegKind::Pred);
        let suffix = match kind {
            RegKind::S32 => "s32",
            RegKind::U64 => "u64",
            _ => unreachable!(),
        };
        writeln!(
            self.body,
            "    setp.{mnemonic}.{suffix} {}, {}, {};",
            pred.name, lhs, rhs
        )
        .unwrap();
        Ok(pred)
    }

    fn copy_state_to_edge(
        &mut self,
        source: &BlockState,
        target_pc: usize,
        predicate: Option<(&str, bool)>,
        entries: &mut BTreeMap<usize, BlockState>,
    ) -> Result<(), LoweringError> {
        if !entries.contains_key(&target_pc) {
            entries.insert(target_pc, self.canonicalise_state(source));
        }
        let target = entries
            .get(&target_pc)
            .expect("state just inserted")
            .clone();
        self.copy_matching_state(source, &target, predicate, target_pc)
    }

    /// Build the register state a join point will use, given the FIRST
    /// edge that reaches it.
    ///
    /// The obvious implementation — mint a fresh "phi" register for
    /// every live slot — is what this used to do, and it is quadratic
    /// in the wrong thing: every edge into every join then copies
    /// *every* live local and stack slot, including the overwhelming
    /// majority that are identical on both sides of the branch. The two
    /// edges out of one `if` carry literally the same state, so that
    /// alone emitted `2 * live_slots` pointless `mov`s per branch. On
    /// the ray-tracer kernel (about 130 live locals, seven branches)
    /// the emitted PTX was 3299 `mov.f32` out of 4069 instructions, and
    /// ptxas kept 933 of them as `FSEL` in the SASS — 38% of the
    /// executed kernel was reconciling values that never differed.
    ///
    /// Instead, adopt the incoming registers as the join's registers.
    /// A later edge whose slot holds a different register then copies
    /// into the adopted one, which is exactly the phi — and a later
    /// edge that agrees copies nothing (`copy_reg` short-circuits on
    /// equal names). Correct because an edge's copies are the last
    /// thing it does before transferring to the join, so the adopted
    /// register cannot be live on that path for any other reason.
    ///
    /// Two slots must still get a fresh register, because for them
    /// "write the adopted register" is not a private act:
    ///
    /// * **Aliased slots.** `iload x; istore y` leaves both locals
    ///   bound to the *same* register. Merging local `x` by writing
    ///   that register would silently also change local `y`. Any name
    ///   appearing more than once in the state is therefore freshened.
    /// * **Reserved registers.** `tid`, the recovered nested indices,
    ///   the loop bound, the return value and the parameter array
    ///   pointers are all read from `Emitter` fields rather than
    ///   through the block state, so a merge that overwrote one would
    ///   corrupt a value the rest of the kernel still reads.
    ///
    /// `U64` slots keep their register unconditionally, as before:
    /// array references are *identified* by their parameter pointer
    /// register (`array_param_of` resolves an `aload`ed reference that
    /// way), so a phi register would be an array the array lowering
    /// cannot resolve. Being reserved, they are also refused as a copy
    /// destination by `copy_reg`, which turns "two different arrays
    /// merge here" from a silently clobbered pointer into a CPU
    /// fallback.
    fn canonicalise_state(&mut self, source: &BlockState) -> BlockState {
        let mut occurrences: HashMap<&str, u32> = HashMap::new();
        for r in source
            .stack
            .0
            .iter()
            .chain(source.locals.0.iter().flatten())
        {
            *occurrences.entry(r.name.as_str()).or_insert(0) += 1;
        }
        let adoptable = |r: &Reg, occurrences: &HashMap<&str, u32>, this: &Self| -> bool {
            r.kind == RegKind::U64
                || (occurrences.get(r.name.as_str()).copied().unwrap_or(0) == 1
                    && !this.is_reserved_reg(&r.name))
        };
        // Two passes so the borrow of `source` for `occurrences` ends
        // before `self.regs` is mutated.
        let keep_stack: Vec<bool> = source
            .stack
            .0
            .iter()
            .map(|r| adoptable(r, &occurrences, self))
            .collect();
        let keep_locals: Vec<bool> = source
            .locals
            .0
            .iter()
            .map(|slot| {
                slot.as_ref()
                    .is_some_and(|r| adoptable(r, &occurrences, self))
            })
            .collect();
        drop(occurrences);

        let mut stack = Vec::with_capacity(source.stack.0.len());
        for (r, &keep) in source.stack.0.iter().zip(&keep_stack) {
            stack.push(if keep {
                r.clone()
            } else {
                Reg {
                    kind: r.kind,
                    name: self.regs.fresh(r.kind),
                    wide: r.wide,
                }
            });
        }
        let mut locals = Vec::with_capacity(source.locals.0.len());
        for (slot, &keep) in source.locals.0.iter().zip(&keep_locals) {
            locals.push(slot.as_ref().map(|r| {
                if keep {
                    r.clone()
                } else {
                    Reg {
                        kind: r.kind,
                        name: self.regs.fresh(r.kind),
                        wide: r.wide,
                    }
                }
            }));
        }
        BlockState {
            stack: OpStack(stack),
            locals: Locals(locals),
        }
    }

    /// Registers the emitter reads from its own fields rather than
    /// through the block state, and which a join merge therefore must
    /// never adopt as a copy destination.
    fn is_reserved_reg(&self, name: &str) -> bool {
        for reserved in [
            self.tid_reg.as_ref(),
            self.tid_reg_inner.as_ref(),
            self.bound_reg.as_ref(),
            self.ret_value_reg.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if reserved.name == name {
                return true;
            }
        }
        self.param_ptr_reg.iter().any(|p| p == name)
    }

    fn copy_matching_state(
        &mut self,
        source: &BlockState,
        target: &BlockState,
        predicate: Option<(&str, bool)>,
        target_pc: usize,
    ) -> Result<(), LoweringError> {
        if source.stack.0.len() != target.stack.0.len() {
            return Err(LoweringError::UnsupportedNode(format!(
                "incompatible JVM state at control-flow join pc={target_pc}"
            )));
        }
        let mut pairs: Vec<(Reg, Reg)> = Vec::new();
        for (from, to) in source.stack.0.iter().zip(&target.stack.0) {
            pairs.push((from.clone(), to.clone()));
        }
        let local_count = source.locals.0.len().max(target.locals.0.len());
        for index in 0..local_count {
            let from = source.locals.0.get(index).and_then(Option::as_ref);
            let to = target.locals.0.get(index).and_then(Option::as_ref);
            match (from, to) {
                (None, None) => {}
                (Some(from), Some(to)) => pairs.push((from.clone(), to.clone())),
                // A verifier-valid method cannot read a local that is only
                // initialised on one predecessor.  If it is dead after this
                // join, preserving neither binding is semantically exact;
                // avoiding a needless rejection also handles javac's
                // branch-local temporary slots.
                _ => {}
            }
        }
        // A join's copies are a *parallel* assignment: every `to` takes the
        // value `from` held on entry to the edge. Emitting them one at a
        // time is only equivalent when no destination is also somebody
        // else's source. A swap (`a, b = b, a`) is the smallest case that
        // is not, and a loop back-edge is where it actually shows up —
        // rotating two accumulators through each other reads the already
        // overwritten register on the second copy. Sequence through
        // temporaries when, and only when, the overlap exists, so every
        // conflict-free join (which is all of them today) emits exactly
        // the PTX it emitted before this existed.
        let destinations: HashSet<&str> = pairs
            .iter()
            .filter(|(from, to)| from.name != to.name)
            .map(|(_, to)| to.name.as_str())
            .collect();
        let conflict = pairs
            .iter()
            .any(|(from, to)| from.name != to.name && destinations.contains(from.name.as_str()));
        if !conflict {
            for (from, to) in &pairs {
                self.copy_reg(from, to, predicate, target_pc)?;
            }
            return Ok(());
        }
        let mut staged: Vec<(Reg, Reg)> = Vec::new();
        for (from, to) in &pairs {
            if from.name == to.name {
                continue;
            }
            let temp = self.regs.fresh_reg_with_wide(from.kind, from.wide);
            self.copy_reg(from, &temp, predicate, target_pc)?;
            staged.push((temp, to.clone()));
        }
        for (temp, to) in &staged {
            self.copy_reg(temp, to, predicate, target_pc)?;
        }
        Ok(())
    }

    fn copy_reg(
        &mut self,
        from: &Reg,
        to: &Reg,
        predicate: Option<(&str, bool)>,
        target_pc: usize,
    ) -> Result<(), LoweringError> {
        if from.kind != to.kind || from.wide != to.wide {
            return Err(LoweringError::UnsupportedNode(format!(
                "incompatible register types at control-flow join pc={target_pc}"
            )));
        }
        if from.name == to.name {
            return Ok(());
        }
        // A join must never write a register the emitter reads from one
        // of its own fields: `tid`, the loop bound, the return value, or
        // an array parameter's pointer. `canonicalise_state` refuses to
        // ADOPT those, so reaching here means two predecessors genuinely
        // disagree about one, most plausibly two different arrays merging
        // into the same reference slot. Overwriting a parameter pointer
        // would silently corrupt every later access through it, so refuse
        // the method and let it run on the CPU.
        if self.is_reserved_reg(&to.name) {
            return Err(LoweringError::UnsupportedNode(format!(
                "control-flow join pc={target_pc} would merge into reserved \
                 register {} (tid / loop bound / return value / array \
                 parameter pointer): predecessors disagree about a value the \
                 kernel reads outside the block state",
                to.name
            )));
        }
        let suffix = match from.kind {
            RegKind::U32 => "u32",
            RegKind::U64 => "u64",
            RegKind::S32 => "s32",
            RegKind::S64 => "s64",
            RegKind::F32 => "f32",
            RegKind::F64 => "f64",
            RegKind::Pred => "pred",
            // A B16 is a scratch operand for `cvt.f32.f16` and never
            // holds a JVM value, so it is never live across an edge.
            RegKind::B16 => "b16",
        };
        let guard = match predicate {
            Some((name, true)) => format!("@!{name} "),
            Some((name, false)) => format!("@{name} "),
            None => String::new(),
        };
        writeln!(
            self.body,
            "    {guard}mov.{suffix} {}, {};",
            to.name, from.name
        )
        .unwrap();
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
                //
                // AUDIT 2026-05-24 (C31): `null` is a reference, which
                // is JVM category-1 — `pop2` of (any, null) must pop two
                // slots, not one. Override the U64-implies-wide default.
                let r = self.regs.fresh_reg_with_wide(RegKind::U64, false);
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
            // AUDIT C31 follow-up (2026-07-11): ldc / ldc_w / ldc2_w.
            // These load a numeric literal that didn't fit `iconst`/
            // `bipush`/`sipush` (any `int` outside sipush range) or has
            // no short form at all (`long`/`float`/`double`) — see
            // `ldc`/`ldc2_w` below for the constant-pool resolution and
            // immediate-push lowering.
            0x12 => self.ldc(self.bytes[pc + 1] as u16)?, // ldc: 1-byte CP index
            0x13 => {
                // ldc_w: 2-byte CP index.
                self.ldc(u16::from_be_bytes([self.bytes[pc + 1], self.bytes[pc + 2]]))?;
            }
            0x14 => {
                // ldc2_w: 2-byte CP index, always a wide (Long/Double) entry.
                self.ldc2_w(u16::from_be_bytes([self.bytes[pc + 1], self.bytes[pc + 2]]))?;
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
            0x33 => self.array_load_byte()?,                   // baload
            0x34 => self.array_load_char()?,                   // caload
            0x35 => self.array_load_short()?,                  // saload
            // ── array store ─────────────────────────────────────────
            0x4F => self.array_store(RegKind::S32, ".s32", 4)?, // iastore
            0x50 => self.array_store(RegKind::S64, ".s64", 8)?, // lastore
            0x51 => self.array_store(RegKind::F32, ".f32", 4)?, // fastore
            0x52 => self.array_store(RegKind::F64, ".f64", 8)?, // dastore
            0x54 => self.array_store_byte()?,                   // bastore
            0x55 => self.array_store_char_or_short(2)?,         // castore
            0x56 => self.array_store_char_or_short(2)?,         // sastore
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
            0x6C => self.div_or_rem_i32("div.s32", true)?,
            0x6D => self.div_or_rem_i64("div.s64", true)?,
            0x6E => self.binop_f32("div.f32")?,
            0x6F => self.binop_f64("div.f64")?,
            0x70 => self.div_or_rem_i32("rem.s32", false)?,
            0x71 => self.div_or_rem_i64("rem.s64", false)?,
            // AUDIT 2026-05-16 / 2026-07-11: PTX has no `rem.f32`/
            // `rem.f64` mnemonic — emitting one made ptxas reject every
            // kernel that hit this path, so this used to reject
            // unconditionally. `frem_f32`/`drem_f64` below now build
            // the fmod identity out of PTX instructions that DO exist
            // (`div.rn` + `cvt.rzi` + `neg` + `fma.rn`, plus an
            // infinite-divisor correctness patch). The analyzer only
            // ever lets `0x72`/`0x73` reach this dispatch when the
            // method was admitted under `AdmissionHint::AllowDivByZero`
            // (see `analyzer::classify`'s `0x72 | 0x73` arm) — `Strict`
            // mode still rejects upstream via `Reason::FloatRemainder`,
            // so this arm is unreachable for a `Strict`-admitted
            // method. See `frem_f32`'s doc comment for the full JLS
            // §15.17.3 derivation and exactly which edge cases are (and
            // are not) handled bit-exactly.
            //
            // Java floats never trap on a zero divisor — IEEE 754
            // division of a float by zero yields ±Infinity or NaN, not
            // an exception — so, unlike `div_or_rem_i32`/
            // `div_or_rem_i64`, neither helper below emits (or needs) a
            // zero-divisor guard branching to `bounds_fail_label`.
            0x72 => self.frem_f32()?,
            0x73 => self.drem_f64()?,
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
            // f2d — but see `float_sqrt_triple_at`: when this widen exists
            // only to reach `Math.sqrt(D)D` and is narrowed straight back,
            // leave the value as F32 and let `invokestatic` emit a single
            // `sqrt.rn.f32`. Skipping the widen here is what tells the two
            // later arms the collapse is in progress.
            0x8D if self.float_sqrt_triple_at(pc) => {}
            0x8D => self.conv("cvt.f64.f32", RegKind::F32, RegKind::F64)?, // f2d
            0x8E => self.conv("cvt.rzi.s32.f64", RegKind::F64, RegKind::S32)?, // d2i
            0x8F => self.conv("cvt.rzi.s64.f64", RegKind::F64, RegKind::S64)?, // d2l
            // d2f — a no-op when the value on the stack is already F32,
            // which happens only for the collapsed float-sqrt triple.
            0x90 if self.stack.0.last().map(|r| r.kind) == Some(RegKind::F32) => {}
            0x90 => self.conv("cvt.rn.f32.f64", RegKind::F64, RegKind::F32)?, // d2f
            0x91 => self.conv_truncate_i32(8)?,                               // i2b
            // i2c — Java `char` is an UNSIGNED 16-bit value, so JVMS i2c
            // zero-extends the low 16 bits (not sign-extends like i2b/i2s).
            // Route through the zero-extending helper so e.g. 0xFFFF maps
            // to 65535 rather than -1.
            0x92 => self.conv_zext_u16()?,       // i2c (unsigned 16)
            0x93 => self.conv_truncate_i32(16)?, // i2s
            // ── compares (push int -1/0/1) ──────────────────────────
            // AUDIT 2026-05-19: `lcmp` (0x94) used to dispatch to
            // `cmp_long_or_float`, which unconditionally returned `Err`
            // — it *looked* supported at the dispatch site while never
            // succeeding — so it was rejected explicitly here instead,
            // same as `fcmpl`/`fcmpg`/`dcmpl`/`dcmpg`, which fell through
            // to the default `UnsupportedNode` arm.
            //
            // AUDIT 2026-07-11: all five now have a real, unconditionally
            // bit-exact lowering (`lcmp`/`cmp_f32`/`cmp_f64` below — a
            // `setp` + `selp` select chain; see each function's doc
            // comment for the JVMS semantics and, for the float/double
            // pair, the explicit `setp.nan` override that gives `fcmpl`/
            // `dcmpl` a NaN result of `-1` and `fcmpg`/`dcmpg` a NaN
            // result of `+1`). `analyzer::classify`'s `0x94..=0x98` arm
            // now admits these opcodes unconditionally too — see that
            // arm's comment and `analyzer::Reason::Compare`'s doc comment
            // for the "does this make anything newly offloadable"
            // reality check: as of this change, no, because every real
            // javac `*cmp*` is immediately followed by an `if<cond>`
            // that consumes the pushed value, and general `if*` still
            // rejects at the `0x99..=0xA4` arm just below. This lowering
            // is nonetheless the correct building block for that future
            // cmp-then-branch fusion, and is directly reachable (and
            // tested) whenever the pushed value is itself stored/used —
            // a shape real javac never emits but a hand-built body can.
            0x94 => self.lcmp()?,
            0x95 => self.cmp_f32(-1)?, // fcmpl: NaN → -1
            0x96 => self.cmp_f32(1)?,  // fcmpg: NaN → +1
            0x97 => self.cmp_f64(-1)?, // dcmpl: NaN → -1
            0x98 => self.cmp_f64(1)?,  // dcmpg: NaN → +1
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
                    let off = i16::from_be_bytes([self.bytes[pc + 1], self.bytes[pc + 2]]) as i32;
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
            // ── arraylength ─────────────────────────────────────────
            0xBE => self.arraylength()?,
            // ── invokestatic (curated Math/StrictMath intrinsics) ────
            // AUDIT 2026-07-11 (intrinsic table follow-up): closes the
            // documented analyzer/lowering gap (see
            // `docs/gpu/annotations.md`'s `ALLOW_INTRINSIC_CALLS`
            // section as of 2026-07-11) — this opcode previously had no
            // dispatch arm at all and fell through to the catch-all
            // `UnsupportedNode` below, so every method admitted via
            // `AdmissionHint::AllowIntrinsicCalls` failed to lower and
            // was silently blacklisted. See `invokestatic` below for
            // the constant-pool resolution and dispatch to each
            // intrinsic's PTX lowering.
            0xB8 => {
                let index = u16::from_be_bytes([self.bytes[pc + 1], self.bytes[pc + 2]]);
                self.invokestatic(index)?;
            }
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
                        let delta =
                            i16::from_be_bytes([self.bytes[pc + 4], self.bytes[pc + 5]]) as i32;
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

    // ─────────────────── constant-pool loads ─────────────────────────
    //
    // AUDIT C31 follow-up (2026-07-11): `ldc`/`ldc_w` push a fresh
    // register holding an `Integer`/`Float` constant-pool entry;
    // `ldc2_w` does the same for `Long`/`Double`. This mirrors exactly
    // how `iconst_*`/`bipush`/`sipush`/`fconst_*`/`dconst_*` already
    // push a literal above in `emit_op` — a `mov.<type> %rN, <value>;`
    // into a fresh register, then the register goes on the simulated
    // operand stack. Float/double use the same exact-bit PTX hex-literal
    // encoding (`0f<8 hex digits>` / `0d<16 hex digits>`) the existing
    // `fconst_0..2`/`dconst_0..1` arms use, rather than a decimal text
    // literal — PTX accepts both forms for floating-point immediates,
    // but the hex-bit form is exact (no host/ptxas decimal-rounding
    // round-trip to worry about), so we stay consistent with what's
    // already there instead of introducing a second, less precise style.
    //
    // Both entry points funnel through `resolve_cp_entry`, which is the
    // single place that (a) requires `self.cp` to be present and (b)
    // bounds-checks the constant-pool index. Reaching either "reject"
    // branch below should be unreachable in practice: the analyzer
    // (`Reason::LoadConstant`, `analyzer::classify_ldc`) already proved
    // the exact same CP entry is a matching Integer/Float/Long/Double
    // before admitting the method, using the same constant pool. The
    // errors exist purely as defence in depth for a caller that built a
    // `KernelSignature`/pool mismatch by hand instead of getting both
    // from the analyzer together.

    /// `ldc` (0x12) / `ldc_w` (0x13) — `index` is the resolved
    /// (already-widened) constant-pool index. Pushes an `Integer` as an
    /// s32 immediate or a `Float` as an exact-bit f32 immediate.
    fn ldc(&mut self, index: u16) -> Result<(), LoweringError> {
        match self.resolve_cp_entry(index)? {
            ConstantPoolEntry::Integer(v) => {
                let r = self.regs.fresh_reg(RegKind::S32);
                writeln!(self.body, "    mov.s32 {}, {};", r.name, v).unwrap();
                self.stack.push(r);
            }
            ConstantPoolEntry::Float(v) => {
                let r = self.regs.fresh_reg(RegKind::F32);
                writeln!(self.body, "    mov.f32 {}, 0f{:08X};", r.name, v.to_bits()).unwrap();
                self.stack.push(r);
            }
            other => {
                return Err(LoweringError::UnsupportedNode(format!(
                    "ldc/ldc_w constant-pool index {index} is {other:?}, not Integer/Float — \
                     the analyzer should have rejected this method upstream"
                )));
            }
        }
        Ok(())
    }

    /// `ldc2_w` (0x14) — `index` is the constant-pool index of a `Long`
    /// or `Double` entry (JVMS §6.5: `ldc2_w` never targets anything
    /// else). Pushes an s64 or an exact-bit f64 immediate.
    fn ldc2_w(&mut self, index: u16) -> Result<(), LoweringError> {
        match self.resolve_cp_entry(index)? {
            ConstantPoolEntry::Long(v) => {
                let r = self.regs.fresh_reg(RegKind::S64);
                writeln!(self.body, "    mov.s64 {}, {};", r.name, v).unwrap();
                self.stack.push(r);
            }
            ConstantPoolEntry::Double(v) => {
                let r = self.regs.fresh_reg(RegKind::F64);
                writeln!(self.body, "    mov.f64 {}, 0d{:016X};", r.name, v.to_bits()).unwrap();
                self.stack.push(r);
            }
            other => {
                return Err(LoweringError::UnsupportedNode(format!(
                    "ldc2_w constant-pool index {index} is {other:?}, not Long/Double — \
                     the analyzer should have rejected this method upstream"
                )));
            }
        }
        Ok(())
    }

    /// Look up constant-pool entry `index`, cloning it out (every
    /// numeric `ConstantPoolEntry` variant is a cheap `Copy`-able
    /// primitive, so the clone is free) so the match arms in `ldc` /
    /// `ldc2_w` don't have to juggle a borrow of `self.cp` alongside
    /// `&mut self` calls to `self.regs`/`self.stack`.
    fn resolve_cp_entry(&self, index: u16) -> Result<ConstantPoolEntry, LoweringError> {
        let cp = self.cp.ok_or_else(|| {
            LoweringError::UnsupportedNode(
                "ldc/ldc_w/ldc2_w with no constant pool available to lowering \
                 (use lowering::lower_method_with_pool instead of lower_method)"
                    .into(),
            )
        })?;
        cp.get(index).cloned().ok_or_else(|| {
            LoweringError::UnsupportedNode(format!(
                "ldc/ldc_w/ldc2_w constant-pool index {index} is out of range"
            ))
        })
    }

    // ─────────────────── invokestatic intrinsics ─────────────────────
    //
    // AUDIT 2026-07-11 (intrinsic table follow-up): `invokestatic`
    // (0xB8) resolves its 2-byte constant-pool index to a
    // `MethodReference`, then looks the (class, name, descriptor) triple
    // up in `analyzer::resolve_math_intrinsic` — the single shared
    // table the analyzer's `classify_invokestatic` already consulted to
    // admit this method in the first place. Reaching the `None` arm
    // below should be unreachable for a method that came through the
    // analyzer; it exists purely as defence in depth, exactly like
    // `ldc`/`ldc2_w`'s "should have been rejected upstream" arms above.

    /// `invokestatic` (0xB8) — resolve `index` to a `MethodReference`
    /// and dispatch to the matching intrinsic's PTX lowering. See the
    /// module-level AUDIT comment above and
    /// `analyzer::resolve_math_intrinsic` for the curated table and the

    /// Is the instruction at `pc` the `f2d` of a `(float) Math.sqrt(f)`?
    ///
    /// `java.lang.Math` declares square root only as `sqrt(D)D`, so there is
    /// no way to spell a float square root in Java except
    ///
    /// ```text
    /// f2d                              (widen the float to double)
    /// invokestatic Math.sqrt:(D)D      (correctly-rounded f64 sqrt)
    /// d2f                              (round the result back to float)
    /// ```
    ///
    /// and javac emits those three adjacent. Lowered literally that is a
    /// DOUBLE-precision square root, which on a consumer GPU is a disaster:
    /// Turing runs FP64 at 1/32 of FP32 rate, and `sqrt.rn.f64` expands to a
    /// `MUFU.RSQ64H` plus a Newton-Raphson chain of `DFMA`/`DMUL`. The
    /// four-sphere ray tracer has eight of these per pixel and its SASS came
    /// out with 76 FP64 instructions against ~250 FP32 ones — the f64 sqrts
    /// alone outweighed the entire rest of the kernel.
    ///
    /// Collapsing the triple to one `sqrt.rn.f32` is **bit-exact**, not an
    /// approximation. Rounding a square root through binary64 and then to
    /// binary32 gives the correctly-rounded binary32 result whenever
    /// `p64 >= 2 * p32 + 2`, and 53 >= 50. That is the classical
    /// innocuous-double-rounding condition for square root, and it was
    /// checked here the blunt way rather than cited: all 2^32 float bit
    /// patterns, `(x as f64).sqrt() as f32` against `x.sqrt()`, zero
    /// mismatches — including subnormals, both zeros, both infinities and
    /// the NaNs. `sqrt.rn.f32` (not `.approx`, not `.ftz`) is required for
    /// that to hold.
    ///
    /// Matching the whole triple rather than just the call matters: an
    /// `f2d` that is NOT feeding a narrowed sqrt still widens normally, and
    /// a genuine `double` square root still lowers to `sqrt.rn.f64`.
    fn float_sqrt_triple_at(&self, pc: usize) -> bool {
        // f2d (1 byte) ; invokestatic (3 bytes) ; d2f
        if self.bytes.get(pc) != Some(&0x8D) || self.bytes.get(pc + 1) != Some(&0xB8) {
            return false;
        }
        if self.bytes.get(pc + 4) != Some(&0x90) {
            return false;
        }
        let (Some(&hi), Some(&lo)) = (self.bytes.get(pc + 2), self.bytes.get(pc + 3)) else {
            return false;
        };
        let Some(cp) = self.cp else { return false };
        let index = u16::from_be_bytes([hi, lo]);
        let Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }) = cp.get(index)
        else {
            return false;
        };
        let Some(class_name) = cp.get_class_name(*class_index) else {
            return false;
        };
        let Some((method_name, descriptor)) = cp.get_name_and_type(*name_and_type_index) else {
            return false;
        };
        matches!(
            resolve_math_intrinsic(class_name, method_name, descriptor),
            Some(MathIntrinsic::SqrtF64)
        )
    }
    /// exactness rationale for each entry.
    fn invokestatic(&mut self, index: u16) -> Result<(), LoweringError> {
        let cp = self.cp.ok_or_else(|| {
            LoweringError::UnsupportedNode(
                "invokestatic with no constant pool available to lowering \
                 (use lowering::lower_method_with_pool instead of lower_method)"
                    .into(),
            )
        })?;
        let Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }) = cp.get(index)
        else {
            return Err(LoweringError::UnsupportedNode(format!(
                "invokestatic constant-pool index {index} is not a MethodReference"
            )));
        };
        let (class_index, name_and_type_index) = (*class_index, *name_and_type_index);
        let class_name = cp.get_class_name(class_index).ok_or_else(|| {
            LoweringError::UnsupportedNode(format!(
                "invokestatic constant-pool index {index}: class_index {class_index} does \
                 not resolve to a class name"
            ))
        })?;
        let (method_name, descriptor) =
            cp.get_name_and_type(name_and_type_index).ok_or_else(|| {
                LoweringError::UnsupportedNode(format!(
                    "invokestatic constant-pool index {index}: name_and_type_index \
                     {name_and_type_index} does not resolve to a name/descriptor pair"
                ))
            })?;
        match resolve_math_intrinsic(class_name, method_name, descriptor) {
            // `Math.sqrt` is only declared `(D)D`, so a float square root in
            // Java is always spelled `(float) Math.sqrt(f)` and always
            // arrives here with an `f2d` in front and a `d2f` behind. When
            // `f2d` recognised that triple it left the operand as F32 (see
            // `float_sqrt_triple_at`), and the whole thing collapses to one
            // `sqrt.rn.f32`. Otherwise the operand really is a double and
            // the f64 square root is what the program asked for.
            Some(MathIntrinsic::SqrtF64)
                if self.stack.0.last().map(|r| r.kind) == Some(RegKind::F32) =>
            {
                self.unop_f32("sqrt.rn.f32")
            }
            Some(MathIntrinsic::SqrtF64) => self.unop_f64("sqrt.rn.f64"),
            Some(MathIntrinsic::AbsF32) => self.unop_f32("abs.f32"),
            Some(MathIntrinsic::AbsF64) => self.unop_f64("abs.f64"),
            Some(MathIntrinsic::AbsI32) => self.abs_i32(),
            Some(MathIntrinsic::AbsI64) => self.abs_i64(),
            Some(MathIntrinsic::MinI32) => self.minmax_i32(true),
            Some(MathIntrinsic::MaxI32) => self.minmax_i32(false),
            Some(MathIntrinsic::MinI64) => self.minmax_i64(true),
            Some(MathIntrinsic::MaxI64) => self.minmax_i64(false),
            Some(MathIntrinsic::MinF32) => self.minmax_f32(true),
            Some(MathIntrinsic::MaxF32) => self.minmax_f32(false),
            Some(MathIntrinsic::MinF64) => self.minmax_f64(true),
            Some(MathIntrinsic::MaxF64) => self.minmax_f64(false),
            Some(MathIntrinsic::FmaF32) => self.fma_f32(),
            Some(MathIntrinsic::FmaF64) => self.fma_f64(),
            Some(MathIntrinsic::Float16ToFloat) => self.float16_to_float(),
            Some(MathIntrinsic::ExpF64) => self.exp_approx(),
            None => Err(LoweringError::UnsupportedNode(format!(
                "invokestatic {class_name}.{method_name}{descriptor} is not a recognised GPU \
                 intrinsic — the analyzer should have rejected this method upstream \
                 (see analyzer::resolve_math_intrinsic)"
            ))),
        }
    }

    /// `Math.exp(double)`, approximately.
    ///
    /// PTX has one exponential, `ex2.approx.f32`, and no f64 form at
    /// all. `exp(x)` becomes `ex2(x * log2(e))` computed in single
    /// precision and widened back, which is why this intrinsic is
    /// admitted only under `CRATONVM_GPU_APPROX_MATH=1` — see
    /// [`crate::analyzer::MathIntrinsic::ExpF64`] for the contract
    /// being traded away.
    ///
    /// Handles both operand shapes. `(float) Math.exp((double) v)`
    /// leaves an F32 on the stack when the `f2d` collapsed, and a
    /// genuine double otherwise; the arithmetic is the same either
    /// way, only the surrounding conversions differ.
    fn exp_approx(&mut self) -> Result<(), LoweringError> {
        // 0x3FB8AA3B is log2(e) rounded to f32.
        const LOG2_E: &str = "0f3FB8AA3B";
        let operand = self.stack.pop()?;
        let x32 = match operand.kind {
            RegKind::F32 => operand,
            RegKind::F64 => {
                let narrowed = self.regs.fresh_reg(RegKind::F32);
                writeln!(
                    self.body,
                    "    cvt.rn.f32.f64 {}, {};",
                    narrowed.name, operand.name
                )
                .unwrap();
                narrowed
            }
            other => {
                return Err(LoweringError::UnsupportedNode(format!(
                    "Math.exp expects a floating-point operand, got {other:?}"
                )))
            }
        };
        let scaled = self.regs.fresh_reg(RegKind::F32);
        let result = self.regs.fresh_reg(RegKind::F32);
        writeln!(
            self.body,
            "    mul.rn.f32 {}, {}, {LOG2_E};",
            scaled.name, x32.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    ex2.approx.f32 {}, {};",
            result.name, scaled.name
        )
        .unwrap();
        let widened = self.regs.fresh_reg(RegKind::F64);
        writeln!(
            self.body,
            "    cvt.f64.f32 {}, {};",
            widened.name, result.name
        )
        .unwrap();
        self.stack.push(widened);
        Ok(())
    }

    /// `Float.float16ToFloat(short)`.
    ///
    /// The operand arrives as a sign-extended `S32` (that is what a
    /// `short` is on the JVM operand stack, and what `saload` leaves
    /// behind). PTX `cvt.f32.f16` wants the raw 16 bits in a `.b16`
    /// register, so the low half is narrowed first; the sign
    /// extension in the discarded upper half is exactly the bits the
    /// narrowing drops.
    ///
    /// Exact by construction — f16 -> f32 is a widening conversion
    /// with no rounding mode to get wrong, denormals and NaNs
    /// included.
    fn float16_to_float(&mut self) -> Result<(), LoweringError> {
        let bits = self.stack.pop()?;
        if bits.kind != RegKind::S32 {
            return Err(LoweringError::UnsupportedNode(format!(
                "Float.float16ToFloat expects a short (S32 register), got {:?}",
                bits.kind
            )));
        }
        let half = self.regs.fresh_reg(RegKind::B16);
        let out = self.regs.fresh_reg(RegKind::F32);
        writeln!(self.body, "    cvt.u16.u32 {}, {};", half.name, bits.name).unwrap();
        writeln!(self.body, "    cvt.f32.f16 {}, {};", out.name, half.name).unwrap();
        self.stack.push(out);
        Ok(())
    }

    /// `Math.abs(int)` / `StrictMath.abs(int)`.
    ///
    /// JLS/javadoc: "if the argument is equal to the value of
    /// `Integer.MIN_VALUE`, the most negative representable `int`
    /// value, the result is that same value" — i.e. Java's `abs` is the
    /// two's-complement WRAPPING absolute value, not a saturating one.
    ///
    /// The PTX ISA's `abs.s32` mnemonic is documented only as `d =
    /// |a|`, with no stated behaviour at the `Integer.MIN_VALUE`
    /// boundary (where the mathematical absolute value `2^31` does not
    /// fit in a signed 32-bit result). Rather than depend on an
    /// unverified assumption about how a given `ptxas`/driver resolves
    /// that gap, this emits the standard branchless two's-complement
    /// absolute-value identity, built entirely from instructions that
    /// PTX gives an unambiguous, overflow-defined meaning to
    /// (arithmetic shift, xor, wrapping subtract — none of these have
    /// an "undefined at the boundary" gap the way `abs` might):
    ///
    /// ```text
    /// mask = a >> 31        // arithmetic shift: all-1s if a < 0, else 0
    /// abs  = (a ^ mask) - mask
    /// ```
    ///
    /// For `a == Integer.MIN_VALUE` (`0x80000000`): `mask = -1`
    /// (`0xFFFFFFFF`), `a ^ mask = 0x7FFFFFFF` (`Integer.MAX_VALUE`),
    /// and `0x7FFFFFFF - 0xFFFFFFFF` wraps (two's-complement subtract,
    /// same as Java `int` arithmetic) back to `0x80000000` —
    /// bit-for-bit `Integer.MIN_VALUE`, exactly what `Math.abs` returns.
    /// For any other `a` this is the textbook branchless `abs`, with no
    /// boundary case at all.
    fn abs_i32(&mut self) -> Result<(), LoweringError> {
        let a = self.stack.pop()?;
        let mask = self.regs.fresh_reg(RegKind::S32);
        let flipped = self.regs.fresh_reg(RegKind::S32);
        let r = self.regs.fresh_reg(RegKind::S32);
        writeln!(self.body, "    shr.s32 {}, {}, 31;", mask.name, a.name).unwrap();
        writeln!(
            self.body,
            "    xor.b32 {}, {}, {};",
            flipped.name, a.name, mask.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    sub.s32 {}, {}, {};",
            r.name, flipped.name, mask.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    /// `Math.abs(long)` / `StrictMath.abs(long)` — the 64-bit twin of
    /// [`abs_i32`](Self::abs_i32); see its doc comment for the full
    /// derivation (`shr.s64` by 63, `xor.b64`, `sub.s64`; `Long.MIN_VALUE`
    /// wraps back to itself the same way `Integer.MIN_VALUE` does).
    fn abs_i64(&mut self) -> Result<(), LoweringError> {
        let a = self.stack.pop()?;
        let mask = self.regs.fresh_reg(RegKind::S64);
        let flipped = self.regs.fresh_reg(RegKind::S64);
        let r = self.regs.fresh_reg(RegKind::S64);
        writeln!(self.body, "    shr.s64 {}, {}, 63;", mask.name, a.name).unwrap();
        writeln!(
            self.body,
            "    xor.b64 {}, {}, {};",
            flipped.name, a.name, mask.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    sub.s64 {}, {}, {};",
            r.name, flipped.name, mask.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    /// `Math.min(int,int)` (`want_min = true`) / `Math.max(int,int)`
    /// (`want_min = false`), and their `StrictMath` twins. Java's
    /// integer `min`/`max` is exactly `a < b ? a : b` / `a > b ? a : b`
    /// — no NaN or signed-zero subtlety like the float/double variants
    /// below — so this is a direct `setp` + `selp`, bit-exact for every
    /// input with no gating needed.
    fn minmax_i32(&mut self, want_min: bool) -> Result<(), LoweringError> {
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let p = self.regs.fresh_reg(RegKind::Pred);
        let r = self.regs.fresh_reg(RegKind::S32);
        let cmp = if want_min {
            "setp.lt.s32"
        } else {
            "setp.gt.s32"
        };
        writeln!(self.body, "    {} {}, {}, {};", cmp, p.name, a.name, b.name).unwrap();
        writeln!(
            self.body,
            "    selp.s32 {}, {}, {}, {};",
            r.name, a.name, b.name, p.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    /// `Math.min(long,long)` / `Math.max(long,long)` — 64-bit twin of
    /// [`minmax_i32`](Self::minmax_i32).
    fn minmax_i64(&mut self, want_min: bool) -> Result<(), LoweringError> {
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let p = self.regs.fresh_reg(RegKind::Pred);
        let r = self.regs.fresh_reg(RegKind::S64);
        let cmp = if want_min {
            "setp.lt.s64"
        } else {
            "setp.gt.s64"
        };
        writeln!(self.body, "    {} {}, {}, {};", cmp, p.name, a.name, b.name).unwrap();
        writeln!(
            self.body,
            "    selp.s64 {}, {}, {}, {};",
            r.name, a.name, b.name, p.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    /// `Math.min(float,float)` (`want_min = true`) / `Math.max(float,float)`
    /// (`want_min = false`), and their `StrictMath` twins.
    ///
    /// Per the javadoc: "If either value is NaN, then the result is
    /// NaN." and "Unlike the numerical comparison operators, this
    /// method considers negative zero to be strictly smaller than
    /// positive zero. If one argument is positive zero and the other
    /// negative zero, the result is negative zero [for `min`; positive
    /// zero for `max`]." PTX's `min.f32`/`max.f32` do NOT implement
    /// either rule (they return the non-NaN operand on a NaN input, and
    /// treat +0.0/-0.0 as equal), so this builds the Java-exact
    /// behaviour explicitly out of ordered predicates and bitwise
    /// selects — the same "narrow to a predicate, then select" idiom
    /// `lcmp`/`cmp_f32` already use:
    ///
    /// 1. `setp.nan.f32 p_a_nan, a, a` — true iff `a` is NaN (PTX's
    ///    `nan` comparator is true when EITHER operand is NaN; comparing
    ///    `a` against itself isolates "is `a` NaN").
    /// 2. `setp.le.f32`/`setp.ge.f32 p_ordered, a, b` (le for `min`, ge
    ///    for `max`) + `selp` — the ordinary case. PTX's ordered
    ///    comparisons are false whenever either operand is NaN, so when
    ///    `a` is not NaN but `b` is, `p_ordered` is false and this
    ///    naturally selects `b` (NaN) — correctly propagating a NaN `b`
    ///    without needing a second NaN check.
    /// 3. Zero handling: `a == 0.0 && b == 0.0` (numeric `==`, so this
    ///    is true for any combination of +0.0/-0.0) selects between the
    ///    ordered result and a bitwise combination of the two operands'
    ///    raw bit patterns — `OR` for `min` (the sign bit ends up set,
    ///    i.e. the result is `-0.0`, iff EITHER operand is `-0.0`,
    ///    matching "the result is negative zero" whenever one of the
    ///    two is), `AND` for `max` (the sign bit ends up set only if
    ///    BOTH are `-0.0`, matching "the result is positive zero"
    ///    whenever the mix is `+0.0`/`-0.0`). This bitwise trick is
    ///    exact for every one of the four sign combinations — verified
    ///    directly against the two javadoc sentences above, not merely
    ///    against a specific `OpenJDK` implementation's source text.
    /// 4. Final `selp` on `p_a_nan` overrides everything above with `a`
    ///    when `a` itself is NaN (step 2's `b`-is-NaN case is already
    ///    handled by the ordered-comparison-is-false fallback, so this
    ///    is the only extra override needed).
    fn minmax_f32(&mut self, want_min: bool) -> Result<(), LoweringError> {
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let p_a_nan = self.regs.fresh_reg(RegKind::Pred);
        let p_a_zero = self.regs.fresh_reg(RegKind::Pred);
        let p_b_zero = self.regs.fresh_reg(RegKind::Pred);
        let p_both_zero = self.regs.fresh_reg(RegKind::Pred);
        let p_ordered = self.regs.fresh_reg(RegKind::Pred);
        let ordered_result = self.regs.fresh_reg(RegKind::F32);
        let bits_a = self.regs.fresh_reg(RegKind::U32);
        let bits_b = self.regs.fresh_reg(RegKind::U32);
        let bits_combined = self.regs.fresh_reg(RegKind::U32);
        let zero_result = self.regs.fresh_reg(RegKind::F32);
        let non_nan_result = self.regs.fresh_reg(RegKind::F32);
        let r = self.regs.fresh_reg(RegKind::F32);

        writeln!(
            self.body,
            "    setp.nan.f32 {}, {}, {};",
            p_a_nan.name, a.name, a.name
        )
        .unwrap();
        let ord_cmp = if want_min {
            "setp.le.f32"
        } else {
            "setp.ge.f32"
        };
        writeln!(
            self.body,
            "    {} {}, {}, {};",
            ord_cmp, p_ordered.name, a.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.f32 {}, {}, {}, {};",
            ordered_result.name, a.name, b.name, p_ordered.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    setp.eq.f32 {}, {}, 0f00000000;",
            p_a_zero.name, a.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    setp.eq.f32 {}, {}, 0f00000000;",
            p_b_zero.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    and.pred {}, {}, {};",
            p_both_zero.name, p_a_zero.name, p_b_zero.name
        )
        .unwrap();
        writeln!(self.body, "    mov.b32 {}, {};", bits_a.name, a.name).unwrap();
        writeln!(self.body, "    mov.b32 {}, {};", bits_b.name, b.name).unwrap();
        let bit_op = if want_min { "or.b32" } else { "and.b32" };
        writeln!(
            self.body,
            "    {} {}, {}, {};",
            bit_op, bits_combined.name, bits_a.name, bits_b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    mov.b32 {}, {};",
            zero_result.name, bits_combined.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.f32 {}, {}, {}, {};",
            non_nan_result.name, zero_result.name, ordered_result.name, p_both_zero.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.f32 {}, {}, {}, {};",
            r.name, a.name, non_nan_result.name, p_a_nan.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    /// `Math.min(double,double)` / `Math.max(double,double)` — the
    /// `f64` twin of [`minmax_f32`](Self::minmax_f32); see its doc
    /// comment for the full derivation. Every `f32` mnemonic/immediate
    /// becomes its `f64`/`b64` counterpart, and the zero-bit-pattern
    /// comparison target widens to the 64-bit all-zero encoding.
    fn minmax_f64(&mut self, want_min: bool) -> Result<(), LoweringError> {
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let p_a_nan = self.regs.fresh_reg(RegKind::Pred);
        let p_a_zero = self.regs.fresh_reg(RegKind::Pred);
        let p_b_zero = self.regs.fresh_reg(RegKind::Pred);
        let p_both_zero = self.regs.fresh_reg(RegKind::Pred);
        let p_ordered = self.regs.fresh_reg(RegKind::Pred);
        let ordered_result = self.regs.fresh_reg(RegKind::F64);
        let bits_a = self.regs.fresh_reg(RegKind::U64);
        let bits_b = self.regs.fresh_reg(RegKind::U64);
        let bits_combined = self.regs.fresh_reg(RegKind::U64);
        let zero_result = self.regs.fresh_reg(RegKind::F64);
        let non_nan_result = self.regs.fresh_reg(RegKind::F64);
        let r = self.regs.fresh_reg(RegKind::F64);

        writeln!(
            self.body,
            "    setp.nan.f64 {}, {}, {};",
            p_a_nan.name, a.name, a.name
        )
        .unwrap();
        let ord_cmp = if want_min {
            "setp.le.f64"
        } else {
            "setp.ge.f64"
        };
        writeln!(
            self.body,
            "    {} {}, {}, {};",
            ord_cmp, p_ordered.name, a.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.f64 {}, {}, {}, {};",
            ordered_result.name, a.name, b.name, p_ordered.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    setp.eq.f64 {}, {}, 0d0000000000000000;",
            p_a_zero.name, a.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    setp.eq.f64 {}, {}, 0d0000000000000000;",
            p_b_zero.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    and.pred {}, {}, {};",
            p_both_zero.name, p_a_zero.name, p_b_zero.name
        )
        .unwrap();
        writeln!(self.body, "    mov.b64 {}, {};", bits_a.name, a.name).unwrap();
        writeln!(self.body, "    mov.b64 {}, {};", bits_b.name, b.name).unwrap();
        let bit_op = if want_min { "or.b64" } else { "and.b64" };
        writeln!(
            self.body,
            "    {} {}, {}, {};",
            bit_op, bits_combined.name, bits_a.name, bits_b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    mov.b64 {}, {};",
            zero_result.name, bits_combined.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.f64 {}, {}, {}, {};",
            non_nan_result.name, zero_result.name, ordered_result.name, p_both_zero.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.f64 {}, {}, {}, {};",
            r.name, a.name, non_nan_result.name, p_a_nan.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    /// `Math.fma(float,float,float)` / `StrictMath.fma(float,float,float)`.
    ///
    /// `invokestatic` args are pushed left-to-right (`a`, then `b`, then
    /// `c`, with `c` on top of the operand stack at the callsite), so
    /// this pops in reverse: `c`, `b`, `a`. PTX's `fma.rn.f32 d, a, b,
    /// c` computes `a*b+c` with a single final rounding — exactly what
    /// both `Math.fma` and `StrictMath.fma` are specified to compute
    /// ("the exact product of the first two arguments... is then added
    /// to the third argument and the result is rounded once"), so this
    /// is bit-exact for every input with no gating hint needed.
    fn fma_f32(&mut self) -> Result<(), LoweringError> {
        let c = self.stack.pop()?;
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::F32);
        writeln!(
            self.body,
            "    fma.rn.f32 {}, {}, {}, {};",
            r.name, a.name, b.name, c.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    /// `Math.fma(double,double,double)` / `StrictMath.fma(double,double,double)`
    /// — the `f64` twin of [`fma_f32`](Self::fma_f32).
    fn fma_f64(&mut self) -> Result<(), LoweringError> {
        let c = self.stack.pop()?;
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::F64);
        writeln!(
            self.body,
            "    fma.rn.f64 {}, {}, {}, {};",
            r.name, a.name, b.name, c.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    // ─────────────────── local-variable helpers ─────────────────────

    fn iload(&mut self, slot: u16) -> Result<(), LoweringError> {
        // If this slot is the induction variable, substitute tid (or
        // tid + K, when `apply_loop_start_offset` has already folded a
        // non-zero loop start `K` into `self.tid_reg` — this call site
        // doesn't need to know which).
        if Some(slot) == self.iv_slot {
            let tid = self
                .tid_reg
                .clone()
                .ok_or_else(|| LoweringError::Internal("tid not initialised".into()))?;
            self.stack.push(tid);
            return Ok(());
        }
        // 2-D nested-loop follow-up: the inner loop's induction variable
        // substitutes to its own recovered index register (`j`), set up
        // by `emit_nested_loop_guard_and_decompose`.
        if Some(slot) == self.iv_slot_inner {
            let j = self.tid_reg_inner.clone().ok_or_else(|| {
                LoweringError::Internal("inner loop index not initialised".into())
            })?;
            self.stack.push(j);
            return Ok(());
        }
        let r = self.locals.get(slot as usize).cloned().ok_or_else(|| {
            LoweringError::UnsupportedNode(format!("iload from uninitialised local {slot}"))
        })?;
        self.stack.push(r);
        Ok(())
    }

    fn lload(&mut self, slot: u16) -> Result<(), LoweringError> {
        let r = self.locals.get(slot as usize).cloned().ok_or_else(|| {
            LoweringError::UnsupportedNode(format!("lload from uninitialised local {slot}"))
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
        let r = self.locals.get(slot as usize).cloned().ok_or_else(|| {
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
        if Some(slot) == self.iv_slot || Some(slot) == self.iv_slot_inner {
            // Induction variable iinc (outer or, for a nested loop,
            // inner) is implicit in the parallel-for dispatch — skip.
            return Ok(());
        }
        let cur = self.locals.get(slot as usize).cloned().ok_or_else(|| {
            LoweringError::UnsupportedNode(format!("iinc on uninitialised local {slot}"))
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
        writeln!(
            self.body,
            "    {} {}, {}, {};",
            mnemonic, r.name, a.name, b.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn binop_i64(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::S64);
        writeln!(
            self.body,
            "    {} {}, {}, {};",
            mnemonic, r.name, a.name, b.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn binop_f32(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::F32);
        // Every float arithmetic mnemonic carries an EXPLICIT `.rn`
        // rounding mode. `div` has always needed one (PTX has no bare
        // `div.f32`), but `add`/`sub`/`mul` accept the bare form and
        // default to round-to-nearest-even — which reads as "already
        // correct" and is not. The PTX ISA makes the rounding modifier
        // the switch that also controls CONTRACTION: an unrounded
        // `mul.f32` feeding an unrounded `add.f32` may be fused by
        // ptxas into a single `fma.rn.f32`, while an operation with an
        // explicit rounding modifier is never contracted.
        //
        // AUDIT 2026-08-21, measured on sm_75 with `ptxas -O3`:
        // `mul.f32` + `add.f32` compiled to one `FFMA`, whereas
        // `mul.rn.f32` + `add.rn.f32` compiled to `FMUL` + `FADD`. The
        // fused form rounds ONCE where JLS §15.17.1/§15.18.2 require
        // the product to be rounded to float before the add, so every
        // kernel containing an `a*b + c` chain silently computed a
        // different (more accurate, but not Java) result on the device
        // than on the CPU. That is the root cause of the 640x480
        // ray-tracer checksum divergence found on 2026-08-21; the
        // earlier fixtures stayed bit-exact only because none of them
        // had a mul-then-add pair to contract. Java has an explicit
        // `Math.fma` for the fused form, lowered by `fma_f32` — fusing
        // is the programmer's call, never the backend's. See the
        // "Float bit-exactness" section of the GPU reference doc.
        //
        // AUDIT 2026-05-16/2026-07-11: PTX still has no `rem.f32`
        // mnemonic, so this single-instruction `binop_f32` helper still
        // has no `"rem.f32"` arm — `frem` (0x72) is not a `binop_f32`
        // at all, it dispatches to the dedicated multi-instruction
        // `frem_f32` helper instead (see its doc comment).
        let m = match mnemonic {
            "add.f32" => "add.rn.f32",
            "sub.f32" => "sub.rn.f32",
            "mul.f32" => "mul.rn.f32",
            "div.f32" => "div.rn.f32",
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
        // Same explicit-`.rn` rule as `binop_f32` — see its comment for
        // why the bare mnemonics are a bit-exactness hazard rather than
        // a harmless default.
        let m = match mnemonic {
            "add.f64" => "add.rn.f64",
            "sub.f64" => "sub.rn.f64",
            "mul.f64" => "mul.rn.f64",
            "div.f64" => "div.rn.f64",
            other => other,
        };
        writeln!(self.body, "    {} {}, {}, {};", m, r.name, a.name, b.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    /// AUDIT 2026-05-16: Java throws ArithmeticException on integer
    /// divide-by-zero and silently wraps `INT_MIN / -1`. PTX
    /// `div.s32`/`rem.s32` are undefined in both cases. In strict mode,
    /// guard with predicates that branch to the deopt exit
    /// (`bounds_fail_label`, which sets the failure flag and returns —
    /// the VM then re-runs the method on the CPU with proper Java
    /// semantics). `ALLOW_DIV_BY_ZERO` skips only the zero-divisor guard.
    fn div_or_rem_i32(&mut self, mnemonic: &str, is_div: bool) -> Result<(), LoweringError> {
        let b = self.stack.pop()?; // divisor
        let a = self.stack.pop()?; // dividend
        let guard_zero = !self.sig.allow_div_by_zero;
        self.used_bounds_label |= guard_zero || is_div;
        if guard_zero {
            // Zero-divisor guard (applies to both div and rem).
            let p_zero = self.regs.fresh_reg(RegKind::Pred);
            writeln!(self.body, "    setp.eq.s32 {}, {}, 0;", p_zero.name, b.name).unwrap();
            writeln!(
                self.body,
                "    @{} bra {};",
                p_zero.name, self.bounds_fail_label
            )
            .unwrap();
        }
        // INT_MIN / -1 overflow guard (div only; Java rem of INT_MIN by
        // -1 is defined as 0 so PTX rem.s32 needs no extra guard once
        // the divisor is non-zero).
        if is_div {
            let p_min = self.regs.fresh_reg(RegKind::Pred);
            let p_neg1 = self.regs.fresh_reg(RegKind::Pred);
            let p_overflow = self.regs.fresh_reg(RegKind::Pred);
            // PTX accepts hex literals for setp immediates; using
            // 0x80000000 avoids parsing `-2147483648` (where the
            // unary-minus operand 2147483648 overflows i32 in some
            // assemblers).
            writeln!(
                self.body,
                "    setp.eq.s32 {}, {}, 0x80000000;",
                p_min.name, a.name
            )
            .unwrap();
            writeln!(
                self.body,
                "    setp.eq.s32 {}, {}, -1;",
                p_neg1.name, b.name
            )
            .unwrap();
            writeln!(
                self.body,
                "    and.pred {}, {}, {};",
                p_overflow.name, p_min.name, p_neg1.name
            )
            .unwrap();
            writeln!(
                self.body,
                "    @{} bra {};",
                p_overflow.name, self.bounds_fail_label
            )
            .unwrap();
        }
        let r = self.regs.fresh_reg(RegKind::S32);
        writeln!(
            self.body,
            "    {} {}, {}, {};",
            mnemonic, r.name, a.name, b.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    /// AUDIT 2026-05-16: 64-bit twin of `div_or_rem_i32`. LONG_MIN is
    /// -9223372036854775808 and the divide-by-zero / overflow rules
    /// match the 32-bit case (JLS §15.17.2).
    fn div_or_rem_i64(&mut self, mnemonic: &str, is_div: bool) -> Result<(), LoweringError> {
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let guard_zero = !self.sig.allow_div_by_zero;
        self.used_bounds_label |= guard_zero || is_div;
        if guard_zero {
            let p_zero = self.regs.fresh_reg(RegKind::Pred);
            writeln!(self.body, "    setp.eq.s64 {}, {}, 0;", p_zero.name, b.name).unwrap();
            writeln!(
                self.body,
                "    @{} bra {};",
                p_zero.name, self.bounds_fail_label
            )
            .unwrap();
        }
        if is_div {
            let p_min = self.regs.fresh_reg(RegKind::Pred);
            let p_neg1 = self.regs.fresh_reg(RegKind::Pred);
            let p_overflow = self.regs.fresh_reg(RegKind::Pred);
            // LONG_MIN as hex (see s32 comment above for rationale).
            writeln!(
                self.body,
                "    setp.eq.s64 {}, {}, 0x8000000000000000;",
                p_min.name, a.name
            )
            .unwrap();
            writeln!(
                self.body,
                "    setp.eq.s64 {}, {}, -1;",
                p_neg1.name, b.name
            )
            .unwrap();
            writeln!(
                self.body,
                "    and.pred {}, {}, {};",
                p_overflow.name, p_min.name, p_neg1.name
            )
            .unwrap();
            writeln!(
                self.body,
                "    @{} bra {};",
                p_overflow.name, self.bounds_fail_label
            )
            .unwrap();
        }
        let r = self.regs.fresh_reg(RegKind::S64);
        writeln!(
            self.body,
            "    {} {}, {}, {};",
            mnemonic, r.name, a.name, b.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    /// AUDIT 2026-07-11: `frem` (0x72) — Java's `%` on `float` operands.
    ///
    /// Java `frem`/`drem` (JLS §15.17.3) is the "fmod" flavour of
    /// remainder: `result = dividend - trunc(dividend / divisor) *
    /// divisor`, where `trunc` rounds toward zero and the result
    /// carries the SIGN OF THE DIVIDEND. This is deliberately NOT
    /// `Math.IEEEremainder`, which rounds the quotient to the NEAREST
    /// integer (ties to even) instead of truncating — the two
    /// disagree whenever the true quotient's fractional part exceeds
    /// 0.5.
    ///
    /// PTX has no `rem.f32` mnemonic (see the AUDIT 2026-05-16/
    /// 2026-07-11 comment at the `0x72`/`0x73` arms in `emit_op`), so
    /// this builds the fmod identity out of instructions that DO
    /// exist:
    ///
    /// 1. `div.rn.f32 t, a, b`         — correctly-rounded quotient.
    /// 2. `cvt.rzi.f32.f32 t, t`       — truncate `t` toward zero,
    ///    staying in `f32` (this is exactly `truncf`).
    /// 3. `neg.f32 negt, t; fma.rn.f32 r, negt, b, a` — `r = a - t*b`,
    ///    with the multiply carried at full precision internally by
    ///    the FMA and only ONE final rounding, instead of a separate
    ///    `mul.f32` + `sub.f32` (two roundings, strictly more error).
    ///
    /// ── Why step 3 is exact once `t` is the right integer ──
    ///
    /// The true mathematical fmod result is always exactly
    /// representable in the source type, for ANY finite `a` and finite
    /// nonzero `b` — this follows because fmod can equivalently be
    /// computed as a finite sequence of Sterbenz-safe subtractions
    /// (`while |a| >= |b|: a -= sign-matched (b * 2^k)`), and each such
    /// subtraction is individually exact, so the result of the whole
    /// process is too. An FMA computes `negt * b + a` as if the
    /// product were formed at infinite precision and rounded only
    /// once at the very end; if `t` is exactly `trunc(a/b)`, that
    /// infinite-precision intermediate value IS the true fmod result,
    /// which we just established is already exactly representable —
    /// so the FMA's single rounding step has nothing to round away.
    /// Given a correct `t`, this sequence is therefore bit-exact, not
    /// merely "correctly rounded".
    ///
    /// ── Why this is gated behind `AdmissionHint::AllowDivByZero` ──
    ///
    /// Step 1 only recovers the EXACT mathematical integer quotient
    /// `trunc(a/b)` when that quotient's magnitude is small enough
    /// that its ULP is < 1 — i.e. `|a/b| < 2^24` for `f32` (24-bit
    /// significand including the implicit leading bit). Outside that
    /// range, a single correctly-rounded division can land on the
    /// wrong side of an integer boundary (worst case: the true
    /// quotient's fractional part is adversarially close to 0 or 1),
    /// so `cvt.rzi` truncates to an integer that is off by one (or
    /// more) from the true quotient. Step 3 then computes `a - t*b`
    /// for the WRONG `t`, so the result is wrong by a whole multiple
    /// of `b` — not a rounding-level error, a flatly incorrect answer.
    /// A fully general, always-exact `fmod` needs an
    /// arbitrary-precision shift-and-subtract reduction (what glibc's
    /// `__ieee754_fmodf` does); that is out of scope for a
    /// single-kernel PTX emitter. See `analyzer::Reason::FloatRemainder`
    /// and `analyzer::classify`'s `0x72 | 0x73` arm for the analyzer
    /// side of this gate — `AllowDivByZero` is reused rather than a
    /// dedicated hint because minting a new `AdmissionHint` variant
    /// would require editing `annotations.rs`, out of scope for this
    /// change; it is already the "accept looser numeric edge-case
    /// semantics for a division-family opcode" opt-in, and `frem` is
    /// literally the floating counterpart of `irem`.
    ///
    /// ── The one edge case patched explicitly: infinite divisor ──
    ///
    /// JLS §15.17.3 requires `a % b == a` whenever `a` is finite and
    /// `b` is `±Infinity`. Steps 1-3 alone get this WRONG: `t = a /
    /// ±Infinity` rounds to a signed zero, so step 3 computes `∓0 *
    /// ±Infinity`, which IEEE 754 defines as NaN (zero times
    /// infinity) — the naive sequence would silently return NaN
    /// instead of `a`. A finite dividend divided by an infinite
    /// divisor is not an exotic input (e.g. a prior overflow feeding
    /// an infinity into this expression), so the extra instructions
    /// are worth it: compute `|b|`, compare it against the canonical
    /// `+Infinity` bit pattern, and `selp` the dividend `a` directly
    /// whenever the divisor is infinite. NaN operands are unaffected:
    /// `NaN == Infinity` is false under IEEE 754 `eq`, so a NaN
    /// divisor still falls through to the fma path (which already
    /// propagates NaN correctly through every step), and a NaN
    /// dividend selects `a` = NaN on either path.
    ///
    /// ── Deliberately NOT patched: sign of an exactly-zero result ──
    ///
    /// JLS also requires the result's sign to always match the
    /// dividend's sign, even when the numeric result is zero (e.g.
    /// `-0.0f % 5.0f` must yield `-0.0f`, not `+0.0f`). Depending on
    /// rounding, `fma(negt, b, a)` can produce `+0.0` where Java wants
    /// `-0.0`, because IEEE 754 addition of two zeros of opposite sign
    /// rounds to `+0` under round-to-nearest. This is a real, known
    /// deviation from JLS bit-for-bit sign semantics that is NOT
    /// corrected here — chasing every signed-zero corner case would
    /// need extra per-sign predicated logic for a difference that is
    /// numerically zero either way. Callers who need bit-exact
    /// signed-zero results must not offload a method containing
    /// `frem`/`drem` under `AllowDivByZero`.
    fn frem_f32(&mut self) -> Result<(), LoweringError> {
        let b = self.stack.pop()?; // divisor
        let a = self.stack.pop()?; // dividend
        let t = self.regs.fresh_reg(RegKind::F32);
        let t_trunc = self.regs.fresh_reg(RegKind::F32);
        let neg_t = self.regs.fresh_reg(RegKind::F32);
        let naive = self.regs.fresh_reg(RegKind::F32);
        let abs_b = self.regs.fresh_reg(RegKind::F32);
        let p_inf_divisor = self.regs.fresh_reg(RegKind::Pred);
        let result = self.regs.fresh_reg(RegKind::F32);
        writeln!(
            self.body,
            "    div.rn.f32 {}, {}, {};",
            t.name, a.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    cvt.rzi.f32.f32 {}, {};",
            t_trunc.name, t.name
        )
        .unwrap();
        writeln!(self.body, "    neg.f32 {}, {};", neg_t.name, t_trunc.name).unwrap();
        writeln!(
            self.body,
            "    fma.rn.f32 {}, {}, {}, {};",
            naive.name, neg_t.name, b.name, a.name
        )
        .unwrap();
        writeln!(self.body, "    abs.f32 {}, {};", abs_b.name, b.name).unwrap();
        writeln!(
            self.body,
            "    setp.eq.f32 {}, {}, 0f7F800000;",
            p_inf_divisor.name, abs_b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.f32 {}, {}, {}, {};",
            result.name, a.name, naive.name, p_inf_divisor.name
        )
        .unwrap();
        self.stack.push(result);
        Ok(())
    }

    /// `drem` (0x73) — the `f64` twin of [`frem_f32`]. See that
    /// function's doc comment for the full JLS §15.17.3 derivation,
    /// the precision boundary (`|a/b| < 2^53` here, `double`'s 53-bit
    /// significand, rather than `f32`'s `2^24`), and exactly which
    /// edge cases are and are not handled bit-exactly. The PTX
    /// sequence is identical, mnemonic-for-mnemonic, with every `f32`
    /// swapped for `f64` and the `+Infinity` bit pattern widened to
    /// the 64-bit encoding.
    fn drem_f64(&mut self) -> Result<(), LoweringError> {
        let b = self.stack.pop()?; // divisor
        let a = self.stack.pop()?; // dividend
        let t = self.regs.fresh_reg(RegKind::F64);
        let t_trunc = self.regs.fresh_reg(RegKind::F64);
        let neg_t = self.regs.fresh_reg(RegKind::F64);
        let naive = self.regs.fresh_reg(RegKind::F64);
        let abs_b = self.regs.fresh_reg(RegKind::F64);
        let p_inf_divisor = self.regs.fresh_reg(RegKind::Pred);
        let result = self.regs.fresh_reg(RegKind::F64);
        writeln!(
            self.body,
            "    div.rn.f64 {}, {}, {};",
            t.name, a.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    cvt.rzi.f64.f64 {}, {};",
            t_trunc.name, t.name
        )
        .unwrap();
        writeln!(self.body, "    neg.f64 {}, {};", neg_t.name, t_trunc.name).unwrap();
        writeln!(
            self.body,
            "    fma.rn.f64 {}, {}, {}, {};",
            naive.name, neg_t.name, b.name, a.name
        )
        .unwrap();
        writeln!(self.body, "    abs.f64 {}, {};", abs_b.name, b.name).unwrap();
        writeln!(
            self.body,
            "    setp.eq.f64 {}, {}, 0d7FF0000000000000;",
            p_inf_divisor.name, abs_b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.f64 {}, {}, {}, {};",
            result.name, a.name, naive.name, p_inf_divisor.name
        )
        .unwrap();
        self.stack.push(result);
        Ok(())
    }

    /// `lcmp` (0x94) — JVMS §6.5 `lcmp`: pop `value2` then `value1` (two
    /// `long`s), push the `int`
    ///
    /// ```text
    ///  1   if value1 >  value2
    ///  0   if value1 == value2
    /// -1   if value1 <  value2
    /// ```
    ///
    /// PTX has no three-way integer compare, so this builds the result
    /// out of two ordered predicates and a `selp` chain — the same
    /// "narrow to a predicate, then select" idiom `div_or_rem_i32`'s
    /// zero-divisor guard uses, just producing a value instead of a
    /// branch:
    ///
    /// 1. `setp.gt.s64 pg, a, b`  — `pg` is true iff `a > b`.
    /// 2. `setp.lt.s64 pl, a, b`  — `pl` is true iff `a < b`.
    /// 3. `selp.s32 not_gt, -1, 0, pl` — `not_gt` = `-1` if `a < b`,
    ///    else `0` (the `a == b` case, since `pl` is false there).
    /// 4. `selp.s32 r, 1, not_gt, pg` — `r` = `1` if `a > b`, else
    ///    `not_gt` from step 3.
    ///
    /// Integers have no NaN-like non-total-order case, so unlike
    /// [`cmp_f32`]/[`cmp_f64`] no third predicate is needed — `pg` and
    /// `pl` are exhaustive and mutually exclusive, so every input lands
    /// on exactly one of the three branches. This lowering is therefore
    /// bit-exact for every `long` input, with no gating hint required
    /// (contrast [`frem_f32`]/[`drem_f64`], which need
    /// `AdmissionHint::AllowDivByZero` because their fma-based identity
    /// has a precision boundary — a compare has none).
    ///
    /// See the AUDIT comment at the `0x94` arm in `emit_op` for why this
    /// building block is not yet reachable end-to-end from real javac
    /// output (every real `lcmp` is immediately consumed by a following
    /// `if<cond>` branch, which still rejects) and
    /// `lowering.rs`'s `lcmp_lowers_to_setp_selp_chain` test for the
    /// hand-built body that exercises this function directly.
    fn lcmp(&mut self) -> Result<(), LoweringError> {
        let b = self.stack.pop()?; // value2
        let a = self.stack.pop()?; // value1
        let p_gt = self.regs.fresh_reg(RegKind::Pred);
        let p_lt = self.regs.fresh_reg(RegKind::Pred);
        let not_gt = self.regs.fresh_reg(RegKind::S32);
        let r = self.regs.fresh_reg(RegKind::S32);
        writeln!(
            self.body,
            "    setp.gt.s64 {}, {}, {};",
            p_gt.name, a.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    setp.lt.s64 {}, {}, {};",
            p_lt.name, a.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.s32 {}, -1, 0, {};",
            not_gt.name, p_lt.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.s32 {}, 1, {}, {};",
            r.name, not_gt.name, p_gt.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    /// `fcmpl` (0x95, `nan_result = -1`) / `fcmpg` (0x96, `nan_result =
    /// 1`) — JVMS §6.5 `fcmpl`/`fcmpg`: pop `value2` then `value1` (two
    /// `float`s), push the `int`
    ///
    /// ```text
    ///  1   if value1 >  value2
    ///  0   if value1 == value2
    /// -1   if value1 <  value2
    /// nan_result  if value1 or value2 is NaN
    /// ```
    ///
    /// where `nan_result` is `-1` for `fcmpl` and `+1` for `fcmpg` — the
    /// two opcodes exist ONLY so the compiler can choose whichever one
    /// makes a source-level `<`/`>` comparison involving NaN evaluate to
    /// `false` (JLS §15.20.1): `a < b` lowers to `fcmpg; ifge` (a NaN
    /// operand must make the branch NOT taken, so the synthetic
    /// `nan_result = +1` must fail `< 0`), while `a > b` lowers to
    /// `fcmpl; ifle` (same requirement, mirrored).
    ///
    /// Building on the ordered-predicate idiom from [`lcmp`]:
    ///
    /// 1. `setp.gt.f32 pg, a, b` / `setp.lt.f32 pl, a, b` — PTX's
    ///    ordered `gt`/`lt` comparisons are false whenever either
    ///    operand is NaN (IEEE 754 unordered compare), so `pg`/`pl`
    ///    alone would silently fall through to the `a == b` case (`0`)
    ///    for a NaN input — wrong per the table above.
    /// 2. `setp.nan.f32 pn, a, b` — PTX's `nan` comparison operator is
    ///    true iff EITHER operand is NaN; this is exactly the override
    ///    condition needed.
    /// 3. Compute the ordered `not_gt`/`ordered` result via the same
    ///    two `selp`s as [`lcmp`] (steps 3-4 there).
    /// 4. `selp.s32 r, nan_result, ordered, pn` — override with the
    ///    caller-supplied NaN default when `pn` is true.
    ///
    /// A NaN operand is not an exotic input for a kernel fed by prior
    /// floating-point computation (e.g. a prior `0.0f / 0.0f`), so the
    /// extra predicate is worth it rather than leaving the NaN case
    /// silently wrong the way the pre-2026-07-11 `frem`/`drem` lowering
    /// did before its own infinite-divisor patch.
    fn cmp_f32(&mut self, nan_result: i32) -> Result<(), LoweringError> {
        let b = self.stack.pop()?; // value2
        let a = self.stack.pop()?; // value1
        let p_gt = self.regs.fresh_reg(RegKind::Pred);
        let p_lt = self.regs.fresh_reg(RegKind::Pred);
        let p_nan = self.regs.fresh_reg(RegKind::Pred);
        let not_gt = self.regs.fresh_reg(RegKind::S32);
        let ordered = self.regs.fresh_reg(RegKind::S32);
        let r = self.regs.fresh_reg(RegKind::S32);
        writeln!(
            self.body,
            "    setp.gt.f32 {}, {}, {};",
            p_gt.name, a.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    setp.lt.f32 {}, {}, {};",
            p_lt.name, a.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    setp.nan.f32 {}, {}, {};",
            p_nan.name, a.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.s32 {}, -1, 0, {};",
            not_gt.name, p_lt.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.s32 {}, 1, {}, {};",
            ordered.name, not_gt.name, p_gt.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.s32 {}, {}, {}, {};",
            r.name, nan_result, ordered.name, p_nan.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    /// `dcmpl` (0x97, `nan_result = -1`) / `dcmpg` (0x98, `nan_result =
    /// 1`) — the `f64` twin of [`cmp_f32`]. See that function's doc
    /// comment for the full JVMS §6.5 / JLS §15.20.1 derivation of why
    /// two opcodes exist and how `nan_result` implements the
    /// compiler's choice between them; the PTX sequence here is
    /// identical, mnemonic-for-mnemonic, with every `f32` swapped for
    /// `f64` (mirroring how [`drem_f64`] relates to [`frem_f32`]).
    fn cmp_f64(&mut self, nan_result: i32) -> Result<(), LoweringError> {
        let b = self.stack.pop()?; // value2
        let a = self.stack.pop()?; // value1
        let p_gt = self.regs.fresh_reg(RegKind::Pred);
        let p_lt = self.regs.fresh_reg(RegKind::Pred);
        let p_nan = self.regs.fresh_reg(RegKind::Pred);
        let not_gt = self.regs.fresh_reg(RegKind::S32);
        let ordered = self.regs.fresh_reg(RegKind::S32);
        let r = self.regs.fresh_reg(RegKind::S32);
        writeln!(
            self.body,
            "    setp.gt.f64 {}, {}, {};",
            p_gt.name, a.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    setp.lt.f64 {}, {}, {};",
            p_lt.name, a.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    setp.nan.f64 {}, {}, {};",
            p_nan.name, a.name, b.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.s32 {}, -1, 0, {};",
            not_gt.name, p_lt.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.s32 {}, 1, {}, {};",
            ordered.name, not_gt.name, p_gt.name
        )
        .unwrap();
        writeln!(
            self.body,
            "    selp.s32 {}, {}, {}, {};",
            r.name, nan_result, ordered.name, p_nan.name
        )
        .unwrap();
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
        // AUDIT 2026-05-16: JLS §5.4.2 requires the shift count to be
        // masked by 0x1F for 32-bit shifts; PTX `shl/shr` with a count
        // ≥ the operand width is undefined. Mask explicitly into a
        // fresh register before issuing the shift.
        let masked = self.regs.fresh_reg(RegKind::S32);
        writeln!(self.body, "    and.b32 {}, {}, 31;", masked.name, b.name).unwrap();
        // PTX shifts accept either signedness for the count operand.
        writeln!(
            self.body,
            "    {} {}, {}, {};",
            mnemonic, r.name, a.name, masked.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    fn shift_i64(&mut self, mnemonic: &str) -> Result<(), LoweringError> {
        // JVM long-shift count is an int — top of stack is s32.
        let b = self.stack.pop()?;
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::S64);
        // AUDIT 2026-05-16: JLS §5.4.2 requires the shift count to be
        // masked by 0x3F for 64-bit shifts. PTX 64-bit shifts still
        // take a 32-bit count operand, so the mask stays in s32.
        let masked = self.regs.fresh_reg(RegKind::S32);
        writeln!(self.body, "    and.b32 {}, {}, 63;", masked.name, b.name).unwrap();
        writeln!(
            self.body,
            "    {} {}, {}, {};",
            mnemonic, r.name, a.name, masked.name
        )
        .unwrap();
        self.stack.push(r);
        Ok(())
    }

    // ─────────────────── conversions ────────────────────────────────

    fn conv(&mut self, mnemonic: &str, _from: RegKind, to: RegKind) -> Result<(), LoweringError> {
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

    /// Zero-extend the low 16 bits of an int — the JVMS `i2c` semantics.
    /// Java `char` is an UNSIGNED 16-bit value, so `i2c` masks to the low
    /// 16 bits rather than sign-extending (which is what
    /// `conv_truncate_i32(16)` does for the signed `i2b`/`i2s`). For an
    /// input with bit 15 set (e.g. 0xFFFF) this yields 65535, not -1.
    fn conv_zext_u16(&mut self) -> Result<(), LoweringError> {
        let a = self.stack.pop()?;
        let r = self.regs.fresh_reg(RegKind::S32);
        writeln!(self.body, "    and.b32 {}, {}, 0xFFFF;", r.name, a.name).unwrap();
        self.stack.push(r);
        Ok(())
    }

    // AUDIT 2026-05-19: `cmp_long_or_float` was removed — it always
    // returned `Err`, so every `*cmp*` opcode rejected explicitly at the
    // `emit_op` dispatch site instead of routing through dead code.
    //
    // AUDIT 2026-07-11: `*cmp*` (0x94-0x98) no longer rejects at all —
    // see `lcmp`/`cmp_f32`/`cmp_f64` above (grouped with `frem_f32`/
    // `drem_f64`, the other numeric-edge-case-heavy opcodes) and the
    // AUDIT comment at the `0x94` arm in `emit_op`.

    // ─────────────────── array ops ──────────────────────────────────

    /// Find the array param backing the top-of-stack array reference.
    /// Returns (param_index, kind). The reference itself must have
    /// been produced by an `aload` of a parameter local.
    fn array_param_of(&self, array_reg: &Reg) -> Result<(usize, ParamKind), LoweringError> {
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
                return Ok((i, *k));
            }
        }
        Err(LoweringError::UnsupportedNode(
            "array reference does not flow from a method parameter (\
             non-parameter array references are not supported)"
                .into(),
        ))
    }

    /// Record that `param_idx` is read element-wise by the body.
    ///
    /// Same 64-param saturation rule as `writes_param_mask`: a kernel with
    /// more than 64 params sets every bit, which reads as "assume every
    /// array is read" and only ever refuses chunking. Correctness over
    /// performance, in the safe direction.
    fn mark_param_read(&mut self, param_idx: usize) {
        self.reads_param_mask |= if param_idx < 64 {
            1u64 << param_idx
        } else {
            u64::MAX
        };
    }

    fn array_load(
        &mut self,
        elem_kind: RegKind,
        suffix: &str,
        elem_size: usize,
    ) -> Result<(), LoweringError> {
        let index = self.stack.pop()?;
        let array_ref = self.stack.pop()?;
        let (param_idx, _kind) = self.array_param_of(&array_ref)?;
        // Element read: see `reads_param_mask` (chunked-writeback guard).
        self.mark_param_read(param_idx);
        self.emit_bounds_check(&index, param_idx);
        let addr = self.emit_element_addr(&index, &array_ref, elem_size);
        let result = self.regs.fresh_reg(elem_kind);
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
        let (param_idx, _kind) = self.array_param_of(&array_ref)?;
        // Element read: see `reads_param_mask` (chunked-writeback guard).
        self.mark_param_read(param_idx);
        self.emit_bounds_check(&index, param_idx);
        let addr = self.emit_element_addr(&index, &array_ref, 1);
        let raw = self.regs.fresh_reg(RegKind::S32);
        let result = self.regs.fresh_reg(RegKind::S32);
        // Load signed 8-bit, widen to 32-bit (sign-extended) — Java baload semantics.
        writeln!(self.body, "    ld.global.s8 {}, [{}];", raw.name, addr.name).unwrap();
        writeln!(self.body, "    cvt.s32.s8 {}, {};", result.name, raw.name).unwrap();
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
        let (param_idx, _kind) = self.array_param_of(&array_ref)?;
        // Element read: see `reads_param_mask` (chunked-writeback guard).
        self.mark_param_read(param_idx);
        self.emit_bounds_check(&index, param_idx);
        let addr = self.emit_element_addr(&index, &array_ref, 2);
        let raw = self.regs.fresh_reg(RegKind::S32);
        let result = self.regs.fresh_reg(RegKind::S32);
        let load_suffix = if is_char { "u16" } else { "s16" };
        let cvt = if is_char {
            "cvt.u32.u16"
        } else {
            "cvt.s32.s16"
        };
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
        let (param_idx, _kind) = self.array_param_of(&array_ref)?;
        // Phase 10 #2 — mark this param as written so the marshaller
        // can skip the post-launch D→H copy for read-only inputs. The
        // mask is a 64-bit field; if a (purely hypothetical) kernel
        // ever exceeds 64 params, conservatively flip every bit so
        // the marshaller treats all params as written and runs the
        // pre-Phase-10 #2 D→H-everywhere path. Setting all bits is
        // the safe direction: correctness over performance.
        self.writes_param_mask |= if param_idx < 64 {
            1u64 << param_idx
        } else {
            u64::MAX
        };
        self.emit_bounds_check(&index, param_idx);
        let addr = self.emit_element_addr(&index, &array_ref, elem_size);
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
        let (param_idx, _kind) = self.array_param_of(&array_ref)?;
        // Phase 10 #2 — see `array_store` for rationale.
        self.writes_param_mask |= if param_idx < 64 {
            1u64 << param_idx
        } else {
            u64::MAX
        };
        self.emit_bounds_check(&index, param_idx);
        let addr = self.emit_element_addr(&index, &array_ref, 1);
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
        let (param_idx, _kind) = self.array_param_of(&array_ref)?;
        // Phase 10 #2 — see `array_store` for rationale.
        self.writes_param_mask |= if param_idx < 64 {
            1u64 << param_idx
        } else {
            u64::MAX
        };
        self.emit_bounds_check(&index, param_idx);
        let addr = self.emit_element_addr(&index, &array_ref, elem_size);
        writeln!(
            self.body,
            "    st.global.s16 [{}], {};",
            addr.name, value.name
        )
        .unwrap();
        Ok(())
    }

    /// `arraylength` — the parameter's `pN_len`, which the prologue
    /// already holds in a register.
    ///
    /// AUDIT 2026-09-02: this issued its own `ld.param.s32`, so a kernel
    /// whose loop bound is written `a.length` inline loaded the same
    /// kernel parameter twice. Reusing the hoisted register also makes
    /// the value the loop bound compares against and the value the
    /// bounds check compares against the SAME register, which is the
    /// identity `Emitter::prove_index_within_param` matches on.
    fn arraylength(&mut self) -> Result<(), LoweringError> {
        let array_ref = self.stack.pop()?;
        let (param_idx, _kind) = self.array_param_of(&array_ref)?;
        let name = self.param_len_operand(param_idx);
        let r = self
            .param_len_reg[param_idx]
            .clone()
            .unwrap_or_else(|| Reg {
                kind: RegKind::S32,
                name,
                wide: false,
            });
        self.stack.push(r);
        Ok(())
    }

    fn scalar_return(&mut self, kind: RegKind, suffix: &str) -> Result<(), LoweringError> {
        let value = self.stack.pop()?;
        let ret_ptr = self.regs.fresh_reg(RegKind::U64);
        writeln!(self.body, "    ld.param.u64 {}, [ret_ptr];", ret_ptr.name).unwrap();
        if self.sig.is_reduction {
            // AUDIT 2026-05-24 (C31): dot-product / sum reduction shape.
            // The element-wise lowering substitutes `iload iv → tid` so
            // each CUDA thread carries one iteration's partial term in
            // `value`. A plain `st.global.<suffix>` would have every
            // thread race-overwrite the single `*ret_ptr` slot — silently
            // wrong sums. Emit `red.global.add.<atomic_suffix>` so each
            // thread's partial contribution accumulates correctly.
            //
            // `red`, not `atom` (found 2026-07-11 on real hardware): PTX's
            // `atom` REQUIRES a destination operand for the fetched old
            // value — the two-operand `atom.global.add [p], v;` form is a
            // ptxas error ("Arguments mismatch for instruction 'atom'"),
            // which made every reduction kernel fail module load with
            // CUDA_ERROR_INVALID_PTX and silently blacklist to CPU. The
            // fire-and-forget form that discards the old value is the
            // `red` (reduction) instruction, which is exactly what an
            // accumulate-only epilogue wants.
            //
            // PTX atomic-add type suffixes are NOT identical to the
            // load/store suffixes: integer atomics use unsigned widths
            // (`red.add.u32` / `red.add.u64`) — they operate on the raw
            // bit pattern, which matches Java two's-complement semantics
            // for signed accumulation. Float atomics use `.f32`
            // (sm_20+) / `.f64` (sm_60+).
            //
            // The host marshaller MUST pre-zero `*ret_ptr` before launch;
            // `KernelSignature::is_reduction` documents this contract.
            let atomic_suffix = match kind {
                RegKind::S32 => ".u32",
                RegKind::S64 => ".u64",
                RegKind::F32 => ".f32",
                RegKind::F64 => ".f64",
                _ => {
                    return Err(LoweringError::UnsupportedNode(format!(
                        "reduction atomic-add for register kind {kind:?} (suffix `{suffix}`) is not supported",
                    )));
                }
            };
            writeln!(
                self.body,
                "    red.global.add{} [{}], {};",
                atomic_suffix, ret_ptr.name, value.name
            )
            .unwrap();
        } else {
            // Non-reduction scalar return. For a single-thread / straight-line
            // shape, every thread writes the same value to `*ret_ptr` so
            // racing on the store is benign. Marshalling pre-allocates a
            // one-element output buffer.
            writeln!(
                self.body,
                "    st.global{} [{}], {};",
                suffix, ret_ptr.name, value.name
            )
            .unwrap();
        }
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
        let size = instr_size(bytes, pc)?;
        // Reject a truncated `Code` attribute before any raw operand
        // read below indexes past the end of `bytes`.
        if pc + size > bytes.len() {
            return Err(LoweringError::UnsupportedNode(format!(
                "instruction at pc={pc} runs past the end of the bytecode \
                 ({} bytes) — truncated Code attribute",
                bytes.len()
            )));
        }
        prev_ops.push(pc);
        pc += size;
    }
    if prev_ops.len() < 2 {
        return Err(LoweringError::UnsupportedNode(
            "loop header does not have two stack-producing instructions before exit-if".into(),
        ));
    }
    let bound_pc = prev_ops[prev_ops.len() - 1];
    let bound_op = bytes[bound_pc];

    // `i < arr.length` written inline: the bound is produced right here
    // by `aload arr; arraylength`, with no cached local in between. Same
    // destination as the cached-length form — the array parameter's
    // `pN_len` — just reached without the round trip through a local.
    if bound_op == 0xBE {
        let aload_pc = *prev_ops
            .get(prev_ops.len().wrapping_sub(2))
            .ok_or_else(|| {
                LoweringError::UnsupportedNode(
                    "inline `arraylength` loop bound has no preceding instruction".into(),
                )
            })?;
        let aload_op = bytes[aload_pc];
        let src_local = match aload_op {
            0x2A..=0x2D => (aload_op - 0x2A) as u16,
            0x19 => bytes[aload_pc + 1] as u16,
            0xC4 if bytes[aload_pc + 1] == 0x19 => {
                u16::from_be_bytes([bytes[aload_pc + 2], bytes[aload_pc + 3]])
            }
            _ => {
                return Err(LoweringError::UnsupportedNode(format!(
                    "inline `arraylength` loop bound at pc={bound_pc} is not taken \
                     from a plain `aload` (op 0x{aload_op:02x}) — the array must be \
                     a method parameter for its length to be a kernel parameter"
                )));
            }
        };
        return param_len_for_local(sig, src_local);
    }

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
        // `for (int i = 0; i < n; i++)` with `n` an `int` PARAMETER: the
        // bound is the parameter itself, never stored to. The guard it
        // needs is the same `tid >= bound` as every other shape. What it
        // is not is a free acceptance: the launch grid is otherwise sized
        // from the largest array argument, and a scalar bound can exceed
        // every one of them, so admitting this shape without telling the
        // host would under-provision threads and silently drop the tail of
        // the loop. The acceptance is therefore paired with
        // `WorkBound::ParamScalar`, which makes the dispatch site size the
        // grid from this parameter's runtime value; see that variant.
        //
        // Only an UNMODIFIED parameter qualifies. A slot the method stores
        // to is not the parameter's incoming value at the header, and
        // proving which store reaches the header is dataflow this
        // recognizer deliberately does not do.
        if let Some(idx) = int_param_index_for_local(sig, slot) {
            if !local_is_ever_stored(bytes, slot)? {
                return Ok(BoundSource::ParamScalar(idx));
            }
            return Err(LoweringError::UnsupportedNode(format!(
                "loop bound is `int` parameter {idx} (local {slot}), but the \
                 method stores to that local, so it is not provably the \
                 parameter's incoming value at the loop header"
            )));
        }
        return Err(LoweringError::UnsupportedNode(format!(
            "loop bound is local {slot}, but its definition is neither an \
             arraylength of a method parameter nor an unmodified `int` \
             parameter - bound is unknown"
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

/// Try to identify the source of a bound-local: scan the pre-loop
/// bytecode for `aload K; arraylength; istore slot` where K is a
/// parameter local. Returns the param's length name on success.
fn pre_loop_bound_source(
    bytes: &[u8],
    header_pc: usize,
    slot: u16,
    sig: &KernelSignature,
) -> Result<Option<BoundSource>, LoweringError> {
    let mut pc = 0usize;
    let mut last_aload_local: Option<u16> = None;
    let mut last_arraylength_local: Option<u16> = None;
    while pc < header_pc {
        let op = bytes[pc];
        let size = instr_size(bytes, pc)?;
        // Reject a truncated `Code` attribute before the raw operand
        // reads in the match arms below index past the end of `bytes`.
        if pc + size > bytes.len() {
            return Err(LoweringError::UnsupportedNode(format!(
                "instruction at pc={pc} runs past the end of the bytecode \
                 ({} bytes) — truncated Code attribute",
                bytes.len()
            )));
        }
        match op {
            0x2A..=0x2D => last_aload_local = Some((op - 0x2A) as u16),
            0x19 => last_aload_local = Some(bytes[pc + 1] as u16),
            0xC4 if bytes[pc + 1] == 0x19 => {
                last_aload_local = Some(u16::from_be_bytes([bytes[pc + 2], bytes[pc + 3]]));
            }
            0xBE => {
                last_arraylength_local = last_aload_local;
                last_aload_local = None;
            }
            // istore *
            0x36 => {
                let target_slot = bytes[pc + 1] as u16;
                if target_slot == slot && last_arraylength_local.is_some() {
                    let src_local = last_arraylength_local.unwrap();
                    return Ok(Some(param_len_for_local(sig, src_local)?));
                }
                last_arraylength_local = None;
            }
            0x3B..=0x3E => {
                let target_slot = (op - 0x3B) as u16;
                if target_slot == slot && last_arraylength_local.is_some() {
                    let src_local = last_arraylength_local.unwrap();
                    return Ok(Some(param_len_for_local(sig, src_local)?));
                }
                last_arraylength_local = None;
            }
            0xC4 if bytes[pc + 1] == 0x36 => {
                let target_slot = u16::from_be_bytes([bytes[pc + 2], bytes[pc + 3]]);
                if target_slot == slot && last_arraylength_local.is_some() {
                    let src_local = last_arraylength_local.unwrap();
                    return Ok(Some(param_len_for_local(sig, src_local)?));
                }
                last_arraylength_local = None;
            }
            _ => {}
        }
        pc += size;
    }
    Ok(None)
}

/// Parameter index of the `int` parameter occupying local `slot`, or
/// `None` when that slot is not a declared parameter or is not an `int`.
///
/// Only `ParamKind::I32` qualifies. A `long` bound would need a 64-bit
/// guard and a grid sized from a value a launch cannot represent; a
/// floating bound is not a trip count at all.
fn int_param_index_for_local(sig: &KernelSignature, slot: u16) -> Option<u32> {
    let mut s = 0u16;
    for (i, k) in sig.param_kinds.iter().enumerate() {
        if s == slot {
            return match k {
                ParamKind::I32 => u32::try_from(i).ok(),
                _ => None,
            };
        }
        s += match k {
            ParamKind::I64 | ParamKind::F64 => 2,
            ParamKind::Void => 0,
            _ => 1,
        };
    }
    None
}

/// Whether any instruction in the method writes local `slot`.
///
/// Deliberately whole-method and opcode-level rather than
/// reaching-definitions: the question is "is this local still the
/// incoming parameter value", and one store anywhere is enough to make
/// the answer "not provably". `iinc` counts as a store.
fn local_is_ever_stored(bytes: &[u8], slot: u16) -> Result<bool, LoweringError> {
    let mut pc = 0usize;
    while pc < bytes.len() {
        let op = bytes[pc];
        let size = instr_size(bytes, pc)?;
        if pc + size > bytes.len() {
            return Err(LoweringError::UnsupportedNode(format!(
                "instruction at pc={pc} runs past the end of the bytecode                  ({} bytes) - truncated Code attribute",
                bytes.len()
            )));
        }
        // `(base slot, how many slots it covers)`. A `lstore`/`dstore`
        // writes TWO slots, and a later method body is free to reuse a
        // dead parameter's slot as the upper half of one - so counting
        // only the named slot would report "never stored" for a slot
        // that is in fact overwritten.
        let stored: Option<(u16, u16)> = match op {
            // istore / fstore / astore
            0x36 | 0x38 | 0x3A => bytes.get(pc + 1).map(|b| (*b as u16, 1)),
            // lstore / dstore
            0x37 | 0x39 => bytes.get(pc + 1).map(|b| (*b as u16, 2)),
            // istore_<n>, fstore_<n>, astore_<n>
            0x3B..=0x3E | 0x43..=0x46 | 0x4B..=0x4E => Some((((op - 0x3B) % 4) as u16, 1)),
            // lstore_<n>, dstore_<n>
            0x3F..=0x42 | 0x47..=0x4A => Some((((op - 0x3B) % 4) as u16, 2)),
            // iinc
            0x84 => bytes.get(pc + 1).map(|b| (*b as u16, 1)),
            // wide istore/fstore/astore and wide iinc: one slot
            0xC4 if matches!(bytes.get(pc + 1), Some(0x36) | Some(0x38) | Some(0x3A) | Some(0x84)) =>
            {
                Some((u16::from_be_bytes([bytes[pc + 2], bytes[pc + 3]]), 1))
            }
            // wide lstore/dstore: two
            0xC4 if matches!(bytes.get(pc + 1), Some(0x37) | Some(0x39)) => {
                Some((u16::from_be_bytes([bytes[pc + 2], bytes[pc + 3]]), 2))
            }
            _ => None,
        };
        if let Some((base, width)) = stored {
            if slot >= base && slot - base < width {
                return Ok(true);
            }
        }
        pc += size;
    }
    Ok(false)
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
    /// A literal compile-time constant.
    Literal(i32),
    /// `pN` - an unmodified `int` parameter used directly as the bound.
    /// See [`crate::emitter::WorkBound::ParamScalar`] for the obligation
    /// this one puts on the host that the other two do not.
    ParamScalar(u32),
}
