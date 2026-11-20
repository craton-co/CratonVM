// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Java bytecode → PTX kernel lowering — entry point.
//!
//! The architecture is two-stage:
//!
//! 1. [`loop_recog`](self::loop_recog) recognizes the canonical
//!    counted-loop pattern. Anything else returns
//!    [`LoweringError::UnsupportedNode`] with a precise reason.
//! 2. [`emit`](self::emit) walks the bytecode, simulates the JVM
//!    operand stack with PTX virtual registers, and emits PTX text.
//!
//! The emitted kernel is a SIMT element-wise body:
//!
//!  * Compute `tid = ctaid.x * ntid.x + tid.x`.
//!  * If the loop has a non-zero (always non-negative — see
//!    [`loop_recog`](self::loop_recog)'s module doc comment) constant
//!    start value `K`, fold it in once: `tid = tid + K` (see
//!    [`emit::Emitter::apply_loop_start_offset`]).
//!  * Compare `tid >= bound`; if so, `ret`.
//!  * Emit the loop body once with `iload iv` rewritten to `tid`
//!    (already `tid + K` when the loop has a non-zero start) and
//!    `iinc iv` skipped.
//!  * Emit each `*aload`/`*astore` with a bounds check that jumps to
//!    a shared failure label on out-of-bounds, writing `1` to
//!    `*failure_flag` and `ret`-ing.

use crate::analyzer::ParamKind;
use crate::emitter::{
    LoweringError, PtxKernel, PtxModule, PtxParam, PtxParamKind, RegDecl, RegKind,
};
use crate::signature::KernelSignature;
use cratonvm_reader::constant_pool::ConstantPool;
use cratonvm_reader::method::ClassFileMethod;

mod emit;
mod loop_recog;

use emit::{locate_bound, BoundSource, Emitter};
use loop_recog::{detect_loop, LoopShape};

/// Lower a single eligible method into a one-kernel PTX module.
///
/// The kernel name is `<class>__<method>__<descriptor mangled>` so it
/// is unique across the program. The body honours the convention
/// established by [`build_param_list`]: array params are `(ptr, len)`
/// pairs, scalar params are single typed registers, and the trailing
/// `failure_flag` is a device-pointer used for deopt signalling.
///
/// No constant pool is available here, so a body containing
/// `ldc`/`ldc_w`/`ldc2_w` (AUDIT C31 follow-up, 2026-07-11) fails to
/// lower with [`LoweringError::UnsupportedNode`] — see
/// [`lower_method_with_pool`] for the variant that can actually emit
/// PTX for a numeric-literal `ldc`. In practice this is harmless: the
/// CP-free `analyzer::analyze`/`analyze_with_annotations` already
/// reject any such method before it ever reaches lowering (see
/// `Reason::LoadConstant`), so this entry point only ever sees `ldc`
/// here if a caller manually built a `KernelSignature` for a method it
/// didn't get from the analyzer.
pub fn lower_method(
    class_name: &str,
    method: &ClassFileMethod,
    sig: &KernelSignature,
    sm_major: u32,
    sm_minor: u32,
) -> Result<PtxModule, LoweringError> {
    lower_method_with_pool_impl(class_name, method, None, sig, sm_major, sm_minor, None, false)
}

/// [`lower_method`], but resolving `ldc`/`ldc_w`/`ldc2_w` against `cp`
/// (AUDIT C31 follow-up, 2026-07-11) instead of failing to lower them.
/// `cp` should be the same constant pool the method's class was parsed
/// with — i.e. the one passed to
/// `analyzer::analyze_with_pool`/`analyze_with_annotations_and_pool` to
/// admit the method in the first place.
pub fn lower_method_with_pool(
    class_name: &str,
    method: &ClassFileMethod,
    cp: &ConstantPool,
    sig: &KernelSignature,
    sm_major: u32,
    sm_minor: u32,
) -> Result<PtxModule, LoweringError> {
    lower_method_with_pool_impl(
        class_name,
        method,
        Some(cp),
        sig,
        sm_major,
        sm_minor,
        None,
        false,
    )
}

/// [`lower_method_with_pool`], additionally honoring `approx_math` when
/// lowering curated `Math.exp(double)` intrinsics.
pub fn lower_method_with_pool_and_approx(
    class_name: &str,
    method: &ClassFileMethod,
    cp: &ConstantPool,
    sig: &KernelSignature,
    sm_major: u32,
    sm_minor: u32,
    approx_math: bool,
) -> Result<PtxModule, LoweringError> {
    lower_method_with_pool_impl(
        class_name,
        method,
        Some(cp),
        sig,
        sm_major,
        sm_minor,
        None,
        approx_math,
    )
}

/// Shared implementation behind [`lower_method`] and
/// [`lower_method_with_pool`]. `cp` is `None` for the CP-free entry
/// point, which keeps its pre-AUDIT-C31 behaviour: an `ldc`/`ldc_w`/
/// `ldc2_w` in the body fails to lower rather than being resolved.
/// `if_convert_budget` overrides the process-wide flag-derived budget for
/// this one call. `None` means "use the flags", which is what production
/// passes; a test that wants to exercise the branch-to-`selp` transform
/// passes `Some(n)` rather than depending on a default that is `0`.
/// `pub(crate)` for `crate::golden`'s if-conversion characterisation,
/// which needs to drive the budget explicitly. The transform is OFF by
/// default and its budget is latched in a `OnceLock`, so a test that
/// set the environment variable could only ever observe one value per
/// process — and `speculation_cost`'s decisions are exactly what the
/// IR refactor must not change.
pub(crate) fn lower_method_with_pool_impl(
    class_name: &str,
    method: &ClassFileMethod,
    cp: Option<&ConstantPool>,
    sig: &KernelSignature,
    sm_major: u32,
    sm_minor: u32,
    if_convert_budget: Option<u32>,
    approx_math: bool,
) -> Result<PtxModule, LoweringError> {
    let kernel_name = mangle(class_name, &method.name, &method.descriptor);
    let params = build_param_list(sig);

    let code = method
        .code()
        .ok_or_else(|| LoweringError::UnsupportedNode("method has no Code attribute".into()))?;
    let bytes = &code.code;
    // `cp` also lets the loop recognizer resolve an `ldc`/`ldc_w`-
    // sourced loop start value (a start constant too large for
    // `iconst`/`bipush`/`sipush`) back to its `Integer` value — see
    // `loop_recog::resolve_start_value`. `None` (the CP-free entry
    // point) is always safe: such a start simply can't be proven
    // constant and the loop is rejected instead of mis-lowered.
    let shape = detect_loop(bytes, cp)?;

    let mut emitter = Emitter::new(bytes, sig, cp);
    emitter.approx_math = approx_math;
    if let Some(budget) = if_convert_budget {
        emitter.if_convert_budget = budget;
    }
    emitter.bind_param_locals()?;

    // The launch grid is sized from this; `Unknown` means "largest
    // array argument", the pre-existing behaviour. Only the single
    // counted loop can name its own trip count — the 2-D flattening's
    // is `R * C`, which is a product of two params rather than one
    // length, and a straight-line kernel has no loop at all.
    let mut work_bound = crate::emitter::WorkBound::Unknown;

    match shape {
        LoopShape::StraightLine => {
            // Single-thread kernel: every CUDA thread runs the body
            // identically. Marshalling pre-allocates a 1-element output
            // buffer; collision on `ret_ptr` is benign because every
            // thread writes the same value.
            emitter.emit_tid();
            // Optional: skip extra threads. We could check `tid != 0`
            // and ret, to avoid redundant work; harmless either way.
            // Walk the whole method.
            emitter.walk(0, bytes.len(), None)?;
        }
        LoopShape::Counted(li) => {
            // Pre-loop: emit straight-line.
            emitter.iv_slot = Some(li.iv_slot);
            emitter.emit_tid();
            // Fold the loop's constant start value `K` into the
            // induction register (`tid` becomes `tid + K`; a no-op
            // when `K == 0`) — see `Emitter::apply_loop_start_offset`'s
            // doc comment for the full correctness analysis (grid
            // over-launch and the output array's untouched `0..K`
            // prefix).
            emitter.apply_loop_start_offset(li.iv_start);
            let bound = locate_bound(bytes, &li, sig)?;
            emitter.walk(0, li.header_pc, Some(&li))?;
            // Drop the operand stack — javac emits `iload iv; iload bound`
            // just before the exit-if, but the body walker hasn't seen
            // them yet (we start at `body_start_pc`). The pre-loop walk
            // ran 0..header_pc so the stack should be empty.
            if emitter.stack_len() != 0 {
                return Err(LoweringError::Internal(format!(
                    "pre-loop walk left {} operand(s) on the stack",
                    emitter.stack_len()
                )));
            }
            // Emit guard. The same bound is what the host should size
            // the grid from: the guard makes thread `t` handle iteration
            // `t + K`, so `bound` threads always cover the loop (`K` of
            // them redundantly, and `K` is 0 for every loop but the
            // constant-start form).
            // What the bound IS decides both the guard's form and what
            // it proves — see `Emitter::emit_loop_guard`. An array length
            // and a non-negative literal are provably `>= 0`, so one
            // unsigned compare retires an out-of-range thread AND
            // establishes `tid >= 0` for the body. An `int` parameter is
            // not: `for (i = 0; i < n; i++)` with a negative `n` runs
            // zero times, and reading that bound as unsigned would run
            // the body instead.
            match bound {
                BoundSource::ParamLen(idx) => {
                    work_bound = crate::emitter::WorkBound::ParamLen(idx as u32);
                    let bound_reg = emitter.materialise_param_len(idx);
                    emitter.emit_loop_guard(&bound_reg, &li, Some(idx), true);
                }
                BoundSource::Literal(v) => {
                    work_bound = crate::emitter::WorkBound::Literal(v);
                    let bound_reg = emitter.materialise_literal_s32(v);
                    emitter.emit_loop_guard(&bound_reg, &li, None, v >= 0);
                }
                BoundSource::ParamScalar(idx) => {
                    work_bound = crate::emitter::WorkBound::ParamScalar(idx);
                    let bound_reg = emitter.materialise_param_scalar(idx as usize);
                    emitter.emit_loop_guard(&bound_reg, &li, None, false);
                }
            }
            // Body — lower its forward CFG once.  The canonical back-edge
            // is intentionally excluded: one CUDA thread owns one loop
            // iteration, so re-emitting it would duplicate work.
            emitter.walk_cfg(li.body_start_pc, li.back_branch_pc, &li)?;
            // Post-loop — anything from li.exit_pc on (returns).
            // Clear the hit_back_branch flag so the post-loop walk runs.
            emitter.clear_back_branch();
            emitter.walk(li.exit_pc, bytes.len(), None)?;
        }
        LoopShape::Nested(nl) => {
            // 2-D nested loop: one CUDA thread handles one `(i, j)` pair
            // from the flattened `R*C`-element iteration space. Both
            // bounds are resolved (but not yet emitted) before anything
            // else, exactly like the single-loop path's `locate_bound`.
            emitter.emit_tid();
            // A scalar-parameter bound is refused HERE and accepted on the
            // single-loop path above, and the asymmetry is the grid.
            // The 2-D shape's trip count is `R * C`, a product of two
            // bounds, and `WorkBound` names one parameter - so the host
            // would fall back to the largest-array rule and, for a
            // product that exceeds it, run fewer threads than there are
            // `(i, j)` pairs. Every pair past the end would simply never
            // execute: no bounds failure, no deopt, a silently partial
            // result. Refusing is the only safe answer until `WorkBound`
            // can carry a product.
            let mut nested_bound = |lp: &loop_recog::CountedLoop,
                                    e: &mut Emitter|
             -> Result<emit::Reg, LoweringError> {
                match locate_bound(bytes, lp, sig)? {
                    BoundSource::ParamLen(idx) => Ok(e.materialise_param_len(idx)),
                    BoundSource::Literal(v) => Ok(e.materialise_literal_s32(v)),
                    BoundSource::ParamScalar(idx) => Err(LoweringError::UnsupportedNode(format!(
                        "nested loop bound is `int` parameter {idx}; the 2-D launch \
                         grid is sized from the largest array argument and \
                         cannot be sized from a product of two scalars"
                    ))),
                }
            };
            let outer_bound = nested_bound(&nl.outer, &mut emitter)?;
            let inner_bound = nested_bound(&nl.inner, &mut emitter)?;
            // Pre-loop: any straight-line setup before the outer loop
            // header (e.g. a local caching `arr.length`), same as the
            // single-loop pre-loop walk. Neither loop's own guard nor
            // the inner induction-variable init is ever walked raw —
            // see `loop_recog::validate_rectangular_nesting`'s doc
            // comment for why skipping them is sound here.
            emitter.walk(0, nl.outer.header_pc, None)?;
            if emitter.stack_len() != 0 {
                return Err(LoweringError::Internal(format!(
                    "pre-loop walk left {} operand(s) on the stack",
                    emitter.stack_len()
                )));
            }
            emitter.emit_nested_loop_guard_and_decompose(&outer_bound, &inner_bound, &nl);
            // Body — the inner loop's forward CFG.  As for a 1-D loop,
            // the already-accounted-for canonical back-edge is excluded.
            emitter.walk_cfg(nl.inner.body_start_pc, nl.inner.back_branch_pc, &nl.inner)?;
            // Post-loop — anything from the outer loop's exit_pc on.
            emitter.clear_back_branch();
            emitter.walk(nl.outer.exit_pc, bytes.len(), None)?;
        }
    }

    emitter.finalize_epilogue();

    // Phase 10 #2 — snapshot the per-`*astore` param mask before
    // moving the body out of the emitter. The marshaller in
    // `vm::runtime::offload` reads this to suppress the post-launch
    // D→H copy for read-only array inputs (closing the residual
    // perf gap to TornadoVM left by the Phase 10 #1 residency cache).
    let writes_param_mask = emitter.writes_param_mask;
    let reads_param_mask = emitter.reads_param_mask;

    let reg_decls = emitter.emit_reg_decls();
    let body = emitter.into_body();
    check_every_branch_has_its_label(&body)?;

    let kernel = PtxKernel {
        name: kernel_name,
        params,
        code: body,
        reg_decls,
    };
    Ok(PtxModule {
        sm_major,
        sm_minor,
        kernels: vec![kernel],
        writes_param_mask,
        reads_param_mask,
        work_bound,
    })
}

/// Public alongside `lower_method`: the kernel parameter convention.
pub fn build_param_list(sig: &KernelSignature) -> Vec<PtxParam> {
    // AUDIT 2026-05-17 (Fix 8): hoist a single reusable `String`
    // buffer for parameter-name composition instead of issuing a
    // fresh `format!("p{i}_ptr")` / `_len` / scalar call per arm.
    // Each `format!` was a heap allocation + a `Display` round-trip;
    // the per-kernel param list is small but lowering runs once per
    // method per JIT pass, and the previous code allocated up to
    // 2 strings per array param.
    use std::fmt::Write;
    let mut out = Vec::with_capacity(sig.param_kinds.len() * 2 + 2);
    let mut buf = String::with_capacity(16);
    let mut make_name = |buf: &mut String, i: usize, suffix: &str| -> String {
        buf.clear();
        buf.push('p');
        // Use `write!` so the digit conversion writes straight into
        // the reused buffer without an intermediate allocation.
        let _ = write!(buf, "{i}");
        buf.push_str(suffix);
        // One `clone` here is unavoidable because `PtxParam` owns its
        // `String`. We still save one allocation per array param vs
        // the old `format!`-per-arm path because `make_name` reuses
        // the scratch buffer's capacity across iterations.
        buf.clone()
    };
    for (i, k) in sig.param_kinds.iter().enumerate() {
        match k {
            ParamKind::I32 => out.push(PtxParam {
                name: make_name(&mut buf, i, ""),
                kind: PtxParamKind::S32,
            }),
            ParamKind::I64 => out.push(PtxParam {
                name: make_name(&mut buf, i, ""),
                kind: PtxParamKind::S64,
            }),
            ParamKind::F32 => out.push(PtxParam {
                name: make_name(&mut buf, i, ""),
                kind: PtxParamKind::F32,
            }),
            ParamKind::F64 => out.push(PtxParam {
                name: make_name(&mut buf, i, ""),
                kind: PtxParamKind::F64,
            }),
            ParamKind::I32Array
            | ParamKind::I64Array
            | ParamKind::F32Array
            | ParamKind::F64Array
            | ParamKind::I16Array
            | ParamKind::I8Array
            | ParamKind::BoolArray => {
                out.push(PtxParam {
                    name: make_name(&mut buf, i, "_ptr"),
                    kind: PtxParamKind::U64Ptr,
                });
                out.push(PtxParam {
                    name: make_name(&mut buf, i, "_len"),
                    kind: PtxParamKind::S32,
                });
            }
            ParamKind::Void => {}
        }
    }
    match sig.return_kind {
        ParamKind::Void => {}
        ParamKind::I32 | ParamKind::I64 | ParamKind::F32 | ParamKind::F64 => {
            out.push(PtxParam {
                name: "ret_ptr".to_string(),
                kind: PtxParamKind::U64Ptr,
            });
        }
        ParamKind::I32Array
        | ParamKind::I64Array
        | ParamKind::F32Array
        | ParamKind::F64Array
        | ParamKind::I16Array
        | ParamKind::I8Array
        | ParamKind::BoolArray => {
            out.push(PtxParam {
                name: "ret_ptr".to_string(),
                kind: PtxParamKind::U64Ptr,
            });
            out.push(PtxParam {
                name: "ret_len".to_string(),
                kind: PtxParamKind::S32,
            });
        }
    }
    out.push(PtxParam {
        name: "failure_flag".to_string(),
        kind: PtxParamKind::U64Ptr,
    });
    // The index of the first element this launch is responsible for.
    //
    // A whole-array launch passes 0 and nothing changes. A CHUNKED launch
    // passes the chunk's base, so one kernel can be launched several times
    // over disjoint slices of the same iteration space while each thread
    // still computes its GLOBAL index. That is what lets the writeback of
    // one chunk overlap with the kernel of the next; without it every
    // launch would start its index space at zero, because CUDA has no
    // launch offset of its own.
    //
    // Costs one `ld.param` and one `add.s32` in the prologue, per thread,
    // whether or not chunking is in use.
    out.push(PtxParam {
        name: "tid_base".to_string(),
        kind: PtxParamKind::S32,
    });
    out
}

/// Refuse a body containing a `bra` to a label it never emits.
///
/// A dangling label is not a compile error here and not a runtime error
/// either: `ptxas` rejects the module, the VM records the method as
/// unloadable, and the kernel runs on the CPU forever after. The Java
/// answer stays correct, so every correctness check keeps passing and the
/// only symptom is a workload that quietly stopped using the GPU. That is
/// the most expensive kind of bug this file can produce, and it is cheap
/// to make impossible: the emitter has exactly one label namespace
/// (`L_body_<pc>` plus a fixed handful), so a text scan settles it.
///
/// Written as a whole-body invariant rather than a check inside whichever
/// transform is under suspicion, because the point is to catch the NEXT
/// one. The 2026-08-29 if-conversion could consume a block that a
/// short-circuit `&&`'s first branch still jumped to; this is what makes
/// that class of mistake loud.
///
/// It reads the instruction list now rather than scanning body text.
/// A `Label`/`Bra` pair answers by variant; a `Raw` still answers by
/// substring, because a `Raw` IS text -- but the text is one
/// instruction, so `bra` cannot be found inside a comment belonging to
/// a different line.
fn check_every_branch_has_its_label(body: &crate::kir::Body) -> Result<(), LoweringError> {
    use crate::kir::Instr;
    fn label_of(i: &Instr) -> Option<&str> {
        match i {
            Instr::Label(name) => Some(name.as_str()),
            Instr::Raw(t) => {
                let t = t.trim();
                t.strip_suffix(':')
                    .filter(|n| !n.is_empty() && !n.contains(char::is_whitespace))
            }
            _ => None,
        }
    }
    fn branch_target(i: &Instr) -> Option<&str> {
        match i {
            Instr::Bra { target, .. } => Some(target.as_str()),
            Instr::Raw(t) => {
                let t = t.trim();
                let idx = t.find("bra ")?;
                Some(t[idx + 4..].trim().trim_end_matches(';').trim())
            }
            _ => None,
        }
    }
    let labels: std::collections::HashSet<&str> = body.code().iter().filter_map(label_of).collect();
    for instr in body.code() {
        let Some(target) = branch_target(instr) else {
            continue;
        };
        if target.is_empty() || target.starts_with('%') {
            // An indirect branch. Nothing here emits one; if something
            // ever does, it is not this check's business.
            continue;
        }
        if !labels.contains(target) {
            return Err(LoweringError::Internal(format!(
                "emitted `bra {target}` but no `{target}:` label - a block was \
                 consumed by a transform while something still branched to it"
            )));
        }
    }
    Ok(())
}

fn mangle(class_name: &str, method_name: &str, descriptor: &str) -> String {
    let mut out = String::with_capacity(class_name.len() + method_name.len() + descriptor.len());
    for ch in class_name
        .chars()
        .chain("__".chars())
        .chain(method_name.chars())
        .chain("__".chars())
        .chain(descriptor.chars())
    {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

impl<'a> Emitter<'a> {
    pub(crate) fn stack_len(&self) -> usize {
        self.stack.len()
    }

    pub(crate) fn clear_back_branch(&mut self) {
        self.hit_back_branch = false;
    }

    /// The register holding `pN_len`.
    ///
    /// AUDIT 2026-09-02: this used to issue its own `ld.param.s32`,
    /// which was the second load of the same kernel parameter —
    /// `bind_param_locals` already hoisted one for the bounds checks.
    /// Reusing it also makes the loop bound and the bounds-check length
    /// literally the same register, which is what lets
    /// `Emitter::prove_index_within_param` recognise
    /// `for (i = 0; i < a.length; i++) a[i]` as needing no check at all.
    pub(crate) fn materialise_param_len(&mut self, idx: usize) -> emit::Reg {
        if let Some(r) = self.param_len_reg.get(idx).and_then(|r| r.clone()) {
            return r;
        }
        let r = self.regs.fresh_reg(RegKind::S32);
        use std::fmt::Write;
        writeln!(self.body, "    ld.param.s32 {}, [p{idx}_len];", r.name).unwrap();
        if let Some(slot) = self.param_len_reg.get_mut(idx) {
            *slot = Some(r.clone());
        }
        r
    }

    /// Materialise the scalar `int` parameter `pN` into a fresh s32
    /// register, for use as a loop bound.
    ///
    /// `build_param_list` names a scalar parameter `p{idx}` with no
    /// suffix; the `_len` suffix belongs to the array form.
    pub(crate) fn materialise_param_scalar(&mut self, idx: usize) -> emit::Reg {
        let r = self.regs.fresh_reg(RegKind::S32);
        use std::fmt::Write;
        writeln!(self.body, "    ld.param.s32 {}, [p{idx}];", r.name).unwrap();
        r
    }

    /// Materialise an integer literal into a fresh s32 register.
    pub(crate) fn materialise_literal_s32(&mut self, v: i32) -> emit::Reg {
        let r = self.regs.fresh_reg(RegKind::S32);
        use std::fmt::Write;
        writeln!(self.body, "    mov.s32 {}, {};", r.name, v).unwrap();
        r
    }

    /// The finished kernel body, with any per-array bounds precondition
    /// moved back to a dominating position.
    ///
    /// The preconditions [`Emitter::prove_index_within_param`] emits are
    /// discovered while walking the body — that is when it becomes known
    /// which arrays are indexed by the induction variable — but they have
    /// to EXECUTE before it. `prologue_splice_at` is the byte offset the
    /// dispatch guard recorded, so they land immediately after it and
    /// immediately before the first thing that depends on them.
    ///
    /// `prologue_splice_at` is an INSTRUCTION INDEX. It used to be a
    /// byte offset into the body `String`, and the doc here used to
    /// explain that splicing text was what this emitter did everywhere
    /// "because there is no IR to insert into". There is now — see
    /// `crate::kir` — so the splice inserts instructions at an index and
    /// does no arithmetic on string lengths.
    pub(crate) fn into_body(mut self) -> crate::kir::Body {
        if self.bounds_prologue.is_empty() {
            return self.body;
        }
        let at = self.prologue_splice_at.expect(
            "a bounds precondition was emitted without a dispatch guard to \n             splice it after; only a guarded shape can prove an index, so \n             this is unreachable unless `prove_index_within_param` grew a \n             new caller",
        );
        let prologue = std::mem::take(&mut self.bounds_prologue);
        self.body.splice_at(at, prologue);
        self.body
    }

    pub(crate) fn emit_reg_decls(&self) -> Vec<RegDecl> {
        let mut out = Vec::new();
        if self.regs.u32_count > 0 {
            out.push(RegDecl {
                kind: RegKind::U32,
                count: self.regs.u32_count,
            });
        }
        if self.regs.u64_count > 0 {
            out.push(RegDecl {
                kind: RegKind::U64,
                count: self.regs.u64_count,
            });
        }
        if self.regs.s32_count > 0 {
            out.push(RegDecl {
                kind: RegKind::S32,
                count: self.regs.s32_count,
            });
        }
        if self.regs.s64_count > 0 {
            out.push(RegDecl {
                kind: RegKind::S64,
                count: self.regs.s64_count,
            });
        }
        if self.regs.f32_count > 0 {
            out.push(RegDecl {
                kind: RegKind::F32,
                count: self.regs.f32_count,
            });
        }
        if self.regs.f64_count > 0 {
            out.push(RegDecl {
                kind: RegKind::F64,
                count: self.regs.f64_count,
            });
        }
        if self.regs.pred_count > 0 {
            out.push(RegDecl {
                kind: RegKind::Pred,
                count: self.regs.pred_count,
            });
        }
        if self.regs.b16_count > 0 {
            out.push(RegDecl {
                kind: RegKind::B16,
                count: self.regs.b16_count,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests;
