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
//! - Exception handler entries are conservatively treated as having
//!   IN = 0 (no facts). This is sound (an exception can arise at
//!   any PC).

/// Result of null-check elimination analysis.
///
/// `nonnull_at_pc[i]` is the bitmask of locals known non-null at the
/// start of the instruction at `pc == i`. PCs that don't start an
/// instruction have a bitmask of 0 (no info).
#[derive(Default)]
pub struct NullCheckInfo {
    /// Per-PC non-null bitmask. Index = bytecode PC, value = u64 mask.
    masks: Vec<u64>,
}

impl NullCheckInfo {
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

/// Length in bytes of a single bytecode instruction starting at `pc`.
/// Conservative: returns 1 for any opcode we don't recognise (forces
/// the worklist to advance by 1 — still sound because the missing
/// transfer just leaves IN unchanged).
fn op_len(code: &[u8], pc: usize) -> usize {
    crate::scev::bytecode_len(code, pc, code.len())
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
/// local index. Recognises `astore_0..3` and `astore <u8>`.
fn astore_at(code: &[u8], pc: usize) -> Option<usize> {
    if pc >= code.len() {
        return None;
    }
    let op = code[pc];
    match op {
        0x4B..=0x4E => Some((op - 0x4B) as usize),
        0x3A if pc + 1 < code.len() => Some(code[pc + 1] as usize),
        _ => None,
    }
}

/// Is the opcode at `pc` one that produces a known-non-null reference
/// on top of stack? (`new`, `anewarray`, `newarray`, `multianewarray`,
/// `aload` of a proven-non-null local — but the last is tracked
/// implicitly via the IN mask, so this only returns true for the
/// allocation opcodes.)
fn produces_nonnull(op: u8) -> bool {
    matches!(
        op,
        0xBB | // new
        0xBC | // newarray
        0xBD | // anewarray
        0xC5 | // multianewarray
        0x12 | // ldc — string/class literals are non-null (numeric ldc is also OK; never null)
        0x13 | // ldc_w
        0x14 // ldc2_w
    )
}

/// 16-bit signed branch offset starting at `code[pc+1..pc+3]`.
fn rel16(code: &[u8], pc: usize) -> Option<i32> {
    if pc + 2 >= code.len() {
        return None;
    }
    Some(i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32)
}

/// 32-bit signed branch offset starting at `code[pc+1..pc+5]`.
fn rel32(code: &[u8], pc: usize) -> Option<i32> {
    if pc + 4 >= code.len() {
        return None;
    }
    Some(i32::from_be_bytes([
        code[pc + 1],
        code[pc + 2],
        code[pc + 3],
        code[pc + 4],
    ]))
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
    let len = op_len(code, pc);
    let ft = pc + len;
    match op {
        // return forms — no successor
        0xAC..=0xB1 => (None, None),
        // athrow — no in-method successor (handler edges intentionally skipped)
        0xBF => (None, None),
        // goto
        0xA7 => {
            let off = rel16(code, pc).unwrap_or(0);
            let tgt = pc.checked_add_signed(off as isize);
            (None, tgt)
        }
        // goto_w
        0xC8 => {
            let off = rel32(code, pc).unwrap_or(0);
            let tgt = pc.checked_add_signed(off as isize);
            (None, tgt)
        }
        // jsr / jsr_w / ret — pre-JDK-7, treat as fall-through only.
        0xA8 | 0xC9 | 0xA9 => (Some(ft), None),
        // tableswitch / lookupswitch — conservatively give up on the
        // jump targets (consumers see IN=0 at the targets, which is
        // sound — just less precise). Fall-through after the switch
        // doesn't exist in JVMS terms; we record no successors.
        0xAA | 0xAB => (None, None),
        // conditional branches with 16-bit offset (ifeq..if_acmpne,
        // ifnull/ifnonnull, ifnull_w doesn't exist)
        0x99..=0xA6 | 0xC6 | 0xC7 => {
            let off = rel16(code, pc).unwrap_or(0);
            let tgt = pc.checked_add_signed(off as isize);
            (Some(ft), tgt)
        }
        _ => (Some(ft), None),
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
fn transfer(code: &[u8], pc: usize, in_mask: u64, prev_inst_pc: &[usize]) -> (u64, u64) {
    if pc >= code.len() {
        return (in_mask, in_mask);
    }
    let op = code[pc];
    let mut out = in_mask;

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
    if opcode_dereferences_receiver(op) {
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
        let next_pc = pc + op_len(code, pc);
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
            if !prev_was_alloc {
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
    if matches!(op, 0xC6 | 0xC7) {
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
    // Entry IN. `this` is the single fact the bytecode cannot prove about
    // itself; everything else starts unproven. See the doc comment.
    in_masks[0] = u64::from(receiver_in_local_0);

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
    let mut is_inst_start = vec![false; len];
    let mut prev_inst_pc = vec![usize::MAX; len];
    {
        let mut p = 0usize;
        let mut prev = usize::MAX;
        while p < len {
            is_inst_start[p] = true;
            prev_inst_pc[p] = prev;
            let l = op_len(code, p);
            if l == 0 {
                break;
            }
            prev = p;
            p += l;
        }
    }

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

    while let Some(pc) = worklist.pop() {
        on_worklist[pc] = false;
        iter_count += 1;
        if iter_count > max_iters {
            // Bail — analysis didn't converge in budget. Return what
            // we have; the consumer treats absent facts as false
            // which is sound.
            break;
        }
        if pc >= len || !is_inst_start[pc] {
            continue;
        }

        let in_m = in_masks[pc];
        let (ft_out, tk_out) = transfer(code, pc, in_m, &prev_inst_pc);

        let (ft_succ, tk_succ) = successors(code, pc);

        // Propagate fall-through (out) to fall-through successor.
        if let Some(s) = ft_succ {
            if s < len && is_inst_start[s] {
                let new_in = if in_masks[s] == top {
                    ft_out
                } else {
                    in_masks[s] & ft_out
                };
                if new_in != in_masks[s] {
                    in_masks[s] = new_in;
                    if !on_worklist[s] {
                        on_worklist[s] = true;
                        worklist.push(s);
                    }
                }
            }
        }
        // Propagate taken-edge (tk_out) to branch target.
        if let Some(s) = tk_succ {
            if s < len && is_inst_start[s] {
                let new_in = if in_masks[s] == top {
                    tk_out
                } else {
                    in_masks[s] & tk_out
                };
                if new_in != in_masks[s] {
                    in_masks[s] = new_in;
                    if !on_worklist[s] {
                        on_worklist[s] = true;
                        worklist.push(s);
                    }
                }
            }
        }
    }

    // Convert `top` entries (unreachable PCs) to 0 so consumers don't
    // see spurious facts.
    for m in in_masks.iter_mut() {
        if *m == top {
            *m = 0;
        }
    }

    NullCheckInfo { masks: in_masks }
}

#[cfg(test)]
mod tests {
    use super::*;

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
