// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Every lowering the SINGLE-PASS backend performs that the optimizing (IR)
//! tier has no equivalent for — enumerated in one place, on purpose.
//!
//! # Why this file exists
//!
//! The optimizing tier installs its body whenever it *can*. Nothing checks
//! that the body it installs is faster than the one the single-pass backend
//! would have installed. That is fine while the two backends have the same
//! repertoire and stops being fine the moment one of them grows a
//! specialisation the other lacks.
//!
//! `cov-02` made that concrete. It taught `IrBuilder::build` to lower
//! `bastore`, which is right; the side effect was that
//! `CratonBench.sieve([ZI)I` stopped falling through to the single-pass
//! backend — which *vectorises* its `boolean[]` loops — and started getting a
//! scalar IR body. 2,462 ms became 15,823 ms, a phase on which CratonVM had
//! been faster than HotSpot C2, with an unchanged checksum and no failing
//! test (`perf-01-sieve-ir-body-slower-than-c1-FIXED-20260804.md`).
//!
//! Fixing that one method was a special case. This is the general form: the
//! set of things the single-pass backend can do and the IR tier cannot is
//! *finite*, it is *knowable*, and until now it was written down nowhere. The
//! admission chain consults this enum, so widening the IR tier onto a method
//! in any of these classes declines instead of silently downgrading it.
//!
//! # What is NOT here, and why
//!
//! Two single-pass loop transforms are deliberately absent because the IR tier
//! has its own, on by default:
//!
//! | single-pass | IR equivalent | default |
//! |---|---|---|
//! | native loop unroller (`unroll_loops`) | `ir_optimize::unroll` | on (`CRATONVM_JIT_UNROLL`) |
//! | `aaload` LICM (`hoist_info`; the FP half was never emitted and its analysis call was dropped in round 11) | `ir_optimize::licm` | on (`CRATONVM_JIT_LICM`) |
//!
//! Both are *on*, and as of 2026-09-16 both parse their kill switch the way
//! the rest of the crate does (`0|false|off|no`) rather than comparing against
//! the literal `"0"` — a switch that silently ignored `=false` is the one
//! thing a veto list like this one must not be reasoning from. The
//! "Default-OFF while it soaks" comments this paragraph used to warn about
//! were corrected in `ir_optimize.rs` itself on 2026-08-04 and no longer
//! exist. Vetoing on either transform would cost a large population of IR
//! bodies to buy nothing.
//!
//! A third was dropped for a different and more interesting reason. **Loop
//! unswitching was detected but never performed** (the detector and its
//! flag-setting pre-header were deleted on 2026-09-12).
//! `detect_loop_unswitch_candidates` feeds `emit_loop_unswitch_preheader`,
//! whose own contract says the sequence "is *additive* — it reads
//! `invariant_local` and sets flags but never writes back to any local
//! ... Removing the emission yields identical final state". It does not
//! duplicate the body or hoist the branch, so it confers no speed advantage to
//! protect. It was in the first draft of this list, on the strength of its
//! name and the fact that an emitter consumes it; that would have declined IR
//! bodies for every loop with an invariant branch — a common shape — to
//! protect nothing at all. **Do not add a variant here because a detector and
//! an emitter exist. Read what the emitter emits.**
//!
//! # The limit of this approach, stated plainly
//!
//! This catches an advantage that is *enumerated here*. It cannot catch one
//! nobody has written down — a future single-pass specialisation added without
//! a variant below is invisible to it, which is exactly the failure `cov-02`
//! demonstrated one level up.
//!
//! Two things narrow that gap and neither closes it:
//!
//! * Adding a variant forces every `match` in this file to be updated, because
//!   they are exhaustive, and `all_variants_are_registered` fails until
//!   [`SinglePassOnly::ALL`] lists it. So a variant cannot be half-added.
//! * The six below were found by reading every list the single-pass emitters
//!   consult at a loop header (`x64/bytecode_walk.rs`, `x64/simd.rs`), asking
//!   for each whether `ir_optimize`/`ir_lower` has a counterpart, and then
//!   reading what the emitter actually emits. That audit is reproducible; it
//!   is not automatic. (The numeral in that sentence read "seven" while `ALL`
//!   held six, from the day loop unswitching was dropped until 2026-09-16.
//!   `the_prose_count_matches_all` now reads it out of this file and compares
//!   it to `ALL.len()`, so it cannot drift again.)
//!
//! **The mechanism that would close it is a backend-parity harness**: compile
//! a corpus with both backends and compare the emitted bodies — for instance,
//! flagging any method whose single-pass body contains VEX-prefixed or
//! `REP`-string bytes that its IR body does not. That needs the two backends
//! driven independently over the same method, which the compile path does not
//! currently allow (the single-pass call site sits ~2,000 lines below the IR
//! one, behind local state built in between). It is written up as the next
//! increment rather than approximated here, because a parity check that only
//! *looks* general is worse than one whose limits are on the label.

use super::bce::{analyze_bounds_elimination_with_handlers, SpeculativeBCEGuard};
use super::escape_analysis::{
    bulk_byte_loops_enabled, detect_batch_loop, detect_bulk_set_byte_stride_loop,
    detect_bulk_zero_byte_fill_loop, detect_byte_sieve_loop, detect_loops,
    find_preheader_bypassing_loop_headers, find_rotation_preheader_entries,
    rotated_preheader_enabled,
};
use super::simd_analysis::{
    detect_int_array_element_wise, detect_int_array_sum, detect_matrix_dot_loop,
    simd_element_wise_covered, simd_sum_covered,
};
use super::{bytecode_analysis, find_induction_variable, has_avx2};
use rustc_hash::{FxHashMap, FxHashSet};

/// The speculative bounds-check guards the driver's SIMD coverage gate
/// consults, computed at most once per admission and only if a SIMD detector
/// matched (most methods never get that far, so the BCE analysis is not
/// charged to them).
///
/// Same call, same inputs as `x64/driver.rs`: `analyze_bounds_elimination_with_handlers`
/// over `detect_loops`' loops merged to one pair per header
/// (`driver::loops_by_header`) with the method's exception table. Two driver
/// filters are not reproduced: a guard at a bypassable header is dropped there
/// too, and every `fires` arm already refuses such a header (`live`); a guard
/// DE-SPECIALISED by the per-bci registry is dropped there and not here (the
/// admission chain has no registry). That one direction can still veto a loop
/// the driver then emits scalar — only after that loop's guard deopted past
/// the give-up threshold, which is rare and is recorded on the veto page.
struct LazySimdGuards<'a> {
    code: &'a [u8],
    code_len: usize,
    loops: &'a [(usize, usize)],
    exception_ranges: &'a [(usize, usize, usize)],
    cell: std::cell::OnceCell<Vec<SpeculativeBCEGuard>>,
}

impl LazySimdGuards<'_> {
    fn get(&self) -> &[SpeculativeBCEGuard] {
        self.cell.get_or_init(|| {
            analyze_bounds_elimination_with_handlers(
                self.code,
                self.code_len,
                self.loops,
                Some(self.exception_ranges),
            )
            .1
        })
    }
}

/// One class of loop lowering that only the single-pass backend can perform.
///
/// Every variant is consumed by a single-pass emitter at a loop header. None
/// has a counterpart in `ir_optimize` or `ir_lower`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SinglePassOnly {
    /// `REP STOSB` over a `byte[]`/`boolean[]` zero-fill loop.
    BulkZeroByteFill,
    /// `a[iv] = 1; iv += step` byte-array stride loop.
    BulkSetByteStride,
    /// The nested Sieve-of-Eratosthenes byte loop nest. This is the one that
    /// cost 6.4x when the IR tier took it.
    ByteSieve,
    /// SuperWord reduction over an `int[]` (AVX2).
    SimdIntArraySum,
    /// Element-wise `out[i] = a[i] OP b[i]` vectorisation (AVX2 at emission).
    SimdArrayElementWise,
    /// The matrix dot-product loop nest.
    MatrixDot,
}

impl SinglePassOnly {
    /// Every variant. Kept honest by `all_variants_are_registered`, which
    /// fails when a variant is added and not listed here.
    pub(crate) const ALL: &'static [SinglePassOnly] = &[
        SinglePassOnly::BulkZeroByteFill,
        SinglePassOnly::BulkSetByteStride,
        SinglePassOnly::ByteSieve,
        SinglePassOnly::SimdIntArraySum,
        SinglePassOnly::SimdArrayElementWise,
        SinglePassOnly::MatrixDot,
    ];

    /// Position in [`ALL`]. Exhaustive on purpose: a new variant does not
    /// compile until it is given one, and the test then fails until `ALL`
    /// carries it at that index.
    fn ordinal(self) -> usize {
        match self {
            SinglePassOnly::BulkZeroByteFill => 0,
            SinglePassOnly::BulkSetByteStride => 1,
            SinglePassOnly::ByteSieve => 2,
            SinglePassOnly::SimdIntArraySum => 3,
            SinglePassOnly::SimdArrayElementWise => 4,
            SinglePassOnly::MatrixDot => 5,
        }
    }

    /// What the admission chain prints when it declines a method.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SinglePassOnly::BulkZeroByteFill => "a bulk byte-array zero fill (REP STOSB)",
            SinglePassOnly::BulkSetByteStride => "a bulk byte-array stride store",
            SinglePassOnly::ByteSieve => "the byte-array sieve loop nest",
            SinglePassOnly::SimdIntArraySum => "a vectorised int[] reduction",
            SinglePassOnly::SimdArrayElementWise => "a vectorised element-wise array loop",
            SinglePassOnly::MatrixDot => "the matrix dot-product loop nest",
        }
    }

    /// Is this lowering switched on at all in this process?
    ///
    /// Mirrors the driver's own gate for each family, so the veto cannot fire
    /// for a lowering the backend would not have emitted anyway — on a host
    /// without AVX2, for instance, none of the SIMD families applies.
    fn enabled(self) -> bool {
        match self {
            SinglePassOnly::BulkZeroByteFill
            | SinglePassOnly::BulkSetByteStride
            | SinglePassOnly::ByteSieve => bulk_byte_loops_enabled(),
            // `detect_int_array_element_wise` runs unconditionally in the
            // driver so downstream passes can see it, but emission checks
            // `has_avx2()`. The veto follows EMISSION, not detection: a
            // method nobody will vectorise must not lose its IR body.
            SinglePassOnly::SimdIntArraySum | SinglePassOnly::SimdArrayElementWise => has_avx2(),
            SinglePassOnly::MatrixDot => matrix_dot_enabled(),
        }
    }

    /// Does this lowering apply to at least one loop in this method?
    ///
    /// Exhaustive, and each arm calls the same detector the driver calls.
    /// The two SIMD arms also apply the driver's COVERAGE gate
    /// (`simd_sum_covered` / `simd_element_wise_covered`, the same functions
    /// `x64/driver.rs` filters with): the veto follows emission, and a
    /// detected loop whose arrays are not covered is emitted scalar — vetoing
    /// the IR tier for it bought the method neither body
    /// (`simd-ir-tier-veto-follows-detection-not-emission-20260918.md`).
    fn fires(
        self,
        code: &[u8],
        code_len: usize,
        loops: &[(usize, usize)],
        bypassable: &FxHashSet<usize>,
        rotation: &FxHashMap<usize, usize>,
        guards: &LazySimdGuards<'_>,
    ) -> bool {
        // A pre-header is placed where a bypassable header would skip it, so
        // the driver drops every batch pre-header at such a header and so does
        // this -- except at a rotated header (`rotation`, from
        // `find_rotation_preheader_entries`, empty unless
        // `CRATONVM_JIT_ROTATED_PREHEADER` is armed), where the driver detects
        // the loop on its unrotated copy and the walk emits the pre-header at
        // the entry `goto` (round 9 wave 4). `detect_batch_loop` is that rule,
        // shared with `x64/driver.rs`.
        match self {
            SinglePassOnly::BulkZeroByteFill => loops.iter().any(|&(h, b)| {
                detect_batch_loop(
                    code,
                    code_len,
                    h,
                    b,
                    bypassable,
                    rotation,
                    detect_bulk_zero_byte_fill_loop,
                )
                .is_some()
            }),
            SinglePassOnly::BulkSetByteStride => loops.iter().any(|&(h, b)| {
                detect_batch_loop(
                    code,
                    code_len,
                    h,
                    b,
                    bypassable,
                    rotation,
                    detect_bulk_set_byte_stride_loop,
                )
                .is_some()
            }),
            SinglePassOnly::ByteSieve => loops.iter().any(|&(h, b)| {
                detect_batch_loop(
                    code,
                    code_len,
                    h,
                    b,
                    bypassable,
                    rotation,
                    detect_byte_sieve_loop,
                )
                .is_some()
            }),
            // `!no_bce()` for both SIMD families: the driver's coverage gate
            // refuses every SIMD candidate under `CRATONVM_JIT_NO_BCE`,
            // element-wise included.
            SinglePassOnly::SimdIntArraySum => {
                !no_bce() && {
                    let simd =
                        covered_simd_loops(code, code_len, loops, bypassable, rotation, guards);
                    !simd.sums.is_empty() && simd.dominate(loops)
                }
            }
            SinglePassOnly::SimdArrayElementWise => {
                !no_bce() && {
                    let simd =
                        covered_simd_loops(code, code_len, loops, bypassable, rotation, guards);
                    !simd.element_wise.is_empty() && simd.dominate(loops)
                }
            }
            SinglePassOnly::MatrixDot => {
                !no_bce()
                    && loops.iter().any(|&(h, b)| {
                        detect_batch_loop(
                            code,
                            code_len,
                            h,
                            b,
                            bypassable,
                            rotation,
                            |c, _, h, b| detect_matrix_dot_loop(c, h, b, induction_var(c, h, b)?),
                        )
                        .is_some()
                    })
            }
        }
    }
}

/// The loops of one method the single-pass backend emits as a SIMD sum or
/// element-wise pre-header, as `(header, back_edge)`: detected and covered by
/// the same predicates as the driver (`simd_sum_covered` /
/// `simd_element_wise_covered`).
struct CoveredSimdLoops {
    sums: Vec<(usize, usize)>,
    element_wise: Vec<(usize, usize)>,
}

impl CoveredSimdLoops {
    /// Does the method's loop work sit in its vectorised loops?
    ///
    /// Every loop must be a vectorised loop or enclose one (by bytecode range:
    /// an outer loop that repeats a vectorised inner loop). A method with a
    /// SIBLING loop that is neither -- a hot scalar loop next to a small sum --
    /// keeps its IR body: the veto would buy one vectorised loop at the price
    /// of the IR tier's code for everything else, and that everything else is
    /// what such a method spends its time in. Measured (round 9 wave 4, lane
    /// `x64core4`, `LoopProbe mixed`: a 20,000-iteration scalar recurrence
    /// plus a 16-element `s += a[i]` over `a.length`, wave-3 binary): 2,032 ms
    /// with its IR body against 2,402 ms vetoed under
    /// `CRATONVM_JIT_VECTORIZE=1` (median of 3; HotSpot 1,268 ms).
    ///
    /// The bulk-byte and matrix-dot families do not take this test: their
    /// veto is the historical one (`CratonBench.sieve`, 6.4x) and their loop
    /// nests are the method.
    fn dominate(&self, loops: &[(usize, usize)]) -> bool {
        loops.iter().all(|&(h, b)| {
            self.sums
                .iter()
                .chain(self.element_wise.iter())
                .any(|&(vh, vb)| h <= vh && vb <= b)
        })
    }
}

/// Collect [`CoveredSimdLoops`] for one method.
fn covered_simd_loops(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    bypassable: &FxHashSet<usize>,
    rotation: &FxHashMap<usize, usize>,
    guards: &LazySimdGuards<'_>,
) -> CoveredSimdLoops {
    let mut out = CoveredSimdLoops {
        sums: Vec::new(),
        element_wise: Vec::new(),
    };
    for &(h, b) in loops {
        let sum = detect_batch_loop(code, code_len, h, b, bypassable, rotation, |c, _, h, b| {
            detect_int_array_sum(c, h, b, induction_var(c, h, b)?)
        });
        if sum.is_some_and(|s| simd_sum_covered(&s, code, code_len, guards.get())) {
            out.sums.push((h, b));
        }
        let ewise = detect_batch_loop(code, code_len, h, b, bypassable, rotation, |c, _, h, b| {
            detect_int_array_element_wise(c, h, b, induction_var(c, h, b)?)
        });
        if ewise.is_some_and(|e| simd_element_wise_covered(&e, code, code_len, guards.get())) {
            out.element_wise.push((h, b));
        }
    }
    out
}

fn induction_var(code: &[u8], header: usize, back_edge: usize) -> Option<usize> {
    find_induction_variable(
        code,
        header,
        back_edge + bytecode_analysis::step(code, back_edge),
    )
}

fn no_bce() -> bool {
    false}

fn matrix_dot_enabled() -> bool {
    true
}

/// `CRATONVM_JIT='-c1-vector-veto'` — let the optimizing tier take methods the
/// single-pass backend would lower better. Default ON, i.e. the veto applies.
///
/// Default-on because it repairs a shipped regression rather than adding a
/// behaviour. The switch exists so the veto is bisectable, so its blast radius
/// can be measured on one binary rather than argued across two, and so a probe
/// that deliberately wants IR codegen for one of these shapes can get it —
/// this term sits *after* `CRATONVM_JIT_FORCE_C2` in the admission chain.
pub(crate) fn c1_vector_veto_enabled() -> bool {
    true
}

/// Which single-pass-only lowering, if any, applies to this method?
///
/// `Some(x)` means the single-pass backend has a lowering for this method that
/// the IR tier cannot match, so letting the IR tier produce the body is a
/// downgrade. The admission chain declines on `Some` and names `x`.
///
/// Runs the same detectors, over the same loops, behind the same gates as the
/// emission path, so it cannot fire for a lowering that would not be emitted.
///
/// **One case where it can, and why it is left alone.** The admission chain
/// sees the method's ORIGINAL bytecode; the driver re-binds `code` to the
/// rewritten buffer when the bytecode loop transform applies
/// (`x64/driver.rs`, `match &loop_xform`). Under
/// `CRATONVM_JIT='bytecode-loop-xform'` the two can disagree in either
/// direction. That transform is opt-in and off by default, so no shipped
/// configuration is affected, and both outcomes are worse codegen rather than
/// wrong codegen. Re-derive this if it ever becomes default-on.
pub(crate) fn single_pass_only_lowering(
    code: &[u8],
    code_len: usize,
    exception_ranges: &[(usize, usize, usize)],
) -> Option<SinglePassOnly> {
    if !c1_vector_veto_enabled() {
        return None;
    }
    let end = code_len.min(code.len());
    // Fast reject, so the analysis below is not charged to every compile.
    // Every variant needs a backward branch to have a loop at all, and `goto`
    // (0xa7) is what `detect_loops` looks for. Scanning raw bytes rather than
    // decoding means an operand byte that happens to be 0xa7 reads as a hit —
    // a false POSITIVE, which costs only the loop detection that then finds
    // nothing. It can never miss a real backward branch.
    if !code[..end].contains(&0xa7) {
        return None;
    }
    let loops = detect_loops(code, code_len);
    if loops.is_empty() {
        return None;
    }
    // The predicate the driver filters its batch pre-headers with
    // (`guard_bypassable_headers`): a forward branch onto a header runs its
    // pre-header, so it does not veto one (round 11 wave 5,
    // `r11-x64loop-header-entered-from-outside-drops-every-preheader`).
    let bypassable =
        find_preheader_bypassing_loop_headers(code, code_len, &loops, exception_ranges);
    // The driver's rotated-header map, computed the same way
    // (`x64/driver.rs`, `rotation_entries`).
    let rotation: FxHashMap<usize, usize> = if rotated_preheader_enabled() {
        find_rotation_preheader_entries(code, code_len, &loops, exception_ranges)
    } else {
        FxHashMap::default()
    };
    // The driver's BCE runs over one pair per header (round 11 wave 9); the
    // SIMD detectors and their `live` veto keep the per-pair `loops`.
    let merged = super::driver::loops_by_header(&loops);
    let guards = LazySimdGuards {
        code,
        code_len,
        loops: &merged,
        exception_ranges,
        cell: std::cell::OnceCell::new(),
    };
    SinglePassOnly::ALL
        .iter()
        .copied()
        .find(|k| k.enabled() && k.fires(code, code_len, &loops, &bypassable, &rotation, &guards))
}

#[cfg(test)]
mod tests;
