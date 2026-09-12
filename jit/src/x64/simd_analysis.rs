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
    /// Local index of the loop bound
    pub(super) bound_local: usize,
    /// Whether accumulator is long (i2l + ladd + lstore vs iadd + istore)
    pub(super) acc_is_long: bool,
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

/// Detect a vectorizable int-array-sum pattern in a loop body.
/// Matches: iload_sum, aload_arr, iload_iv, iaload, iadd, istore_sum, iinc iv 1, goto
/// Or with long accumulator: aload_arr, iload_iv, iaload, i2l, lload_sum, ladd, lstore_sum
pub(super) fn detect_int_array_sum(
    code: &[u8],
    header: usize,
    back_edge: usize,
    iv_local: usize,
) -> Option<SimdIntArraySum> {
    // back_edge should be a goto instruction
    if code.get(back_edge).copied() != Some(0xa7) {
        return None;
    }
    let back_edge_end = back_edge + 3;

    // The loop header should start with: iload_iv, iload_bound, if_icmpge exit
    let mut pc = header;

    // Match: iload <iv>
    let iv_check = extract_iload_local(code, pc)?;
    if iv_check != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    // Match: iload <bound>
    let bound_local = extract_iload_local(code, pc)?;
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    // Match: if_icmpge <exit>
    if pc + 2 >= back_edge_end {
        return None;
    }
    if code[pc] != 0xa2 {
        return None;
    }
    pc += 3; // skip if_icmpge + offset

    // Now match loop body: aload arr, iload iv, iaload, (optional i2l), load sum, add, store sum
    // Pattern A (int sum): aload_arr, iload_iv, iaload, iload_sum, iadd (reversed), istore_sum
    // Pattern B: iload_sum, aload_arr, iload_iv, iaload, iadd, istore_sum

    // Try: aload arr, iload iv, iaload
    let array_local = extract_aload_local(code, pc)?;
    pc += if code[pc] == 0x19 { 2 } else { 1 };

    let iv_load2 = extract_iload_local(code, pc)?;
    if iv_load2 != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    // iaload (0x2e)
    if pc >= back_edge_end || code[pc] != 0x2e {
        return None;
    }
    pc += 1;

    // Check for i2l conversion (long accumulator)
    let acc_is_long = pc < back_edge_end && code[pc] == 0x85;
    if acc_is_long {
        pc += 1;
    }

    let acc_local;
    if acc_is_long {
        // Long path: lload sum, ladd, lstore sum
        acc_local = extract_lload_local(code, pc)?;
        pc += if code[pc] == 0x16 { 2 } else { 1 };

        // ladd (0x61)
        if pc >= back_edge_end || code[pc] != 0x61 {
            return None;
        }
        pc += 1;

        // lstore sum
        let store_local = extract_lstore_local(code, pc)?;
        if store_local != acc_local {
            return None;
        }
        pc += if code[pc] == 0x37 { 2 } else { 1 };
    } else {
        // Int path: for simplicity, only support long accumulator (most common for benchmarks)
        return None;
    }

    // Should be followed by iinc iv, 1 and then goto header
    if pc + 2 >= back_edge_end {
        return None;
    }
    if code[pc] != 0x84 {
        return None;
    }
    // Widening: u8 -> wider int (bytecode operand byte, value fits)
    if code[pc + 1] as usize != iv_local {
        // Widening: always safe
        return None;
    }
    if code[pc + 2] != 0x01 {
        return None;
    }
    // pc + 3 should be the back_edge (goto)
    if pc + 3 != back_edge {
        return None;
    }

    Some(SimdIntArraySum {
        header_pc: header,
        back_edge_pc: back_edge,
        iv_local,
        acc_local,
        array_local,
        bound_local,
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
    /// Local index of the loop bound (upper limit of `i`).
    pub bound_local: usize,
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
    if back_edge >= code.len() || code[back_edge] != 0xa7 {
        return None;
    }
    let back_edge_end = back_edge + 3;
    let mut pc = header;

    // Header: iload iv ; iload bound ; if_icmpge exit
    let iv_check = extract_iload_local(code, pc)?;
    if iv_check != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    let bound_local = extract_iload_local(code, pc)?;
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    if pc + 2 >= back_edge_end || code[pc] != 0xa2 {
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
    //!   [`VectorIsa::detect`]. No width is ever assumed.
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

    use crate::ir::{AliasClass, Graph, MemEffect, MemKind, NodeId, NO_NODE};
    use crate::scev::{
        BoundsProof, CountedLoop, IndexExpr, IntRange, OverflowModel, PreheaderGuard, RangeEnv,
        RefusalReason, TripCount, TripCountProof,
    };
    use cratonvm_types::{element_byte_size, ArrayElementType, HEADER_SIZE};

    // -- element widths ----------------------------------------------------

    /// The width in bytes of one array element of `kind`.
    ///
    /// Deferred to `cratonvm_types::element_byte_size` so a layout change
    /// cannot leave a second table here to drift. Array elements in this VM
    /// are natural-width and contiguous from `HEADER_SIZE`, which is what
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

        /// The widest ISA the host actually supports, or `None` when the
        /// target architecture has no modelled vector unit.
        ///
        /// `cfg!` rather than `#[cfg]` so both arms typecheck everywhere; the
        /// `cpu_features` queries already answer `false` off x86-64.
        pub(crate) fn detect() -> Option<VectorIsa> {
            if cfg!(target_arch = "x86_64") {
                if crate::x64::has_avx2() {
                    Some(VectorIsa::avx2())
                } else if crate::x64::has_sse41() {
                    Some(VectorIsa::sse41())
                } else {
                    Some(VectorIsa::sse2())
                }
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
    /// (`HEADER_SIZE % 8 == 0`, asserted at compile time) and nothing pins it
    /// higher. `HEADER_SIZE` is 32, so element 0 of an array sits 32 bytes past
    /// a base that is only 8-byte aligned: **16-byte alignment of an array's
    /// element 0 is not provable today**, and neither is 32-byte. That is why
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
            .and_then(|b| b.checked_add(HEADER_SIZE as i64))
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
        /// `lanes * elem_bytes(elem)`. May be narrower than the register when
        /// a dependence capped the lane count.
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
        /// The body touches elements of two different widths.
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
        if !matches!(class, AliasClass::ArrayElem { .. }) {
            return Err(VecRefusal::UnstructuredMemoryAccess(op.node));
        }
        let elem = match op.elem {
            Some(e) => e,
            Option::None => return Err(VecRefusal::UnstructuredMemoryAccess(op.node)),
        };
        if elem == MemKind::Ref {
            return Err(VecRefusal::GcReferenceAccess(op.node));
        }
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
        if !cand.reducible {
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

        // ---- one element width ---------------------------------------------
        // A lane count is one number, so a body that mixes widths would need
        // two of them and a reconciliation this gate does not describe.
        let elem = accesses[0].elem;
        if accesses
            .iter()
            .any(|a| elem_bytes(a.elem) != elem_bytes(elem))
        {
            refusals.push(VecRefusal::MixedElementWidths);
        }

        // ---- stride and step -----------------------------------------------
        let stride = cand.counted.iv.stride.as_const();
        match stride {
            Option::None => refusals.push(VecRefusal::VariableStride),
            Some(s) => {
                for access in &accesses {
                    let step = (access.index.scale as i64).checked_mul(s as i64);
                    if step != Some(1) {
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
        let width_bytes = lanes.saturating_mul(elem_bytes(elem));
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

        fn cell(array: NodeId, index: NodeId) -> AliasClass {
            AliasClass::ArrayElem {
                array,
                index: AccessOffset::Dynamic(index),
                // These fixtures are the int-array dependence cases; the
                // reference-element refusal has its own fixtures.
                elem: MemKind::Int,
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
                cell(array, ix),
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
                cell(array, ix),
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
            let mut g = empty_graph();
            let a = fresh_array(&mut g);
            let body = vec![
                read(&mut g, a, -4, MemKind::Int),
                write(&mut g, a, 0, MemKind::Int),
            ];
            let mut case = Case::new(body);
            case.counted = counted_loop(4, 1024);
            (g, case)
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
                    effect: MemEffect::volatile_read(cell(v, ix)),
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
        /// alignment: the element sits at `base + HEADER_SIZE`, and the
        /// 2026-08-06 shrink took `HEADER_SIZE` from 32 to 24 — so the header
        /// stopped contributing a multiple of 16 and index 0 stopped being
        /// provably 16-aligned. Start at the first index that IS, derived from
        /// the header rather than restated, so this keeps exercising the
        /// provable branch at whatever the header becomes next.
        fn strict_alignment_provable_loop() -> (Graph, Case) {
            let (g, mut case) = strict_alignment_unprovable_loop();
            case.base_alignment = 16;
            let elem = elem_bytes(MemKind::Int) as i64;
            let pad = (16 - (HEADER_SIZE as i64 % 16)) % 16;
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
                class: cell(a, ia),
                kind: AccessKind::Read,
                elem: MemKind::Int,
                index: IndexExpr::identity(1),
            };
            let store = VecAccess {
                node: 11,
                class: cell(b, ib),
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
                class: cell(a, ia),
                kind: AccessKind::Read,
                elem: MemKind::Int,
                index: IndexExpr::identity(1),
            };
            let store = VecAccess {
                node: 11,
                class: cell(b, ib),
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
                class: cell(a, i1),
                kind: AccessKind::Read,
                elem: MemKind::Int,
                index: IndexExpr::identity(1),
            };
            let two = VecAccess {
                node: 11,
                class: cell(a, i2),
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
                class: cell(a, ix),
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
                class: cell(a, i1),
                kind: AccessKind::Write,
                elem: MemKind::Int,
                index: IndexExpr::identity(1),
            };
            let load = VecAccess {
                node: 11,
                class: cell(a, i2),
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
                class: cell(a, i1),
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
                class: cell(a, i2),
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
                None => assert!(
                    !cfg!(any(target_arch = "x86_64", target_arch = "aarch64")),
                    "a supported target reported no vector unit"
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

        #[test]
        fn element_zero_alignment_is_not_provable_at_the_current_object_alignment() {
            // The *base* is the limit, not the header: the TLAB grid is
            // 8-byte aligned and `PROVEN_OBJECT_ALIGNMENT` is 8, which is what
            // makes every x86 plan come back `Alignment::Unknown` regardless of
            // the header.
            //
            // Since the 2026-08-06 shrink the header no longer contributes a
            // multiple of 16 either (24, not 32), so element ZERO is not
            // 16-aligned even on a base the caller CAN prove 16-aligned. That
            // costs nothing on x86 — `MOVDQU` is correct and the gate does not
            // refuse — but a strict-alignment ISA would have to start at
            // `HEADER_SIZE % 16` bytes in. Pin the fact rather than the old
            // premise.
            // ...and at HEADER_SIZE = 16 it contributes a multiple of 16
            // again, so element ZERO is back to being 16-aligned on a base the
            // caller can prove 16-aligned. The 24-byte header had broken that;
            // this is the shrink handing it back, not a weakened assertion.
            assert_eq!(HEADER_SIZE % 8, 0);
            assert_eq!(HEADER_SIZE % 16, 0);
            assert_eq!(
                analyze_alignment(MemKind::Int, Some(0), 16, 16),
                Alignment::Proven(16),
                "element zero is 16-aligned again now that HEADER_SIZE % 16 == 0"
            );
            // ...and the first index that IS 16-aligned on a 16-aligned base
            // is `HEADER_SIZE % 16` bytes in, not index 0.
            let pad = ((16 - (HEADER_SIZE as i64 % 16)) % 16) / 4;
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
            assert_eq!(total, 29);
            assert_eq!(
                admitted, 12,
                "12 of 29 corpus loops admitted; the rest name a refusal class"
            );
        }
    }
}
