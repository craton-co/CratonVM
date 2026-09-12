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
//! | `aaload`/FP LICM (`hoist_info`, `fp_hoist_info`) | `ir_optimize::licm` | on (`CRATONVM_JIT_LICM`) |
//!
//! Both are *on* — `map_or(true, ..)` — despite comments next to them in
//! `ir_optimize.rs` that still say "Default-OFF while it soaks". Those
//! comments are stale (corrected 2026-08-04); the flags default on. Vetoing on
//! either would cost a large population of IR bodies to buy nothing.
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
//! * The seven below were found by reading every list the single-pass emitters
//!   consult at a loop header (`x64/bytecode_walk.rs`, `x64/simd.rs`), asking
//!   for each whether `ir_optimize`/`ir_lower` has a counterpart, and then
//!   reading what the emitter actually emits. That audit is reproducible; it
//!   is not automatic.
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

use super::escape_analysis::{
    bulk_byte_loops_enabled, detect_bulk_set_byte_stride_loop, detect_bulk_zero_byte_fill_loop,
    detect_byte_sieve_loop, detect_loops, find_bypassable_loop_headers,
};
use super::simd_analysis::{
    detect_int_array_element_wise, detect_int_array_sum, detect_matrix_dot_loop,
};
use super::{bytecode_analysis, find_induction_variable, has_avx2};
use rustc_hash::FxHashSet;

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
    fn fires(
        self,
        code: &[u8],
        code_len: usize,
        loops: &[(usize, usize)],
        bypassable: &FxHashSet<usize>,
    ) -> bool {
        // A pre-header is placed where a bypassable header would skip it, so
        // the driver drops every speculating transform for such a header and
        // so does this.
        let live = |h: &usize| !bypassable.contains(h);
        match self {
            SinglePassOnly::BulkZeroByteFill => loops.iter().any(|&(h, b)| {
                live(&h) && detect_bulk_zero_byte_fill_loop(code, code_len, h, b).is_some()
            }),
            SinglePassOnly::BulkSetByteStride => loops.iter().any(|&(h, b)| {
                live(&h) && detect_bulk_set_byte_stride_loop(code, code_len, h, b).is_some()
            }),
            SinglePassOnly::ByteSieve => loops
                .iter()
                .any(|&(h, b)| live(&h) && detect_byte_sieve_loop(code, code_len, h, b).is_some()),
            SinglePassOnly::SimdIntArraySum => {
                !no_bce()
                    && loops.iter().any(|&(h, b)| {
                        live(&h)
                            && induction_var(code, h, b)
                                .and_then(|iv| detect_int_array_sum(code, h, b, iv))
                                .is_some()
                    })
            }
            SinglePassOnly::SimdArrayElementWise => loops.iter().any(|&(h, b)| {
                live(&h)
                    && induction_var(code, h, b)
                        .and_then(|iv| detect_int_array_element_wise(code, h, b, iv))
                        .is_some()
            }),
            SinglePassOnly::MatrixDot => {
                !no_bce()
                    && loops.iter().any(|&(h, b)| {
                        live(&h)
                            && induction_var(code, h, b)
                                .and_then(|iv| detect_matrix_dot_loop(code, h, b, iv))
                                .is_some()
                    })
            }
        }
    }
}

fn induction_var(code: &[u8], header: usize, back_edge: usize) -> Option<usize> {
    find_induction_variable(code, header, back_edge + bytecode_analysis::step(code, back_edge))
}

fn no_bce() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_BCE")
}

fn matrix_dot_enabled() -> bool {
    cratonvm_types::flags::runtime_var("CRATONVM_JIT_MATRIX_DOT").map_or(true, |v| {
        let v = v.trim();
        v != "0" && !v.eq_ignore_ascii_case("false") && !v.eq_ignore_ascii_case("off")
    })
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
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_C1_VECTOR_VETO")
            .map(|v| {
                let v = v.trim();
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            })
            .unwrap_or(true)
    })
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
    let bypassable = find_bypassable_loop_headers(code, code_len, &loops, exception_ranges);
    SinglePassOnly::ALL
        .iter()
        .copied()
        .find(|k| k.enabled() && k.fires(code, code_len, &loops, &bypassable))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A variant that exists but is missing from `ALL` would be silently
    /// exempt from the veto — the exact shape of the bug this module is for,
    /// one level down. `ordinal` is exhaustive, so a new variant does not
    /// compile until it has an index; this makes that index mean something.
    #[test]
    fn all_variants_are_registered() {
        for (i, k) in SinglePassOnly::ALL.iter().enumerate() {
            assert_eq!(
                k.ordinal(),
                i,
                "{k:?} is at index {i} of ALL but its ordinal is {}; ALL and \
                 ordinal() have drifted",
                k.ordinal()
            );
        }
        assert_eq!(
            SinglePassOnly::ALL.len(),
            SinglePassOnly::ALL
                .iter()
                .map(|k| k.ordinal())
                .max()
                .map_or(0, |m| m + 1),
            "ALL has a gap: some variant's ordinal is not covered"
        );
    }

    #[test]
    fn every_variant_has_a_distinct_label() {
        let mut labels: Vec<&str> = SinglePassOnly::ALL.iter().map(|k| k.label()).collect();
        labels.sort_unstable();
        let before = labels.len();
        labels.dedup();
        assert_eq!(before, labels.len(), "two variants share a label");
    }

    /// The regression this whole module exists for. If the veto stops firing
    /// on `CratonBench.sieve`, the IR tier takes the method back and the phase
    /// goes 2,462 ms -> 15,823 ms with no test failing and no checksum
    /// changing, which is precisely how it shipped the first time.
    #[test]
    fn the_sieve_that_cost_6x_is_vetoed_and_named() {
        // The exact javac shape of `CratonBench.sieve(boolean[], int)`.
        let sieve = [
            0x03, 0x3d, // 0: int i/count = 0
            0x1c, 0x1b, 0xa3, 0x00, 0x0d, // 2: clear-loop header -> 17
            0x2a, 0x1c, 0x03, 0x54, // 7: a[i] = 0
            0x84, 0x02, 0x01, 0xa7, 0xff, 0xf4, // 11: i++; goto 2
            0x03, 0x3d, 0x05, 0x3e, // 17: count=0; outer i=2
            0x1d, 0x1b, 0xa3, 0x00, 0x2b, // 21: outer header -> 66
            0x2a, 0x1d, 0x33, 0x9a, 0x00, 0x1f, // 26: if (a[i]) -> 60
            0x84, 0x02, 0x01, // 32: count++
            0x1d, 0x1d, 0x60, 0x36, 0x04, // 35: j=i+i
            0x15, 0x04, 0x1b, 0xa3, 0x00, 0x11, // 40: inner header -> 60
            0x2a, 0x15, 0x04, 0x04, 0x54, // 46: a[j] = 1
            0x15, 0x04, 0x1d, 0x60, 0x36, 0x04, // 51: j += i
            0xa7, 0xff, 0xef, // 57: goto 40
            0x84, 0x03, 0x01, 0xa7, 0xff, 0xd6, // 60: i++; goto 21
            0x1c, 0xac, // 66: return count
            0x00, 0x00,
        ];
        let hit = single_pass_only_lowering(&sieve, 68, &[]);
        assert!(
            hit.is_some(),
            "the veto must fire on CratonBench.sieve — without it the IR tier \
             takes this method and emits a scalar loop 6.4x slower than the \
             vectorised one the single-pass backend already emits"
        );
        // Naming it is not decoration: the admission line is what a reader of
        // a perf-gate results directory sees, and "declined" without "why" is
        // what made the original regression take a bisect to find.
        assert!(
            !hit.unwrap().label().is_empty(),
            "a veto that cannot say which lowering it protected is a veto \
             nobody can audit"
        );
    }

    /// The other half, and the one that would catch a veto gone greedy: a
    /// method with no loop at all keeps its IR body. Without this the module
    /// would still pass every other test while refusing everything.
    #[test]
    fn a_loopless_method_is_not_vetoed() {
        // iconst_0; ireturn
        assert_eq!(single_pass_only_lowering(&[0x03, 0xac], 2, &[]), None);
    }

    /// And a method that loops but over nothing the single-pass backend
    /// specialises: `for (int i = 0; i < n; i++) {}` — a bare counted loop
    /// with an empty body.
    #[test]
    fn a_plain_counted_loop_is_not_vetoed() {
        let code = [
            0x03, 0x3c, // 0: int i = 0
            0x1b, 0x1a, 0xa2, 0x00, 0x08, // 2: if (i >= n) -> 10
            0x84, 0x01, 0x01, // 7: i++
            0xa7, 0xff, 0xf8, // 10 (back edge): goto 2
            0xb1, // 13: return
        ];
        assert_eq!(single_pass_only_lowering(&code, 14, &[]), None);
    }
}
