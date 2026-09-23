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
    find_bypassable_loop_headers, find_rotation_preheader_entries, rotated_preheader_enabled,
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
/// over `detect_loops`' loops with the method's exception table. Two driver
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_BCE")
}

fn matrix_dot_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MATRIX_DOT")
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
    *G.get_or_init(|| cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_C1_VECTOR_VETO"))
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
    // The driver's rotated-header map, computed the same way
    // (`x64/driver.rs`, `rotation_entries`).
    let rotation: FxHashMap<usize, usize> = if rotated_preheader_enabled() {
        find_rotation_preheader_entries(code, code_len, &loops, exception_ranges)
    } else {
        FxHashMap::default()
    };
    let guards = LazySimdGuards {
        code,
        code_len,
        loops: &loops,
        exception_ranges,
        cell: std::cell::OnceCell::new(),
    };
    SinglePassOnly::ALL
        .iter()
        .copied()
        .find(|k| k.enabled() && k.fires(code, code_len, &loops, &bypassable, &rotation, &guards))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The module header's spelled-out count is the real one.
    ///
    /// The header said "The seven below" while [`SinglePassOnly::ALL`] held
    /// six, because loop unswitching was dropped from the list (2026-09-12)
    /// and the sentence above it was not. That is precisely the failure this
    /// file's own header warns about — "the comment written to prevent the
    /// drift was the drift" — so the number is read out of the source and
    /// compared, not maintained by hand.
    ///
    /// Only the English numerals this header could plausibly use are
    /// recognised; a count past `ten` fails loudly rather than silently
    /// passing, which is the right direction for a list that is supposed to
    /// stay small enough to enumerate by reading.
    #[test]
    fn the_prose_count_matches_all() {
        const NUMERALS: [&str; 11] = [
            "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        ];
        let src = include_str!("single_pass_only.rs");
        // Split the needle so this literal is not itself a match: the file
        // contains `" below were "` and `"found by reading every list"` as two
        // adjacent tokens, never as the contiguous sentence the header has.
        let needle = concat!(" below were ", "found by reading every list");
        let (before, _) = src.split_once(needle).expect(
            "the header sentence that counts the variants is gone; either              restore it or delete this test deliberately",
        );
        let spelled = before
            .rsplit(|c: char| !c.is_ascii_alphabetic())
            .find(|w| !w.is_empty())
            .expect("a word precedes ` below were found`")
            .to_ascii_lowercase();
        let n = SinglePassOnly::ALL.len();
        let expected = NUMERALS.get(n).unwrap_or_else(|| {
            panic!(
                "ALL now has {n} variants, past the numerals this test knows;                  add one to NUMERALS and update the header"
            )
        });
        assert_eq!(
            &spelled, expected,
            "the module header says \"{spelled}\" but `SinglePassOnly::ALL` has              {n} variants — update the header sentence"
        );
    }

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

    /// `static long s(int[] a) { long s = 0; int n = a.length;
    /// for (int i = 0; i < n; i++) s = a[i] + s; return s; }` (javac; the
    /// historical sum form, detected with or without the vectorize switch).
    /// Locals: a=0, (unused 1), s=2..3, n=4, i=5.
    const SUM_ONLY: [u8; 32] = [
        0x09, 0x41, // 0: s = 0L
        0x2a, 0xbe, 0x36, 0x04, // 2: n = a.length
        0x03, 0x36, 0x05, // 6: i = 0
        0x15, 0x05, 0x15, 0x04, 0xa2, 0x00, 0x11, // 9: if (i >= n) -> 30
        0x2a, 0x15, 0x05, 0x2e, 0x85, 0x20, 0x61, 0x41, // 16: s = a[i] + s
        0x84, 0x05, 0x01, // 24: i++
        0xa7, 0xff, 0xee, // 27: goto 9
        0x20, 0xad, // 30: return s
    ];

    /// [`SUM_ONLY`] followed by a sibling scalar loop,
    /// `for (int j = 0; j < k; j++) s = s * 31 + j;` (k = local 1, j = 6).
    const SUM_AND_SIBLING: [u8; 57] = [
        0x09, 0x41, // 0: s = 0L
        0x2a, 0xbe, 0x36, 0x04, // 2: n = a.length
        0x03, 0x36, 0x05, // 6: i = 0
        0x15, 0x05, 0x15, 0x04, 0xa2, 0x00, 0x11, // 9: if (i >= n) -> 30
        0x2a, 0x15, 0x05, 0x2e, 0x85, 0x20, 0x61, 0x41, // 16: s = a[i] + s
        0x84, 0x05, 0x01, // 24: i++
        0xa7, 0xff, 0xee, // 27: goto 9
        0x03, 0x36, 0x06, // 30: j = 0
        0x15, 0x06, 0x1b, 0xa2, 0x00, 0x13, // 33: if (j >= k) -> 55
        0x20, 0x10, 0x1f, 0x85, 0x69, // 39: s * 31L
        0x15, 0x06, 0x85, 0x61, 0x41, // 44: s = .. + j
        0x84, 0x06, 0x01, // 49: j++
        0xa7, 0xff, 0xed, // 52: goto 33
        0x20, 0xad, // 55: return s
    ];

    /// Round 9 wave 4: the SIMD families veto the IR tier only when the
    /// vectorised loops carry the method's loop work (`dominate`). A sibling
    /// scalar loop keeps the IR body (measured: `LoopProbe mixed`, 2,032 ms
    /// with its IR body against 2,402 ms vetoed).
    #[test]
    fn a_sibling_scalar_loop_keeps_the_ir_body() {
        if !has_avx2() || !c1_vector_veto_enabled() || no_bce() {
            return;
        }
        assert_eq!(
            detect_loops(&SUM_ONLY, SUM_ONLY.len()),
            vec![(9, 27)],
            "fixture: one loop"
        );
        assert_eq!(
            single_pass_only_lowering(&SUM_ONLY, SUM_ONLY.len(), &[]),
            Some(SinglePassOnly::SimdIntArraySum),
            "a method that is its vectorised sum keeps the single-pass body"
        );
        assert_eq!(
            detect_loops(&SUM_AND_SIBLING, SUM_AND_SIBLING.len()),
            vec![(9, 27), (33, 52)],
            "fixture: two sibling loops"
        );
        assert_eq!(
            single_pass_only_lowering(&SUM_AND_SIBLING, SUM_AND_SIBLING.len(), &[]),
            None,
            "a vectorised sum next to a scalar loop must not veto the IR tier"
        );
    }

    /// The dominance rule itself, host-independent.
    #[test]
    fn vectorised_loops_dominate_only_their_own_nest() {
        let simd = CoveredSimdLoops {
            sums: vec![(9, 27)],
            element_wise: Vec::new(),
        };
        assert!(simd.dominate(&[(9, 27)]), "the loop itself");
        assert!(
            simd.dominate(&[(2, 40), (9, 27)]),
            "an outer loop that only repeats it"
        );
        assert!(
            !simd.dominate(&[(9, 27), (33, 52)]),
            "a sibling scalar loop"
        );
        let ewise = CoveredSimdLoops {
            sums: Vec::new(),
            element_wise: vec![(33, 52)],
        };
        assert!(
            ewise.dominate(&[(33, 52)]) && !ewise.dominate(&[(9, 27), (33, 52)]),
            "the element-wise family follows the same rule"
        );
    }
}
