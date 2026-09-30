// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Recognize the canonical element-wise loop pattern.
//!
//! The shape we're looking for is what `javac` produces for
//! `for (int i = 0; i < bound; i++) { body; }`:
//!
//! ```text
//!   <prelude — possibly computes the bound and stores it in a local>
//!   iconst_0 / iconst_*
//!   istore   iv_slot
//! header: iload iv_slot       (loop header — the back branch target)
//!   <load bound>
//!   if_icmp{ge,gt,le,lt}  exit
//!   <body — any of the analyzer-accepted opcodes>
//!   iinc iv_slot, +k
//!   goto header
//! exit: <post-loop / return>
//! ```
//!
//! Only one backward branch is allowed; the back branch must target the
//! loop header, the very first body instruction after the header must
//! be the loop exit `if_icmp*` (or the header itself is the comparison,
//! depending on `javac` version), and the induction variable's `iinc`
//! must appear in the loop body.
//!
//! Anything that doesn't match is rejected with a precise
//! [`crate::emitter::LoweringError::UnsupportedNode`] reason.
//!
//! # Canonical-shape validation (correctness)
//!
//! The element-wise GPU lowering model is "one CUDA thread per loop
//! iteration, the thread index `tid` *is* the loop variable". That
//! identity only holds for the strictly canonical loop:
//!
//! ```text
//!   for (int i = 0; i < bound; i++) { body; }
//! ```
//!
//! — start value `0`, stride `+1`, strict-less-than (`<`) exit. For
//! any other shape the emitter would silently mis-lower:
//!
//! * `i <= bound` (`if_icmpgt` exit) runs `bound + 1` iterations; the
//!   `tid < bound` dispatch drops the last element.
//! * `i != bound` (`if_icmpeq` exit) is not equivalent to `tid < bound`.
//! * a non-`+1` `iinc` stride means element `stride*tid` is read as
//!   element `tid`.
//!
//! # Non-zero (but non-negative) start values
//!
//! `for (int i = K; i < bound; i++)` with a compile-time-constant
//! `K >= 0` *is* accepted: the emitter folds `K` into the induction
//! register once (`tid + K`, computed a single time right after `tid`
//! itself — see [`crate::lowering::emit::Emitter::apply_loop_start_offset`])
//! and uses that combined register everywhere the raw `tid` used to be
//! used, both for the `iload iv` substitution and the `tid >= bound`
//! loop-guard comparison (which becomes `tid + K >= bound`). See that
//! function's doc comment for why `K == 0` is a no-op (byte-for-byte
//! unchanged PTX from before this feature existed).
//!
//! A **negative** `K` is still rejected — not because the register
//! arithmetic would be wrong (`tid + K` is well-defined for negative
//! `K` too), but because of how the VM sizes the CUDA grid: the host
//! (`vm/src/runtime/offload.rs`) launches enough threads to cover the
//! largest input/output array length, which is `>= bound`. For `K >=
//! 0` the loop's trip count is `bound - K <= bound`, so a `bound`-sized
//! (or larger) launch always has enough threads — the guard simply
//! makes the extra ones exit immediately. For `K < 0` the trip count is
//! `bound - K > bound`, i.e. *more* iterations than the array is long,
//! so a `bound`-sized launch would silently under-provision threads and
//! drop the tail of the loop. Proving the launch is always sized to
//! `bound - K` (not just `bound`) is out of scope here, so negative
//! starts are rejected and fall back to the CPU interpreter — the same
//! "correctness over coverage" policy the rest of this module follows.
//!
//! [`classify_counted_loop`] therefore validates all of the above and
//! rejects (via [`crate::emitter::LoweringError::UnsupportedNode`],
//! which makes the VM fall back to the CPU interpreter — always safe)
//! anything that is not exactly the canonical shape. Correctness over
//! coverage: when in doubt we reject rather than mis-lower.

use crate::emitter::LoweringError;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

/// Outcome of the loop scan.
#[derive(Clone, Debug)]
pub(crate) enum LoopShape {
    /// No backward branch — straight-line kernel.
    StraightLine,
    /// Single counted loop matching the canonical pattern.
    Counted(CountedLoop),
    /// Two-level rectangular nested counted loop
    /// (`for (i...) { for (j...) { body } }`) — see [`NestedLoop`] and
    /// [`detect_nested_loop`].
    Nested(NestedLoop),
}

/// A strictly nested, rectangular 2-D counted loop recognized by
/// [`detect_nested_loop`]: the inner loop is the outer loop's entire
/// body (nothing before or after it), both start at `0`, and each
/// individually satisfies every invariant [`classify_counted_loop`]
/// already enforces for a single loop (stride `+1`, `if_icmpge` exit,
/// …). The SIMT lowering
/// (`crate::lowering::emit::Emitter::emit_nested_loop_guard_and_decompose`)
/// treats the flattened iteration space `[0, R*C)` as the thread-index
/// domain and recovers `(i, j)` from the linear thread index `tid` via
/// `i = tid / C`, `j = tid % C` — see that function's doc comment for
/// why the division is always safe.
#[derive(Clone, Debug)]
pub(crate) struct NestedLoop {
    pub outer: CountedLoop,
    pub inner: CountedLoop,
}

#[derive(Clone, Debug)]
pub(crate) struct CountedLoop {
    /// PC of the loop header (the back-branch target). Execution
    /// reaches this after the prelude.
    pub header_pc: usize,
    /// PC of the loop-exit `if_icmp*` opcode. Its operands are
    /// `iv_slot` (LHS) and the bound expression (RHS).
    pub exit_if_pc: usize,
    /// PC just past the loop-exit branch (first body instruction).
    pub body_start_pc: usize,
    /// PC of the back-branch `goto`/`goto_w` instruction.
    pub back_branch_pc: usize,
    /// PC of the first instruction after the loop (the exit branch's
    /// target).
    pub exit_pc: usize,
    /// Local-variable slot that holds the induction variable.
    pub iv_slot: u16,
    /// Loop-exit comparison opcode. Always `if_icmpge` (0xA2) for an
    /// accepted loop — `classify_counted_loop` rejects anything else,
    /// because only `if_icmpge` is the negation of the canonical
    /// `i < bound` continuation test. Kept so the emitter can assert
    /// the invariant and so future non-canonical lowering has the
    /// information it needs.
    pub exit_op: u8,
    /// Induction-variable `iinc` stride. Always `+1` for an accepted
    /// loop — `classify_counted_loop` rejects any other value. Kept
    /// for the same reason as `exit_op`.
    pub iv_stride: i32,
    /// Compile-time-constant loop start value `K` (`for (i = K; ...)`).
    /// Always `>= 0` for an accepted loop — `classify_counted_loop`
    /// (via `resolve_start_value`) rejects a negative or non-constant
    /// start. `0` is by far the common case. The emitter folds this
    /// into the induction register once, right after computing `tid`
    /// (see `emit::Emitter::apply_loop_start_offset`), so every
    /// downstream use of the induction variable — the loop guard and
    /// every `iload iv` in the body — automatically sees `tid + K`.
    pub iv_start: i32,
}

/// `if_icmpge` — the only loop-exit comparison the canonical
/// element-wise lowering accepts. It is the negation of the
/// canonical `i < bound` loop-continuation test that `javac`
/// emits for `for (int i = 0; i < bound; i++)`.
const IF_ICMPGE: u8 = 0xA2;

/// Scan a method's bytecode and classify its loop shape.
///
/// `cp` is the class's constant pool, needed only to resolve a loop
/// start value pushed by `ldc`/`ldc_w` (a start constant too large for
/// `iconst`/`bipush`/`sipush`) back to its `Integer` value — see
/// `resolve_start_value`. `None` is always safe: without a pool an
/// `ldc`-sourced start simply can't be proven constant and the loop is
/// rejected (falls back to the CPU), exactly like any other
/// unprovable start expression.
pub(crate) fn detect_loop(
    bytes: &[u8],
    cp: Option<&ConstantPool>,
) -> Result<LoopShape, LoweringError> {
    let backs = collect_backward_branches(bytes)?;
    match backs.len() {
        0 => Ok(LoopShape::StraightLine),
        1 => {
            let (branch_pc, header_pc) = backs[0];
            let loop_ = classify_counted_loop(bytes, branch_pc, header_pc, cp)?;
            Ok(LoopShape::Counted(loop_))
        }
        2 => match detect_nested_loop(bytes, backs[0], backs[1], cp) {
            Ok(shape) => Ok(shape),
            // Not the rectangular `for (i) for (j)` shape the 2-D
            // mapping needs. It may still be an outer counted loop
            // whose body happens to contain a sequential inner loop —
            // `for (i) { s = 0; for (j) s += ...; out[i] = s; }`, the
            // canonical matmul/dot-per-row kernel. That one parallelises
            // over `i` only and runs `j` as a real per-thread PTX loop.
            Err(_) => classify_outer_parallel_loop(bytes, &backs, cp),
        },
        _ => classify_outer_parallel_loop(bytes, &backs, cp),
    }
}

/// Classify a loop nest as "outer counted loop is the parallel
/// dimension; every other back-edge is a *sequential* loop the CUDA
/// thread executes itself".
///
/// The 2-D mapping in [`detect_nested_loop`] flattens `[0, R*C)` and
/// gives every `(i, j)` pair its own thread. That is the right shape
/// for a rectangular elementwise kernel and the wrong one for a
/// reduction per row: `out[i] = sum_j w[i*n + j] * x[j]` has a
/// loop-carried accumulator, so its `j` iterations must run in order on
/// one thread.
///
/// This classifier admits exactly that: the outermost back-edge is
/// validated as the canonical counted loop (all the usual invariants —
/// stride `+1`, `if_icmpge` exit, non-negative constant start), and
/// every other back-edge is required to lie strictly inside that loop's
/// *body* region. The interior edges are not classified at all here;
/// `Emitter::walk_cfg` lowers them as ordinary PTX control flow, with
/// the loop header's canonical registers acting as the phi nodes.
///
/// The returned shape is a plain [`LoopShape::Counted`] because the
/// emission path is identical to a single counted loop — one thread per
/// outer iteration, guard, body CFG, epilogue. The only difference is
/// that the body CFG is cyclic, which `walk_cfg` handles on its own.
fn classify_outer_parallel_loop(
    bytes: &[u8],
    backs: &[(usize, usize)],
    cp: Option<&ConstantPool>,
) -> Result<LoopShape, LoweringError> {
    let non_canonical =
        || LoweringError::UnsupportedNode("multi-loop or non-canonical control flow".into());
    // The outermost back-edge is the one whose span `(header, branch)`
    // contains every other. Pick the candidate with the lowest header
    // (ties broken by the highest branch) and then *verify* containment
    // rather than assuming it — two sequential loops also have a lowest
    // header, and they must still be rejected.
    let outer = *backs
        .iter()
        .min_by_key(|(branch, header)| (*header, std::cmp::Reverse(*branch)))
        .ok_or_else(non_canonical)?;
    let (outer_branch, outer_header) = outer;
    for &(branch, header) in backs {
        if (branch, header) == outer {
            continue;
        }
        if header <= outer_header || branch >= outer_branch {
            return Err(non_canonical());
        }
    }
    let loop_ = classify_counted_loop(bytes, outer_branch, outer_header, cp)?;
    // `walk_cfg` only ever sees `body_start_pc .. back_branch_pc`. An
    // interior back-edge outside that window would be silently dropped
    // (it would sit in the pre-loop or post-loop straight-line walk,
    // neither of which lowers control flow at all), so require every one
    // of them to be inside the body the CFG walker actually visits.
    for &(branch, header) in backs {
        if (branch, header) == outer {
            continue;
        }
        if header < loop_.body_start_pc || branch >= loop_.back_branch_pc {
            return Err(non_canonical());
        }
    }
    Ok(LoopShape::Counted(loop_))
}

/// Given exactly two backward branches, determine whether they form a
/// strictly nested rectangular 2-D counted loop
/// (`for (i...) { for (j...) { body } }`, the inner loop being the
/// entirety of the outer loop's body) and, if so, classify both levels.
/// Anything else — two sequential loops (e.g. `TwoLoops.clearTwice`),
/// overlapping/crossing back-edges, or an outer body containing more
/// than just the inner loop — is rejected with the same "multi-loop or
/// non-canonical control flow" reason two sequential loops already got
/// before this feature existed.
fn detect_nested_loop(
    bytes: &[u8],
    a: (usize, usize),
    b: (usize, usize),
    cp: Option<&ConstantPool>,
) -> Result<LoopShape, LoweringError> {
    let non_canonical =
        || LoweringError::UnsupportedNode("multi-loop or non-canonical control flow".into());
    // A back-branch pair is `(branch_pc, header_pc)`. The outer loop's
    // header comes first and its back-branch comes last — the inner
    // loop's whole span (`header..branch`) must sit strictly inside the
    // outer's (`outer_header < inner_header < inner_branch <
    // outer_branch`). Anything else (sequential loops, crossing
    // back-edges) is not nesting.
    let ((outer_branch, outer_header), (inner_branch, inner_header)) = if a.1 < b.1 && b.0 < a.0 {
        (a, b)
    } else if b.1 < a.1 && a.0 < b.0 {
        (b, a)
    } else {
        return Err(non_canonical());
    };
    let outer = classify_counted_loop(bytes, outer_branch, outer_header, cp)?;
    let inner = classify_counted_loop(bytes, inner_branch, inner_header, cp)?;

    // The 2-D SIMT lowering computes both loop variables directly from
    // the flattened thread index (`i = tid / C`, `j = tid % C`) and
    // never executes either loop's own iv-init prelude — see
    // `validate_rectangular_nesting`'s doc comment. That is only sound
    // when both loops start at 0; a non-zero start would need the
    // offset folded in per-dimension (safe in principle, same as the
    // single-loop `K` fold — see the module doc comment above — but not
    // implemented for the 2-D case) so reject it here instead of
    // mis-lowering.
    if outer.iv_start != 0 || inner.iv_start != 0 {
        return Err(LoweringError::UnsupportedNode(
            "nested loop with a non-zero start value is not yet supported — only \
             `for (i = 0; ...) for (j = 0; ...)` is lowered"
                .into(),
        ));
    }
    validate_rectangular_nesting(bytes, &outer, &inner)?;

    Ok(LoopShape::Nested(NestedLoop { outer, inner }))
}

/// Structurally validate that `outer`'s body is exactly
/// `{ <inner iv init to 0>; <inner loop>; }` with nothing else — i.e.
/// the inner loop is the outer loop's entire body, and the outer
/// induction-variable `iinc` immediately follows the inner loop with no
/// other instructions in between. This is what `javac` emits for
/// `for (int i = 0; i < R; i++) { for (int j = 0; j < C; j++) { body } }`
/// with no code before or after the inner loop inside the outer body.
///
/// Rejecting anything else is deliberate: the 2-D lowering never
/// executes the outer body's raw bytecode between `outer.body_start_pc`
/// and `inner.header_pc` (nor between `inner.exit_pc` and
/// `outer.back_branch_pc`) — it recomputes `i`/`j` directly from the
/// thread index and jumps straight to the inner loop's own body. Any
/// instruction in those two spans beyond the expected `j = 0` prelude /
/// outer `iinc` would be silently dropped, which is exactly the
/// "mis-lower" outcome the rest of this module refuses to risk —
/// reject instead.
fn validate_rectangular_nesting(
    bytes: &[u8],
    outer: &CountedLoop,
    inner: &CountedLoop,
) -> Result<(), LoweringError> {
    // 1. `outer.body_start_pc` must be exactly a zero-constant push
    //    immediately followed by an `istore` to `inner.iv_slot`, ending
    //    exactly at `inner.header_pc`.
    let push_pc = outer.body_start_pc;
    if push_pc >= bytes.len() {
        return Err(LoweringError::UnsupportedNode(
            "truncated bytecode between outer loop guard and inner loop".into(),
        ));
    }
    let push_size = instr_size(bytes, push_pc)?;
    let is_zero_push = bytes[push_pc] == 0x03 // iconst_0
        || (bytes[push_pc] == 0x10 && bytes.get(push_pc + 1) == Some(&0)) // bipush 0
        || (bytes[push_pc] == 0x11
            && bytes.get(push_pc + 1) == Some(&0)
            && bytes.get(push_pc + 2) == Some(&0)); // sipush 0
    if !is_zero_push {
        return Err(LoweringError::UnsupportedNode(format!(
            "outer loop body at pc={push_pc} does not open with a zero-constant push \
             for the inner loop's induction variable — non-canonical nested loop"
        )));
    }
    let store_pc = push_pc + push_size;
    let store_slot = istore_slot_at(bytes, store_pc)?;
    if store_slot != Some(inner.iv_slot) {
        return Err(LoweringError::UnsupportedNode(format!(
            "outer loop body at pc={store_pc} does not initialise the inner loop's \
             induction variable (slot {}) immediately after entering the outer body \
             — non-canonical nested loop",
            inner.iv_slot
        )));
    }
    let store_size = instr_size(bytes, store_pc)?;
    if store_pc + store_size != inner.header_pc {
        return Err(LoweringError::UnsupportedNode(
            "outer loop body contains code between the inner loop's induction-variable \
             init and the inner loop header — non-canonical nested loop (only a bare \
             inner `for` as the entire outer body is supported)"
                .into(),
        ));
    }

    // 2. `inner.exit_pc` must be exactly the outer loop's own `iinc`,
    //    ending exactly at `outer.back_branch_pc`.
    let iinc_pc = inner.exit_pc;
    if iinc_pc >= bytes.len() {
        return Err(LoweringError::UnsupportedNode(
            "truncated bytecode between inner loop exit and outer loop back-branch".into(),
        ));
    }
    let iinc_slot = iinc_target_slot(bytes, iinc_pc)?;
    if iinc_slot != Some(outer.iv_slot) {
        return Err(LoweringError::UnsupportedNode(format!(
            "inner loop exit at pc={iinc_pc} is not immediately followed by the outer \
             loop's own `iinc` (slot {}) — non-canonical nested loop (only a bare inner \
             `for` as the entire outer body is supported)",
            outer.iv_slot
        )));
    }
    let iinc_size = instr_size(bytes, iinc_pc)?;
    if iinc_pc + iinc_size != outer.back_branch_pc {
        return Err(LoweringError::UnsupportedNode(
            "outer loop body contains code between the outer `iinc` and the outer \
             back-branch — non-canonical nested loop"
                .into(),
        ));
    }

    Ok(())
}

/// If the instruction at `pc` is an `istore` (narrow, short-form, or
/// wide), return the local slot it writes. `None` for any other opcode
/// or an out-of-range `pc`.
fn istore_slot_at(bytes: &[u8], pc: usize) -> Result<Option<u16>, LoweringError> {
    if pc >= bytes.len() {
        return Ok(None);
    }
    Ok(match bytes[pc] {
        0x36 => bytes.get(pc + 1).map(|&b| b as u16), // istore
        0x3B..=0x3E => Some((bytes[pc] - 0x3B) as u16), // istore_0..3
        0xC4 if bytes.get(pc + 1) == Some(&0x36) => Some(u16::from_be_bytes([
            *bytes.get(pc + 2).ok_or_else(truncated)?,
            *bytes.get(pc + 3).ok_or_else(truncated)?,
        ])),
        _ => None,
    })
}

/// If the instruction at `pc` is an `iinc` (narrow or wide), return the
/// local slot it targets. `None` for any other opcode or an
/// out-of-range `pc`.
fn iinc_target_slot(bytes: &[u8], pc: usize) -> Result<Option<u16>, LoweringError> {
    if pc >= bytes.len() {
        return Ok(None);
    }
    Ok(match bytes[pc] {
        0x84 => Some(*bytes.get(pc + 1).ok_or_else(truncated)? as u16),
        0xC4 if bytes.get(pc + 1) == Some(&0x84) => Some(u16::from_be_bytes([
            *bytes.get(pc + 2).ok_or_else(truncated)?,
            *bytes.get(pc + 3).ok_or_else(truncated)?,
        ])),
        _ => None,
    })
}

/// Return all (branch_pc, target_pc) pairs where target_pc < branch_pc.
/// Errors on malformed bytecode.
fn collect_backward_branches(bytes: &[u8]) -> Result<Vec<(usize, usize)>, LoweringError> {
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < bytes.len() {
        let op = bytes[pc];
        let size = instr_size(bytes, pc)?;
        // 0x99..=0xA7: if* + goto      (3-byte, i16 offset)
        // 0xC6 | 0xC7: ifnull/ifnonnull (3-byte, i16 offset)
        // 0xC8:        goto_w           (5-byte, i32 offset)
        if (0x99..=0xA7).contains(&op) || op == 0xC6 || op == 0xC7 {
            let off = i16::from_be_bytes([
                *bytes.get(pc + 1).ok_or_else(truncated)?,
                *bytes.get(pc + 2).ok_or_else(truncated)?,
            ]) as i32;
            let target = pc as i32 + off;
            // `target == bytes.len()` is one past the last byte — not a
            // valid instruction boundary — so reject it too (`>=`).
            if target < 0 || target as usize >= bytes.len() {
                return Err(LoweringError::UnsupportedNode(format!(
                    "branch at pc={pc} has out-of-range target {target}"
                )));
            }
            if (target as usize) < pc {
                out.push((pc, target as usize));
            }
        } else if op == 0xC8 {
            let off = i32::from_be_bytes([
                *bytes.get(pc + 1).ok_or_else(truncated)?,
                *bytes.get(pc + 2).ok_or_else(truncated)?,
                *bytes.get(pc + 3).ok_or_else(truncated)?,
                *bytes.get(pc + 4).ok_or_else(truncated)?,
            ]);
            let target = pc as i32 + off;
            // `target == bytes.len()` is one past the last byte — not a
            // valid instruction boundary — so reject it too (`>=`).
            if target < 0 || target as usize >= bytes.len() {
                return Err(LoweringError::UnsupportedNode(format!(
                    "goto_w at pc={pc} has out-of-range target {target}"
                )));
            }
            if (target as usize) < pc {
                out.push((pc, target as usize));
            }
        }
        pc += size;
    }
    Ok(out)
}

fn truncated() -> LoweringError {
    LoweringError::UnsupportedNode("truncated bytecode".into())
}

/// Given a back-branch and its target (the loop header), recover the
/// canonical-loop fields. Errors if the shape doesn't match.
fn classify_counted_loop(
    bytes: &[u8],
    back_branch_pc: usize,
    header_pc: usize,
    cp: Option<&ConstantPool>,
) -> Result<CountedLoop, LoweringError> {
    let back_op = bytes[back_branch_pc];
    if back_op != 0xA7 && back_op != 0xC8 {
        return Err(LoweringError::UnsupportedNode(format!(
            "non-canonical back-branch opcode 0x{back_op:02x} at pc={back_branch_pc}"
        )));
    }

    // Walk from header_pc forward. We need three things:
    //  * the loop-exit `if_icmp*` (first compare branching past the
    //    back-branch),
    //  * the `pc` of the `iload` that produced the comparison's LHS
    //    operand (that local is the induction variable), and
    //  * the induction variable's `iinc` (to read its stride).
    let mut exit_if_pc = None;
    let mut exit_pc = None;
    // (pc, local-slot) of the most-recent `iload` seen during the walk.
    // When we reach the exit-if, the second-to-last `iload` is the LHS
    // (`iv`) and the last is the RHS (the bound). javac always emits
    // `iload iv; iload bound; if_icmp*` for the canonical loop header.
    let mut iload_history: Vec<u16> = Vec::new();
    let mut pc = header_pc;
    while pc <= back_branch_pc {
        if pc >= bytes.len() {
            break;
        }
        let op = bytes[pc];
        let size = instr_size(bytes, pc)?;

        // Track `iload` instructions (narrow + wide) so we can recover
        // the induction-variable slot from the exit-if's LHS operand.
        if exit_if_pc.is_none() {
            if let Some(slot) = iload_slot(bytes, pc, op) {
                iload_history.push(slot);
            }
        }

        // Loop-exit comparisons: if_icmp* (0x9F-0xA4) and unary if*
        // (0x99-0x9E) whose target is the post-loop region.
        if (0x99..=0xA4).contains(&op) {
            let off = i16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]) as i32;
            let target = (pc as i32 + off) as usize;
            if target > back_branch_pc && exit_if_pc.is_none() {
                exit_if_pc = Some(pc);
                exit_pc = Some(target);
            }
        }

        pc += size;
    }

    let exit_if_pc = exit_if_pc.ok_or_else(|| {
        LoweringError::UnsupportedNode("no loop-exit comparison found inside loop body".into())
    })?;
    let exit_pc = exit_pc.unwrap();
    let exit_op = bytes[exit_if_pc];

    // ── Bug A: validate the exit comparison is the canonical `i < bound`
    // shape. javac compiles `for (i = 0; i < bound; i++)` with the
    // *negated* test `if_icmpge bound -> exit`. `<=` becomes `if_icmpgt`,
    // `!=` becomes `if_icmpeq`, etc. The emitter's `tid < bound` dispatch
    // is only correct for `if_icmpge`; reject every other comparison so
    // the loop runs on the CPU interpreter instead of being mis-lowered.
    if exit_op != IF_ICMPGE {
        return Err(LoweringError::UnsupportedNode(format!(
            "non-canonical loop-exit comparison 0x{exit_op:02x} at pc={exit_if_pc} \
             — only `if_icmpge` (the negation of the canonical `i < bound` test) \
             is lowered; `<=`/`!=`/`>`/`>=` loops run on the CPU"
        )));
    }

    // The induction variable is the LHS of the exit comparison. There
    // are two operand shapes javac emits for `i < <bound>`:
    //
    //   iload iv; iload bound;             if_icmpge    (cached length)
    //   iload iv; aload arr; arraylength;  if_icmpge    (inline .length)
    //
    // The first has two `iload`s, so `iv` is the second-to-last. The
    // second has only one, and it IS `iv`: the bound is produced by
    // `arraylength`, not by a load.
    //
    // Only the cached-length form used to be recognized, which made
    // `for (int i = 0; i < out.length; i++)` -- the shape most people
    // write first -- fall silently back to the CPU while the
    // hoist-into-a-local rewrite offloaded. Nothing about the inline
    // form is harder to lower: `locate_bound` resolves both to the same
    // `BoundSource::ParamLen`, and the launch grid is sized from the
    // largest array argument either way.
    let bound_is_inline_arraylength = exit_if_pc > 0 && bytes[exit_if_pc - 1] == 0xBE;
    let iv_slot = if bound_is_inline_arraylength {
        *iload_history.last().ok_or_else(|| {
            LoweringError::UnsupportedNode(format!(
                "loop header at pc={header_pc} ends in `arraylength; if_icmp*` \
                 but loads no induction variable"
            ))
        })?
    } else {
        if iload_history.len() < 2 {
            return Err(LoweringError::UnsupportedNode(format!(
                "loop header at pc={header_pc} does not have the canonical \
                 `iload iv; iload bound; if_icmp*` (or `iload iv; aload arr; \
                 arraylength; if_icmp*`) operand shape"
            )));
        }
        iload_history[iload_history.len() - 2]
    };

    // Find the induction variable's `iinc` in the loop body and read
    // its stride.
    let iv_stride =
        find_iv_stride(bytes, header_pc, back_branch_pc, iv_slot)?.ok_or_else(|| {
            LoweringError::UnsupportedNode(format!(
                "no `iinc` for induction-variable slot {iv_slot} found in the \
                 loop body — induction variable is not a simple counter"
            ))
        })?;

    // ── Bug B (stride): the element-wise lowering rewrites `iload iv`
    // to the raw thread index `tid` and *skips* the iv `iinc`. That is
    // only correct when the stride is exactly +1; a stride of `k` would
    // make the kernel read element `tid` where the loop wanted element
    // `k*tid`. Reject any non-unit (including negative) stride.
    if iv_stride != 1 {
        return Err(LoweringError::UnsupportedNode(format!(
            "non-unit loop stride {iv_stride} (iinc on slot {iv_slot}) — only \
             `i++` (stride +1) is lowered; strided loops run on the CPU"
        )));
    }

    // ── Bug B (start value): the lowering treats `tid` (optionally
    // offset by a constant `K`) as the loop variable. Resolve the
    // pre-loop's constant `istore iv` and require `K >= 0` — a
    // negative start would need more kernel threads than the host's
    // `bound`-sized launch provides (see the module doc comment for
    // the full derivation). `for (i = 5; ...)` is fine (`K = 5`, every
    // access offset by +5, folded into the induction register once);
    // `for (i = -5; ...)` is rejected.
    let iv_start = resolve_start_value(bytes, header_pc, iv_slot, cp)?;

    let body_start_pc = exit_if_pc + instr_size(bytes, exit_if_pc)?;

    Ok(CountedLoop {
        header_pc,
        exit_if_pc,
        body_start_pc,
        back_branch_pc,
        exit_pc,
        iv_slot,
        exit_op,
        iv_stride,
        iv_start,
    })
}

/// If the instruction at `pc` is an `iload` (narrow `iload`, the
/// `iload_0..3` short forms, or a `wide iload`), return the local slot
/// it reads. Otherwise return `None`.
fn iload_slot(bytes: &[u8], pc: usize, op: u8) -> Option<u16> {
    match op {
        0x15 => bytes.get(pc + 1).map(|&b| b as u16), // iload
        0x1A..=0x1D => Some((op - 0x1A) as u16),      // iload_0..iload_3
        0xC4 if bytes.get(pc + 1) == Some(&0x15) => {
            // wide iload
            Some(u16::from_be_bytes([
                *bytes.get(pc + 2)?,
                *bytes.get(pc + 3)?,
            ]))
        }
        _ => None,
    }
}

/// Scan the loop body for an `iinc` (narrow or wide) targeting
/// `iv_slot` and return its constant delta. `None` means no such
/// `iinc` exists. Errors only on malformed bytecode.
fn find_iv_stride(
    bytes: &[u8],
    header_pc: usize,
    back_branch_pc: usize,
    iv_slot: u16,
) -> Result<Option<i32>, LoweringError> {
    let mut pc = header_pc;
    while pc <= back_branch_pc {
        if pc >= bytes.len() {
            break;
        }
        let op = bytes[pc];
        let size = instr_size(bytes, pc)?;
        if op == 0x84 {
            // iinc index, const  (3 bytes)
            let slot = *bytes.get(pc + 1).ok_or_else(truncated)? as u16;
            if slot == iv_slot {
                let delta = *bytes.get(pc + 2).ok_or_else(truncated)? as i8 as i32;
                return Ok(Some(delta));
            }
        } else if op == 0xC4 && bytes.get(pc + 1) == Some(&0x84) {
            // wide iinc index, const  (6 bytes)
            let slot = u16::from_be_bytes([
                *bytes.get(pc + 2).ok_or_else(truncated)?,
                *bytes.get(pc + 3).ok_or_else(truncated)?,
            ]);
            if slot == iv_slot {
                let delta = i16::from_be_bytes([
                    *bytes.get(pc + 4).ok_or_else(truncated)?,
                    *bytes.get(pc + 5).ok_or_else(truncated)?,
                ]) as i32;
                return Ok(Some(delta));
            }
        }
        pc += size;
    }
    Ok(None)
}

/// Resolve the pre-loop region's (`0..header_pc`) constant initial
/// value for `iv_slot` — the `K` in `for (i = K; ...)`. The store to
/// `iv` immediately preceding the header must be fed by a compile-time
/// integer constant: `iconst_*`/`bipush`/`sipush`, or (AUDIT C31
/// follow-up, 2026-07-11) `ldc`/`ldc_w` of an `Integer` constant-pool
/// entry — needed once `K` falls outside `sipush`'s ±32767 range and
/// javac has no choice but to spill it to the constant pool.
///
/// Returns `Ok(K)` for a non-negative compile-time constant. Errors
/// (rejects the loop) if the start value is missing, is not a provable
/// compile-time constant, or is negative — see the module doc comment
/// for why a negative start is unsafe under this lowering even though
/// the register arithmetic itself would be fine.
fn resolve_start_value(
    bytes: &[u8],
    header_pc: usize,
    iv_slot: u16,
    cp: Option<&ConstantPool>,
) -> Result<i32, LoweringError> {
    // Walk the pre-loop, remembering the most recent constant pushed
    // and the most recent `istore` to `iv_slot`. The canonical prelude
    // ends `... <push K>; istore iv` right before the header.
    let mut pc = 0usize;
    // Most recent integer constant pushed onto the stack (if the last
    // instruction was a constant push) — `Some(value)` or `None`.
    let mut last_const: Option<i32> = None;
    // The constant feeding the most recent `istore iv`, if any.
    let mut iv_start: Option<i32> = None;
    let mut saw_iv_store = false;
    while pc < header_pc {
        if pc >= bytes.len() {
            break;
        }
        let op = bytes[pc];
        let size = instr_size(bytes, pc)?;
        if pc + size > bytes.len() {
            return Err(truncated());
        }
        // Constant pushes the canonical prelude can use for the start.
        let pushed = match op {
            0x02..=0x08 => Some(op as i32 - 0x03), // iconst_m1..iconst_5
            0x10 => Some(*bytes.get(pc + 1).ok_or_else(truncated)? as i8 as i32), // bipush
            0x11 => Some(i16::from_be_bytes([
                *bytes.get(pc + 1).ok_or_else(truncated)?,
                *bytes.get(pc + 2).ok_or_else(truncated)?,
            ]) as i32), // sipush
            0x12 => ldc_int_operand(cp, *bytes.get(pc + 1).ok_or_else(truncated)? as u16), // ldc
            0x13 => ldc_int_operand(
                cp,
                u16::from_be_bytes([
                    *bytes.get(pc + 1).ok_or_else(truncated)?,
                    *bytes.get(pc + 2).ok_or_else(truncated)?,
                ]),
            ), // ldc_w
            _ => None,
        };
        // Identify an `istore` and the slot it writes.
        let store_slot: Option<u16> = match op {
            0x36 => bytes.get(pc + 1).map(|&b| b as u16), // istore
            0x3B..=0x3E => Some((op - 0x3B) as u16),      // istore_0..3
            0xC4 if bytes.get(pc + 1) == Some(&0x36) => Some(u16::from_be_bytes([
                *bytes.get(pc + 2).ok_or_else(truncated)?,
                *bytes.get(pc + 3).ok_or_else(truncated)?,
            ])), // wide istore
            _ => None,
        };
        if let Some(slot) = store_slot {
            if slot == iv_slot {
                // The value stored into `iv` is whatever constant was
                // pushed immediately before. If the previous op was not
                // a provable constant push, `last_const` is `None` →
                // rejected below.
                iv_start = last_const;
                saw_iv_store = true;
            }
            last_const = None;
        } else {
            last_const = pushed;
        }
        pc += size;
    }

    if !saw_iv_store {
        return Err(LoweringError::UnsupportedNode(format!(
            "induction-variable slot {iv_slot} is never initialised by a \
             constant `istore` in the loop prelude — start value unknown"
        )));
    }
    match iv_start {
        Some(v) if v >= 0 => Ok(v),
        Some(v) => Err(LoweringError::UnsupportedNode(format!(
            "negative loop start value {v} for induction-variable slot \
             {iv_slot} — only `for (i = K; ...)` with a non-negative \
             compile-time-constant `K` is lowered; negative-start loops \
             run on the CPU (a negative start needs more kernel threads \
             than the host's bound-sized launch provides — see \
             loop_recog.rs's module doc comment)"
        ))),
        None => Err(LoweringError::UnsupportedNode(format!(
            "loop start value for induction-variable slot {iv_slot} is not a \
             compile-time constant — cannot prove it is non-negative; loop \
             runs on the CPU"
        ))),
    }
}

/// Resolve `ldc`/`ldc_w` constant-pool `index` to its `Integer` value,
/// if `cp` is available and the entry is in fact an `Integer`.
/// `None` (from either a missing pool, an out-of-range index, or a
/// non-`Integer` entry) is always safe here: the caller treats it
/// exactly like an unrecognised opcode — "not a provable constant" —
/// which falls back to rejecting the loop (CPU interpreter), never a
/// mis-lowering.
fn ldc_int_operand(cp: Option<&ConstantPool>, index: u16) -> Option<i32> {
    match cp?.get(index)? {
        ConstantPoolEntry::Integer(v) => Some(*v),
        _ => None,
    }
}

/// JVM instruction size, used here for the loop scanner. Mirrors the
/// table in `analyzer.rs` but lives separately to keep the modules
/// loosely coupled.
pub(crate) fn instr_size(bytes: &[u8], pc: usize) -> Result<usize, LoweringError> {
    let op = bytes[pc];
    Ok(match op {
        0x00..=0x0F
        | 0x1A..=0x35
        | 0x3B..=0x4E
        | 0x4F..=0x56
        | 0x57..=0x5F
        | 0x60..=0x83
        | 0x85..=0x93
        | 0x94..=0x98
        | 0xAC..=0xB1
        | 0xBE
        | 0xBF
        | 0xC2
        | 0xC3 => 1,
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3A | 0xA9 | 0xBC => 2,
        0x11
        | 0x13
        | 0x14
        | 0x84
        | 0x99..=0xA8
        | 0xB2..=0xB8
        | 0xBB
        | 0xBD
        | 0xC0
        | 0xC1
        | 0xC6
        | 0xC7 => 3,
        0xC5 => 4,
        0xB9 | 0xBA | 0xC8 | 0xC9 => 5,
        0xC4 => {
            let sub = *bytes
                .get(pc + 1)
                .ok_or_else(|| LoweringError::UnsupportedNode("truncated wide".into()))?;
            if sub == 0x84 {
                6
            } else {
                4
            }
        }
        0xAA | 0xAB => {
            // Switches: analyzer rejected, but be defensive.
            return Err(LoweringError::UnsupportedNode(
                "switch opcode not supported".into(),
            ));
        }
        _ => {
            return Err(LoweringError::UnsupportedNode(format!(
                "unknown opcode 0x{op:02x} during size walk"
            )))
        }
    })
}
