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
//! ## Status — WIRED (corrected 2026-09-16)
//!
//! This block used to read "Analysis-only. Hoisting itself is intentionally
//! deferred", followed by `TODO(round-12+): wire hoisting consumer into
//! `x64::compile_method` and the IR optimizer`. Both consumers exist:
//!
//! | consumer | what it takes from here |
//! |---|---|
//! | `x64::bce` | `analyze_counted_loop_with_handlers`, `decode_bound_expr_with_hoisted_fields` (round 13 wave 3: `decode_bound_expr` plus the field sites a pre-header hoisted), `MinMax` — the counted-loop proof behind guarded bounds-check elision, told the method's exception table (round 9 wave 2) |
//! | `x64::loop_rewrite` | `analyze_counted_loop_at_with_handlers` — the trip-count versioning guard, told the exception table |
//! | `x64::simd_analysis` | `constant_iv_init` — the vector loop's constant start |
//! | `scev::RangeEnv::with_loop_iv_placed` | `modified_locals_strict` — "the outer body's prefix does not write the outer IV" (round 11 wave 4; `x64::bce` calls it) |
//!
//! The handler-blind wrappers `analyze_counted_loop`, `analyze_counted_loop_at`
//! and `find_iv_stride` have no production caller and are `#[cfg(test)]`
//! since round 11 wave 4.
//!
//! Every `*_with_handlers` entry point builds a [`LoopMethodTables`] per call.
//! A caller asking about every loop of a method can hold one instead, and the
//! whole-method tables the entry-value proof reads are then built once per
//! method rather than once per loop (round 11 wave 8).
//!
//! (A third row used to name `x64::escape_analysis` as a consumer of
//! `modified_locals_strict`. It is not one: it carries its own copy,
//! `find_modified_locals`, which clamps slots past 63 onto bit 63 where this
//! module refuses them. Corrected in round 11.)
//!
//! And hoisting itself is performed, by two passes this module does not
//! own: `ir_optimize::licm` (default-ON, `CRATONVM_JIT_LICM=0` to opt out)
//! re-anchors invariant reads to the pre-header on the SSA graph, and
//! `x64::licm` carries the single-pass `aaload`/FP forms. The three
//! preconditions the old text listed as blockers are discharged there, on
//! the SSA graph, rather than here on bytecode: safepoint placement by the
//! re-anchor (the hoisted read lands at the pre-header's control, so its
//! oop map is the pre-header's), the aliasing question by
//! `Graph::may_alias` + `loop_store_clobber`, and residency by
//! `regalloc::plan_register_residency`.
//!
//! So the accurate status is: **this module is a bytecode-level loop
//! ORACLE with live consumers** (the table above), not a deferred foundation. (A third
//! row, `ir_optimize::licm_scev_corroborates` over `detect_loops` +
//! `find_invariant_loads`, was an uncalled hook; round 9 wave 3 deleted all
//! three.) What it
//! deliberately does not do is emit — it produces no machine code and
//! rewrites no graph, which is why every value below is a bytecode
//! quantity (see the ownership note).
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
// It is wired: `x64::bce` (bounds-check elision — a wrong answer here is a
// missing array check) and `x64::loop_rewrite` (trip-count versioning) consume
// it; see the module "Status" note. It emits nothing itself.
// ===========================================================================

use crate::bytecode_analysis;
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
///
/// Test-only since round 13 wave 3 (lane loopcall): both production decoders
/// (`x64::bce`'s exit-test scan and [`LoopMethodTables`]' recogniser) read the
/// limit through [`decode_bound_expr_with_hoisted_fields`], which is this
/// function exactly when no field site is hoisted.
#[cfg(test)]
pub fn decode_bound_expr(
    code: &[u8],
    pc: usize,
    end: usize,
    resolve: &dyn Fn(u16) -> Option<MinMax>,
) -> Option<(BoundSource, usize)> {
    decode_expr(code, pc, end, resolve, &|_| None, 0)
}

/// What a loop pre-header made of one `aload R; getfield F` site: the value it
/// read ONCE into a frame slot, which the site then loads instead of the
/// field (round 13 wave 3, lane loopcall).
///
/// The `usize` is a SYNTHETIC local: a slot index at or above the method's
/// `max_locals` (and below 64, so a `modified_locals` bit can speak for it),
/// which no bytecode can store to. That is what makes the ordinary invariance
/// test hold for it by construction, and what lets the range proof treat the
/// hoisted array's length as `ArrayLength(synthetic)` exactly as it treats a
/// local array's. The caller (`x64::bce`) owns the mapping back to the slot.
///
/// Sound only because the consumer guarantees that EVERY read of `R.F` the
/// loop performs is the slot load: a site the emitter re-reads from the heap
/// would let a racing store make the bound and the access name different
/// objects (or different sizes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoistedFieldSite {
    /// An array field: `aload R; getfield F; arraylength` decodes as
    /// [`BoundSource::ArrayLength`] of the synthetic local.
    Array(usize),
    /// An `int` field: `aload R; getfield F` decodes as [`BoundSource::Local`]
    /// of the synthetic local.
    Int(usize),
}

/// Decode the limit of a loop's exit test starting at `pc` -- the shapes
/// `decode_bound_expr` lists -- told which `aload R; getfield F` sites (keyed
/// by the `aload`'s pc) a loop pre-header has hoisted into a slot
/// ([`HoistedFieldSite`]). A site `hoisted` answers `None` for decodes exactly
/// as `decode_bound_expr` does.
pub fn decode_bound_expr_with_hoisted_fields(
    code: &[u8],
    pc: usize,
    end: usize,
    resolve: &dyn Fn(u16) -> Option<MinMax>,
    hoisted: &dyn Fn(usize) -> Option<HoistedFieldSite>,
) -> Option<(BoundSource, usize)> {
    decode_expr(code, pc, end, resolve, hoisted, 0)
}

/// Recursion depth cap for `decode_bound_expr`. Nested `Math.min` chains
/// beyond this simply stop being recognised — a bounded compile-time cost with
/// no soundness content.
const BOUND_EXPR_MAX_DEPTH: u32 = 4;

fn decode_expr(
    code: &[u8],
    pc: usize,
    end: usize,
    resolve: &dyn Fn(u16) -> Option<MinMax>,
    hoisted: &dyn Fn(usize) -> Option<HoistedFieldSite>,
    depth: u32,
) -> Option<(BoundSource, usize)> {
    if depth > BOUND_EXPR_MAX_DEPTH {
        return None;
    }
    let (a, p) = decode_atom(code, pc, end, hoisted)?;
    // Postfix lookahead for the `min`/`max` combinator: `<a> <b> invokestatic`.
    if let Some((b, q)) = decode_expr(code, p, end, resolve, hoisted, depth + 1) {
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

fn decode_atom(
    code: &[u8],
    pc: usize,
    end: usize,
    hoisted: &dyn Fn(usize) -> Option<HoistedFieldSite>,
) -> Option<(BoundSource, usize)> {
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
                // A hoisted site reads the pre-header's slot, not the heap.
                match hoisted(pc) {
                    Some(HoistedFieldSite::Array(s))
                        if after + 3 < end && code.get(after + 3) == Some(&0xbe) =>
                    {
                        return Some((BoundSource::ArrayLength(s), after + 4));
                    }
                    Some(HoistedFieldSite::Int(s)) => {
                        return Some((BoundSource::Local(s), after + 3));
                    }
                    _ => {}
                }
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
/// more than once on some path from the header back to it (several textual
/// advances of one stride, one per path, are admitted since round 11 wave 8),
/// or in any shape not listed below, including every `wide`-indexed alias:
///
/// * `iinc iv, c`                              → `Stride::Const(c)`
/// * `iload iv ; <const> ; iadd ; istore iv`   → `Stride::Const(c)`
/// * `iload iv ; <const> ; isub ; istore iv`   → `Stride::Const(-c)`
/// * `iload iv ; iload s ; iadd ; istore iv`   → `Stride::Variable(s)`
///
/// Test-only (round 11 wave 4): production reaches the same scan through
/// `analyze_counted_loop_at_with_handlers`, which passes the handler entries.
#[cfg(test)]
pub fn find_iv_stride(code: &[u8], start: usize, end: usize, iv: usize) -> Option<Stride> {
    let entries = handler_entries(code, code.len(), None);
    find_iv_stride_in(code, start, end, iv, &entries)
}

/// `find_iv_stride` against an already-resolved set of handler entries
/// (see [`handler_entries`]), so a caller that asks about several candidate
/// locals of one loop resolves the exception table once.
fn find_iv_stride_in(
    code: &[u8],
    start: usize,
    end: usize,
    iv: usize,
    entries: &[(usize, usize, usize)],
) -> Option<Stride> {
    let mut found: Option<Stride> = None;
    let mut count = 0usize;
    // PC of the instruction that commits the advance (the `iinc`, or the
    // `istore` of the compound form).
    let mut inc_pc = start;
    // Every advance as `(pc, stride)`, for the several-advance form (round 11
    // wave 8; see `every_iteration_runs_one_advance`).
    let mut advances: Vec<(usize, Stride)> = Vec::new();
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
                    inc_pc = pc;
                    let step = Stride::Const(code[pc + 2] as i8 as i32);
                    found = Some(step);
                    advances.push((pc, step));
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
                    inc_pc = pc;
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
                    // The four instructions are one expression only if control
                    // enters them at the `iload`. A branch landing on the step
                    // operand, the arithmetic or the `istore` itself joins a
                    // value computed on another path: `i = back ? i - 1 : i + 1`
                    // compiles to `…isub; goto S; …iload i; iconst_1; iadd;
                    // S: istore i`, whose textual tail reads as `+1` although
                    // half the iterations step by -1 (round 11).
                    if branch_lands_in(code, start, end, p3, pc) {
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
                    advances.push((pc, step));
                }
            }
        }
        prev = [Some(pc), prev[0], prev[1]];
        pc += bytecode_analysis::step(code, pc);
    }
    if count == 1 {
        return if advance_runs_once_per_iteration(code, start, end, inc_pc, entries) {
            found
        } else {
            None
        };
    }
    // Several textual advances (round 11 wave 8): `while (i < n) { if (p) {
    // i++; continue; } ..; i++; }` has one per path, and its two back edges
    // are one loop once the caller merges them per header. Admitted when
    // every advance has the same stride and every path from the header back
    // to it runs exactly one of them.
    let (_, first) = *advances.first()?;
    if count < 2 || advances.iter().any(|&(_, s)| s != first) {
        return None;
    }
    let pcs: Vec<usize> = advances.iter().map(|&(p, _)| p).collect();
    if every_iteration_runs_one_advance(code, start, end, &pcs, entries) {
        Some(first)
    } else {
        None
    }
}

/// Whether every path from the header `start` back to it, inside `[start,
/// end)`, runs exactly one of the advances at `advances` -- the
/// several-advance form of [`advance_runs_once_per_iteration`].
///
/// A forward dataflow over the region's instructions, the state being how
/// many advances ran since the header (0 or 1). The claim fails when an
/// advance is reached with one already run, when an edge back to the header
/// carries 0, or when two paths reaching one instruction disagree (the
/// advance then sits on only some of the paths through it -- which is also
/// how an advance inside an inner loop shows: the inner header meets 0 from
/// its entry and 1 from its own back edge). An edge leaving the region ends
/// the iteration and carries no obligation.
///
/// Exception edges are edges too. A handler inside the region must have its
/// whole protected range inside it (an exception from outside is a side entry,
/// refused), and each protected instruction sends its state BEFORE it runs
/// to the handler -- an instruction that throws does so before its effect,
/// and neither `iinc` nor `istore` throws. `jsr`/`ret` and an undecodable
/// branch or switch refuse, as does a target that is not an instruction start.
fn every_iteration_runs_one_advance(
    code: &[u8],
    start: usize,
    end: usize,
    advances: &[usize],
    entries: &[(usize, usize, usize)],
) -> bool {
    let end = end.min(code.len());
    if start >= end {
        return false;
    }
    let len = end - start;
    let mut is_start = vec![false; len];
    let mut pc = start;
    while pc < end {
        is_start[pc - start] = true;
        pc += bytecode_analysis::step(code, pc);
    }
    let mut handlers: Vec<(usize, usize, usize)> = Vec::new();
    for &(s, e, h) in entries {
        if s >= e || h < start || h >= end {
            continue; // cannot throw, or lands outside the loop
        }
        if s < start || e > end || !is_start[h - start] {
            return false;
        }
        handlers.push((s, e, h));
    }
    const UNSEEN: u8 = u8::MAX;
    let mut state = vec![UNSEEN; len];
    state[0] = 0;
    let mut work: Vec<usize> = vec![start];
    let mut succs: Vec<usize> = Vec::new();
    let mut edges: Vec<(usize, u8)> = Vec::new();
    while let Some(pc) = work.pop() {
        let before = state[pc - start];
        let after = if advances.contains(&pc) {
            if before != 0 {
                return false; // a second advance on this path
            }
            1
        } else {
            before
        };
        succs.clear();
        if !bytecode_analysis::normal_successors(code, code.len(), pc, &mut succs) {
            return false;
        }
        edges.clear();
        edges.extend(succs.iter().map(|&t| (t, after)));
        for &(s, e, h) in &handlers {
            if pc >= s && pc < e {
                edges.push((h, before));
            }
        }
        for &(t, v) in &edges {
            if t < start || t >= end {
                continue; // leaves the loop
            }
            if t == start {
                if v != 1 {
                    return false; // back to the header without an advance
                }
                continue;
            }
            if !is_start[t - start] {
                return false;
            }
            match state[t - start] {
                UNSEEN => {
                    state[t - start] = v;
                    work.push(t);
                }
                seen if seen == v => {}
                _ => return false,
            }
        }
    }
    true
}

/// Whether every path from the header `start` back to it, inside `[start,
/// end)`, runs the exit test at `test_pc`: a walk from the header that never
/// expands `test_pc` must not reach the header again. Exception edges count
/// (a protected instruction reaches its handler); `jsr`/`ret` and an
/// undecodable branch or switch refuse. An edge leaving the region is an exit.
fn exit_test_on_every_iteration(
    code: &[u8],
    start: usize,
    end: usize,
    test_pc: usize,
    entries: &[(usize, usize, usize)],
) -> bool {
    let end = end.min(code.len());
    if start >= end {
        return false;
    }
    let mut seen = vec![false; end - start];
    seen[0] = true;
    let mut work: Vec<usize> = vec![start];
    let mut succs: Vec<usize> = Vec::new();
    while let Some(pc) = work.pop() {
        if pc == test_pc {
            continue;
        }
        succs.clear();
        if !bytecode_analysis::normal_successors(code, code.len(), pc, &mut succs) {
            return false;
        }
        for &(s, e, h) in entries {
            if s < e && pc >= s && pc < e {
                succs.push(h);
            }
        }
        for &t in &succs {
            if t < start || t >= end {
                continue;
            }
            if t == start {
                return false; // an iteration that skipped the test
            }
            if !seen[t - start] {
                seen[t - start] = true;
                work.push(t);
            }
        }
    }
    true
}

/// Does any branch or switch instruction in `[start, end)` target a pc in
/// `(lo, hi]`?
///
/// The peephole recognisers in this module read an expression off TEXTUALLY
/// consecutive instructions, which is only the dataflow when nothing jumps into
/// the middle of them. An undecodable branch or switch answers `true`, the
/// refusing direction. Exception handlers are not consulted: a handler starts
/// with exactly the thrown reference on the stack, so the verifier rejects one
/// landing inside an expression that has operands pushed.
fn branch_lands_in(code: &[u8], start: usize, end: usize, lo: usize, hi: usize) -> bool {
    let end = end.min(code.len());
    let inside = |t: usize| t > lo && t <= hi;
    let mut pc = start;
    while pc < end {
        match code[pc] {
            0x99..=0xa8 | 0xc6..=0xc9 => match bytecode_analysis::offset_branch_target(code, pc) {
                Some(t) if inside(t) => return true,
                Some(_) => {}
                None => return true,
            },
            0xaa | 0xab => match bytecode_analysis::switch_table(code, code.len(), pc) {
                Some(table) => {
                    if table.targets().any(inside) {
                        return true;
                    }
                }
                None => return true,
            },
            _ => {}
        }
        pc += bytecode_analysis::step(code, pc);
    }
    false
}

/// Whether the single advance at `inc_pc` executes exactly once on every
/// iteration of the loop `[start, end)` — the "by the recorded stride" half of
/// producer obligation 1 in `crate::scev`.
///
/// Uniqueness of the textual write (what `find_iv_stride_in` counts) is not the
/// same claim, and two ordinary shapes separate them:
///
/// * the advance sits in a NESTED loop — `for (i = 0; i < n; ) { while (p())
///   i++; }` — so one outer iteration moves the IV by `k * stride` for a
///   runtime `k`. `CountedLoop::trip_count` then over-states `min` (the loop
///   runs FEWER times than `(bound - init) / stride`), which is exactly the
///   number a vectorizer or unroller sizes itself on;
/// * the advance can be SKIPPED — `while (i < n) { if (p()) continue; i++; }`
///   with the `continue` a `goto header` ahead of the `iinc` — so an iteration
///   can move the IV by zero and `trip_count().max` is not a bound at all.
///
/// Both are refused. Every branch in the body is classified against
/// `inc_pc`: before it, a branch may exit, go forward to at most `inc_pc`
/// (javac's `continue` targets the `iinc` itself), or go backward into an inner
/// loop that closes before `inc_pc`, but not back to the header; after it, a
/// branch may exit, go forward, or return to the header, but not backward into
/// `(start, inc_pc]`, which would make the advance part of an inner loop.
/// `jsr`/`ret` and an undecodable switch refuse.
///
/// Exception edges are classified the same way (round 9 wave 2). A handler
/// `h` inside the body is an edge from every pc of its protected range to `h`:
/// a range holding a pc before the advance may not land on the header or past
/// the advance, a range holding a pc after it may not land in `(start,
/// inc_pc]`, and a range reaching outside the body is a side entry into the
/// loop and refuses. `entries` is [`handler_entries`]' answer, so a caller
/// with no exception table gets its conservative reading.
fn advance_runs_once_per_iteration(
    code: &[u8],
    start: usize,
    end: usize,
    inc_pc: usize,
    entries: &[(usize, usize, usize)],
) -> bool {
    for &(s, e, h) in entries {
        if s >= e || h < start || h >= end {
            continue; // cannot throw, or lands outside the loop
        }
        if s < start || e > end {
            return false; // an exception from outside enters the body
        }
        // Some pc of `[s, e)` precedes the advance: the edge skips it when it
        // lands on the header or beyond the advance.
        if s < inc_pc && (h == start || h > inc_pc) {
            return false;
        }
        // Some pc of `[s, e)` follows the advance: landing back at or before
        // it makes the advance part of an inner loop.
        if e > inc_pc + 1 && h > start && h <= inc_pc {
            return false;
        }
    }
    let mut targets: Vec<usize> = Vec::new();
    let mut pc = start;
    while pc < end {
        let op = code[pc];
        targets.clear();
        match op {
            0x99..=0xa7 | 0xc6 | 0xc7 | 0xc8 => {
                match bytecode_analysis::offset_branch_target(code, pc) {
                    Some(t) => targets.push(t),
                    None => return false,
                }
            }
            0xaa | 0xab => match bytecode_analysis::switch_table(code, code.len(), pc) {
                Some(table) => targets.extend(table.targets()),
                None => return false,
            },
            0xa8 | 0xa9 | 0xc9 => return false,
            _ => {}
        }
        for &t in &targets {
            if t < start || t >= end {
                continue; // leaves the loop: ends the iteration count, not an extra one
            }
            if pc < inc_pc {
                if t == start || t > inc_pc {
                    return false; // skips the advance
                }
            } else if pc > inc_pc && t > start && t <= inc_pc {
                return false; // the advance is inside an inner loop
            }
        }
        pc += bytecode_analysis::step(code, pc);
    }
    true
}

/// Every local written in `[start, end)`, or `None` when the range writes a
/// local the `u64` set cannot represent (slot `>= 64`).
///
/// This refuses rather than silently dropping such a store — an invariance
/// check built on a set that quietly forgot a write is not a check at all.
/// `long`/`double` stores also mark the dead high half.
///
/// (Round 9 wave 3: the saturating wrapper `modified_locals_in_range` and its
/// one caller, the legacy `find_invariant_loads`, were deleted; the tests
/// that used to exercise this scanner through the wrapper now call it
/// directly.)
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
        pc += bytecode_analysis::step(code, pc);
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
        pc += bytecode_analysis::step(code, pc);
    }
    true
}

/// Every `i16`-offset branch in the method as `(source, target)`, or `None`
/// when the method contains a branch form this scan does not model
/// (`tableswitch`, `lookupswitch`, `jsr`/`ret`, `goto_w`/`jsr_w`) or a branch
/// whose target cannot be decoded into the method.
fn branch_edges(code: &[u8], code_len: usize) -> Option<Vec<(usize, usize)>> {
    let mut edges = Vec::new();
    let mut pc = 0usize;
    while pc < code_len {
        match code[pc] {
            0xaa | 0xab | 0xa8 | 0xa9 | 0xc8 | 0xc9 => return None,
            0x99..=0xa7 | 0xc6 | 0xc7 => {
                // A branch this scan cannot place is a branch it cannot rule
                // out landing between the store and the header -- refuse.
                let target = bytecode_analysis::offset_branch_target(&code[..code_len], pc)
                    .filter(|&t| t < code_len)?;
                edges.push((pc, target));
            }
            _ => {}
        }
        pc += bytecode_analysis::step(code, pc);
    }
    Some(edges)
}

/// The constant the induction variable holds on entry to the loop, when the
/// method makes that provable.
///
/// Requires exactly one store to `iv` outside the loop body, before the
/// header, storing a constant push, with no branch landing on the store or the
/// push, and no `iinc` or `wide`-indexed write to `iv` outside the body -- and,
/// the part that makes the answer an ENTRY value, that the store runs on every
/// path into the loop (see below).
///
/// `None` is not "zero" -- it is "unknown", and the range proof falls back to a
/// pre-header guard on the local.
///
/// # Every entry, not "some store before the header"
///
/// This used to argue "JVM definite assignment makes that single store
/// dominate the header". It does not: definite assignment is satisfied just as
/// well by a PARAMETER, which needs no store at all.
///
/// ```text
/// void f(int[] a, int i, boolean c) {   // i is local 1, a parameter
///     if (c) i = 0;                     // iload_2; ifeq L; iconst_0; istore_1
///     for (; i < a.length; i++) a[i]++; // L: iload_1 ...   <- the header
/// }
/// ```
///
/// One store, before the header, no branch landing on it or its push -- and the
/// `ifeq` jumps straight to the header past it. The old scan answered
/// `Some(0)`, the range proof then discharged `i >= 0` from the constant with
/// no runtime test, and `f(a, -5, false)` indexed `a[-5]` with its bounds check
/// elided. The old test for an enclosing loop's back edge landing between the
/// store and the header caught only the BACKWARD spelling of that bypass; a
/// forward branch landing there is the same hole.
///
/// So the loop's entries are now pinned structurally:
///
/// * from the store to the header is straight-line code that falls through
///   into the header, or straight-line code ending in the loop-entry `goto`
///   javac emits for a rotated loop (`goto cond` immediately before the
///   header, targeting into the body); that segment must not write `iv`;
/// * no branch anywhere lands strictly between the store and the header;
/// * no branch from OUTSIDE the body -- other than that entry `goto` -- lands
///   inside `[header_pc, body_end)`: a side entry into the loop would reach it
///   without passing the store.
///
/// An exception handler entry has no bytecode branch, so the scan above cannot
/// see it. Since round 9 wave 2 the handler entries are checked separately
/// ([`constant_iv_init_with_handlers`]): a handler that lands on the push, on
/// the store or anywhere in `(store, body_end)` must have its whole protected
/// range inside `(store, body_end)`, so the exception that enters through it
/// was thrown after the store ran. This form has no exception table and uses
/// [`handler_entries`]' conservative reading (every instruction no normal edge
/// reaches may be a handler, protecting the whole method).
///
/// That is also why the segment must be STRAIGHT-LINE rather than merely
/// "entered only from the store". The looser rule admits
/// `try { bar(); i = 0; foo(); } catch (E e) { .. } for (; i < n; ..)`, whose
/// javac layout puts the handler body between the store and the header behind
/// a `goto header` — and an exception from `bar()` reaches the header through
/// that handler without ever running the store. A handler body needs a jump
/// around it, and the straight-line rule refuses the jump.
pub fn constant_iv_init(
    code: &[u8],
    code_len: usize,
    header_pc: usize,
    body_end: usize,
    iv: usize,
) -> Option<i32> {
    constant_iv_init_with_handlers(code, code_len, header_pc, body_end, iv, None)
}

/// [`constant_iv_init`], told the method's exception table
/// (`(start_pc, end_pc, handler_pc)`, `end_pc` exclusive -- the shape
/// `BackendRequest::exception_ranges` carries) when the caller has it. `None`
/// is the conservative reading; see [`handler_entries`].
pub fn constant_iv_init_with_handlers(
    code: &[u8],
    code_len: usize,
    header_pc: usize,
    body_end: usize,
    iv: usize,
    handlers: Option<&[(usize, usize, usize)]>,
) -> Option<i32> {
    LoopMethodTables::new(code, code_len, handlers).constant_iv_init(header_pc, body_end, iv)
}

/// The whole-method tables the per-loop questions in this module read,
/// built once per method and shared by every loop of it.
///
/// Each free function above builds one per call, which is what a caller that
/// asks about one loop wants. A caller that asks about every loop of a method
/// (`x64::bce` recognises each loop pair of every compile; `x64::simd_analysis`
/// asks for the entry value per candidate) used to pay a whole-method decode
/// per loop for the entry-value proof alone -- `branch_edges` plus the
/// store scan, O(loops x method) (round 11 wave 8,
/// `r11w7-x64-constant-iv-init-rescans-branch-edges-per-call-FIXED-20260924.md`).
/// Holding one of these makes that O(method + loops).
///
/// The handler entries are resolved on construction; the entry-value tables
/// only on the first entry-value question, so a method none of whose loops
/// gets that far pays nothing for them.
pub struct LoopMethodTables<'a> {
    code: &'a [u8],
    /// Clamped to `code.len()`.
    code_len: usize,
    /// [`handler_entries`]' answer.
    entries: Vec<(usize, usize, usize)>,
    init_scan: std::cell::OnceCell<InitScan>,
}

/// The whole-method half of [`constant_iv_init`].
struct InitScan {
    /// [`branch_edges`]' answer: `None` refuses every entry value.
    edges: Option<Vec<(usize, usize)>>,
    /// Every non-`wide` `istore` as `(pc, slot, previous instruction pc)`, in
    /// pc order.
    int_stores: Vec<(usize, usize, Option<usize>)>,
    /// Slots a `wide istore` or `wide iinc` writes anywhere in the method.
    wide_int_writes: Vec<usize>,
}

impl InitScan {
    fn build(code: &[u8], code_len: usize) -> InitScan {
        let mut int_stores = Vec::new();
        let mut wide_int_writes = Vec::new();
        let mut prev: Option<usize> = None;
        let mut pc = 0usize;
        while pc < code_len {
            let op = code[pc];
            if op == 0xc4 && pc + 3 < code_len {
                let real = code[pc + 1];
                let idx = ((code[pc + 2] as usize) << 8) | code[pc + 3] as usize;
                if matches!(real, 0x36 | 0x84) {
                    wide_int_writes.push(idx);
                }
            }
            let istore_target = match op {
                0x36 if pc + 1 < code_len => Some(code[pc + 1] as usize),
                0x3b..=0x3e => Some((op - 0x3b) as usize),
                _ => None,
            };
            if let Some(slot) = istore_target {
                int_stores.push((pc, slot, prev));
            }
            prev = Some(pc);
            pc += bytecode_analysis::step(code, pc);
        }
        InitScan {
            edges: branch_edges(code, code_len),
            int_stores,
            wide_int_writes,
        }
    }
}

impl<'a> LoopMethodTables<'a> {
    /// Tables for `code[..code_len]` with the method's exception table
    /// (`None`: [`handler_entries`]' conservative reading).
    pub fn new(
        code: &'a [u8],
        code_len: usize,
        handlers: Option<&[(usize, usize, usize)]>,
    ) -> LoopMethodTables<'a> {
        let code_len = code_len.min(code.len());
        LoopMethodTables {
            code,
            code_len,
            entries: handler_entries(code, code_len, handlers),
            init_scan: std::cell::OnceCell::new(),
        }
    }

    fn init_scan(&self) -> &InitScan {
        self.init_scan
            .get_or_init(|| InitScan::build(self.code, self.code_len))
    }

    /// [`constant_iv_init_with_handlers`] against these tables.
    pub fn constant_iv_init(&self, header_pc: usize, body_end: usize, iv: usize) -> Option<i32> {
        constant_iv_init_in(self, header_pc, body_end, iv)
    }

    /// [`analyze_counted_loop_at_with_handlers`] against these tables.
    pub fn analyze_counted_loop_at(
        &self,
        header_pc: usize,
        back_edge_pc: usize,
        form: LoopForm,
        resolve: &dyn Fn(u16) -> Option<MinMax>,
    ) -> Option<(CountedLoop, usize)> {
        analyze_counted_loop_in(self, header_pc, back_edge_pc, form, resolve, &|_| None)
    }

    /// [`Self::analyze_counted_loop`], reading the exit test's limit through
    /// [`decode_bound_expr_with_hoisted_fields`]: an `aload R; getfield F`
    /// site `hoisted` names is the pre-header's slot (round 13 wave 3, lane
    /// loopcall). `hoisted` answering `None` everywhere is
    /// [`Self::analyze_counted_loop`] exactly.
    pub fn analyze_counted_loop_with_hoisted_fields(
        &self,
        header_pc: usize,
        back_edge_pc: usize,
        form: LoopForm,
        resolve: &dyn Fn(u16) -> Option<MinMax>,
        hoisted: &dyn Fn(usize) -> Option<HoistedFieldSite>,
    ) -> Option<CountedLoop> {
        analyze_counted_loop_in(self, header_pc, back_edge_pc, form, resolve, hoisted)
            .map(|(l, _)| l)
    }

    /// [`analyze_counted_loop_with_handlers`] against these tables.
    pub fn analyze_counted_loop(
        &self,
        header_pc: usize,
        back_edge_pc: usize,
        form: LoopForm,
        resolve: &dyn Fn(u16) -> Option<MinMax>,
    ) -> Option<CountedLoop> {
        self.analyze_counted_loop_at(header_pc, back_edge_pc, form, resolve)
            .map(|(l, _)| l)
    }
}

/// [`constant_iv_init`] against a method's [`LoopMethodTables`].
fn constant_iv_init_in(
    tables: &LoopMethodTables<'_>,
    header_pc: usize,
    body_end: usize,
    iv: usize,
) -> Option<i32> {
    let code = tables.code;
    let code_len = tables.code_len;
    let entries = tables.entries.as_slice();
    let scan = tables.init_scan();
    let edges = scan.edges.as_deref()?;
    // The LAST store to `iv` before the header (round 11). This used to demand
    // exactly one out-of-body store and no out-of-body `iinc` anywhere in the
    // method, which javac's slot reuse defeats for every loop after the first:
    // `for (int i = 0; …; i++) …; for (int i = 0; …; i++) …` stores and bumps
    // the same slot twice, the entry value of BOTH loops became unknown, and
    // the guarded proof that remained is one `x64::bce` refuses for an inline
    // `a.length` bound — neither loop kept its bounds-check elision.
    //
    // The count was never what made the answer an entry value; the structural
    // checks below are (see the doc above): the segment from THIS store to the
    // header is straight-line, writes nothing to `iv` (an `iinc` there
    // included) and has nothing landing in it, and nothing enters the loop
    // except through it. Any other store or `iinc` is either before this store
    // or unreachable on the way in, so it cannot be the value the loop sees.
    // (A `wide` int write to `iv` anywhere in the method still refuses.)
    if scan.wide_int_writes.contains(&iv) {
        return None;
    }
    let store: Option<(usize, Option<usize>)> = scan
        .int_stores
        .iter()
        .rev()
        .find(|&&(pc, slot, _)| slot == iv && pc < header_pc)
        .map(|&(pc, _, prev)| (pc, prev));
    let (store_pc, push_pc) = match store {
        Some((s, Some(p))) => (s, p),
        _ => return None,
    };
    if store_pc >= header_pc {
        return None; // does not dominate the loop
    }
    // A branch landing ON the store skips the push and stores whatever the
    // other path left on the stack. One landing on the PUSH runs it, so the
    // store still writes the constant: that is exactly where the previous
    // loop's exit lands in `for (…) …; for (int i = 0; …)`, and refusing it
    // (as this did until round 11) lost the second loop's entry value.
    if edges.iter().any(|&(_, t)| t == store_pc) {
        return None;
    }

    // ---- the store-to-header segment -------------------------------------
    let seg_start = store_pc + bytecode_analysis::step(code, store_pc);
    let mut entry_goto: Option<usize> = None;
    let mut p = seg_start;
    while p < header_pc {
        let op = code[p];
        if op == 0xa7 {
            // Only the rotated loop's entry jump, as the last instruction
            // before the header, into the body.
            let t = bytecode_analysis::offset_branch_target(&code[..code_len], p)?;
            if p + 3 != header_pc || t < header_pc || t >= body_end {
                return None;
            }
            entry_goto = Some(p);
            break;
        }
        if !bytecode_analysis::falls_through(op)
            || bytecode_analysis::is_offset_branch(op)
            || bytecode_analysis::is_subroutine_op(op)
        {
            return None;
        }
        p += bytecode_analysis::step(code, p);
    }
    if entry_goto.is_none() && p != header_pc {
        return None; // the segment overran the header: misaligned
    }
    // The segment must not write the IV (any kind, either half of a cat-2).
    let seg_end = entry_goto.unwrap_or(header_pc);
    let seg_writes = modified_locals_strict(code, seg_start, seg_end)?;
    if iv >= 64 || seg_writes & (1u64 << iv) != 0 {
        return None;
    }

    // ---- no path into the loop that skips the store ----------------------
    for &(src, t) in edges {
        if Some(src) == entry_goto {
            continue;
        }
        // Into the segment: a forward bypass, or a re-entry that does not
        // re-run the init (an enclosing loop's back edge).
        if t > store_pc && t < header_pc {
            return None;
        }
        // A side entry into the loop from outside it.
        let src_in_body = src >= header_pc && src < body_end;
        if !src_in_body && t >= header_pc && t < body_end {
            return None;
        }
    }

    // ---- no EXCEPTION path into the loop that skips the store -------------
    //
    // A handler landing on the push, the store, the segment or the body is an
    // entry the branch scan above cannot see. It is harmless only when every
    // pc that can throw to it runs after the store: its whole protected range
    // lies inside `(store, body_end)`, which the checks above made reachable
    // only through the store (the segment is straight-line from it, and the
    // loop has no side entry). A range reaching before the store -- or outside
    // the loop, beyond `body_end` -- can deliver control here without the
    // store having run.
    for &(s, e, h) in entries {
        if s >= e || h < push_pc || h >= body_end {
            continue;
        }
        if s <= store_pc || e > body_end {
            return None;
        }
    }
    const_push_value(code, push_pc, code_len)
}

/// The exception-handler entries the loop scans must treat as extra edges, as
/// `(start_pc, end_pc, handler_pc)` with `end_pc` exclusive and clamped to
/// `code_len`.
///
/// * `Some(table)`: the table itself.
/// * `None` (the caller does not know it): every instruction start that no
///   normal edge reaches from pc 0 is treated as a handler protecting the
///   WHOLE method, because any of them may be a handler entry and nothing says
///   where its exceptions come from. The same conservative reading
///   `x64::bce::store_dominates_every_read` takes. A handler that is also a
///   normal branch target is not found this way; javac does not emit one, and
///   the production callers (the BCE and loop-rewrite paths) pass the real
///   table.
///
/// See `docs/known-issues/jit/bytecode-loop-scans-do-not-model-exception-handler-entries-20260918.md`.
fn handler_entries(
    code: &[u8],
    code_len: usize,
    handlers: Option<&[(usize, usize, usize)]>,
) -> Vec<(usize, usize, usize)> {
    let code_len = code_len.min(code.len());
    if let Some(table) = handlers {
        return table
            .iter()
            .map(|&(s, e, h)| (s, e.min(code_len), h))
            .collect();
    }
    let mut is_start = vec![false; code_len];
    let mut pc = 0usize;
    while pc < code_len {
        is_start[pc] = true;
        pc += bytecode_analysis::step(code, pc);
    }
    let mut seen = vec![false; code_len];
    let mut work: Vec<usize> = Vec::new();
    if code_len > 0 {
        seen[0] = true;
        work.push(0);
    }
    while let Some(p) = work.pop() {
        for q in bytecode_analysis::lenient_successors(code, code_len, p) {
            if q < code_len && is_start[q] && !seen[q] {
                seen[q] = true;
                work.push(q);
            }
        }
    }
    (0..code_len)
        .filter(|&p| is_start[p] && !seen[p])
        .map(|h| (0, code_len, h))
        .collect()
}

/// Whether an exception can enter `[lo, hi)` from outside it: a handler
/// inside the range whose protected range is not wholly inside it.
fn handler_enters_from_outside(entries: &[(usize, usize, usize)], lo: usize, hi: usize) -> bool {
    entries
        .iter()
        .any(|&(s, e, h)| s < e && h >= lo && h < hi && (s < lo || e > hi))
}

/// Whether `[header_pc, end)` can leave the loop anywhere other than the exit
/// test at `exit_pc` and the back edge at `back_edge_pc`.
///
/// Any leaving branch or switch arm, return, `athrow` or subroutine jump
/// counts. A switch whose table cannot be decoded is answered `true`: the
/// question only ever weakens a trip-count lower bound, so the conservative
/// answer is the one that claims an exit.
///
/// An exception to a handler OUTSIDE the loop is deliberately not an exit
/// here, although the exception-handler page proposed counting it: it is the
/// same event as the exception propagating out of the method, which this scan
/// has never counted for an implicit throw (a bounds or null check, a call)
/// and cannot, since nearly every body can throw. An explicit `athrow` is
/// counted either way, and a handler laid out INSIDE the body that then
/// branches out is an ordinary leaving branch in `[header_pc, end)`.
fn loop_has_other_exit(
    code: &[u8],
    header_pc: usize,
    end: usize,
    exit_pc: usize,
    back_edge_pc: usize,
) -> bool {
    let inside = |t: usize| t >= header_pc && t < end;
    let mut pc = header_pc;
    while pc < end {
        let op = code[pc];
        if pc != exit_pc && pc != back_edge_pc {
            match op {
                0x99..=0xa7 | 0xc6 | 0xc7 | 0xc8 => {
                    if !bytecode_analysis::offset_branch_target(code, pc).is_some_and(inside) {
                        return true;
                    }
                }
                0xaa | 0xab => match bytecode_analysis::switch_table(code, code.len(), pc) {
                    Some(table) if table.targets().all(inside) => {}
                    _ => return true,
                },
                0xa8 | 0xa9 | 0xc9 | 0xac..=0xb1 | 0xbf => return true,
                _ => {}
            }
        }
        pc += bytecode_analysis::step(code, pc);
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
///
/// Test-only (round 11 wave 4), as is `analyze_counted_loop_at`: every
/// production caller passes the exception table through the
/// `*_with_handlers` forms.
#[cfg(test)]
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

/// `analyze_counted_loop`, told the method's exception table when the
/// caller has it (see [`analyze_counted_loop_at_with_handlers`]).
pub fn analyze_counted_loop_with_handlers(
    code: &[u8],
    code_len: usize,
    header_pc: usize,
    back_edge_pc: usize,
    form: LoopForm,
    resolve: &dyn Fn(u16) -> Option<MinMax>,
    handlers: Option<&[(usize, usize, usize)]>,
) -> Option<CountedLoop> {
    let tables = LoopMethodTables::new(code, code_len, handlers);
    tables.analyze_counted_loop(header_pc, back_edge_pc, form, resolve)
}

/// `analyze_counted_loop`, also reporting the PC of the exit test it
/// recognised. Test-only (round 11 wave 4); production calls
/// [`analyze_counted_loop_at_with_handlers`].
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
#[cfg(test)]
pub fn analyze_counted_loop_at(
    code: &[u8],
    code_len: usize,
    header_pc: usize,
    back_edge_pc: usize,
    form: LoopForm,
    resolve: &dyn Fn(u16) -> Option<MinMax>,
) -> Option<(CountedLoop, usize)> {
    analyze_counted_loop_at_with_handlers(
        code,
        code_len,
        header_pc,
        back_edge_pc,
        form,
        resolve,
        None,
    )
}

/// `analyze_counted_loop_at`, told the method's exception table
/// (`(start_pc, end_pc, handler_pc)`, `end_pc` exclusive) when the caller has
/// it.
///
/// The "every path" claims this recognition makes -- the entry value, the
/// once-per-iteration advance -- used to be argued over explicit branches only,
/// and an exception handler entry is a path with no branch. They are now
/// argued over the handler entries too:
///
/// * a loop that an exception can ENTER from outside (a handler inside
///   `[header_pc, end)` whose protected range is not wholly inside it) is
///   refused outright: that entry bypasses the header, so neither the exit
///   test's dominance nor the IV's entry value holds on it;
/// * [`constant_iv_init_with_handlers`] refuses a constant entry value a
///   handler can bypass;
/// * the advance must run once per iteration along exception edges as well.
///
/// `None` takes [`handler_entries`]' conservative reading: code no normal
/// edge reaches may be a handler for anything.
pub fn analyze_counted_loop_at_with_handlers(
    code: &[u8],
    code_len: usize,
    header_pc: usize,
    back_edge_pc: usize,
    form: LoopForm,
    resolve: &dyn Fn(u16) -> Option<MinMax>,
    handlers: Option<&[(usize, usize, usize)]>,
) -> Option<(CountedLoop, usize)> {
    let tables = LoopMethodTables::new(code, code_len, handlers);
    tables.analyze_counted_loop_at(header_pc, back_edge_pc, form, resolve)
}

/// [`analyze_counted_loop_at_with_handlers`] against a method's
/// [`LoopMethodTables`].
fn analyze_counted_loop_in(
    tables: &LoopMethodTables<'_>,
    header_pc: usize,
    back_edge_pc: usize,
    form: LoopForm,
    resolve: &dyn Fn(u16) -> Option<MinMax>,
    hoisted: &dyn Fn(usize) -> Option<HoistedFieldSite>,
) -> Option<(CountedLoop, usize)> {
    let code = tables.code;
    let code_len = tables.code_len;
    if header_pc >= code_len || back_edge_pc >= code_len {
        return None;
    }
    let end = (back_edge_pc + bytecode_analysis::step(code, back_edge_pc)).min(code_len);
    if header_pc >= end {
        return None;
    }
    let entries = tables.entries.as_slice();
    if handler_enters_from_outside(entries, header_pc, end) {
        return None;
    }
    let modified_locals = modified_locals_strict(code, header_pc, end)?;
    let heap_stable = body_is_heap_stable(code, header_pc, end);

    let mut pc = header_pc;
    while pc < end {
        if let Some(candidate) = iload_local(code, pc, end) {
            let after = pc + iload_len(code, pc);
            if let Some((bound, q)) =
                decode_bound_expr_with_hoisted_fields(code, after, end, resolve, hoisted)
            {
                // `iload x; <bound>; if_icmp*` is the test of `x` only when
                // nothing jumps in after the `iload`. A rotated
                // `while ((c ? i : j) < n)` lays out `…iload i; goto M; L:
                // iload j; M: iload n; if_icmplt BODY`, and reading the `iload
                // j` triple as the exit test claims a bound on `j` that the
                // `c` path never tests (round 11).
                if q < end && branch_lands_in(code, header_pc, end, pc, q) {
                    pc += iload_len(code, pc);
                    continue;
                }
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
                    //
                    // WHICH edge leaves also fixes the polarity. `ExitCmp` is
                    // exit-when-true; a branch whose TAKEN edge stays inside and
                    // whose fall-through leaves (the rotated `if_icmplt body`
                    // back edge) exits when its test is FALSE, so its opcode
                    // must be negated. Mapping it straight through
                    // `from_opcode` described `for (i = 0; i < n; i++)` rotated
                    // as a decreasing, inclusive loop — refused only by the
                    // luck of a direction mismatch, and outright wrong for a
                    // loop whose stride agrees with the inverted reading.
                    // (`x64/bce.rs` re-derives and negates this itself; every
                    // other consumer took the raw opcode.)
                    let (leaves_loop, taken_exits) = if q + 2 < code.len() {
                        let off = i16::from_be_bytes([code[q + 1], code[q + 2]]) as i32;
                        let taken = q as i32 + off;
                        let fallthrough = (q + 3) as i32;
                        let outside = |t: i32| t < header_pc as i32 || t >= end as i32;
                        (outside(taken) || outside(fallthrough), outside(taken))
                    } else {
                        (false, false)
                    };
                    if !leaves_loop {
                        pc += iload_len(code, pc);
                        continue;
                    }
                    let exit_cmp =
                        ExitCmp::from_opcode(code[q])
                            .map(|c| if taken_exits { c } else { c.negate() });
                    if let Some(cmp) = exit_cmp {
                        // The span every consumer reads ("each iteration
                        // passed the test") needs the test on EVERY path from
                        // the header back to it. A test at the header is; a
                        // test at the back edge (the rotated layout) is not
                        // when a second back edge skips it -- ecj's `while (i
                        // < a.length) { do { a[i] = 0; i++; } while (p); }`
                        // shares the header between the two loops, and the
                        // `do` edge walks `i` past the length with the outer
                        // test never run (round 11 wave 8).
                        if !exit_test_on_every_iteration(code, header_pc, end, q, entries) {
                            pc += iload_len(code, pc);
                            continue;
                        }
                        if let Some(stride) =
                            find_iv_stride_in(code, header_pc, end, candidate, entries)
                        {
                            let init = constant_iv_init_in(tables, header_pc, end, candidate)
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
        pc += bytecode_analysis::step(code, pc);
    }
    None
}

// ---------------------------------------------------------------------------
// Tests — verify the analysis identifies loops and invariants the
// expected way. Hoisting consumer tests live with the (future) emitter.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modified_locals_finds_iinc_and_stores() {
        // istore_1 ; iinc 2, 1 ; istore_3 ; nop
        let code: Vec<u8> = vec![0x3c, 0x84, 0x02, 0x01, 0x3e, 0x00];
        let m = modified_locals_strict(&code, 0, code.len()).unwrap_or(u64::MAX);
        // bits 1, 2, 3 set
        assert_eq!(m, 0b1110);
    }

    /// A `wide istore` decodes as the opcode `0xc4`, which the old scanner's
    /// `_` arm marked nothing for — and `step` then walked past the whole
    /// four-byte instruction, so the write was simply absent from the set and
    /// the local read as loop-invariant.
    #[test]
    fn a_wide_store_is_not_invisible_to_the_modified_set() {
        // wide istore 5 ; nop
        let code: Vec<u8> = vec![0xc4, 0x36, 0x00, 0x05, 0x00];
        let m = modified_locals_strict(&code, 0, code.len()).unwrap_or(u64::MAX);
        assert_eq!(m & (1 << 5), 1 << 5, "wide istore 5 writes local 5");

        // wide iinc 6, 1 ; nop  — six bytes, and the same hole.
        let code: Vec<u8> = vec![0xc4, 0x84, 0x00, 0x06, 0x00, 0x01, 0x00];
        let m = modified_locals_strict(&code, 0, code.len()).unwrap_or(u64::MAX);
        assert_eq!(m & (1 << 6), 1 << 6, "wide iinc 6 writes local 6");
    }

    /// An `lstore_1` writes slots 1 AND 2. Marking only the named slot leaves
    /// the dead high half looking untouched, which is the gap
    /// `x64/escape_analysis.rs` carries a forty-line comment about having
    /// caused a real unsound hoist.
    #[test]
    fn a_long_store_marks_the_dead_high_half() {
        // lstore_1 ; nop
        let code: Vec<u8> = vec![0x40, 0x00];
        let m = modified_locals_strict(&code, 0, code.len()).unwrap_or(u64::MAX);
        assert_eq!(m & 0b110, 0b110, "lstore_1 occupies locals 1 and 2");

        // dstore 4 ; nop — the two-byte form, same rule.
        let code: Vec<u8> = vec![0x39, 0x04, 0x00];
        let m = modified_locals_strict(&code, 0, code.len()).unwrap_or(u64::MAX);
        assert_eq!(m & 0b11_0000, 0b11_0000, "dstore 4 occupies locals 4 and 5");
    }

    /// A store this scan cannot represent is refused (`None`), never reported
    /// as "no locals modified".
    #[test]
    fn a_store_above_the_mask_is_refused() {
        // wide astore 100 — above bit 63, so the set cannot hold it.
        let code: Vec<u8> = vec![0xc4, 0x3a, 0x00, 0x64, 0x00];
        assert_eq!(
            modified_locals_strict(&code, 0, code.len()),
            None,
            "a write the mask cannot represent blocks every hoist"
        );
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
        assert_eq!(bytecode_analysis::step(&code, 1), 27);
        // And a linear scan from PC 0 lands exactly on the end, never
        // mis-decoding a padding/offset byte as an opcode.
        let mut pc = 0;
        let mut steps = 0;
        while pc < code.len() {
            pc += bytecode_analysis::step(&code, pc);
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
        assert_eq!(bytecode_analysis::step(&code, 0), 28);
    }

    #[test]
    fn inst_len_truncated_switch_is_safe() {
        // A bare tableswitch opcode with no room for the header must
        // not read out of bounds; it falls back to a 1-byte step.
        let code: Vec<u8> = vec![0xaa, 0x00, 0x00];
        assert_eq!(bytecode_analysis::step(&code, 0), 1);
        // Likewise a malformed lookupswitch with negative npairs.
        let mut bad: Vec<u8> = vec![0xab, 0x00, 0x00, 0x00];
        bad.extend_from_slice(&0i32.to_be_bytes()); // default
        bad.extend_from_slice(&(-5i32).to_be_bytes()); // npairs < 0
        assert_eq!(bytecode_analysis::step(&bad, 0), 1);
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

    /// Round 13 wave 3 (lane loopcall): an `aload R; getfield F` site a loop
    /// pre-header hoisted reads the slot, named by a synthetic local; every
    /// other site decodes exactly as before.
    #[test]
    fn a_hoisted_field_site_decodes_as_its_synthetic_local() {
        let array_at_0 = |pc: usize| (pc == 0).then_some(HoistedFieldSite::Array(9));
        let int_at_0 = |pc: usize| (pc == 0).then_some(HoistedFieldSite::Int(9));
        let nothing = |_: usize| None;
        let field_7 = BoundSource::Field {
            cp_index: 7,
            receiver_local: Some(0),
        };
        // `aload_0; getfield #7; arraylength`, the array field hoisted.
        let len = [0x2a, 0xb4, 0x00, 0x07, 0xbe];
        assert_eq!(
            decode_bound_expr_with_hoisted_fields(&len, 0, 5, &no_math, &array_at_0),
            Some((BoundSource::ArrayLength(9), 5))
        );
        // Not hoisted: the historical reading, which stops at the `arraylength`
        // (so no exit test is ever read off this shape).
        assert_eq!(
            decode_bound_expr_with_hoisted_fields(&len, 0, 5, &no_math, &nothing),
            Some((field_7.clone(), 4))
        );
        assert_eq!(
            decode_bound_expr(&len, 0, 5, &no_math),
            Some((field_7.clone(), 4))
        );
        // The `arraylength` past `end` is not part of the expression.
        assert_eq!(
            decode_bound_expr_with_hoisted_fields(&len, 0, 4, &no_math, &array_at_0),
            Some((field_7.clone(), 4))
        );
        // An array site with no `arraylength` after it is not a length; a
        // hoisted `int` field is its slot's local.
        let size = [0x2a, 0xb4, 0x00, 0x07, 0x00];
        assert_eq!(
            decode_bound_expr_with_hoisted_fields(&size, 0, 5, &no_math, &array_at_0),
            Some((field_7.clone(), 4))
        );
        assert_eq!(
            decode_bound_expr_with_hoisted_fields(&size, 0, 4, &no_math, &int_at_0),
            Some((BoundSource::Local(9), 4))
        );
        // Keyed by the `aload`'s own pc: the same bytes one pc later are not
        // the hoisted site.
        let shifted = [0x00, 0x2a, 0xb4, 0x00, 0x07];
        assert_eq!(
            decode_bound_expr_with_hoisted_fields(&shifted, 1, 5, &no_math, &int_at_0),
            Some((field_7, 5))
        );
        // The two-byte `aload 5` receiver form.
        let recv5 = [0x19, 0x05, 0xb4, 0x00, 0x07];
        assert_eq!(
            decode_bound_expr_with_hoisted_fields(&recv5, 0, 5, &no_math, &int_at_0),
            Some((BoundSource::Local(9), 5))
        );
        // Inside `Math.min(16, this.size)`.
        let min = [0x10, 0x10, 0x2a, 0xb4, 0x00, 0x07, 0xb8, 0x00, 0x2a];
        let int_at_2 = |pc: usize| (pc == 2).then_some(HoistedFieldSite::Int(9));
        assert_eq!(
            decode_bound_expr_with_hoisted_fields(&min, 0, 9, &resolver, &int_at_2),
            Some((
                BoundSource::Min(
                    Box::new(BoundSource::Const(16)),
                    Box::new(BoundSource::Local(9))
                ),
                9
            ))
        );
    }

    /// `int sum() { int s = 0; for (int i = 0; i < data.length; i++) s += data[i]; return s; }`
    /// (`this` = 0, `s` = 1, `i` = 2; the X2 fixture of `x64/tests.rs`). With
    /// both `aload_0; getfield data` sites hoisted into one slot (synthetic
    /// local 9) the loop is counted over `ArrayLength(9)`, and `data[i]` needs
    /// no guard at all; without the hoist it is not a counted loop.
    #[test]
    fn a_hoisted_field_length_bound_is_a_counted_loop_over_the_slot() {
        let code: Vec<u8> = vec![
            0x03, 0x3c, 0x03, 0x3d, // 0: s = 0; i = 0
            0x1c, // 4: iload_2        <- header
            0x2a, 0xb4, 0x00, 0x01, 0xbe, // 5: aload_0; getfield data; arraylength
            0xa2, 0x00, 0x12, // 10: if_icmpge -> 28
            0x1b, 0x2a, 0xb4, 0x00, 0x01, 0x1c, 0x2e, 0x60, 0x3c, // 13: s += data[i]
            0x84, 0x02, 0x01, // 22: iinc 2, 1
            0xa7, 0xff, 0xeb, // 25: goto -> 4
            0x1b, 0xac, // 28: return s
        ];
        let t = LoopMethodTables::new(&code, code.len(), Some(&[]));
        assert!(
            t.analyze_counted_loop(4, 25, LoopForm::PreTested, &no_math)
                .is_none(),
            "unhoisted, `data.length` of a field is no limit the recogniser reads"
        );
        let hoisted = |pc: usize| matches!(pc, 5 | 14).then_some(HoistedFieldSite::Array(9));
        let l = t
            .analyze_counted_loop_with_hoisted_fields(4, 25, LoopForm::PreTested, &no_math, &hoisted)
            .expect("counted loop over the hoisted slot");
        assert_eq!(l.iv.local, 2);
        assert_eq!(l.iv.init, IntRange::constant(0));
        assert_eq!(l.cmp, ExitCmp::Ge);
        assert_eq!(l.bound, BoundSource::ArrayLength(9));
        assert_eq!(
            l.prove_index_in_bounds_of(
                &IndexExpr::identity(2),
                Some(9),
                IntRange::array_length(),
                &RangeEnv::new()
            ),
            BoundsProof::Static
        );
        // A different array (a real local) does not inherit the tautology.
        assert!(!matches!(
            l.prove_index_in_bounds_of(
                &IndexExpr::identity(2),
                Some(0),
                IntRange::array_length(),
                &RangeEnv::new()
            ),
            BoundsProof::Static
        ));
    }

    /// `for (int i = 0; i < size; i++) s += data[i];` with both fields hoisted:
    /// `size` (pc 5) into synthetic local 10, `data` (pc 13) into 9. The limit
    /// is the `int` slot's local, so `data[i]` needs the pre-header guard
    /// `data.length >= size` -- which the emitter can now load -- and nothing
    /// is static.
    #[test]
    fn a_hoisted_int_field_bound_is_the_slot_local() {
        let code: Vec<u8> = vec![
            0x03, 0x3c, 0x03, 0x3d, // 0: s = 0; i = 0
            0x1c, // 4: iload_2        <- header
            0x2a, 0xb4, 0x00, 0x02, // 5: aload_0; getfield size
            0xa2, 0x00, 0x12, // 9: if_icmpge -> 27
            0x1b, 0x2a, 0xb4, 0x00, 0x01, 0x1c, 0x2e, 0x60, 0x3c, // 12: s += data[i]
            0x84, 0x02, 0x01, // 21: iinc 2, 1
            0xa7, 0xff, 0xec, // 24: goto -> 4
            0x1b, 0xac, // 27: return s
        ];
        let t = LoopMethodTables::new(&code, code.len(), Some(&[]));
        let unhoisted = t
            .analyze_counted_loop(4, 24, LoopForm::PreTested, &no_math)
            .expect("a field limit is a counted loop");
        assert_eq!(
            unhoisted.bound,
            BoundSource::Field {
                cp_index: 2,
                receiver_local: Some(0)
            }
        );
        let hoisted = |pc: usize| match pc {
            5 => Some(HoistedFieldSite::Int(10)),
            13 => Some(HoistedFieldSite::Array(9)),
            _ => None,
        };
        let l = t
            .analyze_counted_loop_with_hoisted_fields(4, 24, LoopForm::PreTested, &no_math, &hoisted)
            .expect("counted loop over the hoisted limit");
        assert_eq!(l.bound, BoundSource::Local(10));
        assert_eq!(l.modified_locals & (1 << 10), 0, "nothing stores a synthetic local");
        assert!(matches!(
            l.prove_index_in_bounds_of(
                &IndexExpr::identity(2),
                Some(9),
                IntRange::array_length(),
                &RangeEnv::new()
            ),
            BoundsProof::Guarded(_)
        ));
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

    // -- round 9 (irsched lane) --------------------------------------------

    /// `void f(int[] a, int i, boolean c) { if (c) i = 0; for (; i < a.length;
    /// i++) ... }` — `i` is a parameter, and the `ifeq` jumps to the header
    /// PAST the only store. The entry value is not 0.
    ///
    /// ```text
    ///  0: iload_2   1: ifeq -> 6   4: iconst_0   5: istore_1
    ///  6: iload_1 (header)   7: aload_0   8: arraylength   9: if_icmpge -> 18
    /// 12: iinc 1,1   15: goto -> 6   18: return
    /// ```
    fn param_bypass_loop(with_bypass: bool) -> Vec<u8> {
        let mut code = vec![
            0x1c, 0x99, 0x00, 0x05, 0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01,
            0x01, 0xa7, 0xff, 0xf7, 0xb1,
        ];
        if !with_bypass {
            code[1..4].fill(0x00);
        }
        code
    }

    #[test]
    fn a_forward_branch_past_the_init_store_unproves_the_constant_init() {
        let code = param_bypass_loop(true);
        assert_eq!(constant_iv_init(&code, code.len(), 6, 18, 1), None);
        let l = analyze_counted_loop(&code, code.len(), 6, 15, LoopForm::PreTested, &no_math)
            .expect("still a counted loop");
        assert!(
            l.iv.init.is_unknown(),
            "the entry value is a runtime quantity"
        );
        // MUST ACCEPT twin: without the bypass the store runs on every entry.
        let straight = param_bypass_loop(false);
        assert_eq!(
            constant_iv_init(&straight, straight.len(), 6, 18, 1),
            Some(0)
        );
    }

    /// javac's rotated loop: `goto cond` enters at the test, and the test is
    /// the back edge, branching back into the body to CONTINUE.
    ///
    /// ```text
    ///  0: iconst_0   1: istore_1   2: goto -> 8
    ///  5: iinc 1,1 (header)   8: iload_1   9: aload_0   10: arraylength
    /// 11: if_icmplt -> 5   14: return
    /// ```
    fn rotated_loop() -> Vec<u8> {
        vec![
            0x03, 0x3c, 0xa7, 0x00, 0x06, 0x84, 0x01, 0x01, 0x1b, 0x2a, 0xbe, 0xa1, 0xff, 0xfa,
            0xb1,
        ]
    }

    #[test]
    fn a_continue_branch_exit_test_is_negated_into_exit_polarity() {
        let code = rotated_loop();
        let (l, test_pc) =
            analyze_counted_loop_at(&code, code.len(), 5, 11, LoopForm::PreTested, &no_math)
                .expect("rotated counted loop");
        assert_eq!(test_pc, 8);
        // `if_icmplt body` continues while `i < a.length`: exits on `>=`.
        assert_eq!(l.cmp, ExitCmp::Ge);
        assert!(!l.is_inclusive());
        assert_eq!(l.iv.stride, Stride::Const(1));
        // The entry `goto` is the only way in, and it runs after the store.
        assert_eq!(l.iv.init, IntRange::constant(0));
    }

    #[test]
    fn an_advance_inside_an_inner_loop_is_not_a_per_iteration_stride() {
        //  0: iload_1  1: iload_2  2: if_icmpge -> 15  5: iinc 1,1
        //  8: iload_3  9: ifne -> 5  12: goto -> 0  15: return
        let mut code = vec![
            0x1b, 0x1c, 0xa2, 0x00, 0x0d, 0x84, 0x01, 0x01, 0x1d, 0x9a, 0xff, 0xfc, 0xa7, 0xff,
            0xf4, 0xb1,
        ];
        assert_eq!(find_iv_stride(&code, 0, 15, 1), None);
        // MUST ACCEPT twin: without the inner back edge it is one advance.
        code[9..12].fill(0x00);
        assert_eq!(find_iv_stride(&code, 0, 15, 1), Some(Stride::Const(1)));
    }

    #[test]
    fn an_advance_a_continue_can_skip_is_not_a_per_iteration_stride() {
        //  0: iload_1  1: iload_2  2: if_icmpge -> 15  5: iload_3
        //  6: ifeq -> 0 (skips the iinc)  9: iinc 1,1  12: goto -> 0  15: return
        let mut code = vec![
            0x1b, 0x1c, 0xa2, 0x00, 0x0d, 0x1d, 0x99, 0xff, 0xfa, 0x84, 0x01, 0x01, 0xa7, 0xff,
            0xf4, 0xb1,
        ];
        assert_eq!(find_iv_stride(&code, 0, 15, 1), None);
        // MUST ACCEPT twin: a branch forward onto the iinc (javac's `continue`).
        // 6: ifeq -> 9
        code[7] = 0x00;
        code[8] = 0x03;
        assert_eq!(find_iv_stride(&code, 0, 15, 1), Some(Stride::Const(1)));
    }

    // -- round 9 wave 2 (sched lane): exception-handler entries -------------

    /// `for (i = 0; i < 10; i++) {}`, laid out pre-tested.
    ///
    /// ```text
    ///  0: iconst_0   1: istore_1 (the init store)
    ///  2: iload_1 (header)   3: bipush 10   5: if_icmpge -> 14
    ///  8: iinc 1,1   11: goto -> 2   14: return
    /// ```
    fn counted_ten() -> Vec<u8> {
        vec![
            0x03, 0x3c, 0x1b, 0x10, 0x0a, 0xa2, 0x00, 0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
            0xb1,
        ]
    }

    /// A handler inside the loop whose protected range covers code BEFORE the
    /// init store enters the loop without the store having run.
    #[test]
    fn a_handler_that_bypasses_the_init_store_unproves_the_constant_init() {
        let code = counted_ten();
        let n = code.len();
        // Control: no handlers.
        assert_eq!(
            constant_iv_init_with_handlers(&code, n, 2, 14, 1, Some(&[])),
            Some(0)
        );
        // The handler at pc 8 protects `[0, 1)`: an exception there reaches
        // the body with the caller's `i`.
        assert_eq!(
            constant_iv_init_with_handlers(&code, n, 2, 14, 1, Some(&[(0, 1, 8)])),
            None
        );
        // MUST ACCEPT twin: a handler protecting only in-loop code runs after
        // the store.
        assert_eq!(
            constant_iv_init_with_handlers(&code, n, 2, 14, 1, Some(&[(2, 8, 8)])),
            Some(0)
        );
        // And the whole recognition refuses a loop an exception can enter
        // from outside.
        assert!(analyze_counted_loop_at_with_handlers(
            &code,
            n,
            2,
            11,
            LoopForm::PreTested,
            &no_math,
            Some(&[(0, 1, 8)]),
        )
        .is_none());
    }

    /// A handler that lands on the header from code ahead of the advance runs
    /// an iteration that moves the IV by zero.
    #[test]
    fn a_handler_that_skips_the_advance_is_not_a_per_iteration_stride() {
        let code = counted_ten();
        let n = code.len();
        let at = |table: &[(usize, usize, usize)]| {
            analyze_counted_loop_at_with_handlers(
                &code,
                n,
                2,
                11,
                LoopForm::PreTested,
                &no_math,
                Some(table),
            )
        };
        // `[3, 8)` precedes the `iinc` at 8 and its handler is the header.
        assert!(at(&[(3, 8, 2)][..]).is_none());
        // MUST ACCEPT twin: the same range handled at pc 5, ahead of the
        // advance, still runs it (javac's in-loop `catch` shape).
        let (l, test_pc) = at(&[(3, 5, 5)][..]).expect("in-loop catch: still counted");
        assert_eq!(test_pc, 2);
        assert_eq!(l.iv.stride, Stride::Const(1));
        assert_eq!(l.iv.init, IntRange::constant(0));
        // A range AFTER the advance handled at or before it makes the advance
        // part of an inner loop.
        assert!(at(&[(11, 14, 8)][..]).is_none());
    }

    /// With no exception table, code no normal edge reaches is a possible
    /// handler for anything, so a loop that contains some is refused -- and
    /// the same loop with the real (empty) table is accepted.
    ///
    /// ```text
    ///  0: iconst_0   1: istore_1   2: iload_1 (header)   3: bipush 10
    ///  5: if_icmpge -> 18   8: goto -> 12   11: nop (unreachable)
    /// 12: iinc 1,1   15: goto -> 2   18: return
    /// ```
    #[test]
    fn without_a_table_unreachable_code_in_the_loop_is_a_possible_handler() {
        let code: Vec<u8> = vec![
            0x03, 0x3c, 0x1b, 0x10, 0x0a, 0xa2, 0x00, 0x0d, 0xa7, 0x00, 0x04, 0x00, 0x84, 0x01,
            0x01, 0xa7, 0xff, 0xf3, 0xb1,
        ];
        let n = code.len();
        assert!(
            analyze_counted_loop_at(&code, n, 2, 15, LoopForm::PreTested, &no_math).is_none(),
            "no table: the unreachable pc 11 may be a handler entry"
        );
        let (l, _) = analyze_counted_loop_at_with_handlers(
            &code,
            n,
            2,
            15,
            LoopForm::PreTested,
            &no_math,
            Some(&[]),
        )
        .expect("the real table says there is no handler");
        assert_eq!(l.iv.init, IntRange::constant(0));
        // The synthetic entries are exactly the unreachable instruction.
        assert_eq!(handler_entries(&code, n, None), vec![(0, n, 11)]);
    }

    /// Round 11: `for (int i = 0; i < a.length; i = back ? i - 1 : i + 1)
    /// a[i] = 7;` — javac's output, whose `istore` is a join of an `isub` arm
    /// and an `iadd` arm. The textual tail `iload i; iconst_1; iadd; istore i`
    /// read as `+1`, and BCE removed `a[i]`'s check for a loop that walks
    /// below zero.
    ///
    /// ```text
    ///  0: iconst_0  1: istore_2  2: iload_2  3: aload_0  4: arraylength
    ///  5: if_icmpge -> 30  8: aload_0  9: iload_2  10: bipush 7  12: iastore
    /// 13: iload_1  14: ifeq -> 23  17: iload_2  18: iconst_1  19: isub
    /// 20: goto -> 26  23: iload_2  24: iconst_1  25: iadd  26: istore_2
    /// 27: goto -> 2  30: return
    /// ```
    #[test]
    fn a_ternary_advance_is_not_a_constant_stride() {
        let mut code: Vec<u8> = vec![
            0x03, 0x3d, 0x1c, 0x2a, 0xbe, 0xa2, 0x00, 0x19, 0x2a, 0x1c, 0x10, 0x07, 0x4f, 0x1b,
            0x99, 0x00, 0x09, 0x1c, 0x04, 0x64, 0xa7, 0x00, 0x06, 0x1c, 0x04, 0x60, 0x3d, 0xa7,
            0xff, 0xe7, 0xb1,
        ];
        assert_eq!(code.len(), 31);
        assert_eq!(find_iv_stride(&code, 2, 30, 2), None);
        // MUST ACCEPT twin: no ternary, so nothing lands inside the advance.
        code[13..23].fill(0x00);
        assert_eq!(find_iv_stride(&code, 2, 30, 2), Some(Stride::Const(1)));
    }

    /// Round 11: javac reuses one slot for sibling loops.
    ///
    /// ```text
    ///  0: iconst_0  1: istore_1  2: iload_1  3: aload_0  4: arraylength
    ///  5: if_icmpge -> 14  8: iinc 1,1  11: goto -> 2
    /// 14: iconst_0  15: istore_1  16: iload_1  17: aload_0  18: arraylength
    /// 19: if_icmpge -> 28  22: iinc 1,1  25: goto -> 16  28: return
    /// ```
    ///
    /// Two out-of-body stores (and each loop's `iinc` outside the other) made
    /// BOTH entry values unknown; and the first loop's exit lands on the
    /// second init's push.
    #[test]
    fn sibling_loops_sharing_a_slot_each_keep_their_constant_init() {
        let code: Vec<u8> = vec![
            0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
            0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
            0xb1,
        ];
        let n = code.len();
        assert_eq!(n, 29);
        assert_eq!(constant_iv_init(&code, n, 2, 14, 1), Some(0));
        assert_eq!(constant_iv_init(&code, n, 16, 28, 1), Some(0));
        // A branch landing on the STORE (past the push) still refuses: turn
        // the first loop's exit into `if_icmpge -> 15`.
        let mut skip_push = code.clone();
        skip_push[7] = 0x0a;
        assert_eq!(constant_iv_init(&skip_push, n, 16, 28, 1), None);
    }

    /// Round 11: a rotated `while ((c ? i : j) < a.length) { a[j] = 0; j++; }`
    /// (ecj's layout). The `iload j` triple is entered at the bound by the `i`
    /// arm's `goto`, so it is not the test of `j`.
    ///
    /// ```text
    ///  0: iconst_0  1: istore_3  2: goto -> 12
    ///  5: aload_0  6: iload_3  7: iconst_0  8: iastore  9: iinc 3,1
    /// 12: iload_1  13: ifeq -> 20  16: iload_2  17: goto -> 21
    /// 20: iload_3  21: aload_0  22: arraylength  23: if_icmplt -> 5
    /// 26: return
    /// ```
    #[test]
    fn an_exit_test_entered_past_its_iload_is_not_that_locals_test() {
        let mut code: Vec<u8> = vec![
            0x03, 0x3e, 0xa7, 0x00, 0x0a, 0x2a, 0x1d, 0x03, 0x4f, 0x84, 0x03, 0x01, 0x1b, 0x99,
            0x00, 0x07, 0x1c, 0xa7, 0x00, 0x04, 0x1d, 0x2a, 0xbe, 0xa1, 0xff, 0xee, 0xb1,
        ];
        assert_eq!(code.len(), 27);
        let n = code.len();
        assert!(
            analyze_counted_loop_at(&code, n, 5, 23, LoopForm::PreTested, &no_math).is_none(),
            "the tested value is `c ? i : j`, not `j`"
        );
        // MUST ACCEPT twin: `while (j < a.length)`.
        code[12..20].fill(0x00);
        let (l, test_pc) = analyze_counted_loop_at(&code, n, 5, 23, LoopForm::PreTested, &no_math)
            .expect("plain rotated counted loop");
        assert_eq!(test_pc, 20);
        assert_eq!(l.iv.local, 3);
        assert_eq!(l.iv.stride, Stride::Const(1));
    }
}

// ── Round 11 wave 8 (lane mid) ───────────────────────────────────────

#[cfg(test)]
mod r11w8_mid_tests {
    use super::*;

    fn no_math(_: u16) -> Option<MinMax> {
        None
    }

    /// `(header, back_edge)` of `code`, pre-tested, with the exception table
    /// `table`.
    fn counted(
        code: &[u8],
        header: usize,
        back_edge: usize,
        table: &[(usize, usize, usize)],
    ) -> Option<(CountedLoop, usize)> {
        analyze_counted_loop_at_with_handlers(
            code,
            code.len(),
            header,
            back_edge,
            LoopForm::PreTested,
            &no_math,
            Some(table),
        )
    }

    /// `int i = 0; while (i < a.length) { if (a[i] >= 0) { i++; continue; }
    /// i++; }` -- one advance per path, and two back edges.
    ///
    /// ```text
    ///  0: iconst_0   1: istore_1   2: iload_1 (header)   3: aload_0
    ///  4: arraylength   5: if_icmpge -> 26   8: aload_0   9: iload_1
    /// 10: iaload   11: ifge -> 20   14: iinc 1,1   17: goto -> 2
    /// 20: iinc 1,1   23: goto -> 2   26: return
    /// ```
    fn continue_loop() -> Vec<u8> {
        vec![
            0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x15, 0x2a, 0x1b, 0x2e, 0x9c, 0x00, 0x09,
            0x84, 0x01, 0x01, 0xa7, 0xff, 0xf1, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xeb, 0xb1,
        ]
    }

    /// The loop merged per header (its farthest back edge, 23) is counted:
    /// every path runs one `iinc`, and the entry value is the constant.
    #[test]
    fn a_continue_that_advances_is_one_advance_per_iteration() {
        let code = continue_loop();
        assert_eq!(code.len(), 27);
        let (l, test_pc) = counted(&code, 2, 23, &[]).expect("merged continue loop is counted");
        assert_eq!(test_pc, 2);
        assert_eq!(l.iv.local, 1);
        assert_eq!(l.iv.stride, Stride::Const(1));
        assert_eq!(l.iv.init, IntRange::constant(0));
        assert_eq!(l.cmp, ExitCmp::Ge);
        assert_eq!(l.bound, BoundSource::ArrayLength(0));
        assert!(!l.has_other_exit, "the continue path stays in the loop");
        // Per pair, `(2, 17)` still reads the second back edge as a side entry
        // onto the header, so its entry value is unknown.
        let (per_pair, _) = counted(&code, 2, 17, &[]).expect("per-pair loop");
        assert!(per_pair.iv.init.is_unknown());
    }

    /// A path that runs BOTH advances (the `continue` removed, so the first
    /// arm falls into the second) and advances of different strides are not a
    /// per-iteration stride.
    #[test]
    fn two_advances_on_one_path_or_of_two_strides_are_refused() {
        let mut both = continue_loop();
        both[17..20].fill(0x00);
        assert!(counted(&both, 2, 23, &[]).is_none());
        let mut strides = continue_loop();
        strides[22] = 0x02;
        assert!(counted(&strides, 2, 23, &[]).is_none());
    }

    /// `if (c) i++; else i++;` with one back edge.
    ///
    /// ```text
    ///  0: iconst_0   1: istore_1   2: iload_1 (header)   3: iload_2
    ///  4: if_icmpge -> 23   7: iload_3   8: ifeq -> 17   11: iinc 1,1
    /// 14: goto -> 20   17: iinc 1,1   20: goto -> 2   23: return
    /// ```
    #[test]
    fn an_advance_in_each_arm_of_an_if_is_one_per_iteration() {
        let code: Vec<u8> = vec![
            0x03, 0x3c, 0x1b, 0x1c, 0xa2, 0x00, 0x13, 0x1d, 0x99, 0x00, 0x09, 0x84, 0x01, 0x01,
            0xa7, 0x00, 0x06, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xee, 0xb1,
        ];
        assert_eq!(code.len(), 24);
        let (l, _) = counted(&code, 2, 20, &[]).expect("counted");
        assert_eq!(l.iv.stride, Stride::Const(1));
        assert_eq!(l.iv.init, IntRange::constant(0));
    }

    /// Exception edges carry the count too: a handler on the header for code
    /// ahead of both advances runs an iteration with none; one landing on the
    /// second arm from the first arm's `iinc` (count 0 before it) is fine.
    #[test]
    fn a_handler_edge_is_an_edge_for_the_several_advance_proof() {
        let code = continue_loop();
        assert!(counted(&code, 2, 23, &[(8, 11, 2)]).is_none());
        assert!(counted(&code, 2, 23, &[(14, 17, 20)]).is_some());
        // From the `goto` after the first advance (count 1) to the second arm
        // (count 0): the arm would advance twice.
        assert!(counted(&code, 2, 23, &[(17, 20, 20)]).is_none());
    }

    /// ecj's rotated `int i = 0; while (i < a.length) { do { a[i] = 0; i++; }
    /// while (p); }`: the `do` loop and the `while` loop share the header, and
    /// the `do` back edge re-enters it without the `while` test. Reading the
    /// pair `(5, 19)` as counted by `i < a.length` let `a[i]` lose its check
    /// while `p` walks `i` past the end.
    ///
    /// ```text
    ///  0: iconst_0   1: istore_1   2: goto -> 16
    ///  5: aload_0 (header)   6: iload_1   7: iconst_0   8: iastore
    ///  9: iinc 1,1   12: iload_2   13: ifne -> 5
    /// 16: iload_1   17: aload_0   18: arraylength   19: if_icmplt -> 5
    /// 22: return
    /// ```
    #[test]
    fn a_back_edge_that_skips_a_rotated_test_is_not_counted_by_it() {
        let mut code: Vec<u8> = vec![
            0x03, 0x3c, 0xa7, 0x00, 0x0e, 0x2a, 0x1b, 0x03, 0x4f, 0x84, 0x01, 0x01, 0x1c, 0x9a,
            0xff, 0xf8, 0x1b, 0x2a, 0xbe, 0xa1, 0xff, 0xf2, 0xb1,
        ];
        assert_eq!(code.len(), 23);
        assert!(counted(&code, 5, 19, &[]).is_none());
        // MUST ACCEPT twin: without the `do` edge the test closes every
        // iteration.
        code[12..16].fill(0x00);
        let (l, test_pc) = counted(&code, 5, 19, &[]).expect("rotated counted loop");
        assert_eq!(test_pc, 16);
        assert_eq!(l.iv.init, IntRange::constant(0));
        assert_eq!(l.bound, BoundSource::ArrayLength(0));
    }

    /// One [`LoopMethodTables`] answers exactly what the per-call functions
    /// answer, for every loop of a method.
    #[test]
    fn method_tables_agree_with_the_per_call_functions() {
        // Two sibling loops sharing slot 1 (see the round-11 sibling test).
        let code: Vec<u8> = vec![
            0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
            0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
            0xb1,
        ];
        let n = code.len();
        let no_table: Option<&[(usize, usize, usize)]> = None;
        for handlers in [no_table, Some(&[][..])] {
            let t = LoopMethodTables::new(&code, n, handlers);
            for (h, b, end) in [(2, 11, 14), (16, 25, 28)] {
                let init = t.constant_iv_init(h, end, 1);
                assert_eq!(init, Some(0));
                assert_eq!(
                    init,
                    constant_iv_init_with_handlers(&code, n, h, end, 1, handlers)
                );
                let form = LoopForm::PreTested;
                assert_eq!(
                    t.analyze_counted_loop(h, b, form, &no_math),
                    analyze_counted_loop_with_handlers(&code, n, h, b, form, &no_math, handlers)
                );
            }
        }
        // A `code_len` past the buffer is clamped rather than read.
        let t = LoopMethodTables::new(&code, n + 10, Some(&[]));
        assert_eq!(t.constant_iv_init(2, 14, 1), Some(0));
    }
}
