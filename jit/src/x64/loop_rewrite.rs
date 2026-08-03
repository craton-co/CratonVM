// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Bytecode loop-rewriter planning and side-table replication.
//!
//! Two unrollers exist in this backend and exactly one of them is live. The
//! native byte-copy unroller in the `goto` arm of the bytecode walk is admitted
//! by `plan_native_unroll`. The bytecode rewriter planned here is opt-in per
//! compiler thread, off by default process-wide, and nothing in the VM arms it
//! except `set_bytecode_loop_rewriter_armed`.
//!
//! `LoopRewriteRefusal` is why this planning is worth reading as a unit: every
//! one of its variants means "compile the original bytecode", which is the
//! pre-existing behaviour and always correct. The transform is an optimisation,
//! so a refusal costs speed and nothing else — and the side-table replication
//! below (`replicate_pc3` / `replicate_pc5`) is the part that would not be
//! harmless if it went wrong, because a re-keying miss produces metadata
//! pointing at the wrong copy of a duplicated loop body.

use super::*;

thread_local! {
    /// Opt-in for the bytecode loop rewriter, per compiler thread.
    ///
    /// **Off by default, process-wide.** Nothing in the VM arms it; the only
    /// way in is [`set_bytecode_loop_rewriter_armed`]. That makes the default
    /// compile path byte-identical to before this wiring landed (one
    /// thread-local `Cell<bool>` load per compile), and it keeps the opt-in
    /// out of the declared-flag table, which lives in a crate this backend
    /// does not own.
    ///
    /// Thread-local rather than a process-wide `AtomicBool` for two reasons:
    /// the JIT compiles on worker threads, so arming is a per-worker decision
    /// a validation harness can make one thread at a time; and the unit tests
    /// in this file run concurrently in one process, where a global switch
    /// would leak one test's transform into another's compile.
    static BYTECODE_LOOP_REWRITER_ARMED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Arm (or disarm) the bytecode loop rewriter for every subsequent compile on
/// THIS thread, returning the previous setting.
///
/// See [`bytecode_loop_xform_rewrites_bytecode`] for what it switches on and
/// `docs/jit/loop-rewriter-wiring.md` for what is and is not validated.
/// Callers that arm it must disarm it again (the tests below use a guard), or
/// every later compile on the same thread inherits it.
pub fn set_bytecode_loop_rewriter_armed(on: bool) -> bool {
    BYTECODE_LOOP_REWRITER_ARMED.with(|c| c.replace(on))
}

/// Does the bytecode-level loop transform (`plan_loop_peel` /
/// `plan_loop_unroll` in `x64::licm`) rewrite the bytecode the emitter
/// compiles?
///
/// `false` unless [`set_bytecode_loop_rewriter_armed`] armed this thread.
///
/// When it is `false`, `compile_with_param_slots` compiles the caller's
/// original bytecode verbatim, so every pc the emitter handles is an
/// interpreter bci, every caller-supplied side table (`field_info`,
/// `invoke_info`, `mic_slots`, `inline_sites`, …) is still keyed correctly,
/// and every deopt bci, oop-map `bytecode_pc`, `pc_to_native` index and
/// `osr_entry_native` index is already in interpreter-bci space with nothing
/// to translate.
///
/// When it is `true`, `compile_with_param_slots` asks
/// `plan_bytecode_loop_xform` for a transform. If it gets one it compiles the
/// REWRITTEN bytes, replicates every pc-keyed side table into output-PC space
/// through `LoopXform::replicate_pc_keyed`, and translates the three
/// bci-valued immediates the emitter bakes into machine code back through
/// `Compiler::orig_bci`. If it does not get one — the common case, because
/// the admission test is deliberately narrow — the compile is byte-identical
/// to the unarmed one except that the native unroller is off.
///
/// This is deliberately the same switch that turns the native byte-copy
/// unroller off (see `native_unroller_enabled`): the two must never both fire
/// on one loop, or `k+1` bytecode copies get machine-code-duplicated `k+1`
/// more times behind one back edge, giving `(k+1)^2` bodies per poll — a
/// time-to-safepoint neither unroller's budget check ever saw.
pub(super) fn bytecode_loop_xform_rewrites_bytecode() -> bool {
    BYTECODE_LOOP_REWRITER_ARMED.with(|c| c.get())
}

/// Is the native byte-copy unroller (the `0xa7` arm of `compile_bytecode`)
/// the current owner of loop unrolling?
///
/// Mutually exclusive with `bytecode_loop_xform_rewrites_bytecode` by
/// construction — see that function for why the exclusion is a correctness
/// requirement and not a tidiness one.
pub(super) fn native_unroller_enabled() -> bool {
    !bytecode_loop_xform_rewrites_bytecode()
        && cratonvm_types::flags::runtime_var_os("CRATONVM_DISABLE_UNROLL").is_none()
}

/// Structural admission test for the native byte-copy loop unroller.
///
/// The unroller duplicates emitted MACHINE CODE between `pc_to_native[header]`
/// and the back edge, shifting eight patch vectors, re-resolving helper
/// `rel32`s and minting fresh IC slots per copy. Whether that is legal is a
/// question about the loop's CONTROL FLOW, and the test that used to guard it
/// — `code[back_edge] == 0xa7` plus a body-byte-size band — asked no
/// control-flow question at all. In particular it never established that:
///
/// * **the loop is reducible** (the header dominates its own region). In an
///   irreducible loop "the bytes from the header to the back edge" are not one
///   iteration of anything, so duplicating them duplicates the wrong region.
/// * **the region has a single entry.** A branch from outside that lands
///   *below* the header enters copy 0 mid-body; the `k` copies that follow it
///   are then whole extra bodies the original would not have run before its
///   next exit test.
/// * **no cycle strictly inside the body is irreducible.** The duplicator
///   resolves an internal forward patch to `pc_to_native[target] + shift`,
///   which is only the copy's own image of the target when the inner cycle is
///   reducible and wholly contained in the body span.
/// * **nothing branches to the back-edge instruction itself.** That
///   instruction exists in the last copy only, so such an edge would target
///   code that is no longer where the branch thinks it is.
/// * **no exception handler lands inside the duplicated region, and no
///   protected range only partially overlaps it.** The bytecode→native handler
///   ranges are derived from `pc_to_native`, which covers copy 0 only, so a
///   throw from copy `1..k` is not covered by the range that protects the
///   loop — the exception escapes a `try` that lexically encloses it.
/// * **the header is not in `bypassable_headers`.** Every other speculating
///   transform in this pipeline — the aaload LICM hoists, the arith LICM
///   hoists, the FP hoists, matrix-dot, the bulk-byte loops — filters on that
///   set, and the unroller sits on the same pre-header: `pc_to_native[header]`
///   points PAST it, so `body_start` excludes it and each copy runs without
///   it, while an external edge into the header bypasses it entirely.
/// * **the poll-free span stays bounded.** `k+1` bodies now sit behind one
///   back-edge poll.
///
/// Rather than re-derive those facts here, this asks the bytecode-level
/// rewriter [`plan_loop_unroll`], whose admission test is exactly that list,
/// proved with real dominators over an instruction-granularity CFG
/// (`MethodCfg`) instead of the "any backward branch is a loop" heuristic used
/// elsewhere in this backend, and re-checked on its own output. The rewritten
/// bytes are discarded — only the verdict is used, so the emitter still sees
/// the caller's bytecode and nothing needs re-keying. See
/// `docs/jit/loop-transforms.md`.
///
/// Fail-closed: every refusal, including a malformed-input `BadShape`, means
/// the loop is compiled without unrolling.
pub(super) fn plan_native_unroll(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
    extra_copies: usize,
    exception_ranges: &[(usize, usize, usize)],
    bypassable_headers: &FxHashSet<usize>,
) -> Option<(usize, usize, usize)> {
    let dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_GEN").is_some();
    if bypassable_headers.contains(&header) {
        if dbg {
            eprintln!(
                "[JIT_GEN] unroll refused: header={header} back_edge={back_edge} \
                 reason=BypassableHeader"
            );
        }
        return None;
    }
    match plan_loop_unroll(
        code,
        code_len,
        header,
        back_edge,
        extra_copies,
        exception_ranges,
    ) {
        Ok(_) => Some((header, back_edge, extra_copies)),
        Err(refusal) => {
            if dbg {
                eprintln!(
                    "[JIT_GEN] unroll refused: header={header} back_edge={back_edge} \
                     copies={extra_copies} reason={refusal:?}"
                );
            }
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Bytecode loop rewriter: planning and side-table replication
// ---------------------------------------------------------------------------

/// Why `compile_with_param_slots` did not compile rewritten bytecode.
///
/// Every variant means "compile the caller's original bytecode", which is the
/// pre-existing behaviour and always correct — the transform is an
/// optimisation, so refusing it costs speed and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopRewriteRefusal {
    /// The default. This thread never called
    /// [`set_bytecode_loop_rewriter_armed`].
    NotArmed,
    /// `CRATONVM_DEOPT_REAL`. The precise-deopt snapshots record
    /// `DeoptimizationPoint::bci`, which the VM *resumes at*, and they are
    /// built deep inside the emitter from the emitter's own pc. Translating
    /// them is a separate piece of work; until it lands, refuse.
    DeoptRealEnabled,
    /// Precise exceptional frames. Same reason: `emit_post_invoke_exception_check`
    /// and `emit_precise_null_check_field_store` record a resume-bearing
    /// snapshot keyed on the emitter pc.
    PreciseExceptionFrames,
    /// The method has an `invokedynamic`. Its lowering is an unconditional
    /// trap that records an `UnreachedCode` snapshot through
    /// `emit_osr_exit_map_at_reason` — and unlike the two above, that path is
    /// NOT gated on `deopt_real_enabled()`, so it would fire in production.
    InvokedynamicPresent,
    /// The method has inline sites. An inlined callee contributes its own bci
    /// space (`docs/jit/deopt-inline-scopes.md`) that this wiring's
    /// caller-only provenance map does not describe.
    InlineSitesPresent,
    /// No loop in the method passed the profitability band and the structural
    /// admission test.
    NoCandidateLoop,
    /// The planner produced a transform whose provenance is not total. Cannot
    /// happen (`rewrite_loop_copies` builds `bci_of` byte by byte and checks
    /// its length), and is re-checked here because `Compiler::orig_bci`'s
    /// soundness rests on it.
    ProvenanceNotTotal,
    /// The rewriter itself refused the last candidate loop.
    Planner(LoopXformRefusal),
}

/// The properties of a pending compile that decide whether the bytecode loop
/// rewriter may run at all, independent of any particular loop.
pub(crate) struct LoopRewriteShape {
    pub(crate) deopt_real: bool,
    pub(crate) precise_exception_frames: bool,
    pub(crate) has_indy: bool,
    pub(crate) has_inline_sites: bool,
}

/// Choose ONE loop to unroll at the bytecode level and rewrite the method.
///
/// The profitability band is deliberately character-for-character the native
/// unroller's (see the `unroll_loops` construction in
/// `compile_with_param_slots`), so arming the rewriter changes *which
/// machinery* unrolls a loop, not *which loops* are considered. The
/// legality question is `plan_loop_unroll`'s, exactly as it already is for
/// the native unroller via `plan_native_unroll`.
///
/// Exactly one loop, because a `LoopXform` describes one rewrite: composing
/// two would mean composing their provenance maps, which the primitives in
/// `x64::licm` do not do. The first admitted loop in `detect_loops` order
/// wins, and that order is innermost-first for a nest, which is the
/// profitable choice. Every other loop in the method is left alone — the
/// rewriter shifts it correctly, it just does not duplicate it.
pub(super) fn plan_bytecode_loop_xform(
    code: &[u8],
    code_len: usize,
    exception_ranges: &[(usize, usize, usize)],
    loop_unroll_hints: &HashMap<usize, usize>,
    shape: LoopRewriteShape,
) -> Result<LoopXform, LoopRewriteRefusal> {
    use LoopRewriteRefusal as R;

    if !bytecode_loop_xform_rewrites_bytecode() {
        return Err(R::NotArmed);
    }
    // Whole-compile refusals, cheapest first. Each names a construct that
    // publishes an emitter pc to the VM as a resume bci through a path this
    // wiring does not translate; see the variant docs.
    if shape.deopt_real {
        return Err(R::DeoptRealEnabled);
    }
    if shape.precise_exception_frames {
        return Err(R::PreciseExceptionFrames);
    }
    if shape.has_indy {
        return Err(R::InvokedynamicPresent);
    }
    if shape.has_inline_sites {
        return Err(R::InlineSitesPresent);
    }

    let loops = detect_loops(code, code_len);
    let bypassable = find_bypassable_loop_headers(code, code_len, &loops, exception_ranges);
    let mut last_refusal: Option<LoopXformRefusal> = None;
    for &(header, back_edge) in &loops {
        if back_edge >= code_len || code[back_edge] != 0xa7 {
            continue;
        }
        // Same pre-header placement contract the native unroller, the LICM
        // hoists, matrix-dot and the bulk-byte loops all apply.
        if bypassable.contains(&header) {
            continue;
        }
        let body_size = back_edge - header;
        if body_size < 5 {
            continue;
        }
        // PGO path: a profiled trip count extends eligibility to larger
        // bodies. A refusal here does NOT fall through to the static
        // heuristic — same as the native unroller's `return`.
        if let Some(&pgo_factor) = loop_unroll_hints.get(&back_edge) {
            if body_size <= 50 || (body_size <= 100 && pgo_factor <= 2) {
                return match plan_loop_unroll(
                    code,
                    code_len,
                    header,
                    back_edge,
                    pgo_factor.saturating_sub(1),
                    exception_ranges,
                ) {
                    Ok(x) if !x.provenance_is_total() => Err(R::ProvenanceNotTotal),
                    Ok(x) => Ok(x),
                    Err(e) => Err(R::Planner(e)),
                };
            }
        }
        let extra_copies = if body_size <= 20 {
            3 // 4x unroll
        } else if body_size <= 50 {
            1 // 2x unroll
        } else {
            continue;
        };
        match plan_loop_unroll(
            code,
            code_len,
            header,
            back_edge,
            extra_copies,
            exception_ranges,
        ) {
            Ok(x) if !x.provenance_is_total() => return Err(R::ProvenanceNotTotal),
            Ok(x) => return Ok(x),
            Err(e) => last_refusal = Some(e),
        }
    }
    Err(last_refusal.map(R::Planner).unwrap_or(R::NoCandidateLoop))
}

/// [`LoopXform::replicate_pc_keyed`] for a table whose entry is a 3-tuple
/// `(pc, a, b)`.
///
/// The primitive is defined over `(usize, T)`; these two adapters pack the
/// payload into a tuple and unpack it again so that EVERY pc-keyed table goes
/// through the one replication function. That is the point: replicating some
/// tables and not others compiles fine and silently produces a loop copy
/// missing a field resolution or an inline cache.
pub(super) fn replicate_pc3<A: Clone, B: Clone>(x: &LoopXform, t: Vec<(usize, A, B)>) -> Vec<(usize, A, B)> {
    let packed: Vec<(usize, (A, B))> = t.into_iter().map(|(pc, a, b)| (pc, (a, b))).collect();
    x.replicate_pc_keyed(&packed)
        .into_iter()
        .map(|(pc, (a, b))| (pc, a, b))
        .collect()
}

/// [`replicate_pc3`] for a 5-tuple `(pc, a, b, c, d)`.
pub(super) fn replicate_pc5<A: Clone, B: Clone, C: Clone, D: Clone>(
    x: &LoopXform,
    t: Vec<(usize, A, B, C, D)>,
) -> Vec<(usize, A, B, C, D)> {
    let packed: Vec<(usize, (A, B, C, D))> = t
        .into_iter()
        .map(|(pc, a, b, c, d)| (pc, (a, b, c, d)))
        .collect();
    x.replicate_pc_keyed(&packed)
        .into_iter()
        .map(|(pc, (a, b, c, d))| (pc, a, b, c, d))
        .collect()
}
