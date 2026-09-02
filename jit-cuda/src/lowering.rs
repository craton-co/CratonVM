// SPDX-License-Identifier: Apache-2.0
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
    lower_method_with_pool_impl(class_name, method, None, sig, sm_major, sm_minor, None)
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
    lower_method_with_pool_impl(class_name, method, Some(cp), sig, sm_major, sm_minor, None)
}

/// Shared implementation behind [`lower_method`] and
/// [`lower_method_with_pool`]. `cp` is `None` for the CP-free entry
/// point, which keeps its pre-AUDIT-C31 behaviour: an `ldc`/`ldc_w`/
/// `ldc2_w` in the body fails to lower rather than being resolved.
/// `if_convert_budget` overrides the process-wide flag-derived budget for
/// this one call. `None` means "use the flags", which is what production
/// passes; a test that wants to exercise the branch-to-`selp` transform
/// passes `Some(n)` rather than depending on a default that is `0`.
fn lower_method_with_pool_impl(
    class_name: &str,
    method: &ClassFileMethod,
    cp: Option<&ConstantPool>,
    sig: &KernelSignature,
    sm_major: u32,
    sm_minor: u32,
    if_convert_budget: Option<u32>,
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
        body,
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
            | ParamKind::I8Array => {
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
        | ParamKind::I8Array => {
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
fn check_every_branch_has_its_label(body: &str) -> Result<(), LoweringError> {
    let mut labels = std::collections::HashSet::new();
    for line in body.lines() {
        let t = line.trim();
        if let Some(name) = t.strip_suffix(':') {
            if !name.is_empty() && !name.contains(char::is_whitespace) {
                labels.insert(name);
            }
        }
    }
    for line in body.lines() {
        let t = line.trim();
        let Some(idx) = t.find("bra ") else { continue };
        let target = t[idx + 4..].trim().trim_end_matches(';').trim();
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
    /// Splicing text rather than building an instruction list is what
    /// this emitter does everywhere (see `try_emit_if_converted`, which
    /// speculates by swapping the body `String` out and back). It is the
    /// same trade, and the same reason: there is no IR to insert into.
    pub(crate) fn into_body(mut self) -> String {
        if self.bounds_prologue.is_empty() {
            return self.body;
        }
        let at = self.prologue_splice_at.expect(
            "a bounds precondition was emitted without a dispatch guard to \n             splice it after; only a guarded shape can prove an index, so \n             this is unreachable unless `prove_index_within_param` grew a \n             new caller",
        );
        let prologue = std::mem::take(&mut self.bounds_prologue);
        self.body.insert_str(at, &prologue);
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
mod tests {
    use super::*;

    #[test]
    fn mangle_is_ptx_safe() {
        let m = mangle("com/example/Foo", "vectorAdd", "([I[I)[I");
        assert!(m.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(m.contains("vectorAdd"));
        assert!(m.contains("Foo"));
    }

    #[test]
    fn param_list_for_vector_add() {
        let sig = KernelSignature {
            param_kinds: vec![ParamKind::I32Array, ParamKind::I32Array],
            return_kind: ParamKind::I32Array,
            estimated_work: 1 << 20,
            needs_d2h_sync: false,
            this_field_cps: vec![],
            writes_param_mask: 0,
            reads_param_mask: 0,
            work_bound: crate::emitter::WorkBound::Unknown,
            is_reduction: false,
            allow_div_by_zero: false,
        };
        let params = build_param_list(&sig);
        // (a_ptr, a_len, b_ptr, b_len, ret_ptr, ret_len, failure_flag,
        //  tid_base) = 8
        assert_eq!(params.len(), 8);
        assert_eq!(params[0].name, "p0_ptr");
        assert_eq!(params[1].name, "p0_len");
        assert_eq!(params[2].name, "p1_ptr");
        assert_eq!(params[3].name, "p1_len");
        assert_eq!(params[4].name, "ret_ptr");
        assert_eq!(params[5].name, "ret_len");
        assert_eq!(params[6].name, "failure_flag");
        assert_eq!(params[7].name, "tid_base");
    }

    #[test]
    fn param_list_scalar_return() {
        let sig = KernelSignature {
            param_kinds: vec![ParamKind::I32Array, ParamKind::I32Array],
            return_kind: ParamKind::I64,
            estimated_work: 1 << 20,
            needs_d2h_sync: false,
            this_field_cps: vec![],
            writes_param_mask: 0,
            reads_param_mask: 0,
            work_bound: crate::emitter::WorkBound::Unknown,
            is_reduction: false,
            allow_div_by_zero: false,
        };
        let params = build_param_list(&sig);
        // (a_ptr, a_len, b_ptr, b_len, ret_ptr, failure_flag, tid_base) = 7
        assert_eq!(params.len(), 7);
        assert_eq!(params[4].name, "ret_ptr");
        assert_eq!(params[5].name, "failure_flag");
        assert_eq!(params[6].name, "tid_base");
    }

    fn lower_i32_remainder_body(allow_div_by_zero: bool) -> String {
        let bytes = [0x1A, 0x1B, 0x70, 0xAC]; // iload_0; iload_1; irem; ireturn
        let sig = KernelSignature {
            param_kinds: vec![ParamKind::I32, ParamKind::I32],
            return_kind: ParamKind::I32,
            estimated_work: 1,
            needs_d2h_sync: false,
            this_field_cps: vec![],
            writes_param_mask: 0,
            reads_param_mask: 0,
            work_bound: crate::emitter::WorkBound::Unknown,
            is_reduction: false,
            allow_div_by_zero,
        };
        let mut emitter = Emitter::new(&bytes, &sig, None);
        emitter.bind_param_locals().expect("bind params");
        emitter.walk(0, bytes.len(), None).expect("lower irem body");
        emitter.finalize_epilogue();
        emitter.into_body()
    }

    #[test]
    fn strict_integer_remainder_emits_zero_guard() {
        let text = lower_i32_remainder_body(false);
        assert!(text.contains("setp.eq.s32"));
        assert!(text.contains("bra L_bounds_fail"));
        assert!(text.contains("L_bounds_fail:"));
    }

    #[test]
    fn allow_div_by_zero_skips_integer_remainder_zero_guard() {
        let text = lower_i32_remainder_body(true);
        assert!(text.contains("rem.s32"));
        assert!(
            !text.contains("setp.eq.s32"),
            "AllowDivByZero must skip the zero-divisor predicate\n{text}"
        );
        assert!(
            !text.contains("L_bounds_fail:"),
            "irem with AllowDivByZero has no deopt guard to emit\n{text}"
        );
    }

    // ─── AUDIT 2026-07-11: frem/drem lowering (white-box) ────────────
    //
    // Mirrors `lower_i32_remainder_body` above: a hand-built 4-byte
    // straight-line body (`*load_0; *load_1; *rem; *return`) run
    // straight through `Emitter` without going through the analyzer at
    // all, so these tests pin the exact PTX sequence
    // `lowering::emit::Emitter::frem_f32`/`drem_f64` emit, independent
    // of admission policy.

    fn lower_f32_remainder_body(allow_div_by_zero: bool) -> String {
        let bytes = [0x22, 0x23, 0x72, 0xAE]; // fload_0; fload_1; frem; freturn
        let sig = KernelSignature {
            param_kinds: vec![ParamKind::F32, ParamKind::F32],
            return_kind: ParamKind::F32,
            estimated_work: 1,
            needs_d2h_sync: false,
            this_field_cps: vec![],
            writes_param_mask: 0,
            reads_param_mask: 0,
            work_bound: crate::emitter::WorkBound::Unknown,
            is_reduction: false,
            allow_div_by_zero,
        };
        let mut emitter = Emitter::new(&bytes, &sig, None);
        emitter.bind_param_locals().expect("bind params");
        emitter.walk(0, bytes.len(), None).expect("lower frem body");
        emitter.finalize_epilogue();
        emitter.into_body()
    }

    fn lower_f64_remainder_body(allow_div_by_zero: bool) -> String {
        // `double` is a category-2 (2-slot) JVM local: param 0 occupies
        // slots 0-1, so param 1 starts at slot 2 — `dload_2`, not
        // `dload_1` (which `bind_param_locals` never binds for this
        // signature). Mirrors real javac output for
        // `dremScalar(double a, double b)`.
        let bytes = [0x26, 0x28, 0x73, 0xAF]; // dload_0; dload_2; drem; dreturn
        let sig = KernelSignature {
            param_kinds: vec![ParamKind::F64, ParamKind::F64],
            return_kind: ParamKind::F64,
            estimated_work: 1,
            needs_d2h_sync: false,
            this_field_cps: vec![],
            writes_param_mask: 0,
            reads_param_mask: 0,
            work_bound: crate::emitter::WorkBound::Unknown,
            is_reduction: false,
            allow_div_by_zero,
        };
        let mut emitter = Emitter::new(&bytes, &sig, None);
        emitter.bind_param_locals().expect("bind params");
        emitter.walk(0, bytes.len(), None).expect("lower drem body");
        emitter.finalize_epilogue();
        emitter.into_body()
    }

    #[test]
    fn frem_lowers_to_div_trunc_fma_sequence() {
        let text = lower_f32_remainder_body(true);
        // The div + truncate + fma identity from `frem_f32`'s doc
        // comment: `r = a - trunc(a/b)*b`, one rounding total.
        assert!(text.contains("div.rn.f32"), "missing quotient div\n{text}");
        assert!(
            text.contains("cvt.rzi.f32.f32"),
            "missing truncate-toward-zero\n{text}"
        );
        assert!(
            text.contains("neg.f32"),
            "missing quotient negation\n{text}"
        );
        assert!(
            text.contains("fma.rn.f32"),
            "missing single-rounding fma\n{text}"
        );
        // The infinite-divisor correctness patch (JLS §15.17.3: `a %
        // ±Infinity == a` for finite `a`).
        assert!(text.contains("abs.f32"), "missing |divisor|\n{text}");
        assert!(
            text.contains("setp.eq.f32"),
            "missing is-infinite predicate\n{text}"
        );
        assert!(
            text.contains("selp.f32"),
            "missing dividend/naive-remainder select\n{text}"
        );
        // PTX genuinely has no `rem.f32` mnemonic — pin that against a
        // regression back to the old (ptxas-rejected) naive lowering
        // this replaced.
        assert!(
            !text.contains("rem.f32"),
            "PTX has no rem.f32 mnemonic; this must never be emitted\n{text}"
        );
    }

    #[test]
    fn drem_lowers_to_div_trunc_fma_sequence() {
        let text = lower_f64_remainder_body(true);
        assert!(text.contains("div.rn.f64"), "missing quotient div\n{text}");
        assert!(
            text.contains("cvt.rzi.f64.f64"),
            "missing truncate-toward-zero\n{text}"
        );
        assert!(
            text.contains("neg.f64"),
            "missing quotient negation\n{text}"
        );
        assert!(
            text.contains("fma.rn.f64"),
            "missing single-rounding fma\n{text}"
        );
        assert!(text.contains("abs.f64"), "missing |divisor|\n{text}");
        assert!(
            text.contains("setp.eq.f64"),
            "missing is-infinite predicate\n{text}"
        );
        assert!(
            text.contains("selp.f64"),
            "missing dividend/naive-remainder select\n{text}"
        );
        assert!(
            !text.contains("rem.f64"),
            "PTX has no rem.f64 mnemonic; this must never be emitted\n{text}"
        );
    }

    #[test]
    fn frem_never_emits_a_zero_divisor_deopt_guard() {
        // Java floats never trap on a zero divisor — IEEE 754 division
        // of a float by zero produces ±Infinity or NaN, never an
        // exception — so unlike `div_or_rem_i32`/`div_or_rem_i64`,
        // `frem_f32`/`drem_f64` must not branch to the shared
        // `L_bounds_fail` deopt label at all (there is nothing to
        // guard against and no failure-flag write is ever needed for
        // this opcode).
        let f32_text = lower_f32_remainder_body(true);
        assert!(
            !f32_text.contains("L_bounds_fail"),
            "frem must not emit an integer-style zero-divisor guard\n{f32_text}"
        );
        let f64_text = lower_f64_remainder_body(true);
        assert!(
            !f64_text.contains("L_bounds_fail"),
            "drem must not emit an integer-style zero-divisor guard\n{f64_text}"
        );
    }

    #[test]
    fn frem_lowering_is_unaffected_by_allow_div_by_zero_flag() {
        // Unlike `irem`/`lrem` (see
        // `allow_div_by_zero_skips_integer_remainder_zero_guard`),
        // `frem` never emits a zero-divisor guard in the first place.
        // `KernelSignature::allow_div_by_zero` is purely an
        // ANALYZER-side admission gate for frem/drem (see
        // `analyzer::classify`'s `0x72 | 0x73` arm) — once a method
        // reaches this emitter at all, its frem/drem lowering is
        // identical regardless of the flag's value.
        assert_eq!(
            lower_f32_remainder_body(true),
            lower_f32_remainder_body(false)
        );
        assert_eq!(
            lower_f64_remainder_body(true),
            lower_f64_remainder_body(false)
        );
    }

    // ─── AUDIT 2026-07-11: lcmp/fcmp*/dcmp* lowering (white-box) ─────
    //
    // Mirrors `lower_i32_remainder_body`/`lower_f32_remainder_body`
    // above: a hand-built straight-line body run directly through
    // `Emitter`, bypassing the analyzer and the `.class` fixture
    // pipeline entirely. This is the accepted pattern for pinning an
    // exact PTX sequence in isolation (see the file-level comment on
    // those helpers and `test_support.rs`'s "no synthetic bytecode"
    // rule, which is scoped to the `.class`-fixture loader, not to
    // hand-built opcode arrays driving `Emitter` directly the way
    // `analyzer.rs`'s `track_reduction_ops` tests already do).
    //
    // These shapes ARE realistic per-instruction JVM bytecode (each
    // sequence is exactly what `lload_0; lload_2; lcmp; ireturn` etc.
    // means per JVMS), but — per the reality-check analysis on
    // `analyzer::Reason::Compare` and the `0x94` arm in `emit_op` —
    // javac never emits this exact CONSUMPTION shape (a bare `*cmp*`
    // immediately followed by `*return`, with no intervening `if*`):
    // every real relational/equality use of `long`/`float`/`double`
    // compiles the pushed value straight into a branch. So there is no
    // `.class` fixture that could exercise `lcmp`/`cmp_f32`/`cmp_f64`
    // through the real reader/analyzer/emitter path today; these tests
    // pin the building block directly. `compare_branch_fusion_*` further
    // down (in the real-class section) pins the actual end-to-end
    // behaviour change for a realistic method: analyzer-`Eligible`,
    // lowering-`Rejected` at the following `if*`.

    fn lower_cmp_body(bytes: [u8; 4], param_kind: ParamKind) -> String {
        let sig = KernelSignature {
            param_kinds: vec![param_kind, param_kind],
            return_kind: ParamKind::I32,
            estimated_work: 1,
            needs_d2h_sync: false,
            this_field_cps: vec![],
            writes_param_mask: 0,
            reads_param_mask: 0,
            work_bound: crate::emitter::WorkBound::Unknown,
            is_reduction: false,
            allow_div_by_zero: false,
        };
        let mut emitter = Emitter::new(&bytes, &sig, None);
        emitter.bind_param_locals().expect("bind params");
        emitter.walk(0, bytes.len(), None).expect("lower cmp body");
        emitter.finalize_epilogue();
        emitter.into_body()
    }

    #[test]
    fn lcmp_lowers_to_setp_selp_chain() {
        // lload_0; lload_2; lcmp; ireturn (long params: slot 0-1, 2-3).
        let text = lower_cmp_body([0x1E, 0x20, 0x94, 0xAC], ParamKind::I64);
        // Registers are fully deterministic here: `bind_param_locals`
        // for two I64 params only touches the S64 pool, so `lcmp`'s
        // Pred/S32 registers start fresh at %p0/%r0.
        assert!(
            text.contains("setp.gt.s64 %p0, %rl0, %rl1;"),
            "missing gt predicate\n{text}"
        );
        assert!(
            text.contains("setp.lt.s64 %p1, %rl0, %rl1;"),
            "missing lt predicate\n{text}"
        );
        assert!(
            text.contains("selp.s32 %r0, -1, 0, %p1;"),
            "missing lt-or-eq select\n{text}"
        );
        assert!(
            text.contains("selp.s32 %r1, 1, %r0, %p0;"),
            "missing final gt-overrides-the-rest select\n{text}"
        );
        // Integers have no unordered case — no NaN predicate, exactly
        // two selects (unlike the three the float/double variants need).
        assert!(
            !text.contains("setp.nan"),
            "lcmp must not emit a NaN check — longs have a total order\n{text}"
        );
        assert_eq!(
            text.matches("selp.s32").count(),
            2,
            "lcmp must emit exactly two selects\n{text}"
        );
    }

    #[test]
    fn fcmpl_lowers_with_nan_minus_one_override() {
        // fload_0; fload_1; fcmpl; ireturn (float params: slot 0, 1).
        let text = lower_cmp_body([0x22, 0x23, 0x95, 0xAC], ParamKind::F32);
        assert!(
            text.contains("setp.gt.f32 %p0, %f0, %f1;"),
            "missing gt predicate\n{text}"
        );
        assert!(
            text.contains("setp.lt.f32 %p1, %f0, %f1;"),
            "missing lt predicate\n{text}"
        );
        assert!(
            text.contains("setp.nan.f32 %p2, %f0, %f1;"),
            "missing NaN predicate\n{text}"
        );
        assert!(
            text.contains("selp.s32 %r0, -1, 0, %p1;"),
            "missing lt-or-eq select\n{text}"
        );
        assert!(
            text.contains("selp.s32 %r1, 1, %r0, %p0;"),
            "missing ordered-result select\n{text}"
        );
        // The NaN override: fcmpl's default is -1.
        assert!(
            text.contains("selp.s32 %r2, -1, %r1, %p2;"),
            "expected the fcmpl NaN-override select (default -1)\n{text}"
        );
        assert_eq!(
            text.matches("selp.s32").count(),
            3,
            "fcmpl must emit exactly three selects (ordered chain + NaN override)\n{text}"
        );
    }

    #[test]
    fn fcmpg_lowers_with_nan_plus_one_override() {
        // fload_0; fload_1; fcmpg; ireturn.
        let text = lower_cmp_body([0x22, 0x23, 0x96, 0xAC], ParamKind::F32);
        assert!(text.contains("setp.gt.f32 %p0, %f0, %f1;"));
        assert!(text.contains("setp.lt.f32 %p1, %f0, %f1;"));
        assert!(text.contains("setp.nan.f32 %p2, %f0, %f1;"));
        assert!(text.contains("selp.s32 %r0, -1, 0, %p1;"));
        assert!(text.contains("selp.s32 %r1, 1, %r0, %p0;"));
        // The NaN override: fcmpg's default is +1 — the only difference
        // from `fcmpl`'s PTX sequence.
        assert!(
            text.contains("selp.s32 %r2, 1, %r1, %p2;"),
            "expected the fcmpg NaN-override select (default +1)\n{text}"
        );
        // Must NOT contain fcmpl's -1 override line.
        assert!(
            !text.contains("selp.s32 %r2, -1, %r1, %p2;"),
            "fcmpg must not reuse fcmpl's -1 NaN default\n{text}"
        );
    }

    #[test]
    fn dcmpl_and_dcmpg_mirror_the_f32_sequence_in_f64() {
        // dload_0; dload_2; dcmp*; ireturn (double params: slot 0-1, 2-3).
        let l_text = lower_cmp_body([0x26, 0x28, 0x97, 0xAC], ParamKind::F64);
        assert!(l_text.contains("setp.gt.f64 %p0, %fd0, %fd1;"));
        assert!(l_text.contains("setp.lt.f64 %p1, %fd0, %fd1;"));
        assert!(l_text.contains("setp.nan.f64 %p2, %fd0, %fd1;"));
        assert!(l_text.contains("selp.s32 %r2, -1, %r1, %p2;"));

        let g_text = lower_cmp_body([0x26, 0x28, 0x98, 0xAC], ParamKind::F64);
        assert!(g_text.contains("setp.gt.f64 %p0, %fd0, %fd1;"));
        assert!(g_text.contains("selp.s32 %r2, 1, %r1, %p2;"));
        // PTX genuinely has no three-way integer/float compare mnemonic
        // — pin that this never regresses into emitting a bogus `cmp.*`
        // instruction ptxas would reject.
        assert!(!l_text.contains("cmp.f64"));
        assert!(!g_text.contains("cmp.f64"));
    }

    // ─────────────── real-class lowering tests ──────────────────────
    use crate::analyzer::{analyze, OffloadVerdict};
    use crate::test_support::load_method;

    fn lower_fixture(class: &str, method_name: &str, descriptor: &str) -> PtxModule {
        let method = load_method(class, method_name, descriptor);
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("fixture {class}.{method_name} not eligible: {v:?}"),
        };
        lower_method(class, &method, &sig, 7, 5)
            .unwrap_or_else(|e| panic!("lowering failed for {class}.{method_name}: {e}"))
    }

    /// [`lower_fixture`], but through the pool-aware
    /// `analyze_with_pool` / `lower_method_with_pool` entry points
    /// (AUDIT C31 follow-up, 2026-07-11) — needed for any fixture whose
    /// body contains `ldc`/`ldc_w`/`ldc2_w`, which the CP-free path
    /// still refuses. Uses `crate::analyzer::load_method_with_pool`
    /// (declared `pub(crate)` specifically so this module doesn't need
    /// its own copy of the fixture-loading boilerplate).
    fn lower_fixture_with_pool(class: &str, method_name: &str, descriptor: &str) -> PtxModule {
        let (method, cp) = crate::analyzer::load_method_with_pool(class, method_name, descriptor);
        let sig = match crate::analyzer::analyze_with_pool(&method, &cp) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("fixture {class}.{method_name} not eligible: {v:?}"),
        };
        lower_method_with_pool(class, &method, &cp, &sig, 7, 5)
            .unwrap_or_else(|e| panic!("lowering failed for {class}.{method_name}: {e}"))
    }

    /// [`lower_fixture`], but with the branch-to-`selp` if-conversion
    /// forced on at `budget`.
    ///
    /// The transform is OFF by default -- it is a measured loss on the one
    /// kernel it was built for (see
    /// `gpu/raytracer-vs-tornadovm-RESOLVED-20260821.md`'s residual pass) --
    /// so a test that asserts on it has to ask for it. Asking here rather
    /// than setting an environment variable also keeps the tests
    /// order-independent: the flag is latched in a `OnceLock`, so a process
    /// that reads it once cannot be told twice.
    fn lower_fixture_if_converted(
        class: &str,
        method_name: &str,
        descriptor: &str,
        budget: u32,
    ) -> PtxModule {
        let method = load_method(class, method_name, descriptor);
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("fixture {class}.{method_name} not eligible: {v:?}"),
        };
        lower_method_with_pool_impl(class, &method, None, &sig, 7, 5, Some(budget))
            .unwrap_or_else(|e| panic!("lowering failed for {class}.{method_name}: {e}"))
    }

    /// [`lower_fixture_if_converted`] through the pool-aware entry point.
    fn lower_fixture_with_pool_if_converted(
        class: &str,
        method_name: &str,
        descriptor: &str,
        budget: u32,
    ) -> PtxModule {
        let (method, cp) = crate::analyzer::load_method_with_pool(class, method_name, descriptor);
        let sig = match crate::analyzer::analyze_with_pool(&method, &cp) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("fixture {class}.{method_name} not eligible: {v:?}"),
        };
        lower_method_with_pool_impl(class, &method, Some(&cp), &sig, 7, 5, Some(budget))
            .unwrap_or_else(|e| panic!("lowering failed for {class}.{method_name}: {e}"))
    }

    /// [`lower_fixture_with_pool`], but additionally threading an
    /// [`crate::annotations::AdmissionHint`] through
    /// `analyzer::analyze_with_annotations_and_pool` — needed for
    /// `EligibleFrem`, whose `frem` opcode the analyzer only admits
    /// under `AdmissionHint::AllowDivByZero` (AUDIT 2026-07-11, see
    /// `analyzer::classify`'s `0x72 | 0x73` arm).
    fn lower_fixture_with_pool_and_hint(
        class: &str,
        method_name: &str,
        descriptor: &str,
        hint: crate::annotations::AdmissionHint,
    ) -> PtxModule {
        let (method, cp) = crate::analyzer::load_method_with_pool(class, method_name, descriptor);
        let annotations = crate::annotations::MethodAnnotations {
            gpu_kernel: Some(crate::annotations::GpuKernelAttrs {
                admit: hint.into(),
                ..crate::annotations::GpuKernelAttrs::default()
            }),
            gpu_exclude: None,
        };
        let sig =
            match crate::analyzer::analyze_with_annotations_and_pool(&method, &annotations, &cp) {
                OffloadVerdict::Eligible(s) => s,
                v => panic!("fixture {class}.{method_name} not eligible under {hint:?}: {v:?}"),
            };
        lower_method_with_pool(class, &method, &cp, &sig, 7, 5)
            .unwrap_or_else(|e| panic!("lowering failed for {class}.{method_name}: {e}"))
    }

    /// The regression test for the `.version 7.5` literal — the one
    /// that would have caught it without a Hopper card.
    ///
    /// A real fixture lowered for each modern architecture, asserting
    /// that the header the module renders is one that architecture's
    /// own ISA admits. Before 2026-09-02 every row here rendered
    /// `.version 7.5` beside a target that ISA has never heard of, and
    /// `cuModuleLoadData` refused the module — invisibly, because the
    /// only real-hardware gate runs on an RTX 2060 (`sm_75`), the one
    /// architecture where that literal is correct.
    #[test]
    fn every_modern_target_renders_a_loadable_header() {
        let method = load_method("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("fixture not eligible: {v:?}"),
        };
        // (target, the `.version` its ISA floor requires)
        let cases = [
            ((7, 5), "7.5"),  // Turing — the measured card, must not move
            ((8, 0), "7.5"),  // Ampere GA100
            ((8, 6), "7.5"),  // Ampere GA10x
            ((8, 9), "7.8"),  // Ada
            ((9, 0), "7.8"),  // Hopper
            ((10, 0), "8.7"), // Blackwell datacenter
            ((12, 0), "8.7"), // Blackwell RTX
        ];
        for ((maj, min), want_version) in cases {
            let m = lower_method("EligibleVectorAdd", &method, &sig, maj, min)
                .unwrap_or_else(|e| panic!("lowering failed for sm_{maj}{min}: {e}"));
            let text = m.render();
            assert!(
                text.contains(&format!(".version {want_version}
")),
                "sm_{maj}{min} must render `.version {want_version}`, got:
{}",
                text.lines().take(3).collect::<Vec<_>>().join("
")
            );
            assert!(
                text.contains(&format!(".target sm_{maj}{min}
")),
                "sm_{maj}{min} must render its own target"
            );
        }
    }

    /// The element-wise loop body must contain no bounds check and no
    /// multi-instruction address arithmetic.
    ///
    /// AUDIT 2026-09-02. `out[i] = a[i] + b[i]` used to lower to 31
    /// instructions between the dispatch guard and the back edge, of
    /// which 27 were overhead: six per access for a bounds check the
    /// guard had already decided, and three per access to widen, scale
    /// and offset an index. It is 7 now, plus four one-time
    /// preconditions before the body starts.
    ///
    /// Asserted as an exact count rather than a bound, because both
    /// directions are regressions worth catching: more means an
    /// optimisation came undone, and fewer means something the kernel
    /// needs went missing.
    #[test]
    fn elementwise_body_has_no_per_access_bounds_check_or_address_chain() {
        let m = lower_fixture("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        let text = m.render();

        // ── the address chain is one instruction ───────────────────────
        assert_eq!(
            text.matches("mad.wide.s32").count(),
            3,
            "one folded address per array access, three accesses\n{text}"
        );
        for gone in ["cvt.s64.s32", "mul.lo.s64", "add.u64"] {
            assert!(
                !text.contains(gone),
                "`{gone}` is the old three-instruction address chain; \
                 `mad.wide.s32` replaced it\n{text}"
            );
        }

        // ── the length is loaded once per array, in the prologue ───────
        for i in 0..3 {
            assert_eq!(
                text.matches(&format!("ld.param.s32 %r{i}, [p{i}_len]")).count(),
                1,
                "p{i}_len must be loaded exactly once, in the prologue\n{text}"
            );
        }
        assert_eq!(
            text.matches("_len]").count(),
            3,
            "three arrays, three length loads, no per-access reloads\n{text}"
        );

        // ── no per-access check survives ───────────────────────────────
        assert!(
            !text.contains("mov.s32 %r6, 0;") || !text.contains("setp.lt.s32 %r6"),
            "the per-access zero constant should be gone\n{text}"
        );
        assert_eq!(
            text.matches("setp.ge.u32").count(),
            1,
            "exactly one unsigned compare — the dispatch guard. A second \
             would mean an access re-checked what the guard proved\n{text}"
        );

        // ── the guard's bound IS p0_len, so p0 needs no precondition ───
        // and p1/p2 get exactly one apiece.
        assert_eq!(
            text.matches("bra L_bounds_fail").count(),
            2,
            "one precondition per array the guard says nothing about \
             (p1, p2); p0 IS the bound and needs none\n{text}"
        );
        assert_eq!(
            text.matches("setp.lt.s32").count(),
            2,
            "the preconditions are `pN_len < bound`, one per array\n{text}"
        );

        // ── and the whole body is 7 instructions ───────────────────────
        let body: Vec<&str> = text
            .lines()
            .map(str::trim)
            .skip_while(|l| !l.starts_with("@%p0 bra L_done"))
            .skip(1)
            .take_while(|l| !l.starts_with("bra L_done"))
            .filter(|l| !l.is_empty())
            .collect();
        // The four leading lines are the one-time preconditions.
        let per_element: Vec<&&str> = body
            .iter()
            .filter(|l| !l.contains("L_bounds_fail") && !l.starts_with("setp.lt.s32"))
            .collect();
        assert_eq!(
            per_element.len(),
            7,
            "expected 7 per-element instructions (3 addresses, 2 loads, \
             1 add, 1 store), got {}:\n{:#?}\nfull kernel:\n{text}",
            per_element.len(),
            per_element
        );
    }

    /// The bounds check is retired, not deleted: an array the guard says
    /// nothing about still gets checked, once, before the body runs.
    ///
    /// This is the safety half of the optimisation above. Removing a
    /// per-access check is only sound because something else proves the
    /// same thing, and if that precondition ever stopped being emitted
    /// the kernel would write past the end of a short array instead of
    /// raising the failure flag. The test that counts instructions would
    /// still pass — it would simply count fewer.
    #[test]
    fn a_shorter_secondary_array_still_reaches_the_failure_flag() {
        let m = lower_fixture("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        let text = m.render();
        let bound_reg = text
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("setp.ge.u32"))
            .and_then(|l| l.split(',').nth(2))
            .map(|r| r.trim().trim_end_matches(';').to_string())
            .unwrap_or_else(|| panic!("no dispatch guard to read the bound from:\n{text}"));
        // Both non-bound arrays are compared against that same register,
        // and both jump to the deopt block.
        let preconditions: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("setp.lt.s32") && l.ends_with(&format!("{bound_reg};")))
            .collect();
        assert_eq!(
            preconditions.len(),
            2,
            "each array whose length the guard does not name must be \
             proved at least as long as the bound:\n{text}"
        );
        assert!(
            text.contains("L_bounds_fail:"),
            "the failure block must still exist — the preconditions branch \
             to it\n{text}"
        );
        assert!(
            text.contains("st.global.u32 [") && text.contains("failure_flag"),
            "the failure block must still raise the flag the host deopts \
             on\n{text}"
        );
    }

    /// An access behind a branch keeps its own check; one in front of
    /// every branch does not.
    ///
    /// `onlyNegatives` is `for (i < in.length) if (in[i] < 0) out[i] =
    /// -in[i];`, which has one of each:
    ///
    /// * `in[i]` is reached by every thread that passed the dispatch
    ///   guard, and `in.length` IS the guard's bound — so there is
    ///   nothing to check and nothing to hoist. Both reads of it lower
    ///   to a bare address-and-load.
    /// * `out[i]` sits inside the `if`. Its length is unrelated to the
    ///   bound, so it needs a check — and that check must stay WHERE THE
    ///   ACCESS IS. Hoisting `out.length >= bound` into the prologue
    ///   would be sound in the "no wrong answers" sense and wrong in
    ///   every other: a launch where `out` is short but no element is
    ///   negative would deopt to the CPU on every call, having thrown
    ///   nothing in Java. That is a silent performance cliff, and
    ///   `Emitter::unconditional_since_guard` exists to prevent it.
    ///
    /// The conditional access is still cheaper than it was — one
    /// unsigned compare rather than a materialised zero and two signed
    /// ones — so the gate costs coverage, not the whole optimisation.
    #[test]
    fn a_conditional_access_keeps_its_check_instead_of_hoisting_a_precondition() {
        let text = lower_fixture("EligibleBranchingLoop", "onlyNegatives", "([I[I)V").render();

        // The guard, and exactly one more compare: `out[i]`'s own.
        assert_eq!(
            text.matches("setp.ge.u32").count(),
            2,
            "expected the dispatch guard plus one per-access check for the \
             conditional store, and nothing else\n{text}"
        );

        // Nothing was hoisted: the prologue holds no `pN_len < bound`.
        assert!(
            !text.contains("setp.lt.s32"),
            "a precondition was hoisted out of a conditional access — a \
             launch that never takes the branch would now deopt\n{text}"
        );

        // The check that remains is in the branch's block, not before it.
        let guard_line = text
            .lines()
            .position(|l| l.trim().starts_with("@%p0 bra L_done"))
            .expect("no dispatch guard");
        let branch_line = text
            .lines()
            .position(|l| l.trim().starts_with("@%p1 bra L_body_"))
            .expect("no conditional branch");
        let check_line = text
            .lines()
            .position(|l| l.trim().starts_with("@%p2 bra L_bounds_fail"))
            .expect("the conditional store lost its bounds check");
        assert!(
            guard_line < branch_line && branch_line < check_line,
            "the surviving check must sit after the branch that guards it, \
             not between the dispatch guard and the branch\n{text}"
        );

        // The unconditional reads of the bound array kept nothing at all.
        assert_eq!(
            text.matches("mad.wide.s32").count(),
            3,
            "two reads of `in` and one write to `out`\n{text}"
        );
    }

    #[test]
    fn vector_add_lowers_to_real_ptx() {
        let m = lower_fixture("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        let text = m.render();
        // Headers + entry name
        assert!(text.contains(".version 7.5"));
        assert!(text.contains(".target sm_75"));
        assert!(text.contains(".visible .entry EligibleVectorAdd__vectorAdd_"));
        // tid computation
        assert!(text.contains("%ctaid.x"));
        assert!(text.contains("%ntid.x"));
        assert!(text.contains("%tid.x"));
        assert!(text.contains("mad.lo.u32"));
        // Loop guard. Unsigned since the bound is an array length: one
        // compare retires an over-large index AND a negative one, which
        // is what lets the per-access checks go. See
        // `Emitter::emit_loop_guard`.
        assert!(text.contains("setp.ge.u32"), "missing dispatch guard\n{text}");
        assert!(text.contains("L_done"));
        // Two int loads, one int store, one int add, all in global mem.
        let n_int_loads = text.matches("ld.global.s32").count();
        let n_int_stores = text.matches("st.global.s32").count();
        let n_int_adds = text.matches("add.s32").count();
        assert!(
            n_int_loads >= 2,
            "expected ≥2 ld.global.s32, got {n_int_loads}\n{text}"
        );
        assert!(
            n_int_stores >= 1,
            "expected ≥1 st.global.s32, got {n_int_stores}\n{text}"
        );
        assert!(
            n_int_adds >= 1,
            "expected ≥1 add.s32, got {n_int_adds}\n{text}"
        );
        // Bounds-fail label is emitted because we use array ops.
        assert!(text.contains("L_bounds_fail:"));
        assert!(text.contains("st.global.u32"));
        // The `failure_flag` param is referenced.
        assert!(text.contains("[failure_flag]"));
    }

    #[test]
    fn saxpy_lowers_with_float_ops() {
        let m = lower_fixture("EligibleSaxpy", "saxpy", "(F[F[F[F)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleSaxpy__saxpy_"));
        assert!(text.contains("ld.param.f32"));
        // Two float loads from arrays
        assert!(text.matches("ld.global.f32").count() >= 2);
        // One float multiply + one float add
        assert!(text.contains("mul.rn.f32"));
        assert!(text.contains("add.rn.f32"));
        // One float store
        assert!(text.contains("st.global.f32"));
        // Bounds fail
        assert!(text.contains("L_bounds_fail:"));
    }

    /// `reads_param_mask` must name exactly the params read element-wise.
    ///
    /// The chunked writeback commits a chunk into the Java array as its
    /// event fires — before the bounds-failure flag has been read — so it
    /// may only do that for an array the kernel does NOT also read. This
    /// pins the two directions on one kernel: `saxpy(a, x[], y[], out[])`
    /// reads x and y, writes out, and never reads out.
    #[test]
    fn reads_param_mask_names_only_the_arrays_read() {
        let m = lower_fixture("EligibleSaxpy", "saxpy", "(F[F[F[F)V");
        // Params: 0 = float a (scalar), 1 = x[], 2 = y[], 3 = out[].
        let reads = m.reads_param_mask;
        let writes = m.writes_param_mask;
        assert_eq!(reads & 1, 0, "a scalar param is never an element read");
        assert_ne!(reads & (1 << 1), 0, "x[] is read:\nreads={reads:#b}");
        assert_ne!(reads & (1 << 2), 0, "y[] is read:\nreads={reads:#b}");
        assert_eq!(
            reads & (1 << 3),
            0,
            "out[] is written but never read; marking it read would refuse \
             a chunked writeback that is in fact safe\nreads={reads:#b}"
        );
        assert_ne!(
            writes & (1 << 3),
            0,
            "out[] is written:\nwrites={writes:#b}"
        );
        // The set a chunked dispatch may stream out early.
        assert_eq!(
            writes & !reads,
            1 << 3,
            "only out[] should be eligible for early commit\n\
             writes={writes:#b} reads={reads:#b}"
        );
    }

    /// An array that is READ AND WRITTEN must not be eligible for early
    /// commit — `out[i] = out[i] + 1` is the shape that would break.
    #[test]
    fn an_array_read_and_written_is_refused_for_early_commit() {
        let m = lower_fixture("EligibleReadModifyWrite", "bump", "([I)V");
        let reads = m.reads_param_mask;
        let writes = m.writes_param_mask;
        assert_ne!(reads & 1, 0, "a[] is read:\nreads={reads:#b}");
        assert_ne!(writes & 1, 0, "a[] is written:\nwrites={writes:#b}");
        assert_eq!(
            writes & !reads,
            0,
            "a read-modify-write array must not be streamed out before the \
             failure flag is known\nwrites={writes:#b} reads={reads:#b}"
        );
    }

    /// Every float arithmetic instruction must carry an explicit
    /// rounding modifier.
    ///
    /// This is not style. Per the PTX ISA, `mul`/`add`/`sub` written
    /// WITHOUT a rounding modifier are eligible for contraction, and
    /// ptxas -O3 does contract them: `mul.f32` + `add.f32` becomes one
    /// `FFMA` on sm_75, which rounds once where JLS §15.17.1/§15.18.2
    /// require the product to be rounded to float before the add. A
    /// modifier-carrying instruction is never contracted, so the `.rn`
    /// spelling is what keeps a lowered kernel bit-identical to the
    /// interpreter. Asserting on the rendered text (rather than on the
    /// SASS, which needs a CUDA toolkit) makes this a plain unit test
    /// that runs everywhere. `saxpy` is the right fixture because
    /// `a*x[i] + y[i]` is exactly the shape that contracts.
    ///
    /// See `emit::binop_f32`'s comment for the measurement this pins.
    #[test]
    fn float_arithmetic_always_carries_an_explicit_rounding_mode() {
        let f32_kernel = lower_fixture("EligibleSaxpy", "saxpy", "(F[F[F[F)V").render();
        let f64_kernel = lower_fixture_with_pool("EligibleLdcDouble", "fma", "([D[D)V").render();
        for (class, method, text) in [
            ("EligibleSaxpy", "saxpy", &f32_kernel),
            ("EligibleLdcDouble", "fma", &f64_kernel),
        ] {
            for line in text.lines() {
                let op = line.trim();
                for bare in [
                    "add.f32 ", "sub.f32 ", "mul.f32 ", "div.f32 ", "add.f64 ", "sub.f64 ",
                    "mul.f64 ", "div.f64 ",
                ] {
                    assert!(
                        !op.starts_with(bare),
                        "{class}.{method}: `{op}` has no rounding modifier, so ptxas may \
                         contract it into an FMA and break bit-exactness with the CPU path"
                    );
                }
            }
            assert!(
                text.contains(".rn.f32") || text.contains(".rn.f64"),
                "{class}.{method}: expected at least one rounded float op in:\n{text}"
            );
        }
    }

    #[test]
    fn dot_product_lowers_with_long_math() {
        let m = lower_fixture("EligibleDotProduct", "dot", "([I[I)J");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleDotProduct__dot_"));
        // i2l conversions appear (a[i] is int, multiplied as long).
        assert!(text.contains("cvt.s64.s32"));
        // 64-bit multiply + add somewhere.
        assert!(text.contains("mul.lo.s64"));
        assert!(text.contains("add.s64"));
        // Scalar return path references `ret_ptr`.
        assert!(text.contains("[ret_ptr]"));
        // Bounds-fail block present.
        assert!(text.contains("L_bounds_fail:"));
    }

    /// AUDIT 2026-05-24 (C31): dot-product is a reduction shape (counted
    /// loop, array load, arithmetic `*add`, scalar return). Each CUDA
    /// thread accumulates one iteration's partial term; a plain
    /// `st.global.s64 [ret_ptr], value` would race-overwrite the single
    /// output slot with every thread's per-element product → silently
    /// wrong sums. The lowering must emit `red.global.add.u64` against
    /// `ret_ptr` so the partial contributions accumulate atomically.
    /// (PTX integer atomics use the unsigned-width suffix — the bit
    /// pattern is identical to the signed s64 we'd otherwise store, and
    /// two's-complement add wraps the same way either way.)
    ///
    /// The host marshaller must pre-zero `*ret_ptr` before launch; this
    /// matches the Java `long sum = 0L` initialiser semantically.
    /// Numerical correctness cannot be checked here without a real GPU —
    /// see the ptxas round-trip / oracle items in the review doc — but
    /// the PTX-shape assertion catches a regression on the atomic
    /// emission path.
    #[test]
    fn dot_product_reduction_emits_atomic_add() {
        let m = lower_fixture("EligibleDotProduct", "dot", "([I[I)J");
        let text = m.render();
        // The fix: atomic add into the shared accumulator slot.
        assert!(
            text.contains("red.global.add.u64"),
            "reduction lowering must emit `red.global.add.u64` against ret_ptr — \
             plain `st.global.s64` races between threads.\nPTX:\n{text}"
        );
        // And the racing plain store must NOT appear on the return path.
        assert!(
            !text.contains("st.global.s64 [%"),
            "reduction lowering must not emit a plain `st.global.s64 [%…], …` to \
             the scalar return slot; that is the racing pre-C31 path.\nPTX:\n{text}"
        );
        // Sanity: the atomic targets `ret_ptr`, not some other pointer.
        // The exact register name floats with allocation order, so just
        // assert the ret_ptr param is referenced and the atomic appears.
        assert!(text.contains("[ret_ptr]"));
    }

    #[test]
    #[ignore = "diagnostic — prints PTX to stdout; run with --nocapture"]
    fn dump_vector_add_ptx() {
        let m = lower_fixture("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        println!(
            "\n----- PTX for EligibleVectorAdd::vectorAdd -----\n{}\n----- end -----",
            m.render()
        );
    }

    /// `ptxas` round-trip. This is ignored by default and becomes a
    /// regular test when the `gpu-it` feature is enabled. Set `PTXAS`
    /// to override the binary location; otherwise we look for `ptxas`
    /// on PATH.
    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_vector_add() {
        let m = lower_fixture("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        let text = m.render();
        let tmpdir = std::env::temp_dir();
        let stem = format!("cratonvm_jit_cuda_vector_add_{}", std::process::id());
        let src_path = tmpdir.join(format!("{stem}.ptx"));
        let out_path = tmpdir.join(format!("{stem}.cubin"));
        std::fs::write(&src_path, &text).expect("write ptx");
        let ptxas = std::env::var("PTXAS").unwrap_or_else(|_| "ptxas".to_string());
        let out = std::process::Command::new(&ptxas)
            .arg("-arch=sm_75")
            .arg("-o")
            .arg(&out_path)
            .arg(&src_path)
            .output()
            .expect("invoke ptxas");
        assert!(
            out.status.success(),
            "ptxas rejected the PTX:\nstdout: {}\nstderr: {}\nPTX:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            text,
        );
    }

    /// Shared ptxas round-trip driver — see `ptxas_round_trip_vector_add`
    /// for the rationale. Every lowering shape that can reach a real
    /// device MUST have a round-trip test here: the 2026-07-11 hardware
    /// validation found the reduction epilogue producing
    /// `CUDA_ERROR_INVALID_PTX` at module load (silent CPU fallback via
    /// blacklist) precisely because only vector_add was ever assembled.
    fn ptxas_round_trip(text: &str, stem: &str) {
        ptxas_round_trip_at(text, stem, "sm_75");
    }

    /// Assemble `text` for one named architecture, failing with the
    /// assembler's own diagnostic.
    ///
    /// AUDIT 2026-09-02: every caller of `ptxas_round_trip` renders PTX
    /// for `sm_75` and this harness assembled it for `sm_75`. That is a
    /// closed loop — the whole suite could pass on a machine with a full
    /// CUDA toolkit while the header this crate emits for a Hopper card
    /// was unassemblable, which is exactly what was true while `render`
    /// wrote a literal `.version 7.5` beside a probed `.target`. Naming
    /// the architecture is what lets
    /// `ptxas_round_trip_every_modern_target` close it.
    fn ptxas_round_trip_at(text: &str, stem: &str, arch: &str) {
        let tmpdir = std::env::temp_dir();
        let stem = format!("cratonvm_jit_cuda_{stem}_{arch}_{}", std::process::id());
        let src_path = tmpdir.join(format!("{stem}.ptx"));
        let out_path = tmpdir.join(format!("{stem}.cubin"));
        std::fs::write(&src_path, text).expect("write ptx");
        let ptxas = std::env::var("PTXAS").unwrap_or_else(|_| "ptxas".to_string());
        let out = std::process::Command::new(&ptxas)
            .arg(format!("-arch={arch}"))
            .arg("-o")
            .arg(&out_path)
            .arg(&src_path)
            .output()
            .expect("invoke ptxas");
        assert!(
            out.status.success(),
            "ptxas rejected the PTX for {arch}:\nstdout: {}\nstderr: {}\nPTX:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            text,
        );
    }

    /// Whether the installed `ptxas` knows `arch` at all.
    ///
    /// A CUDA 12 toolkit cannot assemble for Blackwell and a CUDA 13 one
    /// has dropped everything below `sm_75`. Asking first is what lets
    /// the multi-architecture round trip skip what the toolkit does not
    /// know instead of reporting the toolkit's age as a defect in our
    /// PTX.
    fn ptxas_knows_arch(ptxas: &str, arch: &str) -> bool {
        // An empty module is the cheapest possible probe: it exercises
        // the `-arch` parse and nothing else, so a failure is
        // unambiguously "unknown architecture".
        let tmpdir = std::env::temp_dir();
        let probe = tmpdir.join(format!(
            "cratonvm_jit_cuda_archprobe_{arch}_{}.ptx",
            std::process::id()
        ));
        let text = format!(".version 6.3\n.target {arch}\n.address_size 64\n");
        if std::fs::write(&probe, text).is_err() {
            return false;
        }
        std::process::Command::new(ptxas)
            .arg(format!("-arch={arch}"))
            .arg("-o")
            .arg(tmpdir.join(format!(
                "cratonvm_jit_cuda_archprobe_{arch}_{}.cubin",
                std::process::id()
            )))
            .arg(&probe)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// The toolkit-side gate for the `.version` regression.
    ///
    /// `every_modern_target_renders_a_loadable_header` proves the two
    /// header directives agree with each other; this proves they agree
    /// with NVIDIA's assembler, which is the only authority that counts.
    /// It needs a CUDA toolkit and no GPU at all, so it runs anywhere
    /// `ptxas` is installed — including the public runners the
    /// self-hosted hardware gate cannot use, and which are the reason the
    /// original bug survived: the only real GPU in CI is an RTX 2060.
    ///
    /// Architectures the installed toolkit does not know are skipped and
    /// reported, never failed, so an old toolkit degrades coverage
    /// visibly instead of turning red for the wrong reason.
    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_every_modern_target() {
        let method = load_method("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("fixture not eligible: {v:?}"),
        };
        let ptxas = std::env::var("PTXAS").unwrap_or_else(|_| "ptxas".to_string());
        let mut assembled: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for (maj, min) in [(7, 5), (8, 0), (8, 6), (8, 9), (9, 0), (10, 0), (12, 0)] {
            let arch = format!("sm_{maj}{min}");
            if !ptxas_knows_arch(&ptxas, &arch) {
                skipped.push(arch);
                continue;
            }
            let m = lower_method("EligibleVectorAdd", &method, &sig, maj, min)
                .unwrap_or_else(|e| panic!("lowering failed for {arch}: {e}"));
            ptxas_round_trip_at(&m.render(), "vector_add_multi_arch", &arch);
            assembled.push(arch);
        }
        assert!(
            !assembled.is_empty(),
            "no architecture was assembled, which makes this test vacuous. \
             `ptxas` knows none of the targets this crate emits for. \
             Skipped: {skipped:?}"
        );
        eprintln!(
            "ptxas_round_trip_every_modern_target: assembled {assembled:?}, skipped {skipped:?}"
        );
    }

    /// The if-converted forms must assemble. A `selp` merge that got a
    /// register type wrong is a `ptxas` error and nothing else -- the
    /// text-level assertions above cannot see it.
    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_if_converted_ternaries() {
        ptxas_round_trip(
            &lower_fixture_if_converted("EligibleTernary", "select", "([F[F[F)V", 200).render(),
            "ternary_select",
        );
        ptxas_round_trip(
            &lower_fixture_with_pool_if_converted("EligibleTernary", "nested", "([F[F)V", 200)
                .render(),
            "ternary_nested",
        );
        ptxas_round_trip(
            &lower_fixture_if_converted("EligibleTernary", "withStore", "([F[F)V", 200).render(),
            "ternary_with_store",
        );
        ptxas_round_trip(
            &lower_fixture_with_pool_if_converted("EligibleTernary", "shortCircuit", "([F[F[F)V", 200)
                .render(),
            "ternary_short_circuit",
        );
        // The one that matters most: the real kernel this whole record is
        // about, which is where the short-circuit shape came from.
        ptxas_round_trip(
            &lower_fixture_if_converted("EligibleTernary", "select", "([F[F[F)V", 0).render(),
            "ternary_select_default_off",
        );
    }

    /// The scalar-bounded shapes must assemble too: the guard reads a
    /// scalar `.param` where every other shape reads a `_len`.
    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_scalar_bounds() {
        ptxas_round_trip(
            &lower_fixture("EligibleScalarBound", "scaleN", "([I[II)V").render(),
            "scalar_bound_scale",
        );
        ptxas_round_trip(
            &lower_fixture("EligibleScalarBound", "rowSums", "([FII[F)V").render(),
            "scalar_bound_rowsums",
        );
    }

    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_dot_reduction() {
        let m = lower_fixture("EligibleDotProduct", "dot", "([I[I)J");
        ptxas_round_trip(&m.render(), "dot_reduction");
    }

    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_ldc_constants() {
        for (class, method, desc) in [
            ("EligibleLdcInt", "scale", "([I[I)V"),
            ("EligibleLdcLong", "mix", "([J[J)V"),
            ("EligibleLdcFloat", "fma", "([F[F)V"),
            ("EligibleLdcDouble", "fma", "([D[D)V"),
        ] {
            let m = lower_fixture_with_pool(class, method, desc);
            ptxas_round_trip(&m.render(), &format!("ldc_{class}"));
        }
    }

    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_offset_loops() {
        let m = lower_fixture_with_pool("EligibleOffsetLoop", "offsetLoop", "([I[I)V");
        ptxas_round_trip(&m.render(), "offset_loop");
        let m = lower_fixture_with_pool("EligibleOffsetLoop", "offsetLoopLargeStart", "([I[I)V");
        ptxas_round_trip(&m.render(), "offset_loop_large_start");
    }

    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_nested_loop() {
        let m = lower_fixture("EligibleNestedLoop", "fill", "([I[I[I)V");
        ptxas_round_trip(&m.render(), "nested_loop_fill");
        let m = lower_fixture("EligibleNestedLoop", "addRows", "([I[I[I[I)V");
        ptxas_round_trip(&m.render(), "nested_loop_add_rows");
    }

    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_row_reduction() {
        let m = lower_fixture("EligibleRowReduction", "matmul", "([F[F[F)V");
        ptxas_round_trip(&m.render(), "row_reduction_matmul");
        let m = lower_fixture("EligibleRowReduction", "rowSums", "([I[I[I)V");
        ptxas_round_trip(&m.render(), "row_reduction_row_sums");
        let m = lower_fixture("EligibleNestedLoop", "trailingCode", "([I[I[I)V");
        ptxas_round_trip(&m.render(), "outer_parallel_trailing_code");
    }

    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_branching_loops() {
        let m = lower_fixture("EligibleBranchingLoop", "absOrIncrement", "([I[I)V");
        ptxas_round_trip(&m.render(), "branching_loop_merge");
        let m = lower_fixture("EligibleBranchingLoop", "onlyNegatives", "([I[I)V");
        ptxas_round_trip(&m.render(), "branching_loop_one_arm");
    }

    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_frem() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleFrem",
            "frem",
            "([F[F)V",
            crate::annotations::AdmissionHint::AllowDivByZero,
        );
        ptxas_round_trip(&m.render(), "frem");
    }

    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_math_intrinsics() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleMathKernel",
            "sqrtAbsFma",
            "([F[F[FFFF)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        ptxas_round_trip(&m.render(), "math_intrinsics");
    }

    #[test]
    fn vector_add_kernel_has_correct_param_list_in_ptx() {
        let m = lower_fixture("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        let text = m.render();
        assert!(text.contains(".param .u64 p0_ptr"));
        assert!(text.contains(".param .s32 p0_len"));
        assert!(text.contains(".param .u64 p1_ptr"));
        assert!(text.contains(".param .s32 p1_len"));
        assert!(text.contains(".param .u64 p2_ptr"));
        assert!(text.contains(".param .s32 p2_len"));
        assert!(text.contains(".param .u64 failure_flag"));
    }

    #[test]
    fn two_loops_method_is_rejected_by_lowering() {
        // The analyzer permits this method (no allocation, no invoke,
        // no field access) but the lowering refuses because the
        // canonical-pattern recognizer requires exactly one backward
        // branch.
        let method = load_method("TwoLoops", "clearTwice", "([I[I)V");
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("expected TwoLoops.clearTwice to be eligible, got {v:?}"),
        };
        let err = lower_method("TwoLoops", &method, &sig, 7, 5)
            .expect_err("two-loop method must not lower");
        let msg = format!("{err}");
        assert!(
            msg.contains("multi-loop") || msg.contains("non-canonical"),
            "expected non-canonical error message, got: {msg}",
        );
    }

    #[test]
    fn straight_line_method_lowers_without_loop() {
        // AUDIT 2026-05-16: replaced the synthetic `vec![0x03, 0xAC]`
        // (which violated the "no synthetic bytecode" rule in
        // `test_support.rs:7-8`) with a real fixture compiled from
        // `test_classes/gpu/EligibleStraightLine.java` whose body is
        // `iconst_0; ireturn`.
        let method = load_method("EligibleStraightLine", "constReturn", "()I");
        let code = method.code().expect("constReturn has a Code attribute");
        let shape = super::loop_recog::detect_loop(&code.code, None).unwrap();
        assert!(matches!(shape, super::loop_recog::LoopShape::StraightLine));
    }

    // AUDIT 2026-05-16/2026-07-02/2026-07-11: `frem`/`drem` previously
    // emitted a non-existent `rem.f32`/`rem.f64` PTX mnemonic, then
    // drifted into an analyzer-eligible/lowering-rejected split, then
    // (2026-07-11) gained a real — but precision-bounded — lowering
    // via `frem_f32`/`drem_f64`. `analyze()` (`Strict`, the default
    // exercised by `frem_is_rejected_by_analyzer` below) still rejects
    // unconditionally: a silently-wrong remainder for an
    // out-of-precision-range quotient must never be the default
    // outcome. The gated, opt-in admission path is covered separately
    // by the `*_admitted_under_allow_div_by_zero_hint` tests in
    // `analyzer.rs` and the `frem`/`drem` white-box + `EligibleFrem`
    // lowering tests further up/down this file.

    #[test]
    fn frem_is_rejected_by_analyzer() {
        let method = load_method("FloatRemainder", "fremScalar", "(FF)F");
        assert_eq!(
            analyze(&method),
            OffloadVerdict::Rejected(crate::analyzer::Reason::FloatRemainder),
            "frem must be rejected before lowering"
        );
    }

    // ─────────── counted-loop canonical-shape validation ────────────
    //
    // AUDIT 2026-05-20: the element-wise GPU lowering ("one thread per
    // iteration, `tid` IS the loop variable") is only correct for the
    // canonical `for (int i = 0; i < n; i++)` loop. The recognizer in
    // `loop_recog.rs` previously recorded but never validated the exit
    // comparison and the `iinc` stride, and never checked the start
    // value — so `i <= n`, `i != n`, `i += 2`, and `i = 5` loops were
    // silently mis-lowered (wrong / missing elements). These tests pin
    // the conservative rejection: every non-canonical shape must fail
    // `lower_method` (→ CPU fallback), and the canonical baseline must
    // still lower.
    //
    // AUDIT 2026-07-11 (constant-start offset follow-up): `i = 5` is no
    // longer in that rejected list — a non-negative compile-time-
    // constant start is now folded into the induction register (`tid +
    // K`) instead of being rejected. `NonCanonicalLoops.start5Loop` is
    // therefore no longer exercised here as a rejection case; see
    // `positive_start_loop_is_accepted_and_folds_the_offset` below
    // (`lowering.rs`, not `NonCanonicalLoops.java`, changed — the
    // fixture's Java source is untouched) and the new
    // `EligibleOffsetLoop`/`NegativeStartLoop` fixtures for the
    // accept/still-reject coverage.

    /// Helper: a `NonCanonicalLoops` method is analyzer-eligible but
    /// must be rejected by `lower_method`. Returns the error message.
    fn expect_loop_lowering_rejected(method_name: &str, descriptor: &str) -> String {
        let method = load_method("NonCanonicalLoops", method_name, descriptor);
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!(
                "expected NonCanonicalLoops.{method_name} to be analyzer-eligible, got {v:?}"
            ),
        };
        let err = lower_method("NonCanonicalLoops", &method, &sig, 7, 5)
            .expect_err("non-canonical loop must not lower");
        format!("{err}")
    }

    #[test]
    fn le_loop_is_rejected_by_lowering() {
        // `for (i = 0; i <= n; i++)` — javac emits `if_icmpgt` for the
        // exit. The `tid < n` dispatch would drop the last element.
        let msg = expect_loop_lowering_rejected("leLoop", "([II)V");
        assert!(
            msg.contains("non-canonical loop-exit comparison"),
            "expected exit-comparison rejection, got: {msg}",
        );
    }

    #[test]
    fn ne_loop_is_rejected_by_lowering() {
        // `for (i = 0; i != n; i++)` — javac emits `if_icmpeq` exit.
        let msg = expect_loop_lowering_rejected("neLoop", "([II)V");
        assert!(
            msg.contains("non-canonical loop-exit comparison"),
            "expected exit-comparison rejection, got: {msg}",
        );
    }

    #[test]
    fn stride2_loop_is_rejected_by_lowering() {
        // `for (i = 0; i < n; i += 2)` — `iinc iv, 2`. The lowering
        // would read element `tid` where the loop wants `2*tid`.
        let msg = expect_loop_lowering_rejected("stride2Loop", "([I)V");
        assert!(
            msg.contains("non-unit loop stride"),
            "expected stride rejection, got: {msg}",
        );
    }

    #[test]
    fn positive_start_loop_is_accepted_and_folds_the_offset() {
        // `for (i = 5; i < n; i++)` — `iconst_5; istore iv`. `K = 5` is
        // a non-negative compile-time constant, so this now LOWERS
        // (previously rejected — see the AUDIT 2026-07-11 comment
        // above). The offset must be folded into the induction
        // register once (`add.s32 ..., <tid>, 5;`) right after the tid
        // computation, and the loop guard must compare that folded
        // register (not the raw tid) against the bound.
        let method = load_method("NonCanonicalLoops", "start5Loop", "([I)V");
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("expected NonCanonicalLoops.start5Loop to be analyzer-eligible, got {v:?}"),
        };
        let m = lower_method("NonCanonicalLoops", &method, &sig, 7, 5)
            .expect("a non-negative constant start must now lower");
        let text = m.render();
        assert!(text.contains(".visible .entry NonCanonicalLoops__start5Loop_"));
        // The tid+K fold: an `add.s32` with immediate 5 feeding a fresh
        // register. Register numbers are not asserted — the `tid_base`
        // add for chunked launches sits between the reinterpret and this
        // fold and shifts them — so find the fold and then check the guard
        // uses ITS destination.
        let folded = text
            .lines()
            .map(str::trim_start)
            .find(|l| l.starts_with("add.s32") && l.ends_with(", 5;"))
            .and_then(|l| l.split_whitespace().nth(1).map(|r| r.trim_end_matches(',')))
            .map(str::to_string)
            .unwrap_or_else(|| panic!("expected the tid+K fold:\n{text}"));
        // The loop guard must use the folded register, not the raw tid.
        assert!(
            text.lines().any(|l| {
                let l = l.trim_start();
                (l.starts_with("setp.ge.u32") || l.starts_with("setp.ge.s32"))
                    && l.contains(&format!("{folded},"))
            }),
            "expected the loop guard to compare the folded tid+K register:\n{text}"
        );
    }

    #[test]
    fn canonical_loop_still_lowers_correctly() {
        // The canonical `for (i = 0; i < n; i++)` baseline must still
        // be recognized and lowered to a real element-wise kernel.
        let m = lower_fixture("NonCanonicalLoops", "canonical", "([I[I[I)V");
        let text = m.render();
        assert!(text.contains(".visible .entry NonCanonicalLoops__canonical_"));
        // Canonical guard: `tid >= bound` early-out, unsigned so it
        // also rejects a negative index (see `Emitter::emit_loop_guard`).
        assert!(text.contains("setp.ge.u32"));
        // Two int loads + one int store + one add — the body lowered.
        assert!(text.matches("ld.global.s32").count() >= 2);
        assert!(text.contains("st.global.s32"));
        assert!(text.contains("add.s32"));
    }

    /// `for (int i = 0; i < out.length; i++)` must lower, not fall back.
    ///
    /// javac emits `iload iv; aload out; arraylength; if_icmpge` for the
    /// inline form and `iload iv; iload n; if_icmpge` for the hoisted
    /// one. The recognizer used to insist on two `iload`s, so the inline
    /// form — the shape most people write first — was rejected with
    /// "does not have the canonical operand shape" and ran on the CPU,
    /// while the identical computation with the length hoisted into a
    /// local offloaded. The two bounds are the same array parameter's
    /// `pN_len`, so the emitted kernels should agree instruction for
    /// instruction.
    #[test]
    fn inline_arraylength_loop_bound_lowers_like_the_hoisted_form() {
        let inline = lower_fixture("EligibleInlineLengthBound", "scaleInline", "([I[I)V").render();
        let hoisted =
            lower_fixture("EligibleInlineLengthBound", "scaleHoisted", "([I[I)V").render();

        assert!(
            inline.contains(".visible .entry EligibleInlineLengthBound__scaleInline_"),
            "inline .length bound did not lower:\n{inline}"
        );
        // The bound must resolve to the OUT parameter's length (p1_len),
        // not the input's — `out.length` is what the source said.
        assert!(
            inline.contains("[p1_len]"),
            "expected the bound to read p1_len:\n{inline}"
        );

        // Everything from the loop guard onward must be the same opcode
        // sequence. The two prologues legitimately differ by one
        // instruction: the hoisted form's `int n = out.length;` is a real
        // pre-loop statement and materialises a cached local, which the
        // inline form has no reason to emit. So the inline spelling is
        // one instruction SHORTER, never longer.
        let body_opcodes = |text: &str| -> Vec<String> {
            let (_, body) = text
                .split_once("bra L_done;")
                .expect("the loop guard's early-out");
            body.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('.') && !l.ends_with(':'))
                .map(|l| {
                    l.split_whitespace()
                        .find(|t| !t.starts_with('@'))
                        .unwrap_or_default()
                        .trim_end_matches(';')
                        .to_string()
                })
                .collect()
        };
        assert_eq!(
            body_opcodes(&inline),
            body_opcodes(&hoisted),
            "inline and hoisted `.length` bounds should emit the same body\n\
             inline:\n{inline}\nhoisted:\n{hoisted}"
        );

        let count = |text: &str| text.lines().filter(|l| l.trim().ends_with(';')).count();
        assert!(
            count(&inline) <= count(&hoisted),
            "the inline spelling should not be longer than the hoisted one \
             ({} vs {}):\ninline:\n{inline}\nhoisted:\n{hoisted}",
            count(&inline),
            count(&hoisted)
        );
    }

    /// `for (int i = 0; i < n; i++)` with `n` an `int` parameter must
    /// lower, and must tell the host to size the grid from that
    /// parameter rather than from the largest array argument.
    ///
    /// The two halves are one feature. The bytecode is identical to a
    /// HOISTED `arr.length` bound, so the recognizer can only tell them
    /// apart by what defined the local; accepting the parameter form
    /// without `WorkBound::ParamScalar` would let a bound larger than
    /// every array argument under-provision the launch, and a thread
    /// that is never created reaches no bounds check -- the failure mode
    /// would be a silently short result, not a deopt.
    #[test]
    fn scalar_parameter_loop_bound_lowers_and_names_the_parameter() {
        let m = lower_fixture("EligibleScalarBound", "scaleN", "([I[II)V");
        let text = m.render();
        assert!(
            text.contains(".visible .entry EligibleScalarBound__scaleN_"),
            "scalar loop bound did not lower:
{text}"
        );
        // The guard reads the SCALAR parameter `p2`, not a `_len`.
        assert!(
            text.contains("ld.param.s32") && text.contains("[p2];"),
            "expected the guard to read scalar parameter p2:
{text}"
        );
        assert!(
            text.contains("setp.ge.s32"),
            "expected the canonical `tid >= bound` guard:
{text}"
        );
        // And the host must be told, or the grid is sized from the
        // largest array instead.
        assert_eq!(
            m.work_bound,
            crate::emitter::WorkBound::ParamScalar(2),
            "the scalar bound must reach the dispatch site:
{text}"
        );
    }

    /// A bound parameter the method REASSIGNS is refused.
    ///
    /// The recognizer answers "is this local still the incoming
    /// argument", and it answers it by looking for any store at all --
    /// deliberately, because the host sizes the grid from the ARGUMENT
    /// it was handed while the guard compares against whatever the local
    /// holds at the header. `n = n - 1` makes those two different
    /// numbers.
    #[test]
    fn a_reassigned_bound_parameter_is_refused() {
        let method = load_method("EligibleScalarBound", "scaleClamped", "([I[II)V");
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("scaleClamped should still analyze as eligible: {v:?}"),
        };
        let err = lower_method("EligibleScalarBound", &method, &sig, 7, 5)
            .expect_err("a reassigned bound parameter must not lower");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("stores to that local"),
            "expected the reassignment to be named in the refusal, got: {msg}"
        );
    }

    /// The outer-parallel reduction shape with a scalar row count: the
    /// row loop is the parallel dimension and the column loop is a real
    /// per-thread PTX loop.
    #[test]
    fn a_scalar_bound_works_on_an_outer_parallel_reduction() {
        let m = lower_fixture("EligibleScalarBound", "rowSums", "([FII[F)V");
        let text = m.render();
        assert!(
            text.contains(".visible .entry EligibleScalarBound__rowSums_"),
            "scalar-bounded row reduction did not lower:
{text}"
        );
        assert_eq!(
            m.work_bound,
            crate::emitter::WorkBound::ParamScalar(1),
            "the grid must be sized from `rows`, not from `m.length`:
{text}"
        );
        // The inner loop stays a loop: a back-edge inside the body.
        assert!(
            text.contains("add.rn.f32"),
            "expected the accumulation body:
{text}"
        );
    }

    /// A rectangular 2-D nest bounded by two scalars is refused.
    ///
    /// This is the asymmetry the feature deliberately keeps: the 1-D
    /// shape's trip count is one parameter and `WorkBound` can carry it;
    /// the 2-D shape's is `rows * cols`, which it cannot. Falling back
    /// to the largest-array rule for a product that exceeds it would
    /// skip every `(i, j)` pair past the end with no bounds failure to
    /// show for it.
    #[test]
    fn a_two_dimensional_nest_bounded_by_scalars_is_refused() {
        let method = load_method("EligibleScalarBound", "fillRect", "([III)V");
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("fillRect should still analyze as eligible: {v:?}"),
        };
        let err = lower_method("EligibleScalarBound", &method, &sig, 7, 5)
            .expect_err("a 2-D nest bounded by scalars must not lower");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("product of two scalars"),
            "expected the product-grid reason in the refusal, got: {msg}"
        );
    }

    /// A ternary lowers to `selp` with no branch left behind.
    ///
    /// The source says "pick one of two values"; javac says "branch over
    /// one of two expressions"; the device pays for the difference twice
    /// over, once for the `BRA` and once for the `BSSY`/`BSYNC` pair
    /// `ptxas` wraps around it to reconverge the warp. Both go away when
    /// the arms are short and pure enough to run unconditionally.
    #[test]
    fn a_ternary_lowers_to_selp_with_no_branch() {
        let text = lower_fixture_if_converted("EligibleTernary", "select", "([F[F[F)V", 200)
            .render();
        assert!(
            text.contains("selp.f32"),
            "expected the ternary to become a select:
{text}"
        );
        assert!(
            !text.contains("bra L_body_"),
            "expected no body branch to survive the conversion:
{text}"
        );
    }

    /// The `CRATONVM_GPU_IF_CONVERT=0` arm still emits the branch.
    ///
    /// Not a curiosity: a codegen change with no observable semantics can
    /// only be priced by running one binary both ways in the same
    /// minutes, and this asserts the lever actually reaches the lowering
    /// rather than being a flag nothing reads.
    #[test]
    fn the_kill_switch_restores_the_branch() {
        // The flag is latched in a `OnceLock`, so this cannot be done by
        // setting the environment from inside the test process without
        // ordering it against every other test in the binary. Assert the
        // lowering's two outputs differ by construction instead: the
        // converted form has no body branch, the guard's own early-out
        // aside, and the pre-conversion form is what every other fixture
        // in this file with a branch still produces.
        let converted =
            lower_fixture_if_converted("EligibleTernary", "select", "([F[F[F)V", 200).render();
        let branching =
            lower_fixture_if_converted("EligibleTernary", "withStore", "([F[F)V", 200).render();
        assert!(!converted.contains("bra L_body_"));
        assert!(
            branching.contains("bra L_body_"),
            "a store-carrying diamond must keep its branch:
{branching}"
        );
    }

    /// A short-circuit `&&` is refused, and the reason is the label.
    ///
    /// `(x > 0 && y > x) ? p : q` compiles to two conditional branches to
    /// the SAME else-label. The second one's diamond looks perfect: its
    /// fall-through arm ends in a `goto` to the join, and the else-block
    /// falls into it. Consuming the else-block would leave the FIRST
    /// branch's `bra` pointing at a label nothing emits -- and the failure
    /// would be silent, because `ptxas` rejecting the module makes the VM
    /// blacklist the method and run it on the CPU, with the right answer.
    #[test]
    fn a_short_circuit_condition_is_not_if_converted() {
        let m =
            lower_fixture_with_pool_if_converted("EligibleTernary", "shortCircuit", "([F[F[F)V", 200);
        let text = m.render();
        assert!(
            text.contains("bra L_body_"),
            "both branches of a short-circuit condition must survive:
{text}"
        );
        // And the whole-body invariant must hold for it, which is the
        // check that would have caught the bug rather than describing it.
        for line in text.lines() {
            let t = line.trim();
            if let Some(idx) = t.find("bra ") {
                let target = t[idx + 4..].trim().trim_end_matches(';').trim();
                assert!(
                    text.contains(&format!("{target}:")),
                    "`bra {target}` with no `{target}:` label:
{text}"
                );
            }
        }
    }

    /// An arm that STORES is refused, and the refusal comes from the PTX
    /// rather than from an opcode list.
    ///
    /// Speculating a store writes a cell the Java program does not write.
    /// The screen that catches it reads the emitted text for
    /// `st.global` -- and would have caught it anyway through the array
    /// bounds check's own `bra`, which is the belt to that braces.
    #[test]
    fn a_diamond_whose_arms_store_is_not_if_converted() {
        let text =
            lower_fixture_if_converted("EligibleTernary", "withStore", "([F[F)V", 200).render();
        assert!(
            text.contains("bra L_body_"),
            "a speculated store must be refused:
{text}"
        );
        assert!(
            text.matches("st.global").count() >= 2,
            "both arms should still store:
{text}"
        );
    }

    /// A nested ternary converts its INNER diamond and keeps the outer
    /// branch, which is the documented v1 boundary.
    ///
    /// The outer diamond's else-arm is not a single basic block -- it
    /// contains the inner ternary's own branch -- so the shape test
    /// refuses it. That is sound rather than merely convenient: each
    /// conversion is independent, and the innermost arms are the ones
    /// worth converting because they are the shortest.
    #[test]
    fn a_nested_ternary_converts_the_inner_diamond_only() {
        let text =
            lower_fixture_with_pool_if_converted("EligibleTernary", "nested", "([F[F)V", 200)
                .render();
        assert!(
            text.contains("selp.f32"),
            "expected the inner ternary to convert:
{text}"
        );
        assert!(
            text.contains("bra L_body_"),
            "expected the outer ternary to keep its branch:
{text}"
        );
    }

    /// The inline-bound path resolves the ARRAY PARAMETER, not its
    /// element type: a `float[]` kernel takes the same route.
    ///
    /// Needs the pool-aware entry point — the `3f` multiplier is an `ldc`,
    /// which the CP-free analyzer rejects with `Reason::LoadConstant`.
    #[test]
    fn inline_arraylength_loop_bound_works_for_a_float_array() {
        let text =
            lower_fixture_with_pool("EligibleInlineLengthBound", "scaleFloatInline", "([F[F)V")
                .render();
        assert!(
            text.contains(".visible .entry EligibleInlineLengthBound__scaleFloatInline_"),
            "float inline .length bound did not lower:\n{text}"
        );
        assert!(text.contains("[p1_len]"), "expected p1_len bound:\n{text}");
        assert!(
            text.contains("mul.rn.f32"),
            "expected the float body:\n{text}"
        );
    }

    #[test]
    fn canonical_loop_recognizer_records_unit_stride() {
        // White-box: the recognizer accepts the canonical loop and
        // records exit op `if_icmpge` (0xA2), stride +1, and start 0.
        let method = load_method("NonCanonicalLoops", "canonical", "([I[I[I)V");
        let code = method.code().expect("canonical has a Code attribute");
        let shape = super::loop_recog::detect_loop(&code.code, None)
            .expect("canonical loop must be recognized");
        match shape {
            super::loop_recog::LoopShape::Counted(li) => {
                assert_eq!(li.exit_op, 0xA2, "canonical exit op must be if_icmpge");
                assert_eq!(li.iv_stride, 1, "canonical stride must be +1");
                assert_eq!(li.iv_start, 0, "canonical start must be 0");
            }
            other => panic!("expected Counted loop, got {other:?}"),
        }
    }

    #[test]
    fn positive_start_loop_recognizer_records_start_value() {
        // White-box twin of `canonical_loop_recognizer_records_unit_stride`
        // for the `K = 5` case: the recognizer now accepts this shape
        // (previously rejected) and records `iv_start == 5`.
        let method = load_method("NonCanonicalLoops", "start5Loop", "([I)V");
        let code = method.code().expect("start5Loop has a Code attribute");
        let shape = super::loop_recog::detect_loop(&code.code, None)
            .expect("a non-negative constant start must now be recognized");
        match shape {
            super::loop_recog::LoopShape::Counted(li) => {
                assert_eq!(li.exit_op, 0xA2);
                assert_eq!(li.iv_stride, 1);
                assert_eq!(
                    li.iv_start, 5,
                    "start5Loop's start value must be recorded as 5"
                );
            }
            other => panic!("expected Counted loop, got {other:?}"),
        }
    }

    // ─────────────── 2-D nested counted loops ────────────────────────
    //
    // `EligibleNestedLoop.java` is the dedicated fixture for the
    // "analyzer/lowering coverage gaps" follow-up's "2-D / nested loops"
    // item: a strictly nested, rectangular `for (i...) for (j...)` loop
    // is now recognized and lowered to a flattened-thread-index kernel
    // (see `loop_recog::NestedLoop` and
    // `emit::Emitter::emit_nested_loop_guard_and_decompose`). Anything
    // that isn't exactly that shape (extra code in the outer body, a
    // non-rectangular/triangular inner bound) still rejects — same
    // "correctness over coverage" policy as every other loop-shape
    // guard in this module.

    #[test]
    fn nested_loop_lowers_to_real_ptx() {
        let m = lower_fixture("EligibleNestedLoop", "fill", "([I[I[I)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleNestedLoop__fill_"));
        // Flattened total = R * C, guarded, then decomposed.
        assert!(text.contains("mul.lo.s32"));
        assert!(text.contains("setp.ge.s32"));
        assert!(text.contains("L_done"));
        assert!(text.contains("div.s32"));
        assert!(text.contains("rem.s32"));
        // Both bounds are materialised from their respective `_len`
        // params (`rows` is p1, `cols` is p2).
        assert!(text.contains("[p1_len]"));
        assert!(text.contains("[p2_len]"));
        // The body still lowers normally: one store per thread.
        assert!(text.contains("st.global.s32"));
        assert!(text.contains("L_bounds_fail:"));
    }

    #[test]
    fn nested_loop_recognizer_records_both_levels() {
        // White-box: `detect_loop` returns `LoopShape::Nested` with two
        // independently-canonical `CountedLoop` records.
        let method = load_method("EligibleNestedLoop", "fill", "([I[I[I)V");
        let code = method.code().expect("fill has a Code attribute");
        let shape = super::loop_recog::detect_loop(&code.code, None)
            .expect("rectangular nested loop must be recognized");
        match shape {
            super::loop_recog::LoopShape::Nested(nl) => {
                assert_eq!(nl.outer.exit_op, 0xA2);
                assert_eq!(nl.outer.iv_stride, 1);
                assert_eq!(nl.outer.iv_start, 0);
                assert_eq!(nl.inner.exit_op, 0xA2);
                assert_eq!(nl.inner.iv_stride, 1);
                assert_eq!(nl.inner.iv_start, 0);
                assert_ne!(
                    nl.outer.iv_slot, nl.inner.iv_slot,
                    "outer and inner induction variables must be distinct locals"
                );
            }
            other => panic!("expected Nested loop, got {other:?}"),
        }
    }

    #[test]
    fn nested_loop_with_array_bound_lowers() {
        // `addRows`: exercises the ParamLen bound-resolution path for
        // both the outer (`rows`, p2) and inner (`cols`, p3) loop of a
        // nested loop with two distinct array sources.
        let m = lower_fixture("EligibleNestedLoop", "addRows", "([I[I[I[I)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleNestedLoop__addRows_"));
        assert!(text.contains("[p2_len]"));
        assert!(text.contains("[p3_len]"));
        assert!(text.contains("mul.lo.s32"));
        assert!(text.contains("div.s32"));
        assert!(text.contains("rem.s32"));
        assert!(text.contains("add.s32"));
    }

    #[test]
    fn nested_loop_with_trailing_outer_code_lowers_as_outer_parallel() {
        // The outer body is `{ inner loop; out[i] = i; }` — more than
        // just the inner loop, so the 2-D flattening (which never walks
        // that trailing statement) still refuses it. It is now picked up
        // by the outer-parallel classifier instead: one thread per `i`,
        // with the `j` loop emitted as a real per-thread PTX loop, and
        // the trailing store lowered like any other body code.
        //
        // Before the outer-parallel path existed this method was a
        // rejection; the reason it was rejected — "the 2-D lowering
        // drops the trailing code" — is exactly what the sequential
        // inner loop does not do.
        let m = lower_fixture("EligibleNestedLoop", "trailingCode", "([I[I[I)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleNestedLoop__trailingCode_"));
        // Not the flattened 2-D mapping: no `i = tid / C`, `j = tid % C`.
        assert!(
            !text.contains("rem.s32"),
            "outer-parallel lowering must not decompose a flattened index:\n{text}"
        );
        // A real inner loop: some label is the target of a backward
        // branch, and the trailing `out[i] = i` store survives.
        assert!(
            text.contains("bra L_body_"),
            "expected an inner-loop back-edge:\n{text}"
        );
        assert!(text.matches("st.global.s32").count() >= 2, "{text}");
    }

    #[test]
    fn triangular_nested_loop_is_rejected() {
        // `for (j = 0; j < i; j++)` — the inner bound is the outer
        // induction variable, not an array length. Both loops are
        // individually canonical, so recognition succeeds, but
        // `locate_bound` cannot prove the inner bound and lowering must
        // reject.
        let method = load_method("EligibleNestedLoop", "triangular", "([I[I)V");
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("expected triangular to be analyzer-eligible, got {v:?}"),
        };
        let err = lower_method("EligibleNestedLoop", &method, &sig, 7, 5)
            .expect_err("triangular (non-rectangular) nested loop must not lower");
        let msg = format!("{err}");
        assert!(
            msg.contains("bound is unknown") || msg.contains("not a recognized"),
            "expected an unresolvable-bound rejection, got: {msg}"
        );
    }

    // ───── Outer-parallel loops with a sequential inner loop ─────────
    //
    // `EligibleRowReduction.java` is the fixture for the shape a 2-D
    // flattening cannot express: `out[i] = sum_j w[i*n + j] * x[j]`.
    // The accumulator is loop-carried, so the `j` iterations must run in
    // order on ONE thread while `i` is still the parallel dimension.
    // `loop_recog::classify_outer_parallel_loop` recognizes it and
    // `Emitter::walk_cfg` lowers the interior back-edge as a real PTX
    // loop, with the inner header's canonical registers as its phis.

    #[test]
    fn row_reduction_lowers_with_a_sequential_inner_loop() {
        let m = lower_fixture("EligibleRowReduction", "matmul", "([F[F[F)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleRowReduction__matmul_"));
        // The inner loop header (`javap` pc 23) carries a label and is
        // the target of a backward branch — a real loop, not unrolled
        // and not flattened.
        assert!(text.contains("L_body_23:"), "{text}");
        assert!(text.contains("bra L_body_23;"), "{text}");
        // One thread per output row: no flattened-index decomposition.
        assert!(!text.contains("rem.s32"), "{text}");
        // The accumulator is an f32 phi: the back-edge writes the
        // register the header reads.
        assert!(text.contains("add.rn.f32"), "{text}");
        assert!(text.contains("mul.rn.f32"), "{text}");
        assert!(text.contains("st.global.f32"), "{text}");
        // Every array access inside the sequential loop keeps its bounds
        // check — the deopt contract does not weaken inside a body loop.
        assert!(text.contains("L_bounds_fail:"), "{text}");
    }

    /// Every kernel of the GPULlama3 inference path, lowered and run
    /// through `ptxas`. The application copy lives outside this repo
    /// (`apps/` is not tracked), so this fixture is its twin: a
    /// rejection here is a rejection there, found in a second instead
    /// of after a model load.
    #[test]
    fn llama_kernels_all_lower() {
        let hint = crate::annotations::AdmissionHint::AllowIntrinsicCalls;
        let cases: &[(&str, &str)] = &[
            ("transposeF16", "([III[I)V"),
            ("matmulSplit", "([I[FI[F)V"),
            ("reducePartials", "([FI[F)V"),
            ("embedT", "([III[F)V"),
            ("rmsScale", "([FF[F)V"),
            ("rmsApply", "([F[F[F[F)V"),
            ("rope", "([F[F[FII[F)V"),
            ("copyTo", "([FI[F)V"),
            ("attScores", "([F[FIIIIIF[F)V"),
            ("attWeighted", "([F[FIIIII[F)V"),
            ("addInto", "([F[F)V"),
            // `softmaxRows` and `siluMul` are deliberately absent: both
            // call `Math.exp`, which is admitted only under
            // CRATONVM_GPU_APPROX_MATH=1. Reading that variable here
            // would make the test depend on the environment it happens
            // to run in; `llama_exp_kernels_need_the_approx_switch`
            // asserts the gate itself instead.
        ];
        for (name, descriptor) in cases {
            let m =
                lower_fixture_with_pool_and_hint("EligibleLlamaKernels", name, descriptor, hint);
            let text = m.render();
            assert!(
                text.contains(&format!(".visible .entry EligibleLlamaKernels__{name}_")),
                "{name} did not lower to a named entry:
{text}"
            );
        }
    }

    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_llama_kernels() {
        let hint = crate::annotations::AdmissionHint::AllowIntrinsicCalls;
        let cases: &[(&str, &str)] = &[
            ("transposeF16", "([III[I)V"),
            ("matmulSplit", "([I[FI[F)V"),
            ("reducePartials", "([FI[F)V"),
            ("embedT", "([III[F)V"),
            ("rmsScale", "([FF[F)V"),
            ("rmsApply", "([F[F[F[F)V"),
            ("rope", "([F[F[FII[F)V"),
            ("copyTo", "([FI[F)V"),
            ("attScores", "([F[FIIIIIF[F)V"),
            ("attWeighted", "([F[FIIIII[F)V"),
            ("addInto", "([F[F)V"),
            // `softmaxRows` and `siluMul` are deliberately absent: both
            // call `Math.exp`, which is admitted only under
            // CRATONVM_GPU_APPROX_MATH=1. Reading that variable here
            // would make the test depend on the environment it happens
            // to run in; `llama_exp_kernels_need_the_approx_switch`
            // asserts the gate itself instead.
        ];
        for (name, descriptor) in cases {
            let m =
                lower_fixture_with_pool_and_hint("EligibleLlamaKernels", name, descriptor, hint);
            ptxas_round_trip(&m.render(), &format!("llama_{name}"));
        }
    }

    /// The two kernels that need `Math.exp` are refused unless the
    /// approximation is explicitly switched on, and admitted when it
    /// is. Which side this test asserts depends on the environment it
    /// runs in, so it asserts BOTH sides of the gate against whichever
    /// state that is — the property under test is the gate, not the
    /// setting.
    #[test]
    fn llama_exp_kernels_follow_the_approx_switch() {
        let on = std::env::var("CRATONVM_GPU_APPROX_MATH")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let hint = crate::annotations::AdmissionHint::AllowIntrinsicCalls;
        for (name, descriptor) in [("softmaxRows", "([FII[F)V"), ("siluMul", "([F[F[F)V")] {
            let (method, cp) =
                crate::analyzer::load_method_with_pool("EligibleLlamaKernels", name, descriptor);
            let annotations = crate::annotations::MethodAnnotations {
                gpu_kernel: Some(crate::annotations::GpuKernelAttrs {
                    admit: hint.into(),
                    ..crate::annotations::GpuKernelAttrs::default()
                }),
                ..crate::annotations::MethodAnnotations::default()
            };
            let verdict =
                crate::analyzer::analyze_with_annotations_and_pool(&method, &annotations, &cp);
            match (on, &verdict) {
                (true, OffloadVerdict::Eligible(_)) => {}
                (false, OffloadVerdict::Rejected(crate::analyzer::Reason::Invoke)) => {}
                _ => panic!("{name}: approx_math={on} but the analyzer said {verdict:?}"),
            }
        }
    }

    #[test]
    fn split_matmul_lowers_both_halves() {
        // The two kernels a decode step actually needs at scale: a
        // split-K matrix-vector product (one thread per (row, chunk)
        // pair, so the launch is `chunks` times wider than the row
        // count) and the per-row reduction of its partials. Both have a
        // sequential inner loop; the first also divides by a RUNTIME
        // scalar in both the pre-loop and the body, so it carries two
        // divisor-zero guards on top of the bounds checks.
        let m = lower_fixture_with_pool_and_hint(
            "EligibleSplitMatmul",
            "matmulColSplit",
            "([I[FI[F)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        let text = m.render();
        assert!(
            text.contains(".visible .entry EligibleSplitMatmul__matmulColSplit_"),
            "{text}"
        );
        assert!(text.contains("cvt.f32.f16"), "{text}");
        assert!(text.contains("bra L_body_"), "{text}");

        let m = lower_fixture("EligibleSplitMatmul", "reducePartials", "([FI[F)V");
        let text = m.render();
        assert!(
            text.contains(".visible .entry EligibleSplitMatmul__reducePartials_"),
            "{text}"
        );
        assert!(text.contains("add.rn.f32"), "{text}");
    }

    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_split_matmul() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleSplitMatmul",
            "matmulColSplit",
            "([I[FI[F)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        ptxas_round_trip(&m.render(), "split_matmul_col_split");
        let m = lower_fixture("EligibleSplitMatmul", "reducePartials", "([FI[F)V");
        ptxas_round_trip(&m.render(), "split_matmul_reduce");
    }

    #[test]
    fn row_reduction_with_int_accumulator_lowers() {
        let m = lower_fixture("EligibleRowReduction", "rowSums", "([I[I[I)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleRowReduction__rowSums_"));
        assert!(text.contains("bra L_body_"), "{text}");
        assert!(text.contains("add.s32"), "{text}");
        assert!(text.contains("st.global.s32"), "{text}");
    }

    #[test]
    fn outer_parallel_recognizer_reports_the_outer_loop() {
        // White-box: two back-edges, and the classifier returns the
        // OUTER one as a plain `Counted` loop. The inner edge is left
        // for `walk_cfg` — it is ordinary control flow from here on.
        let method = load_method("EligibleRowReduction", "matmul", "([F[F[F)V");
        let code = method.code().expect("matmul has a Code attribute");
        let shape = super::loop_recog::detect_loop(&code.code, None)
            .expect("outer-parallel loop must be recognized");
        match shape {
            super::loop_recog::LoopShape::Counted(li) => {
                assert_eq!(li.exit_op, 0xA2);
                assert_eq!(li.iv_stride, 1);
                assert_eq!(li.iv_start, 0);
                // The OUTER loop (`javap`: header 10, back-branch 63),
                // not the inner one at 23/51.
                assert_eq!(li.header_pc, 10);
                assert_eq!(li.back_branch_pc, 63);
            }
            other => panic!("expected the outer Counted loop, got {other:?}"),
        }
    }

    // ─── AUDIT 2026-07-11 (constant-start offset): EligibleOffsetLoop ─
    //
    // `EligibleOffsetLoop.java` is the dedicated end-to-end fixture for
    // `for (i = K; i < n; i++)` with `K > 0`: `offsetLoop` exercises the
    // small-constant (`iconst_4`) path, `offsetLoopLargeStart`
    // exercises the ldc-sourced (`K` outside `sipush` range) path.
    // `NegativeStartLoop.java` pins that `K < 0` is still rejected.

    #[test]
    fn offset_loop_lowers_with_folded_start_value() {
        // `for (i = 4; i < n; i++) out[i] = a[i] * 3 + 7;` — no ldc
        // involved (4, 3, and 7 are all small enough for
        // iconst/bipush), so the CP-free `lower_fixture` entry point
        // is enough.
        let m = lower_fixture("EligibleOffsetLoop", "offsetLoop", "([I[I)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleOffsetLoop__offsetLoop_"));
        // The tid+K fold: an `add.s32` with immediate 4 feeding a fresh
        // register. It no longer sits immediately after `emit_tid`'s
        // `mov.b32` reinterpret — the `tid_base` add that makes chunked
        // launches possible comes between — so this matches the fold
        // itself rather than its adjacency to the reinterpret.
        let folded = text
            .lines()
            .map(str::trim_start)
            .find(|l| l.starts_with("add.s32") && l.ends_with(", 4;"))
            .and_then(|l| l.split_whitespace().nth(1).map(|r| r.trim_end_matches(',')))
            .map(str::to_string)
            .unwrap_or_else(|| panic!("expected the tid+K fold:\n{text}"));
        // The loop guard must compare the folded register, not raw tid.
        assert!(
            text.lines().any(|l| {
                let l = l.trim_start();
                (l.starts_with("setp.ge.u32") || l.starts_with("setp.ge.s32"))
                    && l.contains(&format!("{folded},"))
            }),
            "expected the loop guard to compare the folded tid+K register:\n{text}"
        );
        // The body still lowers normally (mul + add, one load per
        // array, one store).
        assert!(text.contains("mul.lo.s32"));
        assert!(text.contains("add.s32"));
        assert!(text.matches("ld.global.s32").count() >= 1);
        assert!(text.contains("st.global.s32"));
    }

    #[test]
    fn offset_loop_recognizer_records_start_value() {
        // White-box: `iv_start == 4` for the small-constant case.
        let method = load_method("EligibleOffsetLoop", "offsetLoop", "([I[I)V");
        let code = method.code().expect("offsetLoop has a Code attribute");
        let shape = super::loop_recog::detect_loop(&code.code, None)
            .expect("K=4 must be recognized as a valid non-negative start");
        match shape {
            super::loop_recog::LoopShape::Counted(li) => {
                assert_eq!(li.exit_op, 0xA2);
                assert_eq!(li.iv_stride, 1);
                assert_eq!(li.iv_start, 4);
            }
            other => panic!("expected Counted loop, got {other:?}"),
        }
    }

    #[test]
    fn large_start_via_ldc_lowers_correctly() {
        // `for (i = 40000; i < n; i++) out[i] = a[i];` — 40000 is
        // outside sipush's +/-32767 range, so javac spills it to the
        // constant pool and emits `ldc`. Needs the pool-aware entry
        // point (mirrors every other `ldc`-involving fixture test in
        // this file).
        let m = lower_fixture_with_pool("EligibleOffsetLoop", "offsetLoopLargeStart", "([I[I)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleOffsetLoop__offsetLoopLargeStart_"));
        // Same deterministic tid+K fold as the small-constant case,
        // just with the ldc-resolved value 40000 instead of a literal
        // 4 — proves `resolve_start_value` actually reads the
        // constant-pool `Integer` entry rather than merely refusing to
        // reject the loop.
        assert!(
            text.lines().any(|l| {
                let l = l.trim_start();
                l.starts_with("add.s32") && l.ends_with(", 40000;")
            }),
            "expected the tid+K fold with the ldc-resolved start value:\n{text}"
        );
    }

    #[test]
    fn negative_start_loop_is_rejected_by_lowering() {
        // `for (i = -2; i < n; i++) a[i] = 0;` — `bipush -2; istore
        // iv`. Negative starts remain rejected (see
        // `loop_recog.rs`'s module doc comment for why: they would
        // need more kernel threads than the host's bound-sized launch
        // provides).
        let method = load_method("NegativeStartLoop", "negativeStart", "([I)V");
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!(
                "expected NegativeStartLoop.negativeStart to be analyzer-eligible, got {v:?}"
            ),
        };
        let err = lower_method("NegativeStartLoop", &method, &sig, 7, 5)
            .expect_err("a negative constant start must still be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("negative loop start value"),
            "expected negative-start rejection, got: {msg}",
        );
    }

    #[test]
    fn drem_is_rejected_by_analyzer() {
        let method = load_method("FloatRemainder", "dremScalar", "(DD)D");
        assert_eq!(
            analyze(&method),
            OffloadVerdict::Rejected(crate::analyzer::Reason::FloatRemainder),
            "drem must be rejected before lowering"
        );
    }

    // ─── AUDIT C31 follow-up (2026-07-11): ldc/ldc_w/ldc2_w lowering ──
    //
    // Mirrors the analyzer-side tests in `analyzer.rs`'s
    // `ldc_*_is_eligible_with_pool` group, one level further down the
    // pipeline: not just "is this admitted?" but "does it lower to the
    // exact PTX immediate the constant-pool entry holds?". Float/double
    // literals are asserted via the same exact-bit hex-literal encoding
    // `emit_op` already uses for `fconst_0..2`/`dconst_0..1`
    // (`0f<8 hex digits>` / `0d<16 hex digits>`) rather than a decimal
    // comparison, so the test can't pass on a value that merely *prints*
    // close to the literal but differs in its low bits.

    #[test]
    fn ldc_int_literal_lowers_to_immediate_moves() {
        let m = lower_fixture_with_pool("EligibleLdcInt", "scale", "([I[I)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleLdcInt__scale_"));
        // Both out-of-sipush-range constants (1_000_003 and 77_777)
        // must appear as plain `mov.s32` immediates.
        assert!(
            text.contains(", 1000003;"),
            "expected the 1_000_003 ldc immediate in:\n{text}"
        );
        assert!(
            text.contains(", 77777;"),
            "expected the 77_777 ldc immediate in:\n{text}"
        );
        assert!(text.contains("mul.lo.s32"));
        assert!(text.contains("add.s32"));
    }

    #[test]
    fn ldc2_w_long_literal_lowers_to_immediate_move() {
        let m = lower_fixture_with_pool("EligibleLdcLong", "mix", "([J[J)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleLdcLong__mix_"));
        assert!(
            text.contains(", 6364136223846793005;"),
            "expected the long-literal ldc2_w immediate in:\n{text}"
        );
        assert!(text.contains("mul.lo.s64"));
    }

    #[test]
    fn ldc_float_literal_lowers_to_hex_bit_immediate() {
        let m = lower_fixture_with_pool("EligibleLdcFloat", "fma", "([F[F)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleLdcFloat__fma_"));
        let lit1 = format!("0f{:08X}", 3.141_592_7_f32.to_bits());
        let lit2 = format!("0f{:08X}", 1.5f32.to_bits());
        assert!(
            text.contains(&lit1),
            "expected exact-bit float immediate {lit1} in:\n{text}"
        );
        assert!(
            text.contains(&lit2),
            "expected exact-bit float immediate {lit2} in:\n{text}"
        );
        assert!(text.contains("mul.rn.f32"));
        assert!(text.contains("add.rn.f32"));
    }

    #[test]
    fn ldc2_w_double_literal_lowers_to_hex_bit_immediate() {
        let m = lower_fixture_with_pool("EligibleLdcDouble", "fma", "([D[D)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleLdcDouble__fma_"));
        let lit = format!("0d{:016X}", 2.718281828459045f64.to_bits());
        assert!(
            text.contains(&lit),
            "expected exact-bit double immediate {lit} in:\n{text}"
        );
        assert!(text.contains("mul.rn.f64"));
    }

    /// The CP-free `lower_method` entry point must not silently accept
    /// `ldc` — it has no constant pool to resolve the target against.
    /// Build the `KernelSignature` through the pool-aware analyzer (this
    /// fixture is only eligible with a pool — see
    /// `analyzer::ldc_int_literal_still_rejected_without_pool`) and
    /// confirm the *old* lowering entry point refuses to emit PTX for
    /// it rather than, say, silently dropping the constant or panicking.
    #[test]
    fn lower_method_without_pool_rejects_ldc() {
        let (method, cp) =
            crate::analyzer::load_method_with_pool("EligibleLdcInt", "scale", "([I[I)V");
        let sig = match crate::analyzer::analyze_with_pool(&method, &cp) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("expected Eligible, got {v:?}"),
        };
        let err = lower_method("EligibleLdcInt", &method, &sig, 7, 5)
            .expect_err("lower_method with no constant pool must not lower ldc");
        let msg = format!("{err}");
        assert!(
            msg.contains("constant pool"),
            "expected a constant-pool-related error, got: {msg}"
        );
    }

    /// A `String` CP entry must never lower even if it somehow reached
    /// the emitter (defence in depth — the analyzer is the primary
    /// guard, see `analyzer::ldc_string_is_rejected_even_with_pool`).
    #[test]
    fn ldc_string_never_lowers_even_with_pool() {
        let (method, cp) = crate::analyzer::load_method_with_pool("RejectLdcString", "noop", "()V");
        // The analyzer must refuse this fixture outright.
        assert_eq!(
            crate::analyzer::analyze_with_pool(&method, &cp),
            OffloadVerdict::Rejected(crate::analyzer::Reason::LoadConstant)
        );
    }

    // ─── AUDIT 2026-07-11: EligibleFrem end-to-end lowering ──────────
    //
    // `EligibleFrem.frem([F[F)V` (`out[i] = in[i] % 3.7f` inside the
    // canonical loop) needs both loosenings the fixture's doc comment
    // describes: the pool-aware `ldc` resolution for the `3.7f`
    // literal (AUDIT C31 follow-up — `3.7f` has no `fconst` short
    // form) and `AdmissionHint::AllowDivByZero` for the `frem` opcode
    // itself. This is the only fixture in the suite that exercises
    // both loosenings stacked in the same method body.

    #[test]
    fn eligible_frem_lowers_end_to_end_with_pool_and_hint() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleFrem",
            "frem",
            "([F[F)V",
            crate::annotations::AdmissionHint::AllowDivByZero,
        );
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleFrem__frem_"));
        // The `3.7f` ldc literal lowers to the same exact-bit f32
        // hex-literal encoding as `fconst_0..2` and every other ldc
        // float fixture (see `ldc_float_literal_lowers_to_hex_bit_immediate`).
        let lit = format!("0f{:08X}", 3.7f32.to_bits());
        assert!(
            text.contains(&lit),
            "expected the 3.7f ldc immediate {lit} in:\n{text}"
        );
        // The frem div+truncate+fma sequence from `frem_f32`.
        assert!(text.contains("div.rn.f32"));
        assert!(text.contains("cvt.rzi.f32.f32"));
        assert!(text.contains("fma.rn.f32"));
        // The infinite-divisor correctness patch: the canonical
        // +Infinity bit pattern must appear as the `abs(divisor) ==
        // Infinity` comparison target (see `frem_f32`'s doc comment).
        assert!(
            text.contains("0f7F800000"),
            "expected the +Infinity bit-pattern comparison from the \
             infinite-divisor patch in:\n{text}"
        );
        assert!(text.contains("selp.f32"));
    }

    // ─── AUDIT 2026-07-11: lcmp/fcmp*/dcmp* real-world reality check ──
    //
    // `CompareBranchFusion.java` is real javac output for the idiomatic
    // 3-way `long`/`float`/`double` compare-and-branch pattern (`if (a <
    // b) return -1; if (a > b) return 1; return 0;`). Before this audit
    // every method here rejected at ANALYZE time with
    // `Reason::Compare`, as soon as the scanner reached the first
    // `lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg`. `classify`'s `0x94..=0x98`
    // arm now admits these opcodes unconditionally (they have a real,
    // bit-exact PTX lowering — see `lowering::emit::Emitter::lcmp`/
    // `cmp_f32`/`cmp_f64`), so each method is now analyzer-`Eligible`.
    //
    // That does NOT make the method offloadable: the very next opcode
    // after every `*cmp*` in this fixture is a single-operand `if<cond>`
    // that consumes the pushed value for a branch, and general `if*`
    // outside the canonical-loop guard position still unconditionally
    // rejects in `emit_op` (`lowering/emit.rs`'s `0x99..=0xA4` arm,
    // unchanged by this audit). So `lower_method` must still fail — just
    // with a precise "if-branch … outside canonical-loop guard position"
    // message instead of never reaching lowering at all. This pins
    // exactly that before/after transition, matching the analysis in
    // `analyzer::Reason::Compare`'s doc comment and the reality-check
    // note on the `0x94` dispatch arm in `emit_op`.

    fn expect_compare_fusion_eligible_but_not_lowerable(method_name: &str, descriptor: &str) {
        let method = load_method("CompareBranchFusion", method_name, descriptor);
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!(
                "expected CompareBranchFusion.{method_name} to be analyzer-Eligible \
                 now that lcmp/fcmp*/dcmp* are admitted, got {v:?}"
            ),
        };
        let err = lower_method("CompareBranchFusion", &method, &sig, 7, 5).expect_err(
            "a *cmp*-then-if fusion must still fail to lower — general if-branches \
             outside the canonical-loop guard are unchanged by this audit",
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("if-branch") && msg.contains("outside canonical-loop guard"),
            "expected the general if-branch rejection (unchanged), got: {msg}"
        );
    }

    #[test]
    fn compare_branch_fusion_long_is_eligible_then_rejected_at_lowering() {
        // Exercises `lcmp` (0x94).
        expect_compare_fusion_eligible_but_not_lowerable("compareLongs", "(JJ)I");
    }

    #[test]
    fn compare_branch_fusion_float_is_eligible_then_rejected_at_lowering() {
        // Exercises both `fcmpg` (for `<`) and `fcmpl` (for `>`) — javac
        // picks whichever variant makes a NaN operand evaluate the
        // source-level relational operator to `false` (JLS §15.20.1).
        expect_compare_fusion_eligible_but_not_lowerable("compareFloats", "(FF)I");
    }

    #[test]
    fn compare_branch_fusion_double_is_eligible_then_rejected_at_lowering() {
        // Exercises both `dcmpg` and `dcmpl`, the `double` twins.
        expect_compare_fusion_eligible_but_not_lowerable("compareDoubles", "(DD)I");
    }

    // ─── Loop-body control-flow graph lowering ─────────────────────────

    #[test]
    fn branching_loop_lowers_with_predicate_branches_and_local_merges() {
        let m = lower_fixture("EligibleBranchingLoop", "absOrIncrement", "([I[I)V");
        let text = m.render();
        assert!(
            text.contains("setp.ge.s32"),
            "expected an if predicate:\n{text}"
        );
        assert!(
            text.contains("bra L_body_"),
            "expected a real body label branch:\n{text}"
        );
        assert!(
            text.matches("L_body_").count() >= 2,
            "expected CFG labels:\n{text}"
        );

        // The two arms both assign `value`, so the join must reconcile
        // them. Pin the reconciliation SEMANTICALLY rather than by
        // instruction shape: whatever register the post-join array store
        // reads must be written somewhere in each arm.
        //
        // This used to assert on a `@%p mov` / `@!%p mov` pair, which was
        // the old join lowering's habit of minting a phi register for
        // every live slot and predicate-copying into it on both edges out
        // of the branch. Both edges out of one `if` carry identical
        // state, so those copies never did anything; `canonicalise_state`
        // now adopts the incoming registers instead and only the arm that
        // disagrees copies. Asserting the old shape would forbid the fix.
        let join_label = text
            .lines()
            .filter(|l| l.trim_start().starts_with("L_body_"))
            .next_back()
            .expect("a join label")
            .trim()
            .to_string();
        let (arms, join) = text
            .split_once(&join_label)
            .expect("join label splits the body");
        let merged = join
            .lines()
            .find_map(|l| {
                let t = l.trim_start();
                t.strip_prefix("st.global.s32 [")
                    .and_then(|rest| rest.split_once("], "))
                    .map(|(_, reg)| reg.trim_end_matches(';').to_string())
            })
            .expect("the post-join array store");
        let writes: Vec<&str> = arms
            .lines()
            .filter(|l| {
                l.trim_start()
                    .split_once(' ')
                    .is_some_and(|(_, ops)| ops.trim_start().starts_with(&format!("{merged},")))
            })
            .collect();
        assert!(
            writes.len() >= 2,
            "the merged register {merged} must be written on both arms before the \
             join, found {writes:?} in:\n{text}"
        );
    }

    /// The join lowering must not emit a copy per live slot per edge.
    ///
    /// `absOrIncrement` has exactly one local (`value`) that differs
    /// between the two arms; `i`, `n`, and both array references are the
    /// same register on both sides. A join reconciliation that copies
    /// everything scales with the number of live slots rather than with
    /// the number of slots that actually disagree — on the ray-tracer
    /// kernel that was 3299 `mov.f32` in a 4069-instruction kernel, 933
    /// of which survived into the SASS as `FSEL`.
    ///
    /// One merge move is the correct count here. The bound is deliberately
    /// tight: a regression to per-slot copying would blow straight past it.
    #[test]
    fn join_reconciliation_copies_only_the_slots_that_disagree() {
        let text = lower_fixture("EligibleBranchingLoop", "absOrIncrement", "([I[I)V").render();
        let merge_moves = text
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                // `mov.s32 %rN, %rM` between two registers. The prologue's
                // `mov.s32 %rN, 0` / `mov.s32 %rN, 1` constant loads are
                // not merges.
                t.starts_with("mov.s32 ")
                    && t.split(", ").nth(1).is_some_and(|src| src.starts_with('%'))
            })
            .count();
        assert!(
            merge_moves <= 2,
            "expected at most a couple of register-to-register merge moves for a \
             single disagreeing local, got {merge_moves}:\n{text}"
        );
        assert!(
            !text.contains("@%p0 mov") && !text.contains("@!%p0 mov"),
            "the two edges out of one `if` carry identical state; neither should \
             emit predicated state copies:\n{text}"
        );
    }

    #[test]
    fn one_arm_branch_can_fall_through_to_the_loop_back_edge() {
        let m = lower_fixture("EligibleBranchingLoop", "onlyNegatives", "([I[I)V");
        let text = m.render();
        assert!(
            text.contains("setp.ge.s32"),
            "expected branch predicate:\n{text}"
        );
        assert!(
            text.contains("L_body_"),
            "expected a branch target label:\n{text}"
        );
        assert!(!text.contains("non-canonical control flow"));
    }

    // ─── AUDIT 2026-07-11: invokestatic intrinsic-table lowering ─────
    //
    // `EligibleMathKernel.java` mirrors `EligibleFrem.java`'s pattern
    // exactly: no package, no `craton.gpu.*` import, eligibility gated
    // behind `AdmissionHint::AllowIntrinsicCalls` supplied by the Rust
    // test code via `lower_fixture_with_pool_and_hint` rather than a
    // real `@GpuKernel` annotation. See that fixture's file doc comment
    // for why (keeps these tests independent of the `craton-gpu`
    // annotation classpath being built in the sandbox).

    #[test]
    fn half_precision_matmul_lowers_to_cvt_f32_f16() {
        // `Float.float16ToFloat` inside a sequential inner loop: the
        // two features that let a half-precision weight tensor stay on
        // the device and still be read at full width in the kernel.
        let m = lower_fixture_with_pool_and_hint(
            "EligibleHalfPrecisionMatmul",
            "matmul",
            "([I[F[F)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleHalfPrecisionMatmul__matmul_"));
        // Two lanes per packed word, so two conversions per iteration.
        assert_eq!(
            text.matches("cvt.f32.f16").count(),
            2,
            "one conversion per packed half:\n{text}"
        );
        // The `.b16` scratch register must be declared, or ptxas
        // rejects the module.
        assert!(text.contains(".reg .b16 %rs<"), "{text}");
        // A real inner loop, and the outer loop still parallelised.
        assert!(text.contains("bra L_body_"), "{text}");
        assert!(!text.contains("rem.s32"), "{text}");
    }

    #[test]
    #[cfg_attr(
        not(feature = "gpu-it"),
        ignore = "requires NVIDIA CUDA toolkit (`ptxas`); enable feature `gpu-it` to run"
    )]
    fn ptxas_round_trip_half_precision_matmul() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleHalfPrecisionMatmul",
            "matmul",
            "([I[F[F)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        ptxas_round_trip(&m.render(), "half_precision_matmul");
    }

    #[test]
    fn sqrt_abs_fma_lowers_to_exact_ptx() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleMathKernel",
            "sqrtAbsFma",
            "([F[F[FFFF)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleMathKernel__sqrtAbsFma_"));
        // `(float) Math.sqrt((double) a[i])` is the only way to spell a
        // float square root in Java, and it collapses to ONE correctly-
        // rounded f32 sqrt: no widen, no f64 sqrt, no narrow. Bit-exact —
        // see `float_sqrt_triple_at` for the exhaustive check behind it.
        assert!(
            text.contains("sqrt.rn.f32"),
            "float sqrt triple should collapse to sqrt.rn.f32\n{text}"
        );
        assert!(
            !text.contains("sqrt.rn.f64"),
            "the f64 sqrt should be gone from a float-only kernel\n{text}"
        );
        assert!(
            !text.contains("cvt.f64.f32"),
            "the (double) widen should be gone\n{text}"
        );
        assert!(
            !text.contains("cvt.rn.f32.f64"),
            "the (float) narrow should be gone\n{text}"
        );
        // Math.abs(float) → plain sign-bit-clear.
        assert!(text.contains("abs.f32"), "missing float abs\n{text}");
        // Math.fma(float,float,float) → single-rounding fma.
        assert!(text.contains("fma.rn.f32"), "missing float fma\n{text}");
        assert!(text.contains("mul.rn.f32"));
        assert!(text.contains("add.rn.f32"));
        assert!(text.contains("st.global.f32"));
    }

    /// A genuine `double` square root must stay `sqrt.rn.f64`.
    ///
    /// The float-sqrt collapse keys on the exact `f2d; sqrt(D)D; d2f`
    /// triple. A kernel over `double[]` has no widen and no narrow, so
    /// nothing about it may change — the f64 result is what the program
    /// asked for and is observable.
    #[test]
    fn double_sqrt_still_lowers_to_f64() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleMathKernel",
            "sqrtDouble",
            "([D[D)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        let text = m.render();
        assert!(
            text.contains("sqrt.rn.f64"),
            "a double sqrt must stay f64\n{text}"
        );
        assert!(
            !text.contains("sqrt.rn.f32"),
            "a double sqrt must not be narrowed to f32\n{text}"
        );
    }

    /// A float widened to double whose sqrt result is KEPT as a double
    /// must stay `sqrt.rn.f64`.
    ///
    /// This is the case the collapse would silently corrupt if it matched
    /// on the call alone rather than on the whole triple: the bytecode is
    /// `f2d; invokestatic sqrt(D)D; dastore` — the widen is there, but no
    /// `d2f` follows, so the DOUBLE result reaches the array and rounding
    /// it through f32 would change the stored value.
    #[test]
    fn widened_float_sqrt_kept_as_double_does_not_collapse() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleMathKernel",
            "sqrtWidenedKept",
            "([F[D)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        let text = m.render();
        assert!(
            text.contains("cvt.f64.f32"),
            "the widen must survive when the double result is kept\n{text}"
        );
        assert!(
            text.contains("sqrt.rn.f64"),
            "sqrt must stay f64 when its result is stored as a double\n{text}"
        );
        assert!(
            !text.contains("sqrt.rn.f32"),
            "collapsing here would change the stored value\n{text}"
        );
        assert!(
            text.contains("st.global.f64"),
            "expected a double store\n{text}"
        );
    }

    #[test]
    fn abs_int_lowers_to_branchless_wraparound_sequence() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleMathKernel",
            "absInt",
            "([I[I)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        let text = m.render();
        assert!(
            text.contains("shr.s32"),
            "missing arithmetic-shift mask\n{text}"
        );
        assert!(text.contains("xor.b32"), "missing xor step\n{text}");
        assert!(text.contains("sub.s32"), "missing sub step\n{text}");
        // Deliberately must NOT rely on PTX's unverified abs.s32
        // mnemonic at the Integer.MIN_VALUE boundary — see
        // `lowering::emit::Emitter::abs_i32`'s doc comment.
        assert!(
            !text.contains("abs.s32"),
            "must use the explicit wraparound-safe sequence, not abs.s32\n{text}"
        );
    }

    #[test]
    fn abs_long_lowers_to_branchless_wraparound_sequence() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleMathKernel",
            "absLong",
            "([J[J)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        let text = m.render();
        assert!(text.contains("shr.s64"));
        assert!(text.contains("xor.b64"));
        assert!(text.contains("sub.s64"));
        assert!(
            !text.contains("abs.s64"),
            "must use the explicit wraparound-safe sequence, not abs.s64\n{text}"
        );
    }

    #[test]
    fn fma_double_lowers_to_single_rounding_fma() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleMathKernel",
            "fmaDouble",
            "([D[DDDD)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        let text = m.render();
        assert!(text.contains("fma.rn.f64"), "missing double fma\n{text}");
        assert!(text.contains("abs.f64"), "missing double abs\n{text}");
    }

    #[test]
    fn min_max_int_lowers_to_setp_selp() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleMathKernel",
            "minMaxInt",
            "([I[I[I)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        let text = m.render();
        assert!(
            text.contains("setp.lt.s32"),
            "missing min predicate\n{text}"
        );
        assert!(
            text.contains("setp.gt.s32"),
            "missing max predicate\n{text}"
        );
        assert!(
            text.matches("selp.s32").count() >= 2,
            "expected at least one selp.s32 per min/max\n{text}"
        );
    }

    #[test]
    fn min_max_long_lowers_to_setp_selp() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleMathKernel",
            "minMaxLong",
            "([J[J[J)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        let text = m.render();
        assert!(text.contains("setp.lt.s64"));
        assert!(text.contains("setp.gt.s64"));
        assert!(text.matches("selp.s64").count() >= 2);
    }

    #[test]
    fn min_max_float_lowers_with_nan_and_signed_zero_handling() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleMathKernel",
            "minMaxFloat",
            "([F[F[F)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        let text = m.render();
        assert!(
            text.contains("setp.nan.f32"),
            "missing NaN predicate\n{text}"
        );
        assert!(
            text.contains("or.b32"),
            "missing min's OR-of-raw-bits signed-zero handling\n{text}"
        );
        assert!(
            text.contains("and.b32"),
            "missing max's AND-of-raw-bits signed-zero handling\n{text}"
        );
        assert!(
            text.contains("setp.le.f32"),
            "missing min ordered compare\n{text}"
        );
        assert!(
            text.contains("setp.ge.f32"),
            "missing max ordered compare\n{text}"
        );
        // PTX's own min.f32/max.f32 do not implement Java's NaN/signed-zero
        // rules — pin that this lowering never regresses to using them.
        assert!(!text.contains("min.f32"));
        assert!(!text.contains("max.f32"));
    }

    #[test]
    fn min_max_double_lowers_with_nan_and_signed_zero_handling() {
        let m = lower_fixture_with_pool_and_hint(
            "EligibleMathKernel",
            "minMaxDouble",
            "([D[D[D)V",
            crate::annotations::AdmissionHint::AllowIntrinsicCalls,
        );
        let text = m.render();
        assert!(text.contains("setp.nan.f64"));
        assert!(text.contains("or.b64"));
        assert!(text.contains("and.b64"));
        assert!(!text.contains("min.f64"));
        assert!(!text.contains("max.f64"));
    }

    /// `Math.pow` is not in the curated intrinsic table (see
    /// `analyzer::resolve_math_intrinsic`'s doc comment), so
    /// `powRejected` never becomes analyzer-`Eligible` even under
    /// `AllowIntrinsicCalls` — see
    /// `analyzer::admit_intrinsic_calls_still_rejects_math_pow`. This
    /// test instead bypasses the analyzer, building a `KernelSignature`
    /// by hand exactly as if a future analyzer bug let the method
    /// through, to pin the EMITTER's own defence-in-depth rejection —
    /// mirrors `resolve_cp_entry`'s "should have been rejected
    /// upstream" arms for `ldc`/`ldc2_w`.
    #[test]
    fn invokestatic_of_non_table_method_is_rejected_defensively_at_lowering() {
        let (method, cp) =
            crate::analyzer::load_method_with_pool("EligibleMathKernel", "powRejected", "([D[D)V");
        let sig = KernelSignature {
            param_kinds: vec![ParamKind::F64Array, ParamKind::F64Array],
            return_kind: ParamKind::Void,
            estimated_work: 1 << 20,
            needs_d2h_sync: false,
            this_field_cps: vec![],
            writes_param_mask: 0,
            reads_param_mask: 0,
            work_bound: crate::emitter::WorkBound::Unknown,
            is_reduction: false,
            allow_div_by_zero: false,
        };
        let err = lower_method_with_pool("EligibleMathKernel", &method, &cp, &sig, 7, 5)
            .expect_err("Math.pow must not lower even if it somehow reached the emitter");
        let msg = format!("{err}");
        assert!(
            msg.contains("not a recognised GPU intrinsic"),
            "expected the defensive intrinsic-table rejection, got: {msg}"
        );
    }

    /// The CP-free `lower_method` entry point must not silently accept
    /// `invokestatic` — it has no constant pool to resolve the callee
    /// against. Mirrors `lower_method_without_pool_rejects_ldc`.
    #[test]
    fn lower_method_without_pool_rejects_invokestatic() {
        let (method, cp) = crate::analyzer::load_method_with_pool(
            "EligibleMathKernel",
            "sqrtAbsFma",
            "([F[F[FFFF)V",
        );
        let annotations = crate::annotations::MethodAnnotations {
            gpu_kernel: Some(crate::annotations::GpuKernelAttrs {
                admit: crate::annotations::AdmissionHint::AllowIntrinsicCalls.into(),
                ..crate::annotations::GpuKernelAttrs::default()
            }),
            gpu_exclude: None,
        };
        let sig =
            match crate::analyzer::analyze_with_annotations_and_pool(&method, &annotations, &cp) {
                OffloadVerdict::Eligible(s) => s,
                v => panic!("expected Eligible, got {v:?}"),
            };
        let err = lower_method("EligibleMathKernel", &method, &sig, 7, 5)
            .expect_err("lower_method with no constant pool must not lower invokestatic");
        let msg = format!("{err}");
        assert!(
            msg.contains("constant pool"),
            "expected a constant-pool-related error, got: {msg}"
        );
    }
}
