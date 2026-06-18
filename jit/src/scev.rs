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

/// The exit comparison that terminates the loop, recorded precisely so
/// the trip-count computation does not conflate inclusive vs. exclusive
/// bounds or counting-up vs. counting-down loops.
///
/// The JVM `if_icmp*` family branches to the exit target **when the
/// comparison holds**, so the *loop-continues* condition is its negation.
/// For the canonical header shape `iload iv; <bound>; if_icmp<op> exit`:
///
/// | opcode        | exits when   | loop runs while | bound  | direction  |
/// |---------------|--------------|-----------------|--------|------------|
/// | `if_icmpge` Ge | `iv >= bound`| `iv <  bound`   | excl.  | increasing |
/// | `if_icmpgt` Gt | `iv >  bound`| `iv <= bound`   | incl.  | increasing |
/// | `if_icmple` Le | `iv <= bound`| `iv >  bound`   | excl.  | decreasing |
/// | `if_icmplt` Lt | `iv <  bound`| `iv >= bound`   | incl.  | decreasing |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCmp {
    /// `if_icmpge` (0xA2): exit when `iv >= bound`; runs while `iv < bound`.
    Ge,
    /// `if_icmpgt` (0xA3): exit when `iv > bound`; runs while `iv <= bound`.
    Gt,
    /// `if_icmple` (0xA4): exit when `iv <= bound`; runs while `iv > bound`.
    Le,
    /// `if_icmplt` (0xA1): exit when `iv < bound`; runs while `iv >= bound`.
    Lt,
}

impl ExitCmp {
    /// Decode the comparison opcode. Returns `None` for opcodes that are
    /// not one of the four counted-loop exit comparisons.
    fn from_opcode(op: u8) -> Option<ExitCmp> {
        match op {
            0xA1 => Some(ExitCmp::Lt),
            0xA2 => Some(ExitCmp::Ge),
            0xA3 => Some(ExitCmp::Gt),
            0xA4 => Some(ExitCmp::Le),
            _ => None,
        }
    }

    /// `true` when the loop counts **up** (the IV must increase to reach
    /// the exit). `Ge`/`Gt` exit on the high side, so they expect a
    /// positive stride.
    pub fn is_increasing(&self) -> bool {
        matches!(self, ExitCmp::Ge | ExitCmp::Gt)
    }
}

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
    /// The exact exit comparison opcode (`>=`, `>`, `<=`, `<`), recorded
    /// so `trip_count` can apply inclusive-vs-exclusive bound and
    /// increasing-vs-decreasing direction semantics correctly. `None`
    /// when no recognized exit comparison was found.
    pub cmp: Option<ExitCmp>,
}

impl InductionVar {
    /// Compute the exact trip count when both `init` and `bound` are
    /// known constants and the stride / comparator are consistent.
    ///
    /// The previous implementation conflated `if_icmpge` / `if_icmpgt` /
    /// `if_icmple`: it always assumed an exclusive upper bound with a
    /// positive stride, giving an off-by-one count for `>` (`Gt`, which
    /// is *inclusive*) and a wrong direction for `<=` (`Le`, which counts
    /// *down*). This version dispatches on the recorded [`ExitCmp`] and
    /// computes the count precisely for each case.
    ///
    /// All arithmetic is performed in `i64` (so `i32::MIN`/`i32::MAX`
    /// init/bound and full-range strides cannot overflow the
    /// distance computation) and the final ceil-division is guarded so
    /// the result can never wrap.
    pub fn trip_count(&self) -> Option<usize> {
        let init = self.init? as i64;
        let bound = self.bound_const? as i64;
        let cmp = self.cmp?;
        let stride = self.stride as i64;
        if stride == 0 {
            return None; // zero stride → not a counted loop (would never exit)
        }

        // Normalize to: how many steps until the *loop-continues* condition
        // first becomes false. For each comparator the loop runs while:
        //   Ge: iv <  bound   (up,   exclusive high bound)
        //   Gt: iv <= bound   (up,   inclusive high bound)
        //   Le: iv >  bound   (down, exclusive low  bound)
        //   Lt: iv >= bound   (down, inclusive low  bound)
        //
        // Direction must match the stride sign, otherwise the loop either
        // never executes the back-edge as a counted loop or diverges; in
        // those cases we cannot give a static trip count.
        match cmp {
            ExitCmp::Ge | ExitCmp::Gt => {
                // Counting up: stride must be positive.
                if stride <= 0 {
                    return None;
                }
                // `distance` = number of integer units the IV must travel
                // past `init` before the exit comparison holds. For the
                // exclusive bound (`Ge`) the loop stops *at* `bound`; for
                // the inclusive bound (`Gt`) it stops one unit past, so we
                // add 1 to the span.
                //   Ge: trips = ceil((bound - init)        / stride)
                //   Gt: trips = ceil((bound - init + 1)    / stride)
                if bound < init {
                    return Some(0); // already past the bound at entry
                }
                // bound - init is non-negative and fits in i64 (both are
                // i32-ranged). Adding 1 for the inclusive case stays in i64.
                let span = (bound - init) + if cmp == ExitCmp::Gt { 1 } else { 0 };
                Some(ceil_div_u(span, stride))
            }
            ExitCmp::Le | ExitCmp::Lt => {
                // Counting down: stride must be negative.
                if stride >= 0 {
                    return None;
                }
                let mag = -stride; // positive magnitude of the (negative) stride
                //   Le: runs while iv >  bound → span = init - bound
                //   Lt: runs while iv >= bound → span = init - bound + 1
                if init < bound {
                    return Some(0); // already past the (lower) bound at entry
                }
                let span = (init - bound) + if cmp == ExitCmp::Lt { 1 } else { 0 };
                Some(ceil_div_u(span, mag))
            }
        }
    }
}

/// Ceiling division of a non-negative numerator by a positive divisor,
/// overflow-safe. `num >= 0` and `den > 0` are required by callers; the
/// `(num + den - 1)` form is avoided so a near-`i64::MAX` numerator can
/// never overflow.
fn ceil_div_u(num: i64, den: i64) -> usize {
    debug_assert!(num >= 0 && den > 0);
    let q = num / den;
    let r = num % den;
    let trips = if r > 0 { q + 1 } else { q };
    // `trips` is bounded by `num` (den >= 1) and num is non-negative, so
    // this cast is always valid on the 64-bit targets the JIT supports.
    trips as usize
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
        let (init, bound_local, bound_const, cmp) =
            analyze_loop_exit(code, code_len, header_pc, local);

        result.push(InductionVar {
            header_pc,
            back_edge_pc,
            local,
            stride: iinc_stride,
            init,
            bound_local,
            bound_const,
            cmp,
        });
    }

    result
}

/// Attempt to extract the loop-exit condition from the header.
///
/// Looks for the pattern:
///   `iload <iv>` ; `iload <bound>` or `sipush/bipush/iconst <N>` ;
///   `if_icmp{lt,ge,gt,le}`
///
/// Returns `(init, bound_local, bound_const, cmp)`, where `cmp` is the
/// precise exit comparator so the trip-count computation does not have
/// to guess inclusive-vs-exclusive bound or loop direction.
fn analyze_loop_exit(
    code: &[u8],
    code_len: usize,
    header_pc: usize,
    iv_local: usize,
) -> (Option<i32>, Option<usize>, Option<i32>, Option<ExitCmp>) {
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
            // Check: is the instruction after the bound a recognized
            // exit comparison branch? Decode the *exact* opcode so the
            // trip-count computation can distinguish >=, >, <=, < instead
            // of lumping them together (the original off-by-one / wrong-
            // direction bug).
            if cmp_pc < code_len {
                if let Some(cmp) = ExitCmp::from_opcode(code[cmp_pc]) {
                    return (None, bound_l, bound_c, Some(cmp));
                }
            }
        }
        pc += bytecode_len(code, pc, code_len);
    }
    (None, None, None, None)
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
        // HIGH security fix: bound switch table size against adversarial
        // overflow (see `x64::checked_tableswitch_count`). On overflow / cap
        // exceeded we return 1 — a safe PC advance; the JIT entry point
        // re-validates and bails the whole compile.
        0xAA => {
            let pad = (4 - ((pc + 1) % 4)) % 4;
            let table = pc + 1 + pad;
            if table + 12 > code_len {
                return 1;
            }
            let low = i32::from_be_bytes([
                code[table + 4],
                code[table + 5],
                code[table + 6],
                code[table + 7],
            ]);
            let high = i32::from_be_bytes([
                code[table + 8],
                code[table + 9],
                code[table + 10],
                code[table + 11],
            ]);
            let n = match crate::x64::checked_tableswitch_count(low, high) {
                Some(c) => c,
                None => return 1,
            };
            match n.checked_mul(4).and_then(|x| x.checked_add(1 + pad + 12)) {
                Some(len) => len,
                None => 1,
            }
        }
        0xAB => {
            let pad = (4 - ((pc + 1) % 4)) % 4;
            let table = pc + 1 + pad;
            if table + 8 > code_len {
                return 1;
            }
            // Read as i32 first to detect negative values explicitly; the
            // historical `u32` cast silently accepted huge "negative"
            // npairs and let them propagate into address arithmetic.
            let npairs_raw = i32::from_be_bytes([
                code[table + 4],
                code[table + 5],
                code[table + 6],
                code[table + 7],
            ]);
            let npairs = match crate::x64::checked_lookupswitch_npairs(npairs_raw) {
                Some(n) => n,
                None => return 1,
            };
            match npairs
                .checked_mul(8)
                .and_then(|x| x.checked_add(1 + pad + 8))
            {
                Some(len) => len,
                None => 1,
            }
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
            if pc + 1 >= code_len {
                return 1;
            }
            if code[pc + 1] == 0x84 {
                6
            } else {
                4
            }
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
            0x03, // 0: iconst_0
            0x3C, // 1: istore_1
            // header at PC=2:
            0x1B, // 2: iload_1
            0x10, 0x0A, // 3: bipush 10
            0xA2, 0x00, 0x08, // 5: if_icmpge +8 → exit at 13
            // body (nop):
            0x00, // 8: nop
            // increment:
            0x84, 0x01, 0x01, // 9: iinc 1, 1
            // back-edge:
            0xA7, 0xFF, 0xF5, // 12: goto -11 → PC=2
            // exit:
            0xB1, // 15: return
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
        assert_eq!(iv.cmp, Some(ExitCmp::Ge)); // if_icmpge decoded precisely
        let mut iv_with_init = iv.clone();
        iv_with_init.init = Some(0);
        // i = 0; i < 10; i++ → exclusive upper bound, increasing → 10 trips.
        assert_eq!(iv_with_init.trip_count(), Some(10));
    }

    #[test]
    fn detect_decrementing_loop() {
        // iload_2; bipush 0; if_icmple exit → exits when iv <= 0, i.e. the
        // loop runs while iv > 0 (decreasing, exclusive low bound). The
        // iinc is -1 so stride matches the down direction.
        let code = vec![
            0x1C, // 0: iload_2
            0x10, 0x00, // 1: bipush 0
            0xA4, 0x00, 0x07, // 3: if_icmple +7 → exit
            0x00, // 6: nop
            0x84, 0x02, 0xFF, // 7: iinc 2, -1
            0xA7, 0xFF, 0xF5, // 10: goto -11 → PC=0
            0xB1, // 13: return
        ];
        let loops = vec![(0, 10)];
        let ivs = analyze_induction_variables(&code, code.len(), &loops);
        assert_eq!(ivs.len(), 1);
        assert_eq!(ivs[0].stride, -1);
        assert_eq!(ivs[0].cmp, Some(ExitCmp::Le)); // if_icmple decoded
        // init is None (set before the loop) → trip_count unknown.
        assert_eq!(ivs[0].trip_count(), None);
        // With a known init, the decreasing-loop count is now computed
        // correctly (the old code could not handle negative strides at all).
        let mut with_init = ivs[0].clone();
        with_init.init = Some(10);
        // i = 10; i > 0; i-- → 10 trips (exclusive low bound).
        assert_eq!(with_init.trip_count(), Some(10));
    }

    /// Build a minimal `InductionVar` for direct `trip_count` unit tests,
    /// independent of bytecode decoding.
    fn iv(init: i32, bound: i32, stride: i16, cmp: ExitCmp) -> InductionVar {
        InductionVar {
            header_pc: 0,
            back_edge_pc: 0,
            local: 0,
            stride,
            init: Some(init),
            bound_local: None,
            bound_const: Some(bound),
            cmp: Some(cmp),
        }
    }

    #[test]
    fn trip_count_ge_exclusive_increasing() {
        // for (i = 0; i < 10; i++)  → if_icmpge, exclusive bound.
        assert_eq!(iv(0, 10, 1, ExitCmp::Ge).trip_count(), Some(10));
        // Non-unit stride: i += 3 over [0,10) → 0,3,6,9 → 4 trips.
        assert_eq!(iv(0, 10, 3, ExitCmp::Ge).trip_count(), Some(4));
        // Already at/over the bound → zero trips.
        assert_eq!(iv(10, 10, 1, ExitCmp::Ge).trip_count(), Some(0));
        assert_eq!(iv(15, 10, 1, ExitCmp::Ge).trip_count(), Some(0));
        // Wrong direction (negative stride with a high-side exit) → unknown.
        assert_eq!(iv(0, 10, -1, ExitCmp::Ge).trip_count(), None);
    }

    #[test]
    fn trip_count_gt_inclusive_increasing() {
        // for (i = 0; i <= 10; i++) → if_icmpgt, inclusive bound → 11 trips.
        assert_eq!(iv(0, 10, 1, ExitCmp::Gt).trip_count(), Some(11));
        // Inclusive bound is exactly one more trip than the exclusive Ge.
        assert_eq!(
            iv(0, 10, 1, ExitCmp::Gt).trip_count().unwrap(),
            iv(0, 10, 1, ExitCmp::Ge).trip_count().unwrap() + 1
        );
        // i += 3 over [0,10] → 0,3,6,9 → 4 trips (10 not hit but bound incl).
        assert_eq!(iv(0, 10, 3, ExitCmp::Gt).trip_count(), Some(4));
        // i += 5 over [0,10] → 0,5,10 → 3 trips (the inclusive endpoint runs).
        assert_eq!(iv(0, 10, 5, ExitCmp::Gt).trip_count(), Some(3));
    }

    #[test]
    fn trip_count_le_exclusive_decreasing() {
        // for (i = 10; i > 0; i--) → if_icmple, exclusive low bound → 10 trips.
        assert_eq!(iv(10, 0, -1, ExitCmp::Le).trip_count(), Some(10));
        // i -= 3 over (0,10] → 10,7,4,1 → 4 trips.
        assert_eq!(iv(10, 0, -3, ExitCmp::Le).trip_count(), Some(4));
        // Already at/below the low bound → zero trips.
        assert_eq!(iv(0, 0, -1, ExitCmp::Le).trip_count(), Some(0));
        // Wrong direction (positive stride with a low-side exit) → unknown.
        assert_eq!(iv(10, 0, 1, ExitCmp::Le).trip_count(), None);
    }

    #[test]
    fn trip_count_lt_inclusive_decreasing() {
        // for (i = 10; i >= 0; i--) → if_icmplt, inclusive low bound → 11 trips.
        assert_eq!(iv(10, 0, -1, ExitCmp::Lt).trip_count(), Some(11));
        // Inclusive bound is exactly one more trip than the exclusive Le.
        assert_eq!(
            iv(10, 0, -1, ExitCmp::Lt).trip_count().unwrap(),
            iv(10, 0, -1, ExitCmp::Le).trip_count().unwrap() + 1
        );
        // i -= 4 over [0,10] → 10,6,2 → 3 trips.
        assert_eq!(iv(10, 0, -4, ExitCmp::Lt).trip_count(), Some(3));
    }

    #[test]
    fn trip_count_zero_stride_is_unknown() {
        // Zero stride never advances → not a counted loop.
        assert_eq!(iv(0, 10, 0, ExitCmp::Ge).trip_count(), None);
        assert_eq!(iv(10, 0, 0, ExitCmp::Le).trip_count(), None);
    }

    #[test]
    fn trip_count_overflow_guarded() {
        // Full-range init/bound must not overflow the distance math.
        // i = i32::MIN; i < i32::MAX; i++ → span = 2^32 - 1, fits in usize
        // on 64-bit; the i64 arithmetic must not panic or wrap.
        let span = (i32::MAX as i64) - (i32::MIN as i64); // 4294967295
        assert_eq!(
            iv(i32::MIN, i32::MAX, 1, ExitCmp::Ge).trip_count(),
            Some(span as usize)
        );
        // Inclusive variant adds exactly one more trip without overflow.
        assert_eq!(
            iv(i32::MIN, i32::MAX, 1, ExitCmp::Gt).trip_count(),
            Some((span + 1) as usize)
        );
        // Large negative stride near i16::MIN must not overflow when negated.
        // i = i32::MAX; i >= i32::MIN; i -= 32768.
        let mag = 32768_i64;
        let expected = ((i32::MAX as i64 - i32::MIN as i64) + 1 + mag - 1) / mag;
        assert_eq!(
            iv(i32::MAX, i32::MIN, i16::MIN, ExitCmp::Lt).trip_count(),
            Some(expected as usize)
        );
    }

    #[test]
    fn trip_count_requires_cmp() {
        // Missing comparator → cannot compute a trip count.
        let mut v = iv(0, 10, 1, ExitCmp::Ge);
        v.cmp = None;
        assert_eq!(v.trip_count(), None);
    }

    #[test]
    fn no_iinc_means_no_iv() {
        // Loop body with no iinc → not a counted loop.
        let code = vec![
            0x00, // 0: nop
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
