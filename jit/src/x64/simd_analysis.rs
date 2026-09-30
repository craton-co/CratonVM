// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! SIMD loop analysis: vectorization and SuperWord.
//!
//! Moved verbatim out of `x64.rs`'s `SIMD loop analysis and vectorization`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.
//!
//! Each bytecode pattern detector below recognizes one hand-written shape and
//! hands it to a hand-written emitter in `x64/simd.rs`.
//!
//! Round 12 wave 3 (lane vec, X5 of `docs/internal/jit-proposals/jit-r12-x64opt-proposals-RETIRED-20260928.md`) deleted the
//! general admission gate that used to live here (`vector_gate`, ~3100 lines
//! with its tests) together with its emitter `x64/vec_emit.rs`: neither had a
//! production caller in any build, and every round reviewed them anyway. They
//! are in git history (`a35b85a27`) for whoever wires a general vectorizer;
//! the lesson worth keeping from them is that a strict-FP reduction may not be
//! reordered, which `driver.rs` states where the retired FP detector stood.

use super::*;

/// Information about a vectorizable int-array sum reduction loop.
/// Pattern: for (i = start; i < bound; i++) sum += arr[i]
#[derive(Debug)]
#[allow(dead_code)]
pub(super) struct SimdIntArraySum {
    /// Bytecode PC of the loop header
    pub(super) header_pc: usize,
    /// Bytecode PC of the back-edge instruction
    pub(super) back_edge_pc: usize,
    /// Local index of the induction variable (i)
    pub(super) iv_local: usize,
    /// Local index of the accumulator (sum)
    pub(super) acc_local: usize,
    /// Local index of the array reference
    pub(super) array_local: usize,
    /// The local the single-pass walk loads into R11 before the pre-header:
    /// `bound.walk_local()`. For a [`SimdLoopBound::Local`] header that is the
    /// `int` bound; for [`SimdLoopBound::ArrayLength`] it is the ARRAY whose
    /// length bounds the loop, and the emitter turns the reference into its
    /// length itself (null-checked). Kept as a field, not derived, because the
    /// walk and the driver's coverage gate read it by name.
    pub(super) bound_local: usize,
    /// Where the loop's exclusive upper bound comes from.
    pub(super) bound: SimdLoopBound,
    /// Whether accumulator is long (i2l + ladd + lstore vs iadd + istore)
    pub(super) acc_is_long: bool,
}

/// Where a single-pass SIMD loop's exclusive upper bound comes from.
///
/// `javac` compiles the idiomatic `for (int i = 0; i < a.length; i++)` to a
/// header that re-reads the length every iteration — `iload i ; aload a ;
/// arraylength ; if_icmpge exit` — so the bound never sits in a local. Until
/// round 9 wave 2 the detectors only accepted `iload bound` there, which left
/// the live AVX2 paths close to dead on real code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SimdLoopBound {
    /// `iload bound` — an `int` local, read once by the pre-header.
    Local(usize),
    /// `aload a ; arraylength` — the length of the array in local `a`. The
    /// pre-headers null-check `a` (a null array falls through to the original
    /// header, whose `arraylength` throws the NPE at the right bci) and load
    /// the length themselves. Matched only under the vectorization opt-in
    /// (`simd_sum_forms_enabled`), for the reason given there.
    ArrayLength(usize),
}

impl SimdLoopBound {
    /// The local the single-pass walk loads into R11 before calling the
    /// pre-header: the `int` bound, or the array whose length is the bound.
    pub(crate) fn walk_local(self) -> usize {
        match self {
            SimdLoopBound::Local(l) | SimdLoopBound::ArrayLength(l) => l,
        }
    }
}

// Re-anchoring for descriptors detected on an unrotated copy of a rotated
// loop (`escape_analysis::detect_batch_loop`, round 9 wave 4).
impl BatchLoopAnchor for SimdIntArraySum {
    fn anchor_at(&mut self, header: usize, back_edge: usize) {
        self.header_pc = header;
        self.back_edge_pc = back_edge;
    }
}

impl BatchLoopAnchor for SimdArrayElementWise {
    fn anchor_at(&mut self, header: usize, back_edge: usize) {
        self.header_pc = header;
        self.back_edge_pc = back_edge;
    }
}

impl BatchLoopAnchor for MatrixDotLoop {
    fn anchor_at(&mut self, header: usize, back_edge: usize) {
        self.header_pc = header;
        self.back_edge_pc = back_edge;
    }
}

/// Parse the bound operand of an `iload iv ; <bound> ; if_icmpge` loop header
/// at `*pc`, advancing `*pc` past it. `aload a ; arraylength` is accepted only
/// when `allow_array_length`.
fn take_simd_loop_bound(
    code: &[u8],
    pc: &mut usize,
    allow_array_length: bool,
) -> Option<SimdLoopBound> {
    if let Some(local) = extract_iload_local(code, *pc) {
        *pc += if code.get(*pc).copied() == Some(0x15) {
            2
        } else {
            1
        };
        return Some(SimdLoopBound::Local(local));
    }
    if !allow_array_length {
        return None;
    }
    let array = extract_aload_local(code, *pc)?;
    let after = *pc
        + if code.get(*pc).copied() == Some(0x19) {
            2
        } else {
            1
        };
    if code.get(after).copied()? != 0xbe {
        return None; // not `arraylength`
    }
    *pc = after + 1;
    Some(SimdLoopBound::ArrayLength(array))
}

/// The coverage obligation of a single-pass SIMD transform for ONE array it
/// touches, shared by the driver's emission gate (`x64/driver.rs`) and the
/// optimizing tier's veto (`x64/single_pass_only.rs`), so the two cannot
/// disagree about which detected loops are actually vectorised.
///
/// A SIMD pre-header replaces the per-element accesses of `arr[i]` for `i` in
/// `[entry_iv, bound)` with an unchecked batch loop, so for a
/// [`SimdLoopBound::Local`] bound it carries the obligation of a BCE elision:
/// `bound <= arr.length` and `iv >= 0`, proven statically (the bound provably
/// IS `arr.length` and the IV provably starts non-negative) or by a
/// speculative loop-header guard that survived de-specialisation.
///
/// A [`SimdLoopBound::ArrayLength`] loop is always covered: its pre-headers
/// check every array for null and for `length >= bound`, and `iv >= 0`,
/// before touching anything, and fall through to the original loop otherwise
/// (`emit_simd_int_array_sum` / `emit_simd_int_array_element_wise`).
///
/// `no_bce` is the caller's business (`CRATONVM_JIT_NO_BCE` refuses every
/// SIMD transform).
///
/// The non-negative start is the loop's ENTRY value
/// (`loop_analysis::constant_iv_init`: the last store before the header, on a
/// straight-line segment nothing lands in, into a loop with no side entry). It
/// used to be `bce::find_iv_nonneg_start`, which demands a single store to the
/// slot in the whole method, so of two sibling `for (int i = 0; ..)` loops
/// sharing javac's slot neither qualified (round 11 wave 2, lane x64loop;
/// `r11-iropt-bce-iv-start-refuses-a-reused-slot-FIXED-20260923.md`). The detectors
/// admit only a body whose one IV write is `iinc iv, 1`, so a non-negative
/// entry value keeps every batched index non-negative.
pub(super) fn simd_array_covered(
    code: &[u8],
    code_len: usize,
    guards: &[SpeculativeBCEGuard],
    header: usize,
    back_edge: usize,
    arr_local: usize,
    bound: SimdLoopBound,
    iv_local: usize,
) -> bool {
    match bound {
        SimdLoopBound::ArrayLength(_) => true,
        SimdLoopBound::Local(bound_local) => {
            let body_end = back_edge.saturating_add(bytecode_analysis::step(code, back_edge));
            (find_bound_arraylength_provenance(code, code_len, bound_local) == Some(arr_local)
                && crate::loop_analysis::constant_iv_init(
                    code, code_len, header, body_end, iv_local,
                )
                .is_some_and(|init| init >= 0))
                // A guard that reads a field hoist's slot (round 13 wave 3)
                // names a synthetic local, never a SIMD loop's array or
                // bound local; refused by name so the batch pre-header, which
                // loads both from their frame homes, can never rely on one.
                || guards.iter().any(|g| {
                    g.loop_header == header
                        && !g.reads_a_field_hoist()
                        && g.array_local == arr_local
                        && g.bound_local == bound_local
                })
        }
    }
}

/// [`simd_array_covered`] for the one array an int-array sum reads.
pub(super) fn simd_sum_covered(
    s: &SimdIntArraySum,
    code: &[u8],
    code_len: usize,
    guards: &[SpeculativeBCEGuard],
) -> bool {
    simd_array_covered(
        code,
        code_len,
        guards,
        s.header_pc,
        s.back_edge_pc,
        s.array_local,
        s.bound,
        s.iv_local,
    )
}

/// [`simd_array_covered`] for all three arrays of an element-wise loop.
pub(super) fn simd_element_wise_covered(
    e: &SimdArrayElementWise,
    code: &[u8],
    code_len: usize,
    guards: &[SpeculativeBCEGuard],
) -> bool {
    [e.out_local, e.a_local, e.b_local].into_iter().all(|arr| {
        simd_array_covered(
            code,
            code_len,
            guards,
            e.header_pc,
            e.back_edge_pc,
            arr,
            e.bound,
            e.iv_local,
        )
    })
}

/// A side-effect-free integer matrix dot-product loop.
///
/// `javac` emits this shape for the inner loop of the conventional
/// `int[][]` matrix multiply:
///
/// ```text
/// for (k = ...; k < bound; k++)
///     sum += a[row][k] * b[k][column];
/// ```
///
/// The generic bytecode emitter necessarily materializes the operand stack and
/// repeats array checks around every load.  The pre-header fast path keeps the
/// loop state in registers instead.  Every speculative shape check branches
/// back to the untouched scalar bytecode with the original `k`/`sum` frame
/// state, so null, jagged, short, negative-index, and zero-trip cases retain
/// exact Java behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MatrixDotLoop {
    pub(super) header_pc: usize,
    pub(super) back_edge_pc: usize,
    pub(super) iv_local: usize,
    pub(super) bound_local: usize,
    pub(super) acc_local: usize,
    pub(super) a_outer_local: usize,
    pub(super) a_row_local: usize,
    pub(super) b_outer_local: usize,
    pub(super) b_column_local: usize,
}

/// Detect the exact read-only matrix dot-product body documented on
/// [`MatrixDotLoop`].  Requiring the whole body (rather than recognizing a
/// subsequence) is what makes fallback restart safe: the skipped bytecodes have
/// no externally visible effects and write only `iv_local`/`acc_local`.
pub(super) fn detect_matrix_dot_loop(
    code: &[u8],
    header: usize,
    back_edge: usize,
    iv_local: usize,
) -> Option<MatrixDotLoop> {
    if code.get(back_edge).copied() != Some(0xa7) || back_edge + 2 >= code.len() {
        return None;
    }

    let take_iload = |pc: &mut usize| -> Option<usize> {
        let local = extract_iload_local(code, *pc)?;
        *pc += if code[*pc] == 0x15 { 2 } else { 1 };
        Some(local)
    };
    let take_aload = |pc: &mut usize| -> Option<usize> {
        let local = extract_aload_local(code, *pc)?;
        *pc += if code[*pc] == 0x19 { 2 } else { 1 };
        Some(local)
    };
    let take_istore = |pc: &mut usize| -> Option<usize> {
        let local = extract_istore_local(code, *pc)?;
        *pc += if code[*pc] == 0x36 { 2 } else { 1 };
        Some(local)
    };
    let take = |pc: &mut usize, opcode: u8| -> Option<()> {
        if code.get(*pc).copied()? != opcode {
            return None;
        }
        *pc += 1;
        Some(())
    };

    let mut pc = header;
    if take_iload(&mut pc)? != iv_local {
        return None;
    }
    let bound_local = take_iload(&mut pc)?;
    if code.get(pc).copied()? != 0xa2 {
        return None;
    }
    let exit_delta = i16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]) as isize;
    let exit_pc = (pc as isize).checked_add(exit_delta)?;
    if exit_pc <= back_edge as isize {
        return None;
    }
    pc += 3;

    let acc_local = take_iload(&mut pc)?;
    let a_outer_local = take_aload(&mut pc)?;
    let a_row_local = take_iload(&mut pc)?;
    take(&mut pc, 0x32)?; // aaload: a[row]
    if take_iload(&mut pc)? != iv_local {
        return None;
    }
    take(&mut pc, 0x2e)?; // iaload: a[row][k]

    let b_outer_local = take_aload(&mut pc)?;
    if take_iload(&mut pc)? != iv_local {
        return None;
    }
    take(&mut pc, 0x32)?; // aaload: b[k]
    let b_column_local = take_iload(&mut pc)?;
    take(&mut pc, 0x2e)?; // iaload: b[k][column]
    take(&mut pc, 0x68)?; // imul
    take(&mut pc, 0x60)?; // iadd
    if take_istore(&mut pc)? != acc_local {
        return None;
    }

    if pc + 2 >= back_edge
        || code.get(pc).copied()? != 0x84
        || *code.get(pc + 1)? as usize != iv_local
        || *code.get(pc + 2)? != 1
    {
        return None;
    }
    pc += 3;
    if pc != back_edge {
        return None;
    }

    let back_delta =
        i16::from_be_bytes([*code.get(back_edge + 1)?, *code.get(back_edge + 2)?]) as isize;
    if (back_edge as isize).checked_add(back_delta)? != header as isize {
        return None;
    }
    if acc_local == iv_local
        || bound_local == iv_local
        || bound_local == acc_local
        || a_row_local == iv_local
        || a_row_local == acc_local
        || b_column_local == iv_local
        || b_column_local == acc_local
    {
        return None;
    }

    Some(MatrixDotLoop {
        header_pc: header,
        back_edge_pc: back_edge,
        iv_local,
        bound_local,
        acc_local,
        a_outer_local,
        a_row_local,
        b_outer_local,
        b_column_local,
    })
}

/// Whether the additional int-array-sum body forms are admitted
/// ([`detect_int_array_sum_forms`] with `extra_forms = true`).
///
/// Default **on** since round 9 wave 4 (lane `x64core4`), switched by the
/// declared vectorization knob ([`VECTORIZE_FLAG`], i.e.
/// `CRATONVM_JIT=vectorize`); `CRATONVM_JIT_VECTORIZE=0` (or `false`/`off`/
/// `no`) is the kill switch. The historical detector only matched
/// `s = a[i] + s` with a `long` accumulator — a spelling `javac` produces only
/// when the source is written that way. The idiomatic `s += a[i]` (accumulator
/// loaded *first*) and every `int` accumulator were refused, so the AVX2
/// reduction almost never fired on real code. The extra forms reuse the same
/// emitter; admitting them also moves such methods under `single_pass_only`'s
/// IR-tier veto, which since wave 4 fires only when the vectorised loops
/// carry the method's loop work (`CoveredSimdLoops::dominate`).
///
/// Since round 9 wave 2 the same switch also admits the `a.length` loop header
/// ([`SimdLoopBound::ArrayLength`]) in both the sum and the element-wise
/// detector (the pre-headers guard it themselves).
///
/// Priced on the wave-3 binary (`docs/internal/jit-review-r9/NOTES-w4-x64core4.md`,
/// `LoopProbe`, isolated processes, outputs identical to HotSpot):
/// `int[]` sums over `a.length` 864 -> 125 ms (HotSpot 121), over a local
/// `n` 831 -> 118 ms (130), the mixed sum phase 953 -> 257 ms (129),
/// element-wise `c[i] = a[i] op b[i]` 666 -> 287 ms (57); byte/char/long
/// sums, fill/copy, `CratonBench` sieve/matrix unchanged (identical machine
/// code for the latter two).
///
/// Latched on first read: detectors run once per loop per compile, and a flag
/// read is expected to be cached.
fn simd_sum_forms_enabled() -> bool {
    simd_detector_switches().extra_forms
}

/// The declared vectorization knob: `CRATONVM_JIT_VECTORIZE=0` (or
/// `false`/`off`/`no`) turns the extra detector forms off. It lived in
/// `vec_emit.rs` until round 12 wave 3 deleted that never-called emitter.
pub(crate) const VECTORIZE_FLAG: &str = "CRATONVM_JIT_VECTORIZE";

/// Kill switch for the round-12 element-wise forms
/// ([`detect_int_array_element_wise_x1`]): the compound `a[i] op= b[i]` /
/// `a[i] op= k` and the loop-invariant operand `c[i] = a[i] op k` /
/// `c[i] = k op a[i]`. Default on; `CRATONVM_JIT_VEC_EWISE_FORMS=0` restores
/// the historical `c[i] = a[i] op b[i]` detector exactly. The forms also need
/// [`VECTORIZE_FLAG`], so `CRATONVM_JIT_VECTORIZE=0` turns them off too.
pub(crate) const EWISE_FORMS_FLAG: &str = "CRATONVM_JIT_VEC_EWISE_FORMS";

/// Both detector switches, read together on first use. One `OnceLock` for the
/// pair: `jit/tests/process_global_statics_ratchet.rs` counts statics, and a
/// second flag cache would be a second static for the same purpose.
#[derive(Debug, Clone, Copy)]
struct SimdDetectorSwitches {
    /// [`VECTORIZE_FLAG`]: the extra sum forms and the `a.length` header.
    extra_forms: bool,
    /// [`VECTORIZE_FLAG`] and [`EWISE_FORMS_FLAG`] both on.
    ewise_x1_forms: bool,
}

fn simd_detector_switches() -> SimdDetectorSwitches {
    static ON: std::sync::OnceLock<SimdDetectorSwitches> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let extra_forms = cratonvm_types::flags::runtime_flag_default_on(VECTORIZE_FLAG);
        SimdDetectorSwitches {
            extra_forms,
            ewise_x1_forms: extra_forms
                && cratonvm_types::flags::runtime_flag_default_on(EWISE_FORMS_FLAG),
        }
    })
}

/// Detect a vectorizable int-array-sum pattern in a loop body.
///
/// Always matched (the historical form):
///
/// ```text
/// aload arr ; iload iv ; iaload ; i2l ; lload sum ; ladd ; lstore sum   // s = a[i] + s
/// ```
///
/// Additionally, when the vectorization opt-in is on (see
/// `simd_sum_forms_enabled`):
///
/// ```text
/// lload sum ; aload arr ; iload iv ; iaload ; i2l ; ladd ; lstore sum   // long s += a[i]
/// iload sum ; aload arr ; iload iv ; iaload ; iadd ; istore sum         // int  s += a[i]
/// aload arr ; iload iv ; iaload ; iload sum ; iadd ; istore sum         // int  s = a[i] + s
/// ```
///
/// each followed by `iinc iv 1 ; goto header`.
pub(super) fn detect_int_array_sum(
    code: &[u8],
    header: usize,
    back_edge: usize,
    iv_local: usize,
) -> Option<SimdIntArraySum> {
    detect_int_array_sum_forms(code, header, back_edge, iv_local, simd_sum_forms_enabled())
}

/// [`detect_int_array_sum`] with the extra-forms switch as an argument, so the
/// forms can be tested without touching process-global flag state.
///
/// Every form is semantically `acc = acc + (widen) a[iv]`: `iadd`/`ladd` are
/// commutative in two's complement, so the operand order on the stack does not
/// change the value, only the bytecode spelling.
pub(super) fn detect_int_array_sum_forms(
    code: &[u8],
    header: usize,
    back_edge: usize,
    iv_local: usize,
    extra_forms: bool,
) -> Option<SimdIntArraySum> {
    if code.get(back_edge).copied() != Some(0xa7) {
        return None;
    }
    let take_iload = |pc: &mut usize| -> Option<usize> {
        let local = extract_iload_local(code, *pc)?;
        *pc += if code[*pc] == 0x15 { 2 } else { 1 };
        Some(local)
    };
    let take_aload = |pc: &mut usize| -> Option<usize> {
        let local = extract_aload_local(code, *pc)?;
        *pc += if code[*pc] == 0x19 { 2 } else { 1 };
        Some(local)
    };
    let take = |pc: &mut usize, opcode: u8| -> Option<()> {
        if code.get(*pc).copied()? != opcode {
            return None;
        }
        *pc += 1;
        Some(())
    };

    // Header: iload iv ; iload bound ; if_icmpge <exit>
    //     or: iload iv ; aload a ; arraylength ; if_icmpge <exit>  (opt-in)
    let mut pc = header;
    if take_iload(&mut pc)? != iv_local {
        return None;
    }
    let bound = take_simd_loop_bound(code, &mut pc, extra_forms)?;
    let bound_local = bound.walk_local();
    if code.get(pc).copied()? != 0xa2 || pc + 3 > back_edge {
        return None;
    }
    pc += 3;

    // `aload arr ; iload iv ; iaload`, the element load every form shares.
    let take_element = |pc: &mut usize| -> Option<usize> {
        let array_local = take_aload(&mut *pc)?;
        if take_iload(&mut *pc)? != iv_local {
            return None;
        }
        take(&mut *pc, 0x2e)?; // iaload
        Some(array_local)
    };

    let (array_local, acc_local, acc_is_long) = match code.get(pc).copied()? {
        // lload sum first: `long s += a[i]`.
        0x16 | 0x1e..=0x21 if extra_forms => {
            let acc = extract_lload_local(code, pc)?;
            pc += if code[pc] == 0x16 { 2 } else { 1 };
            let array = take_element(&mut pc)?;
            take(&mut pc, 0x85)?; // i2l
            take(&mut pc, 0x61)?; // ladd
            (array, acc, true)
        }
        // iload sum first: `int s += a[i]`.
        0x15 | 0x1a..=0x1d if extra_forms => {
            let acc = take_iload(&mut pc)?;
            let array = take_element(&mut pc)?;
            take(&mut pc, 0x60)?; // iadd
            (array, acc, false)
        }
        // Element first: `s = a[i] + s`, long (historical) or int (extra).
        _ => {
            let array = take_element(&mut pc)?;
            if code.get(pc).copied()? == 0x85 {
                pc += 1; // i2l
                let acc = extract_lload_local(code, pc)?;
                pc += if code[pc] == 0x16 { 2 } else { 1 };
                take(&mut pc, 0x61)?; // ladd
                (array, acc, true)
            } else if extra_forms {
                let acc = take_iload(&mut pc)?;
                take(&mut pc, 0x60)?; // iadd
                (array, acc, false)
            } else {
                return None;
            }
        }
    };

    // The store must write back the same accumulator the add read.
    let store_local = if acc_is_long {
        let l = extract_lstore_local(code, pc)?;
        pc += if code[pc] == 0x37 { 2 } else { 1 };
        l
    } else {
        let l = extract_istore_local(code, pc)?;
        pc += if code[pc] == 0x36 { 2 } else { 1 };
        l
    };
    if store_local != acc_local {
        return None;
    }

    // iinc iv, 1 ; goto header
    if pc + 3 != back_edge
        || code.get(pc).copied()? != 0x84
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        || *code.get(pc + 1)? as usize != iv_local
        || *code.get(pc + 2)? != 0x01
    {
        return None;
    }
    let back_delta =
        i16::from_be_bytes([*code.get(back_edge + 1)?, *code.get(back_edge + 2)?]) as isize;
    if (back_edge as isize).checked_add(back_delta)? != header as isize {
        return None;
    }

    // The emitter reads `iv`/`bound` once and writes only `acc` and `iv`, so
    // the accumulator (both slots of a `long`) must not BE the induction
    // variable or the bound. For a `long` the verifier already forbids it; for
    // an `int` accumulator `istore iv` is well-typed bytecode and would turn
    // the loop into something the pre-header does not compute.
    let acc_owns = |slot: usize| slot == acc_local || (acc_is_long && slot == acc_local + 1);
    if acc_owns(iv_local) || acc_owns(bound_local) || acc_owns(array_local) {
        return None;
    }

    Some(SimdIntArraySum {
        header_pc: header,
        back_edge_pc: back_edge,
        iv_local,
        acc_local,
        array_local,
        bound_local,
        bound,
        acc_is_long,
    })
}

/// Extract local index from an lload instruction at pc.
pub(super) fn extract_lload_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x1e => Some(0), // lload_0
        0x1f => Some(1), // lload_1
        0x20 => Some(2), // lload_2
        0x21 => Some(3), // lload_3
        // Cast: non-negative index/count to usize
        0x16 => code.get(pc + 1).map(|&b| b as usize), // lload
        _ => None,
    }
}

/// Extract local index from an lstore instruction at pc.
pub(super) fn extract_lstore_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x3f => Some(0), // lstore_0
        0x40 => Some(1), // lstore_1
        0x41 => Some(2), // lstore_2
        0x42 => Some(3), // lstore_3
        // Cast: non-negative index/count to usize
        0x37 => code.get(pc + 1).map(|&b| b as usize), // lstore
        _ => None,
    }
}

/// Extract local index from an istore instruction at pc.
pub(super) fn extract_istore_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x3b => Some(0),                               // istore_0
        0x3c => Some(1),                               // istore_1
        0x3d => Some(2),                               // istore_2
        0x3e => Some(3),                               // istore_3
        0x36 => code.get(pc + 1).map(|&b| b as usize), // istore
        _ => None,
    }
}

/// Extract local index from a dload instruction at pc.
///
/// Test-only since round 11 wave 3: the FP-sum detector that used this and
/// [`extract_dstore_local`] was retired; only `x64/tests.rs` calls them.
#[cfg(test)]
pub(super) fn extract_dload_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x26 => Some(0), // dload_0
        0x27 => Some(1), // dload_1
        0x28 => Some(2), // dload_2
        0x29 => Some(3), // dload_3
        // Cast: non-negative index/count to usize
        0x18 => code.get(pc + 1).map(|&b| b as usize), // dload
        _ => None,
    }
}

/// Extract local index from a dstore instruction at pc.
#[cfg(test)]
pub(super) fn extract_dstore_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x47 => Some(0), // dstore_0
        0x48 => Some(1), // dstore_1
        0x49 => Some(2), // dstore_2
        0x4a => Some(3), // dstore_3
        // Cast: non-negative index/count to usize
        0x39 => code.get(pc + 1).map(|&b| b as usize), // dstore
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// T5.2.15 — SuperWord / element-wise SIMD detection
// ---------------------------------------------------------------------------

/// Operation that joins the two source vectors in an element-wise loop.
///
/// Expressed as the bytecode opcode of the arithmetic that appears
/// between the two `iaload`s and the `iastore` — the JIT maps this to
/// `PADDD` / `PSUBD` / `PMULLD` when lowering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ElementWiseOp {
    /// iadd (0x60) → PADDD
    Add,
    /// isub (0x64) → PSUBD
    Sub,
    /// imul (0x68) → PMULLD (SSE4.1 or AVX2)
    Mul,
    /// iand (0x7E) → PAND
    And,
    /// ior  (0x80) → POR
    Or,
    /// ixor (0x82) → PXOR
    Xor,
}

/// Information about a vectorizable int-array element-wise loop.
///
/// Matches the pattern:
///
/// ```text
/// for (i = 0; i < n; i++) out[i] = a[i] OP b[i];
/// ```
///
/// where `OP` is one of `iadd`, `isub`, `imul`, `iand`, `ior`, `ixor`.
/// The detector also accepts the simpler form `a[i] OP b[i]` when the
/// result is stored back into `a` (in-place).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct SimdArrayElementWise {
    /// Bytecode PC of the loop header.
    pub header_pc: usize,
    /// Bytecode PC of the back-edge.
    pub back_edge_pc: usize,
    /// Local index of the induction variable.
    pub iv_local: usize,
    /// Local index of the destination array.
    pub out_local: usize,
    /// Local index of source array A.
    pub a_local: usize,
    /// Local index of source array B.
    pub b_local: usize,
    /// The local the single-pass walk loads into R11 before the pre-header —
    /// `bound.walk_local()`: the `int` bound, or for an `a.length` header the
    /// array whose length is the bound (see [`SimdIntArraySum::bound_local`]).
    pub bound_local: usize,
    /// Where the loop's exclusive upper bound comes from.
    pub bound: SimdLoopBound,
    /// Operation to perform element-wise.
    pub op: ElementWiseOp,
    /// The second operand. [`ElementWiseRhs::Array`] is `b[i]` from
    /// `b_local`; the invariant forms (round 12 wave 3,
    /// [`detect_int_array_element_wise_x1`]) set `b_local` to `a_local`, so
    /// every per-array rule that walks `[out, a, b]` (the coverage gate, the
    /// pre-header's guards) still names only arrays the loop touches.
    pub rhs: ElementWiseRhs,
    /// The invariant operand is the LEFT operand: `c[i] = k op a[i]`. Only
    /// `isub` is not commutative, so only it reads this. Always `false` for
    /// [`ElementWiseRhs::Array`].
    pub invariant_first: bool,
}

/// The second operand of an element-wise loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElementWiseRhs {
    /// `b[i]`: the array in `SimdArrayElementWise::b_local`.
    Array,
    /// `iload k` of an `int` local the body never writes (the detector admits
    /// no store but `iinc iv`, and refuses `k == iv`). The pre-header reads it
    /// once per pass.
    Local(usize),
    /// `iconst_*` / `bipush` / `sipush`.
    Const(i32),
}

/// Try to detect an int-array element-wise pattern in a loop body.
///
/// The recognized shape is:
///
/// ```text
/// header: iload iv ; iload bound ; if_icmpge exit
/// body:   aload out ; iload iv ;
///         aload a ; iload iv ; iaload ;
///         aload b ; iload iv ; iaload ;
///         i{add,sub,mul,and,or,xor} ;
///         iastore ;
///         iinc iv 1 ;
///         goto header
/// ```
///
/// Returns `Some` on a match; `None` otherwise. The caller stores the
/// result on the `Compiler` for downstream SIMD emission.
///
/// Since round 12 wave 3 a loop the historical shape refuses is offered to
/// [`detect_int_array_element_wise_x1`] (the compound and invariant-operand
/// forms) under [`EWISE_FORMS_FLAG`]. The historical detector runs first, so
/// every loop it matched is matched exactly as before.
#[allow(dead_code)]
pub(crate) fn detect_int_array_element_wise(
    code: &[u8],
    header: usize,
    back_edge: usize,
    iv_local: usize,
) -> Option<SimdArrayElementWise> {
    let switches = simd_detector_switches();
    detect_int_array_element_wise_forms(code, header, back_edge, iv_local, switches.extra_forms)
        .or_else(|| {
            if switches.ewise_x1_forms {
                detect_int_array_element_wise_x1(
                    code,
                    header,
                    back_edge,
                    iv_local,
                    switches.extra_forms,
                )
            } else {
                None
            }
        })
}

/// [`detect_int_array_element_wise`] with the `a.length` header switch as an
/// argument (`allow_array_length`), so it can be tested without touching
/// process-global flag state. With it on, the header may also be
/// `iload iv ; aload l ; arraylength ; if_icmpge exit` for any array local `l`
/// (one of the three or a fourth); the pre-header then guards all three arrays
/// against `l.length` itself.
pub(crate) fn detect_int_array_element_wise_forms(
    code: &[u8],
    header: usize,
    back_edge: usize,
    iv_local: usize,
    allow_array_length: bool,
) -> Option<SimdArrayElementWise> {
    if back_edge >= code.len() || code[back_edge] != 0xa7 {
        return None;
    }
    let back_edge_end = back_edge + 3;
    let mut pc = header;

    // Header: iload iv ; iload bound ; if_icmpge exit
    //     or: iload iv ; aload l ; arraylength ; if_icmpge exit  (opt-in)
    let iv_check = extract_iload_local(code, pc)?;
    if iv_check != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    let bound = take_simd_loop_bound(code, &mut pc, allow_array_length)?;
    let bound_local = bound.walk_local();

    if pc + 2 >= back_edge_end || code.get(pc).copied()? != 0xa2 {
        return None;
    }
    pc += 3;

    // Body: aload out ; iload iv
    let out_local = extract_aload_local(code, pc)?;
    pc += if code[pc] == 0x19 { 2 } else { 1 };

    let iv2 = extract_iload_local(code, pc)?;
    if iv2 != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    // aload a ; iload iv ; iaload
    let a_local = extract_aload_local(code, pc)?;
    pc += if code[pc] == 0x19 { 2 } else { 1 };
    let iv3 = extract_iload_local(code, pc)?;
    if iv3 != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };
    if pc >= back_edge_end || code[pc] != 0x2e {
        return None; // iaload
    }
    pc += 1;

    // aload b ; iload iv ; iaload
    let b_local = extract_aload_local(code, pc)?;
    pc += if code[pc] == 0x19 { 2 } else { 1 };
    let iv4 = extract_iload_local(code, pc)?;
    if iv4 != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };
    if pc >= back_edge_end || code[pc] != 0x2e {
        return None;
    }
    pc += 1;

    // Element-wise op
    let op = match code.get(pc).copied()? {
        0x60 => ElementWiseOp::Add,
        0x64 => ElementWiseOp::Sub,
        0x68 => ElementWiseOp::Mul,
        0x7E => ElementWiseOp::And,
        0x80 => ElementWiseOp::Or,
        0x82 => ElementWiseOp::Xor,
        _ => return None,
    };
    pc += 1;

    // iastore
    if pc >= back_edge_end || code[pc] != 0x4F {
        return None;
    }
    pc += 1;

    // iinc iv, 1 ; goto header
    if pc + 2 >= back_edge_end
        || code[pc] != 0x84
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        || code[pc + 1] as usize != iv_local
        || code[pc + 2] != 0x01
        || pc + 3 != back_edge
    {
        return None;
    }

    Some(SimdArrayElementWise {
        header_pc: header,
        back_edge_pc: back_edge,
        iv_local,
        out_local,
        a_local,
        b_local,
        bound_local,
        bound,
        op,
        rhs: ElementWiseRhs::Array,
        invariant_first: false,
    })
}

/// A loop-invariant `int` push at `pc`: `iload k` (never the induction
/// variable), `iconst_m1..5`, `bipush` or `sipush`. Returns the operand and
/// the pc after it.
fn take_ewise_invariant(
    code: &[u8],
    pc: usize,
    iv_local: usize,
) -> Option<(ElementWiseRhs, usize)> {
    let op = *code.get(pc)?;
    match op {
        0x02..=0x08 => Some((ElementWiseRhs::Const(i32::from(op) - 3), pc + 1)),
        // Cast: the signed bipush operand byte.
        0x10 => Some((ElementWiseRhs::Const(i32::from(*code.get(pc + 1)? as i8)), pc + 2)),
        0x11 => {
            let v = i16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]);
            Some((ElementWiseRhs::Const(i32::from(v)), pc + 3))
        }
        0x15 | 0x1a..=0x1d => {
            let k = extract_iload_local(code, pc)?;
            if k == iv_local {
                return None; // `a[i] + i` is not invariant
            }
            Some((ElementWiseRhs::Local(k), pc + if op == 0x15 { 2 } else { 1 }))
        }
        _ => None,
    }
}

/// X1 of `docs/internal/jit-proposals/jit-r12-x64opt-proposals-RETIRED-20260928.md`: the element-wise
/// shapes `detect_int_array_element_wise_forms` does not match, for the same
/// pre-header (`emit_simd_int_array_element_wise`).
///
/// ```text
/// header: iload iv ; <bound> ; if_icmpge exit            (as the historical form)
/// body:   aload out ; iload iv ;
///           dup2 ; iaload                                 A = out  (javac `op=`)
///         | aload a ; iload iv ; iaload                   A = a
///         | <k> ; aload a ; iload iv ; iaload             k first
///         then, unless k came first:
///           aload b ; iload iv ; iaload                   B = b[i]
///         | <k>                                           B = k
///         i{add,sub,mul,and,or,xor} ; iastore ;
///         iinc iv 1 ; goto header
/// ```
///
/// `<k>` is [`take_ewise_invariant`]. The historical `c[i] = a[i] op b[i]`
/// (no `dup2`, both operands arrays) is left to the historical detector, which
/// runs first; this one refuses it so the two never both claim a loop.
///
/// Soundness is the historical form's: every access is at index `iv`, so a
/// lane reads `a[i]` (and `b[i]`) before it writes `out[i]` whatever aliases
/// what; the body writes no local but `iv`, so `k` is invariant; and the
/// pre-header self-guards (null, `iv >= 0`, `bound <= length` for every
/// array) fall back to the untouched scalar loop, which throws at the right
/// element after exactly the scalar loop's partial writes.
pub(crate) fn detect_int_array_element_wise_x1(
    code: &[u8],
    header: usize,
    back_edge: usize,
    iv_local: usize,
    allow_array_length: bool,
) -> Option<SimdArrayElementWise> {
    if code.get(back_edge).copied() != Some(0xa7) {
        return None;
    }
    let back_delta =
        i16::from_be_bytes([*code.get(back_edge + 1)?, *code.get(back_edge + 2)?]) as isize;
    if (back_edge as isize).checked_add(back_delta)? != header as isize {
        return None;
    }
    let take_iv = |pc: &mut usize| -> Option<()> {
        if extract_iload_local(code, *pc)? != iv_local {
            return None;
        }
        *pc += if code.get(*pc).copied()? == 0x15 { 2 } else { 1 };
        Some(())
    };
    let take_aload = |pc: &mut usize| -> Option<usize> {
        let local = extract_aload_local(code, *pc)?;
        *pc += if code.get(*pc).copied()? == 0x19 { 2 } else { 1 };
        Some(local)
    };
    let take = |pc: &mut usize, opcode: u8| -> Option<()> {
        if code.get(*pc).copied()? != opcode {
            return None;
        }
        *pc += 1;
        Some(())
    };
    // `aload x ; iload iv ; iaload` -> x
    let take_element = |pc: &mut usize| -> Option<usize> {
        let array = take_aload(&mut *pc)?;
        take_iv(&mut *pc)?;
        take(&mut *pc, 0x2e)?;
        Some(array)
    };

    // Header.
    let mut pc = header;
    take_iv(&mut pc)?;
    let bound = take_simd_loop_bound(code, &mut pc, allow_array_length)?;
    let bound_local = bound.walk_local();
    if code.get(pc).copied()? != 0xa2 {
        return None;
    }
    let exit_delta = i16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]) as isize;
    if (pc as isize).checked_add(exit_delta)? <= back_edge as isize {
        return None; // the exit must leave the loop
    }
    pc += 3;

    // `aload out ; iload iv`
    let out_local = take_aload(&mut pc)?;
    take_iv(&mut pc)?;

    // The left operand.
    let mut compound = false;
    let (a_local, first_invariant) = if code.get(pc).copied()? == 0x5c {
        pc += 1; // dup2: [out, iv] -> [out, iv, out, iv]
        take(&mut pc, 0x2e)?;
        compound = true;
        (out_local, None)
    } else if let Some((k, next)) = take_ewise_invariant(code, pc, iv_local) {
        pc = next;
        (take_element(&mut pc)?, Some(k))
    } else {
        (take_element(&mut pc)?, None)
    };

    // The right operand, unless the invariant came first.
    let (b_local, rhs, invariant_first) = match first_invariant {
        Some(k) => (a_local, k, true),
        None => {
            if let Some((k, next)) = take_ewise_invariant(code, pc, iv_local) {
                pc = next;
                (a_local, k, false)
            } else {
                let b = take_element(&mut pc)?;
                if !compound {
                    // `c[i] = a[i] op b[i]`: the historical detector's shape.
                    return None;
                }
                (b, ElementWiseRhs::Array, false)
            }
        }
    };

    let op = match code.get(pc).copied()? {
        0x60 => ElementWiseOp::Add,
        0x64 => ElementWiseOp::Sub,
        0x68 => ElementWiseOp::Mul,
        0x7E => ElementWiseOp::And,
        0x80 => ElementWiseOp::Or,
        0x82 => ElementWiseOp::Xor,
        _ => return None,
    };
    pc += 1;
    take(&mut pc, 0x4f)?; // iastore

    // iinc iv, 1 ; goto header
    if code.get(pc).copied()? != 0x84
        // Widening: u8 -> usize (bytecode operand byte)
        || *code.get(pc + 1)? as usize != iv_local
        || *code.get(pc + 2)? != 0x01
        || pc + 3 != back_edge
    {
        return None;
    }

    Some(SimdArrayElementWise {
        header_pc: header,
        back_edge_pc: back_edge,
        iv_local,
        out_local,
        a_local,
        b_local,
        bound_local,
        bound,
        op,
        rhs,
        invariant_first,
    })
}

#[cfg(test)]
mod int_array_sum_detector_tests {
    use super::*;

    /// `int sum(int[] a, int n) { int s = 0; for (int i = 0; i < n; i++) s += a[i]; return s; }`
    ///
    /// Locals: 0 = a, 1 = n, 2 = s, 3 = i. This is `javac`'s spelling of
    /// `s += a[i]`: the accumulator is loaded *before* the element.
    fn int_plus_equals() -> Vec<u8> {
        vec![
            0x03, //  0: iconst_0
            0x3d, //  1: istore_2        s = 0
            0x03, //  2: iconst_0
            0x3e, //  3: istore_3        i = 0
            0x1d, //  4: iload_3         <- header
            0x1b, //  5: iload_1
            0xa2, 0x00, 0x0f, //  6: if_icmpge +15 -> 21
            0x1c, //  9: iload_2         s
            0x2a, // 10: aload_0         a
            0x1d, // 11: iload_3         i
            0x2e, // 12: iaload
            0x60, // 13: iadd
            0x3d, // 14: istore_2
            0x84, 0x03, 0x01, // 15: iinc 3, 1
            0xa7, 0xff, 0xf2, // 18: goto -14 -> 4
            0x1c, // 21: iload_2
            0xac, // 22: ireturn
        ]
    }

    /// `long s = 0; for (int i = 0; i < n; i++) s += a[i];`
    ///
    /// Locals: 0 = a, 1 = n, 2-3 = s, 4 = i.
    fn long_plus_equals() -> Vec<u8> {
        vec![
            0x09, //  0: lconst_0
            0x41, //  1: lstore_2
            0x03, //  2: iconst_0
            0x36, 0x04, //  3: istore 4
            0x15, 0x04, //  5: iload 4       <- header
            0x1b, //  7: iload_1
            0xa2, 0x00, 0x11, //  8: if_icmpge +17 -> 25
            0x20, // 11: lload_2        s
            0x2a, // 12: aload_0        a
            0x15, 0x04, // 13: iload 4
            0x2e, // 15: iaload
            0x85, // 16: i2l
            0x61, // 17: ladd
            0x41, // 18: lstore_2
            0x84, 0x04, 0x01, // 19: iinc 4, 1
            0xa7, 0xff, 0xef, // 22: goto -17 -> 5
            0x20, // 25: lload_2
            0xad, // 26: lreturn
        ]
    }

    /// The historical `s = a[i] + s` long form — matched with or without the
    /// switch.
    fn long_element_first() -> Vec<u8> {
        let mut code = long_plus_equals();
        // 11..=17 becomes: aload_0 ; iload 4 ; iaload ; i2l ; lload_2 ; ladd
        code[11..18].copy_from_slice(&[0x2a, 0x15, 0x04, 0x2e, 0x85, 0x20, 0x61]);
        code
    }

    #[test]
    fn the_historical_long_form_is_matched_with_the_switch_off() {
        let code = long_element_first();
        let info = detect_int_array_sum_forms(&code, 5, 22, 4, false).expect("historical form");
        assert_eq!(
            (
                info.acc_local,
                info.array_local,
                info.bound_local,
                info.acc_is_long
            ),
            (2, 0, 1, true)
        );
        assert!(detect_int_array_sum_forms(&code, 5, 22, 4, true).is_some());
    }

    #[test]
    fn the_javac_plus_equals_forms_need_the_switch() {
        let code = long_plus_equals();
        assert!(
            detect_int_array_sum_forms(&code, 5, 22, 4, false).is_none(),
            "default-off: the historical detector's verdict is unchanged"
        );
        let info = detect_int_array_sum_forms(&code, 5, 22, 4, true).expect("long s += a[i]");
        assert_eq!(
            (
                info.acc_local,
                info.array_local,
                info.bound_local,
                info.acc_is_long
            ),
            (2, 0, 1, true)
        );

        let code = int_plus_equals();
        assert!(detect_int_array_sum_forms(&code, 4, 18, 3, false).is_none());
        let info = detect_int_array_sum_forms(&code, 4, 18, 3, true).expect("int s += a[i]");
        assert_eq!(
            (
                info.acc_local,
                info.array_local,
                info.bound_local,
                info.acc_is_long
            ),
            (2, 0, 1, false)
        );
    }

    #[test]
    fn the_int_element_first_form_is_matched_under_the_switch() {
        let mut code = int_plus_equals();
        // 9..=13 becomes: aload_0 ; iload_3 ; iaload ; iload_2 ; iadd
        code[9..14].copy_from_slice(&[0x2a, 0x1d, 0x2e, 0x1c, 0x60]);
        assert!(detect_int_array_sum_forms(&code, 4, 18, 3, false).is_none());
        let info = detect_int_array_sum_forms(&code, 4, 18, 3, true).expect("s = a[i] + s");
        assert_eq!((info.acc_local, info.acc_is_long), (2, false));
    }

    /// `istore i` / `istore n` are well-typed bytecode for an `int`
    /// accumulator. The pre-header reads `iv`/`bound` once and writes back
    /// only `acc` and `iv`, so either alias would be a miscompile.
    #[test]
    fn an_accumulator_that_is_the_iv_or_the_bound_is_refused() {
        let mut code = int_plus_equals();
        code[9] = 0x1d; // iload_3  (i)
        code[14] = 0x3e; // istore_3 (i)
        assert!(detect_int_array_sum_forms(&code, 4, 18, 3, true).is_none());

        let mut code = int_plus_equals();
        code[9] = 0x1b; // iload_1  (n)
        code[14] = 0x3c; // istore_1 (n)
        assert!(detect_int_array_sum_forms(&code, 4, 18, 3, true).is_none());
    }

    #[test]
    fn a_store_to_a_different_local_is_refused() {
        let mut code = int_plus_equals();
        code[14] = 0x3b; // istore_0 — not the accumulator that was read
        assert!(detect_int_array_sum_forms(&code, 4, 18, 3, true).is_none());
    }

    /// `int sum(int[] a) { int s = 0; for (int i = 0; i < a.length; i++) s += a[i]; return s; }`
    ///
    /// Locals: 0 = a, 1 = s, 2 = i. The idiomatic header: the bound is
    /// re-read from the array every iteration and never sits in a local.
    fn int_sum_over_length() -> Vec<u8> {
        vec![
            0x03, //  0: iconst_0
            0x3c, //  1: istore_1        s = 0
            0x03, //  2: iconst_0
            0x3d, //  3: istore_2        i = 0
            0x1c, //  4: iload_2         <- header
            0x2a, //  5: aload_0
            0xbe, //  6: arraylength
            0xa2, 0x00, 0x0f, //  7: if_icmpge +15 -> 22
            0x1b, // 10: iload_1         s
            0x2a, // 11: aload_0         a
            0x1c, // 12: iload_2         i
            0x2e, // 13: iaload
            0x60, // 14: iadd
            0x3c, // 15: istore_1
            0x84, 0x02, 0x01, // 16: iinc 2, 1
            0xa7, 0xff, 0xf1, // 19: goto -15 -> 4
            0x1b, // 22: iload_1
            0xac, // 23: ireturn
        ]
    }

    #[test]
    fn the_array_length_header_is_matched_under_the_switch() {
        let code = int_sum_over_length();
        assert!(
            detect_int_array_sum_forms(&code, 4, 19, 2, false).is_none(),
            "default-off: the `a.length` header stays refused without the opt-in"
        );
        let info = detect_int_array_sum_forms(&code, 4, 19, 2, true).expect("a.length header");
        assert_eq!(info.bound, SimdLoopBound::ArrayLength(0));
        // The walk loads `bound_local` into R11: for this header that must be
        // the ARRAY, which the pre-header turns into its length.
        assert_eq!(info.bound_local, 0);
        assert_eq!(
            (info.array_local, info.acc_local, info.acc_is_long),
            (0, 1, false)
        );
    }

    /// The `iload bound` header keeps reporting a `Local` bound, so the
    /// driver's coverage proof for it is unchanged.
    #[test]
    fn an_iload_header_reports_a_local_bound() {
        let code = int_plus_equals();
        let info = detect_int_array_sum_forms(&code, 4, 18, 3, true).expect("int s += a[i]");
        assert_eq!(info.bound, SimdLoopBound::Local(1));
        assert_eq!(info.bound_local, 1);
    }

    /// `aload a` NOT followed by `arraylength` is not a bound.
    #[test]
    fn an_aload_without_arraylength_is_not_a_bound() {
        let mut code = int_sum_over_length();
        code[6] = 0x00; // nop instead of arraylength
        assert!(detect_int_array_sum_forms(&code, 4, 19, 2, true).is_none());
    }

    /// `static void add(int[] out, int[] a, int[] b) { for (int i = 0; i < a.length; i++) out[i] = a[i] + b[i]; }`
    ///
    /// Locals: 0 = out, 1 = a, 2 = b, 3 = i.
    fn element_wise_over_length() -> Vec<u8> {
        vec![
            0x03, //  0: iconst_0
            0x3e, //  1: istore_3        i = 0
            0x1d, //  2: iload_3         <- header
            0x2b, //  3: aload_1
            0xbe, //  4: arraylength
            0xa2, 0x00, 0x13, //  5: if_icmpge +19 -> 24
            0x2a, //  8: aload_0         out
            0x1d, //  9: iload_3
            0x2b, // 10: aload_1         a
            0x1d, // 11: iload_3
            0x2e, // 12: iaload
            0x2c, // 13: aload_2         b
            0x1d, // 14: iload_3
            0x2e, // 15: iaload
            0x60, // 16: iadd
            0x4f, // 17: iastore
            0x84, 0x03, 0x01, // 18: iinc 3, 1
            0xa7, 0xff, 0xed, // 21: goto -19 -> 2
            0xb1, // 24: return
        ]
    }

    #[test]
    fn the_element_wise_array_length_header_is_matched_under_the_switch() {
        let code = element_wise_over_length();
        assert!(detect_int_array_element_wise_forms(&code, 2, 21, 3, false).is_none());
        let e =
            detect_int_array_element_wise_forms(&code, 2, 21, 3, true).expect("a.length header");
        assert_eq!(e.bound, SimdLoopBound::ArrayLength(1));
        assert_eq!(e.bound_local, 1);
        assert_eq!((e.out_local, e.a_local, e.b_local), (0, 1, 2));
        assert_eq!(e.op, ElementWiseOp::Add);
    }

    /// The coverage predicate the driver and the IR-tier veto share.
    ///
    /// An `a.length` loop is covered by its own pre-header guards; an
    /// `iload n` loop over a parameter `n` is covered only by a matching
    /// speculative guard (or the static proof, which a parameter bound cannot
    /// have).
    #[test]
    fn the_shared_coverage_predicate_matches_the_driver_rule() {
        let code = int_sum_over_length();
        let s = detect_int_array_sum_forms(&code, 4, 19, 2, true).expect("a.length header");
        assert!(simd_sum_covered(&s, &code, code.len(), &[]));

        let code = int_plus_equals();
        let s = detect_int_array_sum_forms(&code, 4, 18, 3, true).expect("int s += a[i]");
        assert!(
            !simd_sum_covered(&s, &code, code.len(), &[]),
            "a parameter bound with no guard is not covered: the driver emits it scalar, \
             so the veto must not fire for it either"
        );
        let guard = SpeculativeBCEGuard {
            loop_header: 4,
            array_local: 0,
            bound_local: 1,
            iv_local: 3,
            covered_pcs: vec![12],
            inclusive: false,
            step_local: None,
            array_field_hoist: None,
            bound_field_hoist: None,
        };
        assert!(simd_sum_covered(
            &s,
            &code,
            code.len(),
            std::slice::from_ref(&guard)
        ));
        let wrong_array = SpeculativeBCEGuard {
            array_local: 5,
            ..guard.clone()
        };
        assert!(!simd_sum_covered(&s, &code, code.len(), &[wrong_array]));

        let code = element_wise_over_length();
        let e =
            detect_int_array_element_wise_forms(&code, 2, 21, 3, true).expect("a.length header");
        assert!(simd_element_wise_covered(&e, &code, code.len(), &[]));
    }

    /// `int twice(int[] a) { int n = a.length; int s = 0;
    ///  for (int i = 0; i < n; i++) s += a[i];
    ///  for (int i = 0; i < n; i++) s += a[i]; return s; }`
    ///
    /// Locals: 0 = a, 1 = n, 2 = s, 3 = i (both loops: javac reuses the slot).
    fn sibling_sums_sharing_a_slot() -> Vec<u8> {
        vec![
            0x2a, //  0: aload_0
            0xbe, //  1: arraylength
            0x3c, //  2: istore_1        n = a.length
            0x03, //  3: iconst_0
            0x3d, //  4: istore_2        s = 0
            0x03, //  5: iconst_0
            0x3e, //  6: istore_3        i = 0
            0x1d, //  7: iload_3         <- header 1
            0x1b, //  8: iload_1
            0xa2, 0x00, 0x0f, //  9: if_icmpge -> 24
            0x1c, 0x2a, 0x1d, 0x2e, 0x60, 0x3d, // 12: s += a[i]
            0x84, 0x03, 0x01, // 18: iinc 3, 1
            0xa7, 0xff, 0xf2, // 21: goto -> 7
            0x03, // 24: iconst_0
            0x3e, // 25: istore_3        i = 0 (same slot)
            0x1d, // 26: iload_3         <- header 2
            0x1b, // 27: iload_1
            0xa2, 0x00, 0x0f, // 28: if_icmpge -> 43
            0x1c, 0x2a, 0x1d, 0x2e, 0x60, 0x3d, // 31: s += a[i]
            0x84, 0x03, 0x01, // 37: iinc 3, 1
            0xa7, 0xff, 0xf2, // 40: goto -> 26
            0x1c, // 43: iload_2
            0xac, // 44: ireturn
        ]
    }

    /// Round 11 wave 2 (`r11-iropt-bce-iv-start-refuses-a-reused-slot`): the
    /// static coverage proof used to demand ONE store to the IV slot in the
    /// whole method, so neither sibling loop qualified. Each loop's own entry
    /// value is what the batch needs.
    #[test]
    fn sibling_loops_sharing_the_iv_slot_are_each_statically_covered() {
        let code = sibling_sums_sharing_a_slot();
        assert_eq!(code.len(), 45);
        let first = detect_int_array_sum_forms(&code, 7, 21, 3, true).expect("first loop");
        let second = detect_int_array_sum_forms(&code, 26, 40, 3, true).expect("second loop");
        assert_eq!(first.bound, SimdLoopBound::Local(1));
        assert!(simd_sum_covered(&first, &code, code.len(), &[]));
        assert!(simd_sum_covered(&second, &code, code.len(), &[]));

        // MUST REFUSE twin: the second loop starts at -1.
        let mut negative = code.clone();
        negative[24] = 0x02; // iconst_m1
        let second = detect_int_array_sum_forms(&negative, 26, 40, 3, true).expect("second loop");
        assert!(!simd_sum_covered(&second, &negative, negative.len(), &[]));
        let first = detect_int_array_sum_forms(&negative, 7, 21, 3, true).expect("first loop");
        assert!(simd_sum_covered(&first, &negative, negative.len(), &[]));
    }
}
