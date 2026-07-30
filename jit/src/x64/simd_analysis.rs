// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! SIMD loop analysis: vectorization, SuperWord, and loop unswitching.
//!
//! Moved verbatim out of `x64.rs`'s `SIMD loop analysis and vectorization`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

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
        0x3b => Some(0), // istore_0
        0x3c => Some(1), // istore_1
        0x3d => Some(2), // istore_2
        0x3e => Some(3), // istore_3
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

/// Detect a vectorizable double-array-sum pattern in a loop body.
/// Matches: dload_sum, aload_arr, iload_iv, daload, dadd, dstore_sum, iinc iv 1, goto
/// Or:      aload_arr, iload_iv, daload, dload_sum, dadd, dstore_sum, iinc iv 1, goto
pub(super) fn detect_fp_array_sum(
    code: &[u8],
    header: usize,
    back_edge: usize,
    iv_local: usize,
) -> Option<SimdFpArraySum> {
    // back_edge should be a goto instruction
    if code.get(back_edge).copied() != Some(0xa7) {
        return None;
    }
    let back_edge_end = back_edge + 3;

    // Header starts with: iload <iv>, iload <bound>, if_icmpge <exit>
    let mut pc = header;

    let iv_check = extract_iload_local(code, pc)?;
    if iv_check != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    let bound_local = extract_iload_local(code, pc)?;
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    if pc + 2 >= back_edge_end || code[pc] != 0xa2 {
        return None; // expect if_icmpge
    }
    pc += 3;

    // Now match loop body. Two patterns:
    // Pattern A: aload arr, iload iv, daload, dload sum, dadd, dstore sum
    // Pattern B: dload sum, aload arr, iload iv, daload, dadd, dstore sum

    let (array_local, acc_local);

    // Try pattern A: aload arr first
    if let Some(arr) = extract_aload_local(code, pc) {
        let arr_len = if code[pc] == 0x19 { 2 } else { 1 };
        let pc2 = pc + arr_len;

        let iv2 = extract_iload_local(code, pc2);
        if iv2 == Some(iv_local) {
            let iv2_len = if code[pc2] == 0x15 { 2 } else { 1 };
            let pc3 = pc2 + iv2_len;

            // daload (0x31)
            if pc3 < back_edge_end && code[pc3] == 0x31 {
                let pc4 = pc3 + 1;

                // dload sum
                if let Some(sum) = extract_dload_local(code, pc4) {
                    let sum_len = if code[pc4] == 0x18 { 2 } else { 1 };
                    let pc5 = pc4 + sum_len;

                    // dadd (0x63)
                    if pc5 < back_edge_end && code[pc5] == 0x63 {
                        let pc6 = pc5 + 1;

                        // dstore sum
                        if let Some(store_sum) = extract_dstore_local(code, pc6) {
                            if store_sum == sum {
                                let store_len = if code[pc6] == 0x39 { 2 } else { 1 };
                                let pc7 = pc6 + store_len;

                                // iinc iv 1
                                if pc7 + 2 < back_edge_end
                                    && code[pc7] == 0x84
                                    && code[pc7 + 1] as usize == iv_local // Widening: always safe
                                    && code[pc7 + 2] == 0x01
                                    && pc7 + 3 == back_edge
                                {
                                    return Some(SimdFpArraySum {
                                        header_pc: header,
                                        back_edge_pc: back_edge,
                                        iv_local,
                                        acc_local: sum,
                                        array_local: arr,
                                        bound_local,
                                        sse_op: 0x58, // ADDPD
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // If pattern A didn't match fully, fall through
        array_local = 0; // not used
        acc_local = 0;
    } else {
        array_local = 0;
        acc_local = 0;
    }

    // Try pattern B: dload sum first
    if let Some(sum) = extract_dload_local(code, pc) {
        let sum_len = if code[pc] == 0x18 { 2 } else { 1 };
        let pc2 = pc + sum_len;

        if let Some(arr) = extract_aload_local(code, pc2) {
            let arr_len = if code[pc2] == 0x19 { 2 } else { 1 };
            let pc3 = pc2 + arr_len;

            let iv2 = extract_iload_local(code, pc3);
            if iv2 == Some(iv_local) {
                let iv2_len = if code[pc3] == 0x15 { 2 } else { 1 };
                let pc4 = pc3 + iv2_len;

                // daload (0x31)
                if pc4 < back_edge_end && code[pc4] == 0x31 {
                    let pc5 = pc4 + 1;

                    // dadd (0x63)
                    if pc5 < back_edge_end && code[pc5] == 0x63 {
                        let pc6 = pc5 + 1;

                        // dstore sum
                        if let Some(store_sum) = extract_dstore_local(code, pc6) {
                            if store_sum == sum {
                                let store_len = if code[pc6] == 0x39 { 2 } else { 1 };
                                let pc7 = pc6 + store_len;

                                if pc7 + 2 < back_edge_end
                                    && code[pc7] == 0x84
                                    && code[pc7 + 1] as usize == iv_local // Widening: always safe
                                    && code[pc7 + 2] == 0x01
                                    && pc7 + 3 == back_edge
                                {
                                    return Some(SimdFpArraySum {
                                        header_pc: header,
                                        back_edge_pc: back_edge,
                                        iv_local,
                                        acc_local: sum,
                                        array_local: arr,
                                        bound_local,
                                        sse_op: 0x58, // ADDPD
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = (array_local, acc_local); // suppress unused warnings
    None
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
// T5.2.17 — Loop unswitching detection
// ---------------------------------------------------------------------------

/// Candidate for loop unswitching.
///
/// Describes a loop whose body contains a conditional branch on a
/// local that is never written inside the loop. The JIT can legally
/// duplicate the loop into two loops — one for each side of the
/// branch — and hoist the condition check out of the header.
///
/// Detection runs on loops of ≤ `MAX_UNSWITCH_BYTECODES` bytes to
/// bound the code-size blow-up from the duplication.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct LoopUnswitchCandidate {
    /// Bytecode PC of the loop header.
    pub header_pc: usize,
    /// Bytecode PC of the back-edge goto.
    pub back_edge_pc: usize,
    /// Bytecode PC of the invariant conditional branch inside the body.
    pub invariant_branch_pc: usize,
    /// Local variable index whose value is the branch predicate and
    /// that is never re-assigned in the loop body.
    pub invariant_local: usize,
    /// Opcode of the branch (if_icmp*, ifeq, ifne, etc.) so the
    /// emitter can mirror it when duplicating.
    pub branch_op: u8,
}

/// Upper limit on loop body size eligible for unswitching.
///
/// Chosen to match the unrolling budget: duplicating a 32-byte loop
/// doubles to 64 bytes, comparable to the 32-byte unroll limit in the
/// static heuristic. Larger loops have more total cost and less
/// marginal benefit from hoisting a single branch.
pub const MAX_UNSWITCH_BYTECODES: usize = 32;

/// Detect loops eligible for unswitching.
///
/// A loop is a candidate when:
/// 1. Its body size is ≤ `MAX_UNSWITCH_BYTECODES` bytes.
/// 2. The body contains an `ifeq/ifne/.../if_icmp*` instruction.
/// 3. The predicate comes from an `iload` whose local is never
///    written (no `istore`/`iinc`) anywhere in the loop body.
///
/// When multiple branches qualify, the first one is returned. Nested
/// loops are handled by the caller (the `loops` list already
/// enumerates each level independently).
#[allow(dead_code)]
pub(crate) fn detect_loop_unswitch_candidates(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
) -> Vec<LoopUnswitchCandidate> {
    let mut out = Vec::new();
    for &(header, back_edge) in loops {
        if header >= code_len || back_edge >= code_len {
            continue;
        }
        let body_size = back_edge.saturating_sub(header);
        if body_size == 0 || body_size > MAX_UNSWITCH_BYTECODES {
            continue;
        }

        // Collect locals written inside the loop (to exclude them
        // from the invariant set).
        let mut written: u64 = 0;
        {
            let mut pc = header;
            while pc <= back_edge && pc < code_len {
                match code[pc] {
                    // istore/lstore/fstore/dstore/astore <local>
                    0x36..=0x3A if pc + 1 < code_len => {
                        // Widening: u8 -> wider int (bytecode operand byte, value fits)
                        written |= 1u64 << (code[pc + 1] as usize & 0x3F);
                    }
                    // istore_0..istore_3
                    // Widening: u8 -> usize (opcode-relative local index, value fits)
                    0x3B..=0x3E => written |= 1u64 << ((code[pc] - 0x3B) as usize),
                    // lstore_0..lstore_3
                    // Widening: u8 -> usize (opcode-relative local index, value fits)
                    0x3F..=0x42 => written |= 1u64 << ((code[pc] - 0x3F) as usize),
                    // fstore_0..fstore_3
                    // Widening: u8 -> usize (opcode-relative local index, value fits)
                    0x43..=0x46 => written |= 1u64 << ((code[pc] - 0x43) as usize),
                    // dstore_0..dstore_3
                    // Widening: u8 -> usize (opcode-relative local index, value fits)
                    0x47..=0x4A => written |= 1u64 << ((code[pc] - 0x47) as usize),
                    // astore_0..astore_3
                    // Widening: u8 -> usize (opcode-relative local index, value fits)
                    0x4B..=0x4E => written |= 1u64 << ((code[pc] - 0x4B) as usize),
                    // iinc <local>, _
                    0x84 if pc + 1 < code_len => {
                        // Widening: u8 -> wider int (bytecode operand byte, value fits)
                        written |= 1u64 << (code[pc + 1] as usize & 0x3F);
                    }
                    _ => {}
                }
                pc += crate::scev::bytecode_len(code, pc, code_len);
            }
        }

        // Walk again looking for an `iload L; if*` pair where L is not
        // in `written`. The invariant_local must be < 64 so it fits in
        // the bitmask.
        let mut pc = header;
        while pc < back_edge && pc < code_len {
            // Try to extract an iload and its local.
            let (iload_local, iload_len) = match code.get(pc).copied() {
                // Widening: u8 -> usize (opcode-relative local index, value fits)
                Some(0x1A..=0x1D) => (Some((code[pc] - 0x1A) as usize), 1usize),
                // Widening: u8 -> wider int (bytecode operand byte, value fits)
                Some(0x15) if pc + 1 < code_len => (Some(code[pc + 1] as usize), 2usize),
                _ => (None, 0),
            };
            if let Some(local) = iload_local {
                let next_pc = pc + iload_len;
                if next_pc < code_len {
                    let op = code[next_pc];
                    // if_icmpeq..if_icmple need a second iload, so we
                    // match the simpler ifeq..ifle (0x99..=0x9E) that
                    // operate on the single top-of-stack.
                    let is_unary_branch = matches!(op, 0x99..=0x9E);
                    if is_unary_branch && local < 64 && (written & (1u64 << local)) == 0 {
                        out.push(LoopUnswitchCandidate {
                            header_pc: header,
                            back_edge_pc: back_edge,
                            invariant_branch_pc: next_pc,
                            invariant_local: local,
                            branch_op: op,
                        });
                        break; // one candidate per loop is enough
                    }
                }
            }
            pc += crate::scev::bytecode_len(code, pc, code_len);
        }
    }
    out
}
