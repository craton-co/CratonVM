// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The compile-time operand-stack simulation.
//!
//! This backend never materialises a JVM operand stack. It tracks, at compile
//! time, where each pushed value currently lives — a frame spill slot, a
//! callee-saved register, a caller-saved scratch register, or an XMM — and
//! emits the move only when something consumes the value.
//!
//! That is why `flush_scratch_registers` exists and why every call, backward
//! branch and return has to reach it: a `Scratch`/`Xmm` slot is a promise that
//! the value is still in a caller-saved register, and a call breaks it. The
//! spill-cursor bookkeeping (`reserve_spill_slots`, `set_spill_depth`,
//! `checked_spill_range_end`) lives here for the same reason — it is the
//! allocation side of the same simulation.

use super::*;

/// TEST-ONLY, opt-in: cap `spill_slots` at this many words, whatever
/// `max_stack` asks for. Unset (the default) means no cap.
///
/// `spill-range-exhausted` is a refusal nothing in the tree could count until
/// the spill census landed, and on every workload measured it fires ZERO times
/// with 7 to 9 words of headroom to spare. A census column that never fires is
/// indistinguishable from one armed where it cannot fire, and the way to tell
/// those apart is to make the thing happen on purpose. Shrinking the budget
/// does that without inventing a pathological method: the same emitter, the
/// same workload and the same code path, with less room. At 48, 32, 24, 16, 12
/// and 8 words it refuses 2, 56, 96, 137, 157 and 177 compiles, which is what
/// establishes that the column is wired where it can fire.
///
/// It caps rather than replaces, so a small method is unaffected and the arm
/// only bites where the budget was actually being used.
pub fn spill_slots_cap() -> Option<usize> {
    static G: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_SPILL_SLOTS_CAP")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
    })
}

/// Why a spill word is being taken.
///
/// A parameter rather than a convention, for the reason the frame-line census
/// gives one function every verdict: the first cut of the spill census
/// attributed three call sites by hand and left 22-30% of reservations in an
/// unnamed remainder -- and an unnamed remainder is exactly where the inline
/// reserve was hiding when it took 280 words to hold 7. With this, a new
/// `reserve_spill_slots` call site does not compile until someone has said
/// which column it belongs in, and `res-total` is the sum of the named columns
/// by construction rather than by hope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpillReason {
    /// The ordinary operand push.
    Push,
    /// `flush_scratch_registers` giving a register-resident operand a home.
    Flush,
    /// `invalidate_callee_saved` re-homing every entry that aliases a register.
    Invalidate,
    /// An inlined callee's local frame.
    InlineLocals,
    /// An inlined body's branch-merge area.
    InlineMerge,
    /// The direct-call argument-service copy.
    CallService,
    /// A helper's argument buffer or out-parameter (intrinsic dispatch, FFM,
    /// the monitor receiver).
    HelperArgs,
}

impl SpillReason {
    fn column(self) -> usize {
        match self {
            SpillReason::Push => crate::SPILL_RES_PUSH,
            SpillReason::Flush => crate::SPILL_RES_FLUSH,
            SpillReason::Invalidate => crate::SPILL_RES_INVALIDATE,
            SpillReason::InlineLocals => crate::SPILL_RES_INLINE_LOCALS,
            SpillReason::InlineMerge => crate::SPILL_RES_INLINE_MERGE,
            SpillReason::CallService => crate::SPILL_RES_CALL_SERVICE,
            SpillReason::HelperArgs => crate::SPILL_RES_HELPER_ARGS,
        }
    }
}

impl Compiler {
    pub(super) fn checked_spill_range_end(&mut self, start: i32, slots: usize) -> Option<i32> {
        let bytes = slots.checked_mul(8).and_then(|n| i32::try_from(n).ok());
        let Some(bytes) = bytes else {
            self.fail("singlepass-codegen/spill-range-byte-count-overflow");
            return None;
        };
        let Some(end) = start.checked_add(bytes) else {
            self.fail("singlepass-codegen/spill-range-end-overflow");
            return None;
        };
        if start < self.base_spill_offset || end > self.spill_limit_offset {
            crate::note_spill_cursor(crate::SPILL_REFUSED_EXHAUSTED, 1);
            self.fail("singlepass-codegen/spill-range-exhausted");
            return None;
        }
        crate::note_spill_peak(
            u64::try_from((end - self.base_spill_offset) / 8).unwrap_or(0),
            u64::try_from((self.spill_limit_offset - end) / 8).unwrap_or(0),
        );
        Some(end)
    }

    pub(super) fn reserve_spill_slots(&mut self, slots: usize, why: SpillReason) -> Option<i32> {
        // One bump, into the column the caller named. `res-total` is derived
        // from these at read time rather than counted alongside them, so the
        // partition cannot drift and there is no second atomic to race with.
        crate::note_spill_cursor(why.column(), slots as u64);
        let start = self.next_spill_offset;
        let end = self.checked_spill_range_end(start, slots)?;
        self.next_spill_offset = end;
        Some(start)
    }

    pub(super) fn spill_range_fits(&mut self, start: i32, slots: usize) -> bool {
        self.checked_spill_range_end(start, slots).is_some()
    }

    pub(super) fn set_spill_depth(&mut self, depth: usize) -> bool {
        let Some(end) = self.checked_spill_range_end(self.base_spill_offset, depth) else {
            return false;
        };
        self.next_spill_offset = end;
        true
    }

    /// Push a value onto the simulated operand stack.
    /// Allocates a spill slot and returns the frame offset.
    ///
    /// T1.1.a — the corresponding `stack_oop_marks` entry is set to
    /// `false` by default. Callers that know the value is an object
    /// reference should call [`Self::mark_top_as_oop`] immediately
    /// after.
    pub(super) fn push_stack(&mut self) -> Option<StackSlot> {
        if self.stack.is_empty() && self.stack_oop_marks.is_empty() {
            self.stack_oop_marks_exact = true;
        }
        let offset = self.reserve_spill_slots(1, SpillReason::Push)?;
        let slot = StackSlot::Frame(offset);
        self.stack.push(slot);
        self.stack_oop_marks.push(false);
        Some(slot)
    }

    /// Stage 1 (precise oop maps) — push a *given* slot onto the simulated
    /// operand stack while keeping the parallel `stack_oop_marks` vector in
    /// lockstep. `is_oop` records whether the value is an object reference.
    ///
    /// This is the ONLY sanctioned way to grow `self.stack` with a
    /// register/XMM-resident value: a bare `self.stack.push(slot)` desyncs
    /// the two vectors (marks shorter than stack), which historically shifted
    /// every later slot's oop bit and produced false-positive / false-negative
    /// precise oop map entries (see `fixed-suite-bugs/app-jvm-bugs/jit-safepoint-revert.md` and
    /// `fixed-suite-bugs/app-jvm-bugs/precise-jit-stack-maps-design.md`). The desync was previously
    /// papered over by a lazy `false`-pad in `emit_oop_map_for_safepoint`;
    /// with this helper the lockstep invariant `stack.len() == marks.len()`
    /// holds continuously, which is the load-bearing prerequisite for a
    /// *moving* GC that rewrites precisely-mapped slots.
    pub(super) fn stack_push(&mut self, slot: StackSlot, is_oop: bool) {
        if self.stack.is_empty() && self.stack_oop_marks.is_empty() {
            self.stack_oop_marks_exact = true;
        }
        self.stack.push(slot);
        self.stack_oop_marks.push(is_oop);
    }

    /// T1.1.a — mark the top-of-stack slot as an object reference. Called
    /// by opcode handlers for every push that produces an oop
    /// (`new`, `anewarray`, `aload*`, `aaload`, `getfield` on reference
    /// fields, `invoke*` returning an `L` or `[` descriptor, etc.).
    pub(super) fn mark_top_as_oop(&mut self) {
        if let Some(mark) = self.stack_oop_marks.last_mut() {
            *mark = true;
        }
    }

    /// Record the operand-stack depth AND oop-mark vector live at
    /// `target_pc`, the first time this branch target is seen — the single
    /// entry point every branch-emitting opcode handler uses instead of
    /// touching `branch_target_stack_depth` directly, so the two maps can
    /// never drift out of lock-step. See `branch_target_stack_oop_marks`'s
    /// doc comment for why the oop marks matter (the dead-code merge
    /// reconstruction needs them to avoid silently mis-marking a live
    /// reference as non-oop).
    pub(super) fn record_branch_target_depth(&mut self, target_pc: usize) {
        let stack_len = self.stack.len();
        let marks = self.stack_oop_marks.clone();
        self.branch_target_stack_depth
            .entry(target_pc)
            .or_insert(stack_len);
        self.branch_target_stack_oop_marks
            .entry(target_pc)
            .or_insert(marks);
    }

    /// Pop `n` invoke arguments, returning their slots (in call order) AND
    /// whether each is a reference.
    ///
    /// `pop_stack` discards the oop mark it pops, which is fine everywhere the
    /// value goes straight back onto the simulated stack — but invoke arguments
    /// leave the stack model entirely and land in a staging buffer, and the
    /// safepoint map has to name the reference ones. This is the only way to
    /// learn which those are, since the marks are gone by the time the buffer
    /// offsets are known.
    ///
    /// The mark is read BEFORE the pop, so it is the mark belonging to the slot
    /// being popped. A short mark vector reads `false` here and additionally
    /// makes `pop_stack` clear `stack_oop_marks_exact`, which
    /// `emit_oop_map_for_safepoint` already treats as an incomplete map — so a
    /// desync cannot turn into a silently unnamed argument.
    pub(super) fn pop_invoke_args(&mut self, n: usize) -> (Vec<StackSlot>, Vec<bool>) {
        let mut slots = Vec::with_capacity(n);
        let mut oops = Vec::with_capacity(n);
        for _ in 0..n {
            oops.push(self.stack_oop_marks.last().copied().unwrap_or(false));
            slots.push(self.pop_stack());
        }
        slots.reverse();
        oops.reverse();
        (slots, oops)
    }

    /// Pop a value from the simulated operand stack.
    /// Returns `StackSlot::Frame(0)` and sets `self.failed` on underflow.
    pub(super) fn pop_stack(&mut self) -> StackSlot {
        let slot = match self.stack.pop() {
            Some(s) => s,
            None => {
                self.fail("singlepass-codegen/operand-stack-underflow-pop");
                return StackSlot::Frame(0);
            }
        };
        // T1.1.a — keep the parallel oop-mark vector in lock-step.
        // If it's shorter than expected (dead-code merge fixup), push a
        // default false to avoid panics and continue with conservative
        // fallback for this frame slice.
        if self.stack_oop_marks.pop().is_none() {
            self.stack_oop_marks_exact = false;
            // desync — conservative fallback
        }
        if self.stack.is_empty() && self.stack_oop_marks.is_empty() {
            self.stack_oop_marks_exact = true;
        }
        // Reclaim spill space if this was a Frame slot at the top — and only if
        // no entry still on the stack lives at or above it.
        //
        // The bare `off == next_spill_offset - 8` test assumed spill offsets are
        // handed out in stack order, so the top entry always owns the topmost
        // slot. `invalidate_callee_saved` breaks that assumption: it reserves ONE
        // fresh slot at the top of the reserve and repoints EVERY matching
        // entry at it, including entries buried under the top of the stack. A
        // pop of a shallower `Frame` entry then rewound the cursor past a slot a
        // DEEPER entry still owned, and the next `push_stack` handed the same
        // offset out again — the pushed value overwrote the buried one, and the
        // buried entry read back whatever the new owner had stored.
        //
        // The shape it was measured on is `kotlin.reflect...KotlinTypeFactory
        // .simpleTypeWithNonTrivialMemberScope`, whose kotlinc-generated body
        // pops five operands into locals 7..11 (`astore`/`istore`, each one an
        // invalidation) while four earlier operands still sit on the stack, then
        // reloads all five to build a lambda and finally calls a constructor
        // with the four buried ones. The buried `arguments` operand arrived null
        // — `NullPointerException: Parameter specified as non-null is null:
        // method ...SimpleTypeImpl.<init>, parameter arguments` — which is
        // `InvocableHandlerMethodKotlinTests.genericParameter()` in the Spring
        // Framework suite. It needs at least four colourable locals to appear
        // (`CRATONVM_JIT_LOCAL_REGS=3` passes, `=4` fails) and disappears under
        // `--nojit` and `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=0`.
        //
        // The added scan is over the simulated stack, which is short, and it can
        // only ever DELAY a reclaim: a slot nobody references is still freed on
        // the next pop that tops out at it. Frame usage can rise for a method
        // that invalidates a lot, which `checked_spill_range_end` already bounds
        // — an exhausted reserve fails the compile and falls back to the
        // interpreter rather than emitting a wrong body.
        if let StackSlot::Frame(off) = slot {
            if off == self.next_spill_offset - 8
                && !self
                    .stack
                    .iter()
                    .any(|s| matches!(s, StackSlot::Frame(o) if *o >= off))
            {
                self.next_spill_offset -= 8;
            }
        }
        // Free scratch XMM if no other stack entry references it
        if let StackSlot::Xmm(xmm) = slot {
            if xmm >= 2
                && xmm <= 7
                && !self
                    .stack
                    .iter()
                    .any(|s| matches!(s, StackSlot::Xmm(x) if *x == xmm))
            {
                self.free_scratch_xmm(xmm);
            }
        }
        slot
    }

    /// Duplicate the single top-of-stack slot. Shared by `dup` (0x59) and the
    /// FORM-2 case of `dup2` (0x5C) — a single category-2 long/double is ONE
    /// 64-bit slot in this model, so `[…, w] → […, w, w]` is exactly a `dup`.
    /// Carries the source slot's precise oop mark onto the copy so a duplicated
    /// reference stays mapped across safepoints (the `push_from_rax` /
    /// `push_stack` paths push a default `false`).
    pub(super) fn emit_dup_top_slot(&mut self) {
        let top = self.peek_stack();
        let top_is_oop = self.stack_oop_marks.last().copied().unwrap_or(false);
        match top {
            StackSlot::Frame(off) => {
                self.emit_load_local(RAX, off);
                self.push_from_rax();
            }
            StackSlot::CalleeSaved(_) => {
                self.stack.push(top);
                self.stack_oop_marks.push(top_is_oop);
            }
            StackSlot::Xmm(_) => {
                self.stack.push(top);
                self.stack_oop_marks.push(top_is_oop);
            }
            StackSlot::Scratch(reg, ..) => {
                let avail = SCRATCH_REGS.iter().copied().find(|&sr| {
                    sr != reg
                        && !self
                            .stack
                            .iter()
                            .any(|s| matches!(s, StackSlot::Scratch(r, ..) if *r == sr))
                });
                // Its OWN home: two stack positions sharing one would have the
                // second flush overwrite the first.
                if let Some(sr) = avail {
                    self.emit_mov_reg_reg(sr, reg);
                    self.stack.push(StackSlot::Scratch(sr));
                    self.stack_oop_marks.push(top_is_oop);
                } else {
                    self.emit_mov_reg_reg(RAX, reg);
                    if let Some(StackSlot::Frame(off)) = self.push_stack() {
                        self.emit_store_local(off, RAX);
                    }
                }
            }
        }
        if top_is_oop {
            if let Some(m) = self.stack_oop_marks.last_mut() {
                *m = true;
            }
        }
    }

    /// Classify a `dup2` (0x5C) at `dup2_pc`: `Some(true)` = FORM-2 (the top is
    /// a single category-2 long/double — one slot here — to be duplicated like
    /// `dup`), `Some(false)` = FORM-1 (two category-1 values), `None` = the top
    /// width cannot be proven locally and the caller must bail to the
    /// interpreter.
    ///
    /// In verified bytecode the instruction immediately preceding a `dup2`
    /// produces its top operand, so the form equals that instruction's result
    /// width: opcode-decodable for loads/consts/arithmetic/conversions, and
    /// resolved from the compiler's PC-keyed field/invoke metadata
    /// (`field_info` / `static_field_info` / `invoke_info`) for
    /// `getfield`/`getstatic`/`invoke*`. Any other preceding op — a stack
    /// shuffle, a branch/return block boundary, `invokedynamic`, `wide`,
    /// `checkcast`, `iinc`, … — leaves the width unprovable (`None`). This is
    /// why the analysis lives in the codegen and not in the CP-less
    /// `dup2_category_safe` scan: only here are field/invoke descriptors
    /// resolved.
    pub(super) fn dup2_top_cat2(&self, code: &[u8], dup2_pc: usize) -> Option<bool> {
        // Find the instruction boundary immediately before `dup2_pc`, and the
        // one before THAT (see the store rule below).
        let mut p = 0usize;
        let mut prev: Option<usize> = None;
        let mut prev2: Option<usize> = None;
        while p < dup2_pc {
            prev2 = prev;
            prev = Some(p);
            let len = bytecode_len_at(code, p);
            if len == 0 {
                return None;
            }
            p += len;
        }
        if p != dup2_pc {
            return None; // dup2_pc is not on an instruction boundary
        }
        let prev = prev?;

        // A STORE consumed the value it stored, so it did not produce the value
        // now on top and the producer table below cannot classify it. One shape
        // is still provable, and it is the one javac emits for every chained
        // assignment `a = b = c = 0.0`:
        //
        //     dconst_0 / dup2 / dstore A / dup2 / dstore B / dup2 / dstore C
        //
        // After `<t>store`, what is left on top is the ORIGINAL that the
        // preceding `dup`/`dup2` copied, and the store's own width names it: a
        // `dstore`/`lstore` consumed a category-2 copy, so the original is
        // category-2 too.
        //
        // BOTH instructions are required. Skipping any store and looking
        // further back is NOT sound — `iload_0; dload_1; dstore_3` leaves an
        // INT on top behind a category-2 store. Only the dup-then-store pair
        // proves the survivor's width.
        //
        // `AccurateMath.tanQ` is 999 invocations of a large method that stayed
        // interpreted for want of this: its first `dup2` follows `dconst_0` and
        // compiled, the second and third follow a `dstore` and did not.
        if let Some(prev2) = prev2 {
            let store_cat2 = match code[prev] {
                // lstore / dstore, wide-index and _0..3 forms
                0x37 | 0x39 | 0x3f..=0x42 | 0x47..=0x4a => Some(true),
                // istore / fstore / astore, wide-index and _0..3 forms
                0x36 | 0x38 | 0x3a | 0x3b..=0x3e | 0x43..=0x46 | 0x4b..=0x4e => Some(false),
                _ => None,
            };
            if let Some(cat2) = store_cat2 {
                // 0x59 dup, 0x5c dup2 — the only producers that leave a copy of
                // the stored value behind.
                if matches!(code[prev2], 0x59 | 0x5c) {
                    return Some(cat2);
                }
                return None;
            }
        }
        let cat2 = match code[prev] {
            // --- category-2 producers (result is long or double) ---
            0x09 | 0x0a | 0x0e | 0x0f          // lconst_*/dconst_*
            | 0x14                              // ldc2_w
            | 0x16 | 0x18                       // lload / dload
            | 0x1e..=0x21 | 0x26..=0x29         // lload_0..3 / dload_0..3
            | 0x2f | 0x31                       // laload / daload
            | 0x61 | 0x63 | 0x65 | 0x67 | 0x69 | 0x6b | 0x6d | 0x6f | 0x71 | 0x73 // l/d add..rem
            | 0x75 | 0x77                       // lneg / dneg
            | 0x79 | 0x7b | 0x7d                // lshl / lshr / lushr
            | 0x7f | 0x81 | 0x83                // land / lor / lxor
            | 0x85 | 0x87 | 0x8a | 0x8c | 0x8d | 0x8f => true, // i2l,i2d,l2d,f2l,f2d,d2l
            // --- category-1 producers ---
            0x01 | 0x02..=0x08 | 0x0b..=0x0d   // aconst_null, iconst_*, fconst_*
            | 0x10 | 0x11 | 0x12 | 0x13         // bipush / sipush / ldc / ldc_w
            | 0x15 | 0x17 | 0x19                // iload / fload / aload
            | 0x1a..=0x1d | 0x22..=0x25 | 0x2a..=0x2d // i/f/a load_0..3
            | 0x2e | 0x30 | 0x32 | 0x33 | 0x34 | 0x35 // iaload,faload,aaload,baload,caload,saload
            | 0x59                              // dup (category-1 only by JVMS)
            | 0x60 | 0x62 | 0x64 | 0x66 | 0x68 | 0x6a | 0x6c | 0x6e | 0x70 | 0x72 // i/f add..rem
            | 0x74 | 0x76                       // ineg / fneg
            | 0x78 | 0x7a | 0x7c | 0x7e | 0x80 | 0x82 // ishl..ixor (int)
            | 0x86 | 0x88 | 0x89 | 0x8b | 0x8e | 0x90 | 0x91 | 0x92 | 0x93 // i2f,l2i,l2f,f2i,d2i,d2f,i2b,i2c,i2s
            | 0x94 | 0x95 | 0x96 | 0x97 | 0x98  // lcmp, fcmpl/g, dcmpl/g (push int)
            | 0xbb | 0xbc | 0xbd | 0xbe         // new, newarray, anewarray, arraylength
            | 0xc1 => false,                    // instanceof
            // --- getfield / getstatic / invoke*: resolve via PC-keyed metadata ---
            0xb4 => {
                // getfield: field_info = (pc, field_index, type_tag)
                let tag = self.field_info.iter().find(|e| e.0 == prev).map(|e| e.2)?;
                matches!(tag, b'J' | b'D')
            }
            0xb2 => {
                // getstatic: static_field_info = (pc, class_id, field_index, type_tag, is_volatile)
                let tag = self
                    .static_field_info
                    .iter()
                    .find(|e| e.0 == prev)
                    .map(|e| e.3)?;
                matches!(tag, b'J' | b'D')
            }
            0xb6 | 0xb7 | 0xb8 | 0xb9 => {
                // invoke*: invoke_info = (pc, *const JitInvokeInfo)
                let rt = self
                    .invoke_info
                    .iter()
                    .find(|e| e.0 == prev)
                    // SAFETY: e.1 is a *const JitInvokeInfo recorded during this same
                    // compilation pass; it points at a live, owned JitInvokeInfo that
                    // outlives this read, so the dereference is valid and aligned.
                    .map(|e| unsafe { (*e.1).return_type })?;
                if rt == b'V' {
                    return None; // void leaves nothing on top — not a dup2 producer
                }
                matches!(rt, b'J' | b'D')
            }
            // Stores, branches, goto/return, pop/pop2, swap, dup_x*/dup2*,
            // invokedynamic, wide, checkcast, monitor, athrow, jsr/ret, iinc,
            // nop, multianewarray, putfield/putstatic — top width not locally
            // provable; bail to the interpreter.
            _ => return None,
        };
        Some(cat2)
    }

    /// The JVM category of the live operand-stack entries at `pc`, as an
    /// index-aligned vector of `Some(true)` = category-2, `Some(false)` =
    /// category-1, `None` = the analysis declined to type that entry.
    ///
    /// This is the **second-entry width oracle** that `dup2_top_cat2` is not:
    /// that helper reads the one instruction before the dup and so can only
    /// ever answer for the TOP entry, whereas `dup2_x2` needs the width of the
    /// entry below it (and, for a category-1 top, the one below that) to know
    /// how many compact entries its four JVM slots occupy.
    ///
    /// The source is `x64::stack_kinds`, the forward abstract interpretation
    /// already computed for the deopt snapshot encoder. Using it to pick a
    /// CODEGEN SHAPE is a stronger use than typing a snapshot, so it is
    /// admitted only under the same two independent cross-checks the snapshot
    /// encoder applies, plus a third:
    ///
    ///   * DEPTH — the analysis derives it from JVMS stack effects, the
    ///     emitter from running its own opcode handlers. A modelling error
    ///     that shifts the stack changes the depth.
    ///   * REF-NESS — every entry the analysis calls a reference must be one
    ///     the emitter's own oop mark also calls a reference. The marks are
    ///     maintained for the GC, so this is a second opinion with a different
    ///     provenance, and it catches an off-by-one that preserves depth.
    ///   * THE TOP ENTRY, when `dup2_top_cat2` answers — a third, wholly
    ///     independent peephole opinion. Disagreement means one of the two is
    ///     wrong and neither may be used. (Checked by the caller, which is the
    ///     only place that knows the dup's pc.)
    ///
    /// Any disagreement returns `None` and the caller stays interpreted.
    pub(super) fn stack_entry_categories(&self, pc: usize) -> Option<Vec<Option<bool>>> {
        let kinds = self.stack_kinds.get(pc)?;
        if kinds.len() != self.stack.len() {
            return None;
        }
        if self.stack_oop_marks.len() != self.stack.len() {
            return None;
        }
        for (i, k) in kinds.iter().enumerate() {
            let cat = k.is_category_2();
            if cat.is_some() {
                let says_ref = k.is_ref();
                if self.stack_oop_marks[i] != says_ref {
                    return None;
                }
            }
        }
        Some(kinds.iter().map(|k| k.is_category_2()).collect())
    }

    /// Peek at the top of the simulated stack.
    /// Returns `StackSlot::Frame(0)` and sets `self.failed` on underflow.
    pub(super) fn peek_stack(&mut self) -> StackSlot {
        match self.stack.last().copied() {
            Some(s) => s,
            None => {
                self.fail("singlepass-codegen/operand-stack-underflow-peek");
                StackSlot::Frame(0)
            }
        }
    }

    /// Reset spill state for a new basic block.
    ///
    /// The operand stack is NOT always empty here. A boolean computed for a
    /// `putfield`/`putstatic` via a conditional (`aload_0; <ifeq/iconst/goto>;
    /// putfield flag:Z`) leaves the receiver (`this`) live on the stack at
    /// `base_spill + 0` while the `ifeq`/`if_icmp`/`goto` runs through this
    /// reset. Each live stack entry `i` owns the canonical frame slot
    /// `base_spill + i*8` (that is exactly where `canonicalize_stack` and the
    /// dead-code merge reconstruction place it), so the next free spill slot is
    /// `base_spill + len*8` — NOT `base_spill`.
    ///
    /// Resetting all the way back to `base_spill` handed the receiver's own slot
    /// (`base_spill + 0`) back to the next `push_stack`, which then stored the
    /// computed boolean there and clobbered `this`. The subsequent `putfield`
    /// read the receiver as `0x1` (the boolean) and wrote to
    /// `0x1 + HEADER + field*SLOT` → SIGSEGV (the real-bytecode RAF avrora crash:
    /// `obj_ptr=0x1`, see fixed-bugs/real-raf-segv-root-cause.md).
    pub(super) fn reset_spills(&mut self) {
        // Reclaim scratch slots ABOVE the live operand stack, but never hand
        // back a slot a live stack value still occupies. The next free spill
        // slot is just past the highest live frame slot (and at least
        // `base_spill_offset` when the stack is empty / fully register-resident).
        let mut next = self.base_spill_offset;
        for &slot in &self.stack {
            if let StackSlot::Frame(off) = slot {
                next = next.max(off + 8);
            }
        }
        if next > self.spill_limit_offset {
            crate::note_spill_cursor(crate::SPILL_REFUSED_PAST_LIMIT, 1);
            self.fail("singlepass-codegen/spill-cursor-past-limit");
            return;
        }
        self.next_spill_offset = next;
    }

    /// Flush the simulated stack to canonical spill offsets (base_spill + i*8).
    /// This ensures that all paths reaching a merge point agree on frame layout.
    pub(super) fn canonicalize_stack(&mut self) {
        // SECURITY FIX (V15) INVARIANT: unlike the dead-code merge
        // reconstruction (which clears+rebuilds `self.stack` and so must
        // also rebuild `stack_oop_marks`), this routine never changes the
        // stack DEPTH — it only relocates each live slot's spill offset in
        // place. The oop-ness of a value is independent of which frame slot
        // backs it, so `stack_oop_marks[i]` stays correct for `stack[i]`
        // across the relocation. We therefore intentionally leave
        // `stack_oop_marks` untouched here; the parallel vector remains in
        // lock-step by index and is still sound at the next safepoint.
        //
        // ALIAS SAFETY: a register-resident slot (CalleeSaved/Scratch/Xmm)
        // occupies a stack position but no frame slot, so Frame slots pushed
        // above it sit BELOW their canonical offset (`push_stack` hands out
        // offsets per Frame push, not per position); `flush_scratch_registers`
        // and `swap` can additionally leave offsets above/inverted. The old
        // ascending walk stored position i to `base + i*8` and could clobber a
        // higher position's still-unread source at that same offset (e.g.
        // [CalleeSaved(R12), Frame(base+0)]: storing R12 to base+0 destroys
        // position 1's value before it is relocated). Resolve the moves as a
        // parallel-move problem instead: only emit a move whose target slot is
        // not some other pending move's source, and break source/target cycles
        // (a swapped Frame pair) by parking one value in RCX. RCX is a pure
        // scratch register between bytecodes (never a StackSlot home), and at
        // most one value is parked at a time: pending sources are distinct
        // frame slots, so the parked move's own cycle fully drains — emitting
        // the park target last — before any other all-blocked state can occur.
        let base = self.base_spill_offset;
        let len = self.stack.len();
        if !self.spill_range_fits(base, len) {
            return;
        }
        // Pending relocations: (position, source). `None` source = the value
        // is parked in RCX awaiting its canonical slot.
        let mut pending: Vec<(usize, Option<StackSlot>)> = (0..len)
            .filter_map(|i| {
                let canonical_off = base + (i as i32) * 8; // Cast: x86-64 immediate encoding
                match self.stack[i] {
                    StackSlot::Frame(off) if off == canonical_off => None, // already in place
                    slot => Some((i, Some(slot))),
                }
            })
            .collect();
        let mut parked = false;
        while !pending.is_empty() {
            let unblocked = pending.iter().position(|&(i, _)| {
                let target = base + (i as i32) * 8; // Cast: x86-64 immediate encoding
                !pending.iter().any(|&(j, src)| {
                    j != i && matches!(src, Some(StackSlot::Frame(off)) if off == target)
                })
            });
            match unblocked {
                Some(k) => {
                    let (i, src) = pending.remove(k);
                    let canonical_off = base + (i as i32) * 8; // Cast: x86-64 immediate encoding
                    match src {
                        Some(slot) => self.load_slot_to_reg(RAX, slot),
                        None => {
                            self.emit_mov_reg_reg(RAX, RCX); // parked value
                            parked = false;
                        }
                    }
                    self.emit_store_local(canonical_off, RAX);
                    self.stack[i] = StackSlot::Frame(canonical_off);
                }
                None => {
                    // Every pending move's target holds another pending move's
                    // source: the blocked-by relation (each move has at most
                    // one blocker — sources are distinct frame slots) contains
                    // a cycle of Frame-sourced moves. Walk blocker edges from
                    // any pending move until a node repeats — that node is ON
                    // the cycle — and park its value in RCX so the cycle can
                    // drain. (Parked moves never block, so a second park
                    // cannot be needed before the first parked move retires;
                    // bail defensively rather than corrupt if that invariant
                    // is ever broken.)
                    if parked {
                        self.fail("singlepass-codegen/parallel-move-second-park");
                        return;
                    }
                    let mut walk = pending[0].0;
                    let mut seen = vec![false; len];
                    loop {
                        if seen[walk] {
                            break; // `walk` is on a cycle
                        }
                        seen[walk] = true;
                        let target = base + (walk as i32) * 8; // Cast: x86-64 immediate encoding
                        match pending.iter().find(|&&(j, src)| {
                            j != walk && matches!(src, Some(StackSlot::Frame(off)) if off == target)
                        }) {
                            Some(&(j, _)) => walk = j,
                            None => {
                                // No blocker found for an all-blocked move —
                                // inconsistent state; bail safely.
                                self.fail("singlepass-codegen/parallel-move-no-blocker");
                                return;
                            }
                        }
                    }
                    let entry = pending
                        .iter_mut()
                        .find(|(i, _)| *i == walk)
                        .expect("cycle node is pending");
                    let slot = entry.1.take().expect("cycle node has a real source");
                    self.load_slot_to_reg(RCX, slot);
                    parked = true;
                }
            }
        }
        self.set_spill_depth(len);
    }

    // -----------------------------------------------------------------------
    // Raw instruction emitters
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/emit.rs`: REX/VEX prefixes, ModRM/SIB, and one method per
    // instruction form. They are still inherent methods on this `Compiler`;
    // the ones called from outside that file are declared `pub(super)` there.

    /// Return the callee-saved register for local `idx`, if register-mapped.
    pub(super) fn reg_for_local(&self, idx: usize) -> Option<u8> {
        self.local_assignments.get(idx).copied().flatten()
    }

    /// Return the XMM register assigned to a float/double local, if any.
    pub(super) fn xmm_for_local(&self, idx: usize) -> Option<u8> {
        self.xmm_assignments.get(idx).copied().flatten()
    }

    /// MOV reg, [rbp - offset]
    ///
    /// Reload elision (see the `slot_mirror` field doc): when the immediately
    /// preceding instruction was a store/load of the SAME slot — nothing
    /// emitted since, verified by exact buffer-position equality — substitute
    /// a register-register move (or nothing) for the memory reload. The
    /// mirror is refreshed to the destination register so back-to-back
    /// consumers keep chaining.
    pub(super) fn emit_load_local(&mut self, reg: u8, offset: i32) {
        if !self.slot_mirror_suppressed {
            if let Some((moff, mreg, mpos)) = self.slot_mirror {
                if moff == offset && mpos == self.buf.pos() && slot_mirror_enabled() {
                    self.emit_mov_reg_reg(reg, mreg); // no-op when reg == mreg
                    self.slot_mirror = Some((offset, reg, self.buf.pos()));
                    return;
                }
            }
        }
        self.rex_w_r(reg);
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        self.modrm_rbp_disp(reg, offset);
        if !self.slot_mirror_suppressed {
            // A completed load is itself a valid mirror source: `reg` now
            // holds `[rbp - offset]` with nothing emitted after it.
            self.slot_mirror = Some((offset, reg, self.buf.pos()));
        }
    }

    /// MOV reg, [rbp + positive_disp] — load a stack-passed argument from
    /// the caller's stack frame. Used in the prologue when a Java param's
    /// index exceeds the platform's ARG_REGS register file (e.g. the 5th
    /// arg on Windows x64 when needs_heap consumes ARG_REGS[0] for the VM
    /// pointer). The 4th-arg-and-beyond live above rbp in the caller's
    /// reserved stack slots:
    ///   * Windows: shadow space at [rbp+0x10..0x28] (caller's home for
    ///     RCX/RDX/R8/R9) + stack args at [rbp+0x30], [rbp+0x38], ...
    ///   * SysV:   stack args at [rbp+0x10], [rbp+0x18], ...
    /// `positive_disp` is the byte offset above rbp.
    pub(super) fn emit_load_caller_arg(&mut self, reg: u8, positive_disp: i32) {
        debug_assert!(positive_disp > 0, "caller arg disp must be positive");
        // ModRM r/m=101 (RBP) with positive displacement, through the checked
        // encoder: the old inline `(-128..=127)` test narrowed with `as u8`,
        // so a shadow-space/stack-arg displacement of 128 or more (reachable
        // with enough stack-passed args) would have encoded as a NEGATIVE
        // disp8 and read the callee's own frame instead of the caller's.
        let Ok(d) = Disp::encode_for_base(positive_disp as i64, RBP) else {
            self.buf
                .mark_codegen_unencodable("caller-arg-displacement-unencodable");
            return;
        };
        self.rex_w_r(reg);
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        self.buf.emit_byte(d.modrm(reg, RBP));
        let (bytes, len) = d.bytes();
        self.buf.emit(&bytes[..len]);
    }

    /// MOV [rbp - offset], reg
    pub(super) fn emit_store_local(&mut self, offset: i32, reg: u8) {
        self.rex_w_r(reg);
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.modrm_rbp_disp(reg, offset);
        if !self.slot_mirror_suppressed {
            // Record the store for the adjacent-reload elision (see
            // `slot_mirror`): the STORE always stays in the stream; only an
            // immediately-following reload of the same slot may be elided.
            self.slot_mirror = Some((offset, reg, self.buf.pos()));
        }
    }

    // -----------------------------------------------------------------------
    // Operand stack helpers
    // -----------------------------------------------------------------------

    /// Pop operand stack → rax
    pub(super) fn pop_to_rax(&mut self) {
        let slot = self.pop_stack();
        match slot {
            StackSlot::Frame(off) => self.emit_load_local(RAX, off),
            StackSlot::CalleeSaved(reg) => self.emit_mov_reg_reg(RAX, reg),
            StackSlot::Scratch(reg, ..) => self.emit_mov_reg_reg(RAX, reg),
            StackSlot::Xmm(xmm) => self.emit_movq_rax_from_xmm(xmm),
        }
    }

    /// Pop operand stack → rcx
    pub(super) fn pop_to_rcx(&mut self) {
        let slot = self.pop_stack();
        match slot {
            StackSlot::Frame(off) => self.emit_load_local(RCX, off),
            StackSlot::CalleeSaved(reg) => self.emit_mov_reg_reg(RCX, reg),
            StackSlot::Scratch(reg, ..) => self.emit_mov_reg_reg(RCX, reg),
            StackSlot::Xmm(xmm) => self.emit_movq_gpr_from_xmm(RCX, xmm),
        }
    }

    /// Push rax → operand stack.
    ///
    /// If a scratch register (R8/R9) is available, the value is moved there
    /// instead of being stored to the frame, avoiding the memory round-trip when
    /// the next bytecode immediately consumes the value.
    pub(super) fn push_from_rax(&mut self) {
        // ── Why this is pure-kernel-only ────────────────────────────────
        //
        // The comment here used to read *"the broad R8/R9 experiment regressed
        // call-heavy methods because each call flushed live scratch values"*,
        // which reads as a cost argument and is not one: a flush emits the
        // store `push_stack` would have emitted anyway, only later.
        //
        // The real blocker is a REGISTER COLLISION. `SCRATCH_REGS` is
        // `[R8, R9]` and `ARG_REGS` contains both on either ABI, so every
        // helper call marshalling three or four arguments destroys a live
        // scratch value unless a flush precedes it — and the emitter has many
        // more `emit_call_absolute` sites than flush sites. `pure_kernel`
        // excludes every one of them, which is why it works. See
        // `operand_cache_enabled` for the full statement and for what widening
        // it would actually take; the flag is opt-in so the two arms can be
        // measured in one binary.
        //
        // The SECOND defect this comment used to carry — a stretch of calls
        // growing the spill region once per flush until `spill-range-exhausted`
        // failed the compile — DOES NOT REPRODUCE. The spill census
        // (`spill_cursor_counts`) shows flush reservations are a small constant
        // that does not move when the budget is cut to the point of refusing
        // 177 compiles, and that peak usage tracks `max_stack` rather than the
        // number of calls; `flush_home` carries the inequality that explains it.
        //
        // What remains true is the constraint: reserving the home at PUSH time
        // was tried on 2026-09-02 and reverted the same day, because it made
        // this function advance the spill cursor and the OSR entry's local
        // homes are derived from the same layout — a nondeterministic heap
        // corruption. Nothing here may touch the cursor.
        if self.kernel_operand_cache || operand_cache_enabled() {
            let free = SCRATCH_REGS.iter().copied().find(|&candidate| {
                !self
                    .stack
                    .iter()
                    .any(|slot| matches!(slot, StackSlot::Scratch(r, ..) if *r == candidate))
            });
            if let Some(reg) = free {
                self.emit_mov_reg_reg(reg, RAX);
                self.stack_push(StackSlot::Scratch(reg), false);
                return;
            }
        }
        match self.push_stack() {
            Some(StackSlot::Frame(off)) => self.emit_store_local(off, RAX),
            Some(_) => unreachable!("push_stack always returns Frame"),
            None => {}
        }
    }

    /// Push RAX as XMM0 for FP intermediates (array loads, conversions, etc.).
    /// Avoids the RAX→frame→XMM0 round-trip when the value is consumed by a
    /// subsequent FP binop.
    pub(super) fn push_from_rax_as_xmm0(&mut self) {
        // Flush any existing Xmm(0) slots first (they'd be clobbered)
        self.flush_xmm0_slots();
        // MOVQ XMM0, RAX
        self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
        self.stack_push(StackSlot::Xmm(0), false);
    }

    /// Return the GPR holding the slot value. For CalleeSaved/Scratch, returns
    /// the register directly (zero-cost). For Frame, loads into `fallback` and
    /// returns `fallback`.
    pub(super) fn slot_to_gpr(&mut self, slot: StackSlot, fallback: u8) -> u8 {
        match slot {
            StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg, ..) => reg,
            StackSlot::Frame(off) => {
                self.emit_load_local(fallback, off);
                fallback
            }
            StackSlot::Xmm(xmm) => {
                self.emit_movq_gpr_from_xmm(fallback, xmm);
                fallback
            }
        }
    }

    /// Emit a load from a StackSlot into a specific GPR register.
    pub(super) fn load_slot_to_reg(&mut self, dst: u8, slot: StackSlot) {
        match slot {
            StackSlot::Frame(off) => self.emit_load_local(dst, off),
            StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg, ..) => {
                if dst != reg {
                    self.emit_mov_reg_reg(dst, reg);
                }
            }
            StackSlot::Xmm(xmm) => {
                // Materialize XMM value to GPR via MOVQ
                self.emit_movq_gpr_from_xmm(dst, xmm);
            }
        }
    }

    /// Where the flush should put the value that currently sits at
    /// operand-stack position `idx`.
    ///
    /// It takes the next word off the spill cursor, and that is not a
    /// compromise — it is optimal. The value being flushed is register-resident
    /// and owns no word; the positions below it own at most `idx` words between
    /// them; so `next_spill_offset <= base_spill_offset + idx*8`, and the
    /// reserved word is never above the position's own canonical home. There is
    /// no cheaper answer to hand it.
    ///
    /// This function exists as a named site because the 2026-09-02 revert left a
    /// note saying the opposite: that a fresh word per flushed value is what
    /// made a call-heavy method grow its spill region until
    /// `spill-range-exhausted` refused the compile. The spill census says that
    /// is not what the cursor does. On a purpose-built 40-argument stress, a
    /// `dup`/`astore` stress, CratonBench and the regression suite,
    /// `flush-reserved` is a small constant (21 of 1067 reservations on the
    /// stress; `res-push` dominates) and `peak-words` tracks the method's own
    /// `max_stack`, not the number of calls it makes. Under
    /// `CRATONVM_JIT_SPILL_SLOTS_CAP` at 48, 32, 24, 16, 12 and 8 words —
    /// budgets tight enough to refuse 2 to 177 compiles — `flush-reserved`
    /// does not move at all.
    ///
    /// A canonical-home variant of this function (store to `base + idx*8` and
    /// reuse a dead word below the cursor) was written, shipped behind a kill
    /// switch, and withdrawn: its engagement counter read ZERO in every arm at
    /// every budget, for the reason the inequality above gives — at a flush
    /// there is no dead word below the cursor to reclaim. Anyone reaching for
    /// this again should read `spill_cursor_counts()` first.
    fn flush_home(&mut self, _idx: usize) -> Option<i32> {
        // No hand-rolled bump here: `SpillReason::Flush` IS the
        // `flush-reserved` column, and counting it twice is what the first
        // fully-attributed run caught (CratonBench read 1722 where 861 was
        // right, and `res-total` inherited the error).
        self.reserve_spill_slots(1, SpillReason::Flush)
    }

    pub(super) fn flush_scratch_registers(&mut self) {
        crate::note_spill_cursor(crate::SPILL_FLUSH_CALLS, 1);
        // Collect scratch slots first to avoid double-mutable-borrow of self
        // (iterating &mut self.stack while calling self.emit_store_local).
        //
        // Each flushed value goes to `flush_home`, which prefers the position's
        // own canonical word over a fresh one — see there for why a fresh word
        // per flush is what made a call-heavy method grow its spill region until
        // the range was exhausted.
        let scratch_slots: Vec<(usize, u8)> = self
            .stack
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                if let StackSlot::Scratch(reg) = *slot {
                    Some((i, reg))
                } else {
                    None
                }
            })
            .collect();
        for (idx, reg) in scratch_slots {
            let Some(off) = self.flush_home(idx) else {
                return;
            };
            self.emit_store_local(off, reg);
            self.stack[idx] = StackSlot::Frame(off);
        }
        // Also flush Xmm stack slots (XMM0-7 are caller-saved temporaries/scratch)
        let xmm_slots: Vec<(usize, u8)> = self
            .stack
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                if let StackSlot::Xmm(xmm) = *slot {
                    if xmm < 8 {
                        Some((i, xmm))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        for (idx, xmm) in xmm_slots {
            let Some(off) = self.flush_home(idx) else {
                return;
            };
            // Direct MOVQ [rbp-off], XMM — saves the round-trip
            // through RAX (3 bytes per spill, ~90 bytes across the
            // 30 flush sites). RAX is preserved, which matters when
            // a flush happens immediately before a return-value path
            // that wants RAX intact.
            self.emit_movq_mem_rbp_from_xmm(off, xmm);
            self.stack[idx] = StackSlot::Frame(off);
        }
        // GC-root soundness (DEFAULT ON; opt out `CRATONVM_JIT_NO_CALLEE_OOP_FLUSH`)
        // — also flush `CalleeSaved` operand-stack entries that hold an object
        // reference to a frame slot. A `CalleeSaved` push is a "zero-cost push":
        // the value stays in a callee-saved register (which survives a call by
        // ABI) until consumed, so the `Scratch`/`Xmm` flushes above leave a live
        // oop residing ONLY in a register across a GC-capable call. That oop is
        // invisible to the conservative `[scanner_sp, entry_sp)` frame scan (the
        // default path: shadow stack off, `safepoint_reg_spill` env-gated off) →
        // the object can be reclaimed → use-after-free. Spilling it to its frame
        // home (and retargeting the slot to `Frame`, exactly as the Scratch/Xmm
        // passes do) puts it on the scanned stack. The spill is value-preserving
        // and every consumer reads the slot's recorded home, so this can only ADD
        // a root, never change a computed value. Only entries marked as oops are
        // spilled (the parallel `stack_oop_marks`); a non-oop callee-saved temp is
        // ABI-preserved across the call and needs no spill. Pre-call flush only —
        // this runs at every `flush_scratch_registers` site, which is the project's
        // canonical pre-call/branch/return flush point.
        if self.flush_callee_saved_oops {
            let callee_oop_slots: Vec<(usize, u8)> = self
                .stack
                .iter()
                .enumerate()
                .filter_map(|(i, slot)| {
                    if let StackSlot::CalleeSaved(reg) = *slot {
                        // Only reference entries need to be made GC-visible; the
                        // parallel mark vector stays valid after we retarget the
                        // slot to `Frame` (a frame home is just as much an oop).
                        if self.stack_oop_marks.get(i).copied().unwrap_or(false) {
                            return Some((i, reg));
                        }
                    }
                    None
                })
                .collect();
            for (idx, reg) in callee_oop_slots {
                let Some(off) = self.flush_home(idx) else {
                    return;
                };
                self.emit_store_local(off, reg);
                self.stack[idx] = StackSlot::Frame(off);
            }
        }
        // Clear scratch XMM tracking — all flushed
        self.scratch_xmm_in_use = 0;
    }

    /// Flush any Xmm(0) stack entries — promote to scratch XMM if possible,
    /// otherwise spill to frame. XMM0 is about to be clobbered.
    pub(super) fn flush_xmm0_slots(&mut self) {
        let xmm0_slots: Vec<usize> = self
            .stack
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                if matches!(slot, StackSlot::Xmm(0)) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        if xmm0_slots.is_empty() {
            return;
        }
        // Try to promote to a scratch XMM register (XMM2-7) instead of spilling
        if let Some(scratch) = self.alloc_scratch_xmm() {
            self.emit_movsd_xmm_xmm(scratch, 0); // MOVSD preserves full 64-bit value
            for idx in xmm0_slots {
                self.stack[idx] = StackSlot::Xmm(scratch);
            }
        } else {
            // All scratch XMMs busy — fall back to frame spill.
            // Direct MOVQ [rbp-off], XMM0 — RCX is left untouched, which
            // helps callers that have RCX live across this flush.
            let Some(off) = self.reserve_spill_slots(1, SpillReason::Flush) else {
                return;
            };
            self.emit_movq_mem_rbp_from_xmm(off, 0);
            for idx in xmm0_slots {
                self.stack[idx] = StackSlot::Frame(off);
            }
        }
    }

    /// Allocate a scratch XMM register (XMM2-7). Returns `None` if all are in use.
    fn alloc_scratch_xmm(&mut self) -> Option<u8> {
        for (i, &xmm) in SCRATCH_XMMS.iter().enumerate() {
            if self.scratch_xmm_in_use & (1 << i) == 0 {
                self.scratch_xmm_in_use |= 1 << i;
                return Some(xmm);
            }
        }
        None
    }

    /// Release a scratch XMM register back to the pool.
    fn free_scratch_xmm(&mut self, xmm: u8) {
        if let Some(i) = SCRATCH_XMMS.iter().position(|&r| r == xmm) {
            self.scratch_xmm_in_use &= !(1 << i);
        }
    }

    /// Invalidate any CalleeSaved or Scratch stack entries for `reg` before it
    /// is overwritten. All references are materialized to a single shared spill slot.
    pub(super) fn invalidate_callee_saved(&mut self, reg: u8) {
        let needs_spill = self
            .stack
            .iter()
            .any(|s| matches!(s, StackSlot::CalleeSaved(r) | StackSlot::Scratch(r, ..) if *r == reg));
        if !needs_spill {
            return;
        }
        // Spill the register value once
        let Some(off) = self.reserve_spill_slots(1, SpillReason::Invalidate) else {
            return;
        };
        self.emit_store_local(off, reg);
        // Update all CalleeSaved/Scratch entries for this register to the shared spill slot
        for slot in &mut self.stack {
            match *slot {
                StackSlot::CalleeSaved(r) | StackSlot::Scratch(r, ..) if r == reg => {
                    *slot = StackSlot::Frame(off);
                }
                _ => {}
            }
        }
    }
}
