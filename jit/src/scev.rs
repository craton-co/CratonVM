// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T5.2.1 — Scalar Evolution (SCEV) loop induction variable analysis.
//!
//! Identifies induction variables in JVM bytecode loops by scanning for
//! the `iinc` instruction pattern. An induction variable is a local
//! that increments (or decrements) by a constant stride on every
//! iteration of a counted loop.
//!
//! The analysis feeds into:
//! - Loop unrolling decisions (is the trip count known at compile time?)
//! - SIMD vectorization (is the array index a linear function of an IV?)
//! - Range-check elimination (can we prove the index is always in bounds?)
//!
//! ## Limitations
//!
//! This is a bytecode-level analysis, not an SSA-level one. It
//! recognizes the `iinc local, stride` pattern directly and infers
//! the bound from the loop's exit condition (`if_icmpge local, bound`
//! or similar). Complex induction variables (e.g. `i = i * 2`) are
//! not detected — those require a full SSA-based SCEV like LLVM's.

// Note: SCEV only needs the (header_pc, back_edge_pc) loop pairs,
// not the full BasicBlock type. The caller passes these as `&[(usize, usize)]`.

/// A detected induction variable in a loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InductionVar {
    /// Bytecode PC of the loop header (the first instruction of the
    /// loop body, which is also the target of the back-edge `goto`).
    pub header_pc: usize,
    /// Bytecode PC of the back-edge `goto` that closes the loop.
    pub back_edge_pc: usize,
    /// Local variable index that carries the induction variable.
    pub local: usize,
    /// Constant stride per iteration (from `iinc local, stride`).
    /// Positive = counting up, negative = counting down.
    pub stride: i16,
    /// Initial value of the IV at loop entry, if statically known.
    /// `None` means the initial value is dynamic (loaded from a
    /// parameter or computed before the loop).
    pub init: Option<i32>,
    /// Upper bound local index, if the exit condition is
    /// `if_icmpge iv, bound_local`. `None` means the bound is
    /// unknown or a constant (see `bound_const`).
    pub bound_local: Option<usize>,
    /// Upper bound constant, if the exit condition compares against
    /// a `bipush`/`sipush`/`iconst` immediate.
    pub bound_const: Option<i32>,
}

impl InductionVar {
    /// Compute the trip count when both init and bound are known
    /// constants and the stride is positive.
    pub fn trip_count(&self) -> Option<usize> {
        let init = self.init? as i64;
        let bound = self.bound_const? as i64;
        let stride = self.stride as i64;
        if stride <= 0 {
            return None; // counting down or zero stride
        }
        let trips = (bound - init + stride - 1) / stride;
        if trips <= 0 {
            return Some(0);
        }
        Some(trips as usize)
    }
}

/// Analyze bytecode for induction variables in detected loops.
///
/// `loops` is a list of `(header_pc, back_edge_pc)` pairs produced
/// by the loop-detection pass in `regalloc.rs::detect_loops`. For
/// each loop, the function scans the body for an `iinc` instruction
/// and the header for an `if_icmpge`-shaped exit condition.
///
/// Returns one `InductionVar` per detected IV. A loop with no
/// `iinc` or an unrecognized exit shape is silently skipped.
pub fn analyze_induction_variables(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
) -> Vec<InductionVar> {
    let mut result = Vec::with_capacity(loops.len());

    for &(header_pc, back_edge_pc) in loops {
        if header_pc >= code_len || back_edge_pc >= code_len {
            continue;
        }

        // Scan the loop body [header_pc, back_edge_pc] for `iinc`.
        let mut iinc_local: Option<usize> = None;
        let mut iinc_stride: i16 = 0;
        let mut pc = header_pc;
        while pc <= back_edge_pc && pc < code_len {
            let op = code[pc];
            match op {
                // iinc local, const (3-byte form)
                0x84 if pc + 2 < code_len => {
                    let local = code[pc + 1] as usize;
                    let stride = code[pc + 2] as i8 as i16;
                    // Take the LAST iinc in the body as the IV (the
                    // loop counter increment is typically at the end).
                    iinc_local = Some(local);
                    iinc_stride = stride;
                    pc += 3;
                }
                // wide iinc (6-byte form)
                0xC4 if pc + 1 < code_len && code[pc + 1] == 0x84 && pc + 5 < code_len => {
                    let local = u16::from_be_bytes([code[pc + 2], code[pc + 3]]) as usize;
                    let stride = i16::from_be_bytes([code[pc + 4], code[pc + 5]]);
                    iinc_local = Some(local);
                    iinc_stride = stride;
                    pc += 6;
                }
                _ => {
                    pc += bytecode_len(code, pc, code_len);
                }
            }
        }

        let local = match iinc_local {
            Some(l) => l,
            None => continue, // no iinc in loop body → not a counted loop
        };

        // Look at the loop header for the exit condition. The most
        // common pattern is:
        //   iload <iv>; iload <bound>; if_icmpge <exit>
        // or:
        //   iload <iv>; sipush <N>; if_icmpge <exit>
        let (init, bound_local, bound_const) =
            analyze_loop_exit(code, code_len, header_pc, local);

        result.push(InductionVar {
            header_pc,
            back_edge_pc,
            local,
            stride: iinc_stride,
            init,
            bound_local,
            bound_const,
        });
    }

    result
}

/// Attempt to extract the loop-exit condition from the header.
///
/// Looks for the pattern:
///   `iload <iv>` ; `iload <bound>` or `sipush/bipush/iconst <N>` ; `if_icmpge`
///
/// Returns `(init, bound_local, bound_const)`.
fn analyze_loop_exit(
    code: &[u8],
    code_len: usize,
    header_pc: usize,
    iv_local: usize,
) -> (Option<i32>, Option<usize>, Option<i32>) {
    let mut pc = header_pc;
    // Walk forward up to 10 instructions looking for the exit pattern.
    for _ in 0..10 {
        if pc + 2 >= code_len {
            break;
        }
        let op = code[pc];
        // Check for `iload <iv_local>`
        let is_iv_load = match op {
            0x15 if pc + 1 < code_len => code[pc + 1] as usize == iv_local,
            0x1A..=0x1D => (op - 0x1A) as usize == iv_local,
            _ => false,
        };
        if is_iv_load {
            let next_pc = pc + if op == 0x15 { 2 } else { 1 };
            if next_pc >= code_len {
                break;
            }
            let next_op = code[next_pc];
            // Check: is the next instruction a bound-load or constant?
            let (bound_l, bound_c, cmp_pc) = match next_op {
                // iload <bound>
                0x15 if next_pc + 1 < code_len => {
                    let bl = code[next_pc + 1] as usize;
                    (Some(bl), None, next_pc + 2)
                }
                // iload_0..iload_3
                0x1A..=0x1D => {
                    let bl = (next_op - 0x1A) as usize;
                    (Some(bl), None, next_pc + 1)
                }
                // iconst_m1..iconst_5
                0x02..=0x08 => {
                    let val = (next_op as i32) - 3;
                    (None, Some(val), next_pc + 1)
                }
                // bipush
                0x10 if next_pc + 1 < code_len => {
                    let val = code[next_pc + 1] as i8 as i32;
                    (None, Some(val), next_pc + 2)
                }
                // sipush
                0x11 if next_pc + 2 < code_len => {
                    let val = i16::from_be_bytes([code[next_pc + 1], code[next_pc + 2]]) as i32;
                    (None, Some(val), next_pc + 3)
                }
                _ => {
                    pc += bytecode_len(code, pc, code_len);
                    continue;
                }
            };
            // Check: is the instruction after the bound a comparison branch?
            if cmp_pc < code_len && matches!(code[cmp_pc], 0xA2 | 0xA3 | 0xA4) {
                // if_icmpge / if_icmpgt / if_icmple → exit condition found.
                return (None, bound_l, bound_c);
            }
        }
        pc += bytecode_len(code, pc, code_len);
    }
    (None, None, None)
}

/// Compute the byte length of the instruction at `pc`.
pub fn bytecode_len(code: &[u8], pc: usize, code_len: usize) -> usize {
    if pc >= code_len {
        return 1;
    }
    match code[pc] {
        0x00..=0x0F => 1,
        0x10 => 2,
        0x11 => 3,
        0x12 => 2,
        0x13 | 0x14 => 3,
        0x15..=0x19 => 2,
        0x1A..=0x35 => 1,
        0x36..=0x3A => 2,
        0x3B..=0x56 => 1,
        0x57..=0x5F => 1,
        0x60..=0x83 => 1,
        0x84 => 3,
        0x85..=0x93 => 1,
        0x94..=0x98 => 1,
        0x99..=0xA6 => 3,
        0xA7 => 3,
        0xA8 => 3,
        0xA9 => 2,
        0xAA => {
            let pad = (4 - ((pc + 1) % 4)) % 4;
            let table = pc + 1 + pad;
            if table + 12 > code_len { return 1; }
            let low = i32::from_be_bytes([
                code[table + 4], code[table + 5],
                code[table + 6], code[table + 7],
            ]);
            let high = i32::from_be_bytes([
                code[table + 8], code[table + 9],
                code[table + 10], code[table + 11],
            ]);
            let n = (high as i64 - low as i64 + 1).max(0) as usize;
            1 + pad + 12 + n * 4
        }
        0xAB => {
            let pad = (4 - ((pc + 1) % 4)) % 4;
            let table = pc + 1 + pad;
            if table + 8 > code_len { return 1; }
            let npairs = u32::from_be_bytes([
                code[table + 4], code[table + 5],
                code[table + 6], code[table + 7],
            ]) as usize;
            1 + pad + 8 + npairs * 8
        }
        0xAC..=0xB1 => 1,
        0xB2..=0xB8 => 3,
        0xB9 => 5,
        0xBA => 5,
        0xBB => 3,
        0xBC => 2,
        0xBD => 3,
        0xBE..=0xBF => 1,
        0xC0..=0xC1 => 3,
        0xC2..=0xC3 => 1,
        0xC4 => {
            if pc + 1 >= code_len { return 1; }
            if code[pc + 1] == 0x84 { 6 } else { 4 }
        }
        0xC5 => 4,
        0xC6 | 0xC7 => 3,
        0xC8 | 0xC9 => 5,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_simple_counting_loop() {
        // for (int i = 0; i < 10; i++) { ... }
        // iconst_0; istore_1; [header:] iload_1; bipush 10; if_icmpge exit;
        // ... body ...; iinc 1, 1; goto header; [exit:]
        let code = vec![
            0x03,             // 0: iconst_0
            0x3C,             // 1: istore_1
            // header at PC=2:
            0x1B,             // 2: iload_1
            0x10, 0x0A,       // 3: bipush 10
            0xA2, 0x00, 0x08, // 5: if_icmpge +8 → exit at 13
            // body (nop):
            0x00,             // 8: nop
            // increment:
            0x84, 0x01, 0x01, // 9: iinc 1, 1
            // back-edge:
            0xA7, 0xFF, 0xF5, // 12: goto -11 → PC=2
            // exit:
            0xB1,             // 15: return
        ];
        let loops = vec![(2usize, 12usize)];
        let ivs = analyze_induction_variables(&code, code.len(), &loops);
        assert_eq!(ivs.len(), 1);
        let iv = &ivs[0];
        assert_eq!(iv.local, 1);
        assert_eq!(iv.stride, 1);
        assert_eq!(iv.bound_const, Some(10));
        // init is None because the init is outside the loop body
        // (iconst_0; istore_1 at PC 0-1, before header at PC 2).
        // When init is supplied externally (e.g. from a caller that
        // knows the local was 0-initialized), trip_count works:
        let mut iv_with_init = iv.clone();
        iv_with_init.init = Some(0);
        assert_eq!(iv_with_init.trip_count(), Some(10));
    }

    #[test]
    fn detect_decrementing_loop() {
        // iinc 2, -1 → stride = -1, trip_count = None (negative stride)
        let code = vec![
            0x1C,             // 0: iload_2
            0x10, 0x00,       // 1: bipush 0
            0xA4, 0x00, 0x07, // 3: if_icmple +7 → exit
            0x00,             // 6: nop
            0x84, 0x02, 0xFF, // 7: iinc 2, -1
            0xA7, 0xFF, 0xF5, // 10: goto -11 → PC=0
            0xB1,             // 13: return
        ];
        let loops = vec![(0, 10)];
        let ivs = analyze_induction_variables(&code, code.len(), &loops);
        assert_eq!(ivs.len(), 1);
        assert_eq!(ivs[0].stride, -1);
        assert_eq!(ivs[0].trip_count(), None); // negative stride
    }

    #[test]
    fn no_iinc_means_no_iv() {
        // Loop body with no iinc → not a counted loop.
        let code = vec![
            0x00,             // 0: nop
            0xA7, 0xFF, 0xFD, // 1: goto -3 → PC=0
        ];
        let loops = vec![(0, 1)];
        let ivs = analyze_induction_variables(&code, code.len(), &loops);
        assert!(ivs.is_empty());
    }

    #[test]
    fn bytecode_len_covers_all_opcodes() {
        // Every opcode must return a length ≥ 1 (no infinite loop).
        let mut code = vec![0u8; 256];
        for op in 0..=0xFF_u8 {
            code[0] = op;
            let len = bytecode_len(&code, 0, code.len());
            assert!(len >= 1, "opcode 0x{op:02X} returned len 0");
        }
    }
}
