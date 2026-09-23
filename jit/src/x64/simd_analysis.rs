// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! SIMD loop analysis: vectorization and SuperWord.
//!
//! Moved verbatim out of `x64.rs`'s `SIMD loop analysis and vectorization`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.
//!
//! The bytecode pattern detectors below are the *historical* layer: each one
//! recognizes a single hand-written shape and hands it to a hand-written
//! emitter. [`vector_gate`] is the replacement discipline — a general
//! admission gate that proves a loop *could* be vectorized, or names why not.
//! It emits nothing; see its module doc for what an emitter still owes.

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
pub(super) fn simd_array_covered(
    code: &[u8],
    code_len: usize,
    guards: &[SpeculativeBCEGuard],
    header: usize,
    arr_local: usize,
    bound: SimdLoopBound,
    iv_local: usize,
) -> bool {
    match bound {
        SimdLoopBound::ArrayLength(_) => true,
        SimdLoopBound::Local(bound_local) => {
            (find_bound_arraylength_provenance(code, code_len, bound_local) == Some(arr_local)
                && find_iv_nonneg_start(code, code_len, iv_local))
                || guards.iter().any(|g| {
                    g.loop_header == header
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
/// declared vectorization knob (`super::vec_emit::VECTORIZE_FLAG`, i.e.
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
/// `VecEmitPolicy::from_flags` (`vec_emit.rs`, no production caller) reads
/// the same key with its own default-off rule; this answer does not depend on
/// it.
///
/// Latched on first read: detectors run once per loop per compile, and a flag
/// read is expected to be cached.
fn simd_sum_forms_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on(super::vec_emit::VECTORIZE_FLAG)
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
#[allow(dead_code)]
pub(crate) fn detect_int_array_element_wise(
    code: &[u8],
    header: usize,
    back_edge: usize,
    iv_local: usize,
) -> Option<SimdArrayElementWise> {
    detect_int_array_element_wise_forms(code, header, back_edge, iv_local, simd_sum_forms_enabled())
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
    })
}

// ---------------------------------------------------------------------------
// P2 — vectorization admission gate
// ---------------------------------------------------------------------------

pub mod vector_gate {
    #![allow(dead_code)]
    //! Decides whether a counted loop *could* be vectorized — and emits nothing.
    //!
    //! The deep-research report's P2 is an ordering claim, not a feature request:
    //! *"Vector work before alias, range, alignment, safepoint, and deopt metadata
    //! are sound will multiply wrong-code risk."* Those four dependencies now
    //! exist, so what this module adds is the thing that consumes them and says
    //! **no** — a gate, with a refusal taxonomy and a proof for every admission.
    //!
    //! # What this module is not
    //!
    //! There is no vector emitter here and nothing turns one on. Every admitted
    //! loop comes back as a [`VecPlan`]: a lane count, a tail plan, an alignment
    //! verdict, an overflow model, and a list of [`PreheaderGuard`]s that a future
    //! emitter **must** discharge. A consumer that cannot emit one of those guards
    //! has to treat the whole plan as refused — a partially-emitted guard set
    //! proves nothing, exactly as in [`crate::scev`].
    //!
    //! `super::vec_emit` is that consumer. It obeys the rule literally: it takes
    //! one binding per `guards` entry and refuses the whole request when even one
    //! is missing. It is off by default and no call site invokes it yet; see
    //! `docs/jit/vectorization-emitter.md` for what it emits and what remains
    //! unvalidated.
    //!
    //! # Where the facts come from
    //!
    //! Nothing here re-derives a fact another pass already proves:
    //!
    //! * **Stride, trip count, index range, overflow** — [`crate::scev`]. The
    //!   [`CountedLoop`] is asked for [`CountedLoop::index_span`],
    //!   [`CountedLoop::prove_index_in_bounds_of`] and
    //!   [`CountedLoop::trip_count`]; its [`OverflowModel`] is copied into the
    //!   plan rather than re-argued. A hand-rolled range check here would be the
    //!   duplicated-proof failure the report's ordering exists to prevent.
    //! * **Aliasing** — [`Graph::may_alias`] over [`AliasClass`]. The dependence
    //!   test asks the IR memory model whether two accesses *can* overlap and only
    //!   then computes a distance from the affine subscripts. It never decides
    //!   disjointness on its own.
    //! * **Vector width** — `x64::cpu_features`, through
    //!   [`VectorIsa::detect`]. No width is ever assumed, and since round 10
    //!   no width is *offered* that `super::vec_emit` could not emit: on
    //!   x86-64 detection answers AVX2 or `None`, because every encoding in
    //!   that emitter is VEX. See [`VectorIsa::detect`] for the argument.
    //!
    //! # The legality rule
    //!
    //! The only transform this gate reasons about is **body widening**: iterations
    //! `b .. b+VF` run as one pass in which each scalar operation becomes one
    //! whole-vector operation, in the original program order. Everything below is
    //! stated against that transform and nothing else.
    //!
    //! For a pair of memory accesses `E` (earlier in program order) and `L`
    //! (later), let `d` be the number of iterations such that `L` at iteration
    //! `k + d` touches what `E` touches at iteration `k`:
    //!
    //! | `d` | preserved by widening? |
    //! |---|---|
    //! | no integer solution | there is no dependence at all |
    //! | `d == 0` | yes — within one vector pass `E`'s op still precedes `L`'s |
    //! | `d > 0` | yes, at **any** lane count — program order and iteration order agree |
    //! | `d < 0` | only when `VF <= |d|`; otherwise widening inverts the two |
    //! | unknown | no — refuse |
    //!
    //! The `d < 0` row is the one that matters and it is why the distance is kept
    //! *signed* rather than collapsed into a flow/anti/output classification. An
    //! anti-dependence is not automatically safe and a flow dependence is not
    //! automatically fatal: `a[i] = a[i+1]` (a distance-1 anti-dependence) widens
    //! correctly because the whole vector load precedes the whole vector store,
    //! while `a[i-1] = x; y = a[i];` (also distance 1, also an anti-dependence)
    //! does not, because there the store is the earlier op. The kind is reported
    //! for diagnostics; the sign decides.
    //!
    //! # Floating point
    //!
    //! Reassociating `float`/`double` addition is **not** value-preserving, so a
    //! floating-point reduction is refused unless the caller passes
    //! [`FpRelaxation::AllowReassociation`]. The default is
    //! [`FpRelaxation::Strict`]. Three further FP facts are encoded rather than
    //! assumed:
    //!
    //! * *Element-wise* FP arithmetic is admitted even under `Strict`. IEEE-754
    //!   add/sub/mul/div are defined per operand pair; a lane computes exactly the
    //!   scalar result, including NaN payload propagation and signed zeros. There
    //!   is no reassociation because there is no accumulator.
    //! * FP `min`/`max` are refused **unconditionally**. `Math.min`/`Math.max` on
    //!   `double` order `-0.0` below `+0.0` and return NaN if either operand is
    //!   NaN; `MINPD`/`MAXPD` return the *second* operand for both of those cases.
    //!   The relaxation flag does not cover this — it is a wrong answer, not a
    //!   reordering.
    //! * Integer reduction *is* admitted under `Strict`. JVM `int`/`long`
    //!   arithmetic is modular two's-complement, so `+`, `*`, `&`, `|`, `^` are
    //!   associative and commutative over the whole domain: reassociating them is
    //!   exact, and `PADDD`/`PMULLD` wrap identically to `iadd`/`imul`.
    //!
    //! Integer `/` and `%` are refused: they trap per element
    //! (`ArithmeticException` on a zero divisor, and `Integer.MIN_VALUE / -1`
    //! overflows), which a lane cannot express.
    //!
    //! # Hard refusals
    //!
    //! Safepoints, deopt points, GC references, irreducible control flow,
    //! unprovable alignment on an ISA that requires it, and float reassociation
    //! are refusals with no lane count that rescues them. See [`VecRefusal`].
    //!
    //! One more joined that list in round 10 wave 6, and it is the odd one out
    //! because the lane count it refuses is *safe*, merely unencodable:
    //! [`VecRefusal::WidthBelowIsaMinimum`]. A backward dependence at distance
    //! 2 or 3 over a 4-byte element caps the vector at two lanes, and two
    //! 4-byte lanes are eight bytes, which is narrower than the narrowest move
    //! any backend in this tree has. Raising the count to an encodable one
    //! would be wrong code rather than a missed optimization, so the answer is
    //! a refusal — issued *here*, where the dependence that caused it is still
    //! in scope. The floor itself is a property of the target
    //! ([`VectorIsa::min_width_bytes`]), not of one emitter, which is what
    //! keeps this function's reasoning ISA-generic.

    use crate::ir::{AliasClass, Graph, MemEffect, MemKind, NodeId, NO_NODE};
    use crate::scev::{
        BoundsProof, CountedLoop, IndexExpr, IntRange, OverflowModel, PreheaderGuard, RangeEnv,
        RefusalReason, TripCount, TripCountProof,
    };
    use cratonvm_types::{element_byte_size, ArrayElementType, ARRAY_DATA_OFFSET};

    // -- element widths ----------------------------------------------------

    /// The width in bytes of one array element of `kind`.
    ///
    /// Deferred to `cratonvm_types::element_byte_size` so a layout change
    /// cannot leave a second table here to drift. Array elements in this VM
    /// are natural-width and contiguous from `ARRAY_DATA_OFFSET`, which is what
    /// makes a lane-per-element vector access meaningful at all.
    pub(crate) fn elem_bytes(kind: MemKind) -> usize {
        element_byte_size(match kind {
            MemKind::Int => ArrayElementType::Int,
            MemKind::Long => ArrayElementType::Long,
            MemKind::Float => ArrayElementType::Float,
            MemKind::Double => ArrayElementType::Double,
            MemKind::Byte => ArrayElementType::Byte,
            MemKind::Char => ArrayElementType::Char,
            MemKind::Short => ArrayElementType::Short,
            MemKind::Ref => ArrayElementType::Reference,
        })
    }

    /// True for the two IEEE-754 element types.
    pub(crate) fn is_float(kind: MemKind) -> bool {
        matches!(kind, MemKind::Float | MemKind::Double)
    }

    // -- the target ---------------------------------------------------------

    /// Whether the ISA's vector moves tolerate an unaligned address.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum AlignmentPolicy {
        /// Unaligned vector moves exist and are architecturally correct
        /// (x86 `MOVDQU`/`VMOVDQU`, AArch64 `LDR q`). Alignment is then a
        /// *performance* fact and never a correctness one.
        UnalignedOk,
        /// Every vector access must be naturally aligned to the vector width.
        /// Unprovable alignment is a refusal, not a slower encoding.
        NaturalRequired,
    }

    /// What the target actually supports. Never inferred from the host at a
    /// call site — obtained from [`VectorIsa::detect`] or named explicitly.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct VectorIsa {
        /// Diagnostic name.
        pub name: &'static str,
        /// Vector register width in bytes.
        pub width_bytes: usize,
        /// Whether unaligned vector moves are legal.
        pub alignment: AlignmentPolicy,
        /// Whether 32-bit integer multiply and min/max exist lane-wise
        /// (`PMULLD` / `PMINSD` / `PMAXSD` — SSE4.1 on x86, architectural on
        /// NEON). SSE2 alone has neither.
        pub int32_mul_minmax: bool,
        /// Whether the ISA can mask a partial vector, making a remainder loop
        /// unnecessary. No ISA modelled here can (that is AVX-512 `k`
        /// registers, which `cpu_features` does not detect).
        pub masked_tail: bool,
        /// The narrowest vector, in bytes, that this tree has an emitter for on
        /// this target.
        ///
        /// A capability *of the target*, declared here beside
        /// [`Self::int32_mul_minmax`] and [`Self::masked_tail`] and read by
        /// [`admit_vectorization`] in the same ISA-generic way. It is
        /// deliberately **not** one emitter's width table imported into the
        /// gate: each target states its own floor, and a backend that grows a
        /// narrower move changes this number and nothing else. An 8-byte
        /// vector is a perfectly good `LD1 {v0.2s}` on some future AArch64
        /// backend; what this field records is that no backend *here* encodes
        /// one yet.
        ///
        /// Why every target below says 16, established by reading the
        /// encoders rather than assumed:
        ///
        /// * The four x86 entries — `super::super::vec_emit` encodes exactly
        ///   two widths (`vec_emit::emitter_encodes_width`, which is
        ///   `{16, 32}`), and every move it has is a whole-register one. The
        ///   half-register move that would make 8 reachable is `VMOVQ`, whose
        ///   two directions are `VEX.128.F3.0F 7E` (load) and
        ///   `VEX.128.66.0F D6` (store) — note that they do not even share a
        ///   mandatory prefix, which `vec_emit::move_opcodes`'s one-`pp`
        ///   return shape cannot express. Adding it is a new encoder family,
        ///   not a table entry.
        /// * `neon128` — `jit/src/aarch64.rs` carries `ld1_4s`, `st1_4s`,
        ///   `add_v4s` and `mul_v4s` and no other vector encoder. Every one of
        ///   them is `Q=1`, the full 128-bit `.4S` arrangement; the `.2S`
        ///   forms are architecturally real and simply absent from this tree.
        ///
        /// The number matters because a backward dependence is the one thing
        /// that can drive an admitted plan's width below the register — see
        /// [`VecRefusal::WidthBelowIsaMinimum`].
        pub min_width_bytes: usize,
    }

    impl VectorIsa {
        /// SSE2 — architectural on every x86-64 CPU. 128-bit, unaligned moves
        /// legal, no 32-bit integer multiply.
        pub(crate) const fn sse2() -> VectorIsa {
            VectorIsa {
                name: "sse2",
                width_bytes: 16,
                alignment: AlignmentPolicy::UnalignedOk,
                int32_mul_minmax: false,
                masked_tail: false,
                min_width_bytes: 16,
            }
        }

        /// SSE4.1 — SSE2 plus `PMULLD`/`PMINSD`/`PMAXSD`.
        pub(crate) const fn sse41() -> VectorIsa {
            VectorIsa {
                name: "sse4.1",
                int32_mul_minmax: true,
                ..VectorIsa::sse2()
            }
        }

        /// AVX2 — 256-bit integer and FP lanes.
        pub(crate) const fn avx2() -> VectorIsa {
            VectorIsa {
                name: "avx2",
                width_bytes: 32,
                int32_mul_minmax: true,
                ..VectorIsa::sse2()
            }
        }

        /// AArch64 NEON — 128-bit, architectural, unaligned moves legal.
        pub(crate) const fn neon128() -> VectorIsa {
            VectorIsa {
                name: "neon128",
                width_bytes: 16,
                alignment: AlignmentPolicy::UnalignedOk,
                int32_mul_minmax: true,
                masked_tail: false,
                // `jit/src/aarch64.rs` has only the `Q=1` `.4S` forms; the
                // 8-byte `.2S` ones are legal AArch64 and absent here.
                min_width_bytes: 16,
            }
        }

        /// A 128-bit ISA whose vector moves *must* be naturally aligned.
        ///
        /// No target this JIT emits for is one. It exists so the
        /// alignment refusal has a target that exercises it, rather than being
        /// an untested branch waiting for the first strict backend.
        pub(crate) const fn strict_align128() -> VectorIsa {
            VectorIsa {
                name: "strict-align128",
                alignment: AlignmentPolicy::NaturalRequired,
                ..VectorIsa::sse2()
            }
        }

        /// The widest ISA the host supports **and for which this tree has an
        /// emitter**, or `None`.
        ///
        /// `cfg!` rather than `#[cfg]` so both arms typecheck everywhere; the
        /// `cpu_features` queries already answer `false` off x86-64.
        ///
        /// # Why x86-64 is AVX2-or-nothing
        ///
        /// Until round 10 this answered `sse41()` / `sse2()` on the `else` side
        /// of the very `has_avx2()` test it starts with. Both answers were
        /// dead by construction: `super::super::vec_emit::emit_vector_loop`,
        /// the only function in the crate that turns a [`VecPlan`] into
        /// machine code, opens with `if !req.host.has_avx2() { return
        /// Err(HostLacksAvx2) }`, and `HostVectorSupport` stores exactly one
        /// bit — `avx2` — so it cannot even express "SSE4.1 but no AVX2".
        /// Every plan decided against `sse41()`/`sse2()` was therefore
        /// guaranteed to be refused at emission, on every host, every time.
        ///
        /// The resolution is to narrow *detection* rather than to widen the
        /// emitter, and the deciding evidence is in the emitter's encoders,
        /// not in a preference:
        ///
        /// * `vec_emit::Asm` has exactly one prefix emitter, `vex_prefix`, and
        ///   every vector helper on it (`vec_rr`, `vec_mem`, `vpshufd`,
        ///   `vextracti128`, `vmovd_to_gpr`, `vzeroupper`) routes through it.
        ///   There is no legacy-SSE (non-VEX) encoding anywhere in the module.
        /// * The 16-byte paths in that emitter are real and tested — `l =
        ///   plan.width_bytes == 32` is `false` for a 16-byte plan and the
        ///   128-bit bytes are pinned by
        ///   `a_narrower_lane_count_emits_the_128_bit_form_and_no_vzeroupper`
        ///   and `a_128_bit_reduction_skips_the_lane_extract`. But they are
        ///   **VEX.128**, which is AVX, not SSE2 or SSE4.1. Teaching
        ///   `emit_vector_loop` to "honour `plan.isa`" by running those paths
        ///   for an `sse2()` plan would emit VEX on a host detected as having
        ///   no VEX at all: a `SIGILL`, not a missing feature.
        /// * The AVX-but-not-AVX2 middle ground — where VEX.128 integer forms
        ///   like `VPADDD xmm` *are* legal — is not expressible either:
        ///   `super::super::cpu_features` has no `has_avx()`, only
        ///   `has_avx2()`/`has_sse41()`. Adding one is a change to a file
        ///   outside this module and belongs with the lane that adds the
        ///   `x64.rs` call site.
        ///
        /// So on x86-64 the honest answer is the one the emitter can honour:
        /// AVX2, or nothing. Nothing means the candidate search does not run
        /// at all on a host whose plans could only be thrown away.
        ///
        /// `sse2()`, `sse41()` and `strict_align128()` survive as *explicitly
        /// named* targets — the type's own doc says an ISA is "obtained from
        /// [`VectorIsa::detect`] **or named explicitly**" — because they are
        /// what gives this gate coverage of the ISA-conditional refusals
        /// ([`VecRefusal::MissingIsaFeature`] for `PMULLD` under plain SSE2,
        /// [`VecRefusal::UnprovableAlignment`] under `NaturalRequired`) and of
        /// 16-byte lane counts. What changed is that the *host* can no longer
        /// select them.
        ///
        /// The aarch64 arm is left alone deliberately: `jit/src/aarch64.rs`
        /// does carry NEON `LD1`/`ST1`/`ADD`/`MUL` encoders, so `neon128()` is
        /// modelling a backend whose instructions exist. It has no `VecPlan`
        /// consumer either, but that is the same "nothing is wired in" fact
        /// that covers this whole pipeline rather than a contradiction between
        /// two files in it. Read, not executed.
        pub(crate) fn detect() -> Option<VectorIsa> {
            if cfg!(target_arch = "x86_64") {
                crate::x64::has_avx2().then(VectorIsa::avx2)
            } else if cfg!(target_arch = "aarch64") {
                Some(VectorIsa::neon128())
            } else {
                None
            }
        }

        /// How many `elem`-typed lanes fit in one vector register.
        pub(crate) fn lanes_for(self, elem: MemKind) -> usize {
            let size = elem_bytes(elem);
            if size == 0 {
                0
            } else {
                self.width_bytes / size
            }
        }
    }

    // -- alignment ----------------------------------------------------------

    /// The alignment verdict for a loop's vector accesses.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Alignment {
        /// Every vector access starts on a boundary of this many bytes.
        Proven(usize),
        /// Not proven. Correct only under [`AlignmentPolicy::UnalignedOk`].
        Unknown,
    }

    /// The alignment the allocator provably gives every object base.
    ///
    /// `types/src/heap_types.rs` pins the TLAB bump grid at 8 bytes
    /// (`HEADER_SIZE % 8 == 0` and `ARRAY_DATA_OFFSET % 8 == 0`, asserted at
    /// compile time) and nothing pins it higher. Element 0 of an array sits
    /// `ARRAY_DATA_OFFSET` bytes past a base that is only 8-byte aligned, so
    /// whatever that offset is, **16-byte alignment of an array's element 0 is
    /// not provable today**, and neither is 32-byte. (Element addresses use
    /// `ARRAY_DATA_OFFSET`, not `HEADER_SIZE`: the two are equal today and the
    /// planned array-length prefix separates them.) That is why
    /// every x86 plan this gate admits comes back
    /// [`Alignment::Unknown`] — which is harmless on x86 (`MOVDQU`) and would
    /// be a hard refusal on a strict-alignment target.
    pub(crate) const PROVEN_OBJECT_ALIGNMENT: usize = 8;

    /// Whether a vector access starting at `first_index` is `width_bytes`
    /// aligned.
    ///
    /// `first_index` is `None` when the first accessed element is not a
    /// compile-time constant, which is itself an unprovable alignment — the
    /// answer is the refusing one, never an assumption.
    pub(crate) fn analyze_alignment(
        elem: MemKind,
        first_index: Option<i64>,
        base_alignment: usize,
        width_bytes: usize,
    ) -> Alignment {
        if width_bytes == 0 || base_alignment < width_bytes {
            return Alignment::Unknown;
        }
        let first = match first_index {
            Some(f) => f,
            None => return Alignment::Unknown,
        };
        let byte_offset = match first
            .checked_mul(elem_bytes(elem) as i64)
            .and_then(|b| b.checked_add(ARRAY_DATA_OFFSET as i64))
        {
            Some(b) if b >= 0 => b,
            _ => return Alignment::Unknown,
        };
        if byte_offset % width_bytes as i64 == 0 {
            Alignment::Proven(width_bytes)
        } else {
            Alignment::Unknown
        }
    }

    // -- the dependence test ------------------------------------------------

    /// Whether an access reads or writes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum AccessKind {
        /// A load.
        Read,
        /// A store.
        Write,
    }

    /// One memory access in the loop body, reduced to the four facts the
    /// dependence test needs.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct VecAccess {
        /// The IR node, for diagnostics and for [`Graph::may_reorder`].
        pub node: NodeId,
        /// Where it touches memory, as the IR memory model classifies it.
        pub class: AliasClass,
        /// Read or write.
        pub kind: AccessKind,
        /// Element type.
        pub elem: MemKind,
        /// The subscript as an affine function of the loop's IV.
        pub index: IndexExpr,
    }

    impl VecAccess {
        /// The `(reads, writes)` pair this access contributes, in the shape
        /// [`Graph::may_alias`] consumes.
        fn split(&self) -> (AliasClass, AliasClass) {
            match self.kind {
                AccessKind::Read => (self.class, AliasClass::None),
                AccessKind::Write => (AliasClass::None, self.class),
            }
        }
    }

    /// The classification of a dependence, in *execution* order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum DepKind {
        /// Write then read — a true dependence.
        Flow,
        /// Read then write.
        Anti,
        /// Write then write.
        Output,
    }

    impl DepKind {
        /// The kind seen with the two accesses swapped.
        fn reversed(self) -> DepKind {
            match self {
                DepKind::Flow => DepKind::Anti,
                DepKind::Anti => DepKind::Flow,
                DepKind::Output => DepKind::Output,
            }
        }
    }

    /// How far apart in the iteration space the two ends of a dependence are,
    /// **relative to program order**. See the module doc's legality table.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum DepDistance {
        /// Distance zero: both ends are the same iteration. Widening keeps the
        /// program order inside one vector pass, so this never caps the lane
        /// count.
        Same,
        /// The sink is `k` iterations after the source, *and* the source is
        /// also the earlier of the two in program order. Iteration order and
        /// program order agree, so widening preserves it at any lane count.
        Forward(u64),
        /// The dependence runs against program order: the later-in-program
        /// access at iteration `n` conflicts with the earlier-in-program access
        /// at iteration `n + k`. Widening inverts the two unless the lane count
        /// is at most `k`.
        Backward(u64),
        /// The accesses may overlap and no distance could be computed — a
        /// runtime-aliased pair, a mismatched scale, or a non-unit step.
        Unknown,
    }

    /// One dependence between two accesses in the loop body.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct Dependence {
        /// The access that runs first.
        pub source: NodeId,
        /// The access that observes it.
        pub sink: NodeId,
        /// Flow / anti / output, for diagnostics.
        pub kind: DepKind,
        /// The signed distance, which is what decides legality.
        pub distance: DepDistance,
    }

    /// The dependence between two body accesses given in **program order**.
    ///
    /// `None` means there is provably no dependence: either the memory model
    /// says the two locations cannot overlap ([`Graph::may_alias`]), or the
    /// affine subscripts admit no integer iteration pair that makes them equal.
    ///
    /// The aliasing question is asked *first* and asked of the IR model. A
    /// distance is only computed once the two accesses are known to share one
    /// base node, because a distance between two references that merely *might*
    /// be the same array is meaningless — that case answers
    /// [`DepDistance::Unknown`], which the gate refuses.
    pub(crate) fn dependence_between(
        graph: &Graph,
        earlier: &VecAccess,
        later: &VecAccess,
        stride: i32,
    ) -> Option<Dependence> {
        let (earlier_reads, earlier_writes) = earlier.split();
        let (later_reads, later_writes) = later.split();
        let conflicts = graph.may_alias(earlier_writes, later_reads)
            || graph.may_alias(earlier_reads, later_writes)
            || graph.may_alias(earlier_writes, later_writes);
        if !conflicts {
            return None;
        }

        let kind = match (earlier.kind, later.kind) {
            (AccessKind::Write, AccessKind::Read) => DepKind::Flow,
            (AccessKind::Read, AccessKind::Write) => DepKind::Anti,
            (AccessKind::Write, AccessKind::Write) => DepKind::Output,
            // A read/read pair never constrains anything, whatever it aliases.
            (AccessKind::Read, AccessKind::Read) => return None,
        };
        let unknown = Dependence {
            source: earlier.node,
            sink: later.node,
            kind,
            distance: DepDistance::Unknown,
        };

        // A distance is only meaningful between two accesses to the *same*
        // object. `may_alias` answering "maybe" over two different base nodes
        // is precisely the runtime-alias case, and it has no distance.
        let same_base = match (earlier.class.base(), later.class.base()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        if !same_base
            || earlier.index.iv_local != later.index.iv_local
            || earlier.index.scale != later.index.scale
            || elem_bytes(earlier.elem) != elem_bytes(later.elem)
        {
            return Some(unknown);
        }

        // Address of `access` at iteration `k` is
        // `scale * (init + stride * k) + offset`, so successive iterations
        // advance it by `scale * stride` elements and the two accesses sit
        // `later.offset - earlier.offset` elements apart within one iteration.
        let step = match (earlier.index.scale as i64).checked_mul(stride as i64) {
            Some(0) | None => return Some(unknown),
            Some(s) => s,
        };
        let delta = later.index.offset as i64 - earlier.index.offset as i64;
        if delta % step != 0 {
            // No integer iteration pair makes the two addresses equal.
            return None;
        }
        let d = -(delta / step);

        Some(if d == 0 {
            Dependence {
                source: earlier.node,
                sink: later.node,
                kind,
                distance: DepDistance::Same,
            }
        } else if d > 0 {
            Dependence {
                source: earlier.node,
                sink: later.node,
                kind,
                distance: DepDistance::Forward(d.unsigned_abs()),
            }
        } else {
            // The conflicting execution order is `later` at the earlier
            // iteration, then `earlier` at the later one — so the roles and
            // the kind both flip.
            Dependence {
                source: later.node,
                sink: earlier.node,
                kind: kind.reversed(),
                distance: DepDistance::Backward(d.unsigned_abs()),
            }
        })
    }

    // -- the loop body ------------------------------------------------------

    /// One node of the loop body, as the gate needs to see it.
    ///
    /// `effect` is what [`Graph::memory_effect`] answers for the node — the
    /// gate never re-derives it. `elem` and `index` decorate an array access
    /// with the two facts the IR memory model does not carry: the element type
    /// (the alias class knows *which* array, not what is in it) and the
    /// subscript as an affine function of the loop's induction variable.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct VecBodyOp {
        /// The IR node.
        pub node: NodeId,
        /// Its memory effect, ordering, safepoint-ness and allocation-ness.
        pub effect: MemEffect,
        /// Element type, for an array access.
        pub elem: Option<MemKind>,
        /// Subscript, for an array access.
        pub index: Option<IndexExpr>,
        /// This node can deoptimize: a speculative guard, an implicit-exception
        /// check that rebuilds an interpreter frame, an uncommon trap.
        pub deopts: bool,
    }

    impl VecBodyOp {
        /// A body node that touches no memory and cannot trap.
        pub(crate) fn pure(node: NodeId) -> VecBodyOp {
            VecBodyOp {
                node,
                effect: MemEffect::NONE,
                elem: None,
                index: None,
                deopts: false,
            }
        }

        /// An array element access.
        pub(crate) fn array(
            node: NodeId,
            class: AliasClass,
            kind: AccessKind,
            elem: MemKind,
            index: IndexExpr,
        ) -> VecBodyOp {
            VecBodyOp {
                node,
                effect: match kind {
                    AccessKind::Read => MemEffect::read(class),
                    AccessKind::Write => MemEffect::write(class),
                },
                elem: Some(elem),
                index: Some(index),
                deopts: false,
            }
        }
    }

    /// A lane-wise arithmetic operation the body performs.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum VecOp {
        /// `iadd` / `ladd` / `fadd` / `dadd`.
        Add,
        /// `isub` and friends.
        Sub,
        /// `imul` and friends.
        Mul,
        /// `idiv` / `fdiv` and friends.
        Div,
        /// `irem` / `frem` and friends.
        Rem,
        /// `iand` / `land`.
        And,
        /// `ior` / `lor`.
        Or,
        /// `ixor` / `lxor`.
        Xor,
        /// `Math.min`.
        Min,
        /// `Math.max`.
        Max,
        /// `ineg` and friends.
        Neg,
    }

    /// One arithmetic operation in the loop body.
    ///
    /// The producer must list **every** arithmetic node, not only the ones it
    /// believes are vectorizable: an operation this vocabulary cannot name is
    /// an operation the gate cannot clear, and an omitted one is a silent
    /// admission. [`VecOp`] therefore includes `Div`/`Rem`, which exist only to
    /// be refused for integers.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct VecArith {
        /// The IR node.
        pub node: NodeId,
        /// The type the operation works on.
        pub elem: MemKind,
        /// Which operation.
        pub op: VecOp,
        /// True when the loop carries this operation's accumulator across
        /// iterations. Vectorizing a reduction means **reassociating** it, and
        /// for `float`/`double` that is not value-preserving.
        pub reduction: bool,
    }

    /// Whether the caller permits floating-point reassociation.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum FpRelaxation {
        /// Refuse any transform that changes an FP result. The default, and
        /// the only setting that matches Java's `strictfp`-by-default
        /// arithmetic.
        Strict,
        /// The caller has taken responsibility for a different FP result.
        /// Admits FP reductions; does **not** admit FP `min`/`max`, which is a
        /// wrong answer rather than a reordered one.
        AllowReassociation,
    }

    /// Everything the gate is asked to decide about.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct VecCandidate<'a> {
        /// The loop, with its induction variable, limit and overflow model
        /// already established by [`crate::scev`].
        pub counted: &'a CountedLoop,
        /// Compile-time knowledge about the locals the limit reads.
        pub env: &'a RangeEnv,
        /// Every node in the loop body.
        pub body: &'a [VecBodyOp],
        /// Every arithmetic operation in the loop body.
        pub arith: &'a [VecArith],
        /// False when the loop's control flow is irreducible, or when the body
        /// has internal control flow the producer has not proved away.
        pub reducible: bool,
        /// The alignment the caller can prove of the *object base* of every
        /// array the loop touches. [`PROVEN_OBJECT_ALIGNMENT`] is the honest
        /// value for this VM today.
        pub base_alignment: usize,
        /// The FP policy.
        pub fp: FpRelaxation,
        /// The target.
        pub isa: VectorIsa,
    }

    // -- verdict ------------------------------------------------------------

    /// How the remainder iterations are handled.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum TailStrategy {
        /// The trip count is an exact multiple of the lane count: no remainder
        /// exists.
        None,
        /// Fall back into the untouched scalar loop for the last
        /// `trip % lanes` iterations. At most `max_iterations` of them.
        ///
        /// This is the only strategy available: masking a partial vector needs
        /// AVX-512 predicate registers, which `cpu_features` does not detect
        /// and no [`VectorIsa`] here claims.
        ScalarRemainder {
            /// Upper bound on the scalar iterations left over.
            max_iterations: usize,
        },
    }

    /// A pre-header obligation, together with **which array it is about**.
    ///
    /// [`crate::scev`]'s guards are per-array by contract — a
    /// [`PreheaderGuard::LengthAtLeast`] says "the guarded array is at least
    /// this long" and names no array, because the caller is the one that knows
    /// which access it asked about. Collapsing two arrays' identical-looking
    /// guards into one would discharge the shorter array's obligation with the
    /// longer array's length, which is exactly the multi-array out-of-bounds
    /// store the bounds-check eliminator's provenance pass exists to prevent.
    /// The attribution is therefore carried, and de-duplication only ever
    /// happens within one array.
    ///
    /// Guards that are really about the induction variable rather than an
    /// array (`AtMost`, `AtLeast`, `StrideInRange`) are attributed to each
    /// array they were produced for. Emitting such a check more than once is
    /// redundant, never wrong.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct ArrayGuard {
        /// The array reference node whose access produced this obligation, or
        /// [`NO_NODE`] when the obligation is not about an array at all — which
        /// today means the loop's `trip >= lanes` check. The field exists so
        /// that two identical-looking `LengthAtLeast` guards over two different
        /// arrays are never deduplicated into one; a guard with no array is in
        /// a bucket of its own and dedupes only against itself.
        pub array: NodeId,
        /// The obligation itself.
        pub guard: PreheaderGuard,
    }

    /// An admitted loop, with every obligation the transform rests on.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct VecPlan {
        /// The target the plan was decided against.
        pub isa: VectorIsa,
        /// The element type all accesses share.
        pub elem: MemKind,
        /// Lanes per vector operation. Always a power of two and at least 2.
        pub lanes: usize,
        /// `lanes * elem_bytes(elem)`.
        ///
        /// May be narrower than the register when a dependence capped the lane
        /// count — but never narrower than [`VectorIsa::min_width_bytes`],
        /// which is where the target's narrowest actual move sits. A cap that
        /// would push it below the floor is
        /// [`VecRefusal::WidthBelowIsaMinimum`] instead of a plan, because a
        /// width with no move behind it is not a narrower plan, it is an
        /// unemittable one.
        pub width_bytes: usize,
        /// Whether every vector access is provably aligned.
        pub alignment: Alignment,
        /// What to do with the remainder.
        pub tail: TailStrategy,
        /// Bounds on the scalar trip count.
        pub trip: TripCount,
        /// The overflow assumption the index proofs rest on. `NoWrapGuarded`
        /// means at least one entry of `guards` is load-bearing for
        /// *soundness*, not just for bounds elision.
        pub overflow: OverflowModel,
        /// Pre-header obligations. **All** of them must be emitted, or the
        /// plan is void. An entry whose `array` is [`NO_NODE`] is not about an
        /// array — today, the loop's `trip >= lanes` check.
        pub guards: Vec<ArrayGuard>,
        /// Every dependence the body carries, including the harmless ones.
        pub dependences: Vec<Dependence>,
        /// The lane ceiling the dependences imposed, if any.
        pub max_safe_lanes: Option<usize>,
    }

    /// Why a loop was not admitted. Each variant is a *hard* refusal at the
    /// stated lane count — none of them is a warning.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum VecRefusal {
        /// The target has no modelled vector unit.
        NoVectorIsa,
        /// The loop's control flow is irreducible, or the body branches.
        /// Widening a body assumes every iteration executes the same
        /// straight-line trace.
        IrreducibleControl,
        /// The induction variable's step is a runtime value, so no compile-time
        /// dependence distance exists.
        VariableStride,
        /// `scale * stride != 1`: the accesses are not unit-stride, so a
        /// contiguous vector load does not cover one iteration block.
        NonUnitStep {
            /// The subscript's multiplier.
            scale: i32,
            /// The induction variable's step.
            stride: i32,
        },
        /// [`CountedLoop::trip_count`] could not bound the iteration count.
        UnknownTripCount,
        /// The loop may execute fewer times than one vector pass covers, and
        /// no runtime check can settle it.
        ///
        /// The gate asks [`CountedLoop::prove_trip_count_at_least`] for a
        /// witness first, and takes the resulting
        /// [`PreheaderGuard::TripCountAtLeast`] — the shape added for exactly
        /// this family, and the one `super::vec_emit` already discharges with
        /// `VecGuardValues::Term`. This variant is what is left when that proof
        /// refuses: a *constant* trip count below the lane count (nothing to
        /// check at runtime — the answer is already known and it is "no"), a
        /// decreasing or non-unit-stride loop (the witness would not be
        /// trip-count-valued), a post-tested loop, or an unbounded entry value.
        ///
        /// Before the witness was asked for, this fired on
        /// `for (i = 0; i < n; i++)` with a runtime `n`, whose compile-time
        /// interval is `[0, i32::MAX]` — i.e. on almost every real loop.
        TripCountTooSmall {
            /// The fewest iterations the loop may run.
            min_trips: u64,
            /// The lane count that needed more.
            lanes: usize,
        },
        /// A node in the body is a safepoint. The back-edge poll is fine —
        /// widening lowers its frequency by a bounded factor — but a safepoint
        /// *inside* the body has to describe a frame whose locals a vectorized
        /// iteration no longer holds.
        SafepointInBody(NodeId),
        /// A node in the body can deoptimize. The deopt metadata describes a
        /// scalar frame at one bytecode index; there is no encoding for "lane 3
        /// of this vector was iteration 11".
        DeoptPointInBody(NodeId),
        /// A node reads or writes [`AliasClass::Any`] — an unanalyzable call.
        OpaqueMemoryEffect(NodeId),
        /// A node carries JMM ordering (a volatile access, a monitor
        /// operation). Widening would coalesce fences that the memory model
        /// requires per iteration.
        OrderedAccess(NodeId),
        /// The loop touches an array of references. A vector store of oops
        /// bypasses the GC write barrier — the same barrier-elision family that
        /// produced a use-after-free on this branch. There is no lane count
        /// that makes this safe; a reference vectorizer needs a vector-aware
        /// barrier first.
        GcReferenceAccess(NodeId),
        /// A memory access that is not an array element (a field, a static, a
        /// monitor) or that arrived without an element type.
        UnstructuredMemoryAccess(NodeId),
        /// The subscript is not an affine function of *this* loop's induction
        /// variable.
        NonAffineSubscript(NodeId),
        /// The body touches elements of two different types (not only two
        /// different widths: `int` and `float` share a width but not an
        /// opcode), or an arithmetic node computes in a type other than the
        /// element type (a `long` accumulator over `int` loads, `int`
        /// arithmetic over `byte` elements). The plan has one `elem`.
        MixedElementWidths,
        /// The body touches no memory at all, so there is nothing to widen.
        NoMemoryAccess,
        /// [`crate::scev`] refused to prove the subscript in range, or refused
        /// its no-wrap obligation outright.
        IndexNotProven {
            /// The access.
            node: NodeId,
            /// Why the proof was refused.
            reason: RefusalReason,
        },
        /// Two accesses may overlap and no distance could be computed.
        UnknownAliasing {
            /// The first access.
            a: NodeId,
            /// The second.
            b: NodeId,
        },
        /// A dependence runs against program order at a distance below two
        /// lanes, so no vector width preserves it.
        LoopCarriedDependence(Dependence),
        /// Vectorizing this reduction means reassociating floating point, and
        /// the caller asked for [`FpRelaxation::Strict`].
        FloatReassociation {
            /// The accumulator type.
            elem: MemKind,
            /// The reduction operator.
            op: VecOp,
        },
        /// The lane-wise instruction does not implement the Java operation.
        /// FP `min`/`max` (NaN and signed-zero rules) and integer `/`/`%`
        /// (per-lane traps) are the two families.
        NonIeeeVectorOp {
            /// The operand type.
            elem: MemKind,
            /// The operator.
            op: VecOp,
        },
        /// The operation needs an ISA feature the target does not have.
        MissingIsaFeature(&'static str),
        /// One element does not fit at least twice in a vector register.
        ElementTooWideForVector {
            /// The element type.
            elem: MemKind,
            /// The register width that could not hold two of them.
            width_bytes: usize,
        },
        /// A backward dependence capped the lane count at a *viable* vector —
        /// two lanes or more — whose bytes are nonetheless narrower than
        /// [`VectorIsa::min_width_bytes`], the narrowest vector this target has
        /// an emitter for.
        ///
        /// **A dependence cap is the only way to reach this.** An uncapped lane
        /// count is `isa.width_bytes / elem_bytes(elem)`, and multiplying that
        /// back by `elem_bytes` gives `isa.width_bytes` again, which is never
        /// below the floor. So whenever this fires there is a
        /// [`DepDistance::Backward`] behind it, and `capped_to` carries the
        /// ceiling it imposed.
        ///
        /// **There is no rescuing lane count, and that is arithmetic rather
        /// than timidity.** Raising the count above the dependence distance is
        /// *wrong code* — the widened body reorders the two ends of the
        /// dependence — and lowering it makes the vector narrower still. For a
        /// 4-byte element on a 16-byte floor the emittable counts are 4 and 8;
        /// the distances that reach here are 2 and 3 (1 is already
        /// [`VecRefusal::LoopCarriedDependence`], 4 and up admit a whole
        /// register). Every emittable count is above every reachable distance,
        /// so the set of widths that are both safe and encodable is empty.
        ///
        /// **Why the gate refuses rather than the emitter.** Before round 10
        /// wave 6 this plan was admitted and `vec_emit::emit_vector_loop`
        /// answered `VecEmitRefusal::UnsupportedWidth { width_bytes: 8, lanes:
        /// 2 }` — a true statement that cannot explain itself, because the
        /// emitter does not know the 8 came from a dependence distance of 2.
        /// Worse, the gate had by then computed an alignment verdict *against
        /// that 8-byte width*, a [`TailStrategy`] against those 2 lanes, and a
        /// full guard set, and published them in a plan no emitter would take.
        /// The decision belongs where the cap is visible.
        WidthBelowIsaMinimum {
            /// The lane count left after the cap. Always at least 2 — below
            /// that the refusal is [`VecRefusal::LoopCarriedDependence`] or
            /// [`VecRefusal::ElementTooWideForVector`].
            lanes: usize,
            /// `lanes * elem_bytes(elem)` — what the plan would have spanned.
            width_bytes: usize,
            /// [`VectorIsa::min_width_bytes`] for the target it was decided
            /// against.
            min_width_bytes: usize,
            /// The lane ceiling the tightest backward dependence imposed.
            ///
            /// `None` is not reachable from this gate, by the uncapped-width
            /// argument above. It is spelled as an option rather than an
            /// `unwrap` because a refusal path must not be the thing that
            /// panics — the same reason `ArrayGuard`'s array is
            /// `unwrap_or(NO_NODE)`.
            capped_to: Option<usize>,
        },
        /// The ISA requires naturally-aligned vector moves and the alignment
        /// could not be proved.
        UnprovableAlignment {
            /// The alignment the ISA needs.
            need: usize,
        },
    }

    /// The gate's answer.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum VecVerdict {
        /// Vectorizable, on these terms.
        Admitted(VecPlan),
        /// Not vectorizable. **Every** reason found, not just the first — a
        /// gate that stops at the first refusal teaches the wrong lesson about
        /// how far a loop is from admissible.
        Refused(Vec<VecRefusal>),
    }

    impl VecVerdict {
        /// True when the loop was admitted.
        pub(crate) fn is_admitted(&self) -> bool {
            matches!(self, VecVerdict::Admitted(_))
        }

        /// The plan, when there is one.
        pub(crate) fn plan(&self) -> Option<&VecPlan> {
            match self {
                VecVerdict::Admitted(p) => Some(p),
                VecVerdict::Refused(_) => Option::None,
            }
        }

        /// Whether this refusal set contains `wanted`.
        pub(crate) fn refused_for(&self, wanted: VecRefusal) -> bool {
            match self {
                VecVerdict::Refused(rs) => rs.contains(&wanted),
                VecVerdict::Admitted(_) => false,
            }
        }

        /// The refusals, or an empty slice.
        pub(crate) fn refusals(&self) -> &[VecRefusal] {
            match self {
                VecVerdict::Refused(rs) => rs,
                VecVerdict::Admitted(_) => &[],
            }
        }
    }

    /// The largest power of two not greater than `n` (`0` for `0`).
    fn floor_pow2(n: usize) -> usize {
        if n == 0 {
            0
        } else {
            1usize << (usize::BITS - 1 - n.leading_zeros())
        }
    }

    /// Reduce one body node to an access, a "nothing to see here", or a
    /// refusal.
    ///
    /// The order of the checks is the order of severity, so the *reason*
    /// reported for a call is `SafepointInBody` rather than the vaguer
    /// `OpaqueMemoryEffect` it would also qualify for.
    fn classify_body_op(op: &VecBodyOp, iv_local: usize) -> Result<Option<VecAccess>, VecRefusal> {
        if op.deopts {
            return Err(VecRefusal::DeoptPointInBody(op.node));
        }
        if op.effect.safepoint || op.effect.allocates {
            return Err(VecRefusal::SafepointInBody(op.node));
        }
        if !op.effect.order.is_plain() {
            return Err(VecRefusal::OrderedAccess(op.node));
        }
        if op.effect.reads == AliasClass::Any || op.effect.writes == AliasClass::Any {
            return Err(VecRefusal::OpaqueMemoryEffect(op.node));
        }
        if !op.effect.touches_memory() {
            return Ok(Option::None);
        }
        let (class, kind) = match (op.effect.reads.is_some(), op.effect.writes.is_some()) {
            (true, false) => (op.effect.reads, AccessKind::Read),
            (false, true) => (op.effect.writes, AccessKind::Write),
            // Reads and writes at once is a monitor or an opaque node; neither
            // is widenable and neither has a subscript.
            _ => return Err(VecRefusal::OpaqueMemoryEffect(op.node)),
        };
        // The element type comes from the memory model, not the producer.
        // `AliasClass::ArrayElem::elem` is read off the accessing op itself by
        // `ir::access_location`, which makes the oop refusal below unforgeable:
        // a producer that mislabels an `Object[]` access as `int` can no longer
        // talk the gate into a barrier-free vector store. The producer's own
        // `op.elem` is still required and must AGREE — a disagreement means the
        // producer mapped the wrong node, and nothing else it said about that
        // access (its subscript) can be trusted either.
        let class_elem = match class {
            AliasClass::ArrayElem { elem, .. } => elem,
            _ => return Err(VecRefusal::UnstructuredMemoryAccess(op.node)),
        };
        // Fail closed on the GC hazard first, whichever side names a reference.
        if class_elem == MemKind::Ref || op.elem == Some(MemKind::Ref) {
            return Err(VecRefusal::GcReferenceAccess(op.node));
        }
        let elem = match op.elem {
            Some(e) if e == class_elem => e,
            _ => return Err(VecRefusal::UnstructuredMemoryAccess(op.node)),
        };
        let index = match op.index {
            Some(i) if i.iv_local == iv_local => i,
            _ => return Err(VecRefusal::NonAffineSubscript(op.node)),
        };
        Ok(Some(VecAccess {
            node: op.node,
            class,
            kind,
            elem,
            index,
        }))
    }

    /// Decide whether `cand` may be vectorized, and on what terms.
    ///
    /// Returns [`VecVerdict::Refused`] with the complete set of reasons, or
    /// [`VecVerdict::Admitted`] with a [`VecPlan`] whose guards a consumer must
    /// discharge in full.
    pub(crate) fn admit_vectorization(graph: &Graph, cand: &VecCandidate<'_>) -> VecVerdict {
        let mut refusals: Vec<VecRefusal> = Vec::new();

        if cand.isa.width_bytes == 0 {
            refusals.push(VecRefusal::NoVectorIsa);
        }
        // A second way out of the loop (a `break`, a `return`, an `athrow`) is
        // internal control flow whether or not the producer noticed: the
        // widened body would run iterations past the one that leaves. `scev`
        // already knows it, so the gate does not take `reducible` on trust.
        if !cand.reducible || cand.counted.has_other_exit {
            refusals.push(VecRefusal::IrreducibleControl);
        }

        // ---- body hazards, and the accesses that survive them --------------
        let mut accesses: Vec<VecAccess> = Vec::new();
        for op in cand.body {
            match classify_body_op(op, cand.counted.iv.local) {
                Ok(Some(access)) => accesses.push(access),
                Ok(Option::None) => {}
                Err(refusal) => refusals.push(refusal),
            }
        }
        if accesses.is_empty() {
            refusals.push(VecRefusal::NoMemoryAccess);
            return VecVerdict::Refused(refusals);
        }

        // ---- one element type -----------------------------------------------
        // A lane count is one number, so a body that mixes widths would need
        // two of them and a reconciliation this gate does not describe.
        //
        // Equal *width* is not enough either. The plan carries ONE `elem`, and
        // `vec_emit` picks every opcode from it: an `int[]` and a `float[]`
        // access in one body (both 4 bytes) would have the float lanes added
        // with `VPADDD`. Likewise every arithmetic node must compute in the
        // access type — a `long` accumulator over `int` loads (`long s +=
        // a[i]`) needs a widening the plan cannot express, and handed to the
        // emitter it would be folded as a 32-bit reduction: the exact
        // truncation (`8 x Integer.MAX_VALUE` summing to -8) the single-pass
        // sum once shipped. A sub-word element with an `int` operation is the
        // same refusal for the same reason.
        let elem = accesses[0].elem;
        if accesses.iter().any(|a| a.elem != elem) || cand.arith.iter().any(|a| a.elem != elem) {
            refusals.push(VecRefusal::MixedElementWidths);
        }

        // ---- stride and step -----------------------------------------------
        // Unit scale AND unit stride, not merely `scale * stride == 1`. The
        // product admits `a[-i]` walked by `i--` (scale -1, stride -1), which is
        // a contiguous ascending access — but nothing downstream can emit it:
        // `VecPlan` records neither sign, and `vec_emit` addresses
        // `[base + iv*size + disp]` with an *increasing* `iv` and an
        // `iv + lanes <= bound` head test. Refusing here is what keeps the
        // plan's implicit "ascending, unit-stride" contract true.
        let stride = cand.counted.iv.stride.as_const();
        match stride {
            Option::None => refusals.push(VecRefusal::VariableStride),
            Some(s) => {
                for access in &accesses {
                    if access.index.scale != 1 || s != 1 {
                        refusals.push(VecRefusal::NonUnitStep {
                            scale: access.index.scale,
                            stride: s,
                        });
                    }
                }
            }
        }

        // ---- index range and overflow, delegated to scev --------------------
        let mut guards: Vec<ArrayGuard> = Vec::new();
        let mut overflow = OverflowModel::NoWrapProven;
        for access in &accesses {
            match cand.counted.index_span(&access.index, cand.env) {
                Ok(proven) => {
                    if proven.overflow == OverflowModel::NoWrapGuarded {
                        overflow = OverflowModel::NoWrapGuarded;
                    }
                }
                Err(reason) => {
                    refusals.push(VecRefusal::IndexNotProven {
                        node: access.node,
                        reason,
                    });
                    continue;
                }
            }
            // `None` for the array local: at IR level there is no JVM slot to
            // name, so the `length >= a.length` tautology shortcut is
            // unavailable and every access costs a `LengthAtLeast` guard.
            match cand.counted.prove_index_in_bounds_of(
                &access.index,
                Option::None,
                IntRange::array_length(),
                cand.env,
            ) {
                BoundsProof::Static => {}
                BoundsProof::Guarded(g) => {
                    // `ArrayElem` always has a base; the fallback keeps the
                    // function total without an `unwrap`.
                    let array = access.class.base().unwrap_or(NO_NODE);
                    for guard in g {
                        let entry = ArrayGuard { array, guard };
                        if !guards.contains(&entry) {
                            guards.push(entry);
                        }
                    }
                }
                BoundsProof::Refused(reason) => refusals.push(VecRefusal::IndexNotProven {
                    node: access.node,
                    reason,
                }),
            }
        }

        // ---- the dependence test --------------------------------------------
        let mut dependences: Vec<Dependence> = Vec::new();
        if let Some(s) = stride {
            for (i, a) in accesses.iter().enumerate() {
                for b in accesses.iter().skip(i) {
                    if let Some(dep) = dependence_between(graph, a, b, s) {
                        dependences.push(dep);
                    }
                }
            }
        }
        let mut max_safe_lanes: Option<usize> = Option::None;
        let mut tightest: Option<Dependence> = Option::None;
        for dep in &dependences {
            match dep.distance {
                // Both orders agree — widening preserves them at any width.
                DepDistance::Same | DepDistance::Forward(_) => {}
                DepDistance::Backward(k) => {
                    let k = usize::try_from(k).unwrap_or(usize::MAX);
                    let tighter = match max_safe_lanes {
                        Option::None => true,
                        Some(m) => k < m,
                    };
                    if tighter {
                        max_safe_lanes = Some(k);
                        tightest = Some(*dep);
                    }
                }
                DepDistance::Unknown => refusals.push(VecRefusal::UnknownAliasing {
                    a: dep.source,
                    b: dep.sink,
                }),
            }
        }
        if let (Some(m), Some(dep)) = (max_safe_lanes, tightest) {
            if m < 2 {
                refusals.push(VecRefusal::LoopCarriedDependence(dep));
            }
        }

        // ---- arithmetic: NaN, reassociation, ISA features -------------------
        for a in cand.arith {
            let fp = is_float(a.elem);
            match a.op {
                // `Math.min`/`Math.max` order -0.0 below +0.0 and are
                // NaN-propagating; MINPD/MAXPD are neither. Not a relaxation.
                VecOp::Min | VecOp::Max if fp => {
                    refusals.push(VecRefusal::NonIeeeVectorOp {
                        elem: a.elem,
                        op: a.op,
                    });
                    continue;
                }
                // Integer division traps per element (`/ 0`, and
                // `MIN_VALUE / -1` overflows); a lane cannot raise.
                VecOp::Div | VecOp::Rem if !fp => {
                    refusals.push(VecRefusal::NonIeeeVectorOp {
                        elem: a.elem,
                        op: a.op,
                    });
                    continue;
                }
                _ => {}
            }
            if fp && a.reduction && cand.fp == FpRelaxation::Strict {
                refusals.push(VecRefusal::FloatReassociation {
                    elem: a.elem,
                    op: a.op,
                });
                continue;
            }
            // An ISA-conditional refusal, and the condition is about the ISA
            // rather than about any one emitter — which is right. `PMULLD`,
            // `PMINSD` and `PMAXSD` all arrive together in SSE4.1, so the
            // *target* question has one answer for the three.
            //
            // It used to under-report on this tree, because
            // `super::super::vec_emit`'s `arith_opcode` encoded only `VPMULLD`:
            // an `int` min/max cleared here on an SSE4.1-or-better target was
            // admitted and then refused at emission with
            // `VecEmitRefusal::UnsupportedOp`. Round 10 wave 8 closed that by
            // adding the two rows the string names (`VPMINSD` = `VEX.66.0F38 39
            // /r`, `VPMAXSD` = `3D /r`), which is where the fix belonged: an
            // encoding is the emitter's to have, and teaching this function one
            // emitter's opcode table would be the coupling wave 5 refused. So
            // this string is now true about the CPU *and* about the tree.
            //
            // What it still does not say, and cannot: `elem_bytes(a.elem) == 4`
            // means a **`long`** min/max is never refused here. `VPMINSQ` and
            // `VPMAXSQ` are AVX-512, so that loop is admitted and refused at
            // emission with `UnsupportedOp { Long, Min | Max }`. That refusal
            // names its own reason, which is the test
            // `docs/internal/retired/r10-vecwidth-the-gate-admits-five-classes-this-emitter-cannot-encode-20260921-RETIRED-20260922.md`
            // sets for leaving a refusal at the emitter rather than moving it
            // here; expressing it here would need a 64-bit capability flag on
            // `VectorIsa` that no target this tree models would set to `true`.
            if !fp
                && matches!(a.op, VecOp::Mul | VecOp::Min | VecOp::Max)
                && elem_bytes(a.elem) == 4
                && !cand.isa.int32_mul_minmax
            {
                refusals.push(VecRefusal::MissingIsaFeature(
                    "32-bit integer multiply / min / max (PMULLD, PMINSD, PMAXSD — SSE4.1)",
                ));
            }
        }

        // ---- lane count -------------------------------------------------------
        let isa_lanes = cand.isa.lanes_for(elem);
        if isa_lanes < 2 {
            refusals.push(VecRefusal::ElementTooWideForVector {
                elem,
                width_bytes: cand.isa.width_bytes,
            });
        }
        let capped = match max_safe_lanes {
            Option::None => isa_lanes,
            Some(m) => isa_lanes.min(m),
        };
        let lanes = floor_pow2(capped);
        let width_bytes = lanes.saturating_mul(elem_bytes(elem));

        // ---- the width the cap left, against the target's floor --------------
        // Computed here rather than with the alignment block below, because
        // this is where the number is decided and this is the only place that
        // can say *why* it came out narrow. A plan that survives to
        // `vec_emit::emit_vector_loop` with `width_bytes == 8` gets
        // `VecEmitRefusal::UnsupportedWidth { width_bytes: 8, lanes: 2 }` from
        // there, which is true and useless: the emitter cannot know the 8 came
        // from a backward dependence at distance 2, and by then this function
        // has already spent an alignment verdict, a tail strategy and a full
        // guard set on a width the target has no move for.
        //
        // `min_width_bytes` keeps this ISA-generic. The gate does not learn
        // x86's `{16, 32}`; it asks the target what its narrowest vector is,
        // exactly as it asks `int32_mul_minmax` whether `PMULLD` exists.
        //
        // The `lanes >= 2` guard is what keeps this from being a second,
        // vaguer voice on a loop that is already refused: `lanes` is
        // `floor_pow2(min(isa_lanes, m))`, so `lanes < 2` means `isa_lanes < 2`
        // (already `ElementTooWideForVector`, pushed just above) or `m < 2`
        // (already `LoopCarriedDependence`). Those two name the cause better
        // than a width complaint would.
        if lanes >= 2 && width_bytes < cand.isa.min_width_bytes {
            refusals.push(VecRefusal::WidthBelowIsaMinimum {
                lanes,
                width_bytes,
                min_width_bytes: cand.isa.min_width_bytes,
                capped_to: max_safe_lanes,
            });
        }

        // ---- trip count and tail ---------------------------------------------
        let trip = cand.counted.trip_count(cand.env);
        let tail = match trip {
            Option::None => {
                refusals.push(VecRefusal::UnknownTripCount);
                TailStrategy::ScalarRemainder { max_iterations: 0 }
            }
            Some(t) => {
                // Widening: lane count to u64.
                if lanes >= 2 && t.min < lanes as u64 {
                    // The compile-time interval is `[0, i32::MAX]` for
                    // `for (i = 0; i < n; i++)` with a runtime `n`, so refusing
                    // on `t.min` alone refuses almost every real loop —
                    // `docs/jit/trip-count-guards.md` names this as the single
                    // largest source of refusals here. Ask for the runtime
                    // witness instead: one pre-header compare, in the shape
                    // `vec_emit` already discharges.
                    match cand
                        .counted
                        .prove_trip_count_at_least(lanes as u64, cand.env)
                    {
                        // Unreachable in practice — the proof's own early-out
                        // is this same interval — and handled rather than
                        // asserted, because "already proved" means there is
                        // nothing to emit either way.
                        TripCountProof::Static => {}
                        TripCountProof::Guarded(gs) => {
                            for guard in gs {
                                // Not an array obligation: `NO_NODE` keeps it
                                // in its own dedup bucket, so it can never
                                // discharge (or be discharged by) an array's
                                // length guard.
                                let entry = ArrayGuard {
                                    array: NO_NODE,
                                    guard,
                                };
                                if !guards.contains(&entry) {
                                    guards.push(entry);
                                }
                            }
                        }
                        // No runtime check settles it. See the variant's doc
                        // for the four shapes that land here.
                        TripCountProof::Refused(_) => {
                            refusals.push(VecRefusal::TripCountTooSmall {
                                min_trips: t.min,
                                lanes,
                            });
                        }
                    }
                }
                match t.exact() {
                    Some(exact) if lanes >= 2 && exact % lanes as u64 == 0 => TailStrategy::None,
                    _ => TailStrategy::ScalarRemainder {
                        max_iterations: lanes.saturating_sub(1),
                    },
                }
            }
        };

        // ---- alignment ---------------------------------------------------------
        // `width_bytes` is the one decided in the lane-count block above.
        let mut alignment = Alignment::Proven(width_bytes);
        for access in &accesses {
            let first = cand.counted.iv.init.as_constant().and_then(|c| {
                (c as i64)
                    .checked_mul(access.index.scale as i64)?
                    .checked_add(access.index.offset as i64)
            });
            if analyze_alignment(access.elem, first, cand.base_alignment, width_bytes)
                == Alignment::Unknown
            {
                alignment = Alignment::Unknown;
                break;
            }
        }
        if alignment == Alignment::Unknown && cand.isa.alignment == AlignmentPolicy::NaturalRequired
        {
            refusals.push(VecRefusal::UnprovableAlignment { need: width_bytes });
        }

        if !refusals.is_empty() {
            return VecVerdict::Refused(refusals);
        }
        let trip = match trip {
            Some(t) => t,
            // Unreachable: a missing trip count pushed `UnknownTripCount` above.
            Option::None => return VecVerdict::Refused(vec![VecRefusal::UnknownTripCount]),
        };
        VecVerdict::Admitted(VecPlan {
            isa: cand.isa,
            elem,
            lanes,
            width_bytes,
            alignment,
            tail,
            trip,
            overflow,
            guards,
            dependences,
            max_safe_lanes,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::ir::{AccessOffset, IrType, Op, UseLists};
        use crate::scev::{AffineIv, BoundSource, ExitCmp, LoopForm, Stride};

        // ── fixtures ────────────────────────────────────────────────────

        fn empty_graph() -> Graph {
            Graph {
                nodes: Vec::new(),
                entry: 0,
                exit: NO_NODE,
                safepoints: Vec::new(),
                uses: UseLists::new(),
                receiver_param: None,
            }
        }

        /// A reference the memory model knows is a fresh in-method allocation,
        /// so two of them are provably distinct objects.
        fn fresh_array(g: &mut Graph) -> NodeId {
            g.add(
                Op::NewArray {
                    element_type: 10,
                    component_class_id: 0,
                },
                IrType::Ref,
                vec![],
                None,
            )
        }

        /// A reference of unknown provenance. Two of them may be the same
        /// object (`f(x, x)`), which is the runtime-alias case.
        fn param_array(g: &mut Graph, i: u16) -> NodeId {
            g.add(Op::Param(i), IrType::Ref, vec![], None)
        }

        /// A runtime subscript value. Distinct nodes are *not* provably
        /// distinct offsets — only two unequal constants are — so every pair
        /// of accesses to one array reaches the affine test.
        fn subscript(g: &mut Graph) -> NodeId {
            g.add(Op::Add, IrType::Int, vec![], None)
        }

        /// The alias class of `array[index]` at element type `elem`. The gate
        /// reads the element type from here (the memory model), so a fixture
        /// has to state it — `read`/`write` pass their own `elem` through, and
        /// the direct dependence tests below are all `int[]`.
        fn cell(array: NodeId, index: NodeId, elem: MemKind) -> AliasClass {
            AliasClass::ArrayElem {
                array,
                index: AccessOffset::Dynamic(index),
                elem,
            }
        }

        /// `for (int i = start; i < bound; i++)`, IV in local 1.
        fn counted_loop(start: i32, bound: i32) -> CountedLoop {
            CountedLoop {
                header_pc: 0,
                back_edge_pc: 32,
                iv: AffineIv {
                    local: 1,
                    init: IntRange::constant(start),
                    stride: Stride::Const(1),
                },
                cmp: ExitCmp::Ge,
                bound: BoundSource::Const(bound),
                bound_range: IntRange::unknown(),
                form: LoopForm::PreTested,
                modified_locals: 1u64 << 1,
                heap_stable: true,
                has_other_exit: false,
            }
        }

        struct Case {
            counted: CountedLoop,
            env: RangeEnv,
            body: Vec<VecBodyOp>,
            arith: Vec<VecArith>,
            reducible: bool,
            base_alignment: usize,
            fp: FpRelaxation,
            isa: VectorIsa,
        }

        impl Case {
            fn new(body: Vec<VecBodyOp>) -> Case {
                Case {
                    counted: counted_loop(0, 1024),
                    env: RangeEnv::new(),
                    body,
                    arith: Vec::new(),
                    reducible: true,
                    base_alignment: PROVEN_OBJECT_ALIGNMENT,
                    fp: FpRelaxation::Strict,
                    isa: VectorIsa::sse2(),
                }
            }

            fn run(&self, g: &Graph) -> VecVerdict {
                admit_vectorization(
                    g,
                    &VecCandidate {
                        counted: &self.counted,
                        env: &self.env,
                        body: &self.body,
                        arith: &self.arith,
                        reducible: self.reducible,
                        base_alignment: self.base_alignment,
                        fp: self.fp,
                        isa: self.isa,
                    },
                )
            }
        }

        fn read(g: &mut Graph, array: NodeId, offset: i32, elem: MemKind) -> VecBodyOp {
            let ix = subscript(g);
            let node = subscript(g);
            VecBodyOp::array(
                node,
                cell(array, ix, elem),
                AccessKind::Read,
                elem,
                IndexExpr::shifted(1, offset),
            )
        }

        fn write(g: &mut Graph, array: NodeId, offset: i32, elem: MemKind) -> VecBodyOp {
            let ix = subscript(g);
            let node = subscript(g);
            VecBodyOp::array(
                node,
                cell(array, ix, elem),
                AccessKind::Write,
                elem,
                IndexExpr::shifted(1, offset),
            )
        }

        // ── the corpus ──────────────────────────────────────────────────
        //
        // Every refusal class below is paired with a "must admit" positive so
        // that the gate is shown to refuse for the stated reason rather than
        // out of general timidity.

        /// `for i: c[i] = a[i] + b[i]` over three freshly-allocated `int[]`.
        fn independent_int_loop() -> (Graph, Case) {
            let mut g = empty_graph();
            let (a, b, c) = (
                fresh_array(&mut g),
                fresh_array(&mut g),
                fresh_array(&mut g),
            );
            let body = vec![
                read(&mut g, a, 0, MemKind::Int),
                read(&mut g, b, 0, MemKind::Int),
                write(&mut g, c, 0, MemKind::Int),
            ];
            let mut case = Case::new(body);
            case.arith = vec![VecArith {
                node: 0,
                elem: MemKind::Int,
                op: VecOp::Add,
                reduction: false,
            }];
            (g, case)
        }

        /// `for i: sum += a[i]` over `int`. Reassociation of modular integer
        /// addition is exact, so this is admitted under `Strict`.
        fn int_reduction_loop() -> (Graph, Case) {
            let mut g = empty_graph();
            let a = fresh_array(&mut g);
            let body = vec![read(&mut g, a, 0, MemKind::Int)];
            let mut case = Case::new(body);
            case.arith = vec![VecArith {
                node: 0,
                elem: MemKind::Int,
                op: VecOp::Add,
                reduction: true,
            }];
            (g, case)
        }

        /// `for i: c[i] = a[i] + b[i]` over `double`. Element-wise, so no
        /// reassociation and no NaN-ordering question.
        fn fp_elementwise_loop() -> (Graph, Case) {
            let mut g = empty_graph();
            let (a, b, c) = (
                fresh_array(&mut g),
                fresh_array(&mut g),
                fresh_array(&mut g),
            );
            let body = vec![
                read(&mut g, a, 0, MemKind::Double),
                read(&mut g, b, 0, MemKind::Double),
                write(&mut g, c, 0, MemKind::Double),
            ];
            let mut case = Case::new(body);
            case.arith = vec![VecArith {
                node: 0,
                elem: MemKind::Double,
                op: VecOp::Add,
                reduction: false,
            }];
            (g, case)
        }

        /// `for i: sum += a[i]` over `double`.
        fn fp_reduction_loop() -> (Graph, Case) {
            let mut g = empty_graph();
            let a = fresh_array(&mut g);
            let body = vec![read(&mut g, a, 0, MemKind::Double)];
            let mut case = Case::new(body);
            case.arith = vec![VecArith {
                node: 0,
                elem: MemKind::Double,
                op: VecOp::Add,
                reduction: true,
            }];
            (g, case)
        }

        fn fp_reduction_relaxed_loop() -> (Graph, Case) {
            let (g, mut case) = fp_reduction_loop();
            case.fp = FpRelaxation::AllowReassociation;
            (g, case)
        }

        /// `for i: m = Math.max(m, a[i])` over `double`, with reassociation
        /// already permitted. Still refused: `MAXPD` is not `Math.max`.
        fn fp_max_reduction_loop() -> (Graph, Case) {
            let (g, mut case) = fp_reduction_relaxed_loop();
            case.arith = vec![VecArith {
                node: 0,
                elem: MemKind::Double,
                op: VecOp::Max,
                reduction: true,
            }];
            (g, case)
        }

        /// `for (i = 1; i < 1024; i++) a[i] = a[i-1];`
        fn carried_distance_one_loop() -> (Graph, Case) {
            let mut g = empty_graph();
            let a = fresh_array(&mut g);
            let body = vec![
                read(&mut g, a, -1, MemKind::Int),
                write(&mut g, a, 0, MemKind::Int),
            ];
            let mut case = Case::new(body);
            case.counted = counted_loop(1, 1024);
            (g, case)
        }

        /// `for (i = 4; i < 1024; i++) a[i] = a[i-4];` — the same shape at a
        /// distance the vector width fits inside.
        fn carried_distance_four_loop() -> (Graph, Case) {
            carried_distance_loop(4)
        }

        /// `for (i = k; i < 1024; i++) a[i] = a[i-k];` — one `int[]`, a load
        /// `k` elements back and a store at the index, which is a backward
        /// dependence at distance exactly `k`.
        ///
        /// The arithmetic, so the fixtures below do not have to be read back
        /// out of the gate: `dependence_between` computes
        /// `step = scale * stride = 1`, `delta = later.offset - earlier.offset
        /// = 0 - (-k) = k` and `d = -(delta / step) = -k`, and a negative `d`
        /// is `DepDistance::Backward(k)`. So
        /// `max_safe_lanes` comes back `Some(k)`.
        fn carried_distance_loop(k: i32) -> (Graph, Case) {
            let mut g = empty_graph();
            let a = fresh_array(&mut g);
            let body = vec![
                read(&mut g, a, -k, MemKind::Int),
                write(&mut g, a, 0, MemKind::Int),
            ];
            let mut case = Case::new(body);
            case.counted = counted_loop(k, 1024);
            (g, case)
        }

        /// `for (i = 2; i < 1024; i++) a[i] = a[i-2];` — the textbook shape
        /// that caps an `int` vector at two lanes, i.e. at eight bytes, which
        /// no backend in this tree has a move for.
        fn carried_distance_two_loop() -> (Graph, Case) {
            carried_distance_loop(2)
        }

        /// `for (i = 0; i < 1023; i++) a[i] = a[i+1];` — an anti-dependence at
        /// distance 1 that a widened body preserves, because the whole vector
        /// load precedes the whole vector store.
        fn forward_anti_dependence_loop() -> (Graph, Case) {
            let mut g = empty_graph();
            let a = fresh_array(&mut g);
            let body = vec![
                read(&mut g, a, 1, MemKind::Int),
                write(&mut g, a, 0, MemKind::Int),
            ];
            let mut case = Case::new(body);
            case.counted = counted_loop(0, 1023);
            (g, case)
        }

        /// `for (i = 1; i < 1024; i++) { a[i-1] = k; x = a[i]; }` — the same
        /// distance and the same *kind* as the loop above, refused because the
        /// store is the earlier operation.
        fn backward_anti_dependence_loop() -> (Graph, Case) {
            let mut g = empty_graph();
            let a = fresh_array(&mut g);
            let body = vec![
                write(&mut g, a, -1, MemKind::Int),
                read(&mut g, a, 0, MemKind::Int),
            ];
            let mut case = Case::new(body);
            case.counted = counted_loop(1, 1024);
            (g, case)
        }

        /// `void f(int[] a, int[] b) { for i: b[i] = a[i]; }` — two parameters,
        /// which the caller may have passed the same array twice.
        fn aliasing_parameters_loop() -> (Graph, Case) {
            let mut g = empty_graph();
            let (a, b) = (param_array(&mut g, 0), param_array(&mut g, 1));
            let body = vec![
                read(&mut g, a, 0, MemKind::Int),
                write(&mut g, b, 0, MemKind::Int),
            ];
            (g, Case::new(body))
        }

        /// The same shape over two freshly-allocated arrays, which the memory
        /// model proves distinct.
        fn distinct_allocation_loop() -> (Graph, Case) {
            let mut g = empty_graph();
            let (a, b) = (fresh_array(&mut g), fresh_array(&mut g));
            let body = vec![
                read(&mut g, a, 0, MemKind::Int),
                write(&mut g, b, 0, MemKind::Int),
            ];
            (g, Case::new(body))
        }

        /// `for i: dst[i] = src[i];` over `Object[]`.
        fn reference_array_loop() -> (Graph, Case) {
            let mut g = empty_graph();
            let (a, b) = (fresh_array(&mut g), fresh_array(&mut g));
            let body = vec![
                read(&mut g, a, 0, MemKind::Ref),
                write(&mut g, b, 0, MemKind::Ref),
            ];
            (g, Case::new(body))
        }

        fn deopt_in_body_loop() -> (Graph, Case) {
            let (mut g, mut case) = distinct_allocation_loop();
            let guard = subscript(&mut g);
            case.body.insert(
                1,
                VecBodyOp {
                    deopts: true,
                    ..VecBodyOp::pure(guard)
                },
            );
            (g, case)
        }

        fn call_in_body_loop() -> (Graph, Case) {
            let (mut g, mut case) = distinct_allocation_loop();
            let call = subscript(&mut g);
            case.body.insert(
                1,
                VecBodyOp {
                    effect: MemEffect::OPAQUE,
                    ..VecBodyOp::pure(call)
                },
            );
            (g, case)
        }

        fn volatile_in_body_loop() -> (Graph, Case) {
            let (mut g, mut case) = distinct_allocation_loop();
            let v = fresh_array(&mut g);
            let ix = subscript(&mut g);
            let node = subscript(&mut g);
            case.body.insert(
                1,
                VecBodyOp {
                    effect: MemEffect::volatile_read(cell(v, ix, MemKind::Int)),
                    ..VecBodyOp::pure(node)
                },
            );
            (g, case)
        }

        fn irreducible_loop() -> (Graph, Case) {
            let (g, mut case) = independent_int_loop();
            case.reducible = false;
            (g, case)
        }

        fn strict_alignment_unprovable_loop() -> (Graph, Case) {
            let (g, mut case) = distinct_allocation_loop();
            case.isa = VectorIsa::strict_align128();
            (g, case)
        }

        /// A 16-aligned base is NOT by itself enough to prove element-zero
        /// alignment: the element sits at `base + ARRAY_DATA_OFFSET`, and that
        /// offset has been 32, 24 and 16 on this branch (the planned length
        /// prefix makes it 24 again), so it does not always contribute a
        /// multiple of 16. Start at the first index that IS aligned, derived
        /// from the offset rather than restated, so this keeps exercising the
        /// provable branch at whatever the layout becomes next.
        fn strict_alignment_provable_loop() -> (Graph, Case) {
            let (g, mut case) = strict_alignment_unprovable_loop();
            case.base_alignment = 16;
            let elem = elem_bytes(MemKind::Int) as i64;
            let pad = (16 - (ARRAY_DATA_OFFSET as i64 % 16)) % 16;
            assert_eq!(
                pad % elem,
                0,
                "header padding must land on an element boundary"
            );
            case.counted = counted_loop((pad / elem) as i32, 1024);
            (g, case)
        }

        /// `for i: c[i] = a[i] * b[i]` over `int` — needs `PMULLD`.
        fn int_multiply_loop_sse2() -> (Graph, Case) {
            let (g, mut case) = independent_int_loop();
            case.arith = vec![VecArith {
                node: 0,
                elem: MemKind::Int,
                op: VecOp::Mul,
                reduction: false,
            }];
            (g, case)
        }

        fn int_multiply_loop_sse41() -> (Graph, Case) {
            let (g, mut case) = int_multiply_loop_sse2();
            case.isa = VectorIsa::sse41();
            (g, case)
        }

        /// `for i: c[i] = a[i] / b[i]` over `int` — traps per element.
        fn int_divide_loop() -> (Graph, Case) {
            let (g, mut case) = independent_int_loop();
            case.arith = vec![VecArith {
                node: 0,
                elem: MemKind::Int,
                op: VecOp::Div,
                reduction: false,
            }];
            (g, case)
        }

        /// `for (i = 0; i <= Integer.MAX_VALUE; i++)` — the IV wraps and no
        /// runtime guard can prevent it.
        fn wrapping_index_loop() -> (Graph, Case) {
            let (g, mut case) = distinct_allocation_loop();
            case.counted.cmp = ExitCmp::Gt; // `i <= bound`
            case.counted.bound = BoundSource::Const(i32::MAX);
            (g, case)
        }

        /// `for (i = 0; i <= n; i++)` with `n` near `Integer.MAX_VALUE`: the
        /// no-wrap proof survives, but only behind a pre-header guard.
        fn guarded_overflow_loop() -> (Graph, Case) {
            let (g, mut case) = distinct_allocation_loop();
            case.counted.cmp = ExitCmp::Gt;
            case.counted.bound = BoundSource::Local(2);
            case.env
                .bind_local(2, IntRange::new(i32::MAX - 100, i32::MAX));
            (g, case)
        }

        fn non_unit_stride_loop() -> (Graph, Case) {
            let (g, mut case) = distinct_allocation_loop();
            case.counted.iv.stride = Stride::Const(2);
            (g, case)
        }

        /// `for (i = 0; i < n; i++)` with nothing known about `n`, so the loop
        /// may run zero times.
        fn unknown_trip_loop() -> (Graph, Case) {
            let (g, mut case) = distinct_allocation_loop();
            case.counted.bound = BoundSource::Local(2);
            (g, case)
        }

        /// `for (i = 0; i < 2; i++) b[i] = a[i];` — a trip count KNOWN to be
        /// below the lane count. The runtime witness cannot rescue this one:
        /// there is nothing to discover at run time, the answer is already
        /// known and it is "no".
        fn small_trip_loop() -> (Graph, Case) {
            let (g, mut case) = distinct_allocation_loop();
            case.counted = counted_loop(0, 2);
            (g, case)
        }

        /// `for (i = 0; i < 1023; i++) b[i] = a[i];` — a trip count the lane
        /// count does not divide.
        fn remainder_tail_loop() -> (Graph, Case) {
            let (g, mut case) = distinct_allocation_loop();
            case.counted = counted_loop(0, 1023);
            (g, case)
        }

        /// `for i: c[i] = obj.field;` — a field read is not an array element.
        fn field_access_loop() -> (Graph, Case) {
            let mut g = empty_graph();
            let obj = fresh_array(&mut g);
            let c = fresh_array(&mut g);
            let node = subscript(&mut g);
            let body = vec![
                VecBodyOp {
                    effect: MemEffect::read(AliasClass::Field {
                        base: obj,
                        offset: AccessOffset::Const(0),
                    }),
                    elem: Some(MemKind::Int),
                    index: Some(IndexExpr::identity(1)),
                    ..VecBodyOp::pure(node)
                },
                write(&mut g, c, 0, MemKind::Int),
            ];
            (g, Case::new(body))
        }

        /// Every loop the gate is measured against, with the verdict each one
        /// is expected to get.
        #[allow(clippy::type_complexity)]
        fn corpus() -> Vec<(&'static str, Graph, Case, bool)> {
            let entries: Vec<(&'static str, fn() -> (Graph, Case), bool)> = vec![
                ("independent int", independent_int_loop, true),
                ("int reduction", int_reduction_loop, true),
                ("fp element-wise", fp_elementwise_loop, true),
                ("fp reduction (strict)", fp_reduction_loop, false),
                ("fp reduction (relaxed)", fp_reduction_relaxed_loop, true),
                ("fp max reduction", fp_max_reduction_loop, false),
                ("carried distance 1", carried_distance_one_loop, false),
                // The `WidthBelowIsaMinimum` must-refuse half. Its must-admit
                // twin is the distance-4 entry below it: same loop shape, same
                // element type, the one difference being a distance that leaves
                // a whole register's worth of lanes.
                ("carried distance 2", carried_distance_two_loop, false),
                ("carried distance 4", carried_distance_four_loop, true),
                (
                    "forward anti-dependence",
                    forward_anti_dependence_loop,
                    true,
                ),
                (
                    "backward anti-dependence",
                    backward_anti_dependence_loop,
                    false,
                ),
                ("aliasing parameters", aliasing_parameters_loop, false),
                ("distinct allocations", distinct_allocation_loop, true),
                ("reference array", reference_array_loop, false),
                ("deopt in body", deopt_in_body_loop, false),
                ("call in body", call_in_body_loop, false),
                ("volatile in body", volatile_in_body_loop, false),
                ("irreducible control", irreducible_loop, false),
                (
                    "strict alignment, unprovable",
                    strict_alignment_unprovable_loop,
                    false,
                ),
                (
                    "strict alignment, provable",
                    strict_alignment_provable_loop,
                    true,
                ),
                ("int multiply on sse2", int_multiply_loop_sse2, false),
                ("int multiply on sse4.1", int_multiply_loop_sse41, true),
                ("int divide", int_divide_loop, false),
                ("wrapping index", wrapping_index_loop, false),
                ("guarded overflow", guarded_overflow_loop, true),
                ("non-unit stride", non_unit_stride_loop, false),
                ("remainder tail", remainder_tail_loop, true),
                // The `TripCountTooSmall` pair. It was missing: the corpus is
                // built as must-refuse / must-admit pairs, one per refusal
                // class, and this class had only the fixture below it and no
                // corpus entry at all — which is why asking
                // `prove_trip_count_at_least` for a witness left the old
                // 11-of-27 number untouched instead of moving it.
                ("unknown trip, guarded", unknown_trip_loop, true),
                ("trip below the lane count", small_trip_loop, false),
                ("field access", field_access_loop, false),
            ];
            entries
                .into_iter()
                .map(|(name, build, expect)| {
                    let (g, case) = build();
                    (name, g, case, expect)
                })
                .collect()
        }

        // ── the dependence test, directly ───────────────────────────────

        #[test]
        fn distinct_allocations_have_no_dependence() {
            let mut g = empty_graph();
            let (a, b) = (fresh_array(&mut g), fresh_array(&mut g));
            let (ia, ib) = (subscript(&mut g), subscript(&mut g));
            let load = VecAccess {
                node: 10,
                class: cell(a, ia, MemKind::Int),
                kind: AccessKind::Read,
                elem: MemKind::Int,
                index: IndexExpr::identity(1),
            };
            let store = VecAccess {
                node: 11,
                class: cell(b, ib, MemKind::Int),
                kind: AccessKind::Write,
                elem: MemKind::Int,
                index: IndexExpr::identity(1),
            };
            assert_eq!(dependence_between(&g, &load, &store, 1), None);
        }

        #[test]
        fn two_parameters_alias_with_no_computable_distance() {
            let mut g = empty_graph();
            let (a, b) = (param_array(&mut g, 0), param_array(&mut g, 1));
            let (ia, ib) = (subscript(&mut g), subscript(&mut g));
            let load = VecAccess {
                node: 10,
                class: cell(a, ia, MemKind::Int),
                kind: AccessKind::Read,
                elem: MemKind::Int,
                index: IndexExpr::identity(1),
            };
            let store = VecAccess {
                node: 11,
                class: cell(b, ib, MemKind::Int),
                kind: AccessKind::Write,
                elem: MemKind::Int,
                index: IndexExpr::identity(1),
            };
            let dep = dependence_between(&g, &load, &store, 1).expect("aliasing pair");
            assert_eq!(dep.distance, DepDistance::Unknown);
            assert_eq!(dep.kind, DepKind::Anti);
        }

        #[test]
        fn read_read_pairs_never_depend() {
            let mut g = empty_graph();
            let a = param_array(&mut g, 0);
            let (i1, i2) = (subscript(&mut g), subscript(&mut g));
            let one = VecAccess {
                node: 10,
                class: cell(a, i1, MemKind::Int),
                kind: AccessKind::Read,
                elem: MemKind::Int,
                index: IndexExpr::identity(1),
            };
            let two = VecAccess {
                node: 11,
                class: cell(a, i2, MemKind::Int),
                kind: AccessKind::Read,
                elem: MemKind::Int,
                index: IndexExpr::shifted(1, 3),
            };
            assert_eq!(dependence_between(&g, &one, &two, 1), None);
        }

        #[test]
        fn dependence_distance_is_signed_against_program_order() {
            let mut g = empty_graph();
            let a = fresh_array(&mut g);
            let (i1, i2) = (subscript(&mut g), subscript(&mut g));
            let make = |node, ix, kind, offset| VecAccess {
                node,
                class: cell(a, ix, MemKind::Int),
                kind,
                elem: MemKind::Int,
                index: IndexExpr::shifted(1, offset),
            };

            // `a[i] = a[i-1]`: the load of `a[i-1]` is earlier in program
            // order, and the store it depends on ran an iteration ago.
            let load = make(10, i1, AccessKind::Read, -1);
            let store = make(11, i2, AccessKind::Write, 0);
            let dep = dependence_between(&g, &load, &store, 1).expect("dependence");
            assert_eq!(dep.distance, DepDistance::Backward(1));
            assert_eq!(dep.kind, DepKind::Flow);
            assert_eq!(dep.source, 11, "the store is the source");
            assert_eq!(dep.sink, 10, "the load is the sink");

            // `a[i] = a[i+1]`: same kinds, same program order, opposite sign.
            let load = make(10, i1, AccessKind::Read, 1);
            let store = make(11, i2, AccessKind::Write, 0);
            let dep = dependence_between(&g, &load, &store, 1).expect("dependence");
            assert_eq!(dep.distance, DepDistance::Forward(1));
            assert_eq!(dep.kind, DepKind::Anti);
        }

        #[test]
        fn same_iteration_dependence_does_not_cap_the_width() {
            let mut g = empty_graph();
            let a = fresh_array(&mut g);
            let (i1, i2) = (subscript(&mut g), subscript(&mut g));
            let store = VecAccess {
                node: 10,
                class: cell(a, i1, MemKind::Int),
                kind: AccessKind::Write,
                elem: MemKind::Int,
                index: IndexExpr::identity(1),
            };
            let load = VecAccess {
                node: 11,
                class: cell(a, i2, MemKind::Int),
                kind: AccessKind::Read,
                elem: MemKind::Int,
                index: IndexExpr::identity(1),
            };
            let dep = dependence_between(&g, &store, &load, 1).expect("dependence");
            assert_eq!(dep.distance, DepDistance::Same);
            assert_eq!(dep.kind, DepKind::Flow);
        }

        #[test]
        fn interleaved_subscripts_are_provably_independent() {
            // `a[2i]` and `a[2i+1]` never name the same element.
            let mut g = empty_graph();
            let a = fresh_array(&mut g);
            let (i1, i2) = (subscript(&mut g), subscript(&mut g));
            let even = VecAccess {
                node: 10,
                class: cell(a, i1, MemKind::Int),
                kind: AccessKind::Write,
                elem: MemKind::Int,
                index: IndexExpr {
                    iv_local: 1,
                    scale: 2,
                    offset: 0,
                },
            };
            let odd = VecAccess {
                node: 11,
                class: cell(a, i2, MemKind::Int),
                kind: AccessKind::Read,
                elem: MemKind::Int,
                index: IndexExpr {
                    iv_local: 1,
                    scale: 2,
                    offset: 1,
                },
            };
            assert_eq!(dependence_between(&g, &even, &odd, 1), None);
        }

        // ── admission ───────────────────────────────────────────────────

        #[test]
        fn independent_int_loop_is_admitted_with_a_width_and_a_tail() {
            let (g, case) = independent_int_loop();
            let verdict = case.run(&g);
            let plan = verdict.plan().unwrap_or_else(|| {
                panic!("expected admission, got {:?}", verdict.refusals());
            });
            assert_eq!(plan.elem, MemKind::Int);
            assert_eq!(plan.lanes, 4, "128-bit / 4-byte int");
            assert_eq!(plan.width_bytes, 16);
            assert_eq!(plan.tail, TailStrategy::None, "1024 is a multiple of 4");
            assert_eq!(plan.trip.exact(), Some(1024));
            assert_eq!(plan.max_safe_lanes, None, "no dependence caps the width");
            assert_eq!(plan.overflow, OverflowModel::NoWrapProven);
            assert_eq!(
                plan.alignment,
                Alignment::Unknown,
                "an 8-byte-aligned object base cannot prove 16-byte element alignment"
            );
        }

        #[test]
        fn every_array_keeps_its_own_length_guard() {
            let (g, case) = independent_int_loop();
            let verdict = case.run(&g);
            let plan = verdict.plan().expect("admitted");
            let arrays: std::collections::BTreeSet<NodeId> =
                plan.guards.iter().map(|entry| entry.array).collect();
            assert_eq!(
                arrays.len(),
                3,
                "three arrays must carry three obligations, not one deduplicated guard: {:?}",
                plan.guards
            );
            assert!(plan
                .guards
                .iter()
                .all(|entry| matches!(entry.guard, PreheaderGuard::LengthAtLeast(_))));
        }

        #[test]
        fn a_trip_count_the_lane_count_does_not_divide_gets_a_scalar_tail() {
            let (g, case) = remainder_tail_loop();
            let verdict = case.run(&g);
            let plan = verdict.plan().expect("admitted");
            assert_eq!(plan.trip.exact(), Some(1023));
            assert_eq!(
                plan.tail,
                TailStrategy::ScalarRemainder { max_iterations: 3 }
            );
        }

        // ── refusals, each with its must-admit twin ─────────────────────

        #[test]
        fn a_loop_carried_dependence_is_refused() {
            let (g, case) = carried_distance_one_loop();
            let verdict = case.run(&g);
            assert!(
                verdict
                    .refusals()
                    .iter()
                    .any(|r| matches!(r, VecRefusal::LoopCarriedDependence(_))),
                "{:?}",
                verdict.refusals()
            );

            // Must admit: the same shape at distance 4 fits four lanes.
            let (g, case) = carried_distance_four_loop();
            let verdict = case.run(&g);
            let plan = verdict.plan().unwrap_or_else(|| {
                panic!("expected admission, got {:?}", verdict.refusals());
            });
            assert_eq!(plan.lanes, 4);
            assert_eq!(plan.max_safe_lanes, Some(4));
        }

        /// A dependence cap that lands below the target's narrowest move is
        /// refused **here**, once, naming the cap — not admitted for
        /// `vec_emit` to reject as an unsupported width.
        ///
        /// Round 10 wave 6. Before it,
        /// `r10-vecplan-dependence-capped-widths-are-admitted-but-never-emittable-20260921-RETIRED-20260922.md`:
        /// a 4-byte element with a backward distance of 2 or 3 was admitted at
        /// `lanes = 2, width_bytes = 8`, and `emit_vector_loop` refused every
        /// such plan with `UnsupportedWidth { width_bytes: 8, lanes: 2 }`.
        ///
        /// What the callees actually do, so each assertion follows from a
        /// contract rather than from output that was read back:
        ///
        /// * `dependence_between` on this fixture computes `d = -k` and
        ///   answers `DepDistance::Backward(k)` (its own arithmetic is quoted
        ///   on `carried_distance_loop`), so `max_safe_lanes == Some(k)` and,
        ///   since `k >= 2`, `LoopCarriedDependence` is **not** pushed — that
        ///   one fires only on `m < 2`.
        /// * `lanes = floor_pow2(min(isa_lanes, k))`. `floor_pow2` is the
        ///   largest power of two not greater than its argument, so both `k =
        ///   2` and `k = 3` give 2, and `width_bytes = 2 * 4 = 8`.
        /// * `VectorIsa::sse2().min_width_bytes` and `avx2()`'s are both 16, so
        ///   `8 < 16` and the refusal fires with the plan's own numbers echoed
        ///   back — `lanes` and `width_bytes` as computed, `min_width_bytes`
        ///   from the ISA, `capped_to` the `max_safe_lanes` above.
        ///
        /// The single-refusal assertion is the point of the fix and not
        /// incidental: every other check in `admit_vectorization` passes for
        /// this loop (one `int[]`, unit stride, constant bounds inside the
        /// array, no safepoint or deopt, `UnalignedOk`), which is exactly why
        /// it used to be admitted.
        #[test]
        fn a_dependence_capped_width_below_the_targets_floor_is_refused_at_the_gate() {
            for k in [2usize, 3] {
                let (g, case) = carried_distance_loop(k as i32);
                let verdict = case.run(&g);
                let expected = VecRefusal::WidthBelowIsaMinimum {
                    lanes: 2,
                    width_bytes: 8,
                    min_width_bytes: 16,
                    capped_to: Some(k),
                };
                assert_eq!(
                    verdict.refusals(),
                    &[expected][..],
                    "distance {k} must be refused exactly once, for the width the cap left"
                );
                assert!(
                    !verdict
                        .refusals()
                        .iter()
                        .any(|r| matches!(r, VecRefusal::LoopCarriedDependence(_))),
                    "distance {k} is a SAFE two-lane vector; the reason is that \
                     two `int` lanes have no move, not that the dependence is fatal"
                );
            }

            // Not an artifact of the 128-bit fixture ISA: a wider register
            // makes the capped width no wider, because the cap is the binding
            // constraint.
            let (g, mut case) = carried_distance_loop(2);
            case.isa = VectorIsa::avx2();
            assert!(case.run(&g).refused_for(VecRefusal::WidthBelowIsaMinimum {
                lanes: 2,
                width_bytes: 8,
                min_width_bytes: 16,
                capped_to: Some(2),
            }));

            // Must admit: the same shape at a distance that leaves a whole
            // register. `carried_distance_four_loop` is the corpus twin, and
            // 4 lanes of `int` is exactly the floor.
            let (g, case) = carried_distance_four_loop();
            let verdict = case.run(&g);
            let plan = verdict.plan().unwrap_or_else(|| {
                panic!("expected admission, got {:?}", verdict.refusals());
            });
            assert_eq!(plan.lanes, 4);
            assert_eq!(plan.width_bytes, 16);
            assert_eq!(plan.width_bytes, plan.isa.min_width_bytes);
            assert_eq!(plan.max_safe_lanes, Some(4));

            // And the reason no lane count rescues distances 2 and 3 is
            // arithmetic, not policy: the narrowest encodable `int` vector is
            // four lanes, which is more than either distance, and a vector
            // wider than the distance is wrong code rather than a missed
            // optimization.
            let smallest_encodable_int_lanes =
                VectorIsa::avx2().min_width_bytes / elem_bytes(MemKind::Int);
            assert_eq!(smallest_encodable_int_lanes, 4);
            assert!(
                smallest_encodable_int_lanes > 3,
                "if this ever fails a distance-3 loop became vectorizable and \
                 the refusal above should be revisited, not deleted"
            );
        }

        #[test]
        fn program_order_decides_whether_an_anti_dependence_is_fatal() {
            let (g, case) = forward_anti_dependence_loop();
            assert!(
                case.run(&g).is_admitted(),
                "load-before-store at distance 1 widens correctly"
            );

            let (g, case) = backward_anti_dependence_loop();
            let verdict = case.run(&g);
            assert!(
                verdict
                    .refusals()
                    .iter()
                    .any(|r| matches!(r, VecRefusal::LoopCarriedDependence(_))),
                "store-before-load at distance 1 must not: {:?}",
                verdict.refusals()
            );
        }

        #[test]
        fn an_aliasing_pair_is_refused() {
            let (g, case) = aliasing_parameters_loop();
            let verdict = case.run(&g);
            assert!(
                verdict
                    .refusals()
                    .iter()
                    .any(|r| matches!(r, VecRefusal::UnknownAliasing { .. })),
                "{:?}",
                verdict.refusals()
            );

            // Must admit: the memory model proves two allocations distinct.
            let (g, case) = distinct_allocation_loop();
            assert!(case.run(&g).is_admitted());
        }

        #[test]
        fn a_float_reduction_is_refused_by_default() {
            let (g, case) = fp_reduction_loop();
            let verdict = case.run(&g);
            assert!(verdict.refused_for(VecRefusal::FloatReassociation {
                elem: MemKind::Double,
                op: VecOp::Add,
            }));

            // Must admit: the same reduction with the relaxation asked for.
            let (g, case) = fp_reduction_relaxed_loop();
            assert!(case.run(&g).is_admitted());

            // Must admit: element-wise FP needs no relaxation at all, because
            // a lane computes exactly the scalar IEEE-754 result.
            let (g, case) = fp_elementwise_loop();
            let verdict = case.run(&g);
            let plan = verdict.plan().unwrap_or_else(|| {
                panic!("expected admission, got {:?}", verdict.refusals());
            });
            assert_eq!(plan.lanes, 2, "128-bit / 8-byte double");
        }

        #[test]
        fn nan_semantics_refuse_fp_min_max_even_when_reassociation_is_allowed() {
            // Math.max orders -0.0 below +0.0 and propagates NaN; MAXPD does
            // neither. That is a wrong answer, not a reordered one, so the
            // relaxation flag does not reach it.
            let (g, case) = fp_max_reduction_loop();
            let verdict = case.run(&g);
            assert!(verdict.refused_for(VecRefusal::NonIeeeVectorOp {
                elem: MemKind::Double,
                op: VecOp::Max,
            }));

            // Must admit: integer min/max has no NaN and no signed zero, so
            // the only question is whether the ISA has the instruction.
            let (g, mut case) = independent_int_loop();
            case.isa = VectorIsa::sse41();
            case.arith = vec![VecArith {
                node: 0,
                elem: MemKind::Int,
                op: VecOp::Max,
                reduction: true,
            }];
            assert!(case.run(&g).is_admitted());
        }

        #[test]
        fn integer_overflow_is_lane_exact_but_index_overflow_is_not_assumed() {
            // Modular two's-complement arithmetic is associative and
            // commutative, so reassociating an integer reduction is exact and
            // PADDD wraps exactly like iadd. Admitted under `Strict`.
            let (g, case) = int_reduction_loop();
            assert!(case.run(&g).is_admitted());

            // Integer division is *not* total: `/ 0` throws and
            // `MIN_VALUE / -1` overflows, neither of which a lane can raise.
            let (g, case) = int_divide_loop();
            let verdict = case.run(&g);
            assert!(verdict.refused_for(VecRefusal::NonIeeeVectorOp {
                elem: MemKind::Int,
                op: VecOp::Div,
            }));

            // The *index* arithmetic gets no such licence: an induction
            // variable that can wrap is refused by scev, not widened.
            let (g, case) = wrapping_index_loop();
            let verdict = case.run(&g);
            assert!(
                verdict.refusals().iter().any(|r| matches!(
                    r,
                    VecRefusal::IndexNotProven {
                        reason: RefusalReason::IvMayWrap,
                        ..
                    }
                )),
                "{:?}",
                verdict.refusals()
            );

            // Must admit: a no-wrap proof that holds behind a pre-header guard
            // is admitted — with the guard, and with the model recorded.
            let (g, case) = guarded_overflow_loop();
            let verdict = case.run(&g);
            let plan = verdict.plan().unwrap_or_else(|| {
                panic!("expected admission, got {:?}", verdict.refusals());
            });
            assert_eq!(plan.overflow, OverflowModel::NoWrapGuarded);
            assert!(
                plan.guards
                    .iter()
                    .any(|e| matches!(e.guard, PreheaderGuard::AtMost { .. })),
                "the no-wrap obligation must be carried: {:?}",
                plan.guards
            );
        }

        #[test]
        fn a_reference_array_loop_is_refused() {
            let (g, case) = reference_array_loop();
            let verdict = case.run(&g);
            assert!(
                verdict
                    .refusals()
                    .iter()
                    .any(|r| matches!(r, VecRefusal::GcReferenceAccess(_))),
                "a vector store of oops bypasses the write barrier: {:?}",
                verdict.refusals()
            );

            // Must admit: the same shape over primitives.
            let (g, case) = distinct_allocation_loop();
            assert!(case.run(&g).is_admitted());
        }

        #[test]
        fn a_deopt_point_in_the_body_is_refused() {
            let (g, case) = deopt_in_body_loop();
            let verdict = case.run(&g);
            assert!(
                verdict
                    .refusals()
                    .iter()
                    .any(|r| matches!(r, VecRefusal::DeoptPointInBody(_))),
                "{:?}",
                verdict.refusals()
            );

            // Must admit: the identical loop without the guard node.
            let (g, case) = distinct_allocation_loop();
            assert!(case.run(&g).is_admitted());
        }

        #[test]
        fn a_safepoint_in_the_body_is_refused() {
            let (g, case) = call_in_body_loop();
            let verdict = case.run(&g);
            assert!(
                verdict
                    .refusals()
                    .iter()
                    .any(|r| matches!(r, VecRefusal::SafepointInBody(_))),
                "{:?}",
                verdict.refusals()
            );

            // A volatile access is a different refusal for a different reason:
            // the fence is per-iteration and widening would coalesce it.
            let (g, case) = volatile_in_body_loop();
            let verdict = case.run(&g);
            assert!(
                verdict
                    .refusals()
                    .iter()
                    .any(|r| matches!(r, VecRefusal::OrderedAccess(_))),
                "{:?}",
                verdict.refusals()
            );

            // Must admit: the loop with neither.
            let (g, case) = distinct_allocation_loop();
            assert!(case.run(&g).is_admitted());
        }

        #[test]
        fn irreducible_control_flow_is_refused() {
            let (g, case) = irreducible_loop();
            assert!(case.run(&g).refused_for(VecRefusal::IrreducibleControl));

            let (g, case) = independent_int_loop();
            assert!(case.run(&g).is_admitted());
        }

        #[test]
        fn unprovable_alignment_is_refused_only_where_the_isa_requires_it() {
            let (g, case) = strict_alignment_unprovable_loop();
            assert!(case
                .run(&g)
                .refused_for(VecRefusal::UnprovableAlignment { need: 16 }));

            // Must admit: the same loop on a base the caller can prove aligned.
            let (g, case) = strict_alignment_provable_loop();
            let verdict = case.run(&g);
            let plan = verdict.plan().unwrap_or_else(|| {
                panic!("expected admission, got {:?}", verdict.refusals());
            });
            assert_eq!(plan.alignment, Alignment::Proven(16));

            // And on x86 the same unprovable alignment is not a refusal at
            // all, because MOVDQU is correct.
            let (g, case) = distinct_allocation_loop();
            let verdict = case.run(&g);
            assert_eq!(
                verdict.plan().expect("admitted").alignment,
                Alignment::Unknown
            );
        }

        #[test]
        fn a_missing_isa_feature_is_refused_not_assumed() {
            let (g, case) = int_multiply_loop_sse2();
            let verdict = case.run(&g);
            assert!(
                verdict
                    .refusals()
                    .iter()
                    .any(|r| matches!(r, VecRefusal::MissingIsaFeature(_))),
                "{:?}",
                verdict.refusals()
            );

            let (g, case) = int_multiply_loop_sse41();
            assert!(case.run(&g).is_admitted());
        }

        /// The must-admit / must-refuse pair for the trip-count floor.
        ///
        /// A runtime limit is the commonest loop in Java and its compile-time
        /// interval is `[0, i32::MAX]`; it is admitted behind ONE pre-header
        /// compare, carried in the plan as an obligation with no array. A
        /// *constant* limit below the lane count is still refused, because
        /// there is nothing a runtime check could discover — the answer is
        /// already known and it is "no".
        #[test]
        fn an_unbounded_trip_count_is_guarded_and_a_known_small_one_is_refused() {
            let (g, case) = unknown_trip_loop();
            let verdict = case.run(&g);
            let plan = match &verdict {
                VecVerdict::Admitted(p) => p,
                VecVerdict::Refused(r) => panic!("a runtime trip count must be guarded: {r:?}"),
            };
            let trip_guards: Vec<&ArrayGuard> = plan
                .guards
                .iter()
                .filter(|e| matches!(e.guard, PreheaderGuard::TripCountAtLeast { .. }))
                .collect();
            assert_eq!(trip_guards.len(), 1, "one compare, not one per access");
            assert_eq!(
                trip_guards[0].array, NO_NODE,
                "a trip-count obligation is not about an array"
            );
            match trip_guards[0].guard {
                PreheaderGuard::TripCountAtLeast { minimum, .. } => {
                    assert_eq!(minimum, plan.lanes as u64, "the minimum is the lane count")
                }
                _ => unreachable!("filtered above"),
            }
            // The tail is unchanged: a guard proves a MINIMUM, not a multiple,
            // so the scalar remainder is still needed.
            assert_eq!(
                plan.tail,
                TailStrategy::ScalarRemainder {
                    max_iterations: plan.lanes - 1
                }
            );

            // …and the refusal is not dead: a limit no runtime check can
            // rescue still names it.
            let (g, mut case) = distinct_allocation_loop();
            case.counted = counted_loop(0, 2);
            let verdict = case.run(&g);
            assert!(
                verdict
                    .refusals()
                    .iter()
                    .any(|r| matches!(r, VecRefusal::TripCountTooSmall { min_trips: 2, .. })),
                "a loop known to run twice cannot enter a four-lane body: {:?}",
                verdict.refusals()
            );
        }

        #[test]
        fn a_non_unit_step_is_refused() {
            let (g, case) = non_unit_stride_loop();
            let verdict = case.run(&g);
            assert!(
                verdict
                    .refusals()
                    .iter()
                    .any(|r| matches!(r, VecRefusal::NonUnitStep { .. })),
                "{:?}",
                verdict.refusals()
            );
        }

        #[test]
        fn a_non_array_access_is_refused() {
            let (g, case) = field_access_loop();
            let verdict = case.run(&g);
            assert!(
                verdict
                    .refusals()
                    .iter()
                    .any(|r| matches!(r, VecRefusal::UnstructuredMemoryAccess(_))),
                "{:?}",
                verdict.refusals()
            );
        }

        // ── target detection ────────────────────────────────────────────

        #[test]
        fn the_width_comes_from_the_host_not_from_an_assumption() {
            match VectorIsa::detect() {
                Some(isa) => {
                    assert!(isa.width_bytes >= 16);
                    assert_eq!(isa.width_bytes % 16, 0);
                    assert!(!isa.masked_tail, "no modelled ISA can mask a tail");
                    if isa.width_bytes == 32 {
                        assert!(crate::x64::has_avx2(), "256-bit claimed without AVX2");
                    }
                }
                // Round 10: `None` is now also the right answer on an x86-64
                // host WITHOUT AVX2 — see `detect`'s own argument. The old
                // spelling of this arm (`!cfg!(any(x86_64, aarch64))`) would
                // have failed on exactly those hosts, so it is narrowed to the
                // two cases that really do have an emitter: aarch64, and an
                // x86-64 host that reports AVX2.
                None => assert!(
                    !cfg!(target_arch = "aarch64")
                        && !(cfg!(target_arch = "x86_64") && crate::x64::has_avx2()),
                    "a target this tree can emit vectors for reported no vector unit"
                ),
            }

            // A target with no vector unit refuses outright rather than
            // falling back to some assumed width.
            let (g, mut case) = independent_int_loop();
            case.isa = VectorIsa {
                width_bytes: 0,
                ..VectorIsa::sse2()
            };
            assert!(case.run(&g).refused_for(VecRefusal::NoVectorIsa));
        }

        /// On x86-64, host detection offers AVX2 or it offers nothing — it
        /// never hands back an ISA whose plans `vec_emit` is guaranteed to
        /// refuse.
        ///
        /// What the callee actually returns, so the assertions below follow
        /// from the contract and not from a reading of the output:
        /// [`VectorIsa::detect`] is `Option<VectorIsa>`, and on `x86_64` its
        /// body is `crate::x64::has_avx2().then(VectorIsa::avx2)`. `bool::then`
        /// answers `Some(f())` when the receiver is **true** and `None` when it
        /// is false — so `detect().is_some()` is definitionally
        /// `crate::x64::has_avx2()`, and the payload, when there is one, is
        /// exactly `VectorIsa::avx2()`. Note the polarity: `is_some()` tracks
        /// the feature being PRESENT, which is the opposite of the emitter's
        /// gate (`if !host.has_avx2() { refuse }`) and the reason both sides
        /// are spelled out here.
        ///
        /// The edit that trips this: restoring either of the `sse41()` /
        /// `sse2()` fallbacks that `detect` carried before round 10, on a host
        /// without AVX2.
        #[cfg(target_arch = "x86_64")]
        #[test]
        fn x86_host_detection_is_avx2_or_nothing() {
            let detected = VectorIsa::detect();
            assert_eq!(
                detected.is_some(),
                crate::x64::has_avx2(),
                "detection must offer an ISA exactly when the host has AVX2"
            );
            if let Some(isa) = detected {
                assert_eq!(isa, VectorIsa::avx2(), "the only x86 answer is AVX2");
                assert_eq!(isa.width_bytes, 32);
            }
            // Stated negatively as well, because the failure this pins is a
            // fallback re-appearing rather than AVX2 disappearing: neither
            // 128-bit x86 target is reachable from the host probe, whatever
            // the host is.
            assert_ne!(detected, Some(VectorIsa::sse2()));
            assert_ne!(detected, Some(VectorIsa::sse41()));
            // …while both remain constructible by name, which is what keeps
            // the gate's ISA-conditional refusals testable.
            assert_eq!(VectorIsa::sse2().width_bytes, 16);
            assert!(!VectorIsa::sse2().int32_mul_minmax);
            assert!(VectorIsa::sse41().int32_mul_minmax);
        }

        #[test]
        fn element_zero_alignment_is_not_provable_at_the_current_object_alignment() {
            // The *base* is the limit, not the header: the TLAB grid is
            // 8-byte aligned and `PROVEN_OBJECT_ALIGNMENT` is 8, which is what
            // makes every x86 plan come back `Alignment::Unknown` regardless of
            // the header.
            //
            // Whether element ZERO is 16-aligned on a base the caller CAN prove
            // 16-aligned depends only on `ARRAY_DATA_OFFSET % 16`: yes at 16 or
            // 32, no at 24 (the value the planned array-length prefix brings
            // back). Either answer costs nothing on x86 — `MOVDQU` is correct
            // and the gate does not refuse — but a strict-alignment ISA would
            // have to start at the padded index below. Pin the rule, not one
            // layout's answer to it, so the layout flip does not trip this.
            assert_eq!(ARRAY_DATA_OFFSET % 8, 0);
            let element_zero = if ARRAY_DATA_OFFSET % 16 == 0 {
                Alignment::Proven(16)
            } else {
                Alignment::Unknown
            };
            assert_eq!(
                analyze_alignment(MemKind::Int, Some(0), 16, 16),
                element_zero,
                "element zero is 16-aligned exactly when ARRAY_DATA_OFFSET % 16 == 0"
            );
            // ...and the first index that IS 16-aligned on a 16-aligned base
            // is `ARRAY_DATA_OFFSET % 16` bytes in, not index 0.
            let pad = ((16 - (ARRAY_DATA_OFFSET as i64 % 16)) % 16) / 4;
            assert_eq!(
                analyze_alignment(MemKind::Int, Some(pad), 16, 16),
                Alignment::Proven(16)
            );
            // The next element along is never 16-aligned.
            let _ = pad;
            assert_eq!(
                analyze_alignment(MemKind::Int, Some(1), 64, 16),
                Alignment::Unknown
            );
            // An unknown start index is an unknown alignment, never an
            // assumed one.
            assert_eq!(
                analyze_alignment(MemKind::Int, None, 64, 16),
                Alignment::Unknown
            );
        }

        // ── round 9: producer-independent refusals ──────────────────────

        /// The oop refusal reads the memory model's element type. A producer
        /// that labels an `Object[]` store as `int` is refused as a reference
        /// access anyway, and one whose label merely disagrees with the model
        /// is refused as unstructured rather than trusted.
        #[test]
        fn the_element_type_comes_from_the_memory_model_not_the_producer() {
            let mut g = empty_graph();
            let (a, b) = (fresh_array(&mut g), fresh_array(&mut g));
            let (ia, ib) = (subscript(&mut g), subscript(&mut g));
            let (na, nb) = (subscript(&mut g), subscript(&mut g));
            let body = vec![
                read(&mut g, a, 0, MemKind::Int),
                VecBodyOp::array(
                    nb,
                    cell(b, ib, MemKind::Ref),
                    AccessKind::Write,
                    MemKind::Int, // the producer's (wrong) claim
                    IndexExpr::shifted(1, 0),
                ),
            ];
            let verdict = Case::new(body).run(&g);
            assert!(
                verdict
                    .refusals()
                    .iter()
                    .any(|r| matches!(r, VecRefusal::GcReferenceAccess(n) if *n == nb)),
                "a mislabelled oop store must still be refused as one: {:?}",
                verdict.refusals()
            );

            let body = vec![
                VecBodyOp::array(
                    na,
                    cell(a, ia, MemKind::Int),
                    AccessKind::Read,
                    MemKind::Float,
                    IndexExpr::shifted(1, 0),
                ),
                write(&mut g, b, 0, MemKind::Float),
            ];
            let verdict = Case::new(body).run(&g);
            assert!(
                verdict.refused_for(VecRefusal::UnstructuredMemoryAccess(na)),
                "a producer/model element disagreement is not trusted: {:?}",
                verdict.refusals()
            );
        }

        /// One plan, one `elem`: equal widths of different types and an
        /// accumulator wider than the element are both refused.
        #[test]
        fn mixed_element_types_and_widening_arithmetic_are_refused() {
            // `long s += a[i]` over `int[]`: handed to `vec_emit` this would be
            // folded as a 32-bit reduction.
            let (g, mut case) = int_reduction_loop();
            case.arith = vec![VecArith {
                node: 0,
                elem: MemKind::Long,
                op: VecOp::Add,
                reduction: true,
            }];
            assert!(case.run(&g).refused_for(VecRefusal::MixedElementWidths));

            // `f[i] = a[i]` with `int[]` and `float[]` — both 4 bytes wide.
            let mut g = empty_graph();
            let (a, f) = (fresh_array(&mut g), fresh_array(&mut g));
            let body = vec![
                read(&mut g, a, 0, MemKind::Int),
                write(&mut g, f, 0, MemKind::Float),
            ];
            assert!(Case::new(body)
                .run(&g)
                .refused_for(VecRefusal::MixedElementWidths));

            // Must admit: the same reduction computed in the element type.
            let (g, case) = int_reduction_loop();
            assert!(case.run(&g).is_admitted());
        }

        /// `for (i = n; i > 0; i--) b[-i] = a[-i]` has `scale * stride == 1`
        /// but is not the ascending shape anything downstream can emit.
        #[test]
        fn a_reversed_unit_step_is_refused() {
            let (g, mut case) = distinct_allocation_loop();
            case.counted.iv.stride = Stride::Const(-1);
            for op in case.body.iter_mut() {
                if let Some(ix) = op.index.as_mut() {
                    ix.scale = -1;
                }
            }
            let verdict = case.run(&g);
            assert!(
                verdict.refused_for(VecRefusal::NonUnitStep {
                    scale: -1,
                    stride: -1
                }),
                "{:?}",
                verdict.refusals()
            );
        }

        /// A `break` is internal control flow even when the producer says the
        /// body is reducible; `scev` knows about it, so the gate asks.
        #[test]
        fn a_second_loop_exit_is_refused_as_control_flow() {
            let (g, mut case) = distinct_allocation_loop();
            case.counted.has_other_exit = true;
            assert!(case.run(&g).refused_for(VecRefusal::IrreducibleControl));

            let (g, case) = distinct_allocation_loop();
            assert!(case.run(&g).is_admitted());
        }

        // ── the honest number ───────────────────────────────────────────

        #[test]
        fn the_corpus_verdicts_are_what_the_taxonomy_claims() {
            let mut admitted = 0usize;
            let mut total = 0usize;
            for (name, g, case, expect) in corpus() {
                let verdict = case.run(&g);
                assert_eq!(
                    verdict.is_admitted(),
                    expect,
                    "{name}: {:?}",
                    verdict.refusals()
                );
                total += 1;
                if verdict.is_admitted() {
                    admitted += 1;
                }
            }
            assert_eq!(total, 30);
            assert_eq!(
                admitted, 12,
                "12 of 30 corpus loops admitted; the rest name a refusal class"
            );
        }
    }
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
}
