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
    BYTECODE_LOOP_REWRITER_ARMED.with(|c| c.get()) || bytecode_loop_xform_flag()
}

/// `CRATONVM_JIT=bytecode-loop-xform` — the process-wide form of the opt-in.
///
/// Read **once**. `runtime_var_os` on a declared flag consults a latched
/// snapshot anyway, so a per-compile read would buy nothing and cost a lookup;
/// the `OnceLock` makes that explicit and leaves one relaxed load on the path.
///
/// This is the switch that lets the transforms be executed by something larger
/// than a unit test, and since the bci translation landed it is sufficient on
/// its own: `CRATONVM_JIT=bytecode-loop-xform` alone now reaches loops under
/// the DEFAULT `deopt_real` configuration. It used to additionally need
/// `-deopt-real`, because `plan_bytecode_loop_xform` refused every compile
/// before it looked at a loop; that refusal is gone, and pairing the two flags
/// now measures the deopt-real-off configuration rather than the transform.
pub(super) fn bytecode_loop_xform_flag() -> bool {
    static ARMED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ARMED.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_BYTECODE_LOOP_XFORM")
    })
}

/// Is the native byte-copy unroller (the `0xa7` arm of `compile_bytecode`)
/// the current owner of loop unrolling?
///
/// Mutually exclusive with `bytecode_loop_xform_rewrites_bytecode` by
/// construction — see that function for why the exclusion is a correctness
/// requirement and not a tidiness one.
pub(super) fn native_unroller_enabled() -> bool {
    !bytecode_loop_xform_rewrites_bytecode()
        && !cratonvm_types::flags::runtime_flag_on("CRATONVM_DISABLE_UNROLL")
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
    let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN");
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
    /// The method has inline sites. An inlined callee contributes its own bci
    /// space (`docs/jit/deopt-inline-scopes.md`) that this wiring's
    /// caller-only provenance map does not describe.
    ///
    /// The last of the four whole-compile refusals this lane started with, and
    /// the only one a bci translation cannot answer. `DeoptRealEnabled`,
    /// `PreciseExceptionFrames` and `InvokedynamicPresent` all named the same
    /// thing — a path that publishes an emitter pc to the VM as a resume bci —
    /// and all three were retired together when `build_and_record_deopt_point`
    /// started publishing `DeoptimizationPoint::bci` through
    /// [`super::Compiler::orig_bci`]. An inlined callee is different in kind:
    /// there is no bci in THIS method's space to translate to.
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

/// The properties of a pending compile the planner records, and — for the last
/// field — refuses on.
///
/// The first three were refusals until the bci translation landed. They are
/// still carried and still counted, because the tally they feed is what says
/// how much that translation bought and would say immediately if a future
/// change put one of them back in the way. Deleting a field here deletes a row
/// from `metrics::LOOP_XFORM_EVENTS`, not just a parameter.
pub(crate) struct LoopRewriteShape {
    /// `CRATONVM_DEOPT_REAL`, process-wide and default-ON. Counted only.
    pub(crate) deopt_real: bool,
    /// This method's handlers need precise exceptional frames. Counted only.
    pub(crate) precise_exception_frames: bool,
    /// This method contains an `invokedynamic`. Counted only.
    pub(crate) has_indy: bool,
    /// This method has inlined callees — the one condition still refused.
    pub(crate) has_inline_sites: bool,
}

/// Number of iterations the peel arm takes off a loop.
///
/// One. Peeling's payoff here is not the peeled iteration itself — it is that
/// after peel(k) the steady-state copy is reachable only by fall-through from
/// the copy above it and by its own back edge, so a header an external branch
/// could enter is no longer *bypassable* and the transforms that filter on
/// `find_bypassable_loop_headers` (the aaload and arith LICM hoists, the FP
/// hoists, the speculative-BCE guards, matrix-dot, the bulk-byte loops) can
/// keep the pre-header they had to drop. One copy is enough for that; every
/// further copy is code growth with no additional claim behind it.
pub(super) const LOOP_PEEL_COPIES: usize = 1;
/// Largest body the peel arm will duplicate. The same ceiling the unroll band
/// applies to its 2x arm — peeling one copy costs exactly what unrolling one
/// extra copy costs, so it is bounded by the same number rather than a new one.
const LOOP_PEEL_MAX_BODY_BYTES: usize = 50;

/// The `trip >= minimum` pre-header check for this loop, when one can be proved
/// and there is anything left to check.
///
/// `None` — meaning "do not version" — in three different situations, and the
/// caller treats all three the same way (emit the plain transform):
///
///  * the loop is not counted in a shape `scev` recognises;
///  * its exit test is not the first thing the header does, so the
///    `LoopForm::PreTested` claim `prove_trip_count_at_least` needs would be a
///    guess. `PostTested` is the conservative spelling and that proof refuses it
///    outright, so this returns `None` rather than assert a form it cannot
///    justify — see `loop_analysis::analyze_counted_loop_at`;
///  * the minimum is already a compile-time fact (`TripCountProof::Static`), in
///    which case a runtime compare would test something already known.
///
/// The guard is a PROFITABILITY filter, not a legality one: peel and unroll are
/// correct at every trip count, because every copy keeps the body's own exit
/// branches. What versioning buys is that the duplicated copies are only
/// reached when the loop actually runs often enough to use them — the common
/// `for (i = 0; i < n; i++)` has a compile-time `trip.min` of zero, so without
/// this the planner duplicates a body that may never execute — and that a loop
/// which runs fewer times takes an *untouched* copy of itself instead of
/// entering a 4x-unrolled body. It costs one body of code (the fallback).
fn trip_count_guard(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
    minimum: usize,
) -> Option<crate::scev::PreheaderGuard> {
    // There is no constant pool here, so a `Math.min`/`Math.max` limit is
    // simply not recognised. That costs a guard, never soundness.
    let (counted, test_pc) = crate::loop_analysis::analyze_counted_loop_at(
        code,
        code_len,
        header,
        back_edge,
        crate::scev::LoopForm::PreTested,
        &|_| None,
    )?;
    if test_pc != header {
        return None;
    }
    // Widening: usize minimum to u64.
    match counted.prove_trip_count_at_least(minimum as u64, &crate::scev::RangeEnv::new()) {
        // Exactly one guard, or none: `plan_loop_version` emits ONE pre-header
        // check, and a partially-discharged obligation proves nothing (the rule
        // `docs/jit/trip-count-guards.md` states for every consumer).
        crate::scev::TripCountProof::Guarded(gs) => match gs.as_slice() {
            [g] => Some(g.clone()),
            _ => None,
        },
        crate::scev::TripCountProof::Static | crate::scev::TripCountProof::Refused(_) => None,
    }
}

/// Duplicate `extra` extra bodies with `kind`'s transform, versioned against a
/// trip-count guard when [`trip_count_guard`] can produce one.
///
/// A versioning refusal is not this method's refusal: the guard is not what
/// makes peel or unroll legal, so an unencodable or compile-time-decided guard
/// falls back to the plain transform, which is exactly what the planner emitted
/// before versioning existed.
fn plan_versioned(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
    extra: usize,
    exception_ranges: &[(usize, usize, usize)],
    kind: LoopXformKind,
) -> Result<LoopXform, LoopXformRefusal> {
    if let Some(guard) = trip_count_guard(code, code_len, header, back_edge, extra + 1) {
        match plan_loop_version(
            code,
            code_len,
            header,
            back_edge,
            extra,
            exception_ranges,
            kind,
            &guard,
        ) {
            Ok(x) => return Ok(x),
            Err(refusal) => {
                if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN") {
                    eprintln!(
                        "[JIT_GEN] loop versioning refused: header={header} \
                         back_edge={back_edge} copies={extra} reason={refusal:?} \
                         — falling back to the unguarded transform"
                    );
                }
            }
        }
    }
    match kind {
        LoopXformKind::Peel => {
            plan_loop_peel(code, code_len, header, back_edge, extra, exception_ranges)
        }
        LoopXformKind::Unroll => {
            plan_loop_unroll(code, code_len, header, back_edge, extra, exception_ranges)
        }
    }
}

/// Choose ONE loop to transform at the bytecode level and rewrite the method.
///
/// Three arms, in the order they are tried:
///
///  * **Peel**, for a loop whose header is *bypassable* — one an external
///    branch can enter. Every speculating transform in this backend drops its
///    pre-header when it sees one, and the planner used to skip such a loop
///    outright. Peeling moves the steady-state loop past that edge (the
///    external branch still lands on copy 0, which flows into the copy below
///    it), so the loop the emitter finds in the rewritten bytecode has a single
///    entry and keeps its hoists. See [`LOOP_PEEL_COPIES`].
///  * **Unroll, PGO factor**, when a profiled trip count is available.
///  * **Unroll, static heuristic**, the band below.
///
/// The unroll band is deliberately character-for-character the native
/// unroller's (see the `unroll_loops` construction in
/// `compile_with_param_slots`), so arming the rewriter changes *which
/// machinery* unrolls a loop, not *which loops* are considered. The legality
/// question is `plan_loop_unroll`'s, exactly as it already is for the native
/// unroller via `plan_native_unroll`.
///
/// Every arm goes through [`plan_versioned`], so a duplicating transform is
/// emitted behind a `trip >= copies + 1` pre-header check whenever one can be
/// proved and encoded, with an untouched copy of the loop on the failing edge.
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

    // ── Tally, before anything can short-circuit ──────────────────────
    //
    // Every one of the four conditions is recorded on every compile it holds
    // for, INDEPENDENTLY of whether one of them refuses. That independence is
    // what retired three of them: counting "which refusal fired" would have
    // reported `deopt_real` for 100% of compiles — it is default-ON and
    // process-wide — and would have hidden the other three permanently. The
    // four counts overlap and must not be summed; `metrics::LOOP_XFORM_EVENTS`
    // says so where a reader will find it.
    //
    // Three of them no longer refuse. They are still counted because they are
    // the measurement that says so, and because a future emit path that
    // reintroduces an untranslated resume bci would show up as
    // `loop_xform_deopt_bci_unpublishable` against these denominators rather
    // than as a mystery.
    //
    // Cost on the default path: one relaxed increment per condition that
    // holds, per compile. Compiles are thousands per run, not millions.
    use crate::metrics::record_loop_xform_event as tally;
    tally("loop_xform_compiles");
    if shape.deopt_real {
        tally("loop_xform_deopt_real");
    }
    if shape.precise_exception_frames {
        tally("loop_xform_precise_exception_frames");
    }
    if shape.has_indy {
        tally("loop_xform_invokedynamic");
    }
    if shape.has_inline_sites {
        tally("loop_xform_inline_sites");
    }
    // "No whole-compile refusal holds", which is the population a narrowing
    // effort is trying to grow — not "none of the four conditions holds", which
    // stopped being the same question when three of them stopped refusing.
    let eligible = !shape.has_inline_sites;
    if eligible {
        tally("loop_xform_eligible");
    }

    if !bytecode_loop_xform_rewrites_bytecode() {
        tally("loop_xform_not_armed");
        return Err(R::NotArmed);
    }
    // The one whole-compile refusal left.
    //
    // `deopt_real`, precise exception frames and `invokedynamic` used to refuse
    // here, and all three named one thing: a path that hands the VM an emitter
    // pc as a resume bci. `build_and_record_deopt_point` now publishes
    // `DeoptimizationPoint::bci` and `FrameState::bci` through
    // `Compiler::orig_bci`, `pc_is_protected` asks its question in interpreter
    // space, and `compile_with_param_slots` re-derives and CHECKS the whole
    // translation before publishing the artifact — so all three are answered
    // rather than avoided.
    //
    // Inlining is not answered by that. An inlined callee's snapshot bcis live
    // in the CALLEE's bci space, and this provenance map describes the caller's
    // rewritten bytes only: there is nothing in this method to translate them
    // to. Retiring it means describing inline scopes
    // (`docs/jit/deopt-inline-scopes.md`), not relaxing a check.
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
        let body_size = back_edge - header;
        if body_size < 5 {
            continue;
        }
        // The peel arm. A bypassable header is the pre-header placement
        // problem every other speculating transform in this backend answers by
        // giving up; peeling answers it by moving the steady-state loop out of
        // the external edge's reach. The rewriter admits the shape — an edge
        // that targets the HEADER is not `ExternalEntry`, only one that lands
        // below it is — so this is a planner arm, not a new transform.
        if bypassable.contains(&header) {
            if body_size > LOOP_PEEL_MAX_BODY_BYTES {
                continue;
            }
            match plan_versioned(
                code,
                code_len,
                header,
                back_edge,
                LOOP_PEEL_COPIES,
                exception_ranges,
                LoopXformKind::Peel,
            ) {
                Ok(x) if !x.provenance_is_total() => return Err(R::ProvenanceNotTotal),
                Ok(x) => return Ok(x),
                Err(e) => {
                    last_refusal = Some(e);
                    continue;
                }
            }
        }
        // PGO path: a profiled trip count extends eligibility to larger
        // bodies. A refusal here does NOT fall through to the static
        // heuristic — same as the native unroller's `return`.
        if let Some(&pgo_factor) = loop_unroll_hints.get(&back_edge) {
            if body_size <= 50 || (body_size <= 100 && pgo_factor <= 2) {
                return match plan_versioned(
                    code,
                    code_len,
                    header,
                    back_edge,
                    pgo_factor.saturating_sub(1),
                    exception_ranges,
                    LoopXformKind::Unroll,
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
        match plan_versioned(
            code,
            code_len,
            header,
            back_edge,
            extra_copies,
            exception_ranges,
            LoopXformKind::Unroll,
        ) {
            Ok(x) if !x.provenance_is_total() => return Err(R::ProvenanceNotTotal),
            Ok(x) => return Ok(x),
            Err(e) => last_refusal = Some(e),
        }
    }
    tally(match last_refusal {
        Some(_) => "loop_xform_planner_refused",
        None => "loop_xform_no_candidate_loop",
    });
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
pub(super) fn replicate_pc3<A: Clone, B: Clone>(
    x: &LoopXform,
    t: Vec<(usize, A, B)>,
) -> Vec<(usize, A, B)> {
    let packed: Vec<(usize, (A, B))> = t.into_iter().map(|(pc, a, b)| (pc, (a, b))).collect();
    x.replicate_pc_keyed(&packed)
        .into_iter()
        .map(|(pc, (a, b))| (pc, a, b))
        .collect()
}

/// Where do two copies of one bytecode's deopt points differ?
///
/// Split in two, because the bci-keyed consumers in `jit/src/lib.rs` take one
/// half on trust and re-validate the other half against the live interpreter
/// frame before using it. `Fatal` is the first half; `Divergent` is the second
/// and is reported rather than refused.
enum PointDifference {
    /// A field a bci-keyed consumer uses WITHOUT checking it: picking the wrong
    /// copy silently applies the wrong one.
    ///
    /// `transfer_osr_exit_into_live_frame` reads the point's `semantics` and
    /// `real_frame_deopt_resume_and_despeculate`'s de-speculation step reads its
    /// `reason` (which is the grouping key here, so it never reaches this).
    /// Neither is checked against anything.
    Fatal(String),
    /// A field the OSR ENTRY contract re-derives and then VERIFIES.
    ///
    /// `osr_entry_frame_state` reads each slot through
    /// `OsrSlotType::from_frame_value` — deliberately coarser than `FrameValue`,
    /// which also encodes *where* the value lives — and `try_osr_entry` compares
    /// every one of those expectations against the live interpreter frame's own
    /// tags, refusing the entry on a mismatch. So picking the wrong copy here
    /// can only make an OSR entry be refused (or accepted) that the other copy
    /// would have decided the other way. Both outcomes are safe: the seeding is
    /// by machine home, which is method-wide in this backend, and every path
    /// that RECONSTRUCTS a frame finds its point by native offset
    /// (`CompiledMethod::find_deopt_point`) or through the box pointer the
    /// copy's own deopt stub bakes — never by bci.
    ///
    /// It is a real thing, not a hypothetical: `IndyDeoptProbe.concatLoop`'s
    /// two unrolled copies disagree about local 3 (`Register` vs `RegisterRef`)
    /// at the `invokedynamic`, because the forward oop dataflow reaches copy 1
    /// through copy 0's `astore_3` and reaches copy 0 through the loop entry,
    /// where the local is not yet a reference. Refusing that discarded the
    /// method for no soundness gain.
    Divergent(String),
}

/// The first difference between two deopt points recorded at two images of one
/// bytecode, or `None` if they describe the same program point.
///
/// `native_offset` and every `FrameValue`'s machine location are excluded by
/// construction: two copies are SUPPOSED to differ there. Operand spill offsets
/// in particular are handed out as the walk emits, so copy 1's operand lives in
/// a different frame slot from copy 0's, and both are right for their own copy.
fn deopt_point_difference(
    a: &crate::deopt::DeoptimizationPoint,
    b: &crate::deopt::DeoptimizationPoint,
) -> Option<PointDifference> {
    use PointDifference::{Divergent, Fatal};
    fn kind(v: &crate::deopt::FrameValue) -> &'static str {
        match crate::OsrSlotType::from_frame_value(v) {
            Some(t) => t.name(),
            None => "undescribable",
        }
    }
    fn slots(
        what: &str,
        a: &[crate::deopt::FrameValue],
        b: &[crate::deopt::FrameValue],
    ) -> Option<String> {
        if a.len() != b.len() {
            return Some(format!("{what} count {} vs {}", a.len(), b.len()));
        }
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            if crate::OsrSlotType::from_frame_value(x) != crate::OsrSlotType::from_frame_value(y) {
                return Some(format!(
                    "{what} {i} is {} ({x:?}) vs {} ({y:?})",
                    kind(x),
                    kind(y)
                ));
            }
        }
        None
    }
    if a.semantics != b.semantics {
        return Some(Fatal(format!(
            "semantics {:?} vs {:?}",
            a.semantics, b.semantics
        )));
    }
    if a.action != b.action {
        return Some(Fatal(format!("action {:?} vs {:?}", a.action, b.action)));
    }
    if a.speculation_id != b.speculation_id {
        return Some(Fatal(format!(
            "speculation id {} vs {}",
            a.speculation_id, b.speculation_id
        )));
    }
    if a.frame_state.bci != b.frame_state.bci {
        return Some(Fatal(format!(
            "frame bci {} vs {}",
            a.frame_state.bci, b.frame_state.bci
        )));
    }
    if a.frame_state.method_key != b.frame_state.method_key {
        return Some(Fatal(format!(
            "method key {:?} vs {:?}",
            a.frame_state.method_key, b.frame_state.method_key
        )));
    }
    // Inline sites are still refused, so a caller chain under a rewrite means
    // something changed that none of this argument considered.
    if a.frame_state.caller.is_some() || b.frame_state.caller.is_some() {
        return Some(Fatal(
            "an inlined caller scope, which inline sites are still refused for".to_string(),
        ));
    }
    if let Some(d) = slots("local", &a.frame_state.locals, &b.frame_state.locals) {
        return Some(Divergent(d));
    }
    if let Some(d) = slots("stack slot", &a.frame_state.stack, &b.frame_state.stack) {
        return Some(Divergent(d));
    }
    if a.frame_state.monitors.len() != b.frame_state.monitors.len() {
        return Some(Divergent(format!(
            "monitor count {} vs {}",
            a.frame_state.monitors.len(),
            b.frame_state.monitors.len()
        )));
    }
    for (i, (m, n)) in a
        .frame_state
        .monitors
        .iter()
        .zip(b.frame_state.monitors.iter())
        .enumerate()
    {
        if m.lock_depth != n.lock_depth
            || crate::OsrSlotType::from_frame_value(&m.object)
                != crate::OsrSlotType::from_frame_value(&n.object)
        {
            return Some(Divergent(format!("monitor {i} differs")));
        }
    }
    None
}

/// May this transformed compile's deopt points be published as interpreter
/// resume points?
///
/// `build_and_record_deopt_point` publishes `DeoptimizationPoint::bci` through
/// `Compiler::orig_bci`. This re-derives that translation from the emitter pc
/// each point was recorded at (`Compiler::deopt_point_pcs`) and refuses the
/// METHOD — not the transform, which is already emitted by the time this runs —
/// if any of four things does not hold. Refusing costs the method its
/// compilation; publishing an output PC as a resume bci resumes arbitrary
/// bytecode, so this is fail-closed by construction and its counter
/// (`loop_xform_deopt_bci_unpublishable`) should read zero forever.
///
///  1. **Every point still has its emitter pc.** A `deopt_points` push that
///     forgets `deopt_point_pcs` would silently misalign every check below, so
///     the lengths are compared first and the mismatch is fatal.
///  2. **The published bci is what the provenance map says**, and is inside the
///     ORIGINAL method. This is the actual translation, checked rather than
///     trusted: an emit path that bakes a raw pc into a point it constructs
///     itself fails here instead of reaching the VM.
///  3. **No point sits on the versioning guard.** The guard's bytes carry the
///     loop header's bci so provenance stays total, but they are an image of no
///     instruction — `encode_preheader_guard` synthesises them — and the
///     abstract operand stack part-way through them is not the header's. A
///     resume there would re-enter the interpreter at the header with the
///     guard's operands live.
///  4. **Copies of one bytecode describe the same frame.** The bci-keyed
///     consumers in `jit/src/lib.rs` (`osr_entry_frame_state`,
///     `transfer_osr_exit_into_live_frame`, the de-speculation reason lookup)
///     take the FIRST point with a matching bci, so if the copies disagree the
///     pick is arbitrary. Only the fields those consumers take ON TRUST are
///     refused; the OSR entry contract's slot types are re-verified against the
///     live interpreter frame, so a divergence there is counted
///     (`loop_xform_deopt_frames_diverge`) and logged instead. See
///     [`PointDifference`], which is where that split is argued.
///
///     Grouped by `(bci, reason)` and compared only across DISTINCT emitter
///     pcs, because neither of the other two shapes is the rewrite's doing. One
///     pc can carry two points with different reasons — an `invokedynamic`
///     inside an OSR-eligible pc records both an `OsrExit` map and the trap's
///     `UnreachedCode` map — and an ordinary compile publishes that same pair
///     under one bci. Refusing it here would refuse a shape that has nothing to
///     do with the coordinate change.
pub(super) fn rewritten_deopt_points_are_publishable(
    x: &LoopXform,
    points: &[crate::deopt::DeoptimizationPoint],
    emitter_pcs: &[usize],
    orig_code_len: usize,
) -> Result<(), String> {
    if points.len() != emitter_pcs.len() {
        return Err(format!(
            "{} deopt points but {} recorded emitter pcs: a `deopt_points` push \
             skipped `deopt_point_pcs`",
            points.len(),
            emitter_pcs.len()
        ));
    }
    let guard = x.guard_span();
    let mut first_at: FxHashMap<(u32, crate::deopt::DeoptReason), usize> = FxHashMap::default();
    for (i, p) in points.iter().enumerate() {
        let pc = emitter_pcs[i];
        if let Some((from, to)) = guard {
            if pc >= from && pc < to {
                return Err(format!(
                    "deopt point at output pc {pc} lies inside the versioning guard \
                     [{from}, {to}), whose bytes are synthetic and are an image of \
                     no original instruction"
                ));
            }
        }
        match x.bci_at(pc) {
            // Widening: u32 -> usize
            Some(bci) if bci == p.bci as usize => {}
            other => {
                return Err(format!(
                    "deopt point at output pc {pc} published bci {} but the \
                     rewrite's provenance says {other:?}",
                    p.bci
                ));
            }
        }
        // Widening: u32 -> usize
        if p.bci as usize >= orig_code_len {
            return Err(format!(
                "deopt point at output pc {pc} published bci {}, past the original \
                 method's {orig_code_len} bytes",
                p.bci
            ));
        }
        if p.frame_state.bci != p.bci {
            return Err(format!(
                "deopt point at output pc {pc} published bci {} but its frame \
                 state says {}",
                p.bci, p.frame_state.bci
            ));
        }
        // Keyed on `(bci, reason)` and compared only across DISTINCT emitter
        // pcs — see point 4 of the doc comment for why the other two shapes are
        // an ordinary compile's and not this rewrite's.
        match first_at.get(&(p.bci, p.reason)) {
            Some(&j) if emitter_pcs[j] != pc => match deopt_point_difference(&points[j], p) {
                Some(PointDifference::Fatal(how)) => {
                    return Err(format!(
                        "two copies of bci {} published disagreeing {:?} points \
                             (output pcs {} and {pc}): {how}; a bci-keyed consumer \
                             takes that field on trust and would pick one arbitrarily",
                        p.bci, p.reason, emitter_pcs[j]
                    ));
                }
                Some(PointDifference::Divergent(how)) => {
                    crate::metrics::record_loop_xform_event("loop_xform_deopt_frames_diverge");
                    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN") {
                        eprintln!(
                            "[JIT_GEN] loop-rewrite copies of bci {} diverge at \
                                 output pcs {} and {pc}: {how} — the OSR entry \
                                 contract re-validates this, so it is reported, not \
                                 refused",
                            p.bci, emitter_pcs[j]
                        );
                    }
                }
                None => {}
            },
            Some(_) => {}
            None => {
                first_at.insert((p.bci, p.reason), i);
            }
        }
    }
    Ok(())
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
