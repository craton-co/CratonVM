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
    pub fn from_opcode(op: u8) -> Option<ExitCmp> {
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

    /// `true` when the *bound itself* is an executed value of the induction
    /// variable — the `<=` / `>=` continue conditions (`if_icmpgt` /
    /// `if_icmplt` exits). This is the distinction the bounds-check
    /// eliminator used to refuse outright: an inclusive loop reaches
    /// `index == bound`, so a `length >= bound` proof is stale by one.
    pub fn is_inclusive(&self) -> bool {
        matches!(self, ExitCmp::Gt | ExitCmp::Lt)
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

// ===========================================================================
// General integer range analysis (deep-research report P1)
//
// `x64/bce.rs` proves array indices in range with a family of one-off
// bytecode patterns: a `+1`-only induction variable, a bound that must be a
// *local*, a whole-method proof that the bound literally IS `A.length`, and a
// hard refusal of every inclusive (`i <= n`) loop for the static path. Each
// new shape has cost another pattern. The types below replace the pattern set
// with one proof:
//
//   * [`IntRange`]      — the lattice. Top is the whole `int` range, bottom is
//                         `Empty`. Nothing optimistic is ever representable:
//                         "we do not know" is `unknown()`, which proves nothing.
//   * [`AffineIv`]      — `iv(k) = init + stride*k`, with direction and
//                         monotonicity derived from the stride's sign.
//   * [`BoundSource`]   — the loop limit: a constant, a local, an
//                         `array.length`, a field, or a `Math.min`/`Math.max`
//                         of those. Not just `array.length`.
//   * [`CountedLoop`]   — IV + comparator + bound + loop form, and the proofs
//                         [`CountedLoop::iv_span`], [`CountedLoop::index_span`],
//                         [`CountedLoop::prove_index_in_bounds`],
//                         [`CountedLoop::trip_count`] and
//                         [`CountedLoop::prove_trip_count_at_least`].
//   * [`PreheaderGuard`]— the residual runtime obligations a proof needs. A
//                         consumer that cannot emit one must treat the proof
//                         as refused.
//
// ## Compile-time facts vs. runtime checks
//
// Every proof here has two ways to succeed and they are kept distinct. A fact
// established at compile time costs nothing and is reported as
// [`BoundsProof::Static`] / [`TripCountProof::Static`]; a fact that only a
// runtime test can settle becomes a [`PreheaderGuard`] the consumer must emit,
// and the verdict says so. The failure mode this structure exists to prevent is
// the third answer — a refusal where one pre-header compare would have done —
// and [`PreheaderGuard::TripCountAtLeast`] closes the largest instance of it:
// `for (i = 0; i < n; i++)` has a compile-time trip-count minimum of zero, so
// every profitability floor (vector lanes, unroll factor, peel count) refused
// it before that shape existed.
//
// ## Overflow model
//
// Java `int` arithmetic wraps silently (JVMS 2.11.3, 6.5 `iadd`). Every
// statement below is therefore explicit about wrapping:
//
//   * lattice arithmetic is evaluated in `i64`. `*_no_wrap` operations return
//     `None` when the exact result does not fit in `i32` — the proof refuses.
//     The wrapping variants (e.g. [`IntRange::add`]) instead fall back to
//     [`IntRange::unknown`], which is sound (a wrapped `int` is still *some*
//     `int`) but useless for a bounds proof, so the two are never confused.
//   * the induction variable's own advance is the dangerous one: for an
//     increasing loop the last executed value `v` satisfies `v <= bound +
//     addend`, and the *next* value `v + stride` must not overflow, or the IV
//     wraps negative while the exit test keeps passing and every elided index
//     walks below the array base. That obligation is discharged statically
//     when the bound's range makes it impossible, and otherwise becomes a
//     [`PreheaderGuard::AtMost`] / [`PreheaderGuard::AtLeast`]. It is never
//     assumed.
//   * a variable stride has no compile-time sign. It is admitted only behind
//     [`PreheaderGuard::StrideInRange`], which pins both sign and magnitude.
//
// ## Producer obligations (checked by nobody but the caller)
//
// A [`CountedLoop`] is a *claim* about bytecode. The producer must guarantee:
//
//   1. `iv.local` is written inside the loop body exactly by the recorded
//      stride, and by nothing else;
//   2. `cmp` is the loop's only exit test on `iv.local`, and `form` correctly
//      says whether that test dominates the body ([`LoopForm::PreTested`]) or
//      whether the body runs once before the first test
//      ([`LoopForm::PostTested`]);
//   3. the value an [`IndexExpr`] describes is the IV value *as the exit test
//      saw it* — an index read after an in-body advance must fold that advance
//      into [`IndexExpr::offset`];
//   4. `modified_locals` has a bit set for every local written in the body, and
//      the producer refuses outright when the body writes a local `>= 64`.
//
// Everything after that is proved here.
// ===========================================================================

/// An element of the integer range lattice over the JVM `int` domain.
///
/// Top is `Range { lo: i32::MIN, hi: i32::MAX }` ([`IntRange::unknown`]) and
/// bottom is [`IntRange::Empty`] (no value — unreachable code, or a
/// contradiction between two facts). There is deliberately no "probably"
/// element: a range that cannot be proved is top, and top proves nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntRange {
    /// Bottom — the empty set.
    Empty,
    /// The inclusive interval `[lo, hi]`. Constructors maintain `lo <= hi`.
    Range {
        /// Inclusive lower endpoint.
        lo: i32,
        /// Inclusive upper endpoint.
        hi: i32,
    },
}

impl IntRange {
    /// Top: every `int`. Proves nothing, which is the point.
    pub const fn unknown() -> IntRange {
        IntRange::Range {
            lo: i32::MIN,
            hi: i32::MAX,
        }
    }

    /// Bottom: no value at all.
    pub const fn empty() -> IntRange {
        IntRange::Empty
    }

    /// The inclusive interval `[lo, hi]`, normalising `lo > hi` to
    /// [`IntRange::Empty`].
    pub fn new(lo: i32, hi: i32) -> IntRange {
        if lo > hi {
            IntRange::Empty
        } else {
            IntRange::Range { lo, hi }
        }
    }

    /// The singleton `{v}`.
    pub fn constant(v: i32) -> IntRange {
        IntRange::Range { lo: v, hi: v }
    }

    /// The range of a JVM array length: non-negative, and no larger than
    /// `Integer.MAX_VALUE` by construction (JVMS `newarray` throws
    /// `NegativeArraySizeException` below zero).
    pub fn array_length() -> IntRange {
        IntRange::Range {
            lo: 0,
            hi: i32::MAX,
        }
    }

    /// The endpoints widened to `i64`, or `None` for [`IntRange::Empty`].
    fn i64_bounds(self) -> Option<(i64, i64)> {
        match self {
            IntRange::Empty => None,
            IntRange::Range { lo, hi } => Some((lo as i64, hi as i64)),
        }
    }

    /// Narrow an exact `i64` interval back into the lattice. `None` when the
    /// interval does not fit in `i32` — i.e. the computation that produced it
    /// can wrap. Callers decide whether that is a refusal or a fallback to
    /// [`IntRange::unknown`]; the two are never silently interchanged.
    fn from_i64(lo: i64, hi: i64) -> Option<IntRange> {
        if lo > hi {
            return Some(IntRange::Empty);
        }
        if lo < i32::MIN as i64 || hi > i32::MAX as i64 {
            return None;
        }
        Some(IntRange::Range {
            lo: lo as i32,
            hi: hi as i32,
        })
    }

    /// Whether this is bottom.
    pub fn is_empty(self) -> bool {
        matches!(self, IntRange::Empty)
    }

    /// Whether this is top (the whole `int` range).
    pub fn is_unknown(self) -> bool {
        matches!(self, IntRange::Range { lo, hi } if lo == i32::MIN && hi == i32::MAX)
    }

    /// Inclusive lower endpoint, or `None` for bottom.
    pub fn lo(self) -> Option<i32> {
        match self {
            IntRange::Empty => None,
            IntRange::Range { lo, .. } => Some(lo),
        }
    }

    /// Inclusive upper endpoint, or `None` for bottom.
    pub fn hi(self) -> Option<i32> {
        match self {
            IntRange::Empty => None,
            IntRange::Range { hi, .. } => Some(hi),
        }
    }

    /// The single value this range admits, if it admits exactly one.
    pub fn as_constant(self) -> Option<i32> {
        match self {
            IntRange::Range { lo, hi } if lo == hi => Some(lo),
            _ => None,
        }
    }

    /// Whether `v` is admitted.
    pub fn contains(self, v: i32) -> bool {
        matches!(self, IntRange::Range { lo, hi } if lo <= v && v <= hi)
    }

    /// Whether `other` is a subset of `self`.
    pub fn contains_all(self, other: IntRange) -> bool {
        match (self, other) {
            (_, IntRange::Empty) => true,
            (IntRange::Empty, _) => false,
            (IntRange::Range { lo: al, hi: ah }, IntRange::Range { lo: bl, hi: bh }) => {
                al <= bl && bh <= ah
            }
        }
    }

    /// Lattice join — the interval hull. Used at control-flow merges and to
    /// fold a post-tested loop's unguarded first iteration into the span.
    pub fn join(self, other: IntRange) -> IntRange {
        match (self, other) {
            (IntRange::Empty, r) | (r, IntRange::Empty) => r,
            (IntRange::Range { lo: al, hi: ah }, IntRange::Range { lo: bl, hi: bh }) => {
                IntRange::Range {
                    lo: al.min(bl),
                    hi: ah.max(bh),
                }
            }
        }
    }

    /// Lattice meet — intersection. Two facts about the same value combine
    /// here; a contradiction becomes [`IntRange::Empty`] (a provably
    /// zero-trip loop, not an excuse to pick either side).
    pub fn meet(self, other: IntRange) -> IntRange {
        match (self, other) {
            (IntRange::Empty, _) | (_, IntRange::Empty) => IntRange::Empty,
            (IntRange::Range { lo: al, hi: ah }, IntRange::Range { lo: bl, hi: bh }) => {
                IntRange::new(al.max(bl), ah.min(bh))
            }
        }
    }

    /// Exact addition: `None` when some sum is not representable as an `int`,
    /// i.e. when Java's wrapping addition could be observed.
    pub fn add_no_wrap(self, other: IntRange) -> Option<IntRange> {
        let ((alo, ahi), (blo, bhi)) = match (self.i64_bounds(), other.i64_bounds()) {
            (Some(a), Some(b)) => (a, b),
            _ => return Some(IntRange::Empty),
        };
        IntRange::from_i64(alo + blo, ahi + bhi)
    }

    /// Wrapping addition, modelling `iadd`. A sum that wraps yields
    /// [`IntRange::unknown`] — sound, and useless for a bounds proof, which is
    /// exactly the intent.
    pub fn add(self, other: IntRange) -> IntRange {
        match self.add_no_wrap(other) {
            Some(r) => r,
            None => IntRange::unknown(),
        }
    }

    /// Exact subtraction; `None` when a difference is not representable.
    pub fn sub_no_wrap(self, other: IntRange) -> Option<IntRange> {
        let ((alo, ahi), (blo, bhi)) = match (self.i64_bounds(), other.i64_bounds()) {
            (Some(a), Some(b)) => (a, b),
            _ => return Some(IntRange::Empty),
        };
        IntRange::from_i64(alo - bhi, ahi - blo)
    }

    /// Exact `+ k`; `None` when the shift is not representable.
    pub fn offset_no_wrap(self, k: i32) -> Option<IntRange> {
        self.add_no_wrap(IntRange::constant(k))
    }

    /// Exact `* k`; `None` when a product is not representable. Handles a
    /// negative `k` (which swaps the endpoints) and `k == i32::MIN`.
    pub fn scale_no_wrap(self, k: i32) -> Option<IntRange> {
        let (lo, hi) = match self.i64_bounds() {
            Some(b) => b,
            None => return Some(IntRange::Empty),
        };
        let k = k as i64;
        let a = lo * k;
        let b = hi * k;
        IntRange::from_i64(a.min(b), a.max(b))
    }

    /// Exact negation; `None` for a range containing `i32::MIN` (whose
    /// negation is not an `int`).
    pub fn neg_no_wrap(self) -> Option<IntRange> {
        IntRange::constant(0).sub_no_wrap(self)
    }

    /// `Math.min` of two ranges, elementwise on the endpoints.
    pub fn min_with(self, other: IntRange) -> IntRange {
        match (self, other) {
            (IntRange::Empty, _) | (_, IntRange::Empty) => IntRange::Empty,
            (IntRange::Range { lo: al, hi: ah }, IntRange::Range { lo: bl, hi: bh }) => {
                IntRange::Range {
                    lo: al.min(bl),
                    hi: ah.min(bh),
                }
            }
        }
    }

    /// `Math.max` of two ranges, elementwise on the endpoints.
    pub fn max_with(self, other: IntRange) -> IntRange {
        match (self, other) {
            (IntRange::Empty, _) | (_, IntRange::Empty) => IntRange::Empty,
            (IntRange::Range { lo: al, hi: ah }, IntRange::Range { lo: bl, hi: bh }) => {
                IntRange::Range {
                    lo: al.max(bl),
                    hi: ah.max(bh),
                }
            }
        }
    }

    /// Whether every admitted value is `>= 0`. Bottom is vacuously
    /// non-negative but callers must not lean on that — check
    /// [`IntRange::is_empty`] first when it matters.
    pub fn is_non_negative(self) -> bool {
        match self {
            IntRange::Empty => true,
            IntRange::Range { lo, .. } => lo >= 0,
        }
    }
}

/// The direction an induction variable travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Strictly increasing — positive stride.
    Increasing,
    /// Strictly decreasing — negative stride.
    Decreasing,
    /// Never moves — zero stride. Not a counted loop.
    Static,
    /// The stride's sign is not known until runtime.
    Unknown,
}

/// How an induction variable advances per iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stride {
    /// A compile-time constant step. May be non-unit and may be negative.
    Const(i32),
    /// `iv += <local>` — the step is a runtime value whose sign and magnitude
    /// are unknown at compile time. Admitted only behind
    /// [`PreheaderGuard::StrideInRange`].
    Variable(usize),
}

impl Stride {
    /// The direction implied by the stride, or [`Direction::Unknown`] for a
    /// runtime step.
    pub fn direction(self) -> Direction {
        match self {
            Stride::Const(0) => Direction::Static,
            Stride::Const(s) if s > 0 => Direction::Increasing,
            Stride::Const(_) => Direction::Decreasing,
            Stride::Variable(_) => Direction::Unknown,
        }
    }

    /// The constant step, if it is one.
    pub fn as_const(self) -> Option<i32> {
        match self {
            Stride::Const(s) => Some(s),
            Stride::Variable(_) => None,
        }
    }
}

/// An affine induction variable: `iv(k) = init + stride * k` for iteration
/// `k = 0, 1, 2, …`, **provided no step wraps**. Monotonicity is a
/// consequence of the stride's sign *and* the no-wrap obligation, never of
/// the sign alone — see the module overflow model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AffineIv {
    /// JVM local slot carrying the induction variable.
    pub local: usize,
    /// Range of the value the local holds on entry to the loop (`k == 0`).
    /// [`IntRange::unknown`] when the entry value is a runtime quantity —
    /// the proof then falls back to a [`PreheaderGuard`] on the local.
    pub init: IntRange,
    /// Per-iteration step.
    pub stride: Stride,
}

/// Whether the loop's exit test dominates the loop body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopForm {
    /// The test runs before every body execution (`while`/`for`, including
    /// javac's `goto cond` rotation). Every executed IV value therefore
    /// satisfies the continue condition.
    PreTested,
    /// The body runs once before the first test (`do { } while`). The entry
    /// value is itself an executed value and is folded into the span; a
    /// post-tested loop with an unbounded entry value is refused.
    PostTested,
}

/// Where a loop's limit comes from. The bounds-check eliminator historically
/// only accepted a local whose whole-method provenance proved it was literally
/// `A.length`; every other shape lost its elision. All of these are limits a
/// proof can use, because the proof never assumes what the limit *is* — it
/// only needs the limit's range and its loop-invariance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundSource {
    /// A compile-time constant (`iconst_*` / `bipush` / `sipush`).
    Const(i32),
    /// A local slot holding the limit.
    Local(usize),
    /// `<local>.length` — the array local's length. Known non-negative.
    ArrayLength(usize),
    /// A field read: `getfield` (with a receiver local) or `getstatic`
    /// (`receiver_local == None`). Range unknown; usable only when the loop
    /// body is heap-stable (see [`CountedLoop::heap_stable`]).
    Field {
        /// Constant-pool index of the `CONSTANT_Fieldref`.
        cp_index: u16,
        /// Receiver local for `getfield`; `None` for `getstatic`.
        receiver_local: Option<usize>,
    },
    /// `Math.min(a, b)`.
    Min(Box<BoundSource>, Box<BoundSource>),
    /// `Math.max(a, b)`.
    Max(Box<BoundSource>, Box<BoundSource>),
}

impl BoundSource {
    /// The statically-known range of this limit under `env`.
    ///
    /// Deliberately conservative: a plain local or a field is
    /// [`IntRange::unknown`] unless the environment says otherwise. The proof
    /// does not need a numeric range to succeed — it falls back to a symbolic
    /// pre-header guard — so an unknown here costs a guard, never soundness.
    pub fn range_in(&self, env: &RangeEnv) -> IntRange {
        match self {
            BoundSource::Const(v) => IntRange::constant(*v),
            BoundSource::Local(l) => env.local(*l),
            BoundSource::ArrayLength(l) => env.array_length(*l).meet(IntRange::array_length()),
            BoundSource::Field { .. } => IntRange::unknown(),
            BoundSource::Min(a, b) => a.range_in(env).min_with(b.range_in(env)),
            BoundSource::Max(a, b) => a.range_in(env).max_with(b.range_in(env)),
        }
    }

    /// Whether this limit cannot change while the loop runs.
    ///
    /// A pre-header guard is evaluated once; if the limit could be raised
    /// inside the body, the guard goes stale and a later iteration's exit test
    /// admits an index past the guarded length. `modified_locals` must have a
    /// bit set for every local the body writes; `heap_stable` must be `false`
    /// unless the body provably performs no store or call that could change a
    /// field.
    pub fn is_invariant(&self, modified_locals: u64, heap_stable: bool) -> bool {
        match self {
            BoundSource::Const(_) => true,
            BoundSource::Local(l) | BoundSource::ArrayLength(l) => {
                *l < 64 && (modified_locals & (1u64 << *l)) == 0
            }
            BoundSource::Field { receiver_local, .. } => {
                heap_stable
                    && match receiver_local {
                        None => true,
                        Some(r) => *r < 64 && (modified_locals & (1u64 << *r)) == 0,
                    }
            }
            BoundSource::Min(a, b) | BoundSource::Max(a, b) => {
                a.is_invariant(modified_locals, heap_stable)
                    && b.is_invariant(modified_locals, heap_stable)
            }
        }
    }
}

/// The base of a symbolic endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundTerm {
    /// A literal value — the endpoint is fully known at compile time.
    Const(i32),
    /// The loop's limit expression.
    Bound(BoundSource),
    /// The induction variable's value on entry to the loop, read from its
    /// local in the pre-header. This is the term behind `bce.rs`'s `iv >= 0`
    /// header check.
    IvEntry(usize),
}

/// A symbolic endpoint `base + addend`, evaluated in 64-bit so the endpoint
/// arithmetic itself can never wrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymBound {
    /// The runtime term.
    pub base: BoundTerm,
    /// A small compile-time displacement.
    pub addend: i32,
}

impl SymBound {
    /// A fully-known endpoint.
    pub fn constant(v: i32) -> SymBound {
        SymBound {
            base: BoundTerm::Const(v),
            addend: 0,
        }
    }

    /// The endpoint's value when it is entirely compile-time known.
    pub fn as_const(&self) -> Option<i32> {
        // `BoundTerm::Const(v)` and `BoundTerm::Bound(BoundSource::Const(v))`
        // are two spellings of the same constant — the first is minted by
        // arithmetic here, the second by `decode_bound_expr` reading an
        // `sipush`/`ldc` limit out of the bytecode. Folding only the first made
        // every proof over a literal loop bound answer "symbolic", which cost a
        // preheader guard on `for (i = 0; i < 16; i++) a[i]` with a known-length
        // array and, worse, silently skipped the always-fails refusal below.
        let base = match self.base {
            BoundTerm::Const(v) => Some(v),
            BoundTerm::Bound(BoundSource::Const(v)) => Some(v),
            _ => None,
        }?;
        let sum = base as i64 + self.addend as i64;
        if (i32::MIN as i64..=i32::MAX as i64).contains(&sum) {
            Some(sum as i32)
        } else {
            None
        }
    }

    /// `self + k`, or `None` when the displacement is not representable.
    pub fn offset(&self, k: i32) -> Option<SymBound> {
        let a = self.addend as i64 + k as i64;
        if !(i32::MIN as i64..=i32::MAX as i64).contains(&a) {
            return None;
        }
        Some(SymBound {
            base: self.base.clone(),
            addend: a as i32,
        })
    }
}

/// Compile-time knowledge about the values locals hold on entry to a loop.
///
/// The default environment knows nothing, which is the fail-closed state. The
/// interesting use is nesting: an inner loop whose limit is the outer loop's
/// induction variable gets that IV's proven range through
/// [`RangeEnv::with_loop_iv`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RangeEnv {
    locals: Vec<(usize, IntRange)>,
    lengths: Vec<(usize, IntRange)>,
}

impl RangeEnv {
    /// An environment that knows nothing.
    pub fn new() -> RangeEnv {
        RangeEnv::default()
    }

    /// Record what is known about a local's value. A second binding for the
    /// same local *meets* the first — facts accumulate, they never widen.
    pub fn bind_local(&mut self, local: usize, range: IntRange) {
        match self.locals.iter_mut().find(|(l, _)| *l == local) {
            Some((_, r)) => *r = r.meet(range),
            None => self.locals.push((local, range)),
        }
    }

    /// Record what is known about `local.length`.
    pub fn bind_array_length(&mut self, local: usize, range: IntRange) {
        let range = range.meet(IntRange::array_length());
        match self.lengths.iter_mut().find(|(l, _)| *l == local) {
            Some((_, r)) => *r = r.meet(range),
            None => self.lengths.push((local, range)),
        }
    }

    /// Builder form of [`RangeEnv::bind_local`].
    pub fn with_local(mut self, local: usize, range: IntRange) -> RangeEnv {
        self.bind_local(local, range);
        self
    }

    /// Builder form of [`RangeEnv::bind_array_length`].
    pub fn with_array_length(mut self, local: usize, range: IntRange) -> RangeEnv {
        self.bind_array_length(local, range);
        self
    }

    /// What is known about `local`; [`IntRange::unknown`] by default.
    pub fn local(&self, local: usize) -> IntRange {
        self.locals
            .iter()
            .find(|(l, _)| *l == local)
            .map(|(_, r)| *r)
            .unwrap_or_else(IntRange::unknown)
    }

    /// What is known about `local.length`; every array length is at least
    /// non-negative.
    pub fn array_length(&self, local: usize) -> IntRange {
        self.lengths
            .iter()
            .find(|(l, _)| *l == local)
            .map(|(_, r)| *r)
            .unwrap_or_else(IntRange::array_length)
    }

    /// The environment an *inner* loop should be analysed in, given a proven
    /// enclosing loop: the outer IV's local is bound to the range it can hold
    /// while the inner loop runs.
    ///
    /// Returns `self` unchanged when the outer loop's span cannot be proved —
    /// the inner loop then simply sees an unknown outer IV.
    pub fn with_loop_iv(&self, outer: &CountedLoop) -> RangeEnv {
        match outer.iv_span(self) {
            Ok(p) => self.clone().with_local(outer.iv.local, p.span.numeric),
            Err(_) => self.clone(),
        }
    }
}

/// The overflow assumption a proof rests on. Every proof states one; there is
/// no third option that quietly assumes non-wrapping arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowModel {
    /// Every arithmetic step was evaluated in `i64` and shown representable in
    /// `i32` from compile-time facts alone. Java's wrapping cannot occur.
    NoWrapProven,
    /// No wrap occurs **provided the returned pre-header guards pass**. The
    /// consumer must emit them; dropping one silently restores the wrap.
    NoWrapGuarded,
}

/// Why a proof was refused. Every one of these means "keep the check".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// The stride is zero — the IV never advances, so this is not a counted
    /// loop and the exit test can never become false through the IV.
    ZeroStride,
    /// The stride's sign is a runtime value in a position where no guard can
    /// pin it (a decreasing exit test, or a post-tested loop).
    UnknownStride,
    /// The stride's sign contradicts the exit comparison — the loop is not
    /// counted in the direction its test implies.
    DirectionMismatch,
    /// The induction variable can wrap the `int` range inside the loop and no
    /// guard can prevent it.
    IvMayWrap,
    /// Evaluating `scale * iv + offset` can wrap.
    IndexMayWrap,
    /// The loop's entry value is unbounded where the proof needs it bounded
    /// (a post-tested loop's first, untested iteration).
    UnboundedEntry,
    /// The limit can change while the loop runs, so a pre-header guard on it
    /// would go stale.
    BoundNotInvariant,
    /// The limit's range is contradictory.
    UnusableBound,
    /// The index is not an affine function of *this* loop's induction
    /// variable.
    NotTheInductionVariable,
    /// The index provably reaches a negative value.
    IndexMayBeNegative,
    /// No endpoint could be established for the index at all.
    UnboundedIndex,
    /// The loop form and the other facts cannot be combined into a proof
    /// (e.g. a post-tested loop with a runtime stride).
    UnsupportedLoopForm,
}

/// A runtime obligation a proof leaves for the loop pre-header. A consumer
/// that cannot emit a guard **must** treat the whole proof as refused; a
/// partially-emitted guard set proves nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreheaderGuard {
    /// `term >= 0`.
    NonNegative(SymBound),
    /// `length >= term`, where `length` is the guarded array's length.
    /// Evaluated in 64-bit: `term`'s `base + addend` must not be materialised
    /// as a wrapping `int` add.
    LengthAtLeast(SymBound),
    /// `term <= limit` — the increasing loop's no-wrap obligation.
    AtMost {
        /// The runtime term.
        term: SymBound,
        /// Inclusive upper limit.
        limit: i32,
    },
    /// `term >= limit` — the decreasing loop's no-wrap obligation.
    AtLeast {
        /// The runtime term.
        term: SymBound,
        /// Inclusive lower limit.
        limit: i32,
    },
    /// `0 <= <local>` and `<local> <= i32::MAX - headroom` — a runtime
    /// stride's sign and magnitude, so the IV can neither walk backwards nor
    /// wrap past the exit test.
    StrideInRange {
        /// Local holding the step.
        local: usize,
        /// The largest value the IV can hold in an executed iteration; the
        /// step must not carry it past `i32::MAX`.
        headroom: SymBound,
    },
    /// `term >= minimum`, evaluated in 64-bit — the loop body executes at
    /// least `minimum` times.
    ///
    /// This is the shape every "the transform only pays off above N
    /// iterations" gate needs and none of the others express: unrolling,
    /// peeling and vectorization all have a minimum below which the
    /// transformed loop is a pessimisation, and `for (i = 0; i < n; i++)` with
    /// a runtime `n` has a compile-time `trip.min` of zero, so without a
    /// runtime check the answer is always "refuse".
    ///
    /// `term` is a **trip-count witness**: a runtime expression the pre-header
    /// evaluates that is a *lower bound* on the number of executed iterations.
    /// It is self-contained — a consumer evaluates `term` and compares it
    /// against `minimum` knowing nothing about the loop's entry value or
    /// stride, because [`CountedLoop::prove_trip_count_at_least`] has already
    /// folded both into the addend. Evaluate in 64-bit: `base + addend` must
    /// not be materialised as a wrapping `int` add.
    ///
    /// Never emitted by [`CountedLoop::iv_span`] or the bounds proofs — a
    /// minimum trip count is not needed to prove an index in range, and
    /// attaching one there would silently make every existing proof
    /// conditional on a fact it does not use.
    TripCountAtLeast {
        /// Runtime lower bound on the executed-iteration count.
        term: SymBound,
        /// Iterations the loop must be shown to run.
        minimum: u64,
    },
}

/// The verdict on "this loop's body executes at least `minimum` times".
///
/// Deliberately shaped like [`BoundsProof`]: `Static` is "proved with nothing
/// to emit", `Guarded` is "proved provided the pre-header discharges these",
/// and `Refused` means the caller must assume the loop may run fewer times.
/// A consumer that cannot emit the guards must treat the whole verdict as
/// [`TripCountProof::Refused`]; a partially-emitted guard set proves nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TripCountProof {
    /// The minimum holds from compile-time facts alone.
    Static,
    /// The minimum holds provided every listed guard is discharged in the
    /// pre-header.
    Guarded(Vec<PreheaderGuard>),
    /// Not proved. The loop may run fewer than `minimum` times.
    Refused(RefusalReason),
}

/// The proven extent of an integer expression over every executed iteration.
///
/// Both endpoint lists are *sets of witnesses*: every executed value is
/// `<= max(max_terms)` and `>= min(min_terms)`. A consumer proving
/// `value < L` must therefore prove `L > t` for **every** `t` in `max_terms`;
/// an empty list means no endpoint was established at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IvSpan {
    /// Sound numeric hull. May be [`IntRange::unknown`] when both endpoints
    /// are symbolic, and [`IntRange::Empty`] when the loop provably never
    /// executes its body.
    pub numeric: IntRange,
    /// Upper-bound witnesses.
    pub max_terms: Vec<SymBound>,
    /// Lower-bound witnesses.
    pub min_terms: Vec<SymBound>,
}

/// A span plus the overflow model and pre-header obligations it rests on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenSpan {
    /// The proven extent.
    pub span: IvSpan,
    /// Obligations the pre-header must discharge.
    pub guards: Vec<PreheaderGuard>,
    /// Which overflow assumption the proof states.
    pub overflow: OverflowModel,
}

/// An array index expressed as an affine function of the loop's induction
/// variable: `scale * iv + offset`.
///
/// `offset` must already account for any advance of `iv` between the exit test
/// and the access itself (see the module's producer obligations).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexExpr {
    /// The local the index is derived from.
    pub iv_local: usize,
    /// Multiplier on the induction variable.
    pub scale: i32,
    /// Constant displacement.
    pub offset: i32,
}

impl IndexExpr {
    /// The bare index `iv`.
    pub fn identity(iv_local: usize) -> IndexExpr {
        IndexExpr {
            iv_local,
            scale: 1,
            offset: 0,
        }
    }

    /// The index `iv + offset`.
    pub fn shifted(iv_local: usize, offset: i32) -> IndexExpr {
        IndexExpr {
            iv_local,
            scale: 1,
            offset,
        }
    }
}

/// The verdict on `0 <= index < length` for every executed iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundsProof {
    /// In range from compile-time facts alone — no check, no guard, no deopt.
    Static,
    /// In range provided every listed guard is discharged in the pre-header.
    Guarded(Vec<PreheaderGuard>),
    /// Not proved. Keep the per-element bounds check.
    Refused(RefusalReason),
}

/// Bounds on how many times a loop's body executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TripCount {
    /// Fewest executions.
    pub min: u64,
    /// Most executions.
    pub max: u64,
}

impl TripCount {
    /// The exact count when the bounds coincide.
    pub fn exact(&self) -> Option<u64> {
        if self.min == self.max {
            Some(self.min)
        } else {
            None
        }
    }

    /// Whether the body provably never runs.
    pub fn is_zero(&self) -> bool {
        self.max == 0
    }
}

/// A loop whose induction variable is affine and whose exit is a single
/// comparison against a loop-invariant limit.
///
/// This is a *claim* about bytecode; see the module's producer obligations for
/// what the constructor must have established. Everything downstream of those
/// obligations is proved by the methods below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountedLoop {
    /// Bytecode PC of the loop header.
    pub header_pc: usize,
    /// Bytecode PC of the back edge.
    pub back_edge_pc: usize,
    /// The induction variable.
    pub iv: AffineIv,
    /// The exit comparison, in `if_icmp*` (exit-when-true) polarity.
    pub cmp: ExitCmp,
    /// The limit the IV is compared against.
    pub bound: BoundSource,
    /// Extra compile-time knowledge about the limit's value, met with whatever
    /// [`BoundSource::range_in`] derives. [`IntRange::unknown`] when there is
    /// none.
    pub bound_range: IntRange,
    /// Whether the exit test dominates the body.
    pub form: LoopForm,
    /// Bit `n` set iff local `n` is written in the loop body. The producer
    /// must refuse the loop outright rather than represent a local `>= 64`.
    pub modified_locals: u64,
    /// Whether the body provably performs no heap store or call that could
    /// change a field the limit reads.
    pub heap_stable: bool,
    /// Whether the body can leave the loop other than through the recognised
    /// exit test: a second leaving branch or switch arm, a return, or an
    /// `athrow`. The exit test then bounds the trip count only from above, so
    /// [`CountedLoop::trip_count`] reports `min == 0` and
    /// [`CountedLoop::prove_trip_count_at_least`] refuses a guard.
    pub has_other_exit: bool,
}

impl CountedLoop {
    /// The IV's direction, as far as it is known at compile time.
    pub fn direction(&self) -> Direction {
        self.iv.stride.direction()
    }

    /// Whether the limit is itself an executed IV value (`i <= n` / `i >= n`).
    pub fn is_inclusive(&self) -> bool {
        self.cmp.is_inclusive()
    }

    /// The limit's range under `env`, met with [`CountedLoop::bound_range`].
    pub fn bound_range_in(&self, env: &RangeEnv) -> IntRange {
        self.bound.range_in(env).meet(self.bound_range)
    }

    /// The displacement that turns the limit into the extreme IV value an
    /// executed iteration can hold:
    ///
    /// | continue test | extreme executed value |
    /// |---------------|------------------------|
    /// | `iv <  n`     | `n - 1`                |
    /// | `iv <= n`     | `n`                    |
    /// | `iv >  n`     | `n + 1`                |
    /// | `iv >= n`     | `n`                    |
    ///
    /// This single number is what `bce.rs` spells as "refuse inclusive loops"
    /// on the static path and as "use JBE instead of JB" on the speculative
    /// one.
    pub fn bound_addend(&self) -> i32 {
        if self.cmp.is_inclusive() {
            0
        } else if self.cmp.is_increasing() {
            -1
        } else {
            1
        }
    }

    /// Prove the extent of the induction variable over every executed
    /// iteration.
    ///
    /// Refuses rather than guesses: an unprovable direction, a limit that the
    /// body can change, an entry value the loop form needs but does not have,
    /// or an unavoidable `int` wrap each return [`RefusalReason`].
    pub fn iv_span(&self, env: &RangeEnv) -> Result<ProvenSpan, RefusalReason> {
        if !self
            .bound
            .is_invariant(self.modified_locals, self.heap_stable)
        {
            return Err(RefusalReason::BoundNotInvariant);
        }
        let bound_range = self.bound_range_in(env);
        if bound_range.is_empty() {
            return Err(RefusalReason::UnusableBound);
        }
        let init = self.iv.init;
        if init.is_empty() {
            return Err(RefusalReason::UnusableBound);
        }

        let increasing = self.cmp.is_increasing();
        // Direction agreement. A runtime stride has no compile-time sign, so
        // it is admitted only where a pre-header guard can pin it: an
        // increasing exit test on a pre-tested loop.
        match self.iv.stride {
            Stride::Const(0) => return Err(RefusalReason::ZeroStride),
            Stride::Const(s) => {
                if (s > 0) != increasing {
                    return Err(RefusalReason::DirectionMismatch);
                }
            }
            Stride::Variable(_) => {
                if !increasing {
                    return Err(RefusalReason::UnknownStride);
                }
                if self.form == LoopForm::PostTested {
                    // The first, untested iteration advances from the entry
                    // value, which the stride guard's headroom (expressed
                    // against the limit) does not cover.
                    return Err(RefusalReason::UnsupportedLoopForm);
                }
            }
        }

        let addend = self.bound_addend();
        let gov = SymBound {
            base: BoundTerm::Bound(self.bound.clone()),
            addend,
        };
        // Numeric image of the governed endpoint, computed in i64 so a limit
        // sitting at the edge of the int range cannot wrap the analysis.
        let (blo, bhi) = match bound_range.i64_bounds() {
            Some(b) => b,
            None => return Err(RefusalReason::UnusableBound),
        };
        let gov_hi = bhi + addend as i64;
        let gov_lo = blo + addend as i64;

        // The entry value. A compile-time-known start collapses to a constant
        // witness (which the consumer can discharge without emitting anything);
        // otherwise it stays the runtime term `bce.rs` spells as the header
        // `iv >= 0` check.
        let entry = match init.as_constant() {
            Some(c) => SymBound::constant(c),
            None => SymBound {
                base: BoundTerm::IvEntry(self.iv.local),
                addend: 0,
            },
        };

        let mut guards: Vec<PreheaderGuard> = Vec::new();
        let mut guarded = false;

        let (numeric, max_terms, min_terms) = if increasing {
            // Every tested value satisfies `iv <= bound + addend`; monotonicity
            // (established by the no-wrap obligation below) additionally puts
            // every executed value at or above the entry value.
            let tested = IntRange::from_i64(i32::MIN as i64, gov_hi.min(i32::MAX as i64))
                .unwrap_or_else(IntRange::unknown)
                .meet(IntRange::new(init.lo().unwrap_or(i32::MIN), i32::MAX));
            let mut max_terms = vec![gov.clone()];
            let min_terms = vec![entry.clone()];
            let numeric = match self.form {
                LoopForm::PreTested => tested,
                LoopForm::PostTested => {
                    if init.is_unknown() {
                        return Err(RefusalReason::UnboundedEntry);
                    }
                    max_terms.push(entry.clone());
                    tested.join(init)
                }
            };
            (numeric, max_terms, min_terms)
        } else {
            let tested = IntRange::from_i64(gov_lo.max(i32::MIN as i64), i32::MAX as i64)
                .unwrap_or_else(IntRange::unknown)
                .meet(IntRange::new(i32::MIN, init.hi().unwrap_or(i32::MAX)));
            let mut min_terms = vec![gov.clone()];
            let max_terms = vec![entry.clone()];
            let numeric = match self.form {
                LoopForm::PreTested => tested,
                LoopForm::PostTested => {
                    if init.is_unknown() {
                        return Err(RefusalReason::UnboundedEntry);
                    }
                    min_terms.push(entry.clone());
                    tested.join(init)
                }
            };
            (numeric, max_terms, min_terms)
        };

        // ---- the overflow obligation -------------------------------------
        //
        // Java `int` addition wraps. For an increasing loop the last executed
        // value is at most `bound + addend`; the step that follows it must not
        // carry it past `i32::MAX`, or the IV reappears at the bottom of the
        // range with the exit test still passing. Symmetrically for a
        // decreasing loop at `i32::MIN`.
        match self.iv.stride {
            Stride::Const(s) if s > 0 => {
                if gov_hi + s as i64 > i32::MAX as i64 {
                    if gov_lo + s as i64 > i32::MAX as i64 {
                        // Even the smallest admissible limit wraps: no runtime
                        // guard could ever pass, so refuse instead of emitting
                        // one that always deopts.
                        return Err(RefusalReason::IvMayWrap);
                    }
                    let limit = i32::MAX as i64 - s as i64 - addend as i64;
                    if limit < i32::MIN as i64 {
                        return Err(RefusalReason::IvMayWrap);
                    }
                    guards.push(PreheaderGuard::AtMost {
                        term: SymBound {
                            base: BoundTerm::Bound(self.bound.clone()),
                            addend: 0,
                        },
                        limit: limit as i32,
                    });
                    guarded = true;
                }
                if self.form == LoopForm::PostTested {
                    // The untested first iteration advances from the entry
                    // value; that step must be provably safe from the entry
                    // range alone (there is nothing to guard it against).
                    let ihi = init.hi().unwrap_or(i32::MAX) as i64;
                    if ihi + s as i64 > i32::MAX as i64 {
                        return Err(RefusalReason::IvMayWrap);
                    }
                }
            }
            Stride::Const(s) => {
                // s < 0 (s == 0 was refused above).
                if gov_lo + (s as i64) < i32::MIN as i64 {
                    if gov_hi + (s as i64) < i32::MIN as i64 {
                        return Err(RefusalReason::IvMayWrap);
                    }
                    let limit = i32::MIN as i64 - s as i64 - addend as i64;
                    if limit > i32::MAX as i64 {
                        return Err(RefusalReason::IvMayWrap);
                    }
                    guards.push(PreheaderGuard::AtLeast {
                        term: SymBound {
                            base: BoundTerm::Bound(self.bound.clone()),
                            addend: 0,
                        },
                        limit: limit as i32,
                    });
                    guarded = true;
                }
                if self.form == LoopForm::PostTested {
                    let ilo = init.lo().unwrap_or(i32::MIN) as i64;
                    if ilo + (s as i64) < i32::MIN as i64 {
                        return Err(RefusalReason::IvMayWrap);
                    }
                }
            }
            Stride::Variable(local) => {
                // Sign AND magnitude in one guard: `0 <= step` keeps the IV
                // from walking below the entry value (an index under the array
                // base), `step <= i32::MAX - (bound + addend)` keeps it from
                // wrapping past the exit test.
                guards.push(PreheaderGuard::StrideInRange {
                    local,
                    headroom: gov.clone(),
                });
                guarded = true;
            }
        }

        Ok(ProvenSpan {
            span: IvSpan {
                numeric,
                max_terms,
                min_terms,
            },
            guards,
            overflow: if guarded {
                OverflowModel::NoWrapGuarded
            } else {
                OverflowModel::NoWrapProven
            },
        })
    }

    /// Prove the extent of `scale * iv + offset` over every executed
    /// iteration.
    ///
    /// A `scale` other than 1 drops the symbolic endpoints (there is no
    /// symbolic multiply here) and keeps only the numeric hull — which is
    /// sound, and simply proves less. Any wrap in the index arithmetic itself
    /// is a refusal, never a wrapped range.
    pub fn index_span(&self, idx: &IndexExpr, env: &RangeEnv) -> Result<ProvenSpan, RefusalReason> {
        if idx.iv_local != self.iv.local {
            return Err(RefusalReason::NotTheInductionVariable);
        }
        let base = self.iv_span(env)?;
        if idx.scale == 1 && idx.offset == 0 {
            return Ok(base);
        }
        let numeric = base
            .span
            .numeric
            .scale_no_wrap(idx.scale)
            .and_then(|r| r.offset_no_wrap(idx.offset))
            .ok_or(RefusalReason::IndexMayWrap)?;

        let (max_terms, min_terms) = if idx.scale == 1 {
            let shift = |terms: &[SymBound]| -> Option<Vec<SymBound>> {
                terms.iter().map(|t| t.offset(idx.offset)).collect()
            };
            let max = shift(&base.span.max_terms).ok_or(RefusalReason::IndexMayWrap)?;
            let min = shift(&base.span.min_terms).ok_or(RefusalReason::IndexMayWrap)?;
            (max, min)
        } else {
            // Numeric-only. When the hull is top these lists become the
            // unsatisfiable endpoints `i32::MAX` / `i32::MIN`, and the bounds
            // proof refuses — which is the intended outcome.
            let max = numeric
                .hi()
                .map(|h| vec![SymBound::constant(h)])
                .unwrap_or_default();
            let min = numeric
                .lo()
                .map(|l| vec![SymBound::constant(l)])
                .unwrap_or_default();
            (max, min)
        };

        Ok(ProvenSpan {
            span: IvSpan {
                numeric,
                max_terms,
                min_terms,
            },
            guards: base.guards,
            overflow: base.overflow,
        })
    }

    /// Prove `0 <= index && index < length` for every executed iteration.
    ///
    /// `length` is what the caller knows about the *guarded array's* length —
    /// [`IntRange::array_length`] when nothing is known, or a constant for a
    /// freshly-allocated array. The guards returned are per-array: a caller
    /// eliding accesses into two different arrays must discharge them for
    /// each array separately.
    pub fn prove_index_in_bounds(
        &self,
        idx: &IndexExpr,
        length: IntRange,
        env: &RangeEnv,
    ) -> BoundsProof {
        self.prove_index_in_bounds_of(idx, None, length, env)
    }

    /// [`CountedLoop::prove_index_in_bounds`], additionally told *which* array
    /// local is being indexed.
    ///
    /// Naming the array discharges the case `bce.rs` spends a whole
    /// whole-method provenance pass on (`find_bound_arraylength_provenance`):
    /// when the limit **is** this array's length, `length >= a.length` is a
    /// tautology and the access needs no guard at all. Naming a *different*
    /// array changes nothing — the guard stays, which is the multi-array
    /// out-of-bounds store that provenance pass exists to prevent.
    pub fn prove_index_in_bounds_of(
        &self,
        idx: &IndexExpr,
        array_local: Option<usize>,
        length: IntRange,
        env: &RangeEnv,
    ) -> BoundsProof {
        let denoted = array_local.map(BoundSource::ArrayLength);
        self.prove_index_in_bounds_of_array(idx, denoted.as_ref(), length, env)
    }

    /// [`CountedLoop::prove_index_in_bounds_of`], keyed on the *expression*
    /// that denotes the guarded array's length rather than on a JVM local slot.
    ///
    /// Two callers need this and neither can use the slot-keyed form:
    ///
    /// * an **IR-level** caller holds a `NodeId`, not a slot. It cannot pass
    ///   `array_local`, because `array_local` is not opaque — scev feeds the
    ///   same `usize` to [`BoundSource::is_invariant`] (a `modified_locals`
    ///   bit, so anything `>= 64` refuses every proof) and to
    ///   [`RangeEnv::array_length`]. Passing a `NodeId` there would not merely
    ///   miss the shortcut, it would corrupt the invariance test. Passing the
    ///   `BoundSource` the producer already built for
    ///   [`CountedLoop::bound`] has neither problem.
    /// * a caller whose limit is this array's length *spelled differently* —
    ///   a local that provably holds `a.length`, or a cached length field.
    ///   `bce.rs`'s `find_bound_arraylength_provenance` is a whole-method pass
    ///   that establishes exactly this; its answer can now be handed over
    ///   instead of re-derived.
    ///
    /// **Caller obligation:** `array_length` must denote the length of the
    /// array `idx` indexes, on every path that reaches the loop. Naming a
    /// *different* array's length is the multi-array out-of-bounds store the
    /// provenance pass exists to prevent — scev cannot check this and does not
    /// try. `None` is always safe and costs one guard.
    pub fn prove_index_in_bounds_of_array(
        &self,
        idx: &IndexExpr,
        array_length: Option<&BoundSource>,
        length: IntRange,
        env: &RangeEnv,
    ) -> BoundsProof {
        let proven = match self.index_span(idx, env) {
            Ok(p) => p,
            Err(r) => return BoundsProof::Refused(r),
        };
        let span = proven.span;
        if span.numeric.is_empty() {
            // The body provably never runs: there is no access to check.
            return BoundsProof::Static;
        }
        let mut guards = proven.guards;

        // ---- index >= 0 ---------------------------------------------------
        // Every lower witness must be non-negative: the true minimum is
        // `>= min(min_terms)`, so one negative witness sinks the proof.
        if !span.numeric.is_non_negative() {
            if span.min_terms.is_empty() {
                return BoundsProof::Refused(RefusalReason::UnboundedIndex);
            }
            for t in &span.min_terms {
                match t.as_const() {
                    Some(c) if c >= 0 => {}
                    Some(_) => return BoundsProof::Refused(RefusalReason::IndexMayBeNegative),
                    None => guards.push(PreheaderGuard::NonNegative(t.clone())),
                }
            }
        }

        // ---- index < length -----------------------------------------------
        // The true maximum is `<= max(max_terms)`, so the length must dominate
        // every witness: `length >= t + 1` for each. The numeric hull gets
        // first refusal — when it already sits below the shortest admissible
        // length there is nothing left to guard.
        let known_len_lo = length.lo().unwrap_or(0);
        let numeric_fits = span
            .numeric
            .hi()
            .map(|h| (h as i64) < known_len_lo as i64)
            .unwrap_or(true);
        if !numeric_fits {
            if span.max_terms.is_empty() {
                return BoundsProof::Refused(RefusalReason::UnboundedIndex);
            }
            for t in &span.max_terms {
                if let Some(c) = t.as_const() {
                    if (c as i64) < known_len_lo as i64 {
                        continue; // statically shorter than the shortest length
                    }
                }
                let needed = match t.offset(1) {
                    Some(n) => n,
                    None => return BoundsProof::Refused(RefusalReason::IndexMayWrap),
                };
                // `a.length >= a.length + k` for `k <= 0` is a tautology: the
                // limit IS the guarded array's length.
                if let Some(denoted) = array_length {
                    if needed.addend <= 0
                        && matches!(&needed.base, BoundTerm::Bound(b) if b == denoted)
                    {
                        continue;
                    }
                }
                if let Some(c) = needed.as_const() {
                    if let Some(h) = length.hi() {
                        if (c as i64) > h as i64 {
                            // No admissible length can satisfy this — refuse
                            // rather than emit a guard that always deopts.
                            return BoundsProof::Refused(RefusalReason::UnboundedIndex);
                        }
                    }
                    // Discharge against the length's LOWER bound. `length` is a
                    // range, so an array whose proven minimum already covers the
                    // demand (`new int[16]` for a `[0,16)` loop) satisfies this
                    // on every admissible run. Emitting the guard anyway pays a
                    // preheader compare for a fact already proven, and makes an
                    // otherwise-`Static` proof look conditional to every
                    // consumer.
                    if let Some(lo) = length.lo() {
                        if (c as i64) <= lo as i64 {
                            continue;
                        }
                    }
                }
                // Normalise a constant demand to base-only. `Const(198) + 1`
                // and `Const(199) + 0` are the same guard; leaving both spellings
                // in circulation defeats the `dedup` below and makes two
                // identical preheader compares look distinct to a consumer.
                let needed = match needed.as_const() {
                    Some(c) => SymBound {
                        base: BoundTerm::Const(c),
                        addend: 0,
                    },
                    None => needed,
                };
                guards.push(PreheaderGuard::LengthAtLeast(needed));
            }
        }

        guards.dedup();
        if guards.is_empty() {
            BoundsProof::Static
        } else {
            BoundsProof::Guarded(guards)
        }
    }

    /// Bound the number of times the body executes.
    ///
    /// Requires a constant stride and [`LoopForm::PreTested`] — a post-tested
    /// loop's first iteration is unconditional and its count is the caller's
    /// business, not a place to guess. `Some(TripCount { max: 0, .. })` is the
    /// provable zero-trip loop.
    pub fn trip_count(&self, env: &RangeEnv) -> Option<TripCount> {
        if self.form != LoopForm::PreTested {
            return None;
        }
        let stride = self.iv.stride.as_const()?;
        if stride == 0 {
            return None;
        }
        let increasing = self.cmp.is_increasing();
        if (stride > 0) != increasing {
            return None;
        }
        let (ilo, ihi) = self.iv.init.i64_bounds()?;
        let (blo, bhi) = self.bound_range_in(env).i64_bounds()?;
        let addend = self.bound_addend() as i64;
        let (gov_lo, gov_hi) = (blo + addend, bhi + addend);
        let mag = (stride as i64).abs();
        let count = |from: i64, to: i64| -> u64 {
            // Executions of `v = from, from±mag, …` while `v` stays on the
            // near side of `to`, inclusive.
            if increasing {
                if to < from {
                    0
                } else {
                    ((to - from) / mag + 1) as u64
                }
            } else if from < to {
                0
            } else {
                ((from - to) / mag + 1) as u64
            }
        };
        let (min, max) = if increasing {
            (count(ihi, gov_lo), count(ilo, gov_hi))
        } else {
            (count(ilo, gov_hi), count(ihi, gov_lo))
        };
        // A second exit can end the loop before the recognised test fails.
        let min = if self.has_other_exit { 0 } else { min.min(max) };
        Some(TripCount { min, max })
    }

    /// Prove that the body executes at least `minimum` times, emitting a
    /// pre-header check when compile-time facts alone do not settle it.
    ///
    /// [`CountedLoop::trip_count`] answers with a compile-time interval, and
    /// for the commonest loop in Java — `for (i = 0; i < n; i++)` with a
    /// runtime `n` — that interval is `[0, i32::MAX]`. Every consumer with a
    /// profitability floor (a vector lane count, an unroll factor, a peel
    /// count) therefore sees `min == 0` and refuses, even though a single
    /// pre-header compare would settle it. This is the method that returns
    /// that compare instead of a refusal.
    ///
    /// ## What is proved
    ///
    /// With a unit stride the executed values are exactly
    /// `entry, entry+1, …, bound + addend`, so the count is
    /// `bound + addend - entry + 1`. The entry value is a *range*, so the
    /// witness uses its highest admissible value — the fewest iterations the
    /// loop can run — and the guard is therefore conservative, never
    /// optimistic, when the entry value is only partly known.
    ///
    /// The result does **not** depend on the no-wrap obligations
    /// [`CountedLoop::iv_span`] mints, and deliberately does not carry them: a
    /// wrapping IV makes the loop run *longer*, never shorter, so a lower
    /// bound on the trip count survives a wrap. A caller that also needs the
    /// index proved in range must still take those guards from the bounds
    /// proof.
    ///
    /// ## What is refused
    ///
    /// * anything [`CountedLoop::trip_count`] refuses (a post-tested loop, a
    ///   runtime or zero stride, a direction mismatch, a limit the body can
    ///   change), with the same [`RefusalReason`] the other proofs use;
    /// * `|stride| != 1`, because the count is then
    ///   `floor((bound + addend - entry) / stride) + 1` and [`SymBound`] has no
    ///   division — the witness would not be trip-count-valued;
    /// * a decreasing loop, because its count is `entry - (bound + addend) + 1`
    ///   and [`SymBound`] cannot negate its base term;
    /// * an unbounded entry value, and any demand no admissible limit could
    ///   meet — the module's standing rule that a guard which can never pass is
    ///   a refusal, not an obligation.
    ///
    /// `minimum == 0` is [`TripCountProof::Static`] for every loop, counted or
    /// not: "runs at least zero times" is not a claim about the loop.
    pub fn prove_trip_count_at_least(&self, minimum: u64, env: &RangeEnv) -> TripCountProof {
        if minimum == 0 {
            return TripCountProof::Static;
        }
        // A trip count is bounded by the `int` range the IV walks, so a demand
        // beyond that is unsatisfiable rather than merely unproven.
        if minimum > u32::MAX as u64 {
            return TripCountProof::Refused(RefusalReason::UnusableBound);
        }
        // Preconditions in the same order, and with the same verdicts, as
        // `iv_span` — the two proofs must never disagree about *why* a loop is
        // unusable.
        if !self
            .bound
            .is_invariant(self.modified_locals, self.heap_stable)
        {
            return TripCountProof::Refused(RefusalReason::BoundNotInvariant);
        }
        if self.form != LoopForm::PreTested {
            // A post-tested loop's first iteration is unconditional; its count
            // is the producer's business, exactly as in `trip_count`.
            return TripCountProof::Refused(RefusalReason::UnsupportedLoopForm);
        }
        let increasing = self.cmp.is_increasing();
        let stride = match self.iv.stride {
            Stride::Const(0) => return TripCountProof::Refused(RefusalReason::ZeroStride),
            Stride::Const(s) => {
                if (s > 0) != increasing {
                    return TripCountProof::Refused(RefusalReason::DirectionMismatch);
                }
                s
            }
            Stride::Variable(_) => return TripCountProof::Refused(RefusalReason::UnknownStride),
        };
        let bound_range = self.bound_range_in(env);
        if bound_range.is_empty() || self.iv.init.is_empty() {
            return TripCountProof::Refused(RefusalReason::UnusableBound);
        }
        // Already proven: emit nothing. This is the branch that keeps a loop
        // with a compile-time trip count from acquiring a pre-header compare it
        // does not need.
        if let Some(t) = self.trip_count(env) {
            if t.min >= minimum {
                return TripCountProof::Static;
            }
        }
        // A pre-header compare on the limit says nothing about a loop that can
        // also leave through a `break`, a return or a throw.
        if self.has_other_exit {
            return TripCountProof::Refused(RefusalReason::UnsupportedLoopForm);
        }
        // Only `+1` yields a trip-count-valued witness; see the doc comment.
        if stride != 1 {
            return TripCountProof::Refused(RefusalReason::UnsupportedLoopForm);
        }
        if self.iv.init.is_unknown() {
            return TripCountProof::Refused(RefusalReason::UnboundedEntry);
        }
        let entry_hi = match self.iv.init.hi() {
            Some(h) => h as i64,
            None => return TripCountProof::Refused(RefusalReason::UnboundedEntry),
        };
        // count = (bound + addend) - entry + 1, with the worst-case entry.
        let witness_addend = self.bound_addend() as i64 - entry_hi + 1;
        if !(i32::MIN as i64..=i32::MAX as i64).contains(&witness_addend) {
            return TripCountProof::Refused(RefusalReason::UnboundedEntry);
        }
        // Refuse rather than hand back an obligation no admissible limit could
        // ever satisfy.
        let bound_hi = match bound_range.hi() {
            Some(h) => h as i64,
            None => return TripCountProof::Refused(RefusalReason::UnusableBound),
        };
        if bound_hi + witness_addend < minimum as i64 {
            return TripCountProof::Refused(RefusalReason::UnusableBound);
        }
        TripCountProof::Guarded(vec![PreheaderGuard::TripCountAtLeast {
            term: SymBound {
                base: BoundTerm::Bound(self.bound.clone()),
                addend: witness_addend as i32,
            },
            minimum,
        }])
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

// ---------------------------------------------------------------------------
// Range-analysis tests.
//
// Every "must refuse" below is paired with a "must accept" twin that differs
// only in the fact that makes the proof possible — a refusal that fires for
// the wrong reason (or a proof that survives a real hazard) shows up as the
// twin flipping.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod range_tests {
    use super::*;

    /// The IV local used throughout; local 2 is the usual limit local.
    const IV: usize = 1;
    const N: usize = 2;

    fn mk(init: IntRange, stride: Stride, cmp: ExitCmp, bound: BoundSource) -> CountedLoop {
        CountedLoop {
            header_pc: 0,
            back_edge_pc: 0,
            iv: AffineIv {
                local: IV,
                init,
                stride,
            },
            cmp,
            bound,
            bound_range: IntRange::unknown(),
            form: LoopForm::PreTested,
            modified_locals: 1u64 << IV,
            heap_stable: true,
            has_other_exit: false,
        }
    }

    fn guards(p: &BoundsProof) -> Vec<PreheaderGuard> {
        match p {
            BoundsProof::Guarded(g) => g.clone(),
            other => panic!("expected Guarded, got {other:?}"),
        }
    }

    fn refusal(p: &BoundsProof) -> RefusalReason {
        match p {
            BoundsProof::Refused(r) => *r,
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    fn bound_term(b: BoundSource, addend: i32) -> SymBound {
        SymBound {
            base: BoundTerm::Bound(b),
            addend,
        }
    }

    fn entry_term(addend: i32) -> SymBound {
        SymBound {
            base: BoundTerm::IvEntry(IV),
            addend,
        }
    }

    // -- lattice ------------------------------------------------------------

    #[test]
    fn lattice_join_meet_and_emptiness() {
        let a = IntRange::new(0, 10);
        let b = IntRange::new(5, 20);
        assert_eq!(a.join(b), IntRange::new(0, 20));
        assert_eq!(a.meet(b), IntRange::new(5, 10));
        // Disjoint facts about one value are a contradiction, not a guess.
        assert!(IntRange::new(0, 3).meet(IntRange::new(9, 12)).is_empty());
        assert!(IntRange::new(1, 0).is_empty());
        assert!(IntRange::unknown().is_unknown());
        assert!(!IntRange::Empty.is_unknown());
        assert_eq!(IntRange::constant(7).as_constant(), Some(7));
        assert!(IntRange::unknown().contains_all(a));
        assert!(!a.contains_all(IntRange::unknown()));
    }

    #[test]
    fn lattice_arithmetic_refuses_to_pretend_wrapping_did_not_happen() {
        let hi = IntRange::new(i32::MAX - 1, i32::MAX);
        // Exact addition has no answer in `int`.
        assert_eq!(hi.add_no_wrap(IntRange::constant(1)), None);
        // The wrapping model is sound but useless — never a tight lie.
        assert!(hi.add(IntRange::constant(1)).is_unknown());
        // In range, both agree.
        assert_eq!(
            IntRange::new(0, 10).add_no_wrap(IntRange::constant(5)),
            Some(IntRange::new(5, 15))
        );
        // Negative scale swaps the endpoints; i32::MIN has no negation.
        assert_eq!(
            IntRange::new(-2, 3).scale_no_wrap(-4),
            Some(IntRange::new(-12, 8))
        );
        assert_eq!(IntRange::constant(i32::MIN).neg_no_wrap(), None);
        // min/max are elementwise.
        assert_eq!(
            IntRange::new(0, 100).min_with(IntRange::new(-5, 20)),
            IntRange::new(-5, 20)
        );
        assert_eq!(
            IntRange::new(0, 100).max_with(IntRange::new(-5, 20)),
            IntRange::new(0, 100)
        );
        assert!(IntRange::array_length().is_non_negative());
    }

    // -- ascending, exclusive vs inclusive ----------------------------------

    #[test]
    fn ascending_exclusive_local_bound_needs_length_ge_bound() {
        // for (i = ?; i < n; i++) a[i]
        let l = mk(
            IntRange::unknown(),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        let p = l.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::array_length(),
            &RangeEnv::new(),
        );
        assert_eq!(
            guards(&p),
            [
                PreheaderGuard::NonNegative(entry_term(0)),
                // `length >= n`: the exclusive comparator's -1 and the "one
                // past the last index" +1 cancel exactly.
                PreheaderGuard::LengthAtLeast(bound_term(BoundSource::Local(N), 0)),
            ]
        );
        // No wrap guard: the last executed index is at most `n - 1`, so the
        // next value `n` is representable for every `n`.
        assert_eq!(
            l.iv_span(&RangeEnv::new()).unwrap().overflow,
            OverflowModel::NoWrapProven
        );
    }

    #[test]
    fn ascending_inclusive_is_accepted_with_length_gt_bound_and_a_wrap_guard() {
        // for (i = ?; i <= n; i++) a[i] — the shape `bce.rs` refuses on the
        // static path and hides behind an off-by-default env flag on the
        // speculative one.
        let l = mk(
            IntRange::unknown(),
            Stride::Const(1),
            ExitCmp::Gt,
            BoundSource::Local(N),
        );
        let p = l.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::array_length(),
            &RangeEnv::new(),
        );
        assert_eq!(
            guards(&p),
            [
                // `n != Integer.MAX_VALUE`, derived rather than special-cased.
                PreheaderGuard::AtMost {
                    term: bound_term(BoundSource::Local(N), 0),
                    limit: i32::MAX - 1,
                },
                PreheaderGuard::NonNegative(entry_term(0)),
                // `length >= n + 1`, i.e. strictly greater than the bound.
                PreheaderGuard::LengthAtLeast(bound_term(BoundSource::Local(N), 1)),
            ]
        );
        assert_eq!(
            l.iv_span(&RangeEnv::new()).unwrap().overflow,
            OverflowModel::NoWrapGuarded
        );
    }

    #[test]
    fn ascending_exclusive_constant_bound_with_known_length_is_static() {
        // for (i = 0; i < 16; i++) a[i] with `a = new int[16]`.
        let l = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Const(16),
        );
        let p = l.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::constant(16),
            &RangeEnv::new(),
        );
        assert_eq!(p, BoundsProof::Static);
        // Twin: one element shorter and the same loop must not be static.
        let p2 = l.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::constant(15),
            &RangeEnv::new(),
        );
        assert!(matches!(p2, BoundsProof::Refused(_)), "got {p2:?}");
    }

    // -- descending ---------------------------------------------------------

    #[test]
    fn descending_inclusive_zero_bound_is_bounded_by_the_entry_value() {
        // for (i = n - 1; i >= 0; i--) a[i]
        let l = mk(
            IntRange::unknown(),
            Stride::Const(-1),
            ExitCmp::Lt,
            BoundSource::Const(0),
        );
        let p = l.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::array_length(),
            &RangeEnv::new(),
        );
        // The `>= 0` half is free: the comparator itself pins the low end.
        // The high end is the entry value, so `length > i_entry`.
        assert_eq!(guards(&p), [PreheaderGuard::LengthAtLeast(entry_term(1))]);
        assert_eq!(l.direction(), Direction::Decreasing);
    }

    #[test]
    fn descending_exclusive_zero_bound_still_pins_the_low_end() {
        // for (i = n; i > 0; i--) a[i - 1]
        let l = mk(
            IntRange::unknown(),
            Stride::Const(-1),
            ExitCmp::Le,
            BoundSource::Const(0),
        );
        let span = l.iv_span(&RangeEnv::new()).unwrap();
        // Runs while `i > 0`, so every executed value is at least 1.
        assert_eq!(span.span.numeric, IntRange::new(1, i32::MAX));
        let p = l.prove_index_in_bounds(
            &IndexExpr::shifted(IV, -1),
            IntRange::array_length(),
            &RangeEnv::new(),
        );
        assert_eq!(guards(&p), [PreheaderGuard::LengthAtLeast(entry_term(0))]);
    }

    #[test]
    fn descending_stride_must_match_the_comparator() {
        // A positive stride under a low-side exit is not a counted loop.
        let bad = mk(
            IntRange::constant(10),
            Stride::Const(1),
            ExitCmp::Le,
            BoundSource::Const(0),
        );
        assert_eq!(
            bad.iv_span(&RangeEnv::new()),
            Err(RefusalReason::DirectionMismatch)
        );
        // Twin: the same loop counting the right way is provable.
        let good = mk(
            IntRange::constant(10),
            Stride::Const(-1),
            ExitCmp::Le,
            BoundSource::Const(0),
        );
        assert!(good.iv_span(&RangeEnv::new()).is_ok());
    }

    #[test]
    fn zero_stride_is_never_a_counted_loop() {
        let l = mk(
            IntRange::constant(0),
            Stride::Const(0),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        assert_eq!(l.iv_span(&RangeEnv::new()), Err(RefusalReason::ZeroStride));
    }

    // -- non-unit strides ---------------------------------------------------

    #[test]
    fn non_unit_ascending_stride_widens_the_wrap_headroom() {
        // for (i = 0; i < n; i += 3): last index <= n-1, next value n+2.
        let l = mk(
            IntRange::constant(0),
            Stride::Const(3),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        let p = l.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::array_length(),
            &RangeEnv::new(),
        );
        assert_eq!(
            guards(&p),
            [
                PreheaderGuard::AtMost {
                    term: bound_term(BoundSource::Local(N), 0),
                    limit: i32::MAX - 2,
                },
                PreheaderGuard::LengthAtLeast(bound_term(BoundSource::Local(N), 0)),
            ]
        );
        // Twin: knowing the limit is small discharges the wrap obligation
        // statically, and only the length guard survives.
        let mut small = l.clone();
        small.bound_range = IntRange::new(0, 1000);
        let p2 = small.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::array_length(),
            &RangeEnv::new(),
        );
        assert_eq!(
            guards(&p2),
            [PreheaderGuard::LengthAtLeast(bound_term(
                BoundSource::Local(N),
                0
            ))]
        );
        assert_eq!(
            small.iv_span(&RangeEnv::new()).unwrap().overflow,
            OverflowModel::NoWrapProven
        );
    }

    #[test]
    fn negative_non_unit_stride_counts_down() {
        // for (i = 10; i > 0; i -= 3) → 10, 7, 4, 1 → 4 trips.
        let l = mk(
            IntRange::constant(10),
            Stride::Const(-3),
            ExitCmp::Le,
            BoundSource::Const(0),
        );
        assert_eq!(
            l.trip_count(&RangeEnv::new()).and_then(|t| t.exact()),
            Some(4)
        );
        assert_eq!(
            l.iv_span(&RangeEnv::new()).unwrap().span.numeric,
            IntRange::new(1, 10)
        );
    }

    // -- bound provenance ---------------------------------------------------

    #[test]
    fn bound_from_a_constant_local_field_and_array_length_all_work() {
        for bound in [
            BoundSource::Const(64),
            BoundSource::Local(N),
            BoundSource::ArrayLength(3),
            BoundSource::Field {
                cp_index: 7,
                receiver_local: Some(0),
            },
            BoundSource::Field {
                cp_index: 9,
                receiver_local: None,
            },
        ] {
            let l = mk(
                IntRange::constant(0),
                Stride::Const(1),
                ExitCmp::Ge,
                bound.clone(),
            );
            let p = l.prove_index_in_bounds(
                &IndexExpr::identity(IV),
                IntRange::array_length(),
                &RangeEnv::new(),
            );
            assert!(
                matches!(p, BoundsProof::Guarded(_) | BoundsProof::Static),
                "bound {bound:?} should be provable, got {p:?}"
            );
        }
    }

    #[test]
    fn a_field_bound_needs_a_heap_stable_body() {
        let field = BoundSource::Field {
            cp_index: 7,
            receiver_local: Some(0),
        };
        let mut l = mk(IntRange::constant(0), Stride::Const(1), ExitCmp::Ge, field);
        // MUST REFUSE: the body can store to the field between the pre-header
        // guard and a later iteration's exit test.
        l.heap_stable = false;
        assert_eq!(
            l.iv_span(&RangeEnv::new()),
            Err(RefusalReason::BoundNotInvariant)
        );
        // MUST ACCEPT twin.
        l.heap_stable = true;
        assert!(l.iv_span(&RangeEnv::new()).is_ok());
    }

    #[test]
    fn a_bound_local_the_body_writes_is_refused() {
        let mut l = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        // MUST REFUSE: raising `n` inside the loop makes a pre-header
        // `length >= n` guard stale on a later trip.
        l.modified_locals |= 1u64 << N;
        assert_eq!(
            l.iv_span(&RangeEnv::new()),
            Err(RefusalReason::BoundNotInvariant)
        );
        // MUST ACCEPT twin.
        l.modified_locals = 1u64 << IV;
        assert!(l.iv_span(&RangeEnv::new()).is_ok());
    }

    #[test]
    fn min_and_max_bounds_fold_through_the_lattice() {
        // for (i = 0; i < Math.min(16, n); i++) into a 16-element array: the
        // numeric side alone finishes the proof, no guard at all.
        let min_bound = BoundSource::Min(
            Box::new(BoundSource::Const(16)),
            Box::new(BoundSource::Local(N)),
        );
        let l = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            min_bound,
        );
        assert_eq!(
            l.prove_index_in_bounds(
                &IndexExpr::identity(IV),
                IntRange::constant(16),
                &RangeEnv::new()
            ),
            BoundsProof::Static
        );
        // `Math.max(0, n)` gives a non-negative limit, which is enough to
        // prove the *low* end of the span without any guard.
        let max_bound = BoundSource::Max(
            Box::new(BoundSource::Const(0)),
            Box::new(BoundSource::Local(N)),
        );
        let l2 = mk(
            IntRange::unknown(),
            Stride::Const(-1),
            ExitCmp::Lt,
            max_bound,
        );
        let span = l2.iv_span(&RangeEnv::new()).unwrap();
        assert!(span.span.numeric.is_non_negative());
        assert!(span.guards.is_empty());

        // Twin: without the `max`, the same descending loop can reach `n`,
        // which may be negative — so the proof asks for the guard instead of
        // assuming the limit is sane.
        let l3 = mk(
            IntRange::unknown(),
            Stride::Const(-1),
            ExitCmp::Lt,
            BoundSource::Local(N),
        );
        assert!(!l3
            .iv_span(&RangeEnv::new())
            .unwrap()
            .span
            .numeric
            .is_non_negative());
        let p3 = l3.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::array_length(),
            &RangeEnv::new(),
        );
        assert_eq!(
            guards(&p3),
            [
                // `n` must not sit at Integer.MIN_VALUE, or `n - 1` underflows.
                PreheaderGuard::AtLeast {
                    term: bound_term(BoundSource::Local(N), 0),
                    limit: i32::MIN + 1,
                },
                PreheaderGuard::NonNegative(bound_term(BoundSource::Local(N), 0)),
                PreheaderGuard::LengthAtLeast(entry_term(1)),
            ]
        );
    }

    // -- the overflow model -------------------------------------------------

    #[test]
    fn inclusive_loop_at_integer_max_value_must_refuse() {
        // MUST REFUSE: `for (i = 0; i <= n; i++)` with `n == Integer.MAX_VALUE`
        // wraps `i` negative while the exit test keeps passing. No pre-header
        // guard can rescue it — every admissible `n` wraps.
        let mut l = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Gt,
            BoundSource::Local(N),
        );
        l.bound_range = IntRange::constant(i32::MAX);
        assert_eq!(l.iv_span(&RangeEnv::new()), Err(RefusalReason::IvMayWrap));
        assert_eq!(
            refusal(&l.prove_index_in_bounds(
                &IndexExpr::identity(IV),
                IntRange::array_length(),
                &RangeEnv::new()
            )),
            RefusalReason::IvMayWrap
        );

        // MUST ACCEPT twin: the identical loop with a limit known to be small
        // needs no wrap guard at all.
        let mut ok = l.clone();
        ok.bound_range = IntRange::new(0, 1000);
        let proven = ok.iv_span(&RangeEnv::new()).unwrap();
        assert_eq!(proven.overflow, OverflowModel::NoWrapProven);
        assert_eq!(
            guards(&ok.prove_index_in_bounds(
                &IndexExpr::identity(IV),
                IntRange::array_length(),
                &RangeEnv::new()
            )),
            [PreheaderGuard::LengthAtLeast(bound_term(
                BoundSource::Local(N),
                1
            ))]
        );
    }

    #[test]
    fn descending_loop_at_integer_min_value_must_refuse() {
        // MUST REFUSE: `for (i = ?; i >= n; i--)` with `n == Integer.MIN_VALUE`
        // underflows on the step that follows the last executed iteration.
        let mut l = mk(
            IntRange::unknown(),
            Stride::Const(-1),
            ExitCmp::Lt,
            BoundSource::Local(N),
        );
        l.bound_range = IntRange::constant(i32::MIN);
        assert_eq!(l.iv_span(&RangeEnv::new()), Err(RefusalReason::IvMayWrap));
        // MUST ACCEPT twin.
        l.bound_range = IntRange::new(0, 10);
        assert!(l.iv_span(&RangeEnv::new()).is_ok());
    }

    #[test]
    fn index_expression_overflow_is_refused_not_wrapped() {
        // MUST REFUSE: `a[2 * i]` where `i` can reach `Integer.MAX_VALUE - 1`.
        let l = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        let doubled = IndexExpr {
            iv_local: IV,
            scale: 2,
            offset: 0,
        };
        assert_eq!(
            l.index_span(&doubled, &RangeEnv::new()),
            Err(RefusalReason::IndexMayWrap)
        );
        // MUST ACCEPT twin: a limit small enough that `2 * i` cannot wrap.
        let mut small = l.clone();
        small.bound_range = IntRange::new(0, 100);
        let span = small.index_span(&doubled, &RangeEnv::new()).unwrap();
        assert_eq!(span.span.numeric, IntRange::new(0, 198));
        assert_eq!(
            guards(&small.prove_index_in_bounds(
                &doubled,
                IntRange::array_length(),
                &RangeEnv::new()
            )),
            [PreheaderGuard::LengthAtLeast(SymBound::constant(199))]
        );
    }

    #[test]
    fn shifted_index_moves_the_length_requirement_by_the_same_amount() {
        // `for (i = 0; i < n; i++) a[i + 1]` needs `length >= n + 1`.
        let l = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        assert_eq!(
            guards(&l.prove_index_in_bounds(
                &IndexExpr::shifted(IV, 1),
                IntRange::array_length(),
                &RangeEnv::new()
            )),
            [PreheaderGuard::LengthAtLeast(bound_term(
                BoundSource::Local(N),
                1
            ))]
        );
        // A negative displacement can push the index below the array base, so
        // the low half stops being free.
        let p = l.prove_index_in_bounds(
            &IndexExpr::shifted(IV, -1),
            IntRange::array_length(),
            &RangeEnv::new(),
        );
        assert_eq!(refusal(&p), RefusalReason::IndexMayBeNegative);
    }

    #[test]
    fn an_index_on_another_local_is_not_this_loops_business() {
        let l = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        assert_eq!(
            l.index_span(&IndexExpr::identity(9), &RangeEnv::new()),
            Err(RefusalReason::NotTheInductionVariable)
        );
    }

    // -- runtime strides ----------------------------------------------------

    #[test]
    fn a_runtime_stride_is_admitted_only_behind_a_sign_and_magnitude_guard() {
        // MUST ACCEPT (guarded): `for (j = i; j < n; j += step)` — the Sieve
        // inner loop shape.
        let l = mk(
            IntRange::unknown(),
            Stride::Variable(4),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        let p = l.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::array_length(),
            &RangeEnv::new(),
        );
        assert_eq!(
            guards(&p),
            [
                PreheaderGuard::StrideInRange {
                    local: 4,
                    headroom: bound_term(BoundSource::Local(N), -1),
                },
                PreheaderGuard::NonNegative(entry_term(0)),
                PreheaderGuard::LengthAtLeast(bound_term(BoundSource::Local(N), 0)),
            ]
        );
        assert_eq!(l.direction(), Direction::Unknown);

        // MUST REFUSE: the same runtime stride under a decreasing exit test.
        // A guard proving `step >= 0` would contradict the direction, and a
        // negative-step guard has no upper witness to hang the length on.
        let down = mk(
            IntRange::unknown(),
            Stride::Variable(4),
            ExitCmp::Le,
            BoundSource::Const(0),
        );
        assert_eq!(
            down.iv_span(&RangeEnv::new()),
            Err(RefusalReason::UnknownStride)
        );
    }

    // -- loop form ----------------------------------------------------------

    #[test]
    fn a_post_tested_loop_folds_its_untested_first_iteration_into_the_span() {
        // MUST REFUSE: `do { a[i] } while (i++ < n)` with an unknown entry
        // value — the first access is not covered by any test.
        let mut l = mk(
            IntRange::unknown(),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        l.form = LoopForm::PostTested;
        assert_eq!(
            l.iv_span(&RangeEnv::new()),
            Err(RefusalReason::UnboundedEntry)
        );
        // MUST ACCEPT twin: a known entry value gives the extra witness.
        l.iv.init = IntRange::constant(0);
        let p = l.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::array_length(),
            &RangeEnv::new(),
        );
        assert_eq!(
            guards(&p),
            [
                PreheaderGuard::LengthAtLeast(bound_term(BoundSource::Local(N), 0)),
                // The untested first iteration indexes with the entry value —
                // known to be 0 here, so the length must simply be non-empty.
                PreheaderGuard::LengthAtLeast(SymBound::constant(1)),
            ]
        );
    }

    // -- trip counts --------------------------------------------------------

    #[test]
    fn trip_counts_cover_both_inclusivities_and_both_directions() {
        let env = RangeEnv::new();
        let up = |init: i32, bound: i32, stride: i32, cmp: ExitCmp| {
            mk(
                IntRange::constant(init),
                Stride::Const(stride),
                cmp,
                BoundSource::Const(bound),
            )
            .trip_count(&env)
            .and_then(|t| t.exact())
        };
        assert_eq!(up(0, 10, 1, ExitCmp::Ge), Some(10)); // i < 10
        assert_eq!(up(0, 10, 1, ExitCmp::Gt), Some(11)); // i <= 10
        assert_eq!(up(0, 10, 3, ExitCmp::Ge), Some(4)); // 0,3,6,9
        assert_eq!(up(10, 0, -1, ExitCmp::Le), Some(10)); // i > 0
        assert_eq!(up(10, 0, -1, ExitCmp::Lt), Some(11)); // i >= 0
    }

    #[test]
    fn a_zero_trip_loop_is_proved_zero_trip_and_needs_no_check() {
        // MUST ACCEPT (trivially): `for (i = 5; i < 5; i++) a[i]` never runs,
        // so there is no access to guard.
        let l = mk(
            IntRange::constant(5),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Const(5),
        );
        let tc = l.trip_count(&RangeEnv::new()).unwrap();
        assert!(tc.is_zero());
        assert!(l.iv_span(&RangeEnv::new()).unwrap().span.numeric.is_empty());
        assert_eq!(
            l.prove_index_in_bounds(
                &IndexExpr::identity(IV),
                IntRange::constant(0),
                &RangeEnv::new()
            ),
            BoundsProof::Static
        );
        // Twin: one more element of range and the loop really does run.
        let runs = mk(
            IntRange::constant(4),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Const(5),
        );
        assert_eq!(
            runs.trip_count(&RangeEnv::new()).and_then(|t| t.exact()),
            Some(1)
        );
    }

    #[test]
    fn an_unknown_entry_value_gives_a_trip_count_range_not_a_number() {
        let mut l = mk(
            IntRange::new(0, 4),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        l.bound_range = IntRange::new(8, 10);
        let tc = l.trip_count(&RangeEnv::new()).unwrap();
        assert_eq!(tc.min, 4); // init 4, bound 8 → 4,5,6,7
        assert_eq!(tc.max, 10); // init 0, bound 10 → 0..9
        assert_eq!(tc.exact(), None);
    }

    #[test]
    fn a_limit_that_is_the_guarded_arrays_own_length_needs_no_guard() {
        // for (i = 0; i < a.length; i++) a[i] — the shape `bce.rs` spends a
        // whole-method `arraylength` provenance pass to prove.
        let l = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::ArrayLength(0),
        );
        assert_eq!(
            l.prove_index_in_bounds_of(
                &IndexExpr::identity(IV),
                Some(0),
                IntRange::array_length(),
                &RangeEnv::new()
            ),
            BoundsProof::Static
        );
        // MUST NOT generalise to another array: `out[i]` under a guard on
        // `a.length` keeps its check — that is the multi-array out-of-bounds
        // store the provenance pass exists to prevent.
        assert_eq!(
            guards(&l.prove_index_in_bounds_of(
                &IndexExpr::identity(IV),
                Some(5),
                IntRange::array_length(),
                &RangeEnv::new()
            )),
            [PreheaderGuard::LengthAtLeast(bound_term(
                BoundSource::ArrayLength(0),
                0
            ))]
        );
        // Inclusive over the same array is a real off-by-one: it keeps a guard
        // (one that can never pass) rather than inheriting the tautology.
        let incl = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Gt,
            BoundSource::ArrayLength(0),
        );
        assert_eq!(
            guards(&incl.prove_index_in_bounds_of(
                &IndexExpr::identity(IV),
                Some(0),
                IntRange::array_length(),
                &RangeEnv::new()
            )),
            [
                PreheaderGuard::AtMost {
                    term: bound_term(BoundSource::ArrayLength(0), 0),
                    limit: i32::MAX - 1,
                },
                PreheaderGuard::LengthAtLeast(bound_term(BoundSource::ArrayLength(0), 1)),
            ]
        );
    }

    // -- nesting ------------------------------------------------------------

    #[test]
    fn an_inner_bound_that_is_the_outer_iv_inherits_the_outer_range() {
        // for (i = 0; i < n; i++)
        //     for (j = 0; j <= i; j++) a[j]
        let outer = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        let inner = CountedLoop {
            header_pc: 10,
            back_edge_pc: 20,
            iv: AffineIv {
                local: 3,
                init: IntRange::constant(0),
                stride: Stride::Const(1),
            },
            cmp: ExitCmp::Gt, // inclusive: `j <= i`
            bound: BoundSource::Local(IV),
            bound_range: IntRange::unknown(),
            form: LoopForm::PreTested,
            modified_locals: 1u64 << 3,
            heap_stable: true,
            has_other_exit: false,
        };

        // Without the nest, the inclusive comparator needs the
        // `i != Integer.MAX_VALUE` wrap guard.
        let bare = inner.iv_span(&RangeEnv::new()).unwrap();
        assert_eq!(bare.overflow, OverflowModel::NoWrapGuarded);
        assert_eq!(
            bare.guards,
            vec![PreheaderGuard::AtMost {
                term: bound_term(BoundSource::Local(IV), 0),
                limit: i32::MAX - 1,
            }]
        );

        // Inside the nest, the outer loop's proven range for `i` is
        // `[0, MAX - 1]`, which discharges the wrap obligation statically.
        let env = RangeEnv::new().with_local(N, IntRange::unknown());
        let nested_env = env.with_loop_iv(&outer);
        assert_eq!(nested_env.local(IV), IntRange::new(0, i32::MAX - 1));
        let nested = inner.iv_span(&nested_env).unwrap();
        assert_eq!(nested.overflow, OverflowModel::NoWrapProven);
        assert!(nested.guards.is_empty());
        // The inner index still needs `length > i`, which is the whole point
        // of an inclusive inner bound.
        assert_eq!(
            guards(&inner.prove_index_in_bounds(
                &IndexExpr::identity(3),
                IntRange::array_length(),
                &nested_env
            )),
            [PreheaderGuard::LengthAtLeast(bound_term(
                BoundSource::Local(IV),
                1
            ))]
        );
    }

    #[test]
    fn a_bounded_outer_range_makes_the_inner_loop_static() {
        // for (i = 0; i < 8; i++) for (j = 0; j <= i; j++) a[j], a.length 16.
        let outer = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Const(8),
        );
        let inner = CountedLoop {
            header_pc: 10,
            back_edge_pc: 20,
            iv: AffineIv {
                local: 3,
                init: IntRange::constant(0),
                stride: Stride::Const(1),
            },
            cmp: ExitCmp::Gt,
            bound: BoundSource::Local(IV),
            bound_range: IntRange::unknown(),
            form: LoopForm::PreTested,
            modified_locals: 1u64 << 3,
            heap_stable: true,
            has_other_exit: false,
        };
        let env = RangeEnv::new().with_loop_iv(&outer);
        assert_eq!(env.local(IV), IntRange::new(0, 7));
        assert_eq!(
            inner.prove_index_in_bounds(&IndexExpr::identity(3), IntRange::constant(16), &env),
            BoundsProof::Static
        );
    }

    // -- minimum trip count -------------------------------------------------

    fn tc_guards(p: &TripCountProof) -> Vec<PreheaderGuard> {
        match p {
            TripCountProof::Guarded(g) => g.clone(),
            other => panic!("expected Guarded, got {other:?}"),
        }
    }

    fn tc_refusal(p: &TripCountProof) -> RefusalReason {
        match p {
            TripCountProof::Refused(r) => *r,
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_limit_yields_a_trip_count_guard_instead_of_a_refusal() {
        // for (i = 0; i < n; i++) — the commonest loop in Java, and the one
        // the vectorization gate reported as its single largest refusal source.
        let env = RangeEnv::new();
        let l = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        // The fact that made every profitability floor refuse: nothing at
        // compile time rules out a zero-trip loop.
        assert_eq!(l.trip_count(&env).unwrap().min, 0);

        // `trip >= 4` is now one pre-header compare: `n >= 4`. The exclusive
        // comparator's -1 and the "+1 because the entry value is itself an
        // executed iteration" cancel exactly, so the witness is bare `n`.
        assert_eq!(
            tc_guards(&l.prove_trip_count_at_least(4, &env)),
            [PreheaderGuard::TripCountAtLeast {
                term: bound_term(BoundSource::Local(N), 0),
                minimum: 4,
            }]
        );

        // The witness folds in the entry value and the comparator, so a
        // consumer never has to know either. `for (i = 3; i <= n; i++)` runs
        // `n - 2` times, so `trip >= 4` is `n - 2 >= 4`.
        let shifted = mk(
            IntRange::constant(3),
            Stride::Const(1),
            ExitCmp::Gt,
            BoundSource::Local(N),
        );
        assert_eq!(
            tc_guards(&shifted.prove_trip_count_at_least(4, &env)),
            [PreheaderGuard::TripCountAtLeast {
                term: bound_term(BoundSource::Local(N), -2),
                minimum: 4,
            }]
        );

        // A partly-known entry value uses its HIGHEST admissible value — the
        // fewest iterations the loop can run. Conservative, never optimistic.
        let ranged = mk(
            IntRange::new(0, 5),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        assert_eq!(
            tc_guards(&ranged.prove_trip_count_at_least(2, &env)),
            [PreheaderGuard::TripCountAtLeast {
                term: bound_term(BoundSource::Local(N), -5),
                minimum: 2,
            }]
        );
    }

    #[test]
    fn a_provable_trip_count_emits_no_guard_at_all() {
        let env = RangeEnv::new();
        // for (i = 0; i < 16; i++) — 16 trips, known at compile time.
        let l = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Const(16),
        );
        assert_eq!(l.trip_count(&env).and_then(|t| t.exact()), Some(16));
        for minimum in [0u64, 1, 2, 4, 8, 16] {
            assert_eq!(
                l.prove_trip_count_at_least(minimum, &env),
                TripCountProof::Static,
                "trip >= {minimum} is a compile-time fact here"
            );
        }
        // MUST REFUSE twin: a demand the loop provably cannot meet is a
        // refusal, not a guard that always fails.
        assert_eq!(
            tc_refusal(&l.prove_trip_count_at_least(17, &env)),
            RefusalReason::UnusableBound
        );

        // A runtime limit whose range is known also settles statically.
        let mut ranged = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        ranged.bound_range = IntRange::new(8, 10);
        assert_eq!(
            ranged.prove_trip_count_at_least(8, &env),
            TripCountProof::Static
        );
        // One more than the proven floor, and it becomes a runtime question.
        assert_eq!(
            tc_guards(&ranged.prove_trip_count_at_least(9, &env)),
            [PreheaderGuard::TripCountAtLeast {
                term: bound_term(BoundSource::Local(N), 0),
                minimum: 9,
            }]
        );
    }

    #[test]
    fn a_trip_count_minimum_is_refused_where_no_witness_is_expressible() {
        let env = RangeEnv::new();

        // Non-unit stride: the count needs a division `SymBound` cannot spell.
        assert_eq!(
            tc_refusal(
                &mk(
                    IntRange::constant(0),
                    Stride::Const(4),
                    ExitCmp::Ge,
                    BoundSource::Local(N),
                )
                .prove_trip_count_at_least(4, &env)
            ),
            RefusalReason::UnsupportedLoopForm
        );
        // Decreasing: the count is `entry - limit + 1`, and `SymBound` cannot
        // negate its base term.
        assert_eq!(
            tc_refusal(
                &mk(
                    IntRange::constant(100),
                    Stride::Const(-1),
                    ExitCmp::Le,
                    BoundSource::Local(N),
                )
                .prove_trip_count_at_least(4, &env)
            ),
            RefusalReason::UnsupportedLoopForm
        );
        // Unknown entry value: nothing to subtract the limit from.
        assert_eq!(
            tc_refusal(
                &mk(
                    IntRange::unknown(),
                    Stride::Const(1),
                    ExitCmp::Ge,
                    BoundSource::Local(N),
                )
                .prove_trip_count_at_least(4, &env)
            ),
            RefusalReason::UnboundedEntry
        );
        // Runtime stride, zero stride, direction mismatch — the same verdicts
        // `iv_span` gives, so the two proofs never disagree about the cause.
        assert_eq!(
            tc_refusal(
                &mk(
                    IntRange::constant(0),
                    Stride::Variable(4),
                    ExitCmp::Ge,
                    BoundSource::Local(N),
                )
                .prove_trip_count_at_least(4, &env)
            ),
            RefusalReason::UnknownStride
        );
        assert_eq!(
            tc_refusal(
                &mk(
                    IntRange::constant(0),
                    Stride::Const(0),
                    ExitCmp::Ge,
                    BoundSource::Local(N),
                )
                .prove_trip_count_at_least(4, &env)
            ),
            RefusalReason::ZeroStride
        );
        assert_eq!(
            tc_refusal(
                &mk(
                    IntRange::constant(0),
                    Stride::Const(-1),
                    ExitCmp::Ge,
                    BoundSource::Local(N),
                )
                .prove_trip_count_at_least(4, &env)
            ),
            RefusalReason::DirectionMismatch
        );
        // A post-tested loop's first iteration is unconditional; `trip_count`
        // refuses it and so does this.
        let mut post = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        post.form = LoopForm::PostTested;
        assert_eq!(
            tc_refusal(&post.prove_trip_count_at_least(4, &env)),
            RefusalReason::UnsupportedLoopForm
        );
        // A limit the body can change would make the pre-header check stale.
        let mut mutable_bound = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        mutable_bound.modified_locals |= 1u64 << N;
        assert_eq!(
            tc_refusal(&mutable_bound.prove_trip_count_at_least(4, &env)),
            RefusalReason::BoundNotInvariant
        );
        // "At least zero times" is not a claim about a loop, so it holds even
        // for the ones every other verdict refuses.
        assert_eq!(
            post.prove_trip_count_at_least(0, &env),
            TripCountProof::Static
        );
        // A demand no `int` trip count could reach.
        assert_eq!(
            tc_refusal(
                &mk(
                    IntRange::constant(0),
                    Stride::Const(1),
                    ExitCmp::Ge,
                    BoundSource::Local(N),
                )
                .prove_trip_count_at_least(u64::from(u32::MAX) + 1, &env)
            ),
            RefusalReason::UnusableBound
        );
    }

    #[test]
    fn the_new_guard_shape_never_leaks_into_an_existing_proof() {
        // The whole point of adding a variant is that no existing guard list
        // changes. These are the exact lists the bounds/span tests above
        // assert, restated here so a future producer that starts attaching a
        // trip-count obligation to `iv_span` fails one focused test.
        let env = RangeEnv::new();
        let unknown_entry = mk(
            IntRange::unknown(),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        let p = unknown_entry.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::array_length(),
            &env,
        );
        assert_eq!(
            guards(&p),
            [
                PreheaderGuard::NonNegative(entry_term(0)),
                PreheaderGuard::LengthAtLeast(bound_term(BoundSource::Local(N), 0)),
            ]
        );

        let inclusive = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Gt,
            BoundSource::Local(N),
        );
        assert_eq!(
            inclusive.iv_span(&env).unwrap().guards,
            [PreheaderGuard::AtMost {
                term: bound_term(BoundSource::Local(N), 0),
                limit: i32::MAX - 1,
            }]
        );

        let variable_stride = mk(
            IntRange::constant(0),
            Stride::Variable(4),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        assert_eq!(
            variable_stride.iv_span(&env).unwrap().guards,
            [PreheaderGuard::StrideInRange {
                local: 4,
                headroom: bound_term(BoundSource::Local(N), -1),
            }]
        );

        // No proof in this module mints the new shape; only the explicit
        // trip-count request does.
        for l in [&unknown_entry, &inclusive, &variable_stride] {
            let mut every: Vec<PreheaderGuard> =
                l.iv_span(&env).map(|p| p.guards).unwrap_or_default();
            if let BoundsProof::Guarded(g) =
                l.prove_index_in_bounds(&IndexExpr::identity(IV), IntRange::array_length(), &env)
            {
                every.extend(g);
            }
            assert!(
                !every
                    .iter()
                    .any(|g| matches!(g, PreheaderGuard::TripCountAtLeast { .. })),
                "a bounds/span proof minted a trip-count guard: {every:?}"
            );
        }
    }

    // -- naming the guarded array -------------------------------------------

    #[test]
    fn an_array_length_expression_discharges_the_same_tautology_as_a_slot() {
        let env = RangeEnv::new();
        // for (i = 0; i < a.length; i++) a[i], with `a` in local 0.
        let l = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::ArrayLength(0),
        );
        let idx = IndexExpr::identity(IV);
        // The slot-keyed form is exactly the expression-keyed form applied to
        // `ArrayLength(slot)` — including the `None` case, which is what keeps
        // every existing caller byte-identical.
        for slot in [None, Some(0usize), Some(5usize)] {
            let denoted = slot.map(BoundSource::ArrayLength);
            assert_eq!(
                l.prove_index_in_bounds_of(&idx, slot, IntRange::array_length(), &env),
                l.prove_index_in_bounds_of_array(
                    &idx,
                    denoted.as_ref(),
                    IntRange::array_length(),
                    &env
                ),
                "slot {slot:?} must delegate unchanged"
            );
        }

        // The reason the overload exists: a limit that IS this array's length
        // but is spelled as something other than `ArrayLength(slot)` — a local
        // the caller has proven holds `a.length`, or an IR node. The slot form
        // cannot express it and pays a guard; the expression form discharges it.
        let via_local = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            BoundSource::Local(N),
        );
        assert_eq!(
            guards(&via_local.prove_index_in_bounds_of(
                &idx,
                Some(0),
                IntRange::array_length(),
                &env
            )),
            [PreheaderGuard::LengthAtLeast(bound_term(
                BoundSource::Local(N),
                0
            ))]
        );
        assert_eq!(
            via_local.prove_index_in_bounds_of_array(
                &idx,
                Some(&BoundSource::Local(N)),
                IntRange::array_length(),
                &env
            ),
            BoundsProof::Static
        );
        // MUST NOT generalise: naming a limit that is *not* this array's
        // length keeps the guard, which is the multi-array out-of-bounds store
        // the whole shortcut is fenced against.
        assert_eq!(
            guards(&via_local.prove_index_in_bounds_of_array(
                &idx,
                Some(&BoundSource::Local(N + 1)),
                IntRange::array_length(),
                &env
            )),
            [PreheaderGuard::LengthAtLeast(bound_term(
                BoundSource::Local(N),
                0
            ))]
        );
    }
}
