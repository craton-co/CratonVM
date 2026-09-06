// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Integer-arithmetic loop-invariant code motion.
//!
//! Moved verbatim out of `x64.rs`'s `Integer-arithmetic LICM`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;

//
// Hoists a *straight-line, side-effect-free, non-faulting* integer expression
// out of a loop body into the loop pre-header. The classic target is an
// arithmetic expression on loop-invariant locals/constants, e.g.
//
//     for (int i = 0; i < N; i++) {
//         int inv = base * 3 + 11;   // <-- recomputed every iteration
//         sum += inv + (i & 1);
//     }
//
// where `base` is never written inside the loop. The expression
// `iload base; iconst_3; imul; bipush 11; iadd` produces exactly one int and
// can never throw: every opcode below is a register/immediate ALU op (integer
// add/sub/mul/shift/bitwise). No memory access, no division (idiv can throw
// ArithmeticException), no calls. Therefore the value is identical on every
// iteration and hoisting it is observationally equivalent.
//
// Safety contract enforced by `match_invariant_iarith` / `find_arith_loop_hoists`:
//   * Every operand is either an integer constant or an `iload` of a local
//     that is NOT in the loop's `modified` bitmask (provably loop-invariant).
//   * Only the whitelisted non-faulting opcodes appear (see `ArithStep`).
//   * The matched run is stack-balanced to a net effect of exactly +1, i.e.
//     it leaves a single value behind, with the simulated mini-stack never
//     going negative (well-formed expression).
//   * `idiv`/`irem`/`ldiv`/`lrem` and any long/float/double op are excluded —
//     so the only opcodes present cannot fault.

/// One step of a replayable loop-invariant integer expression. The steps form
/// a postfix (RPN) program: pushes put a value on a value stack, binary ops
/// consume the top two and push the result.
#[derive(Debug, Clone, Copy)]
pub(super) enum ArithStep {
    /// Push an integer constant.
    PushConst(i32),
    /// Push the current value of a loop-invariant local (read once at the
    /// pre-header — the local is provably unmodified inside the loop).
    PushLocal(usize),
    /// Pop b, pop a, push `a OP b`. The byte is the JVM opcode (iadd, isub,
    /// imul, ishl, ishr, iushr, iand, ior, ixor) — all non-faulting.
    BinOp(u8),
}

/// A loop-invariant integer-arithmetic sequence eligible for hoisting.
pub(super) struct ArithLoopHoist {
    /// Bytecode PC of the loop header (back-edge target).
    pub(super) loop_header: usize,
    /// First PC strictly after the back-edge instruction (`loop_end`).
    /// Mirrors `LoopHoist::loop_end` — see that doc for the OSR-soundness
    /// rationale (an OSR entry inside this loop's body must not skip the
    /// preheader, or the in-loop sequence loads a stale cached value).
    pub(super) loop_end: usize,
    /// First bytecode PC of the invariant run.
    pub(super) seq_start: usize,
    /// Bytecode PC just past the invariant run.
    pub(super) seq_end: usize,
    /// RPN program that recomputes the (single) result value.
    pub(super) steps: Vec<ArithStep>,
}

/// Decode the int-pushing instruction at `pc`. Returns `(ArithStep, next_pc)`
/// if it is a constant push or an `iload` of a *loop-invariant* local.
pub(super) fn match_iarith_push(
    code: &[u8],
    pc: usize,
    modified: u64,
    code_len: usize,
) -> Option<(ArithStep, usize)> {
    match code[pc] {
        // iconst_m1..iconst_5
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        0x02..=0x08 => Some((ArithStep::PushConst(code[pc] as i32 - 3), pc + 1)),
        // bipush
        0x10 if pc + 1 < code_len => {
            // Cast: value to i32 (encoding immediate/displacement)
            Some((ArithStep::PushConst(code[pc + 1] as i8 as i32), pc + 2))
        }
        // sipush
        0x11 if pc + 2 < code_len => {
            // Cast: value to i32 (encoding immediate/displacement)
            let v = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
            Some((ArithStep::PushConst(v), pc + 3))
        }
        // iload_0..iload_3
        0x1a..=0x1d => {
            // Widening: u8 -> usize (opcode-relative local index, value fits)
            let l = (code[pc] - 0x1a) as usize;
            if l < 64 && (modified & (1u64 << l)) == 0 {
                Some((ArithStep::PushLocal(l), pc + 1))
            } else {
                None
            }
        }
        // iload (wide index)
        0x15 if pc + 1 < code_len => {
            // Widening: u8 -> wider int (bytecode operand byte, value fits)
            let l = code[pc + 1] as usize;
            if l < 64 && (modified & (1u64 << l)) == 0 {
                Some((ArithStep::PushLocal(l), pc + 1 + 1))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `true` if `op` is a non-faulting binary integer ALU opcode safe to hoist.
/// Deliberately EXCLUDES idiv (0x6c) and irem (0x70), which throw
/// ArithmeticException on divide-by-zero, and all long/float/double ops.
pub(super) fn is_hoistable_iarith_binop(op: u8) -> bool {
    matches!(
        op,
        0x60 | // iadd
        0x64 | // isub
        0x68 | // imul
        0x78 | // ishl
        0x7a | // ishr
        0x7c | // iushr
        0x7e | // iand
        0x80 | // ior
        0x82 // ixor
    )
}

/// Try to match a maximal loop-invariant integer-arithmetic run starting at
/// `pc`. Returns the RPN program and the PC just past the run. The run must
/// leave exactly one value on the (simulated) value stack and consist solely
/// of constant pushes, invariant `iload`s and the whitelisted non-faulting
/// binary ALU ops.
pub(super) fn match_invariant_iarith(
    code: &[u8],
    pc: usize,
    modified: u64,
    code_len: usize,
) -> Option<(Vec<ArithStep>, usize)> {
    // The run must START with a push (constant or invariant load). Anything
    // else means the first value comes from elsewhere on the operand stack
    // and the run is not self-contained.
    let (first, mut cur) = match_iarith_push(code, pc, modified, code_len)?;

    let mut steps: Vec<ArithStep> = vec![first];
    let mut depth: i32 = 1; // simulated value-stack depth
    let mut best_end: Option<usize> = None;
    let mut best_len = steps.len();

    // Greedily extend the run. After each instruction, if depth == 1 the run
    // is a well-formed single-value expression and is a valid stopping point;
    // remember the longest such prefix.
    loop {
        if depth == 1 {
            best_end = Some(cur);
            best_len = steps.len();
        }
        if cur >= code_len {
            break;
        }
        let op = code[cur];
        if let Some((step, next)) = match_iarith_push(code, cur, modified, code_len) {
            steps.push(step);
            depth += 1;
            cur = next;
        } else if is_hoistable_iarith_binop(op) {
            // A binary op needs two operands available.
            if depth < 2 {
                break;
            }
            steps.push(ArithStep::BinOp(op));
            depth -= 1;
            cur = cur + 1;
        } else {
            break;
        }
    }

    // Need at least one operation (a lone push is not worth a slot, and a
    // single invariant load is already cheap / handled elsewhere).
    let end = best_end?;
    steps.truncate(best_len);
    if steps
        .iter()
        .filter(|s| matches!(s, ArithStep::BinOp(_)))
        .count()
        == 0
    {
        return None;
    }
    Some((steps, end))
}

/// Maximum simulated value-stack depth reached while evaluating an RPN
/// arithmetic program. Used to size the shared scratch slot pool.
pub(super) fn arith_expr_max_depth(steps: &[ArithStep]) -> usize {
    let mut depth: usize = 0;
    let mut max: usize = 0;
    for s in steps {
        match s {
            ArithStep::PushConst(_) | ArithStep::PushLocal(_) => {
                depth += 1;
                if depth > max {
                    max = depth;
                }
            }
            ArithStep::BinOp(_) => {
                // Two operands consumed, one result pushed: net -1.
                depth = depth.saturating_sub(1);
            }
        }
    }
    max
}

// ── Affine self-recurrence strength reduction (CRATONVM_JIT_REASSOC) ──
//
// Collapse an unrolled affine recurrence on a single int local —
//   `x = x*c1 + c2; x = x*c1' + c2'; …`  (≥2 consecutive steps)
// — into one `x = x*K + C`. This is C2's Mul/Add reassociation, the
// optimization that dominated the CratonVM-vs-HotSpot CPU gap on compute
// kernels (e.g. `GpuCompute.heavy`: 96 multiply-adds → 1). Exact under Java
// two's-complement (wrapping) `int` arithmetic. The IR optimizer carries the
// same transform for methods it can compile, but the IR backend declines
// loops/arrays, so the in-loop kernels that matter are folded here in the
// single-pass x64 backend instead.

/// Decode `iload` / `iload_0..3` → (local index, next pc).
pub(super) fn decode_int_load(code: &[u8], pc: usize, code_len: usize) -> Option<(usize, usize)> {
    match *code.get(pc)? {
        // Widening: u8 -> usize (opcode-relative local index, value fits)
        0x1a..=0x1d => Some(((code[pc] - 0x1a) as usize, pc + 1)),
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        0x15 if pc + 1 < code_len => Some((code[pc + 1] as usize, pc + 2)),
        _ => None,
    }
}

/// Decode `istore` / `istore_0..3` → (local index, next pc).
pub(super) fn decode_int_store(code: &[u8], pc: usize, code_len: usize) -> Option<(usize, usize)> {
    match *code.get(pc)? {
        // Widening: u8 -> usize (opcode-relative local index, value fits)
        0x3b..=0x3e => Some(((code[pc] - 0x3b) as usize, pc + 1)),
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        0x36 if pc + 1 < code_len => Some((code[pc + 1] as usize, pc + 2)),
        _ => None,
    }
}

/// Decode a small int constant push (`iconst_m1..5` / `bipush` / `sipush`) →
/// (value, next pc).
pub(super) fn decode_int_const_push(
    code: &[u8],
    pc: usize,
    code_len: usize,
) -> Option<(i32, usize)> {
    match *code.get(pc)? {
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        0x02..=0x08 => Some((code[pc] as i32 - 3, pc + 1)),
        // Cast: value to i32 (encoding immediate/displacement)
        0x10 if pc + 1 < code_len => Some((code[pc + 1] as i8 as i32, pc + 2)),
        0x11 if pc + 2 < code_len => Some((
            // Cast: value to i32 (encoding immediate/displacement)
            i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32,
            pc + 3,
        )),
        _ => None,
    }
}

/// One affine self-update of local `k`: `x' = m*x + b`, ending at `end`.
pub(super) struct AffineStep {
    pub(super) m: i32,
    pub(super) b: i32,
    pub(super) end: usize,
}

/// Match a single `iload_k ; [push c1 ; imul] ; [push c2 ; (iadd|isub)] ;
/// istore_k` step on local `k` at `pc`. At least one of the multiply/add must
/// be present (so a bare `iload_k; istore_k` copy is not "folded").
pub(super) fn match_affine_step(
    code: &[u8],
    pc: usize,
    code_len: usize,
    k: usize,
) -> Option<AffineStep> {
    let (lk, mut p) = decode_int_load(code, pc, code_len)?;
    if lk != k {
        return None;
    }
    let (mut m, mut b) = (1i32, 0i32);
    let mut saw_op = false;
    if let Some((c1, p2)) = decode_int_const_push(code, p, code_len) {
        if code.get(p2) == Some(&0x68) {
            // imul
            m = c1;
            p = p2 + 1;
            saw_op = true;
        }
    }
    if let Some((c2, p2)) = decode_int_const_push(code, p, code_len) {
        match code.get(p2) {
            Some(&0x60) => {
                b = c2;
                p = p2 + 1;
                saw_op = true;
            }
            Some(&0x64) => {
                b = c2.wrapping_neg();
                p = p2 + 1;
                saw_op = true;
            }
            _ => {}
        }
    }
    if !saw_op {
        return None;
    }
    let (sk, end) = decode_int_store(code, p, code_len)?;
    if sk != k {
        return None;
    }
    Some(AffineStep { m, b, end })
}

/// Match a maximal run of ≥2 affine self-updates on the same int local from
/// `start_pc`, with no branch target landing strictly inside the folded
/// region. Returns `(local, K, C, end_pc)` equivalent to `local = local*K + C`.
pub(super) fn match_affine_chain(
    code: &[u8],
    start_pc: usize,
    code_len: usize,
    branch_targets: &[bool],
) -> Option<(usize, i32, i32, usize)> {
    let (k, _) = decode_int_load(code, start_pc, code_len)?;
    let mut p = start_pc;
    let mut steps = 0usize;
    let (mut big_k, mut big_c) = (1i32, 0i32);
    let mut end = start_pc;
    loop {
        // A second-or-later step must not begin at a branch target, and no
        // target may land inside a step's bytes — otherwise a branch could
        // jump into code we are about to fold away.
        if steps > 0 && branch_targets.get(p).copied().unwrap_or(true) {
            break;
        }
        let step = match match_affine_step(code, p, code_len, k) {
            Some(s) => s,
            None => break,
        };
        let mut inner = p + 1;
        let mut inner_target = false;
        while inner < step.end {
            if branch_targets.get(inner).copied().unwrap_or(true) {
                inner_target = true;
                break;
            }
            inner += 1;
        }
        if inner_target {
            break;
        }
        // x' = m*x + b  ⇒  K *= m ; C = C*m + b   (Java int wrapping).
        big_c = big_c.wrapping_mul(step.m).wrapping_add(step.b);
        big_k = big_k.wrapping_mul(step.m);
        end = step.end;
        steps += 1;
        p = step.end;
    }
    (steps >= 2).then_some((k, big_k, big_c, end))
}

/// Find loop-invariant integer-arithmetic runs that can be hoisted to a loop
/// pre-header. For nested loops, a run is attributed to the OUTERMOST loop in
/// which all its operands are invariant (largest span first), so it is
/// computed as few times as possible. A run already claimed by an outer loop
/// is not re-hoisted by an inner one.
pub(super) fn find_arith_loop_hoists(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
) -> Vec<ArithLoopHoist> {
    if loops.is_empty() {
        return Vec::new();
    }

    let mut hoists: Vec<ArithLoopHoist> = Vec::new();
    let mut claimed: FxHashSet<usize> = FxHashSet::default();

    let mut sorted_loops = loops.to_vec();
    sorted_loops.sort_by_key(|&(h, b)| std::cmp::Reverse(b.saturating_sub(h)));

    for &(header, back_edge) in &sorted_loops {
        let loop_end = back_edge + bytecode_len_at(code, back_edge);
        if loop_end > code_len {
            continue;
        }
        // A virtual/static call can re-enter Java, trigger a safepoint, or
        // deopt the current compiled frame.  The arithmetic expression is
        // locally invariant, but keeping its synthetic frame value live
        // across that boundary is not yet proven safe.  In particular,
        // Lucene Sorter's recursive merge loop hoisted `middle - 1` across
        // virtual comparisons and later consumed a corrupted bound.  Retain
        // LICM for the compute-only loops it was designed for, but leave any
        // loop containing an invocation in its bytecode order.
        let mut scan = header;
        let mut has_invoke = false;
        while scan < loop_end {
            if matches!(code[scan], 0xb6..=0xba) {
                has_invoke = true;
                break;
            }
            scan += bytecode_len_at(code, scan);
        }
        if has_invoke {
            continue;
        }
        let modified = find_modified_locals(code, header, loop_end);

        let mut pc = header;
        while pc < loop_end && pc < code_len {
            if claimed.contains(&pc) {
                pc += bytecode_len_at(code, pc);
                continue;
            }
            if let Some((steps, seq_end)) = match_invariant_iarith(code, pc, modified, code_len) {
                // The whole run must lie inside this loop body.
                if seq_end <= loop_end && seq_end > pc {
                    // Mark every PC of the run as claimed so neither this nor
                    // an inner loop hoists an overlapping sub-run.
                    let mut q = pc;
                    while q < seq_end {
                        claimed.insert(q);
                        q += bytecode_len_at(code, q);
                    }
                    hoists.push(ArithLoopHoist {
                        loop_header: header,
                        loop_end,
                        seq_start: pc,
                        seq_end,
                        steps,
                    });
                    pc = seq_end;
                    continue;
                }
            }
            pc += bytecode_len_at(code, pc);
        }
    }

    hoists
}

/// Find loop-invariant FP loads (dload/fload of locals not modified in the loop).
/// These can be hoisted to a frame slot before the loop, avoiding redundant
/// loads on every iteration when the local is not XMM-allocated.
pub(super) fn find_fp_loop_hoists(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
) -> Vec<FpLoopHoist> {
    if loops.is_empty() {
        return Vec::new();
    }

    let mut hoists = Vec::new();
    let mut hoisted_pcs: FxHashSet<usize> = FxHashSet::default();

    // Sort loops by span size descending (outermost first)
    let mut sorted_loops = loops.to_vec();
    sorted_loops.sort_by_key(|&(h, b)| std::cmp::Reverse(b.saturating_sub(h)));

    for &(header, back_edge) in &sorted_loops {
        let loop_end = back_edge + bytecode_len_at(code, back_edge);
        if loop_end > code_len {
            continue;
        }

        let modified = find_modified_locals(code, header, loop_end);

        let mut pc = header;
        while pc < loop_end && pc < code_len {
            if hoisted_pcs.contains(&pc) {
                pc += bytecode_len_at(code, pc);
                continue;
            }

            let (local_idx, is_double) = match code[pc] {
                // fload_0..fload_3
                0x22..=0x25 => ((code[pc] - 0x22) as usize, false), // Widening: always safe
                // dload_0..dload_3
                0x26..=0x29 => ((code[pc] - 0x26) as usize, true), // Widening: always safe
                // fload (wide)
                0x17 if pc + 1 < code_len => (code[pc + 1] as usize, false), // Widening: always safe
                // dload (wide)
                0x18 if pc + 1 < code_len => (code[pc + 1] as usize, true), // Widening: always safe
                _ => {
                    pc += bytecode_len_at(code, pc);
                    continue;
                }
            };

            // Check if the local is modified in the loop.
            //
            // A `dload k` READS slots k and k+1, so a loop that writes either
            // one is a loop this value is not invariant across. Checking only
            // `local_idx` is the mirror of the store-side hole fixed in
            // `find_modified_locals` on 2026-09-05, and the two halves have to
            // agree or the pair still admits an unsound hoist.
            let overlap_modified =
                is_double && (local_idx + 1 >= 64 || (modified & (1u64 << (local_idx + 1))) != 0);
            if local_idx < 64 && !overlap_modified && (modified & (1u64 << local_idx)) == 0 {
                hoists.push(FpLoopHoist {
                    loop_header: header,
                    load_pc: pc,
                    local_idx: local_idx,
                    is_double: is_double,
                });
                hoisted_pcs.insert(pc);
            }

            pc += bytecode_len_at(code, pc);
        }
    }

    hoists
}

/// Detect FP strength reduction opportunities within loops.
/// Finds `ldc2_w <2.0>; dmul` patterns where multiply-by-2.0 can be replaced
/// with dadd self (saves ~3 cycles: addsd latency=1-3 vs mulsd latency=3-5).
/// Returns: set of dmul bytecode PCs to replace, and a map from the preceding
/// ldc2_w PC → the dmul PC (so the ldc2_w can be skipped at emission time).
pub(super) fn find_fp_strength_reductions(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    ldc2w_info: &[(usize, i64)],
) -> FxHashSet<usize> {
    let mut pcs = FxHashSet::default();
    let two_bits = 2.0f64.to_bits() as i64; // Cast: JIT ABI convention

    // Build a lookup for ldc2_w PCs → resolved value
    let ldc_map: FxHashMap<usize, i64> = ldc2w_info.iter().copied().collect();

    for &(header, back_edge) in loops {
        let loop_end = back_edge + bytecode_len_at(code, back_edge);
        let mut pc = header;
        while pc < loop_end && pc < code_len {
            // Pattern: ldc2_w <idx>, dmul
            if code[pc] == 0x14 && pc + 3 < loop_end {
                if let Some(&val) = ldc_map.get(&pc) {
                    if val == two_bits && code[pc + 3] == 0x6b {
                        // dmul at pc+3 with 2.0 operand → strength reduce to dadd self
                        pcs.insert(pc + 3);
                    }
                }
            }
            // Pattern: dmul right after a dload (X), ldc2_w 2.0
            // i.e., dload X; ldc2_w 2.0; dmul — already covered above.
            // Also check: ldc2_w 2.0 earlier, then dload, then dmul (commutative)
            pc += bytecode_len_at(code, pc);
        }
    }

    pcs
}
