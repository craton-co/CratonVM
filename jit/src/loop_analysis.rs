// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Loop analysis scaffolding for LICM (Loop-Invariant Code Motion).
//!
//! Round-8 wave-3 HIGH fix (Fix 3): scaffold for generic LICM of
//! `getfield` / `getstatic` whose receiver is loop-invariant. This
//! module supplies the structural primitives (loop nest discovery and
//! a per-loop scan for invariant heap loads) so subsequent rounds can
//! wire hoisting into the x64 emitter without re-implementing the
//! analysis.
//!
//! The existing `LoopHoist` / `FpLoopHoist` records in `x64.rs` are
//! pattern-specific (aaload chains, scalar fp reloads). This module
//! is the generic foundation that future passes will share with
//! aarch64 and the IR optimizer.
//!
//! ## Status
//!
//! Analysis-only. Hoisting itself is intentionally deferred: real
//! hoisting requires (a) safepoint placement adjustment so the
//! hoisted load is observed at the loop-pre-header oop map and not
//! at every iteration, (b) a value-numbering pass to ensure the
//! receiver expression is itself invariant under aliasing
//! conservative assumptions, and (c) regalloc participation so the
//! hoisted value owns a frame slot for the loop's lifetime. Each of
//! those is its own subsystem.
//!
//! TODO(round-12+): wire hoisting consumer into `x64::compile_method`
//! and the IR optimizer.
//!
//! ## Lifetime and ownership (code-cache audit, 2026-08-01)
//!
//! Audited alongside `lib.rs` and `runtime_lowering.rs` for the shapes that
//! produce wild jumps, and it has none of them. Every value this module
//! produces or consumes is a *bytecode* quantity — `usize` bcis, `u16`
//! constant-pool indices, `u64` local-modification masks, `i32` constants —
//! and not one of them is a machine address, a `Box`/arena pointer handed to a
//! backend, or anything with a reclamation order. No function here takes or
//! returns a raw pointer. `BoundSource` is a recursively *boxed* expression
//! tree, which is the only pointer-shaped thing in the file — but every `Box`
//! is owned by the value that contains it and freed with it, and no address of
//! one is ever handed out, baked, or cached, so it carries none of the
//! `_jit_invoke_infos`-style "must outlive the code" obligation.
//!
//! The one lifetime-adjacent fact worth stating so nobody has to re-derive it:
//! this module's verdicts are consumed at COMPILE time, and what reaches the
//! emitted code is a set of immediates (trip counts, strides, guard constants).
//! Those are baked into the artifact's own buffer and are therefore exactly as
//! long-lived as it is — they name nothing outside it, so no invalidation,
//! redefinition or reclamation event can strand one. That is why this file
//! needs no `_direct_callee_entries`-style keep-alive list and no install-epoch
//! stamp, and why nothing here is reachable from `CompiledMethod::drop`.
//!
//! See `docs/jit/code-cache-lifetime.md`.

/// A natural loop discovered by scanning backward branches.
///
/// The set of body blocks is the closure under predecessor traversal
/// from `back_edges` back to `header_pc`. For the scaffold we store
/// the bytecode PC range conservatively as a half-open interval; a
/// future pass should refine to the precise reachable set when the
/// loop body contains forward exits to non-immediate-postdominators.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LoopInfo {
    /// Bytecode PC of the loop header — the target of every back edge.
    pub header_pc: usize,
    /// Bytecode PCs of back-edge instructions (`goto` / cond-branch
    /// whose target is `header_pc`). A reducible loop has at least
    /// one; an irreducible loop has multiple distinct headers and
    /// is currently *excluded* from detection (we treat irreducible
    /// CFGs as opaque and refuse to hoist).
    pub back_edges: Vec<usize>,
    /// Conservative body extent `[header_pc, body_end_pc)`. Includes
    /// the back-edge instruction. A future refinement may shrink
    /// this to exclude blocks that exit early (e.g. `return` from
    /// inside the loop).
    pub body_blocks: (usize, usize),
}

/// A getfield/getstatic candidate for hoisting out of a loop.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InvariantLoad {
    /// Bytecode PC of the loop header the load is invariant for.
    pub loop_header: usize,
    /// Bytecode PC of the `getfield` (0xb4) or `getstatic` (0xb2).
    pub load_pc: usize,
    /// Constant-pool index of the field reference (CONSTANT_Fieldref).
    pub cp_index: u16,
    /// For `getfield`: the local index whose value is the receiver
    /// (verified non-modified in the loop body). For `getstatic`:
    /// `None` — no receiver needed.
    pub receiver_local: Option<u16>,
}

/// Detect natural loops by scanning bytecode for backward branches.
///
/// Coalesces back edges that share a header into one `LoopInfo`.
/// Multiple back edges to the same header are tracked together (they
/// form a single loop in the standard SSA sense).
///
/// This is structurally similar to `x64::detect_loops` but groups by
/// header and records the body extent. The two implementations
/// should converge in a later round.
///
/// ANALYSIS-ONLY — NOT WIRED INTO CODEGEN. This pass is descriptive
/// scaffolding for the (deferred) generic LICM consumer; nothing in
/// the x64/aarch64 emitter currently calls it, and its output must
/// not be treated as a hoisting decision. See the module-level
/// "Status" doc for why hoisting is deferred. Do not assume the
/// returned `LoopInfo` body extent is exact: it is a conservative
/// half-open PC interval, not a precise reachable-block set.
pub fn detect_loops(code: &[u8], code_len: usize) -> Vec<LoopInfo> {
    use std::collections::BTreeMap;
    let mut by_header: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        let len = inst_len_at(code, pc);
        match op {
            // goto / conditional branches: signed i16 offset at pc+1..=pc+2.
            0xa7 | 0x99..=0xa6 | 0xc6 | 0xc7 => {
                if pc + 2 < code_len {
                    // JVM branch offsets are big-endian signed i16.
                    // Use `from_be_bytes` to avoid the debug overflow
                    // panic that `(byte_hi as i16) << 8` triggers when
                    // the high bit is set.
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target_opt = pc.checked_add_signed(offset as isize);
                    if let Some(target) = target_opt {
                        if target < code_len && target <= pc {
                            by_header.entry(target).or_default().push(pc);
                        }
                    }
                }
            }
            _ => {}
        }
        pc += len;
    }
    by_header
        .into_iter()
        .map(|(header, edges)| {
            let body_end = edges.iter().copied().max().map(|e| e + 3).unwrap_or(header);
            LoopInfo {
                header_pc: header,
                back_edges: edges,
                body_blocks: (header, body_end),
            }
        })
        .collect()
}

/// Identify `getfield` / `getstatic` invocations whose receiver
/// expression is loop-invariant in the given loop.
///
/// SCAFFOLD: this implementation finds candidate sites and conservatively
/// reports them. Receiver-locality is approximated by requiring the
/// receiver to be a single `aload` of a local that is NOT in the
/// modified-locals set of the loop body. The modified-locals set is
/// computed cheaply via a single bytecode scan.
///
/// ANALYSIS-ONLY — NOT WIRED INTO CODEGEN. Consumers must NOT yet act
/// on the returned list: the hoisting itself requires safepoint and
/// oop-map adjustments (see module "Status") not implemented here, and
/// no emitter path currently calls this function. It exists so future
/// hoisting passes can be developed and tested against detection that
/// is already validated, without re-implementing it. Treating the
/// returned candidates as "safe to hoist" today would be incorrect.
pub fn find_invariant_loads(loop_info: &LoopInfo, code: &[u8]) -> Vec<InvariantLoad> {
    let (start, end) = loop_info.body_blocks;
    if end > code.len() || start >= end {
        return Vec::new();
    }
    let modified = modified_locals_in_range(code, start, end);

    let mut out = Vec::new();
    let mut pc = start;
    let mut prev_op: Option<u8> = None;
    let mut prev_local: Option<u16> = None;
    while pc < end {
        let op = code[pc];
        let len = inst_len_at(code, pc);
        match op {
            // aload_0..aload_3 → local index = op - 0x2a
            0x2a..=0x2d => {
                prev_op = Some(op);
                prev_local = Some((op - 0x2a) as u16);
            }
            // aload <wide_index>
            0x19 if pc + 1 < end => {
                prev_op = Some(op);
                prev_local = Some(code[pc + 1] as u16);
            }
            // getstatic — no receiver; always invariant if the field
            // resolution itself is invariant (true unless the loop
            // contains class loading that could re-trigger
            // resolution, which we conservatively assume it doesn't
            // for the scaffold).
            0xb2 if pc + 2 < end => {
                let cp_index = ((code[pc + 1] as u16) << 8) | code[pc + 2] as u16;
                out.push(InvariantLoad {
                    loop_header: loop_info.header_pc,
                    load_pc: pc,
                    cp_index,
                    receiver_local: None,
                });
                prev_op = None;
                prev_local = None;
            }
            // getfield — invariant iff the preceding push was an
            // aload of an unmodified local.
            0xb4 if pc + 2 < end => {
                if let (Some(load_op), Some(local)) = (prev_op, prev_local) {
                    let is_aload = matches!(load_op, 0x19 | 0x2a..=0x2d);
                    let local_is_invariant =
                        (local as usize) < 64 && (modified & (1u64 << local)) == 0;
                    if is_aload && local_is_invariant {
                        let cp_index = ((code[pc + 1] as u16) << 8) | code[pc + 2] as u16;
                        out.push(InvariantLoad {
                            loop_header: loop_info.header_pc,
                            load_pc: pc,
                            cp_index,
                            receiver_local: Some(local),
                        });
                    }
                }
                prev_op = None;
                prev_local = None;
            }
            _ => {
                prev_op = Some(op);
                prev_local = None;
            }
        }
        pc += len;
    }
    out
}

/// Compute the set of locals (0..=63) written within the byte range.
/// Locals above 63 are conservatively reported as modified (we treat
/// any reference to them as a hoisting blocker).
fn modified_locals_in_range(code: &[u8], start: usize, end: usize) -> u64 {
    let mut modified = 0u64;
    let mut pc = start;
    while pc < end {
        match code[pc] {
            // istore/lstore/fstore/dstore/astore (wide index — 1 byte index)
            0x36..=0x3a if pc + 1 < end => {
                let local = code[pc + 1] as usize;
                if local < 64 {
                    modified |= 1u64 << local;
                }
            }
            // istore_0..istore_3
            0x3b..=0x3e => {
                modified |= 1u64 << (code[pc] - 0x3b);
            }
            // lstore_0..lstore_3
            0x3f..=0x42 => {
                modified |= 1u64 << (code[pc] - 0x3f);
            }
            // fstore_0..fstore_3
            0x43..=0x46 => {
                modified |= 1u64 << (code[pc] - 0x43);
            }
            // dstore_0..dstore_3
            0x47..=0x4a => {
                modified |= 1u64 << (code[pc] - 0x47);
            }
            // astore_0..astore_3
            0x4b..=0x4e => {
                modified |= 1u64 << (code[pc] - 0x4b);
            }
            // iinc <index> <const>
            0x84 if pc + 2 < end => {
                let local = code[pc + 1] as usize;
                if local < 64 {
                    modified |= 1u64 << local;
                }
            }
            _ => {}
        }
        pc += inst_len_at(code, pc);
    }
    modified
}

/// Bytecode instruction length in bytes for the opcode at `pc`.
///
/// Returns 1 for genuinely-unknown opcodes so the linear scanner
/// cannot underflow; this may slightly overcount loop bodies for
/// esoteric ops, which only loses hoisting opportunities (never
/// produces incorrect ones).
///
/// The variable-length `tableswitch`/`lookupswitch` instructions are
/// decoded precisely (padding + payload). Returning the wrong length
/// for them is a genuine correctness bug: any caller that advances
/// `pc += inst_len_at(..)` would land in the middle of the switch
/// operand bytes and decode padding / jump offsets as opcodes,
/// desyncing the whole scan for the remainder of the method.
fn inst_len_at(code: &[u8], pc: usize) -> usize {
    if pc >= code.len() {
        return 1;
    }
    match code[pc] {
        // 2-byte: bipush, ldc, [ifsda]load, [ifsda]store, newarray
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xbc => 2,
        // 3-byte: sipush, ldc_w/ldc2_w, branches, getfield/static,
        // putfield/static, invokestatic/special/virtual, new,
        // anewarray, checkcast, instanceof, iinc.
        0x11
        | 0x13
        | 0x14
        | 0x84
        | 0x99..=0xa6
        | 0xa7
        | 0xb2..=0xb8
        | 0xbb
        | 0xbd
        | 0xc0
        | 0xc1
        | 0xc6
        | 0xc7 => 3,
        // 5-byte: invokedynamic / invokeinterface / multianewarray
        0xb9 | 0xba => 5,
        0xc5 => 4,
        // goto_w / jsr_w: 1 opcode + 4-byte offset.
        0xc8 | 0xc9 => 5,
        // tableswitch (0xaa): 1 opcode byte, then 0..=3 padding bytes
        // aligning the next byte to a 4-byte boundary *relative to the
        // start of the code array* (the method's first bytecode is
        // offset 0), then defaultbyte (4) + low (4) + high (4), then
        // (high - low + 1) jump offsets of 4 bytes each.
        0xaa => {
            // First operand byte sits at the next 4-aligned offset.
            let base = (pc + 1 + 3) & !3;
            // Need low at base+4..base+8 and high at base+8..base+12.
            let high_end = base + 12;
            if high_end > code.len() {
                // Truncated/garbage — fall back to a 1-byte step so
                // the scan terminates safely rather than reading OOB.
                return 1;
            }
            let low = i32::from_be_bytes([
                code[base + 4],
                code[base + 5],
                code[base + 6],
                code[base + 7],
            ]);
            let high = i32::from_be_bytes([
                code[base + 8],
                code[base + 9],
                code[base + 10],
                code[base + 11],
            ]);
            // n = high - low + 1 entries. Guard against malformed
            // (high < low) tables that would make n negative.
            let n = (high as i64) - (low as i64) + 1;
            if n < 0 {
                return 1;
            }
            // total = (base - pc) header skip + 12 (default/low/high)
            //         + n * 4 jump offsets.
            (base - pc) + 12 + (n as usize) * 4
        }
        // lookupswitch (0xab): 1 opcode byte, then 0..=3 padding bytes
        // aligning to a 4-byte boundary (same rule as tableswitch),
        // then defaultbyte (4) + npairs (4), then npairs match/offset
        // pairs of 8 bytes each.
        0xab => {
            let base = (pc + 1 + 3) & !3;
            // Need npairs at base+4..base+8.
            let npairs_end = base + 8;
            if npairs_end > code.len() {
                return 1;
            }
            let npairs = i32::from_be_bytes([
                code[base + 4],
                code[base + 5],
                code[base + 6],
                code[base + 7],
            ]);
            if npairs < 0 {
                return 1;
            }
            // total = (base - pc) header skip + 8 (default/npairs)
            //         + npairs * 8 (match/offset pairs).
            (base - pc) + 8 + (npairs as usize) * 8
        }
        // wide-prefixed instruction; nominal 4 bytes for most
        // (3-byte payload follows), 6 for iinc.
        0xc4 => {
            if pc + 1 < code.len() && code[pc + 1] == 0x84 {
                6
            } else {
                4
            }
        }
        // All other opcodes are 1 byte (arithmetic, stack manip,
        // returns, monitor, etc.).
        _ => 1,
    }
}

// ===========================================================================
// Counted-loop recognition for the range analysis (deep-research report P1)
//
// `scev.rs` owns the *proof*: the range lattice, the affine IV, the overflow
// model and the pre-header obligations. This half owns the *recognition* — it
// turns bytecode into the `scev::CountedLoop` claim the proof consumes, and it
// is where the "bound is not `array.length`" restriction actually dies:
// [`decode_bound_expr`] accepts a constant, a local, an `arraylength`, a
// `getfield`/`getstatic`, and a `Math.min`/`Math.max` of any of those.
//
// Nothing here is wired into codegen. Like the rest of this module it is
// analysis-only; see the module "Status" note.
// ===========================================================================

use crate::scev::{AffineIv, BoundSource, CountedLoop, ExitCmp, IntRange, LoopForm, Stride};

/// Which `Math` reduction an `invokestatic` inside a bound expression is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinMax {
    /// `Math.min(int, int)`.
    Min,
    /// `Math.max(int, int)`.
    Max,
}

/// The local index an `iload` at `pc` reads, if the opcode is one.
fn iload_local(code: &[u8], pc: usize, end: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x1a..=0x1d => Some((code[pc] - 0x1a) as usize),
        0x15 if pc + 1 < end => Some(code[pc + 1] as usize),
        _ => None,
    }
}

/// The byte length of an `iload` at `pc` (2 for the wide-index form).
fn iload_len(code: &[u8], pc: usize) -> usize {
    if code.get(pc) == Some(&0x15) {
        2
    } else {
        1
    }
}

/// The value an integer constant push at `pc` produces.
fn const_push_value(code: &[u8], pc: usize, end: usize) -> Option<i32> {
    match *code.get(pc)? {
        // iconst_m1 .. iconst_5
        0x02..=0x08 => Some(code[pc] as i32 - 3),
        0x10 if pc + 1 < end => Some(code[pc + 1] as i8 as i32),
        0x11 if pc + 2 < end => Some(i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32),
        _ => None,
    }
}

/// Decode the operand expression a loop's exit test compares the induction
/// variable against, starting at `pc`. Returns the expression and the PC just
/// past it.
///
/// Accepted shapes — each is a limit the range proof can use, and only the
/// third of them is what `x64/bce.rs` currently recognises:
///
/// * `iconst_*` / `bipush` / `sipush`      → [`BoundSource::Const`]
/// * `iload n`                             → [`BoundSource::Local`]
/// * `aload a ; arraylength`               → [`BoundSource::ArrayLength`]
/// * `aload a ; getfield f` / `getstatic f`→ [`BoundSource::Field`]
/// * `<a> <b> invokestatic Math.min|max`   → [`BoundSource::Min`] / [`BoundSource::Max`]
///
/// `resolve` maps an `invokestatic` constant-pool index to `Some(MinMax)` iff
/// it is the `int` overload of `Math.min`/`Math.max`; this module carries no
/// constant pool of its own, and a resolver that answers `None` simply loses
/// the min/max shape rather than mis-decoding it.
pub fn decode_bound_expr(
    code: &[u8],
    pc: usize,
    end: usize,
    resolve: &dyn Fn(u16) -> Option<MinMax>,
) -> Option<(BoundSource, usize)> {
    decode_expr(code, pc, end, resolve, 0)
}

/// Recursion depth cap for [`decode_bound_expr`]. Nested `Math.min` chains
/// beyond this simply stop being recognised — a bounded compile-time cost with
/// no soundness content.
const BOUND_EXPR_MAX_DEPTH: u32 = 4;

fn decode_expr(
    code: &[u8],
    pc: usize,
    end: usize,
    resolve: &dyn Fn(u16) -> Option<MinMax>,
    depth: u32,
) -> Option<(BoundSource, usize)> {
    if depth > BOUND_EXPR_MAX_DEPTH {
        return None;
    }
    let (a, p) = decode_atom(code, pc, end)?;
    // Postfix lookahead for the `min`/`max` combinator: `<a> <b> invokestatic`.
    if let Some((b, q)) = decode_expr(code, p, end, resolve, depth + 1) {
        if q + 2 < end && code[q] == 0xb8 {
            let cp = ((code[q + 1] as u16) << 8) | code[q + 2] as u16;
            if let Some(kind) = resolve(cp) {
                let combined = match kind {
                    MinMax::Min => BoundSource::Min(Box::new(a), Box::new(b)),
                    MinMax::Max => BoundSource::Max(Box::new(a), Box::new(b)),
                };
                return Some((combined, q + 3));
            }
        }
    }
    Some((a, p))
}

fn decode_atom(code: &[u8], pc: usize, end: usize) -> Option<(BoundSource, usize)> {
    if pc >= end || pc >= code.len() {
        return None;
    }
    let op = code[pc];
    match op {
        0x02..=0x08 => Some((BoundSource::Const(op as i32 - 3), pc + 1)),
        0x10 if pc + 1 < end => Some((BoundSource::Const(code[pc + 1] as i8 as i32), pc + 2)),
        0x11 if pc + 2 < end => Some((
            BoundSource::Const(i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32),
            pc + 3,
        )),
        0x1a..=0x1d => Some((BoundSource::Local((op - 0x1a) as usize), pc + 1)),
        0x15 if pc + 1 < end => Some((BoundSource::Local(code[pc + 1] as usize), pc + 2)),
        // getstatic
        0xb2 if pc + 2 < end => Some((
            BoundSource::Field {
                cp_index: ((code[pc + 1] as u16) << 8) | code[pc + 2] as u16,
                receiver_local: None,
            },
            pc + 3,
        )),
        // aload, then either `arraylength` or `getfield`.
        0x19 | 0x2a..=0x2d => {
            let (recv, after) = if op == 0x19 {
                if pc + 1 >= end {
                    return None;
                }
                (code[pc + 1] as usize, pc + 2)
            } else {
                ((op - 0x2a) as usize, pc + 1)
            };
            if after < end && code[after] == 0xbe {
                return Some((BoundSource::ArrayLength(recv), after + 1));
            }
            if after + 2 < end && code[after] == 0xb4 {
                return Some((
                    BoundSource::Field {
                        cp_index: ((code[after + 1] as u16) << 8) | code[after + 2] as u16,
                        receiver_local: Some(recv),
                    },
                    after + 3,
                ));
            }
            None
        }
        _ => None,
    }
}

/// Prove how `iv` advances inside `[start, end)`, generalised past
/// `x64/bce.rs`'s `iinc +1` / `iv += <local>` pair to any constant stride
/// (non-unit, negative, `iinc` or `iadd`/`isub`) as well as the runtime step.
///
/// Returns `None` — refuse the loop — when the body modifies `iv` zero times,
/// more than once, or in any shape not listed below, including every
/// `wide`-indexed alias:
///
/// * `iinc iv, c`                              → `Stride::Const(c)`
/// * `iload iv ; <const> ; iadd ; istore iv`   → `Stride::Const(c)`
/// * `iload iv ; <const> ; isub ; istore iv`   → `Stride::Const(-c)`
/// * `iload iv ; iload s ; iadd ; istore iv`   → `Stride::Variable(s)`
pub fn find_iv_stride(code: &[u8], start: usize, end: usize, iv: usize) -> Option<Stride> {
    let mut found: Option<Stride> = None;
    let mut count = 0usize;
    // PCs of the previous three instruction starts, most recent first.
    let mut prev: [Option<usize>; 3] = [None, None, None];
    let mut pc = start;
    while pc < end {
        let op = code[pc];
        match op {
            0x84 => {
                if pc + 2 >= end {
                    return None; // truncated iinc: unreadable, so unprovable
                }
                if code[pc + 1] as usize == iv {
                    count += 1;
                    found = Some(Stride::Const(code[pc + 2] as i8 as i32));
                }
            }
            // Any wide-indexed store or iinc that could alias the IV is
            // unprovable; refuse rather than assume it targets another slot.
            0xc4 => {
                if pc + 3 >= end {
                    return None;
                }
                let real = code[pc + 1];
                let idx = ((code[pc + 2] as usize) << 8) | code[pc + 3] as usize;
                if matches!(real, 0x36..=0x3a | 0x84)
                    && (idx == iv || (matches!(real, 0x37 | 0x39) && idx + 1 == iv))
                {
                    return None;
                }
            }
            _ => {
                // The JVM lets one slot hold different kinds across disjoint
                // live ranges. A non-`int` store to the IV's slot — or the dead
                // high half of a `long`/`double` store below it — changes the
                // IV outside the recorded stride, so refuse.
                let clobbers_iv = match op {
                    0x37..=0x3a => {
                        if pc + 1 >= end {
                            return None;
                        }
                        let s = code[pc + 1] as usize;
                        s == iv || (matches!(op, 0x37 | 0x39) && s + 1 == iv)
                    }
                    0x3f..=0x42 => {
                        let s = (op - 0x3f) as usize;
                        s == iv || s + 1 == iv
                    }
                    0x43..=0x46 => (op - 0x43) as usize == iv,
                    0x47..=0x4a => {
                        let s = (op - 0x47) as usize;
                        s == iv || s + 1 == iv
                    }
                    0x4b..=0x4e => (op - 0x4b) as usize == iv,
                    _ => false,
                };
                if clobbers_iv {
                    return None;
                }
                let istore_target = match op {
                    0x36 => {
                        if pc + 1 >= end {
                            return None;
                        }
                        Some(code[pc + 1] as usize)
                    }
                    0x3b..=0x3e => Some((op - 0x3b) as usize),
                    _ => None,
                };
                if istore_target == Some(iv) {
                    count += 1;
                    let (Some(p1), Some(p2), Some(p3)) = (prev[0], prev[1], prev[2]) else {
                        return None;
                    };
                    // `p1` is the arithmetic, `p2` the step operand, `p3` the
                    // `iload iv`. The commuted form `step + iv` is refused: it
                    // is indistinguishable here from `iv` being overwritten by
                    // an unrelated expression.
                    if iload_local(code, p3, end) != Some(iv) {
                        return None;
                    }
                    let step = match (code[p1], const_push_value(code, p2, end)) {
                        (0x60, Some(c)) => Stride::Const(c),
                        (0x64, Some(c)) => Stride::Const(c.checked_neg()?),
                        (0x60, None) => match iload_local(code, p2, end) {
                            Some(s) if s != iv => Stride::Variable(s),
                            _ => return None,
                        },
                        _ => return None,
                    };
                    found = Some(step);
                }
            }
        }
        prev = [Some(pc), prev[0], prev[1]];
        pc += inst_len_at(code, pc);
    }
    if count == 1 {
        found
    } else {
        None
    }
}

/// Every local written in `[start, end)`, or `None` when the range writes a
/// local the `u64` set cannot represent (slot `>= 64`).
///
/// Unlike the private `modified_locals_in_range` this refuses rather than
/// silently dropping such a store — an invariance check built on a set that
/// quietly forgot a write is not a check at all. `long`/`double` stores also
/// mark the dead high half.
pub fn modified_locals_strict(code: &[u8], start: usize, end: usize) -> Option<u64> {
    let mut m = 0u64;
    let mark = |slot: usize, wide: bool, m: &mut u64| -> bool {
        if slot >= 64 || (wide && slot + 1 >= 64) {
            return false;
        }
        *m |= 1u64 << slot;
        if wide {
            *m |= 1u64 << (slot + 1);
        }
        true
    };
    let mut pc = start;
    while pc < end {
        let op = code[pc];
        // A store whose operand bytes fall outside the range is a store this
        // scan cannot read — refuse rather than drop it from the set.
        let hit = match op {
            // istore/lstore/fstore/dstore/astore, wide index byte.
            0x36..=0x3a => {
                if pc + 1 >= end {
                    return None;
                }
                Some((code[pc + 1] as usize, matches!(op, 0x37 | 0x39)))
            }
            0x3b..=0x3e => Some(((op - 0x3b) as usize, false)),
            0x3f..=0x42 => Some(((op - 0x3f) as usize, true)),
            0x43..=0x46 => Some(((op - 0x43) as usize, false)),
            0x47..=0x4a => Some(((op - 0x47) as usize, true)),
            0x4b..=0x4e => Some(((op - 0x4b) as usize, false)),
            0x84 => {
                if pc + 2 >= end {
                    return None;
                }
                Some((code[pc + 1] as usize, false))
            }
            0xc4 => {
                if pc + 3 >= end {
                    return None;
                }
                let real = code[pc + 1];
                let idx = ((code[pc + 2] as usize) << 8) | code[pc + 3] as usize;
                if matches!(real, 0x36..=0x3a | 0x84) {
                    Some((idx, matches!(real, 0x37 | 0x39)))
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some((slot, wide)) = hit {
            if !mark(slot, wide, &mut m) {
                return None;
            }
        }
        pc += inst_len_at(code, pc);
    }
    Some(m)
}

/// Whether `[start, end)` provably performs no operation that could change a
/// field a loop bound reads. Deliberately blunt: any store to the heap, any
/// call, any allocation (which can run a `<clinit>`) and any monitor operation
/// answers `false`.
pub fn body_is_heap_stable(code: &[u8], start: usize, end: usize) -> bool {
    let mut pc = start;
    while pc < end {
        match code[pc] {
            // putstatic / putfield
            0xb3 | 0xb5 => return false,
            // invokevirtual .. invokedynamic
            0xb6..=0xba => return false,
            // new / newarray / anewarray / multianewarray
            0xbb..=0xbd | 0xc5 => return false,
            // monitorenter / monitorexit
            0xc2 | 0xc3 => return false,
            _ => {}
        }
        pc += inst_len_at(code, pc);
    }
    true
}

/// Every `i16`-offset branch target in the method, or `None` when the method
/// contains a branch form this scan does not model (`tableswitch`,
/// `lookupswitch`, `jsr`/`ret`, `goto_w`/`jsr_w`).
fn branch_targets(code: &[u8], code_len: usize) -> Option<Vec<usize>> {
    let mut targets = Vec::new();
    let mut pc = 0usize;
    while pc < code_len {
        match code[pc] {
            0xaa | 0xab | 0xa8 | 0xa9 | 0xc8 | 0xc9 => return None,
            op if matches!(op, 0x99..=0xa7 | 0xc6 | 0xc7) && pc + 2 < code_len => {
                let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                let target = pc as i32 + off;
                if target >= 0 && (target as usize) < code_len {
                    targets.push(target as usize);
                }
            }
            _ => {}
        }
        pc += inst_len_at(code, pc);
    }
    Some(targets)
}

/// The constant the induction variable holds on entry to the loop, when the
/// method makes that provable.
///
/// Requires exactly one store to `iv` outside the loop body, before the
/// header, storing a constant push, with no branch landing on the store or the
/// push, and no `iinc` or `wide`-indexed write to `iv` outside the body. JVM
/// definite assignment then makes that single store dominate the header, so no
/// dominator tree is needed.
///
/// `None` is not "zero" — it is "unknown", and the range proof falls back to a
/// pre-header guard on the local.
pub fn constant_iv_init(
    code: &[u8],
    code_len: usize,
    header_pc: usize,
    body_end: usize,
    iv: usize,
) -> Option<i32> {
    let targets = branch_targets(code, code_len)?;
    let mut store: Option<(usize, Option<usize>)> = None;
    let mut count = 0usize;
    let mut prev: Option<usize> = None;
    let mut pc = 0usize;
    while pc < code_len {
        let op = code[pc];
        let in_body = pc >= header_pc && pc < body_end;
        if op == 0x84 && pc + 2 < code_len && code[pc + 1] as usize == iv && !in_body {
            return None; // an out-of-body iinc moves the entry value
        }
        if op == 0xc4 && pc + 3 < code_len {
            let real = code[pc + 1];
            let idx = ((code[pc + 2] as usize) << 8) | code[pc + 3] as usize;
            if matches!(real, 0x36 | 0x84) && idx == iv {
                return None;
            }
        }
        let istore_target = match op {
            0x36 if pc + 1 < code_len => Some(code[pc + 1] as usize),
            0x3b..=0x3e => Some((op - 0x3b) as usize),
            _ => None,
        };
        if istore_target == Some(iv) && !in_body {
            count += 1;
            store = Some((pc, prev));
        }
        prev = Some(pc);
        pc += inst_len_at(code, pc);
    }
    let (store_pc, push_pc) = match (count, store) {
        (1, Some((s, Some(p)))) => (s, p),
        _ => return None,
    };
    if store_pc >= header_pc {
        return None; // does not dominate the loop
    }
    if targets.contains(&store_pc) || targets.contains(&push_pc) {
        return None;
    }
    // Dominating the header once is not the same as running before EVERY entry
    // to it. An enclosing loop whose back edge lands after the store and at or
    // before this header re-enters the loop without re-running the init:
    // `int i = 0; for (r < R) { for (; i < 16; i++) a[i] += r; }` credited the
    // inner loop a constant init of 0, and so 16 trips, on every outer pass,
    // though it runs none after the first. Any backward branch into
    // `(store_pc, header_pc]` refuses.
    {
        let mut q = 0usize;
        while q < code_len {
            let op = code[q];
            let rel = match op {
                0x99..=0xa7 | 0xc6 | 0xc7 if q + 2 < code_len => {
                    Some(i32::from(i16::from_be_bytes([code[q + 1], code[q + 2]])))
                }
                0xc8 if q + 4 < code_len => Some(i32::from_be_bytes([
                    code[q + 1],
                    code[q + 2],
                    code[q + 3],
                    code[q + 4],
                ])),
                _ => None,
            };
            if let Some(rel) = rel {
                // Cast: bytecode offsets fit i64.
                let target = q as i64 + i64::from(rel);
                // The loop's own back edge and `continue`s sit in the body.
                let in_body = q >= header_pc && q < body_end;
                if !in_body && rel < 0 && target > store_pc as i64 && target <= header_pc as i64 {
                    return None;
                }
            }
            q += inst_len_at(code, q);
        }
    }
    const_push_value(code, push_pc, code_len)
}

/// Whether `[header_pc, end)` can leave the loop anywhere other than the exit
/// test at `exit_pc` and the back edge at `back_edge_pc`.
///
/// Any leaving branch or switch arm, return, `athrow` or subroutine jump
/// counts. A switch whose table cannot be decoded is answered `true`: the
/// question only ever weakens a trip-count lower bound, so the conservative
/// answer is the one that claims an exit.
fn loop_has_other_exit(
    code: &[u8],
    header_pc: usize,
    end: usize,
    exit_pc: usize,
    back_edge_pc: usize,
) -> bool {
    let outside = |pc: usize, rel: i64| {
        let target = pc as i64 + rel;
        target < header_pc as i64 || target >= end as i64
    };
    let read_i32 = |at: usize| -> Option<i64> {
        let b = code.get(at..at + 4)?;
        Some(i64::from(i32::from_be_bytes([b[0], b[1], b[2], b[3]])))
    };
    let mut pc = header_pc;
    while pc < end {
        let op = code[pc];
        if pc != exit_pc && pc != back_edge_pc {
            match op {
                0x99..=0xa7 | 0xc6 | 0xc7 => {
                    let Some(b) = code.get(pc + 1..pc + 3) else {
                        return true;
                    };
                    if outside(pc, i64::from(i16::from_be_bytes([b[0], b[1]]))) {
                        return true;
                    }
                }
                0xc8 => match read_i32(pc + 1) {
                    Some(rel) if !outside(pc, rel) => {}
                    _ => return true,
                },
                0xaa | 0xab => {
                    let base = pc + 1 + (4 - (pc + 1) % 4) % 4;
                    let Some(default) = read_i32(base) else {
                        return true;
                    };
                    if outside(pc, default) {
                        return true;
                    }
                    let offsets: Vec<usize> = if op == 0xaa {
                        let (Some(lo), Some(hi)) = (read_i32(base + 4), read_i32(base + 8)) else {
                            return true;
                        };
                        if hi < lo || (hi - lo) as usize >= code.len() {
                            return true;
                        }
                        (0..(hi - lo + 1) as usize)
                            .map(|i| base + 12 + 4 * i)
                            .collect()
                    } else {
                        let Some(n) = read_i32(base + 4) else {
                            return true;
                        };
                        if n < 0 || n as usize > code.len() {
                            return true;
                        }
                        (0..n as usize).map(|i| base + 12 + 8 * i).collect()
                    };
                    for at in offsets {
                        match read_i32(at) {
                            Some(rel) if !outside(pc, rel) => {}
                            _ => return true,
                        }
                    }
                }
                0xa8 | 0xa9 | 0xc9 | 0xac..=0xb1 | 0xbf => return true,
                _ => {}
            }
        }
        pc += inst_len_at(code, pc);
    }
    false
}

/// Recognise `(header_pc, back_edge_pc)` as a counted loop the range analysis
/// can reason about.
///
/// `form` is the caller's assertion about whether the exit test dominates the
/// body; it cannot be derived from the `(header, back_edge)` pair alone
/// (javac's `goto cond` rotation makes a back-branching test pre-tested), so it
/// is an input, and [`LoopForm::PostTested`] is the conservative answer.
///
/// Returns `None` whenever any ingredient is unprovable — an unrepresentable
/// local write, an unrecognised step shape, or no exit test on the induction
/// variable.
pub fn analyze_counted_loop(
    code: &[u8],
    code_len: usize,
    header_pc: usize,
    back_edge_pc: usize,
    form: LoopForm,
    resolve: &dyn Fn(u16) -> Option<MinMax>,
) -> Option<CountedLoop> {
    analyze_counted_loop_at(code, code_len, header_pc, back_edge_pc, form, resolve).map(|(l, _)| l)
}

/// [`analyze_counted_loop`], also reporting the PC of the exit test it
/// recognised.
///
/// The PC is not a diagnostic. `form` is the caller's *assertion* that the test
/// dominates the body, and it cannot be derived from `(header, back_edge)`
/// alone — but it can be CHECKED once the test has been found: a test at the
/// header runs before every body execution, because the header dominates the
/// region and the back edge targets the header. A caller that asserts
/// [`LoopForm::PreTested`] and then finds the test somewhere in the middle of
/// the body has asserted something false, and every trip count derived from it
/// is one too many. Returning the PC is what lets that caller fail closed
/// instead of trusting its own guess.
pub fn analyze_counted_loop_at(
    code: &[u8],
    code_len: usize,
    header_pc: usize,
    back_edge_pc: usize,
    form: LoopForm,
    resolve: &dyn Fn(u16) -> Option<MinMax>,
) -> Option<(CountedLoop, usize)> {
    if header_pc >= code_len || back_edge_pc >= code_len {
        return None;
    }
    let end = (back_edge_pc + inst_len_at(code, back_edge_pc)).min(code_len);
    if header_pc >= end {
        return None;
    }
    let modified_locals = modified_locals_strict(code, header_pc, end)?;
    let heap_stable = body_is_heap_stable(code, header_pc, end);

    let mut pc = header_pc;
    while pc < end {
        if let Some(candidate) = iload_local(code, pc, end) {
            let after = pc + iload_len(code, pc);
            if let Some((bound, q)) = decode_bound_expr(code, after, end, resolve) {
                if q < end {
                    // The branch must actually be able to LEAVE the loop.
                    //
                    // Without this the first `iload x; <limit>; if_icmp*` triple
                    // in the body wins, so an ordinary in-body
                    // `if (i >= limit) { … }` is read as the loop's exit
                    // condition — and `iv_span` then claims every executed
                    // iteration satisfies `i < limit`, which is unsound for
                    // every consumer that trusts the span.
                    //
                    // One edge must leave, not specifically the taken one: a
                    // pre-tested loop exits on the taken edge, while the
                    // rotated `if_icmplt`-continue shape exits on the
                    // fall-through and branches backward into the body. An
                    // in-body test has both edges inside.
                    let leaves_loop = if q + 2 < code.len() {
                        let off = i16::from_be_bytes([code[q + 1], code[q + 2]]) as i32;
                        let taken = q as i32 + off;
                        let fallthrough = (q + 3) as i32;
                        let outside = |t: i32| t < header_pc as i32 || t >= end as i32;
                        outside(taken) || outside(fallthrough)
                    } else {
                        false
                    };
                    if !leaves_loop {
                        pc += iload_len(code, pc);
                        continue;
                    }
                    if let Some(cmp) = ExitCmp::from_opcode(code[q]) {
                        if let Some(stride) = find_iv_stride(code, header_pc, end, candidate) {
                            let init = constant_iv_init(code, code_len, header_pc, end, candidate)
                                .map(IntRange::constant)
                                .unwrap_or_else(IntRange::unknown);
                            return Some((
                                CountedLoop {
                                    header_pc,
                                    back_edge_pc,
                                    iv: AffineIv {
                                        local: candidate,
                                        init,
                                        stride,
                                    },
                                    cmp,
                                    bound,
                                    bound_range: IntRange::unknown(),
                                    form,
                                    modified_locals,
                                    heap_stable,
                                    has_other_exit: loop_has_other_exit(
                                        code,
                                        header_pc,
                                        end,
                                        q,
                                        back_edge_pc,
                                    ),
                                },
                                pc,
                            ));
                        }
                    }
                }
            }
        }
        pc += inst_len_at(code, pc);
    }
    None
}

/// For each loop, the index of its innermost enclosing loop — the nesting an
/// inner bound that mentions the outer induction variable needs.
///
/// Containment is decided on the conservative body extents [`LoopInfo`]
/// records, so a loop is reported as nested only when its whole extent lies
/// inside another's.
pub fn loop_parents(loops: &[LoopInfo]) -> Vec<Option<usize>> {
    let mut out = vec![None; loops.len()];
    for (i, li) in loops.iter().enumerate() {
        let (s, e) = li.body_blocks;
        let mut best: Option<usize> = None;
        for (j, lj) in loops.iter().enumerate() {
            if i == j {
                continue;
            }
            let (js, je) = lj.body_blocks;
            if js <= s && e <= je && (js, je) != (s, e) {
                let better = match best {
                    None => true,
                    Some(b) => {
                        let (bs, be) = loops[b].body_blocks;
                        js >= bs && je <= be
                    }
                };
                if better {
                    best = Some(j);
                }
            }
        }
        out[i] = best;
    }
    out
}

// ---------------------------------------------------------------------------
// Tests — verify the analysis identifies loops and invariants the
// expected way. Hoisting consumer tests live with the (future) emitter.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_simple_backward_goto_as_loop() {
        // PC 0: iconst_0
        // PC 1: istore_1
        // PC 2: iload_1   ; (loop header)
        // PC 3: iconst_5
        // PC 4: if_icmpge +6  → exits at PC 10
        // PC 7: iinc 1, 1
        // PC 10: ireturn  (note: the if jumps here, the body ends sooner)
        // -- but we need a back edge; reshape:
        //
        // PC 0:  iconst_0   ; (no-op header context)
        // PC 1:  istore_1
        // PC 2:  iload_1                ; HEADER
        // PC 3:  bipush 10              ; 2 bytes
        // PC 5:  if_icmpge +8           ; 3 bytes (exit, forward)
        // PC 8:  iinc 1, 1              ; 3 bytes
        // PC 11: goto -9                ; 3 bytes → target PC 2
        // PC 14: return
        let code: Vec<u8> = vec![
            0x03, 0x3c, 0x1b, 0x10, 0x0a, 0xa2, 0x00, 0x08, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
            0xb1,
        ];
        let loops = detect_loops(&code, code.len());
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].header_pc, 2);
        assert_eq!(loops[0].back_edges, vec![11]);
    }

    #[test]
    fn modified_locals_finds_iinc_and_stores() {
        // istore_1 ; iinc 2, 1 ; istore_3 ; nop
        let code: Vec<u8> = vec![0x3c, 0x84, 0x02, 0x01, 0x3e, 0x00];
        let m = modified_locals_in_range(&code, 0, code.len());
        // bits 1, 2, 3 set
        assert_eq!(m, 0b1110);
    }

    #[test]
    fn invariant_load_finds_aload_then_getstatic_or_getfield() {
        // PC 0: aload_0
        // PC 1: getfield #0x0001
        // PC 4: pop
        // PC 5: getstatic #0x0002
        // PC 8: pop
        let code: Vec<u8> = vec![0x2a, 0xb4, 0x00, 0x01, 0x57, 0xb2, 0x00, 0x02, 0x57];
        let li = LoopInfo {
            header_pc: 0,
            back_edges: vec![],
            body_blocks: (0, code.len()),
        };
        let v = find_invariant_loads(&li, &code);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].load_pc, 1);
        assert_eq!(v[0].cp_index, 1);
        assert_eq!(v[0].receiver_local, Some(0));
        assert_eq!(v[1].load_pc, 5);
        assert_eq!(v[1].cp_index, 2);
        assert_eq!(v[1].receiver_local, None);
    }

    #[test]
    fn inst_len_tableswitch_padded_and_jumptable() {
        // Layout (PC shown). tableswitch must align its first operand
        // byte to a 4-byte boundary relative to code start.
        //
        // PC 0: nop                     ; force pc=1 for the switch so
        //                               ; padding is non-trivial.
        // PC 1: tableswitch (0xaa)
        //   pad PC 2,3 (operand byte aligns to PC 4)
        //   PC 4..8:   default = 0
        //   PC 8..12:  low     = 0
        //   PC 12..16: high    = 2   → n = high-low+1 = 3 jump offsets
        //   PC 16..20: jump[0]
        //   PC 20..24: jump[1]
        //   PC 24..28: jump[2]
        // operand begins at (1+1+3)&!3 = 4, so padding = PC 2,3 (2 bytes).
        // → instruction spans PC 1..28, i.e. length 27.
        let mut code: Vec<u8> = Vec::new();
        code.push(0x00); // PC 0: nop
        code.push(0xaa); // PC 1: tableswitch opcode
                         // operand begins at (1+1+3)&!3 = 4, so padding = PC 2,3 (2 bytes)
        code.push(0x00); // PC 2 padding
        code.push(0x00); // PC 3 padding
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 4..8 default
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 8..12 low
        code.extend_from_slice(&2i32.to_be_bytes()); // PC 12..16 high
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 16..20 jump[0]
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 20..24 jump[1]
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 24..28 jump[2]
        assert_eq!(code.len(), 28);
        // Length of the tableswitch at PC 1 must carry the scanner to
        // PC 28 (end of code): 28 - 1 = 27.
        assert_eq!(inst_len_at(&code, 1), 27);
        // And a linear scan from PC 0 lands exactly on the end, never
        // mis-decoding a padding/offset byte as an opcode.
        let mut pc = 0;
        let mut steps = 0;
        while pc < code.len() {
            pc += inst_len_at(&code, pc);
            steps += 1;
            assert!(steps < 100, "scan failed to terminate (desync)");
        }
        assert_eq!(pc, code.len());
        assert_eq!(steps, 2); // nop, then tableswitch
    }

    #[test]
    fn inst_len_lookupswitch_padded_and_pairs() {
        // PC 0: lookupswitch (0xab)
        //   operand at (0+1+3)&!3 = 4, so 3 padding bytes at PC 1,2,3
        //   PC 4..8:   default = 0
        //   PC 8..12:  npairs  = 2
        //   PC 12..20: pair[0] (match,offset)
        //   PC 20..28: pair[1] (match,offset)
        // → spans PC 0..28, length 28.
        let mut code: Vec<u8> = Vec::new();
        code.push(0xab); // PC 0: lookupswitch opcode
        code.push(0x00); // PC 1 padding
        code.push(0x00); // PC 2 padding
        code.push(0x00); // PC 3 padding
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 4..8 default
        code.extend_from_slice(&2i32.to_be_bytes()); // PC 8..12 npairs
        code.extend_from_slice(&1i32.to_be_bytes()); // PC 12..16 match[0]
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 16..20 offset[0]
        code.extend_from_slice(&2i32.to_be_bytes()); // PC 20..24 match[1]
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 24..28 offset[1]
        assert_eq!(code.len(), 28);
        assert_eq!(inst_len_at(&code, 0), 28);
    }

    #[test]
    fn inst_len_truncated_switch_is_safe() {
        // A bare tableswitch opcode with no room for the header must
        // not read out of bounds; it falls back to a 1-byte step.
        let code: Vec<u8> = vec![0xaa, 0x00, 0x00];
        assert_eq!(inst_len_at(&code, 0), 1);
        // Likewise a malformed lookupswitch with negative npairs.
        let mut bad: Vec<u8> = vec![0xab, 0x00, 0x00, 0x00];
        bad.extend_from_slice(&0i32.to_be_bytes()); // default
        bad.extend_from_slice(&(-5i32).to_be_bytes()); // npairs < 0
        assert_eq!(inst_len_at(&bad, 0), 1);
    }

    // -- counted-loop recognition ------------------------------------------

    use crate::scev::{BoundsProof, IndexExpr, PreheaderGuard, RangeEnv, SymBound};

    /// A resolver that recognises CP index 42 as `Math.min` and 43 as
    /// `Math.max`, and nothing else.
    fn resolver(cp: u16) -> Option<MinMax> {
        match cp {
            42 => Some(MinMax::Min),
            43 => Some(MinMax::Max),
            _ => None,
        }
    }

    fn no_math(_: u16) -> Option<MinMax> {
        None
    }

    /// `for (i = 0; i < a.length; i++) { }` with the array in local 0.
    ///
    /// ```text
    ///  0: iconst_0        3: aload_0        8: iinc 1, 1
    ///  1: istore_1        4: arraylength   11: goto -9  → 2
    ///  2: iload_1         5: if_icmpge +9  14: return
    /// ```
    fn array_length_loop() -> Vec<u8> {
        vec![
            0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
            0xb1,
        ]
    }

    #[test]
    fn decodes_every_bound_shape_not_just_array_length() {
        // constant
        assert_eq!(
            decode_bound_expr(&[0x10, 0x40], 0, 2, &no_math),
            Some((BoundSource::Const(64), 2))
        );
        assert_eq!(
            decode_bound_expr(&[0x03], 0, 1, &no_math),
            Some((BoundSource::Const(0), 1))
        );
        assert_eq!(
            decode_bound_expr(&[0x02], 0, 1, &no_math),
            Some((BoundSource::Const(-1), 1))
        );
        // local
        assert_eq!(
            decode_bound_expr(&[0x1c], 0, 1, &no_math),
            Some((BoundSource::Local(2), 1))
        );
        assert_eq!(
            decode_bound_expr(&[0x15, 0x09], 0, 2, &no_math),
            Some((BoundSource::Local(9), 2))
        );
        // array length
        assert_eq!(
            decode_bound_expr(&[0x2a, 0xbe], 0, 2, &no_math),
            Some((BoundSource::ArrayLength(0), 2))
        );
        // instance field
        assert_eq!(
            decode_bound_expr(&[0x2a, 0xb4, 0x00, 0x07], 0, 4, &no_math),
            Some((
                BoundSource::Field {
                    cp_index: 7,
                    receiver_local: Some(0)
                },
                4
            ))
        );
        // static field
        assert_eq!(
            decode_bound_expr(&[0xb2, 0x00, 0x09], 0, 3, &no_math),
            Some((
                BoundSource::Field {
                    cp_index: 9,
                    receiver_local: None
                },
                3
            ))
        );
        // Math.min(16, n)
        assert_eq!(
            decode_bound_expr(&[0x10, 0x10, 0x1c, 0xb8, 0x00, 0x2a], 0, 6, &resolver),
            Some((
                BoundSource::Min(
                    Box::new(BoundSource::Const(16)),
                    Box::new(BoundSource::Local(2))
                ),
                6
            ))
        );
        // Math.max(0, n)
        assert_eq!(
            decode_bound_expr(&[0x03, 0x1c, 0xb8, 0x00, 0x2b], 0, 5, &resolver),
            Some((
                BoundSource::Max(
                    Box::new(BoundSource::Const(0)),
                    Box::new(BoundSource::Local(2))
                ),
                5
            ))
        );
        // An unresolved invokestatic falls back to the first atom rather than
        // guessing what the call computed.
        assert_eq!(
            decode_bound_expr(&[0x10, 0x10, 0x1c, 0xb8, 0x00, 0x63], 0, 6, &no_math),
            Some((BoundSource::Const(16), 2))
        );
        // Nothing recognisable at all.
        assert_eq!(decode_bound_expr(&[0x60], 0, 1, &no_math), None);
    }

    #[test]
    fn canonical_array_length_loop_is_proved_without_any_guard() {
        let code = array_length_loop();
        let l = analyze_counted_loop(&code, code.len(), 2, 11, LoopForm::PreTested, &no_math)
            .expect("counted loop");
        assert_eq!(l.iv.local, 1);
        assert_eq!(l.iv.stride, Stride::Const(1));
        assert_eq!(l.iv.init, IntRange::constant(0));
        assert_eq!(l.cmp, ExitCmp::Ge);
        assert_eq!(l.bound, BoundSource::ArrayLength(0));
        assert!(l.heap_stable);
        assert_eq!(l.modified_locals, 1 << 1);
        // Indexing the very array the limit came from needs nothing at all.
        assert_eq!(
            l.prove_index_in_bounds_of(
                &IndexExpr::identity(1),
                Some(0),
                IntRange::array_length(),
                &RangeEnv::new()
            ),
            BoundsProof::Static
        );
        // A different array does not inherit that proof.
        assert!(matches!(
            l.prove_index_in_bounds_of(
                &IndexExpr::identity(1),
                Some(3),
                IntRange::array_length(),
                &RangeEnv::new()
            ),
            BoundsProof::Guarded(_)
        ));
    }

    #[test]
    fn inclusive_and_non_unit_and_negative_strides_are_all_recognised() {
        // for (i = 0; i <= n; i += 2)  — inclusive comparator, non-unit step.
        //  0: iconst_0   2: iload_1   4: if_icmpgt +10 → 14   11: goto -9 → 2
        //  1: istore_1   3: iload_2   8: iinc 1, 2            14: return
        let code = vec![
            0x03, 0x3c, 0x1b, 0x1c, 0xa3, 0x00, 0x0a, 0x00, 0x84, 0x01, 0x02, 0xa7, 0xff, 0xf7,
            0xb1,
        ];
        let l = analyze_counted_loop(&code, code.len(), 2, 11, LoopForm::PreTested, &no_math)
            .expect("counted loop");
        assert_eq!(l.cmp, ExitCmp::Gt);
        assert!(l.is_inclusive());
        assert_eq!(l.iv.stride, Stride::Const(2));
        assert_eq!(l.bound, BoundSource::Local(2));

        // for (i = n; i > 0; i--) — descending.
        //  0: iload_2  1: istore_1  2: iload_1  3: iconst_0  4: if_icmple +10 → 14
        //  7: nop      8: iinc 1,-1 11: goto -9 → 2          14: return
        let down = vec![
            0x1c, 0x3c, 0x1b, 0x03, 0xa4, 0x00, 0x0a, 0x00, 0x84, 0x01, 0xff, 0xa7, 0xff, 0xf7,
            0xb1,
        ];
        let d = analyze_counted_loop(&down, down.len(), 2, 11, LoopForm::PreTested, &no_math)
            .expect("counted loop");
        assert_eq!(d.cmp, ExitCmp::Le);
        assert_eq!(d.iv.stride, Stride::Const(-1));
        // The entry value came from another local, so it stays unknown — the
        // proof asks for a runtime witness rather than inventing one.
        assert!(d.iv.init.is_unknown());
    }

    #[test]
    fn compound_assignment_strides_are_recognised_constant_and_variable() {
        // `j += 4` written as iload/iconst/iadd/istore.
        //  0: iload_1  1: iconst_4  2: iadd  3: istore_1
        let konst = vec![0x1b, 0x07, 0x60, 0x3c];
        assert_eq!(
            find_iv_stride(&konst, 0, konst.len(), 1),
            Some(Stride::Const(4))
        );
        // `j -= 4`.
        let minus = vec![0x1b, 0x07, 0x64, 0x3c];
        assert_eq!(
            find_iv_stride(&minus, 0, minus.len(), 1),
            Some(Stride::Const(-4))
        );
        // `j += step` — the Sieve inner loop; the step's sign is a runtime
        // fact, so it becomes a guarded stride, not an assumed positive one.
        let var = vec![0x1b, 0x1c, 0x60, 0x3c];
        assert_eq!(
            find_iv_stride(&var, 0, var.len(), 1),
            Some(Stride::Variable(2))
        );
        // MUST REFUSE: the commuted form is not distinguishable from an
        // unrelated expression overwriting the IV.
        let commuted = vec![0x1c, 0x1b, 0x60, 0x3c];
        assert_eq!(find_iv_stride(&commuted, 0, commuted.len(), 1), None);
        // MUST REFUSE: two modifications in one body.
        let twice = vec![0x84, 0x01, 0x01, 0x84, 0x01, 0x01];
        assert_eq!(find_iv_stride(&twice, 0, twice.len(), 1), None);
        // MUST REFUSE: a wide-indexed istore that aliases the IV.
        let wide = vec![0xc4, 0x36, 0x00, 0x01];
        assert_eq!(find_iv_stride(&wide, 0, wide.len(), 1), None);
        // MUST ACCEPT twin: the same wide istore on a different slot.
        let wide_other = vec![0xc4, 0x36, 0x00, 0x05, 0x84, 0x01, 0x01];
        assert_eq!(
            find_iv_stride(&wide_other, 0, wide_other.len(), 1),
            Some(Stride::Const(1))
        );
    }

    #[test]
    fn a_non_int_store_to_the_iv_slot_refuses_the_loop() {
        // MUST REFUSE: `astore_1` reuses the IV's slot for a reference, which
        // the JVM permits across disjoint live ranges.
        let clobber = vec![0x84, 0x01, 0x01, 0x4c]; // iinc 1,1 ; astore_1
        assert_eq!(find_iv_stride(&clobber, 0, clobber.len(), 1), None);
        // MUST REFUSE: an `lstore_0` writes slot 1 as its dead high half.
        let high_half = vec![0x84, 0x01, 0x01, 0x3f]; // iinc 1,1 ; lstore_0
        assert_eq!(find_iv_stride(&high_half, 0, high_half.len(), 1), None);
        // MUST ACCEPT twin: the same store on a slot the IV does not occupy.
        let ok = vec![0x84, 0x01, 0x01, 0x4d]; // iinc 1,1 ; astore_2
        assert_eq!(find_iv_stride(&ok, 0, ok.len(), 1), Some(Stride::Const(1)));
    }

    #[test]
    fn a_variable_stride_loop_carries_its_sign_guard_all_the_way_through() {
        // for (j = k; j < n; j += step) — locals: 1 = j, 2 = n, 3 = step.
        //  0: iload_1   1: iload_2   2: if_icmpge +12 → 14
        //  5: iload_1   6: iload_3   7: iadd   8: istore_1
        //  9: goto -9 → 0   12: nop  13: nop  14: return
        let code = vec![
            0x1b, 0x1c, 0xa2, 0x00, 0x0c, 0x1b, 0x1d, 0x60, 0x3c, 0xa7, 0xff, 0xf7, 0x00, 0x00,
            0xb1,
        ];
        let l = analyze_counted_loop(&code, code.len(), 0, 9, LoopForm::PreTested, &no_math)
            .expect("counted loop");
        assert_eq!(l.iv.stride, Stride::Variable(3));
        let p = l.prove_index_in_bounds(
            &IndexExpr::identity(1),
            IntRange::array_length(),
            &RangeEnv::new(),
        );
        match p {
            BoundsProof::Guarded(g) => {
                assert!(
                    g.contains(&PreheaderGuard::StrideInRange {
                        local: 3,
                        headroom: SymBound {
                            base: crate::scev::BoundTerm::Bound(BoundSource::Local(2)),
                            addend: -1,
                        },
                    }),
                    "stride guard missing from {g:?}"
                );
            }
            other => panic!("expected Guarded, got {other:?}"),
        }
    }

    #[test]
    fn a_loop_body_that_writes_the_limit_is_refused_by_the_proof() {
        // for (i = 0; i < n; i++) { n = something; } — the body rewrites the
        // limit local, so a pre-header guard on it would go stale.
        //  0: iconst_0  1: istore_1  2: iload_1  3: iload_2  4: if_icmpge +10
        //  7: iconst_0  8: istore_2  9: iinc 1,1  12: goto -10 → 2  15: return
        let code = vec![
            0x03, 0x3c, 0x1b, 0x1c, 0xa2, 0x00, 0x0b, 0x03, 0x3d, 0x84, 0x01, 0x01, 0xa7, 0xff,
            0xf6, 0xb1,
        ];
        let l = analyze_counted_loop(&code, code.len(), 2, 12, LoopForm::PreTested, &no_math)
            .expect("counted loop");
        assert_eq!(l.modified_locals & (1 << 2), 1 << 2);
        assert!(matches!(
            l.prove_index_in_bounds(
                &IndexExpr::identity(1),
                IntRange::array_length(),
                &RangeEnv::new()
            ),
            BoundsProof::Refused(_)
        ));
    }

    #[test]
    fn modified_locals_strict_refuses_what_it_cannot_represent() {
        // A wide-indexed store above slot 63 cannot be recorded, so the whole
        // answer is refused instead of silently losing the write.
        let high = vec![0xc4, 0x36, 0x00, 0x64];
        assert_eq!(modified_locals_strict(&high, 0, high.len()), None);
        // A `long` store marks its dead high half too.
        let wide_pair = vec![0x40]; // lstore_1 → locals 1 and 2
        assert_eq!(
            modified_locals_strict(&wide_pair, 0, wide_pair.len()),
            Some(0b110)
        );
    }

    #[test]
    fn heap_stability_is_required_for_a_field_bound() {
        assert!(body_is_heap_stable(&[0x1b, 0x84, 0x01, 0x01], 0, 4));
        // A call could change any field.
        assert!(!body_is_heap_stable(&[0xb6, 0x00, 0x01], 0, 3));
        // So could a putfield.
        assert!(!body_is_heap_stable(&[0xb5, 0x00, 0x01], 0, 3));
    }

    #[test]
    fn constant_init_is_proved_or_left_unknown_never_assumed() {
        let code = array_length_loop();
        assert_eq!(constant_iv_init(&code, code.len(), 2, 14, 1), Some(0));
        // Local 0 is a parameter — never stored — so its entry value is
        // unknown, not zero.
        assert_eq!(constant_iv_init(&code, code.len(), 2, 14, 0), None);
    }

    /// `for (i = 0; i < 10; i++) { if (i == p) break; }`, or the same loop
    /// with the `break` test replaced by `nop`s.
    fn ten_trip_loop(with_break: bool) -> Vec<u8> {
        let mut code = vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x1b, // 2: iload_1 (header)
            0x10, 10, // 3: bipush 10
            0xa2, 0x00, 14,   // 5: if_icmpge -> 19
            0x1b, // 8: iload_1
            0x1a, // 9: iload_0
            0x9f, 0x00, 9, // 10: if_icmpeq -> 19
            0x84, 1, 1, // 13: iinc 1, 1
            0xa7, 0xff, 0xf2, // 16: goto -> 2
            0xb1, // 19: return
        ];
        if !with_break {
            code[8..13].fill(0x00);
        }
        code
    }

    #[test]
    fn a_second_exit_drops_the_trip_count_floor_to_zero() {
        let env = RangeEnv::default();
        let plain = analyze_counted_loop(
            &ten_trip_loop(false),
            20,
            2,
            16,
            LoopForm::PreTested,
            &no_math,
        )
        .expect("the plain loop is counted");
        assert!(!plain.has_other_exit);
        let t = plain.trip_count(&env).expect("constant trip count");
        assert_eq!((t.min, t.max), (10, 10));

        let broken = analyze_counted_loop(
            &ten_trip_loop(true),
            20,
            2,
            16,
            LoopForm::PreTested,
            &no_math,
        )
        .expect("the loop with a break is still counted");
        assert!(broken.has_other_exit);
        let t = broken.trip_count(&env).expect("an upper bound survives");
        assert_eq!((t.min, t.max), (0, 10));
        assert!(matches!(
            broken.prove_trip_count_at_least(5, &env),
            crate::scev::TripCountProof::Refused(_)
        ));
    }

    #[test]
    fn an_enclosing_back_edge_between_the_store_and_the_header_unproves_the_init() {
        // for (;;) { i = 0 is outside; the outer loop re-enters at the inner
        // header without passing the store:
        // 0: iconst_0; 1: istore_1; 2: iload_1 (inner header); 3: bipush 10;
        // 5: if_icmpge -> 14; 8: iinc 1,1; 11: goto -> 2; 14: goto -> 2
        let mut code = vec![
            0x03, 0x3c, 0x1b, 0x10, 10, 0xa2, 0x00, 9, 0x84, 1, 1, 0xa7, 0xff, 0xf7, 0xa7, 0xff,
            0xf4, 0xb1,
        ];
        assert_eq!(constant_iv_init(&code, code.len(), 2, 14, 1), None);
        // Without the outer back edge the store runs before every entry.
        code[14..17].fill(0x00);
        assert_eq!(constant_iv_init(&code, code.len(), 2, 14, 1), Some(0));
    }

    #[test]
    fn loop_parents_finds_the_enclosing_loop() {
        let outer = LoopInfo {
            header_pc: 0,
            back_edges: vec![40],
            body_blocks: (0, 43),
        };
        let inner = LoopInfo {
            header_pc: 10,
            back_edges: vec![20],
            body_blocks: (10, 23),
        };
        let parents = loop_parents(&[outer, inner]);
        assert_eq!(parents, vec![None, Some(0)]);
    }

    #[test]
    fn invariant_load_skips_when_receiver_local_modified() {
        // PC 0: astore_0  ; (modifies local 0)
        // PC 1: aload_0
        // PC 2: getfield #0x0001
        // PC 5: pop
        let code: Vec<u8> = vec![0x4b, 0x2a, 0xb4, 0x00, 0x01, 0x57];
        let li = LoopInfo {
            header_pc: 0,
            back_edges: vec![],
            body_blocks: (0, code.len()),
        };
        let v = find_invariant_loads(&li, &code);
        // getfield is filtered out because local 0 is stored within
        // the body extent (modified). getstatic would still pass —
        // none here.
        assert!(v.is_empty());
    }
}
