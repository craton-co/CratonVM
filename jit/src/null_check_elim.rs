// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T5.2.14 — Null-check elimination pass.
//!
//! A forward dataflow analysis that tracks which local variables are
//! known to be non-null at each bytecode PC. When a `getfield`,
//! `invokevirtual`, or `arraylength` instruction references a local
//! that's already proven non-null, the JIT can skip emitting the null
//! check (which is a `TEST reg, reg; JZ throw_npe` sequence on x86-64).
//!
//! ## How it works
//!
//! Round-11 (HIGH-1): proper forward dataflow with meet-over-paths.
//! For every bytecode PC we keep an `IN[pc]` bitmask of locals proven
//! non-null *on entry* and an `OUT[pc]` bitmask on exit. The meet
//! operator at a PC with multiple predecessors is bitwise AND
//! (intersection — a local is only non-null if it was non-null on
//! every incoming path). The transfer function `OUT = transfer(pc,
//! IN)` is defined per opcode:
//!
//! * `aload N` followed by an immediate `getfield/putfield/invoke*/
//!   arraylength/monitorenter/monitorexit/aastore/iastore/baload/...`
//!   — the receiver `N` is non-null on the fall-through, because the
//!   dereference would have raised NPE otherwise.
//! * `new`, `anewarray`, `newarray`, `multianewarray` — produce a
//!   non-null value on top of stack; if the next opcode is
//!   `astore N`, set N's bit.
//! * `aload N` followed by `ifnonnull T` — on the FALL-THROUGH (not-
//!   taken) edge N is null, so clear N's bit. On the TAKEN edge N
//!   is non-null (so the IN[T] mask gets N's bit set).
//! * `aload N` followed by `ifnull T` — symmetric: fall-through is
//!   non-null, taken edge proves null.
//! * `astore N` with no known producer — clears N's bit
//!   (conservative).
//! * `invoke*` / `monitorenter/monitorexit` etc — do NOT clear bits
//!   for locals (they only affect operand stack and heap; locals
//!   keep their values across calls per JVMS).
//!
//! Iteration uses a worklist of PCs seeded with the entry (PC 0)
//! plus all branch targets reachable from a successor walk. We loop
//! until no IN[pc] changes. Convergence is guaranteed because the
//! lattice is finite (u64 bitmasks form a join-semilattice under AND
//! — facts can only be REMOVED across iterations, never added back
//! once removed, so the analysis is monotone descending).
//!
//! ## Limitations
//!
//! - Only tracks the first 64 locals (bitmask is `u64`). Methods
//!   with > 64 locals get no elimination beyond local 63. Covers
//!   > 99% of real methods.
//! - No exception table is consulted. Every instruction that no modelled
//!   edge reaches — an exception handler, a `jsr` subroutine, dead code —
//!   is seeded with IN = 0 (or just the receiver bit, when nothing ever
//!   stores a reference into local 0) once the reachable fixpoint settles, and the
//!   fixpoint is re-run from those seeds. Seeding (rather than leaving the
//!   code unvisited) is what makes a JOIN sound: a handler that falls
//!   through into shared code must contribute its facts to the meet, or
//!   the join keeps facts only the normal path established.
//! - The per-opcode transfer reads an operand's origin off the TEXTUALLY
//!   preceding instruction. That is only the dataflow predecessor at a PC
//!   nothing can jump to, so every such peephole is skipped at a merge
//!   point, and consumers must do the same (`NullCheckInfo::is_merge_point`).

/// Result of null-check elimination analysis.
///
/// `nonnull_at_pc[i]` is the bitmask of locals known non-null at the
/// start of the instruction at `pc == i`. PCs that don't start an
/// instruction have a bitmask of 0 (no info).
use crate::bytecode_analysis;

#[derive(Default)]
pub struct NullCheckInfo {
    /// Per-PC non-null bitmask. Index = bytecode PC, value = u64 mask.
    masks: Vec<u64>,
    /// `true` at every PC control can reach other than by falling through
    /// from the textually preceding instruction.
    merge_points: Vec<bool>,
}

impl NullCheckInfo {
    /// Whether control can reach `pc` other than by falling through from the
    /// instruction textually before it (a branch, switch or `jsr` target, the
    /// successor of a non-falling-through instruction, or a PC only an
    /// exception edge reaches).
    ///
    /// A consumer that pairs a fact with "the operand on top of the stack came
    /// from the `aload` just before this PC" must refuse at a merge point: the
    /// fact about the local still holds there, but the operand may have been
    /// pushed on another path. Out-of-range PCs, and an empty analysis, answer
    /// `true`, the refusing direction.
    pub fn is_merge_point(&self, pc: usize) -> bool {
        self.merge_points.get(pc).copied().unwrap_or(true)
    }

    /// Returns `true` if `local` is known non-null at the given PC.
    ///
    /// Only valid for `local < 64`. Always returns `false` for
    /// locals ≥ 64 (conservative).
    pub fn is_nonnull(&self, pc: usize, local: usize) -> bool {
        if local >= 64 || pc >= self.masks.len() {
            return false;
        }
        self.masks[pc] & (1u64 << local) != 0
    }

    /// Number of PCs covered.
    pub fn len(&self) -> usize {
        self.masks.len()
    }

    /// Total non-null facts across all PCs (for diagnostics).
    pub fn total_facts(&self) -> usize {
        self.masks.iter().map(|m| m.count_ones() as usize).sum()
    }
}

/// Bytecode opcodes whose execution dereferences the most-recent
/// reference on the operand stack and therefore PROVES that reference
/// non-null on fall-through. Only opcodes where the receiver came from
/// an `aload` immediately preceding the dereference are useful here —
/// the analysis only tracks references that originated in a local.
fn opcode_dereferences_receiver(op: u8) -> bool {
    // Note: `instanceof` (0xC1) does NOT throw NPE — null produces 0 —
    // so it is intentionally NOT in this list. `checkcast` (0xC0) is
    // sometimes documented as "throws CCE on bad type"; JVMS §6.5
    // additionally allows it to succeed-with-null (null can be cast
    // to any reference type), so it does NOT prove non-null either.
    // We list ONLY opcodes that throw NPE on a null receiver AND whose
    // receiver is the value on TOP of the operand stack — so the
    // `aload N` immediately preceding the opcode (per `prev_inst_pc`) is
    // exactly that receiver. The caller keys off a single preceding
    // instruction, so any opcode whose receiver is NOT top-of-stack would
    // attribute the non-null fact to the wrong local.
    //
    // BUG-I FIX: `putfield`, `invokevirtual`, `invokespecial` and
    // `invokeinterface` are intentionally EXCLUDED:
    //   * `putfield` (0xB5) stack is `[..., objectref, value]` — the
    //     preceding push is the stored VALUE, never the receiver.
    //   * `invoke*` (0xB6/0xB7/0xB9) stack is `[..., objectref, arg1..argN]`
    //     — the preceding push is the last ARGUMENT for any non-zero-arg
    //     callee (only a 0-arg callee has the receiver on top, and this
    //     pass has no constant pool to tell the two apart).
    // Listing them marked the stored value / last argument local non-null,
    // a FALSE fact that let the JIT elide a subsequent `local == null`
    // branch. Tomcat `MessageBytes.setString` (`strValue = s; if (s ==
    // null) …`) took the non-null arm for a null argument, so `isNull()`
    // wrongly returned false (`TestMessageBytesConversion.testConversion
    // Null`, 432/864 under JIT); the `invoke*` form has the same shape
    // (`sink.use(arg); if (arg == null) …`). These opcodes still NPE-check
    // their receiver in codegen — we just cannot identify that receiver
    // from the immediately-preceding instruction, so we forgo the fact
    // rather than assert a wrong one. `getfield` is retained: its receiver
    // IS the top-of-stack operand the preceding `aload` pushed.
    matches!(
        op,
        0xB4 | // getfield
        0xBE | // arraylength
        0xC2 | // monitorenter
        0xC3 // monitorexit
    )
}

/// If the instruction at `pc` is an `aload <local>` form, return the
/// local index. Recognises `aload_0..3` and `aload <u8>`.
fn aload_at(code: &[u8], pc: usize) -> Option<usize> {
    if pc >= code.len() {
        return None;
    }
    let op = code[pc];
    match op {
        0x2A..=0x2D => Some((op - 0x2A) as usize),
        0x19 if pc + 1 < code.len() => Some(code[pc + 1] as usize),
        _ => None,
    }
}

/// If the instruction at `pc` is an `astore <local>` form, return the
/// local index. Recognises `astore_0..3`, `astore <u8>` and `wide astore
/// <u16>`.
///
/// The `wide` form is not an optimisation, it is the KILL. `transfer` clears a
/// local's non-null bit only at a pc this function names, so a store it does
/// not recognise leaves the old fact standing: `new; astore_1; ...;
/// aconst_null; wide astore 1; aload_1; ifnull` elided the `ifnull` on a
/// local that had just been nulled. javac only emits `wide` above slot 255,
/// but nothing in the class-file format stops another compiler (or a bytecode
/// rewriter) from spelling a small index that way.
fn astore_at(code: &[u8], pc: usize) -> Option<usize> {
    if pc >= code.len() {
        return None;
    }
    let op = code[pc];
    match op {
        0x4B..=0x4E => Some((op - 0x4B) as usize),
        0x3A if pc + 1 < code.len() => Some(code[pc + 1] as usize),
        // Widening: u16 -> usize (local index)
        0xC4 if pc + 3 < code.len() && code[pc + 1] == 0x3A => {
            Some(u16::from_be_bytes([code[pc + 2], code[pc + 3]]) as usize)
        }
        _ => None,
    }
}

/// Does any instruction store a reference into local 0?
///
/// When it does not, local 0 holds the receiver for the WHOLE activation of an
/// instance method — an `istore_0` over it cannot be followed by a verifiable
/// `aload_0` without an `astore_0` in between — so the receiver seed is a fact
/// about every program point, including the ones this analysis reaches only by
/// seeding (exception handlers, dead code). Decoded, not scanned: `0x4B` is an
/// ordinary operand byte.
fn stores_ref_to_local_zero(code: &[u8], len: usize) -> bool {
    let mut p = 0usize;
    while p < len {
        if astore_at(code, p) == Some(0) {
            return true;
        }
        let l = bytecode_analysis::step(code, p);
        if l == 0 {
            // UNREACHABLE: `bytecode_analysis::step` is
            // `insn_len(..).unwrap_or(1)` and never answers 0 — an undecodable
            // instruction advances ONE BYTE and desyncs the walk rather than
            // stalling it. This function is only called from
            // `analyze_with_receiver`, which now refuses the whole method
            // beforehand when `bytecode_analysis::decode_method` cannot decode
            // it exactly, so by the time this walk runs the desync this guard
            // used to (not) catch cannot occur. Kept as a backstop against the
            // `step` contract changing, answering the direction that keeps the
            // receiver seed out of the handlers if it ever fires.
            return true;
        }
        p += l;
    }
    false
}

/// Is the opcode at `pc` one that produces a known-non-null reference
/// on top of stack? (`new`, `anewarray`, `newarray`, `multianewarray`,
/// `aload` of a proven-non-null local — but the last is tracked
/// implicitly via the IN mask, so this only returns true for the
/// allocation opcodes.)
///
/// # `ldc` is NOT on this list, and the comment that used to put it here was
/// # wrong
///
/// The entry read `0x12 | // ldc — string/class literals are non-null (numeric
/// ldc is also OK; never null)`. "Never null" is true of `CONSTANT_String`,
/// `CONSTANT_Class`, `CONSTANT_MethodHandle` and `CONSTANT_MethodType`, and
/// false of `CONSTANT_Dynamic`: JVMS 5.4.3.6 resolves a condy by running its
/// bootstrap method, and the bootstrap may return `null` —
/// `java.lang.invoke.ConstantBootstraps.nullConstant` exists to do exactly
/// that, and is what a `null` default in a record/lambda pattern compiles to.
///
/// The fact fed `transfer`'s `ldc #condy; astore N` rule, which set the local's
/// non-null bit, and `is_local_nonnull` is consumed by the `ifnull`/`ifnonnull`
/// branch elision in `x64/op_control.rs` — so a null condy had its own null
/// test compiled away.
///
/// This module carries no constant pool, so it cannot tell a `CONSTANT_String`
/// from a `CONSTANT_Dynamic` and must assume the latter. `0x14` (`ldc2_w`) goes
/// with them for a different reason: it pushes a category-2 value, which can
/// never be `astore`d, so the entry could only ever have fired on malformed
/// code.
fn produces_nonnull(op: u8) -> bool {
    matches!(
        op,
        0xBB | // new
        0xBC | // newarray
        0xBD | // anewarray
        0xC5 // multianewarray
    )
}

/// Does the method use the pre-JDK-7 subroutine instructions?
///
/// Decoded rather than scanned byte-wise: `0xA8` and `0xA9` are ordinary values
/// for a branch offset or a constant-pool index, and a raw-byte scan would
/// refuse a large fraction of all methods for operands that are not opcodes.
/// The walk is the same linear decode the mask tables below use, so a method
/// whose decode desyncs is one this analysis was already going to get wrong in
/// the same way.
///
/// # That last sentence used to be a dismissal of the hazard; it no longer is
///
/// `bytecode_analysis::step` is `insn_len(..).unwrap_or(1)`: an instruction it
/// cannot decode — a `tableswitch`/`lookupswitch` whose table runs past the end
/// of `code`, which is what `insn_len` answers `None` for — advances ONE BYTE.
/// So `l == 0` below never happens (the `break` is a backstop against that
/// contract changing), and the failure mode is not a stall but a DESYNC: the
/// walk marches through the switch's operand bytes marking each as an
/// instruction start, and `is_inst_start` / `prev_inst_pc` / `merge_points`
/// downstream would describe a bytecode stream that does not exist.
///
/// `analyze_with_receiver` now refuses the whole method, before calling this
/// function, whenever `bytecode_analysis::decode_method` cannot decode it
/// exactly — the same "every instruction lies wholly inside `code[..len]`"
/// check `decode_at` performs. That rules the desync out before this walk (or
/// `stores_ref_to_local_zero`'s, or the `is_inst_start` pre-pass's) ever runs,
/// so the `l == 0` branch below is unreachable both because `step` cannot
/// answer 0 AND because the input that used to make it matter is refused one
/// level up. See
/// `docs/internal/fixed-bugs/r10-gcs-null-check-elim-has-no-refusal-for-a-desynced-decode-FIXED-20260921.md`.
fn uses_subroutines(code: &[u8], len: usize) -> bool {
    let mut p = 0usize;
    while p < len {
        if matches!(code[p], 0xA8 | 0xA9 | 0xC9) {
            return true;
        }
        let l = bytecode_analysis::step(code, p);
        if l == 0 {
            break;
        }
        p += l;
    }
    false
}

/// Compute the (fall-through PC, optional branch target PC) successor
/// pair for the instruction at `pc`. Returns `(None, None)` when the
/// instruction does not return (areturn/ireturn/return/athrow) — i.e.
/// has no successors. Returns `(Some(ft), Some(tgt))` for conditional
/// branches, `(None, Some(tgt))` for `goto`. Tableswitch/lookupswitch
/// are conservatively dropped (no successors recorded — we still cover
/// reachable code from the entry via fall-throughs).
fn successors(code: &[u8], pc: usize) -> (Option<usize>, Option<usize>) {
    if pc >= code.len() {
        return (None, None);
    }
    let op = code[pc];
    let ft = pc + bytecode_analysis::step(code, pc);
    let tgt = bytecode_analysis::offset_branch_target(code, pc);
    match op {
        // return forms and athrow — no in-method successor
        0xAC..=0xB1 | 0xBF => (None, None),
        // goto / goto_w
        0xA7 | 0xC8 => (None, tgt),
        // jsr / jsr_w / ret — pre-JDK-7, treat as fall-through only. Kept as
        // defence in depth: `analyze_with_receiver` refuses any method
        // containing one of these before the walk starts (`uses_subroutines`),
        // because "fall through only" is exactly the model that loses the
        // subroutine's effects at the return site.
        0xA8 | 0xC9 | 0xA9 => (Some(ft), None),
        // tableswitch / lookupswitch — the targets come from
        // `bytecode_analysis::switch_targets_lenient` at the call sites.
        0xAA | 0xAB => (None, None),
        // conditional branches with 16-bit offset
        0x99..=0xA6 | 0xC6 | 0xC7 => (Some(ft), tgt),
        _ => (Some(ft), None),
    }
}

/// Meet `out` into `IN[s]`, queueing `s` when that assigned or narrowed it.
#[allow(clippy::too_many_arguments)]
fn meet_into(
    s: usize,
    out: u64,
    is_inst_start: &[bool],
    in_masks: &mut [u64],
    visited: &mut [bool],
    on_worklist: &mut [bool],
    worklist: &mut Vec<usize>,
) {
    if s >= in_masks.len() || !is_inst_start[s] {
        return;
    }
    let new_in = if visited[s] { in_masks[s] & out } else { out };
    if visited[s] && new_in == in_masks[s] {
        return;
    }
    visited[s] = true;
    in_masks[s] = new_in;
    if !on_worklist[s] {
        on_worklist[s] = true;
        worklist.push(s);
    }
}

/// Compute the `OUT` mask for a basic-block-like step from PC.
/// Returns `(out_fallthrough, out_branch)` masks. The two are
/// usually the same, but `ifnull`/`ifnonnull` discriminate (fall-
/// through vs taken edge changes which side proves non-null).
///
/// `prev_inst_pc[pc]` gives the PC of the bytecode instruction that
/// immediately PRECEDES the instruction starting at `pc`, or
/// `usize::MAX` when none (entry / PC outside any instruction).
fn transfer(
    code: &[u8],
    pc: usize,
    in_mask: u64,
    prev_inst_pc: &[usize],
    merge_points: &[bool],
) -> (u64, u64) {
    if pc >= code.len() {
        return (in_mask, in_mask);
    }
    let op = code[pc];
    let mut out = in_mask;
    // Every peephole below attributes this instruction's operand to the
    // TEXTUAL predecessor. At a merge point that operand may come from any
    // incoming path — `(c ? a : b).x` makes the `getfield` a join of
    // `aload a` and `aload b` while `prev_inst_pc` names only `aload b` — so
    // no fact may be derived from it there.
    let linear_only = !merge_points.get(pc).copied().unwrap_or(true);

    // A dereferencing opcode (`getfield`/`invokevirtual`/`arraylength`/…)
    // whose receiver came from an immediately-preceding `aload N` proves
    // N non-null — but ONLY on the dereference's *fall-through*, i.e. in
    // `OUT[deref_pc]`, never in `IN[deref_pc]`.
    //
    // Soundness note (round-12 fix): the fact "N is non-null" is only
    // established *after* the dereference executes without throwing NPE.
    // The earlier formulation set the bit on `OUT[aload_pc]` — which is
    // `IN[deref_pc]`, the state *before* the dereference. That made the
    // receiver look non-null at the very PC of the dereference, so the
    // JIT's `emit_null_check_arraylength` (which trusts `IN[pc]`) elided
    // the null check on `arr.length` and emitted a raw `MOV [arr+12]`
    // that SIGSEGV'd on a genuinely-null `arr` instead of throwing NPE.
    // Placing the fact on `OUT[deref_pc]` keeps the optimization for any
    // *subsequent* use of N while preserving the NPE at the deref itself.
    if opcode_dereferences_receiver(op) && linear_only {
        let prev_local = prev_inst_pc.get(pc).copied().and_then(|q| {
            if q == usize::MAX {
                return None;
            }
            aload_at(code, q)
        });
        if let Some(local) = prev_local {
            if local < 64 {
                out |= 1u64 << local;
            }
        }
        // Array load/store opcodes (iaload..saload, iastore..sastore)
        // also dereference the array receiver, but the receiver is not
        // the most-recent aload (an index push sits between). They are
        // handled separately by the JIT's inline array null-check stub.
    }

    // `new`/`anewarray`/etc followed by `astore N` sets N non-null.
    if produces_nonnull(op) {
        // Find next instruction after this allocation.
        let next_pc = pc + bytecode_analysis::step(code, pc);
        if next_pc < code.len() {
            if let Some(local) = astore_at(code, next_pc) {
                if local < 64 {
                    out |= 1u64 << local;
                }
            }
        }
    }

    // `astore N` — the local's non-null status now depends entirely
    // on the value being stored. Without value-origin tracking we
    // can't tell whether the stored value is non-null, so we
    // conservatively CLEAR the bit. The two common patterns where
    // we WANT to retain non-null are then re-derived by `produces_nonnull`
    // above (which sets the bit when the immediate predecessor is
    // `new`/`anewarray`/...) and by `aload N` followed by a
    // dereferencing op (handled by the `aload` clause above).
    //
    // Important: the `produces_nonnull` block above already set the
    // bit when the predecessor was a non-null producer. We need to
    // preserve that — so check for "this PC's predecessor produced
    // non-null" by re-inspecting `code[pc - prev_len]`. If yes,
    // skip the clear.
    if let Some(local) = astore_at(code, pc) {
        if local < 64 {
            // Was the immediately-preceding INSTRUCTION (boundary
            // resolved by prev_inst_pc) a non-null producer? If yes,
            // the bit was set in OUT of that producer's transfer and
            // flowed into our in_mask — keep it. If no, clear (we
            // don't know what value is being stored).
            let prev_was_alloc = prev_inst_pc.get(pc).copied().map_or(false, |q| {
                q != usize::MAX && q < code.len() && produces_nonnull(code[q])
            });
            // `aload K; astore N` (optionally through a `checkcast`, which
            // passes its operand through unchanged, null included) copies K's
            // value, so K's fact becomes N's. `Foo f = (Foo) obj;` and every
            // "local alias" javac emits are this shape. Sound only where the
            // copied value really is the one that `aload` pushed: the store
            // must not be a merge point (`linear_only`), nor may the
            // `checkcast` between them. The aload itself may be one — it
            // pushes local K's value however control arrived — and K's bit in
            // `in_mask` is K's state at the aload, because neither `aload`
            // nor `checkcast` changes a bit.
            let copied_nonnull = linear_only
                && prev_inst_pc.get(pc).copied().is_some_and(|q| {
                    if q == usize::MAX || q >= code.len() {
                        return false;
                    }
                    let src = if code[q] == 0xC0 {
                        if merge_points.get(q).copied().unwrap_or(true) {
                            None
                        } else {
                            prev_inst_pc.get(q).copied().filter(|&r| r != usize::MAX)
                        }
                    } else {
                        Some(q)
                    };
                    src.and_then(|r| aload_at(code, r))
                        .is_some_and(|k| k < 64 && in_mask & (1u64 << k) != 0)
                });
            if copied_nonnull {
                out |= 1u64 << local;
            } else if !prev_was_alloc || !linear_only {
                // At a merge point the stored value may come from a path that
                // pushed null (`buf = c ? null : new int[3]`), whatever the
                // textual predecessor allocated.
                out &= !(1u64 << local);
            }
        }
    }

    // `aconst_null` followed by `astore N` clears N.
    if op == 0x01 {
        let next_pc = pc + 1;
        if next_pc < code.len() {
            if let Some(local) = astore_at(code, next_pc) {
                if local < 64 {
                    out &= !(1u64 << local);
                }
            }
        }
    }

    // ifnull / ifnonnull discriminate the two outgoing edges.
    // The `aload N; if{null,nonnull} T` pattern:
    //   * ifnull  T: fall-through ⇒ N non-null, taken ⇒ N null
    //   * ifnonnull T: fall-through ⇒ N null, taken ⇒ N non-null
    if matches!(op, 0xC6 | 0xC7) && linear_only {
        // The receiver for the if{null,nonnull} test comes from the
        // most-recent push. If the predecessor instruction was
        // `aload N` we can refine. Use prev_inst_pc to find the
        // actual predecessor boundary.
        let prev_local = prev_inst_pc.get(pc).copied().and_then(|q| {
            if q == usize::MAX {
                return None;
            }
            aload_at(code, q)
        });
        if let Some(local) = prev_local {
            if local < 64 {
                let bit = 1u64 << local;
                if op == 0xC6 {
                    // ifnull: fall-through proves non-null, taken proves null
                    let ft_out = out | bit;
                    let tk_out = out & !bit;
                    return (ft_out, tk_out);
                } else {
                    // ifnonnull: fall-through proves null, taken proves non-null
                    let ft_out = out & !bit;
                    let tk_out = out | bit;
                    return (ft_out, tk_out);
                }
            }
        }
    }

    (out, out)
}

/// Run the null-check elimination analysis on a method's bytecode.
///
/// The `code` slice is the raw bytecode; `code_len` is the effective
/// length (may be less than `code.len()` if padded). Returns a
/// `NullCheckInfo` whose `is_nonnull(pc, local)` method reports
/// whether the local is proven non-null at that PC.
///
/// Round-11 HIGH-1 fix: replaces the round-7 safe-stub
/// (`NullCheckInfo::default()`) with a proper fixpoint forward
/// dataflow using meet-over-paths (intersection at join points).
pub fn analyze(code: &[u8], code_len: usize) -> NullCheckInfo {
    analyze_with_receiver(code, code_len, false)
}

/// Is local 0 the `this` of an instance method?
///
/// `Some(true)` means it is, `Some(false)` that the method is static, and
/// `None` that the two inputs did not agree and the question is therefore
/// unanswered — which every caller must read as "assume nothing".
///
/// # Why this is derived rather than passed
///
/// `compile_with_param_slots` takes no `is_static`, and threading one through
/// would be a cross-crate signature change to a function that already carries
/// two dozen parameters. Both facts it needs are already there:
///
/// * `method_key` is `"<class>.<method>:<descriptor>"`, and
/// * `num_params` is the ARGUMENT count — one per argument regardless of
///   width, `this` included for an instance method (`prologue_param_slots` in
///   `lib.rs`: `count_param_slots(descriptor) + if is_static { 0 } else { 1 }`).
///
/// So `num_params` must equal the descriptor's declared count or that count
/// plus one, and which of the two it is *is* the answer. Note the width
/// convention: `count_param_slots` counts `J`/`D` as ONE, so this must not be
/// confused with `compute_param_jvm_slots`, which counts them as two. Reading
/// the wrong one makes every method with a `long` or `double` parameter look
/// like the other kind.
///
/// # The disagreement case is the load-bearing one
///
/// Returning `None` when the arithmetic comes out to neither is not
/// defensiveness for its own sake — it is the only thing standing between a
/// mis-paired `method_key` and a wrong `this` seed, and a wrong seed elides a
/// null check that was doing real work. The legacy `compile()` wrapper passes
/// `""`, which has no `(` and lands here too.
pub fn receiver_in_local_zero(method_key: &str, num_params: usize) -> Option<bool> {
    let desc_start = method_key.find('(')?;
    let descriptor = &method_key[desc_start..];
    descriptor.find(')')?;
    let declared = crate::count_param_slots(descriptor);
    if num_params == declared {
        Some(false)
    } else if num_params == declared + 1 {
        Some(true)
    } else {
        None
    }
}

/// [`analyze`], plus the one fact no walk of the bytecode can derive for
/// itself: whether local 0 holds a receiver.
///
/// # What the seed buys, and why the absence of it cost a loop
///
/// Entry IN was 0 — *nothing* proven on entry. The only way a local became
/// known non-null was for an instruction to dereference it, so the first
/// `this.field` in a method always paid a `TEST`/`JZ`, and — much worse — so
/// did every later one inside a loop. The IN mask at a loop header is the meet
/// of the entry path and the backedge; the backedge carries "local 0 non-null"
/// (the body's own `getfield` proved it), the entry path does not, and the
/// intersection is empty. A `for (…) sum += this.x;` therefore re-tested `this`
/// on every iteration, forever, for a value the JVM guarantees.
///
/// `this` is non-null by construction: an instance method is only ever entered
/// through a call site that has already null-checked its receiver, and that
/// includes `<init>`, whose receiver is uninitialized but never null. Seeding
/// bit 0 makes the meet come out non-empty and the whole loop stop asking.
///
/// # Why this is safe for the facts it feeds
///
/// The seed is one more fact of exactly the kind the analysis already
/// produces, and it is killed by the same rule: `astore 0` clears bit 0 unless
/// the stored value came from a proven producer. A method that reassigns local
/// 0 loses the fact at the store, which is the correct and conservative
/// direction. An `istore_0`/`fstore_0` over the receiver slot does not clear
/// it, and does not need to: a later `aload_0` of a slot last written by an
/// int store does not verify, so no consumer can reach the stale bit.
///
/// It is also correct under OSR. Entry IN was 0, so every fact this analysis
/// produces is derived from instructions that actually executed on the path to
/// the PC — properties that hold however control arrived, including an
/// interpreter transition into the middle of the method. "Local 0 is non-null"
/// is a property of the frame, not of the path, so it holds there too.
pub fn analyze_with_receiver(
    code: &[u8],
    code_len: usize,
    receiver_in_local_0: bool,
) -> NullCheckInfo {
    let len = code_len.min(code.len());
    if len == 0 {
        return NullCheckInfo::default();
    }

    // Refuse the whole method when the strict decoder cannot describe it.
    //
    // This runs BEFORE `uses_subroutines` (and before `stores_ref_to_local_zero`
    // and the `is_inst_start` pre-pass below), on purpose: all three walk `code`
    // linearly via `bytecode_analysis::step`, which is
    // `insn_len(..).unwrap_or(1)` and never answers 0 — so an instruction that
    // pass cannot decode (a `tableswitch` / `lookupswitch` whose padding or
    // table runs past `code[..len]`) does not stall those walks, it DESYNCS
    // them: the walk advances one byte and starts reading the switch's own
    // operand bytes as opcodes, so every fact derived from that point on
    // describes a bytecode stream that does not exist. This is a MUST analysis
    // over a meet, so a stale fact from before the desync can survive a kill the
    // desync hid — the same shape as the exception-handler and `wide astore`
    // fixes elsewhere in this file. Gating here first also keeps a desync from
    // corrupting `uses_subroutines`'s own scan (a desync could in principle
    // shift it past a real `jsr`/`ret` byte, which would be the subroutine
    // hazard below leaking through undetected).
    //
    // `decode_method` walks with `decode_at`, which requires every instruction
    // to lie wholly inside `code[..len]`, and returns `None` unless the walk
    // lands exactly on `len`. That is precisely "the linear walk this file does
    // is the real instruction stream", so gating on it here rules the desync
    // out before any later walk can hit it.
    // See `docs/internal/fixed-bugs/r10-gcs-null-check-elim-has-no-refusal-for-a-desynced-decode-FIXED-20260921.md`.
    if bytecode_analysis::decode_method(code, len).is_none() {
        return NullCheckInfo::default();
    }

    // `jsr`/`ret` are not modelled, so the whole method is refused.
    //
    // `successors` treats `jsr`/`jsr_w` as falling through to the instruction
    // after them, which pushes OUT[jsr] straight past the subroutine: every
    // fact established before the `jsr` survives it untouched. The subroutine
    // body is reached only by the "seed everything unvisited with IN = 0" pass
    // below, and its `ret` flows to the instruction textually after the `ret`,
    // not back to any return site. So a subroutine that does
    // `aconst_null; astore_1` never narrows the state at the `jsr`'s return
    // point, and that PC is not marked a merge point either (the `merge_points`
    // loop marks the subroutine ENTRY, not the return site), so the syntactic
    // peepholes stay armed there too. Both together are a silent miscompile:
    // a local the subroutine nulled reads as proven non-null afterwards.
    //
    // The x64 emitter claims `jsr`/`ret` never reaches it, and `x64/bce.rs` and
    // `x64/licm.rs` both refuse these opcodes outright — but this is a
    // crate-level module with no refusal of its own, and a caller that forgets
    // gets no warning. `x64/bce.rs::range_safe_pcs` refuses a whole method the
    // same way for the same reason; this is that discipline, here.
    if uses_subroutines(code, len) {
        return NullCheckInfo::default();
    }

    // IN[pc] starts at !0 (the lattice top — everything non-null);
    // entry is special-cased to 0 below. Unreachable PCs stay at
    // top and effectively contribute nothing because they're never
    // pulled into the worklist.
    //
    // Round-11: using `!0` as top would prevent the first meet from
    // narrowing correctly (∧ with !0 = identity), but means we have
    // to INITIALISE every IN to !0 and the meet across an empty
    // predecessor set yields !0. That's wrong for unreachable PCs
    // but they never propagate anywhere, so the masks they produce
    // never matter.
    //
    // For *reachable* PCs we start with !0 and use the worklist to
    // narrow. Entry IN is forced to 0 (nothing proven on entry).
    let top: u64 = !0;
    let mut in_masks = vec![top; len];
    // Whether IN[pc] has been assigned by an edge or a seed. Kept apart from
    // the mask: `!0` is also a legitimate IN (every tracked local non-null),
    // so "still at top" cannot double as "never reached".
    let mut visited = vec![false; len];
    // Entry IN. `this` is the single fact the bytecode cannot prove about
    // itself; everything else starts unproven. See the doc comment.
    in_masks[0] = u64::from(receiver_in_local_0);
    visited[0] = true;

    // Track which PCs are valid instruction starts AND record each
    // instruction's *linear-predecessor* PC. The "linear predecessor"
    // is the PC of the instruction that immediately precedes us in
    // bytecode order — this matches javac's emission order, which is
    // what the `produces_nonnull → astore` pattern needs.
    //
    // Note: linear predecessor is NOT the same as the dataflow
    // predecessor (which can be a branch source). We use it only for
    // syntactic peephole checks like "did the previous instruction
    // allocate?". The dataflow correctness still relies on the
    // forward-meet machinery below.
    //
    // This walk cannot desync: the `decode_method` refusal above already
    // proved every instruction in `code[..len]` decodes and the walk lands
    // exactly on `len`, so `step` retraces a path `decode_at` already
    // validated. `l == 0` stays as a backstop against `step`'s contract
    // changing (see `uses_subroutines`'s doc comment for the full argument).
    let mut is_inst_start = vec![false; len];
    let mut prev_inst_pc = vec![usize::MAX; len];
    {
        let mut p = 0usize;
        let mut prev = usize::MAX;
        while p < len {
            is_inst_start[p] = true;
            prev_inst_pc[p] = prev;
            let l = bytecode_analysis::step(code, p);
            if l == 0 {
                break;
            }
            prev = p;
            p += l;
        }
    }

    // Merge points: every PC control can reach other than by falling through
    // from its textual predecessor. `transfer` refuses its predecessor
    // peepholes there, and the seeding pass below adds the PCs only an
    // exception edge reaches.
    let mut merge_points = vec![false; len];
    merge_points[0] = true;
    for p in 0..len {
        if !is_inst_start[p] {
            continue;
        }
        let mut mark = |t: Option<usize>| {
            if let Some(t) = t.filter(|&t| t < len) {
                merge_points[t] = true;
            }
        };
        let (ft, tk) = successors(code, p);
        mark(tk);
        for t in bytecode_analysis::switch_targets_lenient(code, len, p) {
            mark(Some(t));
        }
        match code[p] {
            // `successors` models `jsr`/`jsr_w` as falling through only;
            // their subroutine entry is still a jump target.
            0xA8 | 0xC9 => mark(bytecode_analysis::offset_branch_target(code, p)),
            _ => {}
        }
        // The next instruction after one that never falls through (goto,
        // return, athrow, a switch, `ret`) is reached, if at all, some other way.
        if ft.is_none() || code[p] == 0xA9 {
            mark(Some(p + bytecode_analysis::step(code, p)));
        }
    }

    // What an instruction no modelled edge reaches may assume on entry.
    // Nothing — except the receiver, when no instruction ever stores a
    // reference into local 0 (`stores_ref_to_local_zero`): then local 0 is
    // `this` at every point of the activation, handler entries included.
    // Seeding handlers with a bare 0 made every join after a `catch` forget
    // `this`, so a loop following any try/catch re-tested its own receiver on
    // every `this.f` — the exact cost the receiver seed exists to remove.
    let handler_seed: u64 = u64::from(receiver_in_local_0 && !stores_ref_to_local_zero(code, len));

    // Worklist of PCs whose IN may have changed and whose successors
    // need re-visiting.
    let mut worklist: Vec<usize> = Vec::with_capacity(len / 4 + 1);
    worklist.push(0);
    let mut on_worklist = vec![false; len];
    on_worklist[0] = true;

    // Cap iterations to avoid pathological non-convergence on
    // malformed bytecode. Each PC can change at most 64 times
    // (one bit flip per local) before reaching a fixed point, so
    // 64 * len is a safe upper bound; we use 128 * len for slack.
    let max_iters = (len as u64).saturating_mul(128).max(4096);
    let mut iter_count: u64 = 0;

    loop {
        while let Some(pc) = worklist.pop() {
            on_worklist[pc] = false;
            iter_count += 1;
            if iter_count > max_iters {
                // This is a MUST analysis descending from top: stopped short
                // of its fixpoint it still holds facts that later iterations
                // would have removed. No facts is the only sound answer.
                return NullCheckInfo::default();
            }
            if pc >= len || !is_inst_start[pc] {
                continue;
            }

            let in_m = in_masks[pc];
            let (ft_out, tk_out) = transfer(code, pc, in_m, &prev_inst_pc, &merge_points);
            let (ft_succ, tk_succ) = successors(code, pc);

            if let Some(s) = ft_succ {
                meet_into(
                    s,
                    ft_out,
                    &is_inst_start,
                    &mut in_masks,
                    &mut visited,
                    &mut on_worklist,
                    &mut worklist,
                );
            }
            if let Some(s) = tk_succ {
                meet_into(
                    s,
                    tk_out,
                    &is_inst_start,
                    &mut in_masks,
                    &mut visited,
                    &mut on_worklist,
                    &mut worklist,
                );
            }
            for s in bytecode_analysis::switch_targets_lenient(code, len, pc) {
                meet_into(
                    s,
                    ft_out,
                    &is_inst_start,
                    &mut in_masks,
                    &mut visited,
                    &mut on_worklist,
                    &mut worklist,
                );
            }
        }

        // Seed every instruction no modelled edge reached with "nothing
        // proven" and run the fixpoint again from there. These are exception
        // handlers, `jsr` subroutines and dead code. A handler that falls
        // through into shared code now narrows that join; left unvisited it
        // contributed nothing, and the join kept facts only the normal path
        // had established (`try { ... a = new int[4]; } catch (E e) {}
        // return a == null ? -1 : a.length;` elided the null check).
        let mut seeded = false;
        for p in 0..len {
            if is_inst_start[p] && !visited[p] {
                visited[p] = true;
                in_masks[p] = handler_seed;
                merge_points[p] = true;
                on_worklist[p] = true;
                worklist.push(p);
                seeded = true;
            }
        }
        if !seeded {
            break;
        }
    }

    // PCs that start no instruction carry no facts.
    for (m, seen) in in_masks.iter_mut().zip(&visited) {
        if !*seen {
            *m = 0;
        }
    }

    NullCheckInfo {
        masks: in_masks,
        merge_points,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `bytecode_analysis::step` never answers 0, which is what makes every
    /// `if l == 0` guard in this file unreachable.
    ///
    /// Pinned rather than asserted in prose: the guards read as live defences,
    /// and their comments now say they are not. If `step` ever grows a zero
    /// answer this fails, and the two comments — plus the desync page they point
    /// at — have to be revisited together.
    ///
    /// Every opcode byte is covered, at a pc with and without room for operands,
    /// because the zero would come from an operand read rather than from the
    /// opcode table.
    #[test]
    fn the_linear_decode_step_never_answers_zero() {
        for op in 0u16..=255 {
            let op = op as u8;
            // Plenty of operand room, then none at all.
            let roomy = [op, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            let bare = [op];
            assert_ne!(
                bytecode_analysis::step(&roomy, 0),
                0,
                "step answered 0 for opcode {op:#04x} with operand room"
            );
            assert_ne!(
                bytecode_analysis::step(&bare, 0),
                0,
                "step answered 0 for opcode {op:#04x} with no operand room"
            );
        }
    }

    #[test]
    fn getfield_proves_receiver_nonnull() {
        // aload_0; getfield #1; ... ; aload_0; getfield #2
        // After the first getfield, local 0 is proven non-null.
        let code = vec![
            0x2A, // 0: aload_0
            0xB4, 0x00, 0x01, // 1: getfield #1
            0x57, // 4: pop (discard field value)
            0x2A, // 5: aload_0
            0xB4, 0x00, 0x02, // 6: getfield #2
            0xB1, // 9: return
        ];
        let info = analyze(&code, code.len());
        // At PC 0, local 0 is NOT proven non-null (no prior evidence).
        assert!(!info.is_nonnull(0, 0));
        // After the aload_0;getfield#1 pair completes, local 0 IS
        // non-null at the next aload_0 (PC 5) and the following
        // getfield (PC 6).
        assert!(info.is_nonnull(5, 0));
        assert!(info.is_nonnull(6, 0));
    }

    #[test]
    fn ifnull_proves_nonnull_on_fallthrough() {
        // aload_1; ifnull +5; aload_1; ...
        // The fall-through after `ifnull` proves local 1 non-null.
        let code = vec![
            0x2B, // 0: aload_1
            0xC6, 0x00, 0x05, // 1: ifnull → PC 6
            0x2B, // 4: aload_1 (fall-through: local 1 non-null)
            0xB1, // 5: return
            0xB1, // 6: return (taken path)
        ];
        let info = analyze(&code, code.len());
        assert!(info.is_nonnull(4, 1));
        // At PC 6 (taken), local 1 must be NULL — definitely NOT non-null.
        assert!(!info.is_nonnull(6, 1));
    }

    #[test]
    fn astore_after_new_sets_nonnull() {
        // new #X; astore_1 → local 1 is non-null
        let code = vec![
            0xBB, 0x00, 0x01, // 0: new #1
            0x4C, // 3: astore_1
            0x2B, // 4: aload_1
            0xB1, // 5: return
        ];
        let info = analyze(&code, code.len());
        assert!(info.is_nonnull(4, 1));
    }

    #[test]
    fn total_facts_counts_correctly() {
        let code = vec![
            0xBB, 0x00, 0x01, // 0: new
            0x4C, // 3: astore_1
            0xB1, // 4: return
        ];
        let info = analyze(&code, code.len());
        assert!(info.total_facts() >= 1);
    }

    #[test]
    fn merge_intersects_predecessors() {
        // Two paths merge at the return (PC 8). On the fall-through
        // path local 1 is proven non-null by the getfield; on the
        // ifeq-taken path it is not. The dataflow meet (bitwise AND)
        // over the two predecessors must yield "not proven non-null"
        // at PC 8.
        //
        //   0: iconst_0                    (0x03)
        //   1: ifeq → PC 8  (offset +7)    (0x99 00 07)
        //   4: aload_1                     (0x2B)
        //   5: getfield #1 (proves L1)     (0xB4 00 01)
        //   8: return  ← merge point       (0xB1)
        //
        // The ifeq branch offset is relative to the ifeq opcode (PC 1),
        // so reaching the return at PC 8 needs offset 7 — NOT 6, which
        // would land mid-getfield (PC 7 is not an instruction start),
        // get dropped by the CFG walk, and leave PC 8 with only the
        // getfield predecessor.
        let code = vec![
            0x03, // 0: iconst_0
            0x99, 0x00, 0x07, // 1: ifeq → PC 8
            0x2B, // 4: aload_1
            0xB4, 0x00, 0x01, // 5: getfield (proves L1 nonnull on this path)
            0xB1, // 8: return (merge)
        ];
        // The "merge" PC here is 8 (the return). Local 1 should NOT
        // be proven non-null at PC 8 because the ifeq-taken path
        // bypassed the getfield.
        let info = analyze(&code, code.len());
        assert!(!info.is_nonnull(8, 1));
    }

    #[test]
    fn fixpoint_terminates_on_loop() {
        // Tight loop should converge without iterating forever.
        //   0: aload_0; getfield (proves L0)
        //   4: pop
        //   5: goto -5 → PC 0 (loop back)
        let code = vec![
            0x2A, // 0: aload_0
            0xB4, 0x00, 0x01, // 1: getfield
            0x57, // 4: pop
            0xA7, 0xFF, 0xFB, // 5: goto -5 → PC 0
        ];
        let info = analyze(&code, code.len());
        // At PC 0 on the first iteration L0 is unproven; after fixpoint
        // it's STILL unproven (the meet over [entry: 0, back-edge:
        // {L0}] = 0). This is the correctness check.
        assert!(!info.is_nonnull(0, 0));
    }

    /// A `CONSTANT_Dynamic` resolves by running a bootstrap method, and a
    /// bootstrap may return `null` (`ConstantBootstraps.nullConstant` is in the
    /// JDK for exactly that). This module has no constant pool, so it cannot
    /// tell a condy from a string literal and must assume the one that can be
    /// null.
    #[test]
    fn an_ldc_constant_is_not_assumed_non_null() {
        // ldc #1; astore_1; aload_1; return
        let code = vec![
            0x12, 0x01, // 0: ldc #1        (may be a null condy)
            0x4C, // 2: astore_1
            0x2B, // 3: aload_1
            0xB1, // 4: return
        ];
        let info = analyze(&code, code.len());
        assert!(
            !info.is_nonnull(3, 1),
            "a local loaded from `ldc` may hold the null a condy bootstrap \
             returned; proving it non-null deletes the program's own null test"
        );

        // The twin: `new` really does produce a non-null reference, so the
        // refusal above is about `ldc` and not about the `astore` rule.
        let allocated = vec![
            0xBB, 0x00, 0x01, // 0: new #1
            0x4C, // 3: astore_1
            0x2B, // 4: aload_1
            0xB1, // 5: return
        ];
        let info = analyze(&allocated, allocated.len());
        assert!(info.is_nonnull(4, 1));
    }

    /// `successors` models `jsr` as falling through, so every fact established
    /// before the `jsr` survives the subroutine — including facts the
    /// subroutine destroyed. The whole method is refused instead.
    #[test]
    fn a_method_with_a_subroutine_produces_no_facts_at_all() {
        // new #1; astore_1; jsr → 8; aload_1; return
        // 8: astore_2 (the return address); aconst_null; astore_1; ret 2
        //
        // Local 1 is non-null when the `jsr` is reached and null when the
        // subroutine returns. The fall-through model keeps the stale fact.
        let code = vec![
            0xBB, 0x00, 0x01, // 0: new #1
            0x4C, // 3: astore_1
            0xA8, 0x00, 0x05, // 4: jsr +5 → 9
            0x2B, // 7: aload_1
            0xB1, // 8: return
            0x4D, // 9: astore_2   (subroutine entry: the return address)
            0x01, // 10: aconst_null
            0x4C, // 11: astore_1  ← the fact the fall-through model loses
            0xA9, 0x02, // 12: ret 2
        ];
        let info = analyze(&code, code.len());
        assert_eq!(
            info.total_facts(),
            0,
            "a method using jsr/ret must produce no facts: the analysis cannot \
             carry the subroutine's effects to the return site, and a stale \
             fact there elides a null check that was doing real work"
        );
        assert!(!info.is_nonnull(7, 1));

        // The twin: the same prefix with the `jsr`/`ret` pair removed keeps its
        // facts, so the refusal is about the subroutine and not about the shape
        // of the fixture.
        let without = vec![
            0xBB, 0x00, 0x01, // 0: new #1
            0x4C, // 3: astore_1
            0x00, 0x00, 0x00, // 4-6: nop, nop, nop  (where the jsr was)
            0x2B, // 7: aload_1
            0xB1, // 8: return
        ];
        let info = analyze(&without, without.len());
        assert!(info.is_nonnull(7, 1));
    }
}

#[cfg(test)]
mod receiver_seed_tests {
    use super::{analyze_with_receiver, receiver_in_local_zero};

    /// `this` is non-null, and the loop header is where that stops being free.
    ///
    /// The body below is `for (i = 0; i < 10; i++) { x = this.f; }`. The
    /// `getfield` at bci 3 proves its own receiver non-null on fall-through,
    /// so the BACKEDGE into the loop header carries the fact. The entry path
    /// does not, and the meet is an intersection — so without a seed the fact
    /// dies at the header on every iteration and the receiver is re-tested
    /// forever, for a value the JVM guarantees at entry.
    ///
    /// Both arms are asserted in one test on purpose: the `false` arm IS the
    /// pre-seed behaviour, so this test would have failed before the seed
    /// existed and states exactly what changed.
    #[test]
    fn the_receiver_seed_is_what_survives_the_loop_header_meet() {
        #[rustfmt::skip]
        let code: Vec<u8> = vec![
            0x03,               // 0:  iconst_0
            0x3C,               // 1:  istore_1
            0x2A,               // 2:  aload_0          <- loop header
            0xB4, 0x00, 0x01,   // 3:  getfield #1
            0x3D,               // 6:  istore_2
            0x1B,               // 7:  iload_1
            0x04,               // 8:  iconst_1
            0x60,               // 9:  iadd
            0x3C,               // 10: istore_1
            0x1B,               // 11: iload_1
            0x10, 0x0A,         // 12: bipush 10
            0xA1, 0xFF, 0xF4,   // 14: if_icmplt 2      (14 - 12)
            0x1B,               // 17: iload_1
            0xAC,               // 18: ireturn
        ];
        let n = code.len();

        let unseeded = analyze_with_receiver(&code, n, false);
        assert!(
            !unseeded.is_nonnull(3, 0),
            "without the seed the entry path carries nothing, so the meet at \
             the loop header must be empty -- if this passes, the header is no \
             longer a join and the test has stopped measuring the thing it names"
        );

        let seeded = analyze_with_receiver(&code, n, true);
        assert!(
            seeded.is_nonnull(3, 0),
            "with `this` seeded, both edges into the header carry local 0 and \
             the getfield receiver check is dead"
        );
    }

    /// A `getfield` on a local that is NOT the receiver keeps its check.
    ///
    /// Same shape, but the field is read out of local 1 -- a parameter, which
    /// the seed says nothing about. The guard here is against a seed that
    /// leaks across locals, which would elide a real null check on an argument
    /// and turn an NPE into a SIGSEGV.
    #[test]
    fn the_seed_does_not_leak_to_a_parameter() {
        #[rustfmt::skip]
        let code: Vec<u8> = vec![
            0x2B,               // 0: aload_1
            0xB4, 0x00, 0x01,   // 1: getfield #1
            0x57,               // 4: pop
            0xB1,               // 5: return
        ];
        let seeded = analyze_with_receiver(&code, code.len(), true);
        assert!(
            !seeded.is_nonnull(1, 1),
            "the seed proves local 0 and nothing else"
        );
    }

    /// The derivation, including the width convention that would silently
    /// misclassify every method with a `long` or `double` parameter.
    #[test]
    fn the_receiver_derivation_reads_an_argument_count_not_a_slot_count() {
        // Instance: descriptor declares 2, `num_params` counts `this` too.
        assert_eq!(receiver_in_local_zero("Foo.bar:(II)V", 3), Some(true));
        // Static: the two agree exactly.
        assert_eq!(receiver_in_local_zero("Foo.bar:(II)V", 2), Some(false));
        // `J` and `D` are ONE argument each here. Counting them as two JVM
        // slots (which `compute_param_jvm_slots` does, and this must not)
        // would make declared = 4 and answer `None` for a plain instance
        // method -- or, worse, `Some(false)` for the static one.
        assert_eq!(receiver_in_local_zero("Foo.bar:(JD)V", 3), Some(true));
        assert_eq!(receiver_in_local_zero("Foo.bar:(JD)V", 2), Some(false));
        // No-arg forms are still distinguishable.
        assert_eq!(receiver_in_local_zero("Foo.bar:()V", 1), Some(true));
        assert_eq!(receiver_in_local_zero("Foo.bar:()V", 0), Some(false));
        // Disagreement is unanswered, never guessed.
        assert_eq!(receiver_in_local_zero("Foo.bar:(II)V", 7), None);
        // The legacy `compile()` wrapper's empty key has no descriptor.
        assert_eq!(receiver_in_local_zero("", 1), None);
        assert_eq!(receiver_in_local_zero("Foo.bar:(II", 3), None);
    }
}

/// Regressions from the 2026-09-12 JIT review: facts that reached a join from
/// only one of its predecessors.
#[cfg(test)]
mod merge_point_tests {
    use super::*;

    /// `int v = (c ? a : b).x; return b == null ? -1 : v;`
    /// The `getfield` is a join of `aload_1` and `aload_2`, but its textual
    /// predecessor is `aload_2`, so "b is non-null" was proven on both paths.
    #[test]
    fn a_dereference_at_a_join_proves_nothing_about_its_textual_predecessor() {
        let code = vec![
            0x1a, //             0: iload_0
            0x99, 0x00, 0x07, // 1: ifeq -> 8
            0x2b, //             4: aload_1
            0xa7, 0x00, 0x04, // 5: goto -> 9
            0x2c, //             8: aload_2
            0xb4, 0x00, 0x01, // 9: getfield #1   (join)
            0x36, 0x03, //      12: istore 3
            0x2c, //            14: aload_2
            0xc7, 0x00, 0x05, // 15: ifnonnull -> 20
            0x02, //            18: iconst_m1
            0xac, //            19: ireturn
            0x1d, //            20: iload_3
            0xac, //            21: ireturn
        ];
        let info = analyze(&code, code.len());
        assert!(info.is_merge_point(9));
        assert!(
            !info.is_nonnull(14, 2),
            "b was never dereferenced on the c == true path"
        );
        assert!(!info.is_nonnull(15, 2));
        assert!(!info.is_nonnull(14, 1));
    }

    /// A `tableswitch` case nulls local 1 and joins a path on which a
    /// `getfield` proved it non-null. Switch edges were not decoded, so the
    /// case never reached the join.
    #[test]
    fn a_switch_case_participates_in_the_join_it_reaches() {
        let code = vec![
            0x2b, //                          0: aload_1
            0xb4, 0x00, 0x01, //              1: getfield     (local 1 non-null)
            0x57, //                          4: pop
            0x1c, //                          5: iload_2
            0x99, 0x00, 0x1b, //              6: ifeq -> 33
            0x1d, //                          9: iload_3
            0xaa, //                         10: tableswitch
            0x00, //                         11: padding
            0x00, 0x00, 0x00, 0x17, //       12: default -> 33
            0x00, 0x00, 0x00, 0x00, //       16: low 0
            0x00, 0x00, 0x00, 0x00, //       20: high 0
            0x00, 0x00, 0x00, 0x12, //       24: case 0 -> 28
            0x01, //                         28: aconst_null
            0x4c, //                         29: astore_1
            0xa7, 0x00, 0x03, //             30: goto -> 33
            0x2b, //                         33: aload_1      (join)
            0xc6, 0x00, 0x05, //             34: ifnull -> 39
            0x03, //                         37: iconst_0
            0xac, //                         38: ireturn
            0x04, //                         39: iconst_1
            0xac, //                         40: ireturn
        ];
        let info = analyze(&code, code.len());
        assert!(info.is_merge_point(28));
        assert!(info.is_merge_point(33));
        assert!(
            !info.is_nonnull(33, 1),
            "the switch case stored null into local 1"
        );
        assert!(!info.is_nonnull(34, 1));
    }

    /// `try { mayThrow(); } catch (E e) { a = null; } if (a == null) ...`
    /// No exception table is consulted; the handler used to stay unvisited,
    /// so the join kept "a non-null" from the normal path alone.
    #[test]
    fn code_only_an_exception_edge_reaches_is_seeded_into_the_join() {
        let code = vec![
            0x2b, //             0: aload_1
            0xb4, 0x00, 0x01, // 1: getfield     (local 1 non-null)
            0x57, //             4: pop
            0xb8, 0x00, 0x02, // 5: invokestatic
            0xa7, 0x00, 0x06, // 8: goto -> 14
            0x57, //            11: pop          (handler entry)
            0x01, //            12: aconst_null
            0x4c, //            13: astore_1
            0x2b, //            14: aload_1      (join)
            0xc6, 0x00, 0x05, // 15: ifnull -> 20
            0x03, //            18: iconst_0
            0xac, //            19: ireturn
            0x04, //            20: iconst_1
            0xac, //            21: ireturn
        ];
        let info = analyze(&code, code.len());
        assert!(
            info.is_merge_point(11),
            "a handler-only entry is a merge point"
        );
        assert!(
            !info.is_nonnull(14, 1),
            "the handler stored null into local 1"
        );
        assert!(!info.is_nonnull(15, 1));
        // The fact still holds where only the normal path reaches.
        assert!(info.is_nonnull(5, 1));
    }
}

/// Round-9 regressions: a store the kill rule could not see, and two facts the
/// analysis threw away for no soundness reason.
#[cfg(test)]
mod round9_tests {
    use super::*;

    /// `wide astore 1` is a store. It used to be invisible to `astore_at`, so
    /// the `new; astore_1` fact survived an explicit null store.
    #[test]
    fn a_wide_astore_kills_the_fact() {
        #[rustfmt::skip]
        let code: Vec<u8> = vec![
            0xBB, 0x00, 0x01,       // 0: new #1
            0x4C,                   // 3: astore_1          (local 1 non-null)
            0x01,                   // 4: aconst_null
            0xC4, 0x3A, 0x00, 0x01, // 5: wide astore 1     (local 1 = null)
            0x2B,                   // 9: aload_1
            0xB1,                   // 10: return
        ];
        let info = analyze(&code, code.len());
        assert!(info.is_nonnull(4, 1), "the fixture must establish the fact");
        assert!(
            !info.is_nonnull(9, 1),
            "local 1 was just nulled by a wide store; proving it non-null              elides the program's own null test"
        );
    }

    /// `aload K; astore N` copies K's fact to N, directly or through a
    /// `checkcast`; an unproven source proves nothing.
    #[test]
    fn a_copy_of_a_non_null_local_is_non_null() {
        #[rustfmt::skip]
        let direct: Vec<u8> = vec![
            0xBB, 0x00, 0x01, // 0: new #1
            0x4C,             // 3: astore_1   (proven)
            0x2B,             // 4: aload_1
            0x4D,             // 5: astore_2   (copy)
            0x2C,             // 6: aload_2
            0xB1,             // 7: return
        ];
        assert!(analyze(&direct, direct.len()).is_nonnull(6, 2));

        #[rustfmt::skip]
        let cast: Vec<u8> = vec![
            0xBB, 0x00, 0x01, // 0: new #1
            0x4C,             // 3: astore_1
            0x2B,             // 4: aload_1
            0xC0, 0x00, 0x02, // 5: checkcast #2
            0x4D,             // 8: astore_2
            0x2C,             // 9: aload_2
            0xB1,             // 10: return
        ];
        assert!(analyze(&cast, cast.len()).is_nonnull(9, 2));

        #[rustfmt::skip]
        let unproven: Vec<u8> = vec![
            0xBB, 0x00, 0x01, // 0: new #1
            0x4C,             // 3: astore_1
            0x2D,             // 4: aload_3    (nothing known about local 3)
            0x4D,             // 5: astore_2
            0x2C,             // 6: aload_2
            0xB1,             // 7: return
        ];
        assert!(!analyze(&unproven, unproven.len()).is_nonnull(6, 2));
    }

    /// A copy whose store is a join proves nothing: the stored value may come
    /// from the other arm.
    #[test]
    fn a_copy_at_a_join_is_not_trusted() {
        #[rustfmt::skip]
        let code: Vec<u8> = vec![
            0xBB, 0x00, 0x01, // 0: new #1
            0x4C,             // 3: astore_1           (local 1 non-null)
            0x1A,             // 4: iload_0
            0x99, 0x00, 0x07, // 5: ifeq -> 12
            0x2B,             // 8: aload_1
            0xA7, 0x00, 0x04, // 9: goto -> 13
            0x01,             // 12: aconst_null
            0x4D,             // 13: astore_2          (join)
            0x2C,             // 14: aload_2
            0xB1,             // 15: return
        ];
        let info = analyze(&code, code.len());
        assert!(info.is_merge_point(13));
        assert!(!info.is_nonnull(14, 2));
    }

    /// The receiver survives a join with an exception handler when nothing
    /// stores into local 0 — and does not when something does.
    #[test]
    fn the_receiver_seed_reaches_handler_code() {
        #[rustfmt::skip]
        let code: Vec<u8> = vec![
            0xB8, 0x00, 0x02, // 0: invokestatic #2
            0xA7, 0x00, 0x05, // 3: goto -> 8
            0x57,             // 6: pop                (handler entry)
            0x00,             // 7: nop
            0x2A,             // 8: aload_0            (join)
            0xC6, 0x00, 0x05, // 9: ifnull -> 14
            0x03,             // 12: iconst_0
            0xAC,             // 13: ireturn
            0x04,             // 14: iconst_1
            0xAC,             // 15: ireturn
        ];
        let seeded = analyze_with_receiver(&code, code.len(), true);
        assert!(
            seeded.is_nonnull(8, 0),
            "`this` is non-null on the handler path too"
        );
        let unseeded = analyze_with_receiver(&code, code.len(), false);
        assert!(!unseeded.is_nonnull(8, 0));

        #[rustfmt::skip]
        let reassigned: Vec<u8> = vec![
            0xB8, 0x00, 0x02, // 0: invokestatic #2
            0xA7, 0x00, 0x06, // 3: goto -> 9
            0x57,             // 6: pop                (handler entry)
            0x01,             // 7: aconst_null
            0x4B,             // 8: astore_0           (local 0 is not `this` any more)
            0x2A,             // 9: aload_0            (join)
            0xC6, 0x00, 0x05, // 10: ifnull -> 15
            0x03,             // 13: iconst_0
            0xAC,             // 14: ireturn
            0x04,             // 15: iconst_1
            0xAC,             // 16: ireturn
        ];
        let info = analyze_with_receiver(&reassigned, reassigned.len(), true);
        assert!(!info.is_nonnull(9, 0));
    }
}
